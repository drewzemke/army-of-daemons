use anyhow::{Result, Context};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{self, DaemonInstance, DaemonKind, GlobalConfig, Per};
use crate::db::Database;

static SEVERITY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)severity:\s*([1-5])").unwrap());

pub struct DryRunOutput {
    pub instance_id: String,
    #[allow(dead_code)]
    pub command: String,
    pub raw_count: usize,
    pub filtered_items: Vec<Value>,
    pub rendered_prompt: String,
}

/// run source + filter only, skip analyze and report
pub async fn dry_run_instance(instance: &DaemonInstance) -> Result<DryRunOutput> {
    let config = &instance.config;
    let since = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339();

    let source_ctx = config::template::build_context_with_since(instance, &[], 0, &since);

    let rendered_cmd = config::render_template(&config.source.command, &source_ctx)
        .context("rendering source command template")?;

    let working_dir = config.working_directory.as_deref()
        .map(|wd| config::render_template(wd, &source_ctx))
        .transpose()
        .context("rendering working_directory template")?;

    let items = crate::daemon::source::execute(
        &rendered_cmd,
        &config.source.format,
        &config.source.success_codes,
        config.source.max_items,
        working_dir.as_deref(),
    ).await?;

    let raw_count = items.len();

    let filtered = if let Some(ref filter_config) = config.filter {
        crate::daemon::filter::apply_rules(items, &filter_config.rules)
    } else {
        items
    };

    // preview what the first report would be built from
    let first_batch = batch_items(config.per, filtered.clone()).into_iter().next().unwrap_or_default();
    let ctx = config::template::build_context_with_since(instance, &first_batch, 0, &since);
    let rendered_prompt = match config.kind() {
        DaemonKind::Analyze => config::render_template(&config.analyze.as_ref().unwrap().prompt, &ctx)
            .context("rendering analyze prompt")?,
        DaemonKind::Static if first_batch.is_empty() => String::new(),
        DaemonKind::Static => {
            let (title, body) = render_static(config, &ctx)?;
            format!("{}\n\n{body}", title.unwrap_or_default())
        }
        DaemonKind::Worker => String::new(),
    };

    Ok(DryRunOutput {
        instance_id: instance.instance_id.clone(),
        command: rendered_cmd,
        raw_count,
        filtered_items: filtered,
        rendered_prompt,
    })
}

