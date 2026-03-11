mod agent;
mod client;
mod config;
mod daemon;
mod llm;
mod memories;
mod protocol;
mod schedule;
mod skills;
mod storage;
mod telegram;
mod tools;
mod tui;

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
    /// Start the Telegram bot listener
    Telegram,
    /// Manage scheduled skill invocations via cron
    #[command(subcommand)]
    Schedule(ScheduleCommand),
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
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    let config = config::load_config()?;

    match cli.command {
        Commands::Daemon => {
            let soul = config::load_soul()?;

            let llm_backend = create_llm_backend(&config);

            let mut tool_registry = tools::ToolRegistry::new();
            tool_registry.register_defaults(&config);

            let agent = agent::Agent::new(llm_backend, tool_registry, soul);

            let db_path = config::data_dir().join("openferris.db");
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

            daemon::run(config, agent, storage, mems).await?;
        }
        Commands::Tui => {
            tui::run(&config.daemon.listen).await?;
        }
        Commands::Telegram => {
            let tg_config = config
                .telegram
                .clone()
                .ok_or_else(|| anyhow::anyhow!("No [telegram] section in config.toml. Add bot_token to enable."))?;
            telegram::run(config.daemon.listen.clone(), tg_config).await?;
        }
        Commands::Run { skill_name } => {
            let result = client::send_skill(&config.daemon.listen, &skill_name).await?;
            println!("{}", result);
        }
        Commands::Schedule(cmd) => {
            schedule_command(cmd)?;
        }
        Commands::Forget { window, yes } => {
            forget_command(&window, yes)?;
        }
    }

    Ok(())
}

fn forget_command(window: &str, skip_confirm: bool) -> Result<()> {
    let db_path = config::data_dir().join("openferris.db");
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

fn schedule_command(cmd: ScheduleCommand) -> Result<()> {
    let msg = match cmd {
        ScheduleCommand::Add {
            skill_name,
            cron_expr,
        } => schedule::add(&skill_name, &cron_expr)?,
        ScheduleCommand::Remove { skill_name } => schedule::remove(&skill_name)?,
        ScheduleCommand::List => schedule::list()?,
    };
    println!("{}", msg);
    Ok(())
}

fn create_llm_backend(config: &config::AppConfig) -> Box<dyn llm::LlmBackend> {
    match config.llm.backend.as_str() {
        "llamacpp" => Box::new(llm::llamacpp::LlamaCppBackend::new(
            config.llm.endpoint.clone(),
            config.llm.model.clone(),
        )),
        other => {
            tracing::warn!("Unknown LLM backend '{}', defaulting to llamacpp", other);
            Box::new(llm::llamacpp::LlamaCppBackend::new(
                config.llm.endpoint.clone(),
                config.llm.model.clone(),
            ))
        }
    }
}
