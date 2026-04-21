# army-of-daemons

A generic, config-driven daemon system that monitors external processes, triages events with LLMs, and presents results in a terminal UI.

## overview

The system has two halves:

1. **Runner** — a long-lived background process that executes daemon configs on a schedule, invokes `claude -p` for analysis, and writes results to SQLite.
2. **TUI** — a stateless ratatui frontend that reads from the same SQLite database. Can be opened/closed freely without affecting the runner.

SQLite is the shared contract between them. The runner writes, the TUI reads (plus marks reports as read and copies to clipboard).

The database is opened in WAL mode (`PRAGMA journal_mode=WAL`) with a 5-second busy timeout (`PRAGMA busy_timeout=5000`). This allows concurrent reads from the TUI/status command while the runner writes. Each process opens its own connection.

## user-facing commands

```
army-of-daemons run           # start the runner (foreground, user manages lifecycle)
army-of-daemons tui           # open the TUI
army-of-daemons status        # one-line summary for zellij status bar, e.g. "🔴 3 | 🟡 2"
army-of-daemons check <name>  # manually run a single daemon once (useful for testing configs)
army-of-daemons export        # export reports as JSON (all, or filtered by daemon/date)
army-of-daemons validate      # validate all daemon configs and templates
```

## config location

All user config lives in `~/.daemon-army/`:

```
~/.daemon-army/
├── config.toml            # global settings
├── daemons/               # one TOML file per daemon
│   ├── pod-errors.toml
│   ├── ci-failures.toml
│   └── teammate-commits.toml
└── army.db                # SQLite database (auto-created)
```

### global config (`config.toml`)

```toml
# all fields optional, these are defaults
db_path = "~/.daemon-army/army.db"
log_path = "~/.daemon-army/runner.log"
prune_after_days = 30
display_after_days = 7  # hide reports older than this in the TUI / status line
default_model = "sonnet"

# default allowed tools for claude -p (can be overridden per daemon)
default_allowed_tools = ""
```

### daemon config (e.g. `daemons/pod-errors.toml`)

```toml
name = "pod-errors"
description = "triage production pod errors"
schedule = "every 60s"                # or cron: "*/5 * * * *"
working_directory = "~/RSS/pod-reports"  # cwd for claude -p invocation

[source]
command = "kubectl logs deployment/{{name}} --context aws-prod -n apps --since-time {{since}}"
format = "jsonl"   # "jsonl" | "json" | "lines"
success_codes = [0]  # exit codes that mean "command succeeded" (default: [0])
max_items = 200      # truncate source output to this many items (default: 200)

[source.params]
name = ["assessment-server", "workflow-v2-server", "folder-server"]

[filter]
rules = [
  { field = "level", op = "gte", value = 50 }
]
dedup_key = "{{operationName}}:{{errors.0.message}}"
dedup_window = "30m"

[analyze]
model = "haiku"   # override global default
allowed_tools = "Bash(kubectl *)"
timeout = "5m"    # kill analysis if it takes longer than this
prompt = """
Investigate these errors from the {{name}} deployment (aws-prod/apps):

{% for item in items %}
- {{ item.operationName | default(value="unknown") }}: {{ item.msg | default(value="") }}
{% endfor %}

Raw log entries:
{{ items_json }}

What's going on and what should I do about it?

Begin your response with a markdown header in exactly this format:
# Report: <short title>
Severity: <N>

Where N is 1-5: 1=noise, 2=low, 3=medium/look at today, 4=high/soon, 5=critical/drop everything.
Then write your analysis below.
"""

[report]
notify = true
```

## core abstractions

### daemon pipeline

Every daemon runs this pipeline:

```
source → filter → analyze → report
```

1. **Source**: execute a shell command, parse its output into structured items. If the item count exceeds `max_items`, truncate and note the truncation in the prompt context.
2. **Filter**: apply rules to each item, dedup by key within a configurable time window.
3. **Analyze**: render the prompt template with items, invoke `claude -p` once. The prompt instructs the LLM to include a standardized markdown header with a severity rating (see "severity extraction" below).
4. **Report**: parse severity from the response header, write the analysis to SQLite, flag as unread.

If any stage fails (command exits with a code not in `success_codes`, claude unreachable, parse error), the failure itself becomes the report — the error message is stored as the analysis, severity defaults to 3 (medium), and the report is flagged with `is_error = 1` so the user sees it in the TUI.

**Backpressure**: if a daemon instance's previous run is still in progress when the next scheduled run triggers, the scheduled run is skipped and a warning is logged. Runs do not pile up.

### params expansion

When `[source.params]` is defined, the daemon config is expanded into independent instances. For `name = ["a", "b", "c"]`, three instances are created:

- `pod-errors::a`
- `pod-errors::b`
- `pod-errors::c`

