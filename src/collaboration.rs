use crate::{agent::Agent, model::*};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const MAX_DEPTH: u8 = 2;

pub fn schemas(depth: u8, connected: bool) -> Vec<Value> {
    let mut tools = vec![json!({
        "type":"function","function":{
            "name":"list_agents",
            "description":"List paired agents and their full device IDs. Reachability and permission to execute must be checked by ask_agent.",
            "parameters":{"type":"object","properties":{},"required":[],"additionalProperties":false}
        }
    })];
    if connected && depth < MAX_DEPTH {
        tools.push(json!({
            "type":"function","function":{
                "name":"ask_agent",
                "description":"Ask a paired device's independent agent to perform a task in its own workspace and return its answer. Include the necessary context. Waits up to 10 minutes. Reuse returned session_id for follow-ups. Writes and shell require approval on the receiving device. On uncertain delivery, use the returned task_id to reconcile before submitting new work.",
                "parameters":{
                    "type":"object",
                    "properties":{
                        "device_id":{"type":"string","description":"Full device ID from list_agents"},
                        "prompt":{"type":"string","description":"Concrete task or follow-up question, including relevant context"},
                        "session_id":{"type":"string","description":"Optional remote session ID from a previous answer"},
                        "plan":{"type":"boolean","description":"Request read-only execution. Parent Plan mode always forces this to true."}
                    },
                    "required":["device_id","prompt"],"additionalProperties":false
                }
            }
        }));
    }
    tools
}

pub fn list_agents(agent: &Agent) -> Value {
    let config = agent.config.read().unwrap();
    json!({
        "local_device_id":config.device.id,
        "agents":config.peers.values().map(|peer| json!({
            "device_id":peer.device.id,
            "name":peer.device.name,
            "platform":peer.device.platform,
            "endpoint":peer.device.endpoint,
            "capabilities":peer.device.capabilities,
            "allowed_to_call_this_device":peer.allow_agent,
        })).collect::<Vec<_>>()
    })
}

pub async fn ask(
    agent: &Agent,
    parent: &Task,
    call_id: &str,
    args: &Value,
    plan: bool,
    depth: u8,
    cancel: CancellationToken,
) -> Result<Value> {
    if depth >= MAX_DEPTH {
        bail!("Agent delegation depth reached the 2-hop limit");
    }
    let target = args["device_id"].as_str().context("Missing device_id")?;
    let prompt = args["prompt"].as_str().context("Missing prompt")?;
    if prompt.trim().is_empty() || prompt.len() > 65536 {
        bail!("Prompt must contain 1–65536 bytes");
    }
    let local_id = agent.status().device.id;
    if target == local_id {
        bail!("Use local tools for this device");
    }
    let net = agent
        .network
        .read()
        .unwrap()
        .upgrade()
        .context("Agent network is unavailable")?;
    let request = RunRequest {
        // Correlate retries with the same model tool call, including after lost replies.
        id: hex::encode(Sha256::digest(serde_json::to_vec(&json!([
            local_id, parent.id, call_id
        ]))?)),
        session_id: args["session_id"].as_str().map(str::to_owned),
        prompt: prompt.into(),
        plan: plan || args["plan"].as_bool().unwrap_or(false),
        delegation_depth: depth + 1,
    };
    let task_id = request.id.clone();
    agent.store.event(
        parent,
        "delegation_start",
        json!({
            "device_id":target,"task_id":task_id,"session_id":request.session_id,
            "plan":request.plan,"depth":request.delegation_depth,
        }),
    )?;
    let started = std::time::Instant::now();
    let mut submitted = false;
    let work = async {
        let status = net.remote(target, "status", Value::Null).await?;
        let compatible = status["device"]["capabilities"]
            .as_array()
            .is_some_and(|caps| {
                caps.iter()
                    .any(|cap| cap["id"] == "agent.collaborate" && cap["available"] == true)
            });
        if !compatible {
            bail!("Peer must be upgraded to support bounded agent collaboration");
        }
        submitted = true;
        let task: Task = serde_json::from_value(net.remote(target, "run", json!(request)).await?)?;
        if task.id != task_id {
            bail!("Peer returned a different task ID");
        }
        agent.store.event(
            parent,
            "delegation_accepted",
            json!({
                "device_id":target,"task_id":task_id,"session_id":task.session_id,
            }),
        )?;
        loop {
            let result = net
                .remote(target, "result", json!({"task_id":task_id}))
                .await?;
            let task: Task = serde_json::from_value(result["task"].clone())?;
            if task.id != task_id {
                bail!("Peer returned a different task ID");
            }
            if !matches!(
                task.state.as_str(),
                "queued" | "running" | "waiting_approval"
            ) {
                return Ok(json!({
                    "device_id":target,"task_id":task_id,"session_id":task.session_id,
                    "state":task.state,"error":task.error,"answer":result["answer"],
                }));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    let result: Result<Value> = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow::anyhow!("Parent task cancelled")),
        result = tokio::time::timeout(Duration::from_secs(600), work) => {
            result.unwrap_or_else(|_| Err(anyhow::anyhow!("Delegation exceeded its 10-minute budget")))
        }
    };
    let output = match result {
        Ok(value) => value,
        Err(error) => {
            // A lost reply does not establish whether execution started or stopped.
            let cancellation_requested = if submitted {
                matches!(
                    tokio::time::timeout(
                        Duration::from_secs(3),
                        net.remote(target, "cancel", json!({"task_id":task_id})),
                    )
                    .await,
                    Ok(Ok(_))
                )
            } else {
                false
            };
            json!({
                "device_id":target,"task_id":task_id,"state":if submitted {"unknown"} else {"rejected"},
                "error":error.to_string(),
                "cancellation_requested":cancellation_requested,
                "reconcile_before_retry":submitted,
            })
        }
    };
    agent.store.event(
        parent,
        "delegation_result",
        json!({
            "result":output,"elapsed_ms":started.elapsed().as_millis(),
        }),
    )?;
    Ok(output)
}
