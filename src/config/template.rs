use super::DaemonInstance;
use crate::db::ReportSummary;
use chrono::Utc;
use minijinja::{Environment, Value};
use std::collections::HashMap;

pub fn make_env() -> Environment<'static> {
    Environment::new()
}

pub fn build_context(
    instance: &DaemonInstance,
    items: &[serde_json::Value],
    run_number: u64,
) -> Value {
    build_context_full(instance, items, run_number, "", &[])
}

pub fn build_context_with_since(
    instance: &DaemonInstance,
    items: &[serde_json::Value],
    run_number: u64,
    since: &str,
) -> Value {
    build_context_full(instance, items, run_number, since, &[])
}

pub fn build_context_full(
    instance: &DaemonInstance,
    items: &[serde_json::Value],
    run_number: u64,
    since: &str,
    recent_reports: &[ReportSummary],
) -> Value {
    let now = Utc::now();
    let items_json = serde_json::to_string_pretty(items).unwrap_or_default();
    let count = items.len();

    let mut map: HashMap<String, Value> = HashMap::new();
    map.insert("since".into(), Value::from(since.to_string()));
    map.insert("timestamp".into(), Value::from(now.to_rfc3339()));
    map.insert("daemon_name".into(), Value::from(instance.daemon_name.clone()));
    map.insert("instance_id".into(), Value::from(instance.instance_id.clone()));
    map.insert("run_number".into(), Value::from(run_number));

    for (k, v) in &instance.params {
        map.insert(k.clone(), Value::from(v.clone()));
    }

    map.insert("items".into(), Value::from_serialize(items));
    map.insert("items_json".into(), Value::from(items_json));
    map.insert("count".into(), Value::from(count));

    map.insert("recent_reports".into(), Value::from_serialize(recent_reports));
    let recent_reports_json = serde_json::to_string_pretty(recent_reports).unwrap_or_default();
    map.insert("recent_reports_json".into(), Value::from(recent_reports_json));

    Value::from_serialize(&map)
}

pub fn render_template(template_str: &str, ctx: &Value) -> anyhow::Result<String> {
    render(template_str, ctx)
}

pub fn render(template_str: &str, ctx: &Value) -> anyhow::Result<String> {
    let env = make_env();
    let rendered = env.render_str(template_str, ctx)?;
    Ok(rendered)
}
