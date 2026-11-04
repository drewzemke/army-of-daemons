mod actions;
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
    command: Option<Commands>,
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
    /// Fetch a single report by id
    Report {
        /// Report id
        id: i64,
        /// Output as JSON
        #[arg(long)]
        json: bool,
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
    /// Dry-run a daemon: run source + filter, print items as JSON
    DryRun {
        /// Daemon or instance name
        name: String,
        /// Also print the rendered prompt
        #[arg(long)]
        show_prompt: bool,
    },
    /// Re-analyze a report's source data with the current prompt
    Replay {
        /// Report ID to replay
        id: i64,
        /// Override the model for this replay
        #[arg(long)]
        model: Option<String>,
    },
    /// Resume a claude session from a report
    Resume {
        /// Report ID
        id: i64,
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

    match cli.command.unwrap_or(Commands::Tui) {
        Commands::Run => daemon::runner::run(&config_dir).await,
        Commands::Tui => tui::run(&config_dir).await,
        Commands::Status => {
            let global = config::load_global(&config_dir)?;
            let db = db::Database::open(&global.db_path)?;
            let line = db.status_line(global.display_after_days as i64 * 86400, &std::collections::HashMap::new())?;
            println!("{line}");
            Ok(())
        }
        Commands::Check { name } => daemon::runner::check_one(&config_dir, &name).await,
        Commands::Report { id, json } => {
            let global = config::load_global(&config_dir)?;
            let database = db::Database::open(&global.db_path)?;
            match database.get_report(id)? {
                Some(report) => {
                    if json {
                        println!("{}", serde_json::to_string_pretty(&report)?);
                    } else {
                        println!("{}", report.format_text());
                    }
                }
                None => {
                    eprintln!("report {id} not found");
                    std::process::exit(1);
                }
            }
            Ok(())
        }
        Commands::Export { daemon, days } => {
            let global = config::load_global(&config_dir)?;
            let database = db::Database::open(&global.db_path)?;
            let reports = database.export_reports(daemon.as_deref(), days)?;
            println!("{}", serde_json::to_string_pretty(&reports)?);
            Ok(())
        }
        Commands::DryRun { name, show_prompt } => {
            let daemons = config::load_daemons(&config_dir)?;
            let instances = daemon::runner::expand_all(&daemons);
            let matching: Vec<_> = instances
                .iter()
                .filter(|i| i.instance_id == name || i.daemon_name == name)
                .collect();

            if matching.is_empty() {
                anyhow::bail!("no daemon or instance matching '{name}'");
            }

            for instance in matching {
                eprintln!("dry-run {}...", instance.instance_id);
                let output = daemon::pipeline::dry_run_instance(instance).await?;
                eprintln!(
                    "{}: {} raw items, {} after filter",
                    output.instance_id, output.raw_count, output.filtered_items.len()
                );
                println!("{}", serde_json::to_string_pretty(&output.filtered_items)?);
                if show_prompt {
                    eprintln!("\n--- rendered prompt ---");
                    eprintln!("{}", output.rendered_prompt);
                }
            }
            Ok(())
        }
        Commands::Replay { id, model } => {
            let global = config::load_global(&config_dir)?;
            let database = db::Database::open(&global.db_path)?;
            let report = database.get_report(id)?
                .ok_or_else(|| anyhow::anyhow!("no report with id {id}"))?;

            let daemons = config::load_daemons(&config_dir)?;
            let instances = daemon::runner::expand_all(&daemons);
            let instance = instances.into_iter()
                .find(|i| i.instance_id == report.instance_id)
                .ok_or_else(|| anyhow::anyhow!(
                    "daemon config for '{}' not found — was it deleted?",
                    report.instance_id
                ))?;

            let new_id = daemon::pipeline::replay_report(
                &report, &instance, &global, &database, model.as_deref(),
            ).await?;

            eprintln!("created report {new_id}");
            let new_report = database.get_report(new_id)?
                .ok_or_else(|| anyhow::anyhow!("failed to read new report"))?;
            println!("{}", new_report.format_text());
            Ok(())
        }
        Commands::Resume { id } => {
            let global = config::load_global(&config_dir)?;
            let database = db::Database::open(&global.db_path)?;
            let report = database.get_report(id)?
                .ok_or_else(|| anyhow::anyhow!("no report with id {id}"))?;
            let session_id = report.session_id
                .ok_or_else(|| anyhow::anyhow!("report {id} has no session id (error reports don't have sessions)"))?;

            let mut cmd = std::process::Command::new("claude");
            cmd.arg("--resume").arg(&session_id);
            if let Some(ref dir) = report.working_dir {
                cmd.current_dir(config::resolve_path(dir));
            }

            use std::os::unix::process::CommandExt;
            let err = cmd.exec();
            Err(err.into())
        }
        Commands::Validate => {
            config::validate_all(&config_dir)?;
            println!("all configs valid");
            Ok(())
        }
    }
}