/// run the full pipeline for one daemon instance
pub async fn run_instance(instance: &DaemonInstance, global: &GlobalConfig, db: &Database) -> Result<()> {
    let instance_id = &instance.instance_id;
    let config = &instance.config;

    // ensure instance exists in db
    let params_json = if instance.params.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&instance.params)?)
    };
    db.upsert_instance(instance_id, &instance.daemon_name, params_json.as_deref())?;

    let _ = db.set_running(instance_id);
    let _guard = RunningGuard { db: db.clone(), instance_id: instance_id.clone() };

    // figure out "since"
    let since = match db.get_last_checked(instance_id)? {
        Some(ts) => chrono::DateTime::from_timestamp(ts, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        None => {
            // first run gets a 24h lookback so there's something to work with
            let five_min_ago = chrono::Utc::now() - chrono::Duration::hours(24);
            five_min_ago.to_rfc3339()
        }
    };

    let (_, run_count) = db.update_instance_run(instance_id, false)?;

    // build template context (empty items for source command rendering)
    let source_ctx = config::template::build_context_with_since(
        instance, &[], run_count as u64, &since,
    );

    // render source command
    let rendered_cmd = config::render_template(&config.source.command, &source_ctx)
        .context("rendering source command template")?;

    let working_dir = config.working_directory.as_deref()
        .map(|wd| config::render_template(wd, &source_ctx))
        .transpose()
        .context("rendering working_directory template")?;

    // 1. SOURCE
    tracing::info!(%instance_id, cmd=%rendered_cmd, "executing source");
    let items = match crate::daemon::source::execute(
        &rendered_cmd,
        &config.source.format,
        &config.source.success_codes,
        config.source.max_items,
        working_dir.as_deref(),
    ).await {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(%instance_id, err=%e, "source failed");
            report_error(db, instance_id, &format!("source command failed: {e}"))?;
            return Ok(());
        }
    };

    // source succeeded — clear error state if it was set
    db.set_error(instance_id, false)?;
    tracing::info!(%instance_id, count=items.len(), "source returned items");

    if items.is_empty() {
        db.update_instance_run(instance_id, true)?;
        return Ok(());
    }

    // 2. FILTER
    let filtered = if let Some(ref filter_config) = config.filter {
        let after_rules = crate::daemon::filter::apply_rules(items, &filter_config.rules);
        tracing::info!(%instance_id, count=after_rules.len(), "after filter rules");

        // dedup
        if let Some(ref dedup_key_template) = filter_config.dedup_key {
            let window_secs = filter_config
                .dedup_window
                .as_deref()
                .map(|w| config::parse_duration(w).map(|d| d.as_secs() as i64))
                .transpose()?
                .unwrap_or(1800); // default 30min

            let mut kept = Vec::new();
            for item in after_rules {
                let merged_ctx = config::template::build_dedup_context(instance, &item, run_count as u64, &since);
                let key = config::render_template(dedup_key_template, &merged_ctx)
                    .unwrap_or_default();

                // empty key = broken template, skip dedup
                let key_stripped: String = key.chars().filter(|c| !matches!(c, ':' | '/' | ',' | ' ')).collect();
                if key_stripped.is_empty() {
                    tracing::warn!(%instance_id, "dedup key rendered empty, skipping dedup for item");
                    kept.push(item);
                    continue;
                }

                match db.check_dedup(instance_id, &key, window_secs) {
                    Ok(true) => {
                        tracing::debug!(%instance_id, %key, "dedup hit, skipping");
                    }
                    Ok(false) => {
                        // mark as seen immediately so all items are tracked, not just
                        // the one that ends up as the report's dedup_key
                        let _ = db.mark_seen(instance_id, &key);
                        kept.push(item);
                    }
                    Err(e) => {
                        tracing::warn!(%instance_id, err=%e, "dedup check failed, keeping item");
                        kept.push(item);
                    }
                }
            }
            tracing::info!(%instance_id, kept=kept.len(), "after dedup");
            kept
        } else {
            after_rules
        }
    } else {
        items
    };

    if filtered.is_empty() {
        db.update_instance_run(instance_id, true)?;
        return Ok(());
    }

    if config.kind() == DaemonKind::Worker {
        db.update_instance_run(instance_id, true)?;
        tracing::info!(%instance_id, count=filtered.len(), "worker run complete (no report)");
        return Ok(());
    }

    for batch in batch_items(config.per, filtered) {
        let ok = create_report(instance, global, db, &batch, run_count as u64, &since, working_dir.as_deref()).await?;
        if !ok {
            return Ok(());
        }
    }

    db.update_instance_run(instance_id, true)?;
    Ok(())
}

/// analyze/render one batch and store it as a report. returns false if an
/// error report was written instead
async fn create_report(
    instance: &DaemonInstance,
    global: &GlobalConfig,
    db: &Database,
    batch: &[Value],
    run_count: u64,
    since: &str,
    working_dir: Option<&str>,
) -> Result<bool> {
    let instance_id = &instance.instance_id;
    let config = &instance.config;
    let recent_reports = db.recent_report_summaries(instance_id, 10).unwrap_or_default();
    let ctx = config::template::build_context_full(instance, batch, run_count, since, &recent_reports);

    let (severity, title, body, session_id) = if let Some(ref analyze) = config.analyze {
        // 3. ANALYZE
        let rendered_prompt = config::render_template(&analyze.prompt, &ctx)
            .context("rendering analyze prompt")?;
        let model = analyze.model.as_deref().unwrap_or(&global.default_model);
        let allowed_tools = analyze.allowed_tools.as_deref()
            .unwrap_or(&global.default_allowed_tools);

        tracing::info!(%instance_id, prompt_bytes=rendered_prompt.len(), %model, "invoking claude");
        let response = match invoke_claude(&rendered_prompt, model, allowed_tools, working_dir).await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::error!(%instance_id, err=%e, "claude invocation failed");
                report_error(db, instance_id, &format!("claude analysis failed: {e}"))?;
                return Ok(false);
            }
        };
        let (severity, llm_title, body) = parse_response_header(&response.result);
        let title = match config.title {
            Some(ref t) => Some(config::render_template(t, &ctx).context("rendering title")?),
            None => llm_title,
        };
        (severity, title, body, response.session_id)
    } else {
        let (title, body) = render_static(config, &ctx)?;
        (0, title, body, None)
    };

    // 4. REPORT
    let dedup_key = config.filter.as_ref()
        .and_then(|f| f.dedup_key.as_ref())
        .zip(batch.first())
        .and_then(|(tmpl, item)| {
            let item_ctx = config::template::build_dedup_context(instance, item, run_count, since);
            config::render_template(tmpl, &item_ctx).ok()
        });
    let source_json = serde_json::to_string(batch).ok();

    let report_id = db.insert_report(
        instance_id,
        severity,
        title.as_deref(),
        dedup_key.as_deref(),
        &body,
        source_json.as_deref(),
        session_id.as_deref(),
        working_dir,
        false,
    )?;
    tracing::info!(%instance_id, %report_id, %severity, "report created");
    Ok(true)
}

