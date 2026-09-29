use axum::{extract::State, routing::post, Json, Router};
use mutlink::{
    agent::Agent,
    config::{Config, RelayLogin},
    crypto,
    model::*,
    network::Network,
    relay, server,
};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

async fn listen(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", socket.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
    (url, task)
}

async fn mock_model(State(count): State<Arc<AtomicUsize>>, Json(body): Json<Value>) -> String {
    let turn = count.fetch_add(1, Ordering::SeqCst);
    assert_eq!(body["stream"], true);
    let delta = if turn == 0 {
        json!({"tool_calls":[{"index":0,"id":"call-read","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"input.txt\"}"}}]})
    } else if turn == 1 {
        let messages = body["messages"].as_array().unwrap();
        assert!(messages
            .iter()
            .any(|m| m["role"] == "tool" && m["content"].as_str().unwrap_or("").contains("hello")));
        json!({"tool_calls":[{"index":0,"id":"call-write","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"output.txt\",\"content\":\"checked\"}"}}]})
    } else {
        assert!(body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call-write"));
        json!({"content":"Finished after checking the file."})
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"delta":{},"finish_reason":if turn<2{"tool_calls"}else{"stop"}}]})
    )
}

async fn provider() -> (String, tokio::task::JoinHandle<()>) {
    listen(
        Router::new()
            .route("/chat/completions", post(mock_model))
            .with_state(Arc::new(AtomicUsize::new(0))),
    )
    .await
}

fn agent(root: &std::path::Path, workspace: &std::path::Path, url: &str) -> Arc<Agent> {
    let mut cfg = Config::load(root).unwrap();
    cfg.workspace = workspace.to_path_buf();
    cfg.provider.base_url = url.into();
    cfg.save(root).unwrap();
    Agent::open(root.to_path_buf()).unwrap()
}

