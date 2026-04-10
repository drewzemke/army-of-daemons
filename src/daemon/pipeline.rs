use anyhow::{Result, Context};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{self, DaemonInstance, GlobalConfig};
use crate::db::Database;

static SEVERITY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)severity:\s*([1-5])").unwrap());

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

    let working_dir = config.working_directory.as_deref();

    // 1. SOURCE
    tracing::info!(%instance_id, cmd=%rendered_cmd, "executing source");
    let items = match crate::daemon::source::execute(
        &rendered_cmd,
        &config.source.format,
        &config.source.success_codes,
        config.source.max_items,
        working_dir,
    ).await {
        Ok(items) => items,
        Err(e) => {
            tracing::error!(%instance_id, err=%e, "source failed");
            report_error(db, instance_id, &format!("source command failed: {e}"))?;
            return Ok(());
        }
    };

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
                let merged_ctx = merge_item_into_context(instance, &item, run_count as u64, &since);
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

    // 3. ANALYZE
    let analyze_ctx = config::template::build_context_with_since(
        instance, &filtered, run_count as u64, &since,
    );
    let rendered_prompt = config::render_template(&config.analyze.prompt, &analyze_ctx)
        .context("rendering analyze prompt")?;

    let model = config.analyze.model.as_deref().unwrap_or(&global.default_model);
    let allowed_tools = config.analyze.allowed_tools.as_deref()
        .unwrap_or(&global.default_allowed_tools);

    tracing::info!(%instance_id, prompt_bytes=rendered_prompt.len(), %model, "invoking claude");

    let analysis = match invoke_claude(&rendered_prompt, model, allowed_tools, working_dir).await {
        Ok(text) => text,
        Err(e) => {
            tracing::error!(%instance_id, err=%e, "claude invocation failed");
            report_error(db, instance_id, &format!("claude analysis failed: {e}"))?;
            return Ok(());
        }
    };

    // 4. REPORT — parse severity and title from response header
    let (severity, title, clean_analysis) = parse_response_header(&analysis);
    let dedup_key = config.filter.as_ref()
        .and_then(|f| f.dedup_key.as_ref())
        .and_then(|tmpl| {
            // use first item's fields for the dedup key
            let item_ctx = merge_item_into_context(instance, &filtered[0], run_count as u64, &since);
            config::render_template(tmpl, &item_ctx).ok()
        });
    let source_json = serde_json::to_string(&filtered).ok();

    let report_id = db.insert_report(
        instance_id,
        severity,
        title.as_deref(),
        dedup_key.as_deref(),
        &clean_analysis,
        source_json.as_deref(),
        false,
    )?;

    db.update_instance_run(instance_id, true)?;
    tracing::info!(%instance_id, %report_id, %severity, "report created");

    Ok(())
}

fn report_error(db: &Database, instance_id: &str, message: &str) -> Result<()> {
    db.insert_report(instance_id, 3, Some("daemon error"), None, message, None, true)?;
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

/// invoke `claude -p` with the prompt piped via stdin
async fn invoke_claude(
    prompt: &str,
    model: &str,
    allowed_tools: &str,
    working_dir: Option<&str>,
) -> Result<String> {
    let mut cmd = Command::new("claude");
    // strip API key so claude always uses the team plan (interactive login),
    // never accidentally burns personal API credits
    cmd.env_remove("ANTHROPIC_API_KEY");
    cmd.arg("-p")
        .arg("--model").arg(model)
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

    // write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        // drop stdin to signal EOF
    }

    let output = child.wait_with_output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude exited with {}: {stderr}", output.status);
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// build a template context that has both the instance variables and the item's
/// fields available at the top level (for dedup_key rendering)
fn merge_item_into_context(instance: &DaemonInstance, item: &Value, run_count: u64, since: &str) -> minijinja::Value {
    use std::collections::HashMap;

    let mut map: HashMap<String, minijinja::Value> = HashMap::new();

    // magic variables
    map.insert("since".into(), minijinja::Value::from(since.to_string()));
    map.insert("daemon_name".into(), minijinja::Value::from(instance.daemon_name.clone()));
    map.insert("instance_id".into(), minijinja::Value::from(instance.instance_id.clone()));
    map.insert("run_number".into(), minijinja::Value::from(run_count));

    // params
    for (k, v) in &instance.params {
        map.insert(k.clone(), minijinja::Value::from(v.clone()));
    }

    // overlay item fields at top level
    if let Value::Object(obj) = item {
        for (k, v) in obj {
            map.insert(k.clone(), minijinja::Value::from_serialize(v));
        }
    }

    minijinja::Value::from_serialize(&map)
}