Each instance has its own:
- `{{since}}` (last successful check time)
- dedup state
- reports

Multiple param keys produce a cartesian product: `name = ["a", "b"]` + `env = ["staging", "prod"]` = 4 instances.

### magic template variables

These are injected by the runtime, not defined in config:

| Variable | Value |
|----------|-------|
| `{{since}}` | ISO 8601 timestamp of last successful check for this instance |
| `{{timestamp}}` | current time (ISO 8601) |
| `{{daemon_name}}` | the daemon's name |
| `{{instance_id}}` | full instance id, e.g. `pod-errors::assessment-server` |
| `{{run_number}}` | monotonic counter per instance |

All `[source.params]` keys are also available as template variables.

### template engine

Uses `minijinja` for template rendering. Supports:
- Variable substitution: `{{ var }}`
- Loops: `{% for item in items %} ... {% endfor %}`
- Conditionals: `{% if condition %} ... {% endif %}`
- Filters: `{{ var | default(value="fallback") }}`

The analyze prompt receives these additional variables:
- `items` — list of filtered items (as structured data)
- `items_json` — the same items serialized as a JSON string
- `count` — number of items

### scheduling

Each daemon config specifies a schedule:
- Interval syntax: `every 30s`, `every 5m`, `every 1h`
- Cron syntax: `*/5 * * * *`

The runner uses tokio tasks with timers. Each daemon instance gets its own task.

### state tracking

Persisted state per instance in SQLite:
- `last_checked` — timestamp of last successful source fetch
- `last_run` — timestamp of last run attempt (success or failure)
- `run_count` — monotonic counter

The `{{since}}` variable defaults to 5 minutes before first run if no prior state exists.

### dedup

Dedup operates per-instance. When a new batch of items arrives:
1. Compute the dedup key for each item using the template.
2. If the rendered key is empty or contains only separator characters (`:`, `/`, etc.), log a warning and treat the item as non-deduplicatable (always create a new report). This catches template typos that reference nonexistent fields.
3. Check SQLite for any report with the same dedup key within the `dedup_window`.
4. Drop items that match.

This means the same error won't generate a new report unless the window has elapsed.

### filter rules

Filter rules use dot-notation for nested field access (e.g., `errors.0.message` for array index 0). Supported operators:

| Operator | Description |
|----------|-------------|
| `eq` | equals (string or numeric) |
| `neq` | not equals |
| `gt` | greater than (numeric) |
| `gte` | greater than or equal (numeric) |
| `lt` | less than (numeric) |
| `lte` | less than or equal (numeric) |
| `contains` | string contains substring |
| `matches` | regex match |
| `exists` | field is present (value ignored) |

Type coercion: if both the field value and rule value are parseable as numbers, compare numerically. Otherwise compare as strings. If the field path doesn't resolve, the item is excluded (fails the filter) unless the operator is `exists` (which returns false).

## SQLite schema

All timestamps are stored as **Unix epoch integers** (seconds). This makes time-range queries trivial and eliminates timezone ambiguity. Conversion to human-readable format happens in the TUI layer.

```sql
PRAGMA journal_mode=WAL;
PRAGMA busy_timeout=5000;

-- daemon instance state
CREATE TABLE instances (
    id TEXT PRIMARY KEY,              -- e.g. "pod-errors::assessment-server"
    daemon_name TEXT NOT NULL,
    params_json TEXT,                 -- JSON of param values for this instance
    last_checked INTEGER,             -- unix epoch seconds
    last_run INTEGER,
    run_count INTEGER DEFAULT 0
);

-- reports generated by analysis
CREATE TABLE reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    instance_id TEXT NOT NULL REFERENCES instances(id),
    created_at INTEGER NOT NULL,      -- unix epoch seconds
    severity INTEGER NOT NULL,        -- 1-5
    title TEXT,                       -- short title from LLM response header
    dedup_key TEXT,
    analysis TEXT NOT NULL,           -- markdown content from claude
    source_items_json TEXT,           -- raw items that triggered this report
    is_read INTEGER DEFAULT 0,
    is_error INTEGER DEFAULT 0,       -- 1 if this report is a daemon failure
    FOREIGN KEY (instance_id) REFERENCES instances(id)
);

-- runner heartbeat and metadata
CREATE TABLE runner_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- index for dedup lookups
CREATE INDEX idx_reports_dedup ON reports(instance_id, dedup_key, created_at);

-- index for TUI queries
CREATE INDEX idx_reports_unread ON reports(is_read, severity, created_at);
```

## TUI

Built with `ratatui`. Reads from SQLite, writes only `is_read` updates.

The TUI checks the `runner_state` heartbeat on startup and periodically. If the runner hasn't heartbeated in >90 seconds, a warning banner is shown at the top of every view.

