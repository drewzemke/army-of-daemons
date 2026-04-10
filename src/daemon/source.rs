use anyhow::{Result, bail};
use serde_json::Value;
use tokio::process::Command;

/// run a source command and parse its output into structured items
pub async fn execute(
    command: &str,
    format: &str,
    success_codes: &[i32],
    max_items: usize,
    working_dir: Option<&str>,
) -> Result<Vec<Value>> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);

    if let Some(dir) = working_dir {
        let path = crate::config::resolve_path(dir);
        cmd.current_dir(path);
    }

    let output = cmd.output().await?;
    let code = output.status.code().unwrap_or(-1);

    if !success_codes.contains(&code) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command exited with code {code}: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let items = parse_output(&stdout, format)?;

    if items.len() > max_items {
        Ok(items.into_iter().take(max_items).collect())
    } else {
        Ok(items)
    }
}

fn parse_output(output: &str, format: &str) -> Result<Vec<Value>> {
    match format {
        "jsonl" => {
            let mut items = Vec::new();
            for line in output.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(v) => items.push(v),
                    Err(_) => continue, // skip non-json lines
                }
            }
            Ok(items)
        }
        "json" => {
            if output.trim().is_empty() {
                return Ok(vec![]);
            }
            let val: Value = serde_json::from_str(output.trim())?;
            match val {
                Value::Array(arr) => Ok(arr),
                other => Ok(vec![other]),
            }
        }
        "lines" => {
            let items: Vec<Value> = output
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| Value::String(l.to_string()))
                .collect();
            Ok(items)
        }
        _ => bail!("unknown source format: {format}"),
    }
}
