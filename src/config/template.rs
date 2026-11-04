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

    map.insert("params".into(), Value::from_serialize(&instance.params));
    if instance.config.per == crate::config::Per::Item
        && let [item] = items
    {
        map.insert("item".into(), Value::from_serialize(item));
    }

    map.insert("items".into(), Value::from_serialize(items));
    map.insert("items_json".into(), Value::from(items_json));
    map.insert("count".into(), Value::from(count));

    map.insert("recent_reports".into(), Value::from_serialize(recent_reports));
    let recent_reports_json = serde_json::to_string_pretty(recent_reports).unwrap_or_default();
    map.insert("recent_reports_json".into(), Value::from(recent_reports_json));

    Value::from_serialize(&map)
}

/// context for rendering a dedup key: magic vars, `params`, and `item`
pub fn build_dedup_context(
    instance: &DaemonInstance,
    item: &serde_json::Value,
    run_number: u64,
    since: &str,
) -> Value {
    let mut map: HashMap<String, Value> = HashMap::new();
    map.insert("since".into(), Value::from(since.to_string()));
    map.insert("daemon_name".into(), Value::from(instance.daemon_name.clone()));
    map.insert("instance_id".into(), Value::from(instance.instance_id.clone()));
    map.insert("run_number".into(), Value::from(run_number));
    map.insert("params".into(), Value::from_serialize(&instance.params));
    map.insert("item".into(), Value::from_serialize(item));
    Value::from_serialize(&map)
}

/// context for actions and `on_read`, rebuilt from a stored report
pub fn build_report_context(instance: &DaemonInstance, report: &crate::db::Report) -> Value {
    let items: Vec<serde_json::Value> = report
        .source_items_json
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    let since = chrono::DateTime::from_timestamp(report.created_at, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();
    build_context_with_since(instance, &items, 0, &since)
}

pub fn render_template(template_str: &str, ctx: &Value) -> anyhow::Result<String> {
    render(template_str, ctx)
}

pub fn render(template_str: &str, ctx: &Value) -> anyhow::Result<String> {
    let env = make_env();
    let rendered = env.render_str(template_str, ctx)?;
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DaemonConfig, Per};
    use serde_json::json;

    fn instance(per: Per) -> DaemonInstance {
        let mut config: DaemonConfig = toml::from_str(
            "name = \"d\"\nschedule = \"every 1m\"\n[source]\ncommand = \"echo\"",
        ).unwrap();
        config.per = per;
        DaemonInstance {
            instance_id: "d::r".into(),
            daemon_name: "d".into(),
            config,
            params: HashMap::from([("repo".to_string(), "r".to_string())]),
        }
    }

    fn report(items: serde_json::Value) -> crate::db::Report {
        crate::db::Report {
            id: 1,
            instance_id: "d::r".into(),
            created_at: 0,
            severity: 0,
            title: None,
            dedup_key: None,
            analysis: String::new(),
            source_items_json: Some(items.to_string()),
            session_id: None,
            working_dir: None,
            is_read: false,
            is_error: false,
        }
    }

    #[test]
    fn params_are_namespaced() {
        let ctx = build_context(&instance(Per::Run), &[], 0);
        assert_eq!(render("{{ params.repo }}", &ctx).unwrap(), "r");
        assert_eq!(render("{{ repo }}", &ctx).unwrap(), "");
    }

    #[test]
    fn per_item_context_exposes_item() {
        let items = [json!({"x": 1})];
        let ctx = build_context(&instance(Per::Item), &items, 0);
        assert_eq!(render("{{ item.x }}|{{ items | length }}", &ctx).unwrap(), "1|1");

        let ctx = build_context(&instance(Per::Run), &items, 0);
        assert_eq!(render("{{ item }}", &ctx).unwrap(), "");
    }

    #[test]
    fn dedup_context_has_item_and_params_but_no_bare_fields() {
        let ctx = build_dedup_context(&instance(Per::Run), &json!({"id": 7}), 0, "");
        assert_eq!(render("{{ params.repo }}:{{ item.id }}", &ctx).unwrap(), "r:7");
        assert_eq!(render("{{ id }}", &ctx).unwrap(), "");
    }

    #[test]
    fn report_context_rebuilds_items() {
        let r = report(json!([{"id": 1}, {"id": 2}]));
        let ctx = build_report_context(&instance(Per::Run), &r);
        assert_eq!(render("{{ params.repo }}:{{ items | length }}:{{ item }}", &ctx).unwrap(), "r:2:");

        let r = report(json!([{"id": 9}]));
        let ctx = build_report_context(&instance(Per::Item), &r);
        assert_eq!(render("{{ item.id }}", &ctx).unwrap(), "9");
    }
}