**Manual trigger** (`x` keybind): the TUI spawns `army-of-daemons check <instance_id>` as a subprocess. This works whether or not the runner is active — it's a standalone one-shot execution. If the runner is also running, the backpressure mechanism prevents duplicate concurrent runs.

### views

**Dashboard** (default view)
```
┌─────────────────────────────────────────────────┐
│ army-of-daemons                          ? help │
├─────────────────────────────────────────────────┤
│ daemon              status   unread  last check │
│ ─────────────────────────────────────────────── │
│ pod-errors::assess… 🔴 crit     3    2m ago    │
│ pod-errors::folder… 🟢 ok       0    1m ago    │
│ ci-failures         🟡 warn     1    5m ago    │
│ teammate-commits    🔵 info     2    30s ago   │
│                                                 │
├─────────────────────────────────────────────────┤
│ j/k navigate  Enter drill in  x run now  q quit│
└─────────────────────────────────────────────────┘
```

**Detail view** (reports for one daemon instance)
```
┌─────────────────────────────────────────────────┐
│ pod-errors::assessment-server        Esc go back│
├─────────────────────────────────────────────────┤
│ severity  time          dedup key    read       │
│ ─────────────────────────────────────────────── │
│ 🔴 5     10:32 today   auth:timeout  ○         │
│ 🟡 3     09:15 today   db:conn_pool  ●         │
│ 🔵 2     yesterday     parse:null    ●         │
│                                                 │
├─────────────────────────────────────────────────┤
│ Enter view  r mark read  R read all  y copy path│
└─────────────────────────────────────────────────┘
```

**Report view** (single report, rendered markdown)
```
┌─────────────────────────────────────────────────┐
│ pod-errors::assessment-server  2024-01-15 10:32 │
│ severity: 🔴 critical (5)           Esc go back│
├─────────────────────────────────────────────────┤
│                                                 │
│  The assessment-server is experiencing auth     │
│  token timeout errors when communicating with   │
│  Cognito. The connection pool appears to be...  │
│                                                 │
│  ## Suggested actions                           │
│  - Check Cognito service health                 │
│  - Review connection pool settings in...        │
│                                                 │
├─────────────────────────────────────────────────┤
│ y copy path  Y copy content  r mark read        │
└─────────────────────────────────────────────────┘
```

### keybindings

| Key | Action |
|-----|--------|
| `j` / `k` / `↑` / `↓` | navigate list |
| `Enter` | drill into daemon / open report |
| `Esc` / `q` | go back / quit (from dashboard) |
| `r` | mark report as read |
| `R` | mark all reports read for this daemon |
| `y` | copy report path to clipboard |
| `Y` | copy report content to clipboard |
| `x` | manually trigger daemon run |
| `/` | filter / search |
| `?` | help overlay |

### severity display

| Level | Label | Color |
|-------|-------|-------|
| 1 | noise | dim gray |
| 2 | low | blue |
| 3 | medium | yellow |
| 4 | high | orange (rgb) |
| 5 | critical | red |

Dashboard status shows the highest severity of unread reports for each daemon instance.

### status command

`army-of-daemons status` outputs a single line for zellij status bar integration:

```
🔴 3 | 🟡 2
```

Format: highest-severity emoji + count of unread at that level, for each level that has unreads, descending. If no unreads, outputs `✅`.

## `claude -p` invocation

The analyze step shells out to `claude -p` directly. No SDK, no crate — just `tokio::process::Command`.

The prompt is piped via **stdin**, not passed as a CLI argument. This avoids shell escaping issues and `ARG_MAX` limits (source output may contain quotes, backticks, dollar signs, etc.).

```rust
let child = Command::new("claude")
    .arg("-p")
    .arg("--model").arg(&model)
    .arg("--allowedTools").arg(&allowed_tools)
    .current_dir(&working_directory)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()?;

child.stdin.write_all(rendered_prompt.as_bytes()).await?;
```

The `working_directory` from the daemon config is set as the cwd of the child process, so claude sees the right project context.

**Single invocation** per analysis. The prompt template instructs the LLM to begin its response with a standardized markdown header:

```markdown
# Report: <short title>
Severity: <N>

<analysis body>
```

### severity extraction

The runner parses the `Severity: N` line from the response header:
1. Look for a line matching `Severity: \d` (case-insensitive) in the first 5 lines of the response.
2. Extract the first digit 1-5 found on that line.
3. If no valid severity is found, default to 3 and log a warning.

The `Severity:` line is stripped from the stored report — only the title and analysis body are kept in the database. The severity integer is stored separately.

### process management

Each `claude -p` child process PID is tracked. On hot-reload cancellation or shutdown, the child process is killed (SIGTERM, then SIGKILL after 5s grace period). The `analyze.timeout` config (default: no timeout) kills the process if it runs too long, and the timeout is reported as a daemon error.

### startup check

