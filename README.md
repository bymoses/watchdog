# bms-watchdog

Rust/Tokio local battery and process monitor. The runtime is a single host binary; Docker is only used to run Cargo/Rust tooling during development.

![Dashboard screenshot](docs/assets/dashboard.png)

## Build/check

```bash
./scripts/cargo-docker.sh check
./scripts/cargo-docker.sh test
./scripts/cargo-docker.sh build --release
```

## Run

```bash
./target/release/bms-watchdog start
# UI: http://127.0.0.1:24923
```

Useful options:

```bash
./target/release/bms-watchdog start --no-serve
./target/release/bms-watchdog start --once --force-collect
./target/release/bms-watchdog start --port 24923 --data-dir ~/.local/share/bms-watchdog
./target/release/bms-watchdog start --redact-browser-titles
./target/release/bms-watchdog start --pi-sessions-dir ~/.pi/agent/sessions
./target/release/bms-watchdog start --agent-retention-days 90
./target/release/bms-watchdog start --context-retention-days 90 --context-path-mode basename
./target/release/bms-watchdog start --workspace-root /data-shared/workspace --tmux-socket /run/user/1000/tmux.sock
./target/release/bms-watchdog start --browser-event-retention-days 90 --browser-title-retention-days 0
./target/release/bms-watchdog start --no-context-tracking --no-terminal-context --no-browser-integration
./target/release/bms-watchdog start --no-agent-tracking
```

Private/incognito browser focused-window titles are redacted by default. USB-C/typec power-role and USB power-supply context is recorded when the kernel exposes it.

Retention defaults are tuned for a continuous trace: battery/environment/app aggregate history, focus-context/activity intervals, and normalized coding-agent telemetry are kept for 365 days, while detailed per-process snapshots are kept for 2 days. Context retention is independent of raw environment retention, and nullable environment anchors do not keep old environment samples alive. `--agent-retention-days 0` keeps agent telemetry indefinitely without changing source session files.

## Battery charge type

The dashboard rail exposes a Fast / Standard / Long_Life toggle backed by `GET` and `POST /api/battery/charge-type`. This is the only place watchdog actuates hardware instead of observing it.

The write set is read from the kernel's own `charge_types` enum, never hardcoded, so the control adapts to whatever the host advertises and hides itself entirely when no battery exposes the attribute. Writes are validated against that enum, applied with the kernel's own spelling, and then read back — sysfs accepts a write and silently ignores it when the EC refuses, so read-back is the only trustworthy confirmation.

`charge_types` is root-owned `0644` by default, so the toggle renders read-only with a remediation hint until write access is granted. Granting it is a deliberate, separate step that keeps the daemon unprivileged: a udev rule scoped to that single attribute (see `devices/<device-id>/battery-charge-types-install.sh` in the sibling `devices` repo). Running watchdog as root is not required and not recommended.

`POST` requires the same-origin CSRF header as every other mutating endpoint.

## Coding-agent usage

The dashboard's **Agent usage** panel reads an agent-neutral normalized SQLite store and API (`GET /api/agents/usage`). Pi is the first adapter; another agent only needs an adapter that emits normalized sessions and usage events, not a new table, endpoint, or chart.

Pi session roots resolve in this order:

1. repeated `--pi-sessions-dir DIR` flags;
2. `PI_CODING_AGENT_SESSION_DIR`;
3. `$PI_CODING_AGENT_DIR/sessions`;
4. `~/.pi/agent/sessions`.

The background indexer scans at startup and every 30 seconds on its own SQLite connection. It streams JSONL, resumes appends from checkpoints, retries incomplete trailing records, rebuilds rewritten files, and deduplicates history copied into forks/clones. A canonical response is counted once and attributed to its originating session; every containing session retains a reference. Pi catalog costs are shown as estimated USD costs. Pi may not persist internal compaction and branch-summary model usage, so those calls may be absent.

The normalized database stores token/cost counters, model/provider metadata, timestamps, explicit session names, and project paths. It never stores prompts, assistant/thinking content, tool arguments/results, commands, model-content errors, or native response IDs. Binding the server beyond localhost exposes project/session metadata through the API and should only be done on a trusted network.

## Focused-window collection

Normalized, privacy-filtered app/title/PID facts are stored as change-driven `focus_context_intervals`. An unchanged poll extends one open interval in place; app/title/PID changes, monitoring or suspend gaps, and local-midnight boundaries close it and start another. The compact context key is a SHA-256 digest of normalized facts and does not persist extra source content. Computed labels and reconciled activity rows are interval-level, so unchanged 30-second confirmations do not add label or activity rows.

