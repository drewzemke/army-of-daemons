use crate::config::ActionConfig;
use anyhow::Result;
use minijinja::Value;

/// actions whose `when` expression is truthy for this report (or that have no `when`)
pub fn available<'a>(_actions: &'a [ActionConfig], _ctx: &Value) -> Vec<&'a ActionConfig> {
    todo!()
}

/// render and run a shell command; errors carry the first line of stderr
pub async fn run_command(_template: &str, _ctx: &Value, _working_dir: Option<&str>) -> Result<()> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn action(key: &str, when: Option<&str>) -> ActionConfig {
        ActionConfig {
            key: key.into(),
            name: key.into(),
            command: "true".into(),
            when: when.map(String::from),
        }
    }

    #[test]
    fn when_filters_actions() {
        let actions = vec![
            action("a", None),
            action("b", Some("item.url")),
            action("c", Some("item.missing")),
            action("d", Some("item.kind == 'pr'")),
        ];
        let ctx = Value::from_serialize(json!({"item": {"url": "x", "kind": "pr"}}));
        let keys: Vec<_> = available(&actions, &ctx).iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["a", "b", "d"]);

        let ctx = Value::from_serialize(json!({"item": {"url": null}}));
        let keys: Vec<_> = available(&actions, &ctx).iter().map(|a| a.key.as_str()).collect();
        assert_eq!(keys, vec!["a"]);
    }

    #[tokio::test]
    async fn run_command_renders_and_reports_stderr() {
        let ctx = Value::from_serialize(json!({"item": {"code": 3}}));
        run_command("exit 0", &ctx, None).await.unwrap();
        let err = run_command("echo boom {{ item.code }} >&2; echo more >&2; exit 1", &ctx, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("boom 3"), "{err}");
        assert!(!err.contains("more"), "{err}");
    }
}
