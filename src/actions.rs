use crate::config::ActionConfig;
use anyhow::Result;
use minijinja::Value;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// an action the LLM offered on a report, with the values it filled in
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Offer {
    pub action: String,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// an action ready to show in the TUI: its key, hint label, and render context
#[derive(Debug, Clone)]
pub struct Runnable {
    pub key: char,
    pub label: String,
    pub name: String,
    pub command: String,
    pub ctx: Value,
}

/// prompt suffix telling the LLM which actions it may offer, or None if there are none
pub fn offer_instructions(actions: &[ActionConfig]) -> Option<String> {
    let offerable: Vec<_> = actions.iter().filter(|a| a.offer).collect();
    if offerable.is_empty() {
        return None;
    }
    let mut text = String::from(
        "\n\n## Offering actions\n\n\
         You may offer the user these actions. Offer one only when your analysis clearly supports it; \
         offering nothing is fine. Arg values may only contain letters, digits, and `._/@:+-`.\n\n",
    );
    for a in offerable {
        let args = if a.args.is_empty() { "no args".to_string() } else { format!("args: {}", a.args.join(", ")) };
        text.push_str(&format!("- \"{}\" ({args})\n", a.name));
    }
    text.push_str(
        "\nTo offer actions, end your response with a fenced block like this (one entry per offer):\n\n\
         ```actions\n\
         [{\"action\": \"<name>\", \"args\": {\"<arg>\": \"<value>\"}, \"reason\": \"<few words>\"}]\n\
         ```\n",
    );
    Some(text)
}

/// pull the ```actions block out of an LLM response, keeping only valid offers
pub fn extract_offers(response: &str, actions: &[ActionConfig]) -> (String, Vec<Offer>) {
    const FENCE: &str = "```actions";
    let Some(start) = response.rfind(FENCE) else {
        return (response.to_string(), vec![]);
    };
    let inner_start = start + FENCE.len();
    let (inner, rest) = match response[inner_start..].find("```") {
        Some(end) => (&response[inner_start..inner_start + end], &response[inner_start + end + 3..]),
        None => (&response[inner_start..], ""),
    };
    let body = format!("{}{}", response[..start].trim_end(), rest.trim_end());

    let parsed: Vec<Offer> = serde_json::from_str(inner.trim()).unwrap_or_else(|e| {
        tracing::warn!(err=%e, "unparseable actions block");
        vec![]
    });
    let offers = parsed.into_iter().filter(|o| valid_offer(o, actions)).collect();
    (body, offers)
}

fn valid_offer(offer: &Offer, actions: &[ActionConfig]) -> bool {
    let Some(action) = actions.iter().find(|a| a.offer && a.name == offer.action) else {
        tracing::warn!(action=%offer.action, "offer for unknown or non-offerable action");
        return false;
    };
    let same_args = offer.args.len() == action.args.len()
        && action.args.iter().all(|a| offer.args.contains_key(a));
    let safe = offer.args.values().all(|v| {
        !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "._/@:+-".contains(c))
    });
    if !(same_args && safe) {
        tracing::warn!(action=%offer.action, args=?offer.args, "rejected offer");
    }
    same_args && safe
}