On runner startup, verify that `claude` is on PATH and responsive (`claude --version`). If not, exit with a clear error message rather than failing opaquely on first daemon run.

## hot-reloading

The runner watches `~/.daemon-army/daemons/` with `notify` (file watcher crate). On change:
- New `.toml` file → parse and start new daemon tasks
- Modified `.toml` → cancel existing tasks for that daemon, wait for in-flight `claude -p` to finish (up to 30s grace period, then kill), re-parse, start new tasks
- Deleted `.toml` → cancel tasks, kill in-flight processes (state/reports in DB are kept, just no new runs)
- Renamed to `.toml.disabled` → same as delete (convention for temporarily disabling a daemon)

No need to restart the runner when editing configs.

## runner lifecycle

### heartbeat

The runner writes a heartbeat to a `runner_state` table every 30 seconds:

```sql
CREATE TABLE runner_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- key: "heartbeat", value: unix epoch timestamp
-- key: "pid", value: runner process ID
-- key: "started_at", value: unix epoch timestamp
```

The TUI checks the heartbeat and shows a warning banner ("runner not detected — last heartbeat Nm ago") when it's stale (>90 seconds old).

### graceful shutdown

The runner handles SIGINT and SIGTERM via `tokio::signal`. On shutdown:
1. Set a shutdown flag — no new daemon runs are started.
2. Wait for in-flight `claude -p` processes to complete (up to 30s).
3. Kill any remaining child processes (SIGTERM, then SIGKILL after 5s).
4. Flush final state to SQLite.
5. Exit cleanly.

### logging

The runner logs to `log_path` (default `~/.daemon-army/runner.log`) using `tracing` with `tracing-subscriber`. Log levels configurable via `RUST_LOG` env var.

Every pipeline stage is logged:
- Source execution: command, exit code, item count, duration
- Filter: items before/after filtering, dedup hits
- Analysis: prompt size (bytes), response time, parsed severity
- Report: report ID, instance, severity
- Skips: "instance X still running, skipping scheduled run"
- Errors: full error context for any stage failure

## pruning

A background task runs daily (or on startup) and deletes reports older than `prune_after_days` (default 30). Pruned reports are hard-deleted from SQLite. Instance state is kept. Use `army-of-daemons export` before pruning if you want to keep historical data.

## crate dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | async runtime, timers, process spawning |
| `ratatui` + `crossterm` | TUI |
| `rusqlite` | SQLite (with bundled feature) |
| `serde` + `toml` | config parsing |
| `minijinja` | template rendering |
| `notify` | filesystem watching for hot-reload |
| `arboard` | cross-platform clipboard |
| `clap` | CLI argument parsing |
| `chrono` | timestamp handling |
| `serde_json` | JSON parsing for source output |
| `cron` | cron expression parsing (if we support cron syntax) |
| `dirs` | resolve `~` in paths |
| `tracing` + `tracing-subscriber` | structured logging for the runner |

## credentials and environment

Source commands inherit the runner's full environment. This means `kubectl` uses the user's kubeconfig, `gh` uses the user's GitHub token, etc. No special credential management is needed — the runner runs as the user.

Daemon config files in `~/.daemon-army/daemons/` may reference environment variables in commands (e.g., `$GITHUB_TOKEN`). These are expanded by the shell when the command executes, not by the template engine.

**Security note**: daemon configs should not be committed to version control if they contain sensitive values. The `~/.daemon-army/` location is outside any repo by design.

## build order

Phase 1 — foundation:
1. Project scaffolding, Cargo.toml, clap CLI skeleton
2. Config parsing (global + daemon TOML files)
3. SQLite schema + state module (WAL mode, heartbeat table)
4. Template engine integration (minijinja + magic variables)

Phase 2 — runner:
5. Source execution (shell command → parsed items, success_codes, max_items)
6. Filter + dedup logic (with dot-notation field access, operator set, dedup_window)
7. `claude -p` invocation via stdin (analyze + severity extraction from header)
8. Report writing to SQLite
9. Scheduler (interval + cron, tokio tasks, backpressure/skip-if-running)
10. Hot-reload (notify file watcher, graceful task cancellation with PID tracking)
11. Graceful shutdown (signal handling, child process cleanup)
12. Heartbeat writer
13. Pruning task
14. Startup checks (claude on PATH, config validation, instance cap)

Phase 3 — TUI:
15. Dashboard view (with runner health indicator)
16. Detail view (reports list)
17. Report view (rendered markdown)
18. Keybindings + clipboard (arboard)
19. Manual trigger (`x` → shells out to `check`)
20. Status command (zellij one-liner)

Phase 4 — polish:
21. `export` command (JSON output, filterable by daemon/date range)
22. `validate` command (check all configs, templates, instance cap)
23. Example daemon configs for the three initial use cases
24. Error handling hardening
25. Testing
