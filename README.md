# army-of-daemons

A config-driven daemon system that monitors external processes, triages events with LLMs via `claude -p`, and presents results in a terminal UI.

## Quick start

```sh
cargo install --path .

# create config directory
mkdir -p ~/.daemon-army/daemons

# copy an example daemon
cp examples/pod-errors.toml ~/.daemon-army/daemons/

# validate your configs
army validate

# run a single daemon once to test
army check pod-errors

# start the runner (keeps running in foreground)
army run

# open the TUI (in another terminal)
army
```

## Commands

| Command | Description |
|---------|-------------|
| `army` | Open the TUI dashboard (default) |
| `army run` | Start the daemon runner in the foreground |
| `army check <name>` | Run a single daemon or instance once |
| `army status` | One-line summary for embedding in a status bar |
| `army validate` | Validate all daemon configs and templates |
| `army dry-run <name>` | Run source + filter, print items as JSON (no LLM call) |
| `army replay <id>` | Re-analyze a report's source data with the current prompt |
| `army resume <id>` | Resume the claude session from a report |
| `army export` | Export reports as JSON |

All commands accept `--config-dir <path>` to override the default `~/.daemon-army/`.

## How it works

The runner executes each daemon on a schedule. Every daemon runs a four-stage pipeline:

```
source -> filter -> analyze -> report
```

1. **Source**: run a shell command, parse output as structured data
2. **Filter**: apply field-based rules, deduplicate by key within a time window
3. **Analyze**: send items to `claude -p` for LLM analysis
4. **Report**: store the analysis in SQLite with a severity rating

The TUI reads from the same SQLite database. It can be opened and closed freely without affecting the runner.

## Config directory

```
~/.daemon-army/
├── config.toml          # global settings (optional)
├── daemons/             # one TOML file per daemon
│   ├── pod-errors.toml
│   └── ...
└── army.db              # SQLite database (auto-created)
```

## Global config

`~/.daemon-army/config.toml` — all fields are optional:

```toml
db_path = "~/.daemon-army/army.db"
log_path = "~/.daemon-army/runner.log"
prune_after_days = 30
display_after_days = 7
default_model = "sonnet"
default_allowed_tools = ""
```

`display_after_days` hides reports older than this many days from the TUI and status line. They stay in the database until `prune_after_days` elapses (export still sees them).

## Daemon spec

Each file in `~/.daemon-army/daemons/` defines one daemon. Here's the full schema with every field:

```toml
name = "my-daemon"
description = "what this daemon monitors"  # informational only
schedule = "every 60s"                      # or cron: "*/5 * * * *"
working_directory = "~/some/path"           # cwd for claude -p (optional)
icon = "GG"                                 # optional: own segment in `army status`
per = "run"                                 # "run" (one report per run) or "item" (one per item)
title = "{{ items | length }} things"       # optional: report title (overrides the LLM's title)
body = "..."                                # static daemons only: report body template

[source]
command = "some-command --flag {{params.key}}"  # shell command, rendered as a template
format = "jsonl"                            # "jsonl", "json", or "lines"
success_codes = [0]                         # exit codes that mean success (default: [0])
max_items = 200                             # truncate to this many items, keeping newest (default: 200)

[source.params]                             # optional: expand into multiple instances
key = ["value1", "value2"]

[filter]                                    # optional section
rules = [                                   # optional: field-based filter rules
  { field = "level", op = "gte", value = 50 }
]
dedup_key = "{{item.a}}:{{item.b}}"        # optional: template for deduplication key
dedup_window = "30m"                        # optional: suppress duplicates within this window

[analyze]                                    # optional: omit for worker daemons (no LLM)
model = "haiku"                             # optional: override global default_model
allowed_tools = "Bash(kubectl *)"           # optional: --allowedTools passed to claude
timeout = "5m"                              # optional: kill analysis after this duration
prompt = """
Your prompt here. Uses Jinja2 syntax (minijinja).
"""

[report]
notify = true                               # default: true

on_read = "some-command {{item.id}}"        # optional: runs when a report is marked read

[[actions]]                                 # optional: run with `a` + key in the TUI
key = "o"
name = "open"
when = "item.url"                           # optional: minijinja expression; hidden if falsy
command = "open {{item.url}}"
```

