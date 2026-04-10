mod config;
mod daemon;
mod db;
mod tui;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "army-of-daemons", about = "Config-driven daemon army with LLM triage")]
struct Cli {
    /// Path to config directory (default: ~/.daemon-army)
    #[arg(long, global = true)]
    config_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the daemon runner (foreground)
    Run,
    /// Open the TUI dashboard
    Tui,
    /// One-line status for zellij status bar
    Status,
    /// Run a single daemon once
    Check {
        /// Daemon or instance name (e.g. "pod-errors" or "pod-errors::my-server")
        name: String,
    },
    /// Export reports as JSON
    Export {
        /// Filter by daemon name
        #[arg(long)]
        daemon: Option<String>,
        /// Only reports newer than this many days
        #[arg(long, default_value = "30")]
        days: u32,
    },
    /// Validate all daemon configs
    Validate,
}

fn resolve_config_dir(cli_override: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = cli_override {
        return Ok(dir);
    }
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    Ok(home.join(".daemon-army"))
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_dir = resolve_config_dir(cli.config_dir)?;

    match cli.command {
        Commands::Run => daemon::runner::run(&config_dir).await,
        Commands::Tui => tui::run(&config_dir).await,
        Commands::Status => {
            let global = config::load_global(&config_dir)?;
            let db = db::Database::open(&global.db_path)?;
            let line = db.status_line()?;
            println!("{line}");
            Ok(())
        }
        Commands::Check { name } => daemon::runner::check_one(&config_dir, &name).await,
        Commands::Export { daemon, days } => {
            let global = config::load_global(&config_dir)?;
            let database = db::Database::open(&global.db_path)?;
            let reports = database.export_reports(daemon.as_deref(), days)?;
            println!("{}", serde_json::to_string_pretty(&reports)?);
            Ok(())
        }
        Commands::Validate => {
            config::validate_all(&config_dir)?;
            println!("all configs valid");
            Ok(())
        }
    }
}