async fn until_approval(a: &Agent) -> Approval {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Some(approval) = a.store.approvals().unwrap().first() {
                return approval.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("agent should request approval")
}
async fn finished(a: &Agent, id: &str) -> Task {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let t = a.store.task(id).unwrap();
            if !matches!(t.state.as_str(), "queued" | "running" | "waiting_approval") {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
fn trust(a: &Arc<Agent>, b: &Arc<Agent>, endpoint: Option<String>) {
    let mut device = b.status().device;
    device.endpoint = endpoint;
    let id = device.id.clone();
    a.config.write().unwrap().trust(device, &id, true).unwrap();
}

#[tokio::test]
async fn real_stream_tool_loop_blocks_write_until_exact_approval() {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(work.path().join("input.txt"), "hello").unwrap();
    let (url, mock) = provider().await;
    let a = agent(root.path(), work.path(), &url);
    let source = a.status().device.id;
    let request = RunRequest {
        id: id(),
        session_id: None,
        prompt: "Read input, then write output".into(),
        plan: false,
    };
    let t = a.submit(request.clone(), &source).unwrap();
    let approval = until_approval(&a).await;
    assert!(!work.path().join("output.txt").exists());
    assert_eq!(a.submit(request, &source).unwrap().id, t.id);
    a.store.decide(&approval.id, true).unwrap();
    assert_eq!(finished(&a, &t.id).await.state, "succeeded");
    assert_eq!(
        std::fs::read_to_string(work.path().join("output.txt")).unwrap(),
        "checked"
    );
    let events = a.store.events(Some(&t.session_id), 0).unwrap();
    assert!(events.iter().any(|e| e.kind == "delta"));
    assert!(events.iter().any(|e| e.kind == "metrics"));
    mock.abort();
}

#[tokio::test]
async fn plan_mode_and_rejected_approval_cannot_modify_files() {
    for plan in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        std::fs::write(work.path().join("input.txt"), "hello").unwrap();
        let (url, mock) = provider().await;
        let a = agent(root.path(), work.path(), &url);
        let t = a
            .submit(
                RunRequest {
                    id: id(),
                    session_id: None,
                    prompt: "test".into(),
                    plan,
                },
                &a.status().device.id,
            )
            .unwrap();
        if !plan {
            let p = until_approval(&a).await;
            a.store.decide(&p.id, false).unwrap();
        }
        assert_eq!(finished(&a, &t.id).await.state, "succeeded");
        assert!(a
            .store
            .events(Some(&t.session_id), 0)
            .unwrap()
            .iter()
            .any(|e| e.kind == "tool_result" && e.data["output"]["ok"] == false));
        assert!(!work.path().join("output.txt").exists());
        mock.abort();
    }
}

#[tokio::test]
async fn direct_two_agents_enforce_identity_replay_and_task_ownership() {
    let ar = tempfile::tempdir().unwrap();
    let br = tempfile::tempdir().unwrap();
    let aw = tempfile::tempdir().unwrap();
    let bw = tempfile::tempdir().unwrap();
    std::fs::write(bw.path().join("input.txt"), "hello").unwrap();
    let (url, mock) = provider().await;
    let a = agent(ar.path(), aw.path(), &url);
    let b = agent(br.path(), bw.path(), &url);
    let bn = Network::new(b.clone());
    let (endpoint, peer) = listen(server::peer_router(bn.clone())).await;
    trust(&a, &b, Some(endpoint));
    trust(&b, &a, None);
    let an = Network::new(a.clone());
    let target = b.status().device.id;
    let status = an.remote(&target, "status", Value::Null).await.unwrap();
    assert_eq!(status["device"]["id"], target);
    let request = RunRequest {
        id: id(),
        session_id: None,
        prompt: "remote task".into(),
        plan: false,
    };
    let t: Task =
        serde_json::from_value(an.remote(&target, "run", json!(request)).await.unwrap()).unwrap();
    let approval = until_approval(&b).await;
    assert!(an
        .remote(&target, "approve", json!({"id":approval.id,"allow":true}))
        .await
        .is_err());
    assert!(!bw.path().join("output.txt").exists());
    b.store.decide(&approval.id, true).unwrap();
    assert_eq!(finished(&b, &t.id).await.state, "succeeded");
    assert_eq!(
        an.remote(&target, "task", json!({"task_id":t.id}))
            .await
            .unwrap()["state"],
        "succeeded"
    );
    assert!(bn
        .dispatch("other-device", "events", json!({"task_id":t.id}))
        .is_err());
    let packet=crypto::seal(&a.config.read().unwrap(),&target,json!({"kind":"request","body":RemoteRequest{id:id(),action:"status".into(),payload:Value::Null,expires_at:now()+60}})).unwrap();
    assert!(bn.receive(packet.clone()).is_ok());
    assert!(bn.receive(packet).is_err());
    b.config.write().unwrap().peers.clear();
    assert!(an.remote(&target, "status", Value::Null).await.is_err());
    peer.abort();
    mock.abort();
}

async fn login(url: &str, device: &str, account: &str, register: bool) -> RelayLogin {
    relay::login(
        url,
        relay::Credentials {
            account: account.into(),
            password: "test-password-12345".into(),
            device_id: device.into(),
            register,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn relay_roundtrip_and_account_isolation() {
    let db = tempfile::tempdir().unwrap();
    let service = relay::Relay::open(&db.path().join("relay.db"), true).unwrap();
    let (url, relay_task) = listen(relay::router(service)).await;
    let ar = tempfile::tempdir().unwrap();
    let br = tempfile::tempdir().unwrap();
    let a = Agent::open(ar.path().to_path_buf()).unwrap();
    let b = Agent::open(br.path().to_path_buf()).unwrap();
    trust(&a, &b, None);
    trust(&b, &a, None);
    let ai = a.status().device.id;
    let bi = b.status().device.id;
    let al = login(&url, &ai, "alice", true).await;
    let bl = login(&url, &bi, "alice", false).await;
    a.config.write().unwrap().relay = Some(al.clone());
    b.config.write().unwrap().relay = Some(bl.clone());
    let an = Network::new(a.clone());
    let bn = Network::new(b.clone());
    let worker_a = tokio::spawn(an.clone().worker());
    let worker_b = tokio::spawn(bn.clone().worker());
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if a.relay_connected.load(Ordering::Relaxed)
                && b.relay_connected.load(Ordering::Relaxed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        an.remote(&bi, "status", Value::Null).await.unwrap()["device"]["id"],
        bi
    );
    let devices = an.devices().await.unwrap();
    assert_eq!(devices["discovered"].as_array().unwrap().len(), 2);
    let outsider = login(&url, &"f".repeat(64), "mallory", true).await;
    let http = reqwest::Client::new();
    let external: Value = http
        .get(format!("{url}/devices"))
        .bearer_auth(&outsider.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(external, json!([]));
    let packet = crypto::seal(&a.config.read().unwrap(), &bi, json!({"test":true})).unwrap();
    assert!(!http
        .post(format!("{url}/packets"))
        .bearer_auth(&outsider.token)
        .json(&packet)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(!http
        .get(format!("{url}/devices"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    worker_a.abort();
    worker_b.abort();
    relay_task.abort();
}

#[tokio::test]
async fn local_api_requires_token_and_rejects_browser_origin() {
    let root = tempfile::tempdir().unwrap();
    let a = Agent::open(root.path().to_path_buf()).unwrap();
    let token = a.config.read().unwrap().local_token.clone();
    let (url, task) = listen(server::local_router(Network::new(a))).await;
    let c = reqwest::Client::new();
    let rpc = json!({"action":"status"});
    assert!(!c
        .post(format!("{url}/rpc"))
        .json(&rpc)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(!c
        .post(format!("{url}/rpc"))
        .bearer_auth(&token)
        .header("origin", "https://evil.example")
        .json(&rpc)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(c
        .post(format!("{url}/rpc"))
        .bearer_auth(token)
        .json(&rpc)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    task.abort();
}
