use axum::{extract::State, routing::post, Json, Router};
use mutlink::{agent::Agent, config::Config, model::*, network::Network, server};
use serde_json::{json, Value};
use std::{
    sync::{Arc, RwLock},
    time::Duration,
};

async fn listen(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    (
        url,
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
    )
}

fn tool(name: &str, args: Value, index: usize) -> Value {
    json!({"tool_calls":[{
        "index":0,"id":format!("call-{index}"),"type":"function",
        "function":{"name":name,"arguments":args.to_string()}
    }]})
}

async fn model(State(target): State<Arc<RwLock<String>>>, Json(body): Json<Value>) -> String {
    let messages = body["messages"].as_array().unwrap();
    let start = messages.iter().rposition(|m| m["role"] == "user").unwrap();
    let prompt = messages[start]["content"].as_str().unwrap();
    let results: Vec<Value> = messages[start + 1..]
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect();
    let n = results.len();
    let peer = target.read().unwrap().clone();
    let ask = |prompt: &str| {
        tool(
            "ask_agent",
            json!({
                "device_id":peer,"prompt":prompt,"plan":false
            }),
            n,
        )
    };
    let read = || tool("read_file", json!({"path":"input.txt"}), n);
    let delta = match (prompt, n) {
        ("coordinate", 0) => tool("list_agents", json!({}), n),
        ("coordinate", 1) => {
            assert_eq!(results[0]["result"]["agents"][0]["device_id"], peer);
            ask("consult")
        }
        ("coordinate", 2) => {
            assert_eq!(results[1]["result"]["state"], "succeeded");
            assert!(results[1]["result"]["answer"]["text"]
                .as_str()
                .unwrap()
                .contains("context:"));
            tool(
                "ask_agent",
                json!({
                    "device_id":peer,"prompt":"followup",
                    "session_id":results[1]["result"]["session_id"]
                }),
                n,
            )
        }
        ("coordinate", 3) => {
            assert_eq!(
                results[2]["result"]["answer"]["text"],
                "followup retained context"
            );
            json!({"content":"collaboration complete"})
        }
        ("consult", 0) => read(),
        ("consult", 1) => ask("context"),
        ("consult", 2) => {
            assert_eq!(results[1]["result"]["state"], "succeeded");
            json!({"content":format!("{}; {}", results[0]["result"]["content"],
                results[1]["result"]["answer"]["text"])})
        }
        ("context", 0) => {
            assert!(!body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["function"]["name"] == "ask_agent"));
            // Even a model ignoring its schema cannot escape Plan mode.
            tool(
                "write_file",
                json!({"path":"forbidden.txt","content":"no"}),
                n,
            )
        }
        ("context", 1) => {
            assert_eq!(results[0]["ok"], false);
            ask("must not run")
        }
        ("context", 2) => {
            assert_eq!(results[1]["ok"], false);
            assert!(results[1]["error"].as_str().unwrap().contains("2-hop"));
            read()
        }
        ("context", 3) => {
            json!({"content":format!("context: {}", results[2]["result"]["content"])})
        }
        ("followup", 0) => {
            assert!(messages[..start].iter().any(|m| m["role"] == "assistant"
                && m["content"].as_str().unwrap_or("").contains("context:")));
            json!({"content":"followup retained context"})
        }
        ("coordinate write", 0) => ask("write"),
        ("write", 0) => tool(
            "write_file",
            json!({"path":"output.txt","content":"approved"}),
            n,
        ),
        ("write", 1) => json!({"content":"write completed"}),
        ("coordinate write", 1) => json!({"content":results[0].to_string()}),
        _ => panic!("unexpected model turn: {prompt}/{n}"),
    };
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":delta}]})
    )
}

struct Pair {
    a: Arc<Agent>,
    b: Arc<Agent>,
    an: Arc<Network>,
    bn: Arc<Network>,
    _roots: Vec<tempfile::TempDir>,
    jobs: Vec<tokio::task::JoinHandle<()>>,
}

