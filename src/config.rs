use crate::model::{capabilities, now, Device};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use crypto_box::SecretKey;
use directories::ProjectDirs;
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    pub max_steps: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayLogin {
    pub url: String,
    pub token: String,
    pub account: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub device: Device,
    pub allow_agent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub device: Device,
    pub secret_key: String,
    pub local_token: String,
    pub workspace: PathBuf,
    pub provider: Provider,
    #[serde(default)]
    pub peers: BTreeMap<String, Peer>,
    #[serde(default)]
    pub relay: Option<RelayLogin>,
}

pub fn home(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("MUTLINK_HOME") {
        return Ok(PathBuf::from(path));
    }
    Ok(ProjectDirs::from("dev", "mutlink", "mutlink")
        .context("Cannot locate user configuration directory")?
        .data_local_dir()
        .to_path_buf())
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn device_id(public_key: &str) -> Result<String> {
    let bytes = STANDARD.decode(public_key)?;
    if bytes.len() != 32 {
        bail!("Invalid device public key");
    }
    Ok(hex::encode(Sha256::digest(bytes)))
}

pub fn private_write(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing parent directory")?;
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let temp = parent.join(format!(".{}.tmp", random_token()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    use std::io::Write;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    // Windows rename cannot replace an existing file; config writes retain a backup.
    #[cfg(windows)]
    if path.exists() {
        let backup = path.with_extension("backup");
        if backup.exists() {
            fs::remove_file(&backup)?;
        }
        fs::rename(path, &backup)?;
    }
    fs::rename(&temp, path)?;
    Ok(())
}

impl Config {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join("config.json");
        if path.exists() {
            return Ok(serde_json::from_slice(&fs::read(path)?)?);
        }
        let secret = SecretKey::generate(&mut OsRng);
        let public_key = STANDARD.encode(secret.public_key().as_bytes());
        let name = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "My computer".into());
        let config = Self {
            device: Device {
                id: device_id(&public_key)?,
                name,
                platform: format!("{} / {}", std::env::consts::OS, std::env::consts::ARCH),
                public_key,
                endpoint: None,
                last_seen: now(),
                capabilities: capabilities(),
            },
            secret_key: STANDARD.encode(secret.to_bytes()),
            local_token: random_token(),
            workspace: std::env::current_dir()?.canonicalize()?,
            provider: Provider {
                base_url: "https://api.openai.com/v1".into(),
                model: "gpt-4.1".into(),
                api_key_env: "OPENAI_API_KEY".into(),
                max_steps: 24,
            },
            peers: BTreeMap::new(),
            relay: None,
        };
        config.save(root)?;
        Ok(config)
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        private_write(&root.join("config.json"), &serde_json::to_vec_pretty(self)?)
    }

    pub fn secret(&self) -> Result<SecretKey> {
        let bytes: [u8; 32] = STANDARD
            .decode(&self.secret_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid private key"))?;
        Ok(SecretKey::from(bytes))
    }

    pub fn trust(&mut self, device: Device, fingerprint: &str, allow_agent: bool) -> Result<()> {
        let expected = device_id(&device.public_key)?;
        if device.id != expected || fingerprint != expected {
            bail!("Fingerprint does not match the device public key");
        }
        if expected == self.device.id {
            bail!("Cannot pair a device with itself");
        }
        self.peers.insert(
            expected,
            Peer {
                device,
                allow_agent,
            },
        );
        Ok(())
    }
}

pub fn check_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)?;
    let local = matches!(
        parsed.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    );
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && local) {
        bail!("HTTPS is required outside loopback");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        bail!("URL must not contain credentials or fragments");
    }
    Ok(())
}
