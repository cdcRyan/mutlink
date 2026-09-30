use crate::{
    agent::Agent,
    config::{private_write, Provider},
    provider,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    pub default: String,
    pub models: BTreeMap<String, Provider>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelList {
    pub path: String,
    pub selected: String,
    pub models: Vec<ModelEntry>,
}

impl ModelCatalog {
    pub fn load(root: &Path, legacy: &Provider) -> Result<Self> {
        let path = root.join("models.json");
        let catalog = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<Self>(&bytes)
                .with_context(|| format!("Invalid model configuration: {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let catalog = Self {
                    default: "default".into(),
                    models: BTreeMap::from([("default".into(), legacy.clone())]),
                };
                catalog.save(root)?;
                catalog
            }
            Err(error) => {
                return Err(error).with_context(|| format!("Cannot read {}", path.display()))
            }
        };
        catalog
            .validate()
            .with_context(|| format!("Invalid model configuration: {}", path.display()))?;
        Ok(catalog)
    }

    pub fn active(&self) -> Result<&Provider> {
        self.models
            .get(&self.default)
            .context("Default model name is not in models")
    }

    fn validate(&self) -> Result<()> {
        self.active()?;
        for (name, model) in &self.models {
            if name.is_empty()
                || name.len() > 128
                || name.chars().any(char::is_whitespace)
                || name.chars().any(char::is_control)
            {
                bail!("Model names must contain 1–128 bytes without spaces or control characters");
            }
            model
                .validate()
                .with_context(|| format!("Invalid model profile: {name}"))?;
        }
        Ok(())
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        self.validate()?;
        private_write(&root.join("models.json"), &serde_json::to_vec_pretty(self)?)
    }
}

/// Reload the human-editable catalog before each operation. Only model settings
/// are applied; device identity, trust and workspace configuration stay in config.json.
pub fn execute(agent: &Agent, action: &str, payload: &Value) -> Result<ModelList> {
    let mut config = agent.config.write().unwrap();
    let mut catalog = ModelCatalog::load(&agent.root, &config.provider)?;
    match action {
        "model.use" => {
            let name = payload["name"].as_str().context("Missing model name")?;
            if !catalog.models.contains_key(name) {
                bail!("Unknown model profile: {name}. Use /model to list configured models.");
            }
            catalog.default = name.into();
            catalog.save(&agent.root)?;
        }
        "model.set" => {
            let model: Provider = serde_json::from_value(payload.clone())?;
            let name = payload["name"].as_str().unwrap_or(&model.model).to_owned();
            catalog.models.insert(name.clone(), model);
            catalog.default = name;
            catalog.save(&agent.root)?;
        }
        "model.list" => {}
        _ => bail!("Unknown model operation"),
    }
    config.provider = catalog.active()?.clone();
    Ok(ModelList {
        path: agent.root.join("models.json").display().to_string(),
        selected: catalog.default,
        models: catalog
            .models
            .into_iter()
            .map(|(name, model)| ModelEntry {
                name,
                ready: provider::ready(&model),
                model: model.model,
                base_url: model.base_url,
                api_key_env: model.api_key_env,
            })
            .collect(),
    })
}
