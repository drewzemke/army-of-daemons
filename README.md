# army-of-daemons

A config-driven daemon system that monitors external processes, triages events with LLMs via `claude -p`, and presents results in a terminal UI.

Requires the [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI on your `PATH`.

## Quick start

```sh
cargo install --path .
mkdir -p ~/.daemon-army/daemons
# add a daemon spec to ~/.daemon-army/daemons/ (see below), then:
army validate           # check configs and templates
army check my-daemon    # run one daemon once
army run                # start the runner (foreground)
army                    # open the TUI (in another terminal)
```

## Commands

| Command | Description |
|---------|-------------|
| `army` / `army tui` | Open the TUI dashboard |
| `army run` | Start the daemon runner in the foreground |
| `army check <name>` | Run a single daemon or instance once |
| `army dry-run <name>` | Run source + filter, print items as JSON (no LLM call); `--show-prompt` to print the rendered prompt |
| `army validate` | Validate all daemon configs and templates |
| `army status` | One-line summary for a status bar |
| `army report <id>` | Print a report (`--json` for JSON) |
| `army replay <id>` | Re-analyze a report's source data with the current prompt |
| `army resume <id>` | Resume the claude session behind a report |
| `army export` | Export reports as JSON (`--daemon`, `--days`) |

All commands accept `--config-dir <path>` to override the default `~/.daemon-army/`.

## How it works

The runner executes each daemon on a schedule through a four-stage pipeline:

```
source -> filter -> analyze -> report
```

1. **Source**: run a shell command, parse output as structured data
2. **Filter**: apply field-based rules, deduplicate by key within a time window
3. **Analyze**: send items to `claude -p` for LLM analysis
4. **Report**: store the result in SQLite with a severity rating

The TUI reads from the same SQLite database and can be opened and closed freely without affecting the runner.

## Configuration

```
~/.daemon-army/
├── config.toml   # global settings (optional)
├── daemons/      # one TOML file per daemon
└── army.db       # SQLite database (auto-created)
```

`config.toml` fields, all optional (defaults shown):

```toml
db_path = "~/.daemon-army/army.db"
log_path = "~/.daemon-army/runner.log"
prune_after_days = 30      # delete reports older than this
display_after_days = 7     # hide older reports from the TUI and status line
default_model = "sonnet"
default_allowed_tools = ""
max_concurrent = 5
```

## Daemon spec

Each file in `~/.daemon-army/daemons/` defines one daemon. Every field:

```toml
name = "my-daemon"
description = "what this daemon monitors"  # informational only
schedule = "every 60s"                      # "every 30s|5m|1h" or a cron expression
working_directory = "~/some/path"           # optional: cwd for source and claude -p
icon = "GG"                                 # optional: own segment in `army status`
per = "run"                                 # "run" (one report per run) or "item" (one per item)
title = "{{ items | length }} things"       # optional: report title (overrides the LLM's)
body = "..."                                # static daemons only: report body template
on_read = "some-command {{item.id}}"        # optional: runs when a report is marked read

[source]
command = "some-command --flag {{params.key}}"
format = "jsonl"                            # "jsonl", "json", or "lines"
success_codes = [0]                         # default: [0]
max_items = 200                             # keep the newest N items (default: 200)

[source.params]                             # optional: expand into multiple instances
key = ["value1", "value2"]

[filter]                                    # optional
rules = [{ field = "level", op = "gte", value = 50 }]
dedup_key = "{{item.a}}:{{item.b}}"
dedup_window = "30m"

[analyze]                                   # optional: omit for worker daemons
model = "haiku"                             # overrides default_model
allowed_tools = "Bash(kubectl *)"           # passed to claude as --allowedTools
timeout = "5m"
prompt = """
Your prompt here (minijinja / Jinja2 syntax).
"""

[[actions]]                                 # optional: run with `a` + key in the TUI
key = "o"
name = "open"
when = "item.url"                           # optional: hidden if this expression is falsy
command = "open {{item.url}}"
```

### Source formats

- `jsonl`: each line parsed as JSON; non-JSON lines are skipped
- `json`: whole output parsed as one value; arrays expand into items
- `lines`: each non-empty line becomes a string item

### Templates

Commands, prompts, and other templates use [minijinja](https://github.com/mitsuhiko/minijinja). Always available:

| Variable | Description |
|----------|-------------|
| `since` | Last successful check time (RFC 3339); 24 hours ago on first run |
| `timestamp` | Current time (RFC 3339) |
| `daemon_name` | The daemon's `name` |
| `instance_id` | e.g. `pod-errors::dashboard` |
| `run_number` | Monotonic counter per instance |
| `params` | Values from `[source.params]`, e.g. `{{params.repo}}` |

After the source runs, templates also get `items` (filtered items), `items_json` (same, as a JSON string), and `count`. `dedup_key` is rendered per item with `item` available, as are all templates when `per = "item"`. Nested fields use dot notation: `{{ item.author.login }}`.

Wrap braces that aren't template syntax (e.g. jj `-T` templates) in `{% raw %}...{% endraw %}`. Note that TOML `"""` strings process escapes, so a literal `\"` in a command needs `\\\"`.

### Filter rules

Each rule has a `field` (dot notation, e.g. `errors.0.message`), an `op`, and usually a `value`. Ops: `eq`, `neq`, `gt`, `gte`, `lt`, `lte`, `contains`, `matches` (regex), `exists` (no value). Comparison is numeric when both sides parse as numbers, otherwise string.

### Dedup

When `dedup_key` is set, each item's key is checked against previously seen keys, and items that pass are marked seen immediately (before analysis). Keys expire after `dedup_window`. If a key renders empty (e.g. a typo'd field name), the item is kept and a warning is logged.

### Params and instances

`[source.params]` expands a daemon into independent instances via cartesian product. `repo = ["api", "web"]` and `env = ["staging", "prod"]` produce four instances (`my-daemon::api,staging`, ...), each with its own schedule, dedup state, and reports.

### Daemon kinds

- **Analyze** (has `[analyze]`): items go to `claude -p`, the response becomes the report.
- **Static** (has top-level `body`): reports are rendered from templates, no LLM, no severity. Can't be combined with `[analyze]`.
- **Worker** (neither): just runs the source command on schedule, e.g. `jj fetch` across repos. Shows up in the TUI with its last-checked time; source failures still produce error reports.

### Prompt requirements

Analyze prompts **must** tell the LLM to begin its response with:

```
# Report: <short title>
Severity: <N>
```

where N is 1-5. The header is parsed and stripped from the stored report. Unparseable severity defaults to 3.

The prompt is piped to claude via stdin. `ANTHROPIC_API_KEY` is stripped from the child environment so claude uses your interactive login rather than API credits.

### Actions and on_read

`[[actions]]` are shell commands you can run on a report from the TUI: press `a`, then the action's key. `on_read` runs whenever a report goes from unread to read; if it fails, the report is marked unread again and the error shows in the bottom bar.

Both get the report's template context and run in the background with no TTY, so wrap anything interactive in e.g. `zellij run -- ...`. Action keys must be single characters, unique within the daemon.

On analyze daemons, an action with `offer = true` only appears when the LLM offers it on a report. You write the command; the LLM decides whether it applies and fills in its `args`:

```toml
[[actions]]
key = "r"
name = "rerun failed jobs"
offer = true
args = ["run_id"]
command = "gh run rerun {{ args.run_id }} --failed --repo org/{{ params.repo }}"
```

army appends the offerable actions to the prompt, and the LLM ends its response with a fenced `actions` block of `{"action", "args", "reason"}` entries. Offers for unknown actions, with missing or extra args, or with values outside `[A-Za-z0-9._/@:+-]` are dropped. An action offered more than once gets digit keys (`1`, `2`, ...).

## TUI keybindings

Press `?` in the TUI for the full list.

| Key | Action |
|-----|--------|
| `j`/`k` or arrows | Navigate / scroll |
| `Enter` | Drill into daemon / open report |
| `Esc` / `q` | Back / quit |
| `r` / `u` | Mark read / unread (`r` on the dashboard marks all read) |
| `R` | Mark all read (daemon view) / resume claude session (report view) |
| `D` | Delete report |
| `a` | Run an action (then its key; `Esc` cancels) |
| `y` | Copy report content |
| `i` | Copy claude session ID |
| `s` | Toggle source data |
| `d` | Dry-run daemon |
| `x` | Run daemon now |
| `t` | Cycle time range |
| `/` | Search / filter |

## Status bar

`army status` prints a line like `🔴 3 | 🟡 2` of unread counts by severity, or `✅` when there are none. Daemons with an `icon` get their own segment (`GG 4 | 🔴 3`), and static reports from daemons without one count under `■`. Embed it in zellij, tmux, etc.

## Runner behavior

- **Hot-reload**: changes in `daemons/` apply without a restart. Rename a file to `.toml.disabled` to disable it.
- **Backpressure**: a tick is skipped if the instance is still running from the last one.
- **Graceful shutdown**: Ctrl-C waits for in-flight analyses to finish.
- **Pruning**: reports past `prune_after_days` are deleted daily.
- **Logging**: to `log_path`; set `RUST_LOG=debug` for more.
- **Heartbeat**: written every 30s; the TUI warns when it's stale.