impl Pair {
    async fn new() -> Self {
        let mut agents = Vec::new();
        let mut roots = Vec::new();
        let mut jobs = Vec::new();
        let mut targets = Vec::new();
        for name in ["A", "B"] {
            let target = Arc::new(RwLock::new(String::new()));
            let (url, job) = listen(
                Router::new()
                    .route("/chat/completions", post(model))
                    .with_state(target.clone()),
            )
            .await;
            let root = tempfile::tempdir().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            std::fs::write(workspace.path().join("input.txt"), name).unwrap();
            let mut config = Config::load(root.path()).unwrap();
            config.provider.base_url = url;
            config.workspace = workspace.path().to_path_buf();
            config.save(root.path()).unwrap();
            agents.push(Agent::open(root.path().to_path_buf()).unwrap());
            targets.push(target);
            roots.extend([root, workspace]);
            jobs.push(job);
        }
        let a = agents.remove(0);
        let b = agents.remove(0);
        let an = Network::new(a.clone());
        let bn = Network::new(b.clone());
        let (ae, aj) = listen(server::peer_router(an.clone())).await;
        let (be, bj) = listen(server::peer_router(bn.clone())).await;
        jobs.extend([aj, bj]);
        for (source, destination, endpoint, target) in
            [(&a, &b, be, &targets[0]), (&b, &a, ae, &targets[1])]
        {
            let mut device = destination.status().device;
            device.endpoint = Some(endpoint);
            *target.write().unwrap() = device.id.clone();
            source
                .config
                .write()
                .unwrap()
                .trust(device.clone(), &device.id, true)
                .unwrap();
        }
        Self {
            a,
            b,
            an,
            bn,
            _roots: roots,
            jobs,
        }
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        for agent in [&self.a, &self.b] {
            for token in agent.running.lock().unwrap().values() {
                token.cancel();
            }
        }
        for job in &self.jobs {
            job.abort();
        }
    }
}

fn submit(agent: &Arc<Agent>, prompt: &str, plan: bool) -> Task {
    agent
        .submit(
            RunRequest {
                id: id(),
                session_id: None,
                prompt: prompt.into(),
                plan,
                delegation_depth: 0,
            },
            &agent.status().device.id,
        )
        .unwrap()
}