### Schedule syntax

- Interval: `every 30s`, `every 5m`, `every 1h`
- Cron: any standard cron expression like `*/5 * * * *`

### Source format

| Format | Parsing behavior |
|--------|-----------------|
| `jsonl` | Each line is parsed as JSON independently. Non-JSON lines are silently skipped. |
| `json` | Entire output is parsed as one JSON value. Arrays are expanded into items. |
| `lines` | Each non-empty line becomes a string item. |

### Template variables

Source commands and prompts are rendered with [minijinja](https://github.com/mitsuhiko/minijinja) (Jinja2 syntax). These variables are always available:

| Variable | Description |
|----------|-------------|
| `{{since}}` | Last successful check time (RFC 3339). On first run, defaults to 24 hours ago. |
| `{{timestamp}}` | Current time (RFC 3339) |
| `{{daemon_name}}` | The daemon's name field |
| `{{instance_id}}` | Full instance ID, e.g. `pod-errors::dashboard` |
| `{{run_number}}` | Monotonic counter per instance |

Keys from `[source.params]` are available under `params` (e.g. `{{params.repo}}`).

The analyze prompt gets additional variables:

| Variable | Description |
|----------|-------------|
| `{{items}}` | List of filtered items (use in `{% for %}` loops) |
| `{{items_json}}` | Same items serialized as a JSON string |
| `{{count}}` | Number of items |

Nested item fields are accessed with dot notation: `{{ item.author.login }}`.

`dedup_key` is rendered once per item, with that item available as `item` (e.g. `{{item.id}}`).

Use `{% raw %}...{% endraw %}` to protect sections that contain braces that aren't template syntax (e.g. jj `-T` templates).

### Filter rules

Each rule has a `field`, `op`, and optional `value`:

| Operator | Description |
|----------|-------------|
| `eq` | Equals (string or numeric) |
| `neq` | Not equals |
| `gt` | Greater than (numeric) |
| `gte` | Greater than or equal (numeric) |
| `lt` | Less than (numeric) |
| `lte` | Less than or equal (numeric) |
| `contains` | String contains substring |
| `matches` | Regex match |
| `exists` | Field is present (no value needed) |

Fields use dot notation for nested access: `errors.0.message` accesses the first element's `message` field.

Type coercion: if both values parse as numbers, comparison is numeric. Otherwise string.

### Dedup

Dedup prevents the same event from generating repeated reports. When `dedup_key` is set, each item's key is rendered from the template and checked against a `seen_keys` table in the database.

- All items that pass dedup are marked as seen immediately, before analysis runs.
- `dedup_window` controls how long a key is remembered. After the window elapses, the same key can trigger a new report.
- If a dedup key template renders to an empty string (e.g. typo in field name), the item is kept and a warning is logged.

Choose your dedup key based on what makes an event unique:
- Commit hash for git commits
- PR number for pull requests
- `operationName:message` for log errors
- Some combination of fields that identifies "the same thing happened again"

### Params and instances

When `[source.params]` is defined, the daemon is expanded into independent instances via cartesian product. For example:

```toml
[source.params]
repo = ["api", "web"]
env = ["staging", "prod"]
```

This produces four instances: `my-daemon::api,staging`, `my-daemon::api,prod`, `my-daemon::web,staging`, `my-daemon::web,prod`. Each instance has its own schedule, dedup state, and reports.

### Worker daemons

Omit the `[analyze]` section to create a worker daemon that just runs a command on schedule without LLM analysis. Useful for maintenance tasks like fetching repos or cleaning caches.

```toml
name = "jj-fetch"
description = "fetch latest commits"
schedule = "every 10m"
working_directory = "~/projects/{{params.repo}}"

[source]
command = "jj fetch"
format = "lines"

[source.params]
repo = ["api", "web", "infra"]
```

Worker daemons still show up in the TUI with their last-checked time, and source failures still produce error reports.

### Static daemons

Set a top-level `body` (and optionally `title`) instead of `[analyze]` to render reports from templates without an LLM. Static reports have no severity. `body` and `[analyze]` can't be combined.

### Actions and on_read

`[[actions]]` are shell commands you can run on a report from the TUI: press `a`, then the action's key. `on_read` runs whenever a report goes from unread to read; if it fails, the report is marked unread again and the error shows in the bottom bar.

Both are rendered with the report's context (`params`, `items`, and `item` for per-item daemons) and run in the background with no TTY, so wrap anything interactive in e.g. `zellij run -- ...`. Action keys must be single characters, unique within the daemon.

### Reports per run vs per item

By default each run produces one report covering all filtered items (`per = "run"`). With `per = "item"`, each item gets its own report, and templates can use `{{ item }}` in addition to `{{ items }}`.

### Prompt requirements

Every prompt **must** instruct the LLM to begin its response with this header:

```
# Report: <short title>
Severity: <N>
```

Where N is 1-5. The runner parses this header to extract the report title and severity. The header is stripped from the stored report — only the body is kept.

If the LLM doesn't produce a parseable severity, it defaults to 3.

### The `claude -p` invocation

The prompt is piped to claude via stdin (not as a CLI argument) to avoid shell escaping issues and argument length limits. `ANTHROPIC_API_KEY` is stripped from the child process environment so claude always uses interactive auth (e.g. a team plan), never personal API credits.

## TUI keybindings

| Key | Action |
|-----|--------|
| `j`/`k` or arrows | Navigate |
| `Enter` | Drill into daemon / open report |
| `Esc` / `q` | Back / quit |
| `r` | Mark report as read |
| `R` | Mark all read for this daemon |
| `y` | Copy report ID to clipboard |
| `a` | Run one of the report's actions (then press its key, `Esc` to cancel) |
| `Y` | Copy report content to clipboard |
| `x` | Manually trigger a daemon run |
| `/` | Search / filter |
| `?` | Help |

## Status bar integration

`army status` prints a single line like `🔴 3 | 🟡 2` showing unread counts by severity. If there are no unreads, it prints `✅`. Daemons with an `icon` get their own segment (`GG 4 | 🔴 3`), and static reports from daemons without one are counted under `■`. This is designed to be embedded in a terminal status bar (zellij, tmux, etc.).

## Runner details

- **Hot-reload**: the runner watches `~/.daemon-army/daemons/` for changes. Add, edit, or delete TOML files without restarting. Rename to `.toml.disabled` to temporarily disable a daemon.
- **Backpressure**: if a daemon instance is still running when its next scheduled tick fires, the tick is skipped.
- **Graceful shutdown**: Ctrl-C waits for in-flight analyses to complete, then exits.
- **Pruning**: reports older than `prune_after_days` are deleted daily.
- **Logging**: the runner logs to `log_path` (default `~/.daemon-army/runner.log`). Set `RUST_LOG=debug` for verbose output.
- **Heartbeat**: the runner writes a heartbeat to SQLite every 30 seconds. The TUI shows a warning if the heartbeat is stale.

## TOML quoting tips

TOML's `"""triple quotes"""` process escape sequences (`\"` becomes `"`). If your source command contains literal backslash-quotes (common with jj `-T` templates), you need to double-escape: `\\\"` in the TOML file produces `\"` in the rendered command.

If a section of your command has braces that aren't minijinja template syntax, wrap it in `{% raw %}...{% endraw %}`.

## Examples

See the `examples/` directory for working daemon configs:

- `pod-errors.toml` — monitor Kubernetes pod error logs
- `teammate-commits.toml` — summarize teammate commits via jj
- `new-prs.toml` — full code review of new pull requests with opus
- `ci-failures.toml` — monitor GitHub Actions failures
