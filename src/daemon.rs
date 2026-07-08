use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use openferris::agent::{Agent, AgentResult};
use openferris::config::{self, AppConfig};
use openferris::counterparty;
use openferris::llm::{ChatMessage, Role};
use openferris::protocol::{
    AgentNotification, DaemonRequest, DaemonResponse, RequestKind, ResponseKind,
};
use openferris::skills;
use openferris::storage::{self, Storage};

use crate::memories::Memories;

struct QueuedRequest {
    request: DaemonRequest,
    response_tx: oneshot::Sender<DaemonResponse>,
    progress_tx: mpsc::UnboundedSender<AgentNotification>,
}

/// Fraction of the model's context window the stored per-counterparty thread
/// history is allowed to occupy. The rest is left for the system prompt,
/// persistent context, the current turn, and tool round-trips (the agent
/// still compacts in-run as a backstop). Half keeps a fresh turn comfortably
/// within budget.
const HISTORY_TOKEN_FRACTION: f32 = 0.5;
/// Rough chars-per-token conversion, matching `agent::estimate_tokens`'s
/// chars/4 heuristic — `Storage::load_thread` budgets in chars, not tokens,
/// since it never tokenizes.
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;
/// Fallback token budget when the backend's context window can't be queried.
const DEFAULT_HISTORY_TOKEN_BUDGET: usize = 50_000;
const GOAL_SKILL_NAME: &str = "goal-pursuit";
const GOAL_MAX_TURNS_HARD: usize = 25;

/// Resolve the DB-thread counterparty for a `FreeformMessage`'s session_id.
///
/// TUI sessions (`tui:<uuid>`) and anything else not recognized below are
/// treated as the owner — the only two owner-facing interactive surfaces
/// today are Telegram and the TUI, and the TUI has no per-chat identity to
/// check. Telegram sessions encode the chat_id as `telegram:<chat_id>` and
/// resolve through the configured allowlist/default chat (see
/// `openferris::counterparty::telegram_counterparty`); by the time a message
/// reaches the daemon the transport has usually already rejected
/// non-allowlisted chats, so this mostly resolves to the owner too.
fn resolve_counterparty(session_id: &str, config: &AppConfig) -> String {
    if let Some(chat_id_str) = session_id.strip_prefix("telegram:") {
        if let (Ok(chat_id), Some(tg)) = (chat_id_str.parse::<i64>(), config.telegram.as_ref()) {
            return counterparty::telegram_counterparty(
                chat_id,
                tg.default_chat_id,
                &tg.allowed_users,
            );
        }
        // No [telegram] config or an unparseable chat id: still bucket by
        // chat rather than defaulting to the owner thread.
        return format!("telegram:{}", chat_id_str);
    }
    counterparty::OWNER.to_string()
}

/// Data needed to log an interaction after the agent finishes.
struct LogData {
    source: String,
    skill: Option<String>,
    user_message: String,
    result: AgentResult,
}

