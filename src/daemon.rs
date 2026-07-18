use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

use openferris::agent::{Agent, AgentResult, PromptContext};
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

/// Fraction of the model context available to history on smaller backends.
const HISTORY_TOKEN_FRACTION: f32 = 0.5;
/// Interactive chat should remain compact enough for fast cold prefill. Older
/// durable facts belong in memories, not an indefinitely growing transcript.
const MAX_HISTORY_TOKEN_BUDGET: usize = 16_000;
/// Rough chars-per-token conversion, matching `agent::estimate_tokens`'s
/// chars/4 heuristic — `Storage::load_thread` budgets in chars, not tokens,
/// since it never tokenizes.
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;
/// Fallback token budget when the backend's context window can't be queried.
const DEFAULT_HISTORY_TOKEN_BUDGET: usize = MAX_HISTORY_TOKEN_BUDGET;
const GOAL_SKILL_NAME: &str = "goal-pursuit";
const GOAL_MAX_TURNS_HARD: usize = 25;

/// Max number of times, within a single goal-pursuit run, that the evaluator
/// (see `evaluate_done`) may reject a turn's `<goal_status>done</goal_status>`
/// claim before the daemon just accepts it. The per-run turn budget
/// (`requested_max_turns`, capped by `GOAL_MAX_TURNS_HARD`) is the ultimate
/// backstop against a runaway loop, so this cap exists only to stop the
/// worker model and the evaluator from disagreeing forever when the
/// evaluator itself is being unreasonable — past the cap we trust the
/// worker's self-report and move on (with a `tracing::warn!`, so it's visible
/// in logs that the evaluator was overridden).
const GOAL_MAX_EVALUATOR_REJECTIONS: usize = 2;

/// How often the daemon polls for due wakeups (see `set_wakeup`/1.3 in the
/// refactor plan). ~a minute matches the tool's "fires within about a
/// minute of `due`" promise to the LLM without polling SQLite too eagerly.
const WAKEUP_TICK_INTERVAL: Duration = Duration::from_secs(60);
/// Source tag for a `DaemonRequest` the wakeup tick enqueues itself, as
/// opposed to one that arrived from a client connection.
const WAKEUP_SOURCE: &str = "wakeup";

/// Resolve owner-facing web and TUI sessions to the owner's message thread.
fn resolve_counterparty(_session_id: &str) -> String {
    counterparty::OWNER.to_string()
}

/// Data needed to log an interaction after the agent finishes.
struct LogData {
    source: String,
    skill: Option<String>,
    user_message: String,
    result: AgentResult,
}

/// Start the daemon. `db_path` is the same SQLite file `storage` was opened
/// from — the wakeup tick task (see [`WAKEUP_TICK_INTERVAL`]) needs its own
/// `Connection` (the worker task above owns `storage` for the request path),
/// so it reopens the file rather than sharing a handle. `Storage::open` sets
/// WAL + a 5s `busy_timeout`, so the extra connection can write concurrently
/// with the worker's without `SQLITE_BUSY`.
pub async fn run(
    config: AppConfig,
    agent: Agent,
    storage: Storage,
    memories: Memories,
    db_path: PathBuf,
) -> Result<()> {
    run_with_wakeup_tick(
        config,
        agent,
        storage,
        memories,
        db_path,
        WAKEUP_TICK_INTERVAL,
    )
    .await
}

