use anyhow::{Result, bail};
use notify::{EventKind, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing_appender::non_blocking::WorkerGuard;

use crate::config::{self, DaemonInstance, GlobalConfig};
use crate::db::Database;

struct RunnerState {
    tasks: HashMap<String, tokio::task::JoinHandle<()>>,
    running: HashMap<String, Arc<std::sync::atomic::AtomicBool>>,
}

/// start the daemon runner
pub async fn run(config_dir: &Path) -> Result<()> {
    let global = config::load_global(config_dir)?;
    let _guard = setup_logging(&global.log_path)?;

    // verify claude is available
    check_claude_available().await?;

    let db = Database::open(&global.db_path)?;
    let global = Arc::new(global);
    let config_dir = config_dir.to_path_buf();

    // initial load
    let daemons = config::load_daemons(&config_dir)?;
    let instances = expand_all(&daemons);

    // clear any stale running_since from a previous crash
    match db.clear_stale_running(0) {
        Ok(n) if n > 0 => tracing::info!(cleared = n, "cleared stale running flags"),
        Err(e) => tracing::warn!(err=%e, "failed to clear stale running flags"),
        _ => {}
    }

    tracing::info!(
        instance_count = instances.len(),
        max_concurrent = global.max_concurrent,
        "runner starting"
    );
    eprintln!(
        "army-of-daemons: runner started with {} instances (max {} concurrent)",
        instances.len(),
        global.max_concurrent
    );

    let state = Arc::new(Mutex::new(RunnerState {
        tasks: HashMap::new(),
        running: HashMap::new(),
    }));
    let semaphore = Arc::new(tokio::sync::Semaphore::new(global.max_concurrent));

    // start all instance tasks
    start_instances(&instances, &state, &global, &db, &semaphore).await;

    // heartbeat task
    let hb_db = db.clone();
    let _heartbeat = tokio::spawn(async move {
        let pid = std::process::id();
        loop {
            if let Err(e) = hb_db.write_heartbeat(pid) {
                tracing::error!(err=%e, "heartbeat write failed");
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // pruning task
    let prune_db = db.clone();
    let prune_days = global.prune_after_days;
    let _pruner = tokio::spawn(async move {
        loop {
            match prune_db.prune(prune_days) {
                Ok(n) if n > 0 => tracing::info!(deleted = n, "pruned old reports"),
                Err(e) => tracing::error!(err=%e, "prune failed"),
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
        }
    });

    // hot-reload watcher
    let reload_state = Arc::clone(&state);
    let reload_global = Arc::clone(&global);
    let reload_db = db.clone();
    let reload_dir = config_dir.clone();
    let reload_sem = Arc::clone(&semaphore);

    let _watcher_handle = tokio::spawn(async move {
        if let Err(e) = watch_configs(reload_dir, reload_state, reload_global, reload_db, reload_sem).await {
            tracing::error!(err=%e, "config watcher failed");
        }
    });

    // wait for shutdown signal
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("received shutdown signal");
        eprintln!("\narmy-of-daemons: shutting down...");
        let _ = shutdown_tx.send(true);
    });

    shutdown_rx.changed().await.ok();

    // graceful shutdown: cancel all tasks
    let mut s = state.lock().await;
    for (id, handle) in s.tasks.drain() {
        tracing::info!(%id, "cancelling task");
        handle.abort();
    }

    // give in-flight processes a moment
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    tracing::info!("runner stopped");
    eprintln!("army-of-daemons: stopped");
    Ok(())
}

/// run a single daemon/instance check (for `check` subcommand)
pub async fn check_one(config_dir: &Path, name: &str) -> Result<()> {
    let global = config::load_global(config_dir)?;
    let db = Database::open(&global.db_path)?;
    let daemons = config::load_daemons(config_dir)?;

    let instances = expand_all(&daemons);
    let matching: Vec<_> = instances
        .iter()
        .filter(|i| i.instance_id == name || i.daemon_name == name)
        .collect();

    if matching.is_empty() {
        bail!("no daemon or instance matching '{name}'");
    }

    for instance in matching {
        eprintln!("checking {}...", instance.instance_id);
        if let Err(e) = super::pipeline::run_instance(instance, &global, &db).await {
            eprintln!("error: {e}");
        }
    }

    Ok(())
}

pub fn expand_all(daemons: &[config::DaemonConfig]) -> Vec<DaemonInstance> {
    let mut all = Vec::new();
    for d in daemons {
        all.extend(config::expand_instances(d));
    }
    all
}

async fn start_instances(
    instances: &[DaemonInstance],
    state: &Arc<Mutex<RunnerState>>,
    global: &Arc<GlobalConfig>,
    db: &Database,
    semaphore: &Arc<tokio::sync::Semaphore>,
) {
    let mut s = state.lock().await;
    for instance in instances {
        let id = instance.instance_id.clone();
        if s.tasks.contains_key(&id) {
            continue;
        }

        let inst = instance.clone();
        let g = Arc::clone(global);
        let d = db.clone();
        let sem = Arc::clone(semaphore);
        let running = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let running_flag = Arc::clone(&running);

        let schedule = match parse_schedule(&instance.config.schedule) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(instance=%id, err=%e, "invalid schedule, skipping");
                continue;
            }
        };

        let handle = tokio::spawn(async move {
            match schedule {
                Schedule::Interval(dur) => {
                    loop {
                        if running_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            tracing::warn!(instance=%inst.instance_id, "still running, skipping");
                        } else {
                            running_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                            let Ok(_permit) = sem.acquire().await else { return };
                            if let Err(e) = super::pipeline::run_instance(&inst, &g, &d).await {
                                tracing::error!(instance=%inst.instance_id, err=%e, "pipeline failed");
                            }
                            running_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                        tokio::time::sleep(dur).await;
                    }
                }
                Schedule::Cron(expr) => {
                    loop {
                        let next = next_cron_tick(&expr);
                        tokio::time::sleep(next).await;
                        if running_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            tracing::warn!(instance=%inst.instance_id, "still running, skipping");
                        } else {
                            running_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                            let Ok(_permit) = sem.acquire().await else { return };
                            if let Err(e) = super::pipeline::run_instance(&inst, &g, &d).await {
                                tracing::error!(instance=%inst.instance_id, err=%e, "pipeline failed");
                            }
                            running_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        s.running.insert(id.clone(), running);
        s.tasks.insert(id, handle);
    }
}

async fn watch_configs(
    config_dir: PathBuf,
    state: Arc<Mutex<RunnerState>>,
    global: Arc<GlobalConfig>,
    db: Database,
    semaphore: Arc<tokio::sync::Semaphore>,
) -> Result<()> {
    let daemons_dir = config_dir.join("daemons");
    if !daemons_dir.exists() {
        return Ok(());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(32);

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
            match event.kind {
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => {
                    let _ = tx.blocking_send(());
                }
                _ => {}
            }
        }
    })?;

    watcher.watch(&daemons_dir, RecursiveMode::NonRecursive)?;

    // debounce: wait a bit after changes to batch rapid edits
    loop {
        rx.recv().await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        // drain any queued events
        while rx.try_recv().is_ok() {}

        tracing::info!("config change detected, reloading");
        eprintln!("army-of-daemons: reloading configs...");

        match config::load_daemons(&config_dir) {
            Ok(daemons) => {
                let instances = expand_all(&daemons);
                // cancel all existing tasks
                let mut s = state.lock().await;
                for (id, handle) in s.tasks.drain() {
                    tracing::info!(%id, "stopping for reload");
                    handle.abort();
                }
                s.running.clear();
                drop(s);

                // brief pause for in-flight processes
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;

                // restart
                start_instances(&instances, &state, &global, &db, &semaphore).await;
                tracing::info!(count = instances.len(), "reloaded");
                eprintln!("army-of-daemons: reloaded {} instances", instances.len());
            }
            Err(e) => {
                tracing::error!(err=%e, "reload failed (config parse)");
                eprintln!("army-of-daemons: reload failed: {e}");
            }
        }
    }
}

enum Schedule {
    Interval(std::time::Duration),
    Cron(Box<cron::Schedule>),
}

fn parse_schedule(s: &str) -> Result<Schedule> {
    if s.starts_with("every ") {
        let dur = config::parse_duration(s)?;
        Ok(Schedule::Interval(dur))
    } else {
        let schedule: cron::Schedule = s.parse()
            .map_err(|e| anyhow::anyhow!("invalid cron: {e}"))?;
        Ok(Schedule::Cron(Box::new(schedule)))
    }
}

fn next_cron_tick(schedule: &cron::Schedule) -> std::time::Duration {
    use chrono::Utc;
    let now = Utc::now();
    if let Some(next) = schedule.upcoming(Utc).next() {
        let delta = next - now;
        delta.to_std().unwrap_or(std::time::Duration::from_secs(60))
    } else {
        std::time::Duration::from_secs(60)
    }
}

async fn check_claude_available() -> Result<()> {
    let output = tokio::process::Command::new("claude")
        .arg("--version")
        .output()
        .await;

    match output {
        Ok(o) if o.status.success() => {
            let version = String::from_utf8_lossy(&o.stdout);
            tracing::info!(version=%version.trim(), "claude available");
            Ok(())
        }
        Ok(o) => {
            bail!("claude --version failed: {}", String::from_utf8_lossy(&o.stderr));
        }
        Err(e) => {
            bail!("claude not found on PATH: {e}. Install claude CLI first.");
        }
    }
}

fn setup_logging(log_path: &Path) -> Result<WorkerGuard> {
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let (non_blocking, guard) = tracing_appender::non_blocking(file);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(
            fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_target(false),
        )
        .init();

    Ok(guard)
}
