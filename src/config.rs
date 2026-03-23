use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub user: UserConfig,
    pub llm: LlmConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub files: FilesConfig,
    pub telegram: Option<TelegramConfig>,
    pub gmail: Option<GmailConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserConfig {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    pub zip_code: Option<String>,
}

fn default_timezone() -> String {
    "UTC".to_string()
}

#[derive(Debug, Clone, Deserialize)]
pub struct LlmConfig {
    #[serde(default = "default_backend")]
    pub backend: String,
    pub endpoint: String,
    pub model: Option<String>,
    /// Number of parallel slots on the llama.cpp server.
    /// Set >1 to enable subagent support (parent uses slot 0, subagents use 1+).
    #[serde(default = "default_parallel_slots")]
    pub parallel_slots: u32,
}

fn default_backend() -> String {
    "llamacpp".to_string()
}

fn default_parallel_slots() -> u32 {
    1
}

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    /// Path to the Unix domain socket.
    #[serde(default = "default_socket")]
    pub socket: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket: default_socket(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    /// Telegram user IDs allowed to use the bot. If empty, anyone can use it.
    #[serde(default)]
    pub allowed_users: Vec<u64>,
    /// Default chat ID for outbound messages (e.g., skill-initiated notifications).
    pub default_chat_id: Option<i64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct GmailConfig {
    /// Email addresses allowed to trigger auto-replies.
    #[serde(default)]
    pub allowed_senders: Vec<String>,
    /// Poll interval in seconds (default 60).
    #[serde(default = "default_gmail_poll_interval")]
    pub poll_interval_secs: u64,
    /// Seconds between replies to the same thread (default 300).
    #[serde(default = "default_gmail_rate_limit")]
    pub rate_limit_secs: u64,
    /// Email address to always CC on outbound emails.
    pub always_cc: Option<String>,
}

fn default_gmail_poll_interval() -> u64 {
    60
}

fn default_gmail_rate_limit() -> u64 {
    300
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

fn default_socket() -> String {
    // Prefer XDG_RUNTIME_DIR (per-user, tmpfs, correct permissions)
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        format!("{}/openferris.sock", runtime_dir)
    } else {
        format!("{}/openferris.sock", data_dir().display())
    }
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
