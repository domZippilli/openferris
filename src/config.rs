use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub user: UserConfig,
    pub llm: LlmConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub files: FilesConfig,
    pub telegram: Option<TelegramConfig>,
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

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    /// Telegram user IDs allowed to use the bot. If empty, anyone can use it.
    #[serde(default)]
    pub allowed_users: Vec<u64>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct FilesConfig {
    /// Extra directories the agent is allowed to read/write.
    /// The workspace directory (~/.local/share/openferris/workspace/) is always allowed.
    #[serde(default)]
    pub allowed_directories: Vec<String>,
}

/// Returns the list of directories the agent may read/write,
/// always including the default workspace.
pub fn allowed_directories(config: &FilesConfig) -> Vec<PathBuf> {
    let mut dirs = vec![data_dir().join("workspace")];
    for dir in &config.allowed_directories {
        // Expand ~ to home directory
        let expanded = if let Some(rest) = dir.strip_prefix("~/") {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(rest)
        } else {
            PathBuf::from(dir)
        };
        dirs.push(expanded);
    }
    dirs
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

/// Load user profile from ~/.local/share/openferris/USER.md, falling back to bundled default.
pub fn load_user() -> String {
    let user_file = data_dir().join("USER.md");
    if user_file.exists() {
        std::fs::read_to_string(&user_file).unwrap_or_default()
    } else {
        include_str!("../USER.md").to_string()
    }
}

/// Load identity from ~/.local/share/openferris/IDENTITY.md, falling back to bundled default.
pub fn load_identity() -> String {
    let user_identity = data_dir().join("IDENTITY.md");
    if user_identity.exists() {
        std::fs::read_to_string(&user_identity).unwrap_or_default()
    } else {
        include_str!("../IDENTITY.md").to_string()
    }
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
