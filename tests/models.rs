use axum::{extract::State, routing::post, Json, Router};
use mutlink::{
    agent::Agent,
    config::Config,
    model::*,
    models::{self, ModelCatalog},
    network::Network,
    server::{self, Rpc},
};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;

#[test]
fn imports_legacy_once_and_preserves_profiles_and_device_config() {
    let root = tempfile::tempdir().unwrap();
    let mut config = Config::load(root.path()).unwrap();
    config.provider.model = "legacy-model".into();
    config.save(root.path()).unwrap();
    let original = std::fs::read(root.path().join("config.json")).unwrap();
    let agent = Agent::open(root.path().to_path_buf()).unwrap();
    let imported = ModelCatalog::load(root.path(), &config.provider).unwrap();
    assert_eq!(imported.default, "default");
    assert_eq!(imported.active().unwrap().model, "legacy-model");
    models::execute(
        &agent,
        "model.set",
        &json!({
            "name":"local","model":"local-model","base_url":"http://127.0.0.1:1234/v1",
            "api_key_env":"LOCAL_MODEL_KEY"
        }),
    )
    .unwrap();
    let catalog = ModelCatalog::load(root.path(), &config.provider).unwrap();
    assert_eq!(catalog.models.len(), 2);
    assert_eq!(catalog.default, "local");
    assert_eq!(catalog.active().unwrap().max_steps, 24);
    assert_eq!(
        std::fs::read(root.path().join("config.json")).unwrap(),
        original
    );
    assert!(models::execute(&agent, "model.use", &json!({"name":"missing"})).is_err());
    assert_eq!(agent.status().model, "local-model");
    models::execute(&agent, "model.use", &json!({"name":"default"})).unwrap();
    assert_eq!(agent.status().model, "legacy-model");
    drop(agent);
    assert_eq!(
        Agent::open(root.path().to_path_buf())
            .unwrap()
            .status()
            .model,
        "legacy-model"
    );
}

#[test]
fn reloads_file_edits_and_rejects_invalid_configuration_without_overwriting() {
    let root = tempfile::tempdir().unwrap();
    let agent = Agent::open(root.path().to_path_buf()).unwrap();
    let provider = agent.config.read().unwrap().provider.clone();
    let mut catalog = ModelCatalog::load(root.path(), &provider).unwrap();
    let mut changed = provider.clone();
    changed.model = "edited-model".into();
    catalog.models.insert("edited".into(), changed);
    catalog.default = "edited".into();
    catalog.save(root.path()).unwrap();
    let list = models::execute(&agent, "model.list", &Value::Null).unwrap();
    assert_eq!(list.selected, "edited");
    assert_eq!(agent.status().model, "edited-model");
    let path = root.path().join("models.json");
    for invalid in [
        b"{".to_vec(),
        serde_json::to_vec(&json!({"default":"absent","models":{}})).unwrap(),
        serde_json::to_vec(&json!({"default":"bad","models":{"bad":{
            "model":"bad","base_url":"http://external.example/v1","api_key_env":"KEY","max_steps":24
        }}}))
        .unwrap(),
    ] {
        std::fs::write(&path, &invalid).unwrap();
        assert!(models::execute(&agent, "model.list", &Value::Null).is_err());
        assert!(models::execute(&agent, "model.use", &json!({"name":"edited"})).is_err());
        assert_eq!(agent.status().model, "edited-model");
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
    }
}

#[derive(Default)]
struct ModelState {
    entered: Notify,
    resume: Notify,
    calls: Mutex<Vec<String>>,
}

async fn mock(State(state): State<Arc<ModelState>>, Json(body): Json<Value>) -> String {
    state
        .calls
        .lock()
        .unwrap()
        .push(body["model"].as_str().unwrap().into());
    let last = body["messages"].as_array().unwrap().last().unwrap();
    let delta = if last["role"] == "user" && last["content"] == "hold" {
        state.entered.notify_one();
        state.resume.notified().await;
        json!({"tool_calls":[{"index":0,"id":"read","type":"function",
            "function":{"name":"list_files","arguments":"{}"}}]})
    } else {
        json!({"content":body["model"]})
    };
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":delta}]})
    )
}

async fn finished(agent: &Agent, id: &str) -> Task {
    tokio::time::timeout(Duration::from_secs(8), async {
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
async fn switching_changes_next_task_but_keeps_active_task_and_history() {
    let root = tempfile::tempdir().unwrap();
    let state = Arc::new(ModelState::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = Config::load(root.path()).unwrap();
    config.provider.base_url = format!("http://{}", listener.local_addr().unwrap());
    config.provider.model = "first-model".into();
    config.save(root.path()).unwrap();
    let router = Router::new()
        .route("/chat/completions", post(mock))
        .with_state(state.clone());
    let mock_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let agent = Agent::open(root.path().to_path_buf()).unwrap();
    let network = Network::new(agent.clone());
    let source = agent.status().device.id;
    let first = agent
        .submit(
            RunRequest {
                id: id(),
                session_id: None,
                prompt: "hold".into(),
                plan: true,
                delegation_depth: 0,
            },
            &source,
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), state.entered.notified())
        .await
        .unwrap();
    server::execute(&network, Rpc {
        action:"model.set".into(),
        payload:json!({"name":"second","model":"second-model","base_url":config.provider.base_url,
            "api_key_env":config.provider.api_key_env,"max_steps":24}),
        target:None,
    }).await.unwrap();
    assert_eq!(agent.status().model, "second-model");
    assert!(network
        .dispatch("peer", "model.use", json!({"name":"default"}))
        .is_err());
    state.resume.notify_one();
    assert_eq!(finished(&agent, &first.id).await.state, "succeeded");
    assert_eq!(
        agent.store.task_answer(&first.id).unwrap()["text"],
        "first-model"
    );
    let second = agent
        .submit(
            RunRequest {
                id: id(),
                session_id: Some(first.session_id.clone()),
                prompt: "next".into(),
                plan: true,
                delegation_depth: 0,
            },
            &source,
        )
        .unwrap();
    assert_eq!(finished(&agent, &second.id).await.state, "succeeded");
    assert_eq!(
        agent.store.task_answer(&second.id).unwrap()["text"],
        "second-model"
    );
    assert_eq!(
        *state.calls.lock().unwrap(),
        vec!["first-model", "first-model", "second-model"]
    );
    assert_eq!(agent.store.sessions().unwrap().len(), 1);
    drop(network);
    drop(agent);
    assert_eq!(
        Agent::open(root.path().to_path_buf())
            .unwrap()
            .status()
            .model,
        "second-model"
    );
    mock_task.abort();
}
