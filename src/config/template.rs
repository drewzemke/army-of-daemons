use super::DaemonInstance;
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
    let since = ""; // filled in by caller if needed
    build_context_with_since(instance, items, run_number, since)
}

pub fn build_context_with_since(
    instance: &DaemonInstance,
    items: &[serde_json::Value],
    run_number: u64,
    since: &str,
) -> Value {
    let now = Utc::now();
    let items_json = serde_json::to_string_pretty(items).unwrap_or_default();
    let count = items.len();

    // start with magic variables
    let mut map: HashMap<String, Value> = HashMap::new();
    map.insert("since".into(), Value::from(since.to_string()));
    map.insert("timestamp".into(), Value::from(now.to_rfc3339()));
    map.insert("daemon_name".into(), Value::from(instance.daemon_name.clone()));
    map.insert("instance_id".into(), Value::from(instance.instance_id.clone()));
    map.insert("run_number".into(), Value::from(run_number));

    // inject params
    for (k, v) in &instance.params {
        map.insert(k.clone(), Value::from(v.clone()));
    }

    // items for the analyze prompt
    let items_val = Value::from_serialize(items);
    map.insert("items".into(), items_val);
    map.insert("items_json".into(), Value::from(items_json));
    map.insert("count".into(), Value::from(count));

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
