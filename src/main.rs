mod client;
mod daemon;
mod gmail;
mod memories;
mod tui;
mod web;

use openferris::{agent, config, llm, schedule, skills, storage, tools};

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Subcommand)]
enum ScheduleCommand {
    /// Add a scheduled skill invocation
    Add {
        /// Skill name to schedule
        skill_name: String,
        /// Cron expression (e.g., "0 7 * * *" for 7am daily)
        cron_expr: String,
    },
    /// Remove a scheduled skill invocation
    Remove {
        /// Skill name to unschedule
        skill_name: String,
    },
    /// List all scheduled skill invocations
    List,
}

#[derive(Parser)]
#[command(name = "openferris", about = "AI personal assistant", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the central daemon
    Daemon,
    /// Interactive terminal session with the daemon
    Tui,
    /// Run a named skill (e.g., openferris run daily-briefing)
    Run {
        /// Skill name to execute
        skill_name: String,
    },
    /// Pursue a goal over multiple bounded inference turns
    Goal {
        /// Maximum inference turns before stopping
        #[arg(long, default_value_t = 5)]
        max_turns: usize,
        /// Exit criteria that define when the goal is complete
        #[arg(required = true, trailing_var_arg = true)]
        exit_criteria: Vec<String>,
    },
    /// Start the private web chat interface
    Web {
        /// Address to listen on (use loopback with `tailscale serve`)
        #[arg(long, default_value = "127.0.0.1:3030")]
        listen: String,
    },
    /// Start the Gmail listener
    Gmail,
    /// Manage scheduled skill invocations via cron
    #[command(subcommand)]
    Schedule(ScheduleCommand),
    /// Run a prompt through the real agent and print the full trace
    TestAgent {
        /// Prompt to send to the agent
        prompt: String,
        /// Skill to use
        #[arg(long, default_value = "default")]
        skill: String,
    },
    /// Clear interaction history and/or memories
    Forget {
        /// Time window to clear: "1h", "24h", "7d", "30d", or "all"
        #[arg(default_value = "all")]
        window: String,
        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Default to debug-level logging for test-agent, info for everything else.
    let default_filter = match &cli.command {
        Commands::TestAgent { .. } => "openferris=debug",
        _ => "openferris=info",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.parse().unwrap()),
        )
        .init();

    let config = config::load_config()?;

    match cli.command {
        Commands::Daemon => {
            let soul = config::load_soul(&config.agent.name)?;
            let (agent, db_path, _skills_dir) = build_agent(&config, soul)?;
            let storage = storage::Storage::open(&db_path)?;
            tracing::info!("Storage opened at {}", db_path.display());

            let mem_path = memories::Memories::default_path();
            let mems = memories::Memories::new(mem_path.clone());

            // Seed the workspace skills README so the agent knows the format
            let workspace_skills_dir = config::data_dir().join("workspace").join("skills");
            let skills_readme = workspace_skills_dir.join("README.md");
            if !skills_readme.exists() {
                std::fs::create_dir_all(&workspace_skills_dir)?;
                std::fs::write(&skills_readme, include_str!("../skills/README.md"))?;
                tracing::info!("Seeded skills README at {}", skills_readme.display());
            }
            tracing::info!("Memories at {}", mem_path.display());

            daemon::run(config, agent, storage, mems, db_path).await?;
        }
        Commands::Tui => {
            tui::run(&config.daemon.socket).await?;
        }
        Commands::Web { listen } => {
            web::run(config.daemon.socket.clone(), &listen, &config.agent.name).await?;
        }
        Commands::Gmail => {
            let gmail_config = config.gmail.clone().ok_or_else(|| {
                anyhow::anyhow!("No [gmail] section in config.toml. Add allowed_senders to enable.")
            })?;
            gmail::run(
                config.daemon.socket.clone(),
                gmail_config,
                config.user.emails.clone(),
            )
            .await?;
        }
        Commands::TestAgent { prompt, skill } => {
            let soul = config::load_soul(&config.agent.name)?;
            let user_profile = config::load_user();
            let (agent, _db_path, skills_dir) = build_agent(&config, soul)?;

            let skill = skills::load_skill(&skill, &skills_dir)?;

            let (progress_tx, mut progress_rx) =
                tokio::sync::mpsc::unbounded_channel::<openferris::protocol::AgentNotification>();
            let progress_handle = tokio::spawn(async move {
                use openferris::protocol::AgentNotification;
                while let Some(notif) = progress_rx.recv().await {
                    match notif {
                        AgentNotification::ToolProgress(label) => {
                            eprintln!("[progress] {}", label);
                        }
                        AgentNotification::AssistantChunk(text) => {
                            // Stream chunks to stderr so they appear live without
                            // disrupting the final stdout response.
                            eprint!("{}", text);
                            use std::io::Write;
                            let _ = std::io::stderr().flush();
                        }
                    }
                }
            });

            let result = agent
                .run(
                    &skill,
                    &prompt,
                    &[],
                    agent::PromptContext {
                        user_profile: &user_profile,
                        persistent_context: "",
                    },
                    Some(progress_tx),
                )
                .await?;
            // Terminate the streamed line so the final response prints cleanly.
            eprintln!();

            progress_handle.abort();

            println!("=== RESPONSE ===");
            println!("{}", result.response);
            if !result.memories.is_empty() {
                println!("\n=== MEMORIES ===");
                for mem in &result.memories {
                    println!("  - {}", mem);
                }
            }
        }
        Commands::Run { skill_name } => {
            let primary = config.daemon.socket.clone();
            let name = skill_name.clone();
            let outcome = send_via_daemon(&primary, move |socket| {
                let name = name.clone();
                async move { client::send_skill(&socket, &name).await }
            })
            .await;

            match outcome {
                Ok(response) => println!("{}", response),
                Err(e) => log_cli_failure_and_exit(
                    &format!("run {}", skill_name),
                    Some(&skill_name),
                    &format!("run {}", skill_name),
                    e,
                ),
            }
        }
        Commands::Goal {
            max_turns,
            exit_criteria,
        } => {
            let exit_criteria = exit_criteria.join(" ");
            let primary = config.daemon.socket.clone();
            let criteria = exit_criteria.clone();
            let outcome = send_via_daemon(&primary, move |socket| {
                let criteria = criteria.clone();
                async move { client::send_goal(&socket, &criteria, max_turns).await }
            })
            .await;

            match outcome {
                Ok(response) => println!("{}", response),
                Err(e) => log_cli_failure_and_exit(
                    "goal",
                    Some("goal-pursuit"),
                    &format!("goal {}", exit_criteria),
                    e,
                ),
            }
        }
        Commands::Schedule(cmd) => {
            schedule_command(cmd).await?;
        }
        Commands::Forget { window, yes } => {
            forget_command(&window, yes)?;
        }
    }

    Ok(())
}

