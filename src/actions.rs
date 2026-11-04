use crate::config::ActionConfig;
use anyhow::Result;
use minijinja::Value;

/// actions whose `when` expression is truthy for this report (or that have no `when`)
pub fn available<'a>(actions: &'a [ActionConfig], ctx: &Value) -> Vec<&'a ActionConfig> {
    let env = crate::config::template::make_env();
    actions
        .iter()
        .filter(|a| match a.when {
            None => true,
            Some(ref when) => env
                .compile_expression(when)
                .and_then(|e| e.eval(ctx))
                .is_ok_and(|v| v.is_true()),
        })
        .collect()
}

/// render and run a shell command; errors carry the first line of stderr
pub async fn run_command(template: &str, ctx: &Value, working_dir: Option<&str>) -> Result<()> {
    let cmd = crate::config::render_template(template, ctx)?;
    let mut proc = tokio::process::Command::new("sh");
    proc.arg("-c").arg(&cmd).stdin(std::process::Stdio::null());
    if let Some(dir) = working_dir {
        proc.current_dir(crate::config::resolve_path(dir));
    }
    let output = proc.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        anyhow::bail!("exit {}: {first}", output.status.code().unwrap_or(-1));
    }
    Ok(())
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
