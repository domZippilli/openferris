use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub user: UserConfig,
    pub llm: LlmConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
}

#[derive(Debug, Deserialize)]
pub struct UserConfig {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub zip_code: Option<String>,
}

fn default_timezone() -> String {
    "UTC".to_string()
}

#[derive(Debug, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
    pub endpoint: String,
    pub model: Option<String>,
}

fn default_backend() -> String {
    "llamacpp".to_string()
}

#[derive(Debug, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:7700".to_string()
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("openferris")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("openferris")
}

pub fn load_config() -> Result<AppConfig> {
    let path = config_dir().join("config.toml");
    let content = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "Failed to read config: {}\nCreate it with your LLM endpoint.",
            path.display()
        )
    })?;
    let config: AppConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config: {}", path.display()))?;
    Ok(config)
}

pub fn load_soul() -> Result<String> {
    let user_soul = config_dir().join("SOUL.md");
    if user_soul.exists() {
        std::fs::read_to_string(&user_soul)
            .with_context(|| format!("Failed to read SOUL.md: {}", user_soul.display()))
    } else {
        Ok(include_str!("../SOUL.md").to_string())
    }
}
