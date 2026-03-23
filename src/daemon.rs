use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use openferris::agent::{Agent, AgentResult};
use openferris::config::{self, AppConfig};
use openferris::llm::{ChatMessage, Role};
use openferris::protocol::{DaemonRequest, DaemonResponse, RequestKind, ResponseKind};
use openferris::skills;
use openferris::storage::Storage;

use crate::memories::Memories;

struct QueuedRequest {
    request: DaemonRequest,
    session_history: Vec<ChatMessage>,
    response_tx: oneshot::Sender<DaemonResponse>,
}

/// Data needed to log an interaction after the agent finishes.
struct LogData {
    source: String,
    skill: Option<String>,
    user_message: String,
    result: AgentResult,
}

pub async fn run(config: AppConfig, agent: Agent, storage: Storage, memories: Memories) -> Result<()> {
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
    tracing::info!("OpenFerris daemon listening on {}", socket_path);

    let (tx, mut rx) = mpsc::unbounded_channel::<QueuedRequest>();

    // Single worker task — processes requests sequentially, owns storage and memories.
    let worker_agent = agent.clone();
    let user_skills_dir = config::config_dir().join("skills");
    tokio::spawn(async move {
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

            // Async: run agent
            let (response, log_data) = process_request(
                &worker_agent,
                &queued,
                &user_skills_dir,
                &identity,
                &user_profile,
                &persistent_context,
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
            let mut session_history: Vec<ChatMessage> = vec![];

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

                        let is_freeform =
                            matches!(request.kind, RequestKind::FreeformMessage { .. });
                        let user_text = match &request.kind {
                            RequestKind::FreeformMessage { text } => Some(text.clone()),
                            _ => None,
                        };

                        let (resp_tx, resp_rx) = oneshot::channel();
                        let queued = QueuedRequest {
                            request,
                            session_history: if is_freeform {
                                session_history.clone()
                            } else {
                                vec![]
                            },
                            response_tx: resp_tx,
                        };

                        if tx.send(queued).is_err() {
                            tracing::error!("Worker channel closed");
                            break;
                        }

                        match resp_rx.await {
                            Ok(response) => {
                                // Update session history for freeform conversations
                                if is_freeform {
                                    if let Some(text) = user_text {
                                        session_history.push(ChatMessage {
                                            role: Role::User,
                                            content: text,
                                        });
                                    }
                                    if let ResponseKind::Done { ref text } = response.kind {
                                        session_history.push(ChatMessage {
                                            role: Role::Assistant,
                                            content: text.clone(),
                                        });
                                    }
                                }
                                let _ = write_response(&mut writer, &response).await;
                            }
                            Err(_) => {
                                tracing::error!("Worker dropped response channel");
                                break;
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
    user_skills_dir: &std::path::Path,
    identity: &str,
    user_profile: &str,
    persistent_context: &str,
) -> (DaemonResponse, Option<LogData>) {
    let request_id = queued.request.id.clone();
    let source = queued
        .request
        .source
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    match &queued.request.kind {
        RequestKind::RunSkill { skill_name, context } => {
            match skills::load_skill(skill_name, user_skills_dir) {
                Ok(skill) => {
                    let msg = match context {
                        Some(ctx) => format!("Execute the {} skill now.\n\n{}", skill_name, ctx),
                        None => format!("Execute the {} skill now.", skill_name),
                    };
                    match agent.run(&skill, &msg, &[], identity, user_profile, persistent_context).await {
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
            }
        }
        RequestKind::FreeformMessage { text } => {
            match skills::load_skill("default", user_skills_dir) {
                Ok(skill) => {
                    match agent
                        .run(&skill, text, &queued.session_history, identity, user_profile, persistent_context)
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
        // StoreMemory is handled directly in the worker loop above.
        RequestKind::StoreMemory { .. } => unreachable!(),
    }
}