fn forget_command(window: &str, skip_confirm: bool) -> Result<()> {
    let db_path = config::db_path();
    let mems = memories::Memories::new(memories::Memories::default_path());

    let since = parse_time_window(window)?;

    // Count interactions from SQLite
    let interactions = if db_path.exists() {
        let store = storage::Storage::open(&db_path)?;
        store.count_interactions(since.as_deref())?
    } else {
        0
    };

    // Count memories from markdown file
    let memory_count = match &since {
        Some(ts) => mems.count_since(ts)?,
        None => mems.count()?,
    };

    if interactions == 0 && memory_count == 0 {
        println!("Nothing to forget in that time window.");
        return Ok(());
    }

    let window_label = if since.is_some() {
        format!("the last {}", window)
    } else {
        "all time".to_string()
    };

    println!(
        "This will delete from {}:\n  {} interactions\n  {} memories",
        window_label, interactions, memory_count
    );

    if !skip_confirm {
        eprint!("\nProceed? [y/N] ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Delete interactions from SQLite
    let del_i = if db_path.exists() && interactions > 0 {
        let store = storage::Storage::open(&db_path)?;
        store.delete_interactions(since.as_deref())?
    } else {
        0
    };

    // Delete memories from markdown file
    let del_m = match &since {
        Some(ts) => mems.delete_since(ts)?,
        None => mems.delete_all()?,
    };

    println!("Deleted {} interactions and {} memories.", del_i, del_m);

    Ok(())
}

/// Parse a human-friendly time window into a SQLite datetime string.
/// Returns None for "all" (meaning delete everything).
fn parse_time_window(window: &str) -> Result<Option<String>> {
    if window == "all" {
        return Ok(None);
    }

    let (num_str, unit) = if let Some(s) = window.strip_suffix('h') {
        (s, "hours")
    } else if let Some(s) = window.strip_suffix('d') {
        (s, "days")
    } else if let Some(s) = window.strip_suffix('m') {
        (s, "minutes")
    } else {
        anyhow::bail!(
            "Invalid time window '{}'. Use format like: 1h, 24h, 7d, 30d, or all",
            window
        );
    };

    let num: i64 = num_str.parse().map_err(|_| {
        anyhow::anyhow!(
            "Invalid time window '{}'. Use format like: 1h, 24h, 7d, 30d, or all",
            window
        )
    })?;

    // Compute the cutoff time using chrono (local time to match our storage)
    let now = chrono::Local::now();
    let cutoff = match unit {
        "minutes" => now - chrono::Duration::minutes(num),
        "hours" => now - chrono::Duration::hours(num),
        "days" => now - chrono::Duration::days(num),
        _ => unreachable!(),
    };

    Ok(Some(cutoff.format("%Y-%m-%d %H:%M:%S").to_string()))
}

/// Send a request to the daemon at `primary`, falling back to the
/// daemon-published pointer socket (written by the daemon on startup, for
/// clients like cron whose env-derived default path may differ) if the
/// primary is unreachable. `request` performs the actual RPC against a given
/// socket path; it's called once against `primary`, and again against the
/// fallback socket only if that differs from `primary` and the first attempt
/// failed. A failure of both attempts names both sockets and both underlying
/// errors so the operator can tell which is actually down.
async fn send_via_daemon<F, Fut>(primary: &str, request: F) -> Result<String>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    match request(primary.to_string()).await {
        Ok(r) => Ok(r),
        Err(primary_err) => match client::read_socket_pointer() {
            Some(fallback) if fallback != primary => match request(fallback.clone()).await {
                Ok(r) => {
                    tracing::warn!(
                        "Connected via daemon-published socket {} (primary {} unreachable)",
                        fallback,
                        primary
                    );
                    Ok(r)
                }
                Err(fallback_err) => Err(anyhow::anyhow!(
                    "daemon unreachable:\n  primary {}: {:#}\n  fallback {}: {:#}",
                    primary,
                    primary_err,
                    fallback,
                    fallback_err
                )),
            },
            _ => Err(primary_err.context(format!("daemon unreachable at socket {}", primary))),
        },
    }
}

/// Print a CLI failure, best-effort log it to SQLite, and exit(1). Shared by
/// the `Run`/`Goal` failure paths, which differ only in the eprintln label,
/// the logged skill name, and the logged description.
fn log_cli_failure_and_exit(
    label: &str,
    skill_name: Option<&str>,
    description: &str,
    err: anyhow::Error,
) -> ! {
    let msg = format!("{:#}", err);
    eprintln!("openferris {}: {}", label, msg);
    if let Ok(store) = storage::Storage::open(&config::db_path())
        && let Err(log_err) = store.log_interaction("cli", skill_name, description, &msg)
    {
        eprintln!("also failed to log interaction: {}", log_err);
    }
    std::process::exit(1);
}

async fn schedule_command(cmd: ScheduleCommand) -> Result<()> {
    let msg = match cmd {
        ScheduleCommand::Add {
            skill_name,
            cron_expr,
        } => schedule::add_async(&skill_name, &cron_expr).await?,
        ScheduleCommand::Remove { skill_name } => schedule::remove_async(&skill_name).await?,
        ScheduleCommand::List => schedule::list_async().await?,
    };
    println!("{}", msg);
    Ok(())
}

/// Build the top-level `Agent` shared by `Daemon` and `TestAgent`: the LLM
/// backend, a tool registry with the DB-backed tools registered, and — when
/// `config.llm.parallel_slots > 1` — the `run_skill` subagent tool. Returns
/// the agent along with the DB path and skills directory it derived, since
/// each caller needs a different subset afterward (`Daemon` needs the DB
/// path to open storage; `TestAgent` needs the skills directory to load the
/// requested skill).
fn build_agent(
    config: &config::AppConfig,
    soul: String,
) -> anyhow::Result<(agent::Agent, std::path::PathBuf, std::path::PathBuf)> {
    let llm_backend = create_llm_backend(config)?;
    let db_path = config::db_path();
    let skills_dir = config::config_dir().join("skills");

    let mut tool_registry = tools::ToolRegistry::new();
    tool_registry.register_defaults(config);
    tool_registry.register_db_tools(db_path.clone(), config);

    if config.llm.parallel_slots > 1 {
        tool_registry.register(Box::new(tools::run_skill::RunSkillTool::new(
            config.llm.clone(),
            config.clone(),
            soul.clone(),
            config::load_user(),
            skills_dir.clone(),
            db_path.clone(),
        )));
    }

    let agent = agent::Agent::new(llm_backend, tool_registry, soul);
    Ok((agent, db_path, skills_dir))
}

/// Build the parent agent's LLM backend (always slot 0 — subagents spawned by
/// `RunSkillTool` construct their own backend on slot 1 directly, without
/// going through this function). `openai_compat`/`openai-compatible`/
/// `llamacpp` are all handled by the same backend; anything else falls back
/// to it too, with a warning, since it's currently the only backend that
/// exists.
fn create_llm_backend(config: &config::AppConfig) -> anyhow::Result<Box<dyn llm::LlmBackend>> {
    match config.llm.backend.as_str() {
        "openai_compat" | "openai-compatible" | "llamacpp" => {}
        other => tracing::warn!(
            "Unknown LLM backend '{}', defaulting to openai_compat",
            other
        ),
    }
    let model_adapter = llm::model_adapter::create_model_adapter(&config.llm.model_adapter)?;
    tracing::info!(model_adapter = model_adapter.name(), "Using model adapter");
    Ok(Box::new(llm::openai_compat::OpenAiCompatBackend::new(
        config.llm.endpoint.clone(),
        config.llm.model.clone(),
        config.llm.temperature,
        config.llm.top_k,
        config.llm.enable_thinking,
        0,
        model_adapter,
    )?))
}
