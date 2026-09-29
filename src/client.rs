use crate::{
    config::Config,
    server::{DaemonInfo, Rpc},
};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{path::Path, process::Stdio, time::Duration};

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    url: String,
    token: String,
}
impl Client {
    pub fn connect(root: &Path) -> Result<Self> {
        let info: DaemonInfo = serde_json::from_slice(&std::fs::read(root.join("daemon.json"))?)?;
        let url = reqwest::Url::parse(&info.url)?;
        if url.host_str() != Some("127.0.0.1") {
            bail!("Daemon API must be on loopback");
        }
        let cfg = Config::load(root)?;
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(65))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            url: info.url,
            token: cfg.local_token,
        })
    }

    pub async fn ensure(root: &Path) -> Result<Self> {
        if let Ok(c) = Self::connect(root) {
            if c.call("status", Value::Null, None).await.is_ok() {
                return Ok(c);
            }
        }
        std::fs::create_dir_all(root)?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("daemon.log"))?;
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("--home")
            .arg(root)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x00000008 | 0x00000200);
        }
        let mut child = command.spawn().context("Could not start agent daemon")?;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(c) = Self::connect(root) {
                if c.call("status", Value::Null, None).await.is_ok() {
                    // Reap the child when this client remains alive; daemon continues when the client exits.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return Ok(c);
                }
            }
            if let Some(status) = child.try_wait()? {
                // Another simultaneous client may have won the daemon lock.
                if !status.success() {
                    if let Ok(c) = Self::connect(root) {
                        if c.call("status", Value::Null, None).await.is_ok() {
                            return Ok(c);
                        }
                    }
                    bail!(
                        "Agent failed to start; inspect {}",
                        root.join("daemon.log").display()
                    );
                }
            }
        }
        bail!(
            "Agent startup timed out; inspect {}",
            root.join("daemon.log").display()
        )
    }

    pub async fn call(&self, action: &str, payload: Value, target: Option<&str>) -> Result<Value> {
        let response = self
            .http
            .post(format!("{}/rpc", self.url))
            .bearer_auth(&self.token)
            .json(&Rpc {
                action: action.into(),
                payload,
                target: target.map(str::to_string),
            })
            .send()
            .await?;
        let status = response.status();
        let value: Value = response.json().await?;
        if !status.is_success() {
            bail!(
                "{}",
                value["error"].as_str().unwrap_or("Agent request failed")
            );
        }
        Ok(value)
    }
}