async fn finished(agent: &Agent, id: &str) -> Task {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let task = agent.store.task(id).unwrap();
            if !matches!(
                task.state.as_str(),
                "queued" | "running" | "waiting_approval"
            ) {
                return task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn both_agents_delegate_follow_up_and_enforce_plan_and_depth() {
    let pair = Pair::new().await;
    for (source, peer, network) in [(&pair.a, &pair.b, &pair.bn), (&pair.b, &pair.a, &pair.an)] {
        let root = submit(source, "coordinate", true);
        assert_eq!(finished(source, &root.id).await.state, "succeeded");
        assert_eq!(
            source.store.task_answer(&root.id).unwrap()["text"],
            "collaboration complete"
        );
        let events = source.store.events(Some(&root.session_id), 0).unwrap();
        let children: Vec<_> = events
            .iter()
            .filter(|e| e.kind == "delegation_result")
            .collect();
        assert_eq!(children.len(), 2);
        assert_eq!(
            children[0].data["result"]["session_id"],
            children[1].data["result"]["session_id"]
        );
        assert!(network
            .remote(
                &source.status().device.id,
                "result",
                json!({"task_id":root.id})
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("belongs to another device"));
        for agent in [source, peer] {
            assert!(!agent
                .config
                .read()
                .unwrap()
                .workspace
                .join("forbidden.txt")
                .exists());
            assert!(agent.store.approvals().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn delegated_writes_require_peer_approval_and_parent_cancel_propagates() {
    let pair = Pair::new().await;
    for allow in [true, false] {
        let root = submit(&pair.a, "coordinate write", false);
        let approval = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Some(approval) = pair.b.store.approvals().unwrap().first() {
                    return approval.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let output = pair.b.config.read().unwrap().workspace.join("output.txt");
        assert!(!output.exists());
        assert!(pair
            .an
            .remote(
                &pair.b.status().device.id,
                "approve",
                json!({"id":approval.id,"allow":true})
            )
            .await
            .is_err());
        if allow {
            pair.b.store.decide(&approval.id, true).unwrap();
            assert_eq!(finished(&pair.a, &root.id).await.state, "succeeded");
            assert_eq!(std::fs::read_to_string(&output).unwrap(), "approved");
            std::fs::remove_file(output).unwrap();
        } else {
            pair.a.cancel(&root.id).unwrap();
            assert_eq!(finished(&pair.a, &root.id).await.state, "cancelled");
            assert_eq!(
                finished(&pair.b, &approval.task_id).await.state,
                "cancelled"
            );
            assert!(!output.exists());
        }
    }
}

#[tokio::test]
async fn peer_permission_and_wire_depth_are_enforced() {
    let pair = Pair::new().await;
    let source = pair.a.status().device.id;
    let target = pair.b.status().device.id;
    pair.b
        .config
        .write()
        .unwrap()
        .peers
        .get_mut(&source)
        .unwrap()
        .allow_agent = false;
    let root = submit(&pair.a, "coordinate write", false);
    assert_eq!(finished(&pair.a, &root.id).await.state, "succeeded");
    assert!(pair.b.store.tasks().unwrap().is_empty());
    assert!(pair.a.store.task_answer(&root.id).unwrap()["text"]
        .as_str()
        .unwrap()
        .contains("not authorized"));
    pair.b
        .config
        .write()
        .unwrap()
        .peers
        .get_mut(&source)
        .unwrap()
        .allow_agent = true;
    assert!(pair
        .an
        .remote(
            &target,
            "run",
            json!({
                "id":id(),"prompt":"write","delegation_depth":3,
            })
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("2-hop"));
    assert!(pair.b.store.tasks().unwrap().is_empty());
}

#[tokio::test]
async fn lost_submission_reply_retains_task_id_and_requests_cancellation() {
    async fn lose_reply(
        State(net): State<Arc<Network>>,
        Json(packet): Json<mutlink::crypto::Packet>,
    ) -> (axum::http::StatusCode, Json<Value>) {
        let value = mutlink::crypto::open(&net.agent.config.read().unwrap(), &packet).unwrap();
        let response = net.receive(packet).unwrap().unwrap();
        if value["body"]["action"] == "run" {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(Value::Null),
            )
        } else {
            (axum::http::StatusCode::OK, Json(json!(response)))
        }
    }
    let mut pair = Pair::new().await;
    let (endpoint, job) = listen(
        Router::new()
            .route("/peer", post(lose_reply))
            .with_state(pair.bn.clone()),
    )
    .await;
    pair.jobs.push(job);
    let peer = pair.b.status().device.id;
    pair.a
        .config
        .write()
        .unwrap()
        .peers
        .get_mut(&peer)
        .unwrap()
        .device
        .endpoint = Some(endpoint);
    let root = submit(&pair.a, "coordinate write", false);
    assert_eq!(finished(&pair.a, &root.id).await.state, "succeeded");
    let events = pair.a.store.events(Some(&root.session_id), 0).unwrap();
    let result = &events
        .iter()
        .find(|e| e.kind == "delegation_result")
        .unwrap()
        .data["result"];
    assert_eq!(result["state"], "unknown");
    assert_eq!(result["reconcile_before_retry"], true);
    assert_eq!(result["cancellation_requested"], true);
    let task_id = result["task_id"].as_str().unwrap();
    assert_eq!(finished(&pair.b, task_id).await.state, "cancelled");
    assert_eq!(pair.b.store.tasks().unwrap().len(), 1);
    assert!(!pair
        .b
        .config
        .read()
        .unwrap()
        .workspace
        .join("output.txt")
        .exists());
}
