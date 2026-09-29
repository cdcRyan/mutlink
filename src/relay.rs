use crate::{
    config::{device_id, random_token},
    crypto::Packet,
    model::{now, Device},
};
use anyhow::{bail, Result};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

pub struct Relay {
    db: Mutex<Connection>,
    allow_registration: bool,
    auth_slots: tokio::sync::Semaphore,
    attempts: Mutex<HashMap<String, (i64, u32)>>,
}

#[derive(Serialize, Deserialize)]
pub struct Credentials {
    pub account: String,
    pub password: String,
    pub device_id: String,
    #[serde(default)]
    pub register: bool,
}

pub struct Error(anyhow::Error);
impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Self(e)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self(e.into())
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Do not return database details, account existence or credentials.
        let _ = self.0;
        (StatusCode::BAD_REQUEST, Json(json!({"error":"Request rejected; check credentials, ownership and request limits"}))).into_response()
    }
}
type Api<T> = std::result::Result<Json<T>, Error>;
fn digest(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

impl Relay {
    pub fn open(path: &std::path::Path, allow_registration: bool) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS accounts(id TEXT PRIMARY KEY,password TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tokens(hash TEXT PRIMARY KEY,account TEXT NOT NULL,device TEXT NOT NULL,expires INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS devices(id TEXT PRIMARY KEY,account TEXT NOT NULL,body TEXT NOT NULL,seen INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS packets(id TEXT PRIMARY KEY,account TEXT NOT NULL,recipient TEXT NOT NULL,body TEXT NOT NULL,expires INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS packet_recipient ON packets(recipient,expires);")?;
        Ok(Arc::new(Self {
            db: Mutex::new(db),
            allow_registration,
            auth_slots: tokio::sync::Semaphore::new(4),
            attempts: Mutex::new(HashMap::new()),
        }))
    }

    fn identity(&self, headers: &HeaderMap) -> Result<(String, String)> {
        let token = headers
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| anyhow::anyhow!("Authentication required"))?;
        Ok(self.db.lock().unwrap().query_row(
            "SELECT account,device FROM tokens WHERE hash=?1 AND expires>?2",
            params![digest(token), now()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
}

pub fn router(relay: Arc<Relay>) -> Router {
    Router::new()
        .route(
            "/",
            get(|| async {
                asset(
                    "text/html; charset=utf-8",
                    include_str!("../web/index.html"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/app.js"),
                )
            }),
        )
        .route(
            "/style.css",
            get(|| async { asset("text/css; charset=utf-8", include_str!("../web/style.css")) }),
        )
        .route(
            "/nacl.js",
            get(|| async {
                asset(
                    "text/javascript; charset=utf-8",
                    include_str!("../web/vendor/nacl-fast.min.js"),
                )
            }),
        )
        .route("/auth", post(auth))
        .route("/devices", get(devices).post(register_device))
        .route("/packets", get(packets).post(send))
        .route("/packets/{id}", axum::routing::delete(ack))
        .route("/logout", post(logout))
        .layer(axum::extract::DefaultBodyLimit::max(2 * 1024 * 1024))
        .with_state(relay)
}

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    ([
        ("content-type", content_type),
        ("content-security-policy", "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
    ], body)
}

pub async fn serve(
    path: &std::path::Path,
    listen: SocketAddr,
    allow_registration: bool,
) -> Result<()> {
    let relay = Relay::open(path, allow_registration)?;
    let listener = tokio::net::TcpListener::bind(listen).await?;
    println!(
        "mutlink relay listening on {} (put TLS in front for remote access)",
        listener.local_addr()?
    );
    axum::serve(listener, router(relay))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

async fn auth(State(relay): State<Arc<Relay>>, Json(c): Json<Credentials>) -> Api<Value> {
    let _slot = relay
        .auth_slots
        .try_acquire()
        .map_err(|_| Error(anyhow::anyhow!("Busy")))?;
    let account = c.account.trim().to_lowercase();
    if account.is_empty()
        || account.len() > 128
        || c.password.len() < 12
        || c.password.len() > 1024
        || c.device_id.len() != 64
        || !c.device_id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error(anyhow::anyhow!("Invalid credentials")));
    }
    {
        let mut attempts = relay.attempts.lock().unwrap();
        attempts.retain(|_, (start, _)| *start > now() - 300);
        if attempts.len() >= 1024 && !attempts.contains_key(&account) {
            return Err(Error(anyhow::anyhow!("Authentication rate limit")));
        }
        let entry = attempts.entry(account.clone()).or_insert((now(), 0));
        if entry.1 >= 20 {
            return Err(Error(anyhow::anyhow!("Authentication rate limit")));
        }
        entry.1 += 1;
    }
    let hash: Option<String> = relay
        .db
        .lock()
        .unwrap()
        .query_row(
            "SELECT password FROM accounts WHERE id=?1",
            [&account],
            |r| r.get(0),
        )
        .optional()?;
    let password = c.password;
    let create = c.register && relay.allow_registration && hash.is_none();
    let encoded = tokio::task::spawn_blocking(move || -> Result<String> {
        if create {
            Ok(Argon2::default()
                .hash_password(
                    password.as_bytes(),
                    &SaltString::generate(&mut rand::rngs::OsRng),
                )
                .map_err(|_| anyhow::anyhow!("Hash failed"))?
                .to_string())
        } else {
            // Perform the same expensive hash work for unknown accounts.
            let hash = hash.unwrap_or_else(|| {
                Argon2::default()
                    .hash_password(
                        b"unavailable-account",
                        &SaltString::generate(&mut rand::rngs::OsRng),
                    )
                    .unwrap()
                    .to_string()
            });
            let parsed =
                PasswordHash::new(&hash).map_err(|_| anyhow::anyhow!("Invalid credentials"))?;
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .map_err(|_| anyhow::anyhow!("Invalid credentials"))?;
            Ok(hash)
        }
    })
    .await
    .map_err(|e| Error(e.into()))??;
    let token = random_token();
    let db = relay.db.lock().unwrap();
    if create {
        db.execute(
            "INSERT INTO accounts VALUES (?1,?2)",
            params![account, encoded],
        )?;
    }
    db.execute("DELETE FROM tokens WHERE expires<=?1", [now()])?;
    db.execute(
        "INSERT INTO tokens VALUES (?1,?2,?3,?4)",
        params![digest(&token), account, c.device_id, now() + 604800],
    )?;
    Ok(Json(
        json!({"token":token,"account":account,"expires_at":now()+604800}),
    ))
}

async fn register_device(
    State(relay): State<Arc<Relay>>,
    headers: HeaderMap,
    Json(mut device): Json<Device>,
) -> Api<Value> {
    let (account, id) = relay.identity(&headers)?;
    if device.id != id
        || device_id(&device.public_key)? != id
        || device.name.len() > 256
        || serde_json::to_vec(&device)
            .map_err(anyhow::Error::from)?
            .len()
            > 65536
    {
        return Err(Error(anyhow::anyhow!("Invalid device")));
    }
    device.last_seen = now();
    let db = relay.db.lock().unwrap();
    let owner: Option<String> = db
        .query_row("SELECT account FROM devices WHERE id=?1", [&id], |r| {
            r.get(0)
        })
        .optional()?;
    if owner.is_some_and(|v| v != account) {
        return Err(Error(anyhow::anyhow!("Device belongs to another account")));
    }
    db.execute("INSERT INTO devices VALUES (?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET body=excluded.body,seen=excluded.seen",
        params![id, account, serde_json::to_string(&device).map_err(anyhow::Error::from)?, now()])?;
    Ok(Json(json!({"ok":true})))
}

async fn devices(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Api<Value> {
    let (account, _) = relay.identity(&headers)?;
    let db = relay.db.lock().unwrap();
    let mut q =
        db.prepare("SELECT body,seen FROM devices WHERE account=?1 ORDER BY seen DESC LIMIT 100")?;
    let rows = q
        .query_map([account], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(json!(rows
        .into_iter()
        .map(|(body, seen)| json!({
            "device":serde_json::from_str::<Value>(&body).unwrap_or(Value::Null),
            "online":now()-seen<15
        }))
        .collect::<Vec<_>>())))
}

async fn send(
    State(relay): State<Arc<Relay>>,
    headers: HeaderMap,
    Json(packet): Json<Packet>,
) -> Api<Value> {
    let (account, sender) = relay.identity(&headers)?;
    let db = relay.db.lock().unwrap();
    let valid: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM devices WHERE id=?1 AND account=?2)",
        params![packet.to, account],
        |r| r.get(0),
    )?;
    if packet.from != sender || !valid || packet.id.len() > 128 {
        return Err(Error(anyhow::anyhow!("Invalid packet ownership")));
    }
    db.execute("DELETE FROM packets WHERE expires<=?1", [now()])?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM packets WHERE account=?1",
        [&account],
        |r| r.get(0),
    )?;
    if count >= 256 {
        return Err(Error(anyhow::anyhow!("Queue full")));
    }
    db.execute(
        "INSERT OR IGNORE INTO packets VALUES (?1,?2,?3,?4,?5)",
        params![
            packet.id,
            account,
            packet.to,
            serde_json::to_string(&packet).map_err(anyhow::Error::from)?,
            now() + 120
        ],
    )?;
    Ok(Json(json!({"queued":true})))
}

async fn packets(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Api<Vec<Packet>> {
    let (account, device) = relay.identity(&headers)?;
    let db = relay.db.lock().unwrap();
    let mut q = db.prepare(
        "SELECT body FROM packets WHERE account=?1 AND recipient=?2 AND expires>?3 LIMIT 20",
    )?;
    let rows = q
        .query_map(params![account, device, now()], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(
        rows.into_iter()
            .filter_map(|s| serde_json::from_str(&s).ok())
            .collect(),
    ))
}

async fn ack(
    State(relay): State<Arc<Relay>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Api<Value> {
    let (account, device) = relay.identity(&headers)?;
    relay.db.lock().unwrap().execute(
        "DELETE FROM packets WHERE id=?1 AND account=?2 AND recipient=?3",
        params![id, account, device],
    )?;
    Ok(Json(json!({"ok":true})))
}

async fn logout(State(relay): State<Arc<Relay>>, headers: HeaderMap) -> Api<Value> {
    let (account, device) = relay.identity(&headers)?;
    relay.db.lock().unwrap().execute(
        "DELETE FROM tokens WHERE account=?1 AND device=?2",
        params![account, device],
    )?;
    Ok(Json(json!({"ok":true})))
}

pub async fn login(url: &str, c: Credentials) -> Result<crate::config::RelayLogin> {
    crate::config::check_url(url)?;
    let value: Value = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?
        .post(format!("{}/auth", url.trim_end_matches('/')))
        .json(&c)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(token) = value["token"].as_str() else {
        bail!("Missing login token");
    };
    Ok(crate::config::RelayLogin {
        url: url.trim_end_matches('/').into(),
        token: token.into(),
        account: c.account.to_lowercase(),
    })
}
