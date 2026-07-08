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
    #[serde(default)]
    pub fetch: FetchConfig,
    #[serde(default)]
    pub gws: GwsConfig,
    pub search: Option<SearchConfig>,
    pub firecrawl: Option<FirecrawlConfig>,
    pub camoufox: Option<CamoufoxConfig>,
    pub telegram: Option<TelegramConfig>,
    pub gmail: Option<GmailConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchConfig {
    /// SearXNG (or compatible) JSON search endpoint, e.g. "http://127.0.0.1:8888".
    /// The tool appends "/search?format=json&q=..." to this base.
    pub endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FirecrawlConfig {
    /// Firecrawl API base, e.g. "http://127.0.0.1:3002". The tool POSTs to
    /// {endpoint}/v1/scrape.
    pub endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CamoufoxConfig {
    /// Camoufox stealth-fetch API base, e.g. "http://127.0.0.1:8765".
    /// The tool POSTs to {endpoint}/fetch.
    pub endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UserConfig {
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// The owner's email address(es), used to resolve the per-counterparty
    /// message thread (storage.rs) for inbound/outbound email: mail to/from
    /// one of these addresses lands in the shared "owner" thread rather than
    /// a per-address "email:<addr>" thread. Deliberately separate from
    /// `[gmail].allowed_senders`, which may include non-owner senders
    /// authorized to email the agent but who aren't the owner.
    #[serde(default)]
    pub emails: Vec<String>,
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
    /// Sampling temperature sent with chat completion requests.
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    /// Restrict sampling to the top K tokens. Set by config/env to match the
    /// local vLLM MTP benchmark defaults unless explicitly overridden.
    #[serde(default = "default_top_k")]
    pub top_k: u32,
    /// Pass `enable_thinking=true` through chat_template_kwargs for backends
    /// like vLLM/Gemma4 that require explicit opt-in to reasoning channels.
    #[serde(default)]
    pub enable_thinking: bool,
    /// Number of parallel slots on OpenAI-compatible servers that support slot pinning.
    /// Set >1 to enable subagent support (parent uses slot 0, subagents use 1+).
    #[serde(default = "default_parallel_slots")]
    pub parallel_slots: u32,
}

fn default_backend() -> String {
    "openai_compat".to_string()
}

fn default_parallel_slots() -> u32 {
    1
}

fn default_temperature() -> f32 {
    0.6
}

fn default_top_k() -> u32 {
    20
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

#[derive(Debug, Default, Deserialize, Clone)]
pub struct FetchConfig {
    /// Local/internal-network ports that fetch_url is permitted to reach.
    /// fetch_url normally blocks loopback/private addresses to avoid SSRF;
    /// this allowlist punches a hole for known-safe local services like the
    /// Quartz wiki on 8088. Only the *port* matters — any internal address
    /// reaching one of these ports is allowed.
    #[serde(default)]
    pub allowed_local_ports: Vec<u16>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct GwsConfig {
    /// Allow the generic gws tool to run `drive files delete` and
    /// `drive files trash`. Other destructive Workspace operations stay blocked.
    #[serde(default)]
    pub allow_drive_file_deletes: bool,
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
    // Deliberately does NOT consult the socket pointer file: the daemon uses
    // this value to decide where to BIND, and the pointer records where some
    // previous daemon (or a test-run daemon on a tempdir path) bound —
    // trusting it here once crash-looped the daemon on a stale temp path.
    // Clients that need pointer fallback (notably cron, which lacks
    // $XDG_RUNTIME_DIR) read socket_pointer_path() explicitly after the
    // primary path fails to connect (see main.rs Run/Goal).
    // Prefer XDG_RUNTIME_DIR (per-user, tmpfs, correct permissions).
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

/// Path to the file the daemon writes on startup with its actual bound socket
/// path. Clients (esp. cron, which lacks `$XDG_RUNTIME_DIR`) can fall back to
/// this when the env-derived `default_socket()` path doesn't match the daemon.
pub fn socket_pointer_path() -> PathBuf {
    data_dir().join("daemon.socket.path")
}

/// Path to the SQLite database (interactions, messages, wakeups, etc.).
/// Centralizes what used to be `data_dir().join("openferris.db")` written out
/// at each call site.
pub fn db_path() -> PathBuf {
    data_dir().join("openferris.db")
}

pub fn load_config() -> Result<AppConfig> {
    let path = config_dir().join("config.toml");
    let content = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "Failed to read config: {}\nCreate it with your LLM endpoint.",
            path.display()
        )
    })?;
    let mut config: AppConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config: {}", path.display()))?;
    warn_unknown_keys(&content);
    warn_config_footguns(&config);
    if let Ok(value) = std::env::var("OPENFERRIS_LLM_TEMPERATURE") {
        config.llm.temperature = value.parse().with_context(|| {
            format!("Failed to parse OPENFERRIS_LLM_TEMPERATURE={value:?} as f32")
        })?;
    }
    if let Ok(value) = std::env::var("OPENFERRIS_LLM_TOP_K") {
        config.llm.top_k = value
            .parse()
            .with_context(|| format!("Failed to parse OPENFERRIS_LLM_TOP_K={value:?} as u32"))?;
    }
    Ok(config)
}

/// Top-level config tables/keys the typed `AppConfig` knows about. Kept in
/// sync with `AppConfig`'s fields; used by `warn_unknown_keys` to catch typos
/// (like the README's old `listen` key) without the hard-fail forward-compat
/// cost of `deny_unknown_fields`.
const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "user",
    "llm",
    "daemon",
    "files",
    "fetch",
    "gws",
    "search",
    "firecrawl",
    "camoufox",
    "telegram",
    "gmail",
];

/// Warn about top-level config keys the typed struct will silently ignore.
/// Shallow by design: only the top level is checked.
fn warn_unknown_keys(content: &str) {
    // The typed parse already succeeded by the time this runs, so a raw-parse
    // failure here can't really happen; bail quietly if it somehow does.
    let Ok(value) = content.parse::<toml::Value>() else {
        return;
    };
    let Some(table) = value.as_table() else {
        return;
    };
    for key in table.keys() {
        if !KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            tracing::warn!(
                "Unknown key '{}' in config.toml — it is ignored (typo?)",
                key
            );
        }
    }
}

/// Warn about configurations that parse fine but are probably not what the
/// user wants.
fn warn_config_footguns(config: &AppConfig) {
    if let Some(tg) = &config.telegram
        && tg.allowed_users.is_empty()
    {
        tracing::warn!(
            "[telegram] is configured with empty allowed_users — anyone can message the bot"
        );
    }
    if config.gmail.is_some() && config.user.emails.is_empty() {
        tracing::warn!(
            "[gmail] is configured but [user] emails is empty — the owner's email address \
             won't resolve to the owner thread"
        );
    }
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

pub fn load_soul() -> Result<String> {
    let user_soul = config_dir().join("SOUL.md");
    if user_soul.exists() {
        std::fs::read_to_string(&user_soul)
            .with_context(|| format!("Failed to read SOUL.md: {}", user_soul.display()))
    } else {
        Ok(include_str!("../SOUL.md").to_string())
    }
}