pub async fn run(
    config: AppConfig,
    agent: Agent,
    storage: Storage,
    memories: Memories,
) -> Result<()> {
    let agent = Arc::new(agent);
    // Owned (not borrowed from `config`) so `config` can move wholesale into
    // the worker task below, which needs it to resolve counterparties.
    let socket_path = config.daemon.socket.clone();
    // Remove stale socket file from a previous run
    if std::path::Path::new(&socket_path).exists() {
        std::fs::remove_file(&socket_path)?;
    }
    let listener = UnixListener::bind(&socket_path)?;
    // Restrict socket to owner only
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    // Publish the resolved socket path so CLI clients that compute a different
    // default (e.g. cron without $XDG_RUNTIME_DIR) can fall back to the real one.
    let pointer = config::socket_pointer_path();
    if let Some(parent) = pointer.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&pointer, &socket_path) {
        tracing::warn!(
            "Failed to write socket pointer file {}: {}",
            pointer.display(),
            e
        );
    }
    tracing::info!("OpenFerris daemon listening on {}", socket_path);

    let (tx, mut rx) = mpsc::unbounded_channel::<QueuedRequest>();

    // Single worker task — processes requests sequentially, owns storage and memories.
    let worker_agent = agent.clone();
    let user_skills_dir = config::config_dir().join("skills");
    tokio::spawn(async move {
        // `config` moves in wholesale: the worker needs `config.telegram` to
        // resolve counterparties (see `resolve_counterparty`). Conversation
        // history itself now lives in `storage`'s `messages` table, keyed by
        // counterparty rather than transport session — it survives daemon
        // restarts and is shared across channels for the same counterparty.
        let config = config;
        while let Some(queued) = rx.recv().await {
            let request_id = queued.request.id.clone();

            // Handle StoreMemory directly — no agent needed.
            if let RequestKind::StoreMemory { ref content } = queued.request.kind {
                let response = match memories.add(content) {
                    Ok(()) => {
                        tracing::info!("Manual memory stored: {}", content);
                        DaemonResponse {
                            request_id,
                            kind: ResponseKind::Done {
                                text: format!("Remembered: {}", content),
                            },
                        }
                    }
                    Err(e) => DaemonResponse {
                        request_id,
                        kind: ResponseKind::Error {
                            message: format!("Failed to store memory: {}", e),
                        },
                    },
                };
                let _ = queued.response_tx.send(response);
                continue;
            }

            // Sync: load persistent context from storage + memories
            let interaction_context = match storage.build_context() {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::warn!("Failed to load interaction context: {}", e);
                    String::new()
                }
            };

            let memory_context = match memories.load_for_prompt() {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::warn!("Failed to load memories: {}", e);
                    String::new()
                }
            };

            let persistent_context = format!("{}{}", memory_context, interaction_context);

            // Sync: load identity and user profile (re-read each time so edits take effect)
            let identity = config::load_identity();
            let user_profile = config::load_user();

            // Conversation continuity: only freeform messages from an owner
            // surface (Telegram, TUI) thread together, keyed by the resolved
            // counterparty rather than the raw session_id — so the same
            // person is coherent across channels, and restarts don't wipe it.
            let counterparty: Option<String> = match &queued.request.kind {
                RequestKind::FreeformMessage { .. } => queued
                    .request
                    .session_id
                    .as_deref()
                    .map(|sid| resolve_counterparty(sid, &config)),
                _ => None,
            };

            let history: Vec<ChatMessage> = match &counterparty {
                Some(cp) => {
                    let token_budget = match worker_agent.context_window_tokens().await {
                        Ok(n) => ((n as f32) * HISTORY_TOKEN_FRACTION) as usize,
                        Err(e) => {
                            tracing::warn!(
                                "Could not size history budget: {}; using {}",
                                e,
                                DEFAULT_HISTORY_TOKEN_BUDGET
                            );
                            DEFAULT_HISTORY_TOKEN_BUDGET
                        }
                    };
                    let char_budget = token_budget * CHARS_PER_TOKEN_ESTIMATE;
                    match storage.load_thread(cp, char_budget) {
                        Ok(h) => h,
                        Err(e) => {
                            tracing::warn!("Failed to load thread for {}: {}", cp, e);
                            Vec::new()
                        }
                    }
                }
                None => Vec::new(),
            };

            // Async: run agent
            let progress_tx = queued.progress_tx.clone();
            let (response, log_data) = process_request(
                &worker_agent,
                &queued,
                &history,
                &user_skills_dir,
                &identity,
                &user_profile,
                &persistent_context,
                progress_tx,
            )
            .await;

            // Sync: log interaction and store memories
            if let Some(data) = log_data {
                if let Err(e) = storage.log_interaction(
                    &data.source,
                    data.skill.as_deref(),
                    &data.user_message,
                    &data.result.response,
                ) {
                    tracing::warn!("Failed to log interaction: {}", e);
                }
                for memory in &data.result.memories {
                    tracing::info!("Storing memory: {}", memory);
                    if let Err(e) = memories.add(memory) {
                        tracing::warn!("Failed to store memory: {}", e);
                    }
                }
            }

            // Thread persistence: append user-visible turns to the messages
            // table. Only on success: a failed turn shouldn't poison future
            // context. Never persists tool calls/results/intermediate
            // iterations — only the inbound text and the final response.
            if let ResponseKind::Done { text: ref reply } = response.kind {
                let source = queued
                    .request
                    .source
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                match &queued.request.kind {
                    RequestKind::FreeformMessage { text } => {
                        if let Some(cp) = &counterparty {
                            if let Err(e) = storage.append_message(
                                cp,
                                &source,
                                storage::DIRECTION_INBOUND,
                                storage::KIND_CHAT,
                                text,
                            ) {
                                tracing::warn!(
                                    "Failed to append inbound message to thread {}: {}",
                                    cp,
                                    e
                                );
                            }
                            if let Err(e) = storage.append_message(
                                cp,
                                &source,
                                storage::DIRECTION_OUTBOUND,
                                storage::KIND_CHAT,
                                reply,
                            ) {
                                tracing::warn!(
                                    "Failed to append outbound message to thread {}: {}",
                                    cp,
                                    e
                                );
                            }
                        }
                    }
                    RequestKind::RunSkill { .. } | RequestKind::PursueGoal { .. } => {
                        // Scheduled/skill runs and goal pursuits don't run
                        // with the owner thread as history (they start from
                        // their skill prompt + the interaction annex), but
                        // their final response is recorded to the owner
                        // thread as a run_note: audited, but excluded from
                        // thread rendering so it doesn't duplicate whatever
                        // the run may have already sent via a delivery tool.
                        if let Err(e) = storage.append_message(
                            counterparty::OWNER,
                            &source,
                            storage::DIRECTION_OUTBOUND,
                            storage::KIND_RUN_NOTE,
                            reply,
                        ) {
                            tracing::warn!("Failed to append run note to owner thread: {}", e);
                        }
                    }
                    // StoreMemory is handled before the worker queue and
                    // never reaches here; if that ever changes, there's
                    // nothing to persist to a thread — don't panic the
                    // worker over it.
                    RequestKind::StoreMemory { .. } => {}
                }
            }

            let _ = queued.response_tx.send(response);
        }
    });

    // Accept connections
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::error!("Failed to accept connection: {}", e);
                continue;
            }
        };
        tracing::info!("Client connected");
        let tx = tx.clone();

        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();

            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        tracing::info!("Client disconnected");
                        break;
                    }
                    Ok(_) => {
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            continue;
                        }

                        let request: DaemonRequest = match serde_json::from_str(trimmed) {
                            Ok(r) => r,
                            Err(e) => {
                                let resp = DaemonResponse {
                                    request_id: "unknown".to_string(),
                                    kind: ResponseKind::Error {
                                        message: format!("Invalid request: {}", e),
                                    },
                                };
                                let _ = write_response(&mut writer, &resp).await;
                                continue;
                            }
                        };

                        tracing::debug!("Daemon request: {:?}", request.kind);

                        let request_id = request.id.clone();
                        let (resp_tx, resp_rx) = oneshot::channel();
                        let (prog_tx, mut prog_rx) = mpsc::unbounded_channel::<AgentNotification>();
                        let queued = QueuedRequest {
                            request,
                            response_tx: resp_tx,
                            progress_tx: prog_tx,
                        };

                        if tx.send(queued).is_err() {
                            tracing::error!("Worker channel closed");
                            break;
                        }

                        // Read progress updates and the final response concurrently.
                        // The agent fires progress via prog_tx (non-blocking) while
                        // the worker sends the final response via resp_tx (oneshot).
                        let mut resp_rx = resp_rx;
                        let mut finished = false;
                        while !finished {
                            tokio::select! {
                                Some(notif) = prog_rx.recv() => {
                                    let resp = DaemonResponse {
                                        request_id: request_id.clone(),
                                        kind: notification_to_response_kind(notif),
                                    };
                                    let _ = write_response(&mut writer, &resp).await;
                                }
                                result = &mut resp_rx => {
                                    // Drain any remaining notifications
                                    while let Ok(notif) = prog_rx.try_recv() {
                                        let resp = DaemonResponse {
                                            request_id: request_id.clone(),
                                            kind: notification_to_response_kind(notif),
                                        };
                                        let _ = write_response(&mut writer, &resp).await;
                                    }
                                    match result {
                                        Ok(response) => {
                                            // Thread history now lives in
                                            // storage (keyed by resolved
                                            // counterparty), so the
                                            // connection just relays the
                                            // response to the client.
                                            let _ = write_response(&mut writer, &response).await;
                                        }
                                        Err(_) => {
                                            tracing::error!("Worker dropped response channel");
                                        }
                                    }
                                    finished = true;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Client read error: {}", e);
                        break;
                    }
                }
            }
        });
    }
}