/// re-analyze a report's stored source items with the current prompt
pub async fn replay_report(
    report: &crate::db::Report,
    instance: &DaemonInstance,
    global: &GlobalConfig,
    db: &Database,
    model_override: Option<&str>,
) -> Result<i64> {
    let items: Vec<Value> = report.source_items_json.as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("parsing stored source items")?
        .unwrap_or_default();

    if items.is_empty() {
        anyhow::bail!("report {} has no stored source items", report.id);
    }

    let analyze = instance.config.analyze.as_ref()
        .ok_or_else(|| anyhow::anyhow!("cannot replay a worker daemon (no [analyze] config)"))?;

    let since = chrono::DateTime::from_timestamp(report.created_at, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default();

    let recent_reports = db.recent_report_summaries(&report.instance_id, 10).unwrap_or_default();
    let analyze_ctx = config::template::build_context_full(
        instance, &items, 0, &since, &recent_reports,
    );
    let rendered_prompt = config::render_template(&analyze.prompt, &analyze_ctx)
        .context("rendering analyze prompt")?;

    let model = model_override
        .or(analyze.model.as_deref())
        .unwrap_or(&global.default_model);
    let allowed_tools = analyze.allowed_tools.as_deref()
        .unwrap_or(&global.default_allowed_tools);

    let working_dir = instance.config.working_directory.as_deref()
        .map(|wd| {
            let ctx = config::template::build_context_with_since(instance, &[], 0, &since);
            config::render_template(wd, &ctx)
        })
        .transpose()
        .context("rendering working_directory template")?;

    eprintln!("replaying report {} with model {model}...", report.id);

    let response = invoke_claude(&rendered_prompt, model, allowed_tools, working_dir.as_deref()).await
        .context("claude invocation failed")?;

    let (severity, title, clean_analysis) = parse_response_header(&response.result);
    let source_json = serde_json::to_string(&items).ok();

    let report_id = db.insert_report(
        &report.instance_id,
        severity,
        title.as_deref(),
        None,
        &clean_analysis,
        source_json.as_deref(),
        response.session_id.as_deref(),
        working_dir.as_deref(),
        false,
    )?;

    Ok(report_id)
}

/// split filtered items into one batch per report
pub fn batch_items(per: Per, items: Vec<Value>) -> Vec<Vec<Value>> {
    if items.is_empty() {
        return vec![];
    }
    match per {
        Per::Run => vec![items],
        Per::Item => items.into_iter().map(|i| vec![i]).collect(),
    }
}

/// render `title` and `body` for a static daemon
pub fn render_static(config: &crate::config::DaemonConfig, ctx: &minijinja::Value) -> Result<(Option<String>, String)> {
    let title = config.title.as_deref()
        .map(|t| config::render_template(t, ctx))
        .transpose()
        .context("rendering title")?;
    let body = config::render_template(config.body.as_deref().unwrap_or_default(), ctx)
        .context("rendering body")?;
    Ok((title, body))
}

fn report_error(db: &Database, instance_id: &str, message: &str) -> Result<()> {
    if db.is_in_error(instance_id)? {
        return Ok(());
    }
    db.set_error(instance_id, true)?;
    db.insert_report(instance_id, 5, Some("daemon error"), None, message, None, None, None, true)?;
    Ok(())
}

/// parse the standardized header from the LLM response:
/// # Report: <title>
/// Severity: <N>
fn parse_response_header(response: &str) -> (i32, Option<String>, String) {
    let mut severity = 3;
    let mut title = None;
    let mut body_start = 0;
    let lines: Vec<&str> = response.lines().collect();

    for (i, line) in lines.iter().enumerate().take(5) {
        if let Some(rest) = line.strip_prefix("# Report:").or_else(|| line.strip_prefix("# Report :")) {
            title = Some(rest.trim().to_string());
        }
        if let Some(caps) = SEVERITY_RE.captures(line) {
            if let Ok(n) = caps[1].parse::<i32>() {
                severity = n;
            }
            // body starts after the severity line (skip blank line after if present)
            body_start = i + 1;
            if lines.get(body_start).is_some_and(|l| l.trim().is_empty()) {
                body_start += 1;
            }
            break;
        }
    }

    let clean = if body_start > 0 && body_start < lines.len() {
        lines[body_start..].join("\n")
    } else {
        response.to_string()
    };

    (severity, title, clean)
}

pub struct ClaudeResponse {
    pub result: String,
    pub session_id: Option<String>,
}

/// invoke `claude -p` with the prompt piped via stdin
async fn invoke_claude(
    prompt: &str,
    model: &str,
    allowed_tools: &str,
    working_dir: Option<&str>,
) -> Result<ClaudeResponse> {
    let mut cmd = Command::new("claude");
    // strip API key so claude always uses the team plan (interactive login),
    // never accidentally burns personal API credits
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.arg("-p")
        .arg("--model").arg(model)
        .arg("--output-format").arg("json")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if !allowed_tools.is_empty() {
        cmd.arg("--allowedTools").arg(allowed_tools);
    }

    if let Some(dir) = working_dir {
        cmd.current_dir(crate::config::resolve_path(dir));
    }

    let mut child = cmd.spawn().context("failed to spawn claude")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
    }

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude exited with {}: {stderr}", output.status);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: Value = serde_json::from_str(&stdout)
        .context("parsing claude JSON output")?;

    let result = json.get("result")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let session_id = json.get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(ClaudeResponse { result, session_id })
}