/// everything runnable on a report: static actions (filtered by `when`) then offers.
/// an action offered more than once gets digit keys instead of its own key
pub fn runnables(actions: &[ActionConfig], offers: &[Offer], ctx: &Value) -> Vec<Runnable> {
    let mut out: Vec<Runnable> = available(actions, ctx)
        .into_iter()
        .filter(|a| !a.offer)
        .filter_map(|a| {
            Some(Runnable {
                key: a.key.chars().next()?,
                label: a.name.clone(),
                name: a.name.clone(),
                command: a.command.clone(),
                ctx: ctx.clone(),
            })
        })
        .collect();

    let offerable = available(actions, ctx);
    let mut digit = 0u32;
    for offer in offers {
        let Some(action) = offerable.iter().find(|a| a.offer && a.name == offer.action) else { continue };
        let repeated = offers.iter().filter(|o| o.action == offer.action).count() > 1;
        let key = if repeated {
            digit += 1;
            match char::from_digit(digit, 10) {
                Some(d) if digit <= 9 => d,
                _ => continue,
            }
        } else {
            let Some(k) = action.key.chars().next() else { continue };
            k
        };

        let mut label = action.name.clone();
        if repeated {
            let vals: Vec<&str> = offer.args.values().map(String::as_str).collect();
            label.push_str(&format!(" {}", vals.join(" ")));
        }
        if let Some(ref reason) = offer.reason {
            label.push_str(&format!(" ({reason})"));
        }

        out.push(Runnable {
            key,
            label,
            name: action.name.clone(),
            command: action.command.clone(),
            ctx: Value::from_iter(ctx.try_iter().into_iter().flatten().filter_map(|k| {
                let v = ctx.get_item(&k).ok()?;
                Some((k.to_string(), v))
            }).chain([("args".to_string(), Value::from_serialize(&offer.args))])),
        });
    }
    out
}

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
            offer: false,
            args: vec![],
        }
    }

    fn offerable(key: &str, name: &str, args: &[&str]) -> ActionConfig {
        ActionConfig {
            key: key.into(),
            name: name.into(),
            command: format!("echo {}", args.iter().map(|a| format!("{{{{ args.{a} }}}}")).collect::<Vec<_>>().join(" ")),
            when: None,
            offer: true,
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }

    fn offer(action: &str, args: &[(&str, &str)]) -> Offer {
        Offer {
            action: action.into(),
            args: args.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            reason: None,
        }
    }

    #[test]
    fn instructions_only_mention_offerable_actions() {
        assert!(offer_instructions(&[action("o", None)]).is_none());
        let text = offer_instructions(&[action("o", None), offerable("r", "rerun", &["run_id"])]).unwrap();
        assert!(text.contains("rerun"), "{text}");
        assert!(text.contains("run_id"), "{text}");
        assert!(text.contains("```actions"), "{text}");
        assert!(!text.contains("\"o\""), "{text}");
    }

    #[test]
    fn extract_strips_block_and_parses_offers() {
        let actions = [offerable("r", "rerun", &["run_id"])];
        let resp = "# Report: x\nbody text\n\n```actions\n[{\"action\": \"rerun\", \"args\": {\"run_id\": \"123\"}, \"reason\": \"flaky\"}]\n```\n";
        let (body, offers) = extract_offers(resp, &actions);
        assert_eq!(body.trim_end(), "# Report: x\nbody text");
        assert_eq!(offers.len(), 1);
        assert_eq!(offers[0].args["run_id"], "123");
        assert_eq!(offers[0].reason.as_deref(), Some("flaky"));
    }

    #[test]
    fn extract_without_block_is_a_noop() {
        let (body, offers) = extract_offers("just text", &[offerable("r", "rerun", &["run_id"])]);
        assert_eq!(body, "just text");
        assert!(offers.is_empty());
    }

    #[test]
    fn extract_drops_invalid_offers() {
        let actions = [offerable("r", "rerun", &["run_id"]), action("o", None)];
        let resp = r#"```actions
[
  {"action": "rerun", "args": {"run_id": "1; rm -rf ~"}},
  {"action": "rerun", "args": {"run_id": "$(whoami)"}},
  {"action": "rerun", "args": {}},
  {"action": "rerun", "args": {"run_id": "1", "extra": "2"}},
  {"action": "nope", "args": {}},
  {"action": "o", "args": {}},
  {"action": "rerun", "args": {"run_id": "abc-1.2_3"}}
]
```"#;
        let (_, offers) = extract_offers(resp, &actions);
        assert_eq!(offers, vec![offer("rerun", &[("run_id", "abc-1.2_3")])]);
    }

    #[test]
    fn extract_survives_garbage_json() {
        let (body, offers) = extract_offers("text\n```actions\nnot json\n```", &[offerable("r", "rerun", &[])]);
        assert_eq!(body.trim_end(), "text");
        assert!(offers.is_empty());
    }

    #[test]
    fn runnables_merge_static_and_offered() {
        let actions = vec![action("o", None), offerable("r", "rerun", &["run_id"])];
        let ctx = Value::from_serialize(json!({}));
        let rs = runnables(&actions, &[offer("rerun", &[("run_id", "9")])], &ctx);
        let keys: Vec<_> = rs.iter().map(|r| r.key).collect();
        assert_eq!(keys, vec!['o', 'r']);
        assert_eq!(crate::config::render_template(&rs[1].command, &rs[1].ctx).unwrap(), "echo 9");
    }

    #[test]
    fn unoffered_actions_are_hidden() {
        let actions = vec![offerable("r", "rerun", &["run_id"])];
        let ctx = Value::from_serialize(json!({}));
        assert!(runnables(&actions, &[], &ctx).is_empty());
    }

    #[test]
    fn repeated_offers_get_digit_keys_and_labels_with_args() {
        let actions = vec![offerable("r", "rerun", &["run_id"])];
        let ctx = Value::from_serialize(json!({}));
        let mut second = offer("rerun", &[("run_id", "2")]);
        second.reason = Some("flaky".into());
        let rs = runnables(&actions, &[offer("rerun", &[("run_id", "1")]), second], &ctx);
        let keys: Vec<_> = rs.iter().map(|r| r.key).collect();
        assert_eq!(keys, vec!['1', '2']);
        assert!(rs[1].label.contains('2') && rs[1].label.contains("flaky"), "{}", rs[1].label);
        assert_eq!(crate::config::render_template(&rs[0].command, &rs[0].ctx).unwrap(), "echo 1");
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