fn notification_to_response_kind(n: AgentNotification) -> ResponseKind {
    match n {
        AgentNotification::ToolProgress(text) => ResponseKind::Progress { text },
        AgentNotification::AssistantChunk(text) => ResponseKind::AssistantChunk { text },
    }
}

async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &DaemonResponse,
) -> Result<()> {
    let mut data = serde_json::to_string(response)?;
    data.push('\n');
    writer.write_all(data.as_bytes()).await?;
    Ok(())
}

/// Run the agent and return (response for client, optional log data for storage).
/// Takes `persistent_context` as a pre-built String so storage isn't held across .await.
async fn process_request(
    agent: &Agent,
    queued: &QueuedRequest,
    history: &[ChatMessage],
    user_skills_dir: &std::path::Path,
    identity: &str,
    user_profile: &str,
    persistent_context: &str,
    progress_tx: mpsc::UnboundedSender<AgentNotification>,
) -> (DaemonResponse, Option<LogData>) {
    let request_id = queued.request.id.clone();
    let source = queued
        .request
        .source
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    match &queued.request.kind {
        RequestKind::RunSkill {
            skill_name,
            context,
        } => match skills::load_skill(skill_name, user_skills_dir) {
            Ok(skill) => {
                let msg = match context {
                    Some(ctx) => format!("Execute the {} skill now.\n\n{}", skill_name, ctx),
                    None => format!("Execute the {} skill now.", skill_name),
                };
                match agent
                    .run(
                        &skill,
                        &msg,
                        &[],
                        identity,
                        user_profile,
                        persistent_context,
                        Some(progress_tx.clone()),
                    )
                    .await
                {
                    Ok(result) => {
                        let log = LogData {
                            source,
                            skill: Some(skill_name.clone()),
                            user_message: msg,
                            result: result.clone(),
                        };
                        let response = DaemonResponse {
                            request_id,
                            kind: ResponseKind::Done {
                                text: result.response,
                            },
                        };
                        (response, Some(log))
                    }
                    Err(e) => (
                        DaemonResponse {
                            request_id,
                            kind: ResponseKind::Error {
                                message: format!("{:#}", e),
                            },
                        },
                        None,
                    ),
                }
            }
            Err(e) => (
                DaemonResponse {
                    request_id,
                    kind: ResponseKind::Error {
                        message: format!("{:#}", e),
                    },
                },
                None,
            ),
        },
        RequestKind::FreeformMessage { text } => {
            match skills::load_skill("default", user_skills_dir) {
                Ok(skill) => {
                    match agent
                        .run(
                            &skill,
                            text,
                            history,
                            identity,
                            user_profile,
                            persistent_context,
                            Some(progress_tx.clone()),
                        )
                        .await
                    {
                        Ok(result) => {
                            let log = LogData {
                                source,
                                skill: None,
                                user_message: text.clone(),
                                result: result.clone(),
                            };
                            let response = DaemonResponse {
                                request_id,
                                kind: ResponseKind::Done {
                                    text: result.response,
                                },
                            };
                            (response, Some(log))
                        }
                        Err(e) => (
                            DaemonResponse {
                                request_id,
                                kind: ResponseKind::Error {
                                    message: format!("{:#}", e),
                                },
                            },
                            None,
                        ),
                    }
                }
                Err(e) => (
                    DaemonResponse {
                        request_id,
                        kind: ResponseKind::Error {
                            message: format!("{:#}", e),
                        },
                    },
                    None,
                ),
            }
        }
        RequestKind::PursueGoal {
            exit_criteria,
            max_turns,
        } => match skills::load_skill(GOAL_SKILL_NAME, user_skills_dir) {
            Ok(skill) => match pursue_goal(
                agent,
                &skill,
                exit_criteria,
                *max_turns,
                identity,
                user_profile,
                persistent_context,
                progress_tx.clone(),
            )
            .await
            {
                Ok(result) => {
                    let log = LogData {
                        source,
                        skill: Some(GOAL_SKILL_NAME.to_string()),
                        user_message: format!("Pursue goal with exit criteria: {}", exit_criteria),
                        result: result.clone(),
                    };
                    let response = DaemonResponse {
                        request_id,
                        kind: ResponseKind::Done {
                            text: result.response,
                        },
                    };
                    (response, Some(log))
                }
                Err(e) => (
                    DaemonResponse {
                        request_id,
                        kind: ResponseKind::Error {
                            message: format!("{:#}", e),
                        },
                    },
                    None,
                ),
            },
            Err(e) => (
                DaemonResponse {
                    request_id,
                    kind: ResponseKind::Error {
                        message: format!("{:#}", e),
                    },
                },
                None,
            ),
        },
        // StoreMemory is handled directly in the worker loop above.
        RequestKind::StoreMemory { .. } => unreachable!(),
    }
}

