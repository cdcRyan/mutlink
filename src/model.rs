use serde::{Deserialize, Serialize};
use serde_json::Value;

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub public_key: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub last_seen: i64,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub id: String,
    pub version: String,
    pub description: String,
    pub risk: String,
    pub available: bool,
}

pub fn capabilities() -> Vec<Capability> {
    [
        ("agent.run", "Run an independent agent session", "approval"),
        ("files.list", "List workspace files", "read"),
        ("files.read", "Read a workspace file", "read"),
        ("files.search", "Search workspace text", "read"),
        ("files.write", "Write a workspace file", "approval"),
        (
            "shell.run",
            "Run a command with your OS user permissions",
            "approval",
        ),
    ]
    .into_iter()
    .map(|(id, description, risk)| Capability {
        id: id.into(),
        version: "1.0.0".into(),
        description: description.into(),
        risk: risk.into(),
        available: true,
    })
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub workspace: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub session_id: String,
    pub prompt: String,
    pub state: String,
    pub source_device: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: i64,
    pub task_id: String,
    pub session_id: String,
    pub kind: String,
    pub data: Value,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub task_id: String,
    pub tool: String,
    pub arguments: Value,
    pub state: String,
    pub created_at: i64,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    #[serde(default = "id")]
    pub id: String,
    pub session_id: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub plan: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRequest {
    pub id: String,
    pub action: String,
    pub payload: Value,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteResponse {
    pub id: String,
    pub ok: bool,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub device: Device,
    pub workspace: String,
    pub model: String,
    pub provider_ready: bool,
    pub relay: Option<String>,
    pub relay_connected: bool,
}