struct RunningGuard {
    db: Database,
    instance_id: String,
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        let _ = self.db.clear_running(&self.instance_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DaemonConfig, Per};
    use serde_json::json;

    #[test]
    fn batching_follows_per() {
        let items = vec![json!(1), json!(2), json!(3)];
        assert_eq!(batch_items(Per::Run, items.clone()), vec![items.clone()]);
        assert_eq!(
            batch_items(Per::Item, items),
            vec![vec![json!(1)], vec![json!(2)], vec![json!(3)]]
        );
        assert!(batch_items(Per::Run, vec![]).is_empty());
    }

    #[test]
    fn static_render_uses_title_and_body() {
        let config: DaemonConfig = toml::from_str(r#"
name = "d"
schedule = "every 1m"
title = "{{ items | length }} things"
body = "{% for i in items %}- {{ i }}\n{% endfor %}"
[source]
command = "echo"
"#).unwrap();
        let ctx = minijinja::Value::from_serialize(json!({"items": [1, 2]}));
        let (title, body) = render_static(&config, &ctx).unwrap();
        assert_eq!(title.as_deref(), Some("2 things"));
        assert_eq!(body, "- 1\n- 2\n");

        let mut untitled = config.clone();
        untitled.title = None;
        assert_eq!(render_static(&untitled, &ctx).unwrap().0, None);
    }

    #[test]
    fn response_header_parsing_still_works() {
        let _ = Per::Run;
        let (sev, title, body) = parse_response_header("# Report: hi\nSeverity: 4\n\nbody");
        assert_eq!((sev, title.as_deref(), body.as_str()), (4, Some("hi"), "body"));
    }
}