async fn pursue_goal(
    agent: &Agent,
    skill: &openferris::skills::Skill,
    exit_criteria: &str,
    requested_max_turns: usize,
    identity: &str,
    user_profile: &str,
    persistent_context: &str,
    progress_tx: mpsc::UnboundedSender<AgentNotification>,
) -> Result<AgentResult> {
    if requested_max_turns == 0 || requested_max_turns > GOAL_MAX_TURNS_HARD {
        anyhow::bail!("max_turns must be between 1 and {}", GOAL_MAX_TURNS_HARD);
    }

    let mut history: Vec<ChatMessage> = Vec::new();
    let mut final_memories: Vec<String> = Vec::new();
    let mut last_response = String::new();

    for turn in 1..=requested_max_turns {
        let _ = progress_tx.send(AgentNotification::ToolProgress(format!(
            "Goal turn {}/{}...",
            turn, requested_max_turns
        )));

        let prompt = if turn == 1 {
            format!(
                "Pursue this goal.\n\nExit criteria:\n{}\n\nYou have at most {} inference turns. Work on the goal now.",
                exit_criteria, requested_max_turns
            )
        } else {
            format!(
                "Continue pursuing the same goal.\n\nExit criteria:\n{}\n\nThis is inference turn {} of {}. Continue from the prior work.",
                exit_criteria, turn, requested_max_turns
            )
        };

        // Do not stream assistant prose for goal mode: intermediate turns are
        // working state, and TUI clients suppress the final Done text once any
        // AssistantChunk has rendered.
        let result = agent
            .run(
                skill,
                &prompt,
                &history,
                identity,
                user_profile,
                persistent_context,
                None,
            )
            .await?;

        final_memories.extend(result.memories);
        let status = extract_goal_status(&result.response);
        let clean_response = strip_goal_status(&result.response);
        last_response = clean_response.clone();

        history.push(ChatMessage {
            role: Role::User,
            content: prompt,
        });
        history.push(ChatMessage {
            role: Role::Assistant,
            content: result.response,
        });

        if matches!(status.as_deref(), Some("done")) {
            return Ok(AgentResult {
                response: clean_response,
                memories: final_memories,
            });
        }
    }

    Ok(AgentResult {
        response: format!(
            "Stopped after reaching the max turn limit ({}).\n\n{}",
            requested_max_turns, last_response
        ),
        memories: final_memories,
    })
}

