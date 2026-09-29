use crate::{
    agent::Agent,
    config::{self, Config, Provider},
    crypto::Packet,
    model::*,
    network::Network,
    relay,
};
use anyhow::{bail, Context, Result};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
};
use subtle::ConstantTimeEq;

#[derive(Clone, Serialize, Deserialize)]
pub struct Rpc {
    pub action: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub target: Option<String>,
}
#[derive(Serialize, Deserialize)]
pub struct DaemonInfo {
    pub url: String,
    pub pid: u32,
}
pub struct ApiError(anyhow::Error);
impl From<anyhow::Error> for ApiError {
    fn from(value: anyhow::Error) -> Self {
        Self(value)
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":self.0.to_string()})),
        )
            .into_response()
    }
}

pub fn local_router(net: Arc<Network>) -> Router {
    Router::new()
        .route("/rpc", post(rpc))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(net)
}
pub fn peer_router(net: Arc<Network>) -> Router {
    Router::new()
        .route("/peer", post(peer))
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(net)
}

async fn rpc(
    State(net): State<Arc<Network>>,
    headers: HeaderMap,
    Json(request): Json<Rpc>,
) -> Result<Json<Value>, ApiError> {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    let expected = net.agent.config.read().unwrap().local_token.clone();
    if headers.contains_key("origin") || !bool::from(token.as_bytes().ct_eq(expected.as_bytes())) {
        return Err(anyhow::anyhow!("Local authentication required").into());
    }
    Ok(Json(execute(&net, request).await?))
}

async fn peer(
    State(net): State<Arc<Network>>,
    Json(packet): Json<Packet>,
) -> Result<Json<Packet>, ApiError> {
    // Return a generic error to unauthenticated peers.
    let reply = net
        .receive(packet)
        .map_err(|_| anyhow::anyhow!("Encrypted packet rejected"))?
        .context("Expected an encrypted request")?;
    Ok(Json(reply))
}

