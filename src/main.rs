mod agent;
mod client;
mod config;
mod daemon;
mod llm;
mod protocol;
mod skills;
mod tools;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};

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

            daemon::run(config, agent).await?;
        }
        Commands::Tui => {
            tui::run(&config.daemon.listen).await?;
        }
        Commands::Run { skill_name } => {
            let result = client::send_skill(&config.daemon.listen, &skill_name).await?;
            println!("{}", result);
        }
    }

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