fn extract_goal_status(text: &str) -> Option<String> {
    let start = text.find("<goal_status>")? + "<goal_status>".len();
    let end = text[start..].find("</goal_status>")? + start;
    Some(text[start..end].trim().to_ascii_lowercase())
}

fn strip_goal_status(text: &str) -> String {
    let Some(start) = text.find("<goal_status>") else {
        return text.trim().to_string();
    };
    let Some(rel_end) = text[start..].find("</goal_status>") else {
        return text.trim().to_string();
    };
    let end = start + rel_end + "</goal_status>".len();
    let mut out = String::new();
    out.push_str(&text[..start]);
    out.push_str(&text[end..]);
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openferris::config::{
        CamoufoxConfig, DaemonConfig, FetchConfig, FilesConfig, FirecrawlConfig, GwsConfig,
        LlmConfig, SearchConfig, UserConfig,
    };
    use openferris::llm::mock::MockLlm;
    use openferris::llm::{ChunkCallback, LlmBackend};
    use openferris::tools::ToolRegistry;
    use std::sync::Arc;

    /// Resolver tests for `resolve_counterparty`. The Telegram/email-specific
    /// mapping rules themselves are covered in `openferris::counterparty`'s
    /// own tests; this only checks the session_id-parsing glue that's local
    /// to the daemon.
    fn minimal_config(socket: String) -> AppConfig {
        AppConfig {
            user: UserConfig {
                timezone: "UTC".to_string(),
                zip_code: None,
                emails: vec![],
            },
            llm: LlmConfig {
                backend: "mock".to_string(),
                endpoint: String::new(),
                model: None,
                temperature: 0.6,
                top_k: 20,
                enable_thinking: false,
                parallel_slots: 1,
            },
            daemon: DaemonConfig { socket },
            files: FilesConfig::default(),
            fetch: FetchConfig::default(),
            gws: GwsConfig::default(),
            search: None::<SearchConfig>,
            firecrawl: None::<FirecrawlConfig>,
            camoufox: None::<CamoufoxConfig>,
            telegram: None,
            gmail: None,
        }
    }

    #[test]
    fn test_resolve_counterparty_tui_is_owner() {
        let config = minimal_config("/tmp/unused.sock".to_string());
        assert_eq!(
            resolve_counterparty("tui:some-uuid", &config),
            counterparty::OWNER
        );
    }

    #[test]
    fn test_resolve_counterparty_telegram_without_config_buckets_by_chat() {
        let config = minimal_config("/tmp/unused.sock".to_string());
        assert_eq!(
            resolve_counterparty("telegram:555", &config),
            "telegram:555"
        );
    }

    /// A `LlmBackend` that forwards to a shared `Arc<MockLlm>`, so a test can
    /// keep its own handle to the mock (to inspect `messages_at`/`call_count`)
    /// even though `Agent::new` takes ownership of a `Box<dyn LlmBackend>`.
    struct ArcMockLlm(Arc<MockLlm>);

    #[async_trait::async_trait]
    impl LlmBackend for ArcMockLlm {
        async fn chat_completion(&self, messages: &[ChatMessage]) -> Result<String> {
            self.0.chat_completion(messages).await
        }

        async fn chat_completion_stream(
            &self,
            messages: &[ChatMessage],
            on_chunk: ChunkCallback<'_>,
        ) -> Result<String> {
            self.0.chat_completion_stream(messages, on_chunk).await
        }

        async fn context_window_tokens(&self) -> Result<usize> {
            self.0.context_window_tokens().await
        }
    }

    /// End-to-end: an outbound send that landed in the owner's thread before
    /// this turn started (simulating what `send_telegram`/`send_email` append
    /// on every delivery — see tools/telegram.rs, email.rs) is visible to the
    /// LLM as history on the *next* FreeformMessage from an owner surface.
    /// This is the coherence bug from the refactor plan: previously, only the
    /// in-memory per-session_id history fed the model, so a prior outbound
    /// send never appeared in what the model saw on a follow-up turn.
    #[tokio::test]
    async fn test_prior_outbound_send_appears_in_next_turn_history() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("openferris.db");
        let socket_path = tmp.path().join("daemon.sock");

        let storage = Storage::open(&db_path).unwrap();
        // Simulate a prior send_telegram call: an outbound chat turn in the
        // owner thread, with no FreeformMessage round trip involved at all.
        storage
            .append_message(
                counterparty::OWNER,
                "telegram",
                storage::DIRECTION_OUTBOUND,
                storage::KIND_CHAT,
                &storage::outbound_tag("telegram", "Don't forget the 3pm meeting"),
            )
            .unwrap();

        let mock = Arc::new(MockLlm::new(vec![
            "Sure — you asked about the meeting I mentioned.".to_string(),
        ]));
        let agent = Agent::new(
            Box::new(ArcMockLlm(mock.clone())),
            ToolRegistry::new(),
            String::new(),
        );
        let memories = Memories::new(tmp.path().join("MEMORIES.md"));

        let config = minimal_config(socket_path.to_string_lossy().to_string());

        tokio::spawn(run(config, agent, storage, memories));

        // The listener binds synchronously near the top of `run`, but give it
        // a moment to actually get there before the first connection attempt.
        let socket_str = socket_path.to_string_lossy().to_string();
        let mut connected = false;
        for _ in 0..100 {
            if tokio::net::UnixStream::connect(&socket_str).await.is_ok() {
                connected = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(connected, "daemon never came up on {}", socket_str);

        let request = DaemonRequest {
            id: uuid::Uuid::new_v4().to_string(),
            kind: RequestKind::FreeformMessage {
                text: "What did you send me earlier?".to_string(),
            },
            source: Some("tui".to_string()),
            session_id: Some("tui:test-session".to_string()),
        };

        let reply = crate::client::send_request(&socket_str, &request)
            .await
            .unwrap();
        assert_eq!(reply, "Sure — you asked about the meeting I mentioned.");

        // The one (and only) chat_completion call must have had the prior
        // outbound send in its message list.
        assert_eq!(mock.call_count(), 1);
        let sent_messages = mock.messages_at(0).unwrap();
        let found = sent_messages
            .iter()
            .any(|m| m.content.contains("Don't forget the 3pm meeting"));
        assert!(
            found,
            "expected the prior outbound send in the LLM's message list, got: {:#?}",
            sent_messages
        );
    }
}
