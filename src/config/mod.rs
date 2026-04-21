pub mod template;

pub use template::render_template;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct GlobalConfig {
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,
    #[serde(default = "default_log_path")]
    pub log_path: PathBuf,
    #[serde(default = "default_prune_days")]
    pub prune_after_days: u32,
    #[serde(default = "default_display_days")]
    pub display_after_days: u32,
    #[serde(default = "default_model")]
    pub default_model: String,
    #[serde(default)]
    pub default_allowed_tools: String,
}

fn default_db_path() -> PathBuf {
    dirs::home_dir().unwrap().join(".daemon-army/army.db")
}
fn default_log_path() -> PathBuf {
    dirs::home_dir().unwrap().join(".daemon-army/runner.log")
}
fn default_prune_days() -> u32 { 30 }
fn default_display_days() -> u32 { 7 }
fn default_model() -> String { "sonnet".into() }

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            log_path: default_log_path(),
            prune_after_days: default_prune_days(),
            display_after_days: default_display_days(),
            default_model: default_model(),
            default_allowed_tools: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct DaemonConfig {
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub description: String,
    pub schedule: String,
    #[serde(default)]
    pub working_directory: Option<String>,
    pub source: SourceConfig,
    #[serde(default)]
    pub filter: Option<FilterConfig>,
    pub analyze: AnalyzeConfig,
    #[serde(default)]
    #[allow(dead_code)]
    pub report: ReportConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceConfig {
    pub command: String,
    #[serde(default = "default_format")]
    pub format: String,
    #[serde(default)]
    pub params: HashMap<String, Vec<String>>,
    #[serde(default = "default_success_codes")]
    pub success_codes: Vec<i32>,
    #[serde(default = "default_max_items")]
    pub max_items: usize,
}

fn default_format() -> String { "jsonl".into() }
fn default_success_codes() -> Vec<i32> { vec![0] }
fn default_max_items() -> usize { 200 }

#[derive(Debug, Clone, Deserialize)]
pub struct FilterConfig {
    #[serde(default)]
    pub rules: Vec<FilterRule>,
    #[serde(default)]
    pub dedup_key: Option<String>,
    #[serde(default)]
    pub dedup_window: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FilterRule {
    pub field: String,
    pub op: String,
    #[serde(default)]
    pub value: Option<toml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AnalyzeConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub allowed_tools: Option<String>,
    pub prompt: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub timeout: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[allow(dead_code)]
pub struct ReportConfig {
    #[serde(default = "default_true")]
    pub notify: bool,
}

fn default_true() -> bool { true }

/// a single expanded instance from a daemon config + param combination
#[derive(Debug, Clone)]
pub struct DaemonInstance {
    pub instance_id: String,
    pub daemon_name: String,
    pub config: DaemonConfig,
    pub params: HashMap<String, String>,
}

pub fn load_global(config_dir: &Path) -> Result<GlobalConfig> {
    let path = config_dir.join("config.toml");
    if !path.exists() {
        return Ok(GlobalConfig::default());
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let config: GlobalConfig = toml::from_str(&content)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(config)
}

pub fn load_daemons(config_dir: &Path) -> Result<Vec<DaemonConfig>> {
    let daemons_dir = config_dir.join("daemons");
    if !daemons_dir.exists() {
        return Ok(vec![]);
    }

    let mut configs = Vec::new();
    for entry in std::fs::read_dir(&daemons_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "toml") {
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let config: DaemonConfig = toml::from_str(&content)
                .with_context(|| format!("parsing {}", path.display()))?;
            configs.push(config);
        }
    }
    Ok(configs)
}

/// expand a daemon config into instances via cartesian product of params
pub fn expand_instances(config: &DaemonConfig) -> Vec<DaemonInstance> {
    let params = &config.source.params;
    if params.is_empty() {
        return vec![DaemonInstance {
            instance_id: config.name.clone(),
            daemon_name: config.name.clone(),
            config: config.clone(),
            params: HashMap::new(),
        }];
    }

    let keys: Vec<&String> = params.keys().collect();
    let value_lists: Vec<&Vec<String>> = keys.iter().map(|k| &params[*k]).collect();
    let combos = cartesian_product(&value_lists);

    combos
        .into_iter()
        .map(|combo| {
            let mut param_map = HashMap::new();
            // build the instance id suffix from param values
            let mut suffix_parts = Vec::new();
            for (i, key) in keys.iter().enumerate() {
                param_map.insert((*key).clone(), combo[i].clone());
                suffix_parts.push(combo[i].clone());
            }
            let suffix = suffix_parts.join(",");
            DaemonInstance {
                instance_id: format!("{}::{}", config.name, suffix),
                daemon_name: config.name.clone(),
                config: config.clone(),
                params: param_map,
            }
        })
        .collect()
}

fn cartesian_product(lists: &[&Vec<String>]) -> Vec<Vec<String>> {
    if lists.is_empty() {
        return vec![vec![]];
    }
    let rest = cartesian_product(&lists[1..]);
    let mut result = Vec::new();
    for item in lists[0] {
        for r in &rest {
            let mut combo = vec![item.clone()];
            combo.extend(r.clone());
            result.push(combo);
        }
    }
    result
}

/// resolve ~ and env vars in a path string
pub fn resolve_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}

/// parse a duration string like "every 60s", "every 5m", "30m", "1h"
pub fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let s = s.strip_prefix("every ").unwrap_or(s).trim();
    let (num_str, unit) = if let Some(n) = s.strip_suffix("ms") {
        (n, "ms")
    } else if let Some(n) = s.strip_suffix('s') {
        (n, "s")
    } else if let Some(n) = s.strip_suffix('m') {
        (n, "m")
    } else if let Some(n) = s.strip_suffix('h') {
        (n, "h")
    } else {
        bail!("unknown duration unit in: {s}");
    };

    let num: u64 = num_str.parse().with_context(|| format!("invalid duration: {s}"))?;
    match unit {
        "ms" => Ok(std::time::Duration::from_millis(num)),
        "s" => Ok(std::time::Duration::from_secs(num)),
        "m" => Ok(std::time::Duration::from_secs(num * 60)),
        "h" => Ok(std::time::Duration::from_secs(num * 3600)),
        _ => unreachable!(),
    }
}

pub fn validate_all(config_dir: &Path) -> Result<()> {
    let _global = load_global(config_dir)?;
    let daemons = load_daemons(config_dir)?;

    if daemons.is_empty() {
        println!("warning: no daemon configs found in {}/daemons/", config_dir.display());
        return Ok(());
    }

    let mut total_instances = 0;
    for daemon in &daemons {
        let instances = expand_instances(daemon);
        total_instances += instances.len();
        println!("  {} -> {} instance(s)", daemon.name, instances.len());

        // validate schedule
        if daemon.schedule.starts_with("every ") {
            parse_duration(&daemon.schedule)
                .with_context(|| format!("daemon '{}': invalid schedule", daemon.name))?;
        }
        // else it should be cron — we'll validate that when we parse it

        // validate templates compile
        for inst in &instances {
            let ctx = template::build_context(inst, &[], 0);
            let env = template::make_env();
            env.render_str(&daemon.analyze.prompt, &ctx)
                .with_context(|| format!("daemon '{}': prompt template error", daemon.name))?;
            if let Some(ref dk) = daemon.filter.as_ref().and_then(|f| f.dedup_key.clone()) {
                env.render_str(dk, &ctx)
                    .with_context(|| format!("daemon '{}': dedup_key template error", daemon.name))?;
            }
        }
    }

    println!("total instances: {total_instances}");
    Ok(())
}