pub async fn execute(net: &Arc<Network>, request: Rpc) -> Result<Value> {
    if let Some(target) = request.target {
        return net.remote(&target, &request.action, request.payload).await;
    }
    let a = &net.agent;
    let p = request.payload;
    let local_id = a.config.read().unwrap().device.id.clone();
    match request.action.as_str() {
        "shutdown" => {
            net.shutdown.cancel();
            Ok(json!({"stopping":true}))
        }
        "status" | "run" | "task" | "cancel" => net.dispatch(&local_id, &request.action, p),
        "events" => Ok(json!(a
            .store
            .events(p["session_id"].as_str(), p["after"].as_i64().unwrap_or(0))?)),
        "tasks" => Ok(json!(a.store.tasks()?)),
        "sessions" => Ok(json!(a.store.sessions()?)),
        "approvals" => Ok(json!(a.store.approvals()?)),
        "approve" => {
            a.store.decide(
                p["id"].as_str().context("Missing approval id")?,
                p["allow"].as_bool().unwrap_or(false),
            )?;
            Ok(json!({"ok":true}))
        }
        "devices" => net.devices().await,
        "pair.export" => Ok(json!(a.status().device)),
        "pair.trust" => {
            let device: Device = serde_json::from_value(p["device"].clone())?;
            let fingerprint = p["fingerprint"]
                .as_str()
                .context("Full fingerprint required")?;
            update(a, |c| {
                c.trust(
                    device,
                    fingerprint,
                    p["allow_agent"].as_bool().unwrap_or(false),
                )
            })?;
            Ok(json!({"trusted":fingerprint}))
        }
        "pair.revoke" => {
            let id = p["id"].as_str().context("Missing device ID")?;
            update(a, |c| {
                c.peers.remove(id);
                Ok(())
            })?;
            // Revocation also cancels ongoing work from this source.
            for task in a.store.tasks()? {
                if task.source_device == id {
                    let _ = a.cancel(&task.id);
                }
            }
            Ok(json!({"revoked":id}))
        }
        "model.set" => {
            let provider: Provider = serde_json::from_value(p)?;
            config::check_url(&provider.base_url)?;
            if provider.model.is_empty()
                || provider.api_key_env.is_empty()
                || provider.max_steps == 0
                || provider.max_steps > 100
            {
                bail!("Model, API key environment name and 1–100 steps required");
            }
            update(a, |c| {
                c.provider = provider;
                Ok(())
            })?;
            Ok(json!(a.status()))
        }
        "workspace.set" => {
            let path = PathBuf::from(p["path"].as_str().context("Missing path")?).canonicalize()?;
            if !path.is_dir() {
                bail!("Workspace must be a directory");
            }
            if !a.running.lock().unwrap().is_empty() {
                bail!("Wait for active tasks before changing workspace");
            }
            update(a, |c| {
                c.workspace = path;
                Ok(())
            })?;
            Ok(json!(a.status()))
        }
        "device.set" => {
            update(a, |c| {
                if let Some(name) = p["name"].as_str() {
                    if name.is_empty() || name.len() > 256 {
                        bail!("Invalid device name");
                    }
                    c.device.name = name.into();
                }
                if let Some(endpoint) = p["endpoint"].as_str() {
                    let url = reqwest::Url::parse(endpoint)?;
                    if !matches!(url.scheme(), "http" | "https") {
                        bail!("Invalid endpoint");
                    }
                    c.device.endpoint = Some(endpoint.trim_end_matches('/').into());
                }
                Ok(())
            })?;
            Ok(json!(a.status().device))
        }
        "login" => {
            let url = p["url"].as_str().context("Missing relay URL")?;
            let login = relay::login(
                url,
                relay::Credentials {
                    account: p["account"].as_str().context("Missing account")?.into(),
                    password: p["password"].as_str().context("Missing password")?.into(),
                    device_id: local_id,
                    register: p["register"].as_bool().unwrap_or(false),
                },
            )
            .await?;
            update(a, |c| {
                c.relay = Some(login);
                Ok(())
            })?;
            Ok(json!({"logged_in":true}))
        }
        "logout" => {
            let login = a.config.read().unwrap().relay.clone();
            if let Some(login) = login {
                reqwest::Client::new()
                    .post(format!("{}/logout", login.url))
                    .bearer_auth(login.token)
                    .send()
                    .await?
                    .error_for_status()?;
            }
            update(a, |c| {
                c.relay = None;
                Ok(())
            })?;
            Ok(json!({"logged_out":true}))
        }
        _ => bail!("Unknown local action"),
    }
}

fn update(a: &Agent, change: impl FnOnce(&mut Config) -> Result<()>) -> Result<()> {
    let mut current = a.config.write().unwrap();
    let mut next = current.clone();
    change(&mut next)?;
    next.save(&a.root)?;
    *current = next;
    Ok(())
}

pub async fn serve(root: &Path, peer_listen: Option<SocketAddr>) -> Result<()> {
    std::fs::create_dir_all(root)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("daemon.lock"))?;
    lock.try_lock_exclusive()
        .context("A mutlink daemon is already running for this home")?;
    let agent = Agent::open(root.to_path_buf())?;
    let net = Network::new(agent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let info = DaemonInfo {
        url: format!("http://{}", listener.local_addr()?),
        pid: std::process::id(),
    };
    let peer = if let Some(addr) = peer_listen {
        Some(tokio::net::TcpListener::bind(addr).await?)
    } else {
        None
    };
    config::private_write(&root.join("daemon.json"), &serde_json::to_vec(&info)?)?;
    let worker = tokio::spawn(net.clone().worker());
    let peer_task = peer.map(|listener| {
        let app = peer_router(net.clone());
        tokio::spawn(async move { axum::serve(listener, app).await })
    });
    println!("mutlink agent ready at {}", info.url);
    let shutdown = net.shutdown.clone();
    let stopping_agent = agent.clone();
    let result = axum::serve(listener, local_router(net))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {},
                _ = shutdown.cancelled() => {},
            }
            for token in agent.running.lock().unwrap().values() {
                token.cancel();
            }
        })
        .await;
    worker.abort();
    if let Some(task) = peer_task {
        task.abort();
    }
    // Let cancelled tools kill their process groups and persist final state before
    // the Tokio runtime is dropped.
    for _ in 0..100 {
        if stopping_agent.running.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_file(root.join("daemon.json"));
    result?;
    Ok(())
}