/// Same as [`run`], but with the wakeup tick's polling interval as a
/// parameter — production always uses [`WAKEUP_TICK_INTERVAL`] (`run` above
/// hardcodes it), but the integration test below needs a much shorter
/// interval to fire a seeded wakeup without a real-time wait.
async fn run_with_wakeup_tick(
    config: AppConfig,
    agent: Agent,
    storage: Storage,
    memories: Memories,
    db_path: PathBuf,
    wakeup_tick_interval: Duration,
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
    // default (e.g. cron without $XDG_RUNTIME_DIR) can fall back to the real
    // one. Never under cfg(test): the integration tests in this file run real
    // daemons on tempdir sockets, and writing those paths to the REAL data
    // dir once left the pointer aiming at a vanished tempdir. (The pointer is
    // only ever read by client fallback, never to choose a bind address.)
    #[cfg(not(test))]
    {
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
    }
    tracing::info!("OpenFerris daemon listening on {}", socket_path);

    let (tx, mut rx) = mpsc::unbounded_channel::<QueuedRequest>();

    // Wakeup tick: polls for due `set_wakeup` entries and enqueues each as an
    // internal RunSkill request through the same worker channel a client
    // request would use, so it gets identical handling — including the
    // owner-thread run_note persistence RunSkill already does (see the
    // `ResponseKind::Done` match arm below). This is the one-shot deferred-
    // action primitive from refactor plan 1.3: the goal heartbeat (1.2)
    // already covers coarse per-goal resumption, this covers precise-time
    // and non-goal follow-ups ("remind me at 9").
    {
        let tick_tx = tx.clone();
        let wakeup_db_path = db_path.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(wakeup_tick_interval);
            loop {
                interval.tick().await;
                if let Err(e) = fire_due_wakeups(&wakeup_db_path, &tick_tx).await {
                    tracing::warn!("Wakeup tick failed: {}", e);
                }
            }
        });
    }

    // Single worker task — processes requests sequentially, owns storage and memories.
    let worker_agent = agent.clone();
    let user_skills_dir = config::config_dir().join("skills");
    let warm_cache = config.daemon.warm_cache;
    tokio::spawn(async move {
        // Conversation history lives in `storage`'s `messages` table, keyed by
        // counterparty rather than transport session — it survives daemon
        // restarts and is shared across channels for the same counterparty.
        if warm_cache {
            let started = std::time::Instant::now();
            let result = match worker_agent.context_window_tokens().await {
                Ok(context_window) => {
                    let token_budget = (((context_window as f32) * HISTORY_TOKEN_FRACTION)
                        as usize)
                        .min(MAX_HISTORY_TOKEN_BUDGET);
                    match storage
                        .load_thread(counterparty::OWNER, token_budget * CHARS_PER_TOKEN_ESTIMATE)
                    {
                        Ok(history) => {
                            warm_interactive_cache(
                                &worker_agent,
                                history,
                                &memories,
                                &user_skills_dir,
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(()) => tracing::info!(
                    elapsed_ms = started.elapsed().as_millis(),
                    "Interactive KV cache warmed"
                ),
                Err(error) => tracing::warn!(
                    elapsed_ms = started.elapsed().as_millis(),
                    "Interactive KV cache warm-up failed; continuing: {error:#}"
                ),
            }
        }

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

            // Threaded interactive requests already have authoritative chat
            // history. The cross-interface interaction annex is for one-shot
            // runs that lack a thread; including it in interactive requests
            // duplicates turns and invalidates the LLM prefix cache before
            // the long history begins.
            let counterparty: Option<String> = match &queued.request.kind {
                RequestKind::FreeformMessage { .. } => queued
                    .request
                    .session_id
                    .as_deref()
                    .map(resolve_counterparty),
                _ => None,
            };

            let memory_context = match memories.load_for_prompt() {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::warn!("Failed to load memories: {}", e);
                    String::new()
                }
            };

            let interaction_context = if counterparty.is_none() {
                match storage.build_context() {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        tracing::warn!("Failed to load interaction context: {}", e);
                        String::new()
                    }
                }
            } else {
                String::new()
            };

            let persistent_context = format!("{}{}", memory_context, interaction_context);

            // Sync: load user profile (re-read each time so edits take effect)
            let user_profile = config::load_user();

            let history: Vec<ChatMessage> = match &counterparty {
                Some(cp) => {
                    let token_budget = match worker_agent.context_window_tokens().await {
                        Ok(n) => (((n as f32) * HISTORY_TOKEN_FRACTION) as usize)
                            .min(MAX_HISTORY_TOKEN_BUDGET),
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
                        Ok(h) => {
                            tracing::debug!(
                                counterparty = %cp,
                                messages = h.len(),
                                char_budget,
                                "Loaded interactive thread history"
                            );
                            h
                        }
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
            let prompt_ctx = PromptContext {
                user_profile: &user_profile,
                persistent_context: &persistent_context,
            };
            let (response, log_data) = process_request(
                &worker_agent,
                &queued,
                &history,
                &user_skills_dir,
                prompt_ctx,
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

async fn warm_interactive_cache(
    agent: &Agent,
    history: Vec<ChatMessage>,
    memories: &Memories,
    user_skills_dir: &Path,
) -> Result<()> {
    let skill = skills::load_skill("default", user_skills_dir)?;
    let memory_context = memories.load_for_prompt()?;
    let user_profile = config::load_user();
    agent
        .warm_cache(
            &skill,
            &history,
            PromptContext {
                user_profile: &user_profile,
                persistent_context: &memory_context,
            },
        )
        .await
}

/// Poll `db_path` for pending wakeups whose `due_ts` has passed and enqueue
/// each as a `RunSkill` request on `tx`, exactly as if a client had asked to
/// run the `default` skill with the wakeup's note as context. Reused by the
/// production tick loop in [`run_with_wakeup_tick`] and directly by the
/// integration test below.
///
/// Marks each wakeup `fired` *before* enqueuing its run — at-most-once
/// delivery. If the process crashes between the mark and the run actually
/// executing, the wakeup is silently lost rather than risking a double-fire
/// on the next tick after restart. There's no cheap idempotency key to make
/// "fire it again" safe here (the "work" is an arbitrary agent run that may
/// already have sent an email or message), so lost-but-never-duplicated is
/// the safer failure mode for a personal-assistant reminder.
async fn fire_due_wakeups(db_path: &Path, tx: &mpsc::UnboundedSender<QueuedRequest>) -> Result<()> {
    let storage = Storage::open(db_path)?;
    let now = storage::now_local();
    let due = storage.due_wakeups(&now)?;

    for (id, due_ts, note) in due {
        if let Err(e) = storage.mark_wakeup_fired(id) {
            tracing::warn!(
                "Failed to mark wakeup {} fired, skipping it this tick: {}",
                id,
                e
            );
            continue;
        }
        tracing::info!("Wakeup #{} fired (was due {})", id, due_ts);

        let request = DaemonRequest {
            id: uuid::Uuid::new_v4().to_string(),
            kind: RequestKind::RunSkill {
                skill_name: "default".to_string(),
                context: Some(wakeup_context(&due_ts, &note)),
            },
            source: Some(WAKEUP_SOURCE.to_string()),
            session_id: None,
        };
        let (response_tx, response_rx) = oneshot::channel();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel::<AgentNotification>();
        let queued = QueuedRequest {
            request,
            response_tx,
            progress_tx,
        };
        if tx.send(queued).is_err() {
            tracing::error!("Wakeup #{}: worker channel closed, dropping", id);
            break;
        }
        // Don't block the tick loop on the run finishing; just log the
        // outcome when it does. The worker itself records the final
        // response as an owner-thread run_note (see the `RequestKind::
        // RunSkill` arm in the thread-persistence match below) — reusing
        // that existing path rather than inventing a new one for wakeups.
        tokio::spawn(async move {
            match response_rx.await {
                Ok(resp) => tracing::debug!("Wakeup #{} run finished: {:?}", id, resp.kind),
                Err(_) => tracing::warn!("Wakeup #{}: worker dropped the response channel", id),
            }
        });
    }

    Ok(())
}

/// Build the instruction handed to the fresh `default`-skill run a fired
/// wakeup triggers. That run starts with none of the original conversation —
/// only this text plus the skill's normal persistent context/system prompt —
/// so it has to spell out plainly that nobody is waiting on a live chat
/// reply, since the `default` skill's own prompt otherwise assumes a human
/// just sent a message.
fn wakeup_context(due_ts: &str, note: &str) -> String {
    format!(
        "This is an automated wakeup you (or a prior run) scheduled earlier via set_wakeup, \
         originally due {}. Nobody is chatting with you right now — there is no message to \
         reply to. Act on the note below directly: do the work it describes, and use \
         send_email yourself if the owner needs to be told something. The note \
         is the only context you have; nothing else about why it was set is available.\n\n\
         Wakeup note: {}",
        due_ts, note
    )
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

/// Build an error `DaemonResponse` (with no log data) for `request_id`. Used
/// by every `process_request` branch's failure path so the error shape stays
/// identical regardless of which stage (skill load, agent run, goal pursuit)
/// produced it.
fn err_resp(request_id: String, e: anyhow::Error) -> (DaemonResponse, Option<LogData>) {
    (
        DaemonResponse {
            request_id,
            kind: ResponseKind::Error {
                message: format!("{:#}", e),
            },
        },
        None,
    )
}

/// Build a `Done` `DaemonResponse` plus the matching [`LogData`] for a
/// successful agent run. Used by every `process_request` branch's success
/// path.
fn done_resp(
    request_id: String,
    source: String,
    skill: Option<String>,
    user_message: String,
    result: AgentResult,
) -> (DaemonResponse, Option<LogData>) {
    let response = DaemonResponse {
        request_id,
        kind: ResponseKind::Done {
            text: result.response.clone(),
        },
    };
    let log = LogData {
        source,
        skill,
        user_message,
        result,
    };
    (response, Some(log))
}

/// Run the agent and return (response for client, optional log data for storage).
/// `prompt.persistent_context` is a pre-built String so storage isn't held across .await.
async fn process_request(
    agent: &Agent,
    queued: &QueuedRequest,
    history: &[ChatMessage],
    user_skills_dir: &std::path::Path,
    prompt: PromptContext<'_>,
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
        } => {
            let skill = match skills::load_skill(skill_name, user_skills_dir) {
                Ok(skill) => skill,
                Err(e) => return err_resp(request_id, e),
            };
            let msg = match context {
                Some(ctx) => format!("Execute the {} skill now.\n\n{}", skill_name, ctx),
                None => format!("Execute the {} skill now.", skill_name),
            };
            match agent
                .run(&skill, &msg, &[], prompt, Some(progress_tx.clone()))
                .await
            {
                Ok(result) => done_resp(request_id, source, Some(skill_name.clone()), msg, result),
                Err(e) => err_resp(request_id, e),
            }
        }
        RequestKind::FreeformMessage { text } => {
            let skill = match skills::load_skill("default", user_skills_dir) {
                Ok(skill) => skill,
                Err(e) => return err_resp(request_id, e),
            };
            match agent
                .run(&skill, text, history, prompt, Some(progress_tx.clone()))
                .await
            {
                Ok(result) => done_resp(request_id, source, None, text.clone(), result),
                Err(e) => err_resp(request_id, e),
            }
        }
        RequestKind::PursueGoal {
            exit_criteria,
            max_turns,
        } => {
            let skill = match skills::load_skill(GOAL_SKILL_NAME, user_skills_dir) {
                Ok(skill) => skill,
                Err(e) => return err_resp(request_id, e),
            };
            match pursue_goal(
                agent,
                &skill,
                exit_criteria,
                *max_turns,
                prompt,
                progress_tx.clone(),
            )
            .await
            {
                Ok(result) => done_resp(
                    request_id,
                    source,
                    Some(GOAL_SKILL_NAME.to_string()),
                    format!("Pursue goal with exit criteria: {}", exit_criteria),
                    result,
                ),
                Err(e) => err_resp(request_id, e),
            }
        }
        // StoreMemory is handled directly in the worker loop above.
        RequestKind::StoreMemory { .. } => unreachable!(),
    }
}

async fn pursue_goal(
    agent: &Agent,
    skill: &openferris::skills::Skill,
    exit_criteria: &str,
    requested_max_turns: usize,
    prompt_ctx: PromptContext<'_>,
    progress_tx: mpsc::UnboundedSender<AgentNotification>,
) -> Result<AgentResult> {
    if requested_max_turns == 0 || requested_max_turns > GOAL_MAX_TURNS_HARD {
        anyhow::bail!("max_turns must be between 1 and {}", GOAL_MAX_TURNS_HARD);
    }

    let mut history: Vec<ChatMessage> = Vec::new();
    let mut final_memories: Vec<String> = Vec::new();
    let mut last_response = String::new();
    // Set when the evaluator rejects a "done" claim; carries its reason into
    // the next turn's prompt (see the `Verdict::NotMet` arm below).
    let mut pending_rejection: Option<String> = None;
    let mut evaluator_rejections = 0usize;

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
        } else if let Some(reason) = pending_rejection.take() {
            format!(
                "An independent check of your work against the exit criteria found them not yet met: {}\n\nContinue working, or if the evaluator is wrong, state exactly why and finish.\n\nExit criteria:\n{}\n\nThis is inference turn {} of {}.",
                reason, exit_criteria, turn, requested_max_turns
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
            .run(skill, &prompt, &history, prompt_ctx, None)
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
            if evaluator_rejections >= GOAL_MAX_EVALUATOR_REJECTIONS {
                tracing::warn!(
                    "Goal evaluator rejection cap ({}) reached; accepting the model's \
                     done claim without further evaluation",
                    GOAL_MAX_EVALUATOR_REJECTIONS
                );
                return Ok(AgentResult {
                    response: clean_response,
                    memories: final_memories,
                });
            }

            match evaluate_done(agent, exit_criteria, &clean_response).await {
                Verdict::Met => {
                    return Ok(AgentResult {
                        response: clean_response,
                        memories: final_memories,
                    });
                }
                Verdict::NotMet(reason) => {
                    evaluator_rejections += 1;
                    tracing::info!(
                        "Evaluator rejected done claim ({}/{}): {}",
                        evaluator_rejections,
                        GOAL_MAX_EVALUATOR_REJECTIONS,
                        reason
                    );
                    let _ = progress_tx.send(AgentNotification::ToolProgress(format!(
                        "Evaluator: not yet met ({}/{} rejections) — continuing...",
                        evaluator_rejections, GOAL_MAX_EVALUATOR_REJECTIONS
                    )));
                    pending_rejection = Some(reason);
                    // Fall through: the loop continues to the next turn
                    // instead of returning, feeding `pending_rejection` into
                    // the next prompt above.
                }
            }
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

/// Result of `evaluate_done` judging a turn's `<goal_status>done</goal_status>`
/// claim against the goal's exit criteria.
enum Verdict {
    Met,
    /// Carries the evaluator's one-to-two sentence reason, fed back to the
    /// worker as the next turn's prompt.
    NotMet(String),
}

/// System prompt for the goal-pursuit done-claim evaluator (refactor plan
/// 1.4). Cast strictly as an independent verifier rather than a collaborator:
/// the whole point of this extra call is to catch a worker model grading its
/// own homework and declaring victory prematurely.
const EVALUATOR_SYSTEM_PROMPT: &str = "You are a strict, independent verifier for an autonomous \
agent's goal-pursuit system. You will be given a goal's exit criteria and the text of a response \
the agent produced while claiming the goal is now done. Your only job is to judge whether the \
response text actually demonstrates that the exit criteria are met -- not whether the agent \
sounds confident, not whether it tried hard, not whether it's plausible that it's done. Be \
skeptical of vague claims, unfinished steps, and hedged language dressed up as completion. \
Respond with exactly one verdict tag -- <verdict>met</verdict> or <verdict>not_met</verdict> -- \
followed by a one-to-two sentence reason. Nothing else.";

/// Ask a fresh, tool-free LLM call to independently judge whether a
/// goal-pursuit turn's `<goal_status>done</goal_status>` claim is actually
/// justified by that turn's own response, given the goal's exit criteria.
/// This is a bare completion via `Agent::raw_completion` (system + user
/// message, no tools, no skill prompt) — deliberately not a full `Agent::run`,
/// since the evaluator's job is to judge, not to act.
///
/// Limitation: the daemon does not read the goal file. Goal files live in the
/// agent's own workspace (`~/.local/share/openferris/workspace/goals/`), and
/// the daemon deliberately never reads agent-owned files (see refactor plan
/// 1.2/1.3 — the daemon only ever writes instructions and reads back model
/// output). So the evaluator judges only the exit criteria plus this turn's
/// response text; it has no visibility into the goal file's plan/progress
/// log/prior turns beyond what the response text itself restates. A
/// sufficiently persuasive but inaccurate response could still fool it. A
/// richer evaluation would require the daemon to read agent workspace files,
/// which is out of scope here.
///
/// Fails open on any evaluator trouble (backend error, malformed/missing
/// verdict): an evaluator outage must not be able to block goal completion
/// forever, so ambiguity resolves to `Verdict::Met` with a `tracing::warn!`.
async fn evaluate_done(agent: &Agent, exit_criteria: &str, response_text: &str) -> Verdict {
    let messages = vec![
        ChatMessage {
            role: Role::System,
            content: EVALUATOR_SYSTEM_PROMPT.to_string(),
        },
        ChatMessage {
            role: Role::User,
            content: format!(
                "Exit criteria:\n{}\n\nThe agent's response for this turn, which claims the goal \
                 is done:\n{}\n\nIs the goal actually met, per the exit criteria? Answer with \
                 exactly one verdict tag, then your one-to-two sentence reason.",
                exit_criteria, response_text
            ),
        },
    ];

    match agent.raw_completion(&messages).await {
        Ok(text) => match extract_verdict(&text).as_deref() {
            Some("met") => Verdict::Met,
            Some("not_met") => Verdict::NotMet(strip_verdict(&text)),
            _ => {
                tracing::warn!(
                    "Goal evaluator returned a missing/malformed verdict, treating as met \
                     (fail open): {}",
                    text
                );
                Verdict::Met
            }
        },
        Err(e) => {
            tracing::warn!(
                "Goal evaluator call failed, treating as met (fail open): {}",
                e
            );
            Verdict::Met
        }
    }
}

/// Extract the lowercased, trimmed content of a `<verdict>...</verdict>` tag,
/// mirroring `extract_goal_status`'s style. Returns `None` if the tag is
/// missing or unclosed — callers treat that as fail-open (see
/// `evaluate_done`).
fn extract_verdict(text: &str) -> Option<String> {
    let start = text.find("<verdict>")? + "<verdict>".len();
    let end = text[start..].find("</verdict>")? + start;
    Some(text[start..end].trim().to_ascii_lowercase())
}

/// Strip the `<verdict>...</verdict>` tag out of the evaluator's response,
/// leaving the reason text, mirroring `strip_goal_status`'s style.
fn strip_verdict(text: &str) -> String {
    let Some(start) = text.find("<verdict>") else {
        return text.trim().to_string();
    };
    let Some(rel_end) = text[start..].find("</verdict>") else {
        return text.trim().to_string();
    };
    let end = start + rel_end + "</verdict>".len();
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

    fn minimal_config(socket: String) -> AppConfig {
        AppConfig {
            agent: openferris::config::AgentConfig {
                name: "Ferris".to_string(),
            },
            user: UserConfig {
                timezone: "UTC".to_string(),
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
            daemon: DaemonConfig {
                socket,
                warm_cache: false,
            },
            files: FilesConfig::default(),
            fetch: FetchConfig::default(),
            gws: GwsConfig::default(),
            search: None::<SearchConfig>,
            firecrawl: None::<FirecrawlConfig>,
            camoufox: None::<CamoufoxConfig>,
            gmail: None,
        }
    }

    #[test]
    fn test_resolve_counterparty_tui_is_owner() {
        assert_eq!(resolve_counterparty("tui:some-uuid"), counterparty::OWNER);
    }

    #[test]
    fn test_extract_verdict_met_well_formed() {
        let text = "<verdict>met</verdict> The response cites a concrete confirmation.";
        assert_eq!(extract_verdict(text).as_deref(), Some("met"));
    }

    #[test]
    fn test_extract_verdict_not_met_well_formed() {
        let text = "<verdict>not_met</verdict> No evidence the email was actually sent.";
        assert_eq!(extract_verdict(text).as_deref(), Some("not_met"));
    }

    #[test]
    fn test_extract_verdict_missing() {
        let text = "I think this looks good overall.";
        assert_eq!(extract_verdict(text), None);
    }

    #[test]
    fn test_extract_verdict_malformed_unclosed_tag() {
        // Opening tag with no matching close — must not panic or misparse.
        let text = "<verdict>not_met the criteria are not satisfied";
        assert_eq!(extract_verdict(text), None);
    }

    #[test]
    fn test_extract_verdict_embedded_in_prose() {
        let text = "Looking at the exit criteria closely, <verdict>not_met</verdict> because \
                     step 3 was never actually completed.";
        assert_eq!(extract_verdict(text).as_deref(), Some("not_met"));
        let reason = strip_verdict(text);
        assert!(!reason.contains("<verdict>"));
        assert!(reason.contains("step 3 was never actually completed"));
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
    /// this turn started (simulating what a delivery tool appends) is visible to the
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
        // Simulate a legacy outbound delivery: an outbound chat turn in the
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

        tokio::spawn(run(config, agent, storage, memories, db_path));

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

    /// End-to-end for refactor plan 1.3: a wakeup seeded in the past should
    /// get picked up by the tick loop, fire a `default`-skill run carrying
    /// the wakeup's note, and flip to `fired` so it never fires again. Uses
    /// `run_with_wakeup_tick` directly (rather than the public `run`, which
    /// hardcodes the production 60s interval) with a short interval so the
    /// test doesn't need a real-time wait.
    #[tokio::test]
    async fn test_due_wakeup_fires_through_daemon_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("openferris.db");
        let socket_path = tmp.path().join("daemon.sock");

        let storage = Storage::open(&db_path).unwrap();
        storage
            .add_wakeup(
                "2020-01-01 00:00:00",
                "Check the openferris.org DNS propagated and tell the owner via email.",
            )
            .unwrap();

        let mock = Arc::new(MockLlm::new(vec!["DNS looks propagated.".to_string()]));
        let agent = Agent::new(
            Box::new(ArcMockLlm(mock.clone())),
            ToolRegistry::new(),
            String::new(),
        );
        let memories = Memories::new(tmp.path().join("MEMORIES.md"));
        let config = minimal_config(socket_path.to_string_lossy().to_string());

        tokio::spawn(run_with_wakeup_tick(
            config,
            agent,
            storage,
            memories,
            db_path.clone(),
            std::time::Duration::from_millis(50),
        ));

        // Poll for the mock to have been called — the tick fires on its own,
        // with no client request needed.
        let mut fired = false;
        for _ in 0..100 {
            if mock.call_count() >= 1 {
                fired = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(fired, "wakeup never triggered a chat_completion call");

        let sent_messages = mock.messages_at(0).unwrap();
        let found = sent_messages.iter().any(|m| {
            m.content
                .contains("Check the openferris.org DNS propagated")
                && m.content.contains("set_wakeup")
        });
        assert!(
            found,
            "expected the wakeup note in the fired run's context, got: {:#?}",
            sent_messages
        );

        // The row must have flipped to fired so it's a one-shot, not
        // re-fired on subsequent ticks.
        let reader = Storage::open(&db_path).unwrap();
        assert!(reader.pending_wakeups().unwrap().is_empty());
        assert!(
            reader
                .due_wakeups(&storage::now_local())
                .unwrap()
                .is_empty()
        );
    }

    /// A minimal skill for `pursue_goal` tests below — no tools, empty
    /// prompt: only the evaluator loop's own behavior is under test here, not
    /// skill content or tool execution.
    fn test_goal_skill() -> openferris::skills::Skill {
        openferris::skills::Skill {
            name: "goal-pursuit".to_string(),
            description: "test".to_string(),
            tools: vec![],
            prompt: "Pursue the goal.".to_string(),
        }
    }

    /// Refactor plan 1.4: the evaluator must reject a premature "done" claim
    /// once, feed its reason back as the next turn's prompt, then accept the
    /// second "done" once the evaluator agrees it's actually met. Exercises
    /// the full call sequence: worker turn 1 -> evaluator (not_met) ->
    /// worker turn 2 -> evaluator (met) -> run ends. Every LLM call
    /// (worker or evaluator) consumes one scripted MockLlm response in
    /// order, so the script below has exactly 4 entries for 2 work turns.
    #[tokio::test]
    async fn test_evaluator_rejects_then_accepts_done() {
        let mock = Arc::new(MockLlm::new(vec![
            // Turn 1: worker claims done prematurely.
            "I looked into it.\n<goal_status>done</goal_status>".to_string(),
            // Evaluator: rejects it.
            "<verdict>not_met</verdict> The response doesn't demonstrate the email was \
             actually sent, which the exit criteria requires."
                .to_string(),
            // Turn 2: worker addresses the evaluator's reason and claims done again.
            "I actually sent the email this time.\n<goal_status>done</goal_status>".to_string(),
            // Evaluator: accepts it.
            "<verdict>met</verdict> The response confirms the email was sent.".to_string(),
        ]));
        let agent = Agent::new(
            Box::new(ArcMockLlm(mock.clone())),
            ToolRegistry::new(),
            String::new(),
        );
        let skill = test_goal_skill();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel::<AgentNotification>();

        let result = pursue_goal(
            &agent,
            &skill,
            "Send the owner a confirmation email.",
            5,
            PromptContext {
                user_profile: "",
                persistent_context: "",
            },
            progress_tx,
        )
        .await
        .unwrap();

        assert_eq!(
            mock.call_count(),
            4,
            "expected 2 work turns + 2 evaluator calls"
        );
        assert!(!result.response.contains("<goal_status>"));
        assert!(
            result
                .response
                .contains("I actually sent the email this time")
        );

        // Turn 2's prompt (the 3rd chat_completion call, 0-indexed 2) must
        // carry the evaluator's rejection reason forward.
        let turn2_messages = mock.messages_at(2).unwrap();
        let turn2_prompt = turn2_messages
            .last()
            .expect("turn 2 call must have at least one message");
        assert!(
            turn2_prompt
                .content
                .contains("doesn't demonstrate the email was actually sent"),
            "expected the evaluator's reason in turn 2's prompt, got: {:#?}",
            turn2_prompt
        );
        assert!(
            turn2_prompt.content.contains("if the evaluator is wrong"),
            "expected the standard reject-and-continue framing, got: {:#?}",
            turn2_prompt
        );
    }

    /// Refactor plan 1.4: an evaluator response with no parseable
    /// `<verdict>` tag (garbage/off-format output, or a backend error) must
    /// fail open — the model's "done" claim is accepted immediately rather
    /// than blocking goal completion on an unreliable evaluator.
    #[tokio::test]
    async fn test_evaluator_garbage_response_accepts_done_immediately() {
        let mock = Arc::new(MockLlm::new(vec![
            // Turn 1: worker claims done.
            "All done here.\n<goal_status>done</goal_status>".to_string(),
            // Evaluator: garbage, no <verdict> tag at all.
            "uh, I'm not sure how to answer that".to_string(),
        ]));
        let agent = Agent::new(
            Box::new(ArcMockLlm(mock.clone())),
            ToolRegistry::new(),
            String::new(),
        );
        let skill = test_goal_skill();
        let (progress_tx, _progress_rx) = mpsc::unbounded_channel::<AgentNotification>();

        let result = pursue_goal(
            &agent,
            &skill,
            "Do the thing.",
            5,
            PromptContext {
                user_profile: "",
                persistent_context: "",
            },
            progress_tx,
        )
        .await
        .unwrap();

        assert_eq!(
            mock.call_count(),
            2,
            "expected exactly 1 work turn + 1 evaluator call; a garbage verdict must not \
             trigger another work turn"
        );
        assert!(!result.response.contains("<goal_status>"));
        assert!(result.response.contains("All done here"));
    }
}
