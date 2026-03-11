use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};

use crate::agent::{Agent, AgentResult};
use crate::config::{self, AppConfig};
use crate::llm::{ChatMessage, Role};
use crate::protocol::{DaemonRequest, DaemonResponse, RequestKind, ResponseKind};
use crate::skills;
use crate::storage::Storage;

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

pub async fn run(config: AppConfig, agent: Agent, storage: Storage) -> Result<()> {
    let agent = Arc::new(agent);
    let listener = TcpListener::bind(&config.daemon.listen).await?;
    tracing::info!("OpenFerris daemon listening on {}", config.daemon.listen);

    let (tx, mut rx) = mpsc::unbounded_channel::<QueuedRequest>();

    // Single worker task — processes requests sequentially, owns storage.
    // Storage is accessed only before/after the async agent call (never across .await).
    let worker_agent = agent.clone();
    let user_skills_dir = config::config_dir().join("skills");
    tokio::spawn(async move {
        while let Some(queued) = rx.recv().await {
            // Sync: load persistent context from storage
            let persistent_context = match storage.build_context() {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::warn!("Failed to load context: {}", e);
                    String::new()
                }
            };

            // Async: run agent
            let (response, log_data) = process_request(
                &worker_agent,
                &queued,
                &user_skills_dir,
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
                    if let Err(e) = storage.store_memory(memory) {
                        tracing::warn!("Failed to store memory: {}", e);
                    }
                }
            }

            let _ = queued.response_tx.send(response);
        }
    });

    // Accept connections
    loop {
        let (stream, addr) = listener.accept().await?;
        tracing::info!("Client connected: {}", addr);
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
                        tracing::info!("Client disconnected: {}", addr);
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
                        tracing::error!("Read error from {}: {}", addr, e);
                        break;
                    }
                }
            }
        });
    }
}

async fn write_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
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
    persistent_context: &str,
) -> (DaemonResponse, Option<LogData>) {
    let request_id = queued.request.id.clone();
    let source = queued
        .request
        .source
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    match &queued.request.kind {
        RequestKind::RunSkill { skill_name } => {
            match skills::load_skill(skill_name, user_skills_dir) {
                Ok(skill) => {
                    let msg = format!("Execute the {} skill now.", skill_name);
                    match agent.run(&skill, &msg, &[], persistent_context).await {
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
            match skills::load_skill("triage", user_skills_dir) {
                Ok(skill) => {
                    match agent
                        .run(&skill, text, &queued.session_history, persistent_context)
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
    }
}
