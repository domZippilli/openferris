use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use openferris::agent::{Agent, AgentResult};
use openferris::config::{self, AppConfig};
use openferris::llm::{ChatMessage, Role};
use openferris::protocol::{
    AgentNotification, DaemonRequest, DaemonResponse, RequestKind, ResponseKind,
};
use openferris::skills;
use openferris::storage::Storage;

use crate::memories::Memories;

struct QueuedRequest {
    request: DaemonRequest,
    response_tx: oneshot::Sender<DaemonResponse>,
    progress_tx: mpsc::UnboundedSender<AgentNotification>,
}

/// Fraction of the model's context window the stored per-session history is
/// allowed to occupy. The rest is left for the system prompt, persistent
/// context, the current turn, and tool round-trips (the agent still compacts
/// in-run as a backstop). Half keeps a fresh turn comfortably within budget.
const HISTORY_TOKEN_FRACTION: f32 = 0.5;
const GOAL_SKILL_NAME: &str = "goal-pursuit";
const GOAL_MAX_TURNS_HARD: usize = 25;

/// Drop whole oldest turns (user+assistant pairs) from the front until the
/// estimated token count fits `budget`. Always keeps at least the most recent
/// pair so a follow-up still has its immediate antecedent.
fn trim_history(history: &mut Vec<ChatMessage>, budget: usize) {
    while history.len() > 2 && openferris::agent::estimate_tokens(history) > budget {
        history.drain(0..2);
    }
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
    let socket_path = &config.daemon.socket;
    // Remove stale socket file from a previous run
    if std::path::Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    // Restrict socket to owner only
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))?;
    }
    // Publish the resolved socket path so CLI clients that compute a different
    // default (e.g. cron without $XDG_RUNTIME_DIR) can fall back to the real one.
    let pointer = config::socket_pointer_path();
    if let Some(parent) = pointer.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&pointer, socket_path) {
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
        // Per-conversation history, keyed by request.session_id. Lives for the
        // life of the daemon process (in-memory; cleared on restart) so a chat
        // stays coherent across the separate connections clients open per
        // message. The single worker owns it, so no locking is needed.
        let mut sessions: HashMap<String, Vec<ChatMessage>> = HashMap::new();
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

            // Conversation continuity: only freeform messages carrying a
            // session_id thread together. Pull the prior turns for this session
            // (a clone, so the worker can update the canonical copy afterward).
            let session_key = match &queued.request.kind {
                RequestKind::FreeformMessage { .. } => queued.request.session_id.clone(),
                _ => None,
            };
            let history: Vec<ChatMessage> = session_key
                .as_ref()
                .and_then(|k| sessions.get(k).cloned())
                .unwrap_or_default();

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

            // Record this turn so the session stays coherent on the next
            // message. Only on success: a failed turn shouldn't poison future
            // context, and skipping it keeps user/assistant pairs aligned.
            if let (Some(key), RequestKind::FreeformMessage { text }) =
                (&session_key, &queued.request.kind)
                && let ResponseKind::Done { text: ref reply } = response.kind
            {
                let budget = match worker_agent.context_window_tokens().await {
                    Ok(n) => ((n as f32) * HISTORY_TOKEN_FRACTION) as usize,
                    Err(e) => {
                        tracing::warn!("Could not size history budget: {}; using 50_000", e);
                        50_000
                    }
                };
                let entry = sessions.entry(key.clone()).or_default();
                entry.push(ChatMessage {
                    role: Role::User,
                    content: text.clone(),
                });
                entry.push(ChatMessage {
                    role: Role::Assistant,
                    content: reply.clone(),
                });
                trim_history(entry, budget);
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
                                            // Session history now lives in the
                                            // worker (keyed by session_id), so
                                            // the connection just relays the
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
