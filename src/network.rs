use crate::{
    agent::Agent,
    config,
    crypto::{self, Packet},
    model::*,
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;

type Waiter = (String, oneshot::Sender<RemoteResponse>);
pub struct Network {
    pub agent: Arc<Agent>,
    pub shutdown: tokio_util::sync::CancellationToken,
    pending: Mutex<HashMap<String, Waiter>>,
    http: reqwest::Client,
}

impl Network {
    pub fn new(agent: Arc<Agent>) -> Arc<Self> {
        Arc::new(Self {
            agent,
            pending: Mutex::new(HashMap::new()),
            shutdown: tokio_util::sync::CancellationToken::new(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        })
    }

    pub fn dispatch(&self, source: &str, action: &str, payload: Value) -> Result<Value> {
        match action {
            "status" => Ok(serde_json::to_value(self.agent.status())?),
            "run" => Ok(serde_json::to_value(
                self.agent
                    .submit(serde_json::from_value(payload)?, source)?,
            )?),
            "task" | "events" | "cancel" => {
                let task_id = payload["task_id"].as_str().context("Missing task_id")?;
                let task = self.agent.store.task(task_id)?;
                if task.source_device != source {
                    bail!("Task belongs to another device");
                }
                match action {
                    "events" => Ok(json!(self.agent.store.events(
                        Some(&task.session_id),
                        payload["after"].as_i64().unwrap_or(0)
                    )?)),
                    "cancel" => {
                        self.agent.cancel(task_id)?;
                        Ok(json!({"cancelled":true}))
                    }
                    _ => Ok(json!(task)),
                }
            }
            _ => bail!("Unsupported remote action"),
        }
    }

    pub fn receive(&self, packet: Packet) -> Result<Option<Packet>> {
        let cfg = self.agent.config.read().unwrap().clone();
        let payload = crypto::open(&cfg, &packet)?;
        self.agent.store.accept_packet(&packet.id)?;
        if payload["kind"] == "response" {
            let response: RemoteResponse = serde_json::from_value(payload["body"].clone())?;
            let mut pending = self.pending.lock().unwrap();
            if pending
                .get(&response.id)
                .is_some_and(|(source, _)| source == &packet.from)
            {
                if let Some((_, tx)) = pending.remove(&response.id) {
                    let _ = tx.send(response);
                }
            }
            return Ok(None);
        }
        if payload["kind"] != "request" {
            bail!("Invalid packet kind");
        }
        let request: RemoteRequest = serde_json::from_value(payload["body"].clone())?;
        if request.expires_at <= now() || request.expires_at > now() + 120 {
            bail!("Expired request");
        }
        let result = self.dispatch(&packet.from, &request.action, request.payload);
        let response = match result {
            Ok(data) => RemoteResponse {
                id: request.id,
                ok: true,
                data,
            },
            Err(error) => RemoteResponse {
                id: request.id,
                ok: false,
                data: json!({"error":error.to_string()}),
            },
        };
        Ok(Some(crypto::seal(
            &cfg,
            &packet.from,
            json!({"kind":"response","body":response}),
        )?))
    }

    pub async fn remote(&self, target: &str, action: &str, payload: Value) -> Result<Value> {
        let cfg = self.agent.config.read().unwrap().clone();
        let peer = cfg
            .peers
            .get(target)
            .context("Pair this device and verify its fingerprint first")?;
        let request = RemoteRequest {
            id: id(),
            action: action.into(),
            payload,
            expires_at: now() + 90,
        };
        let clear = json!({"kind":"request","body":request});
        if let Some(endpoint) = &peer.device.endpoint {
            // The packet is end-to-end encrypted even when a LAN endpoint uses HTTP.
            let url = reqwest::Url::parse(endpoint)?;
            if !matches!(url.scheme(), "http" | "https") {
                bail!("Invalid direct endpoint");
            }
            let packet = crypto::seal(&cfg, target, clear.clone())?;
            let direct = self
                .http
                .post(format!("{}/peer", endpoint.trim_end_matches('/')))
                .json(&packet)
                .send()
                .await;
            if let Ok(response) = direct {
                if response.status().is_success() {
                    let packet: Packet = response.json().await?;
                    if packet.from != target {
                        bail!("Wrong response device");
                    }
                    let value = crypto::open(&cfg, &packet)?;
                    self.agent.store.accept_packet(&packet.id)?;
                    if value["kind"] != "response" {
                        bail!("Invalid direct reply");
                    }
                    let reply: RemoteResponse = serde_json::from_value(value["body"].clone())?;
                    if reply.id != request.id {
                        bail!("Response correlation mismatch");
                    }
                    return response_value(reply);
                }
            }
        }
        let login = cfg
            .relay
            .as_ref()
            .context("Direct connection unavailable; log both devices into a relay")?;
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap()
            .insert(request.id.clone(), (target.into(), tx));
        let result: Result<Value> = async {
            self.send_relay(login, &crypto::seal(&cfg, target, clear)?)
                .await?;
            let response = tokio::time::timeout(Duration::from_secs(45), rx)
                .await
                .context(
                "Device did not respond; check its online state. Retry run with the same task ID.",
            )??;
            response_value(response)
        }
        .await;
        self.pending.lock().unwrap().remove(&request.id);
        result
    }

    async fn send_relay(&self, login: &config::RelayLogin, packet: &Packet) -> Result<()> {
        self.http
            .post(format!("{}/packets", login.url))
            .bearer_auth(&login.token)
            .json(packet)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    pub async fn devices(&self) -> Result<Value> {
        let cfg = self.agent.config.read().unwrap().clone();
        let discovered: Value = if let Some(login) = cfg.relay {
            self.http
                .get(format!("{}/devices", login.url))
                .bearer_auth(login.token)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        } else {
            json!([])
        };
        Ok(json!({"local":self.agent.status().device,"trusted":cfg.peers,"discovered":discovered}))
    }

    pub async fn worker(self: Arc<Self>) {
        let mut delay = 1u64;
        loop {
            let login = self.agent.config.read().unwrap().relay.clone();
            let Some(login) = login else {
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            };
            let result: Result<()> = async {
                self.http
                    .post(format!("{}/devices", login.url))
                    .bearer_auth(&login.token)
                    .json(&self.agent.status().device)
                    .send()
                    .await?
                    .error_for_status()?;
                let packets: Vec<Packet> = self
                    .http
                    .get(format!("{}/packets", login.url))
                    .bearer_auth(&login.token)
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                for packet in packets {
                    let packet_id = packet.id.clone();
                    // Invalid/replayed packets are acknowledged and discarded.
                    if let Ok(Some(reply)) = self.receive(packet) {
                        self.send_relay(&login, &reply).await?;
                    }
                    self.http
                        .delete(format!("{}/packets/{packet_id}", login.url))
                        .bearer_auth(&login.token)
                        .send()
                        .await?
                        .error_for_status()?;
                }
                Ok(())
            }
            .await;
            self.agent
                .relay_connected
                .store(result.is_ok(), std::sync::atomic::Ordering::Relaxed);
            delay = if result.is_ok() {
                1
            } else {
                (delay * 2).min(30)
            };
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
    }
}

fn response_value(reply: RemoteResponse) -> Result<Value> {
    if !reply.ok {
        bail!(
            "{}",
            reply.data["error"]
                .as_str()
                .unwrap_or("Remote execution failed")
        );
    }
    Ok(reply.data)
}
