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
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
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
fn default_max_concurrent() -> usize { 5 }

impl Default for GlobalConfig {
    fn default() -> Self {
        Self {
            db_path: default_db_path(),
            log_path: default_log_path(),
            prune_after_days: default_prune_days(),
            display_after_days: default_display_days(),
            default_model: default_model(),
            default_allowed_tools: String::new(),
            max_concurrent: default_max_concurrent(),
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
    #[serde(default)]
    pub analyze: Option<AnalyzeConfig>,
    #[serde(default)]
    #[allow(dead_code)]
    pub report: ReportConfig,
    #[serde(default)]
    pub per: Per,
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub on_read: Option<String>,
    #[serde(default)]
    pub actions: Vec<ActionConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Per {
    #[default]
    Run,
    Item,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonKind {
    /// LLM writes the report and assigns severity
    Analyze,
    /// runs the source command, no report
    Worker,
    /// report rendered from `title`/`body` templates, no severity
    Static,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActionConfig {
    pub key: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub when: Option<String>,
    /// only shown when an analyze daemon's LLM offers it on a report
    #[serde(default)]
    pub offer: bool,
    /// values the LLM fills in for an offered action, available as `args.<name>`
    #[serde(default)]
    pub args: Vec<String>,
}

impl DaemonConfig {
    pub fn kind(&self) -> DaemonKind {
        match (&self.analyze, &self.body) {
            (Some(_), _) => DaemonKind::Analyze,
            (None, Some(_)) => DaemonKind::Static,
            (None, None) => DaemonKind::Worker,
        }
    }
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

/// structural + template checks for a single daemon
pub fn validate_daemon(config: &DaemonConfig) -> Result<()> {
    let name = &config.name;
    if config.body.is_some() && config.analyze.is_some() {
        bail!("daemon '{name}': `body` can't be combined with [analyze]");
    }

    let env = template::make_env();
    let mut templates: Vec<(String, &str)> = Vec::new();
    templates.extend(config.title.as_deref().map(|t| ("title".to_string(), t)));
    templates.extend(config.body.as_deref().map(|t| ("body".to_string(), t)));
    templates.extend(config.on_read.as_deref().map(|t| ("on_read".to_string(), t)));

    let mut keys = std::collections::HashSet::new();
    for action in &config.actions {
        let label = format!("action '{}'", action.name);
        if action.key.chars().count() != 1 {
            bail!("daemon '{name}': {label}: key must be a single character, got {:?}", action.key);
        }
        if !keys.insert(&action.key) {
            bail!("daemon '{name}': {label}: duplicate key '{}'", action.key);
        }
        if action.offer && config.kind() != DaemonKind::Analyze {
            bail!("daemon '{name}': {label}: offered actions need an [analyze] section");
        }
        if !action.offer && !action.args.is_empty() {
            bail!("daemon '{name}': {label}: `args` only works with offer = true");
        }
        for arg in &action.args {
            let ident = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if !ident {
                bail!("daemon '{name}': {label}: arg {arg:?} must be alphanumeric/underscore");
            }
        }
        let tmpl = env.template_from_str(&action.command)
            .with_context(|| format!("daemon '{name}': {label} command template error"))?;
        if !action.offer && tmpl.undeclared_variables(false).contains("args") {
            bail!("daemon '{name}': {label}: `args` only works with offer = true");
        }
        templates.push((format!("{label} command"), &action.command));

        if let Some(ref when) = action.when {
            let expr = env.compile_expression(when)
                .with_context(|| format!("daemon '{name}': {label}: invalid `when`"))?;
            if config.per == Per::Run && expr.undeclared_variables(false).contains("item") {
                bail!("daemon '{name}': {label}: `when` uses `item`, which needs per = \"item\"");
            }
        }
    }

    for (label, src) in templates {
        let tmpl = env.template_from_str(src)
            .with_context(|| format!("daemon '{name}': {label} template error"))?;
        if config.per == Per::Run && tmpl.undeclared_variables(false).contains("item") {
            bail!("daemon '{name}': {label} uses `item`, which needs per = \"item\"");
        }
    }
    Ok(())
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
        validate_daemon(daemon)?;

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
            if let Some(ref analyze) = daemon.analyze {
                env.render_str(&analyze.prompt, &ctx)
                    .with_context(|| format!("daemon '{}': prompt template error", daemon.name))?;
            }
            if let Some(ref dk) = daemon.filter.as_ref().and_then(|f| f.dedup_key.clone()) {
                let ctx = template::build_dedup_context(inst, &serde_json::json!({}), 0, "");
                env.render_str(dk, &ctx)
                    .with_context(|| format!("daemon '{}': dedup_key template error", daemon.name))?;
            }
        }
    }

    println!("total instances: {total_instances}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> DaemonConfig {
        toml::from_str(toml_str).expect("valid toml")
    }

    const BASE: &str = r#"
name = "d"
schedule = "every 1m"
[source]
command = "echo"
"#;

    fn with(extra_top: &str, extra_tail: &str) -> DaemonConfig {
        parse(&format!("{extra_top}\n{BASE}\n{extra_tail}"))
    }

    #[test]
    fn kind_is_inferred_from_sections() {
        assert_eq!(with("", "").kind(), DaemonKind::Worker);
        assert_eq!(with("", "[analyze]\nprompt = \"hi\"").kind(), DaemonKind::Analyze);
        assert_eq!(with("body = \"hi\"", "").kind(), DaemonKind::Static);
        // title alone doesn't make a static daemon
        assert_eq!(with("title = \"t\"", "").kind(), DaemonKind::Worker);
    }

    #[test]
    fn per_defaults_to_run() {
        assert_eq!(with("", "").per, Per::Run);
        assert_eq!(with("per = \"item\"", "").per, Per::Item);
    }

    #[test]
    fn worker_static_and_analyze_validate() {
        validate_daemon(&with("", "")).unwrap();
        validate_daemon(&with("body = \"{{ items | length }}\"", "")).unwrap();
        validate_daemon(&with("title = \"{{ params.x }}\"", "[analyze]\nprompt = \"hi\"")).unwrap();
    }

    #[test]
    fn body_with_analyze_is_rejected() {
        let c = with("body = \"hi\"", "[analyze]\nprompt = \"hi\"");
        let err = validate_daemon(&c).unwrap_err().to_string();
        assert!(err.contains("body"), "{err}");
    }

    #[test]
    fn item_on_per_run_is_rejected() {
        for (top, tail) in [
            ("body = \"{{ item.x }}\"", ""),
            ("title = \"{{ item.x }}\"\nbody = \"b\"", ""),
            ("on_read = \"echo {{ item.x }}\"", ""),
            ("", "[[actions]]\nkey = \"o\"\nname = \"o\"\ncommand = \"open {{ item.url }}\""),
            ("", "[[actions]]\nkey = \"o\"\nname = \"o\"\ncommand = \"true\"\nwhen = \"item.url\""),
        ] {
            let c = with(top, tail);
            assert!(validate_daemon(&c).is_err(), "expected error for {top} / {tail}");
            let c = with(&format!("per = \"item\"\n{top}"), tail);
            validate_daemon(&c).unwrap_or_else(|e| panic!("per=item should allow {top} / {tail}: {e}"));
        }
    }

    #[test]
    fn loop_variable_named_item_is_fine_on_per_run() {
        let c = with("body = \"{% for item in items %}{{ item.x }}{% endfor %}\"", "");
        validate_daemon(&c).unwrap();
    }

    #[test]
    fn action_keys_must_be_single_unique_chars() {
        let dup = with("", r#"
[[actions]]
key = "o"
name = "a"
command = "true"
[[actions]]
key = "o"
name = "b"
command = "true"
"#);
        assert!(validate_daemon(&dup).is_err());

        for bad in ["", "oo", "esc"] {
            let c = with("", &format!("[[actions]]\nkey = \"{bad}\"\nname = \"a\"\ncommand = \"true\""));
            assert!(validate_daemon(&c).is_err(), "key {bad:?} should be rejected");
        }
    }

    const OFFER: &str = r#"
[[actions]]
key = "r"
name = "rerun"
offer = true
args = ["run_id"]
command = "gh run rerun {{ args.run_id }}"
"#;

    #[test]
    fn offered_actions_validate_on_analyze_daemons() {
        let c = with("", &format!("[analyze]\nprompt = \"hi\"\n{OFFER}"));
        validate_daemon(&c).unwrap();
        let a = &c.actions[0];
        assert!(a.offer);
        assert_eq!(a.args, vec!["run_id"]);
    }

    #[test]
    fn offered_actions_need_an_analyze_daemon() {
        assert!(validate_daemon(&with("", OFFER)).is_err());
        assert!(validate_daemon(&with("body = \"b\"", OFFER)).is_err());
    }

    #[test]
    fn args_only_allowed_on_offered_actions() {
        let c = with("", "[analyze]\nprompt = \"hi\"\n[[actions]]\nkey = \"o\"\nname = \"o\"\ncommand = \"open {{ args.x }}\"");
        assert!(validate_daemon(&c).is_err());
        let c = with("", "[analyze]\nprompt = \"hi\"\n[[actions]]\nkey = \"o\"\nname = \"o\"\nargs = [\"x\"]\ncommand = \"true\"");
        assert!(validate_daemon(&c).is_err());
    }

    #[test]
    fn arg_names_must_be_identifiers() {
        let tail = OFFER.replace("[\"run_id\"]", "[\"run id\"]");
        let c = with("", &format!("[analyze]\nprompt = \"hi\"\n{tail}"));
        assert!(validate_daemon(&c).is_err());
    }

    #[test]
    fn broken_templates_are_rejected() {
        assert!(validate_daemon(&with("body = \"{{ oops\"", "")).is_err());
        assert!(validate_daemon(&with("", "[[actions]]\nkey = \"o\"\nname = \"a\"\ncommand = \"true\"\nwhen = \"((\"")).is_err());
    }
}
