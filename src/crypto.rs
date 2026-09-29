use crate::{
    config::{device_id, Config},
    model::{id, now},
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use crypto_box::{
    aead::{Aead, AeadCore},
    PublicKey, SalsaBox,
};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packet {
    pub id: String,
    pub from: String,
    pub to: String,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Serialize, Deserialize)]
struct Content {
    version: u8,
    id: String,
    from: String,
    to: String,
    issued_at: i64,
    expires_at: i64,
    payload: Value,
}

fn key(value: &str) -> Result<PublicKey> {
    let bytes: [u8; 32] = STANDARD
        .decode(value)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid public key"))?;
    Ok(PublicKey::from(bytes))
}

pub fn seal(config: &Config, to: &str, value: Value) -> Result<Packet> {
    let peer = config.peers.get(to).context("Device is not trusted")?;
    let cipher = SalsaBox::new(&key(&peer.device.public_key)?, &config.secret()?);
    let nonce = SalsaBox::generate_nonce(&mut OsRng);
    let content = Content {
        version: 1,
        id: id(),
        from: config.device.id.clone(),
        to: to.into(),
        issued_at: now(),
        expires_at: now() + 120,
        payload: value,
    };
    let ciphertext = cipher
        .encrypt(&nonce, serde_json::to_vec(&content)?.as_slice())
        .map_err(|_| anyhow::anyhow!("Encryption failed"))?;
    Ok(Packet {
        id: content.id,
        from: content.from,
        to: content.to,
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    })
}

pub fn open(config: &Config, packet: &Packet) -> Result<Value> {
    if packet.to != config.device.id || packet.ciphertext.len() > 2 * 1024 * 1024 {
        bail!("Invalid recipient or packet size");
    }
    let peer = config
        .peers
        .get(&packet.from)
        .context("Unknown or revoked sending device")?;
    if device_id(&peer.device.public_key)? != packet.from {
        bail!("Identity mismatch");
    }
    let nonce = STANDARD.decode(&packet.nonce)?;
    if nonce.len() != 24 {
        bail!("Invalid nonce");
    }
    let cipher = SalsaBox::new(&key(&peer.device.public_key)?, &config.secret()?);
    let clear = cipher
        .decrypt(
            nonce.as_slice().into(),
            STANDARD.decode(&packet.ciphertext)?.as_slice(),
        )
        .map_err(|_| anyhow::anyhow!("Packet authentication failed"))?;
    let body: Content = serde_json::from_slice(&clear)?;
    if body.version != 1
        || body.id != packet.id
        || body.from != packet.from
        || body.to != packet.to
        || body.expires_at <= now()
        || body.issued_at > now() + 30
        || body.expires_at - body.issued_at > 120
        || body.expires_at <= body.issued_at
    {
        bail!("Expired or invalid packet metadata");
    }
    Ok(body.payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn authenticated_payload_and_identity_cannot_be_substituted() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let mut alice = Config::load(a.path()).unwrap();
        let mut bob = Config::load(b.path()).unwrap();
        alice
            .trust(bob.device.clone(), &bob.device.id, true)
            .unwrap();
        bob.trust(alice.device.clone(), &alice.device.id, true)
            .unwrap();
        let packet = seal(&alice, &bob.device.id, json!({"task":"hello"})).unwrap();
        assert_eq!(open(&bob, &packet).unwrap()["task"], "hello");
        let mut tampered = packet.clone();
        tampered.id = id();
        assert!(open(&bob, &tampered).is_err());
        tampered = packet.clone();
        tampered.ciphertext.push('A');
        assert!(open(&bob, &tampered).is_err());
        bob.peers.clear();
        assert!(open(&bob, &packet).is_err());
    }
}