Label rules are data in SQLite rather than project-name conditionals. User rules precede built-in and heuristic origins, `activity.primary` is exclusive, and other dimensions may have several values. Rule outputs can create typed dynamic entities. Service/video labels do not imply `activity.primary=entertainment` unless a rule explicitly emits it. A collision-safe monotonic rule revision enqueues bounded background relabel jobs with persisted cursors and progress. Historical environment rows are compacted into low-confidence title/app runs. Adjacent equivalent activities merge while retaining source mappings for deterministic relabel splits.

`GET /api/context/latest` returns the latest normalized interval and its computed labels. The same object is included in `GET /api/status`. Activity summary/timeline/episodes, unknown review, entities, rules/manual labels, relabel progress, policies, interventions, export/deletion, source health, and storage metrics are available through the `/api/activity/*`, `/api/context/*`, and `/api/attention/*` endpoints and dashboard.

### Terminal and agent presence

Focused Ghostty windows are resolved through bounded argument-array tmux calls. Exact one-client matches record session/window/pane, executable-only tool facts, and dynamic Git common-directory or workspace-root repository/project entities. Ambiguous clients remain unresolved. Paths default to basename plus a stable hash; use `--context-path-mode full|basename|hash`. No pane title, scrollback, terminal contents, shell arguments, history, or commands are retained.

The optional Pi package in `integrations/pi-presence/` emits secure same-user runtime leases containing only exact current native session ID, PID/start identity, CWD, and idle/busy state. Install, inspect, or remove it with:

```bash
./target/release/bms-watchdog install pi-presence --action install
./target/release/bms-watchdog install pi-presence --action status
./target/release/bms-watchdog install pi-presence --action uninstall
```

Lease files require a `0700` directory and `0600` regular files, are symlink/owner/PID/TTL checked, and create transition history only for state/session changes. Focus attribution uses the current fork/resume session, not canonical usage origin. `pi` and `claude` executable/CWD correlation remains a lower-confidence fallback.

### Browser integration and attention policies

`browser-extension/` builds Firefox/Zen and Chromium WebExtensions. The default is domains-only with private windows excluded. Transition events are sent immediately; unchanged heartbeats update one current-state lease and add no historical row. Generic HTML5, YouTube SPA, Twitch, and Kick media adapters keep focus, visible, media, audible, verified-focused, and background playback metrics distinct.

Pairing is loopback-only with a five-minute one-time code, exact extension-origin CORS, bearer tokens returned once and stored hashed, bounded batches/timestamps, replay dedupe, and no cookies. Full URLs, queries/fragments, page/form text, cookies, titles by default, and media identities are rejected. Observe/warn/soft-block and opt-in hard-block policies support calendar-day, rolling, and session windows, snooze/override, offline fail-open caching, and intervention auditing.

An explicit `--focused-window-file` remains the first source. Otherwise, the collector queries Niri using the process's valid `NIRI_SOCKET`; if the service started before Niri imported that variable into the user manager, it deterministically discovers current-user `niri.*.sock` sockets under `XDG_RUNTIME_DIR` (or `/run/user/<current uid>`). A failed/stale socket falls through to the remaining candidates, and `NIRI_SOCKET` is set only on the `niri msg` subprocess.

The generated service intentionally remains attached to `default.target`: graphical ordering would delay or disable headless battery monitoring and would require re-enabling existing units to change install targets. Socket discovery removes the startup race without coupling the monitor lifecycle to a graphical session. If focus remains empty, verify that `niri msg -j focused-window` works as the same user and that the Niri socket is in the user's runtime directory.

## Install user service

```bash
./target/release/bms-watchdog install systemctl-unit --port 24923
```

For tests or previewing the unit without touching systemd:

```bash
./target/release/bms-watchdog install systemctl-unit --dry-run --unit-dir /tmp/systemd-user
```

## Migrations

Default SQLite migrations live in `migrations/` and are embedded into the binary. They are applied automatically on startup and recorded in `schema_migrations`.

To migrate an old database into a new data dir:

```bash
./target/release/bms-watchdog migrate old-db \
  --source /home/user/workspace/bms/devices/battery-monitor/data/battery-monitor.sqlite \
  --data-dir ~/.local/share/bms-watchdog
```

Use `--replace` only when you want to delete the destination DB first.

## Tests

```bash
./scripts/cargo-docker.sh test
bun run test:e2e
bun test tests/dashboard-context.test.ts
(cd browser-extension && bun run ci)
(cd integrations/pi-presence && bun run check)
bun run benchmark:context-storage
```
