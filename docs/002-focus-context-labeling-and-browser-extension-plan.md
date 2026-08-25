# Focus context, dynamic labeling, agent presence, and browser integration plan

## Status

Planning document. No implementation in this document is complete unless explicitly noted as current behavior.

## Goal

Turn focused-window samples into privacy-conscious, structured activity telemetry that can be aggregated by activity, project, service, tool, coding harness, and coding-agent session without hardcoding project names.

The same foundation should support:

- reliable work, entertainment, communication, and unknown-time summaries;
- automatic discovery of new projects and services;
- exact Pi session attribution when an optional Pi extension is installed;
- useful fallback attribution without any agent extension;
- Ghostty/tmux pane, working-directory, and foreground-tool context;
- browser active-tab and media telemetry with substantially better timing than the 30-second system poll;
- historical relabeling when rules or aliases change;
- user corrections and reusable rules;
- future watch-time warnings and blocking policies;
- future agent adapters, browsers, terminal emulators, and application-specific enrichers.

The design must continue to avoid collecting prompts, assistant/thinking content, tool arguments/results, terminal contents, or shell commands.

## Why this needs separate layers

A focused window title is an observation, not an activity label. `YouTube` may be entertainment, technical research, music, or a work tutorial. A tmux window called `watchdog` is a useful project hint, but a Git repository and an exact Pi session are stronger evidence.

Use four distinct layers:

```text
Raw observations
    OS focus, browser state transitions, tmux metadata, agent presence
        ↓
Normalized facts and dynamic entities
    browser.domain, repository, project, tool, agent session
        ↓
Versioned labels
    activity=work, service=youtube, project=<entity>, content=video
        ↓
Derived intervals, analytics, budgets, and interventions
```

Do not collapse these layers into a single `category` column. Facts must remain available so changed rules can relabel history.

## Design principles

1. **Facts before conclusions.** Capture bounded structured facts, then derive labels.
2. **Dynamic entities before name lists.** Discover projects, services, accounts, and tools; never add project names to Rust match statements.
3. **Multiple dimensions.** A sample can be `service=youtube`, `content=video`, `activity=research`, and `project=watchdog` simultaneously.
4. **Confidence and provenance.** Every inferred entity or label records how it was obtained and how certain it is.
5. **Exact integrations are optional.** Core collection remains useful if the browser or Pi extension is absent.
6. **Relabeling is expected.** Rules, aliases, and taxonomies will change.
7. **Intervals must not bridge gaps.** Suspend, monitoring outages, stale heartbeats, and midnight boundaries must be handled explicitly.
8. **Privacy defaults are conservative.** Store domains instead of full URLs, executable names instead of arguments, and repository identities instead of terminal content.
9. **Interventions are downstream.** Telemetry must work without enabling blockers; blockers consume stable attention metrics later.
10. **Adapters emit normalized contracts.** Pi, Claude Code, tmux, browsers, and future sources should not create incompatible analytics paths.

## Current state

The existing collector records the following in `environment_samples` every configured poll interval, currently 30 seconds:

- focused application ID;
- focused window title;
- focused PID;
- audio/video/network and system context.

The process collector separately records process identities and command lines, but detailed process rows are retained for only two days and are not durably linked to a focused terminal pane.

The agent telemetry subsystem already stores Pi's native session ID, project path, session name, parent relationship, activity timestamps, and normalized usage. It does not currently say which Pi session was visible or focused at a given OS sample.

There is currently no tmux metadata, browser extension, browser ingestion API, dynamic entity registry, labeling engine, or policy engine.

## Verified terminal feasibility

On the current host, the relationship can be resolved as:

```text
Ghostty PID 4290
└── tmux client PID 4327
    └── attached session: bms
        └── current window: watchdog
            └── pane: %9
                ├── cwd: /data-shared/workspace/bms/watchdog
                ├── current command: pi
                ├── pane title: π - watchdog
                └── Pi process PID 4923
```

The tmux client is a direct child of the Ghostty process reported by Niri. `tmux list-clients` identifies its currently attached session, and `tmux display-message` identifies the current window/pane, path, command, title, and pane PID.

This makes high-confidence enrichment feasible in the common one-Ghostty-client case.

### Terminal ambiguity

Ghostty may use one host process for multiple windows or tabs. If several tmux clients descend from the same focused Ghostty PID, Niri's PID alone may not identify the visible client.

The collector must:

- return an exact match only when one client is structurally eligible;
- otherwise consider recent tmux client activity as a heuristic;
- record candidate count, match method, and confidence;
- leave the pane unresolved rather than silently choosing an old client;
- support a later terminal-title or Ghostty-specific integration that exposes a pane/client identity.

## Core terminology

### Observation

A point-in-time source report: an OS focus sample, browser state transition, media heartbeat, or agent presence event.

### Fact

A bounded normalized value extracted from an observation, such as `browser.domain=youtube.com`, `terminal.command=pi`, or `repo.key=<hash>`.

### Entity

A dynamically discovered identity with a stable local key. Entity kinds initially include:

- `project`;
- `repository`;
- `service`;
- `tool`;
- `account`;
- `content`;
- `coding_session` when an external session cannot yet be linked to `agent_sessions`.

### Label

A versioned conclusion in a dimension, for example:

- `activity.primary=work`;
- `service=youtube`;
- `project=<project entity>`;
- `tool=pi`;
- `content=livestream`.

### Presence lease

A short-lived statement that a harness process owns a native agent session and is idle or busy. A lease expires unless refreshed.

### Attention interval

A derived time interval during which a reconciled state was continuously valid. Intervals are the preferred input for duration aggregation and policies.

## Target architecture

```text
Niri focused window ─┐
/proc process tree ──┼─> Focus collector ─> focus context facts ─┐
tmux server ─────────┘                                          │
                                                                │
Pi presence extension ─> runtime lease files ─> agent resolver ─┤
Pi JSONL adapter ─────────────────────────────> agent sessions ──┤
                                                                ├─> entity resolver
Browser extension ─> authenticated localhost event API ─────────┤       ↓
                                                                │   label engine
Manual corrections / label rules ───────────────────────────────┘       ↓
                                                               interval builder
                                                                       ↓
                                                    analytics and policy engine
                                                                       ↓
                                                   dashboard / browser intervention
```

Keep browser high-frequency collection and the battery polling loop independently failure-tolerant. Neither should hold long SQLite write transactions.

# Plan A: normalized context and entity model

## A1. Add a dynamic entity registry

Add a migration after `003_agent_usage.sql` with an agent-neutral entity table:

```sql
CREATE TABLE context_entities (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  canonical_key BLOB NOT NULL,
  display_name TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  first_seen_ts INTEGER NOT NULL,
  last_seen_ts INTEGER NOT NULL,
  user_confirmed INTEGER NOT NULL DEFAULT 0,
  archived INTEGER NOT NULL DEFAULT 0,
  UNIQUE(kind, canonical_key)
);

CREATE INDEX idx_context_entities_kind_name
  ON context_entities(kind, display_name);
```

`canonical_key` is a compact SHA-256 key derived from a typed source identity. Examples:

```text
project + NUL + normalized Git common directory
repository + NUL + normalized Git common directory
service + NUL + registrable domain
account + NUL + application ID + NUL + local account identifier
```

Do not use display names as keys. Two repositories can share a basename, and a project can be renamed.

### Entity aliases and relationships

Add:

```sql
CREATE TABLE context_entity_aliases (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_id INTEGER NOT NULL,
  alias_kind TEXT NOT NULL,
  alias_value TEXT NOT NULL,
  source TEXT NOT NULL,
  confidence REAL NOT NULL,
  created_ts INTEGER NOT NULL,
  UNIQUE(alias_kind, alias_value),
  FOREIGN KEY(entity_id) REFERENCES context_entities(id) ON DELETE CASCADE
);

CREATE TABLE context_entity_relations (
  parent_entity_id INTEGER NOT NULL,
  child_entity_id INTEGER NOT NULL,
  relation TEXT NOT NULL,
  confidence REAL NOT NULL,
  source TEXT NOT NULL,
  PRIMARY KEY(parent_entity_id, child_entity_id, relation),
  FOREIGN KEY(parent_entity_id) REFERENCES context_entities(id) ON DELETE CASCADE,
  FOREIGN KEY(child_entity_id) REFERENCES context_entities(id) ON DELETE CASCADE
);
```

Relationships support:

- repository belongs to project;
- worktree belongs to repository;
- service belongs to a broader category;
- project is a component of another project.

Users can merge or split discovered entities without rewriting raw observations.

## A2. Store focus context as change-driven intervals

Keep `environment_samples` backward compatible, but do **not** add a wide context row for every 30-second sample. Extend one open context interval while its meaningful facts remain unchanged; close it and insert a new interval when the app/title, terminal pane, repository/project, foreground tool, agent session/state, confidence, or source validity changes.

A monitoring gap, suspend gap, stale source, or local-midnight boundary also closes an interval even if its labels appear unchanged.

```sql
CREATE TABLE focus_context_intervals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  last_observed_ts INTEGER NOT NULL,
  start_environment_id INTEGER,
  end_environment_id INTEGER,
  context_key BLOB NOT NULL CHECK(length(context_key) = 32),
  extractor_version INTEGER NOT NULL,
  closed_reason TEXT,

  focused_app TEXT NOT NULL DEFAULT '',
  focused_title TEXT,

  terminal_kind TEXT,
  terminal_match_method TEXT,
  terminal_match_confidence REAL,
  terminal_candidate_count INTEGER,
  tmux_client_pid INTEGER,
  tmux_session TEXT,
  tmux_window TEXT,
  tmux_pane TEXT,
  tmux_pane_pid INTEGER,
  terminal_cwd_key BLOB,
  terminal_cwd_display TEXT,
  foreground_executable TEXT,

  repository_entity_id INTEGER,
  project_entity_id INTEGER,

  agent_type TEXT,
  agent_process_pid INTEGER,
  agent_session_id INTEGER,
  native_agent_session_id TEXT,
  agent_state TEXT,
  agent_match_method TEXT,
  agent_match_confidence REAL,

  ambiguity_json TEXT NOT NULL DEFAULT '{}',

  FOREIGN KEY(start_environment_id) REFERENCES environment_samples(id) ON DELETE SET NULL,
  FOREIGN KEY(end_environment_id) REFERENCES environment_samples(id) ON DELETE SET NULL,
  FOREIGN KEY(repository_entity_id) REFERENCES context_entities(id) ON DELETE SET NULL,
  FOREIGN KEY(project_entity_id) REFERENCES context_entities(id) ON DELETE SET NULL,
  FOREIGN KEY(agent_session_id) REFERENCES agent_sessions(id) ON DELETE SET NULL
);

CREATE INDEX idx_focus_context_interval_start
  ON focus_context_intervals(start_ts);
CREATE INDEX idx_focus_context_interval_end
  ON focus_context_intervals(end_ts);
CREATE INDEX idx_focus_context_interval_project
  ON focus_context_intervals(project_entity_id, start_ts);
CREATE INDEX idx_focus_context_interval_agent_session
  ON focus_context_intervals(agent_session_id, start_ts);
```

`context_key` is a deterministic hash of normalized, privacy-filtered facts and confidence/provenance fields. It allows a fast equality check against the current open interval; it is not globally unique because the same state can recur later.

The original environment rows remain the source for exact sampled app/title inspection. `start_environment_id` and `end_environment_id` are optional anchors, not one-to-one context links. A sample resolves to context by timestamp overlap when needed.

This interval table contains common query fields. Less common extractor details remain in `ambiguity_json` or a future sparse fact table. Avoid putting all facts into JSON because project/session/time aggregation should remain indexed.

### Path storage modes

Add a context privacy setting with modes:

- `full`: full local CWD;
- `basename`: repository/workspace basename plus a stable path hash;
- `hash`: stable path hash only.

Recommended default: `basename`. The current Pi adapter already stores project paths; document that context privacy mode does not retroactively hide existing agent telemetry paths unless agent path storage is changed too.

## A3. Add versioned labels

Use separate manual and computed labels so relabeling cannot erase user corrections:

```sql
CREATE TABLE labeler_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  labeler_version INTEGER NOT NULL,
  rules_revision INTEGER NOT NULL,
  started_ts INTEGER NOT NULL,
  completed_ts INTEGER,
  range_start_ts INTEGER,
  range_end_ts INTEGER,
  status TEXT NOT NULL,
  error TEXT
);

CREATE TABLE focus_interval_labels (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  context_interval_id INTEGER NOT NULL,
  dimension TEXT NOT NULL,
  label_key BLOB NOT NULL CHECK(length(label_key) = 32),
  value TEXT,
  entity_id INTEGER,
  confidence REAL NOT NULL,
  source TEXT NOT NULL,
  rule_id INTEGER,
  labeler_version INTEGER NOT NULL,
  evidence_json TEXT NOT NULL DEFAULT '[]',
  FOREIGN KEY(context_interval_id) REFERENCES focus_context_intervals(id) ON DELETE CASCADE,
  FOREIGN KEY(entity_id) REFERENCES context_entities(id) ON DELETE CASCADE,
  UNIQUE(context_interval_id, dimension, label_key, source, labeler_version)
);

CREATE INDEX idx_focus_interval_labels_dimension_entity
  ON focus_interval_labels(dimension, entity_id, context_interval_id);

CREATE TABLE manual_activity_labels (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  dimension TEXT NOT NULL,
  value TEXT,
  entity_id INTEGER,
  note TEXT,
  created_ts INTEGER NOT NULL,
  FOREIGN KEY(entity_id) REFERENCES context_entities(id) ON DELETE CASCADE
);
```

`label_key` is derived from the typed value or entity reference, allowing several values in non-exclusive dimensions without repeating long identity strings in indexes.

The implementation should retain only the latest successful computed label version after a safe relabel transition unless explicit derived-label auditing is enabled. Labeler-run metadata, manual labels, source context intervals, and normalized browser transitions remain. This prevents several historical label copies from multiplying storage.

### Initial dimensions

- `activity.primary`: `work`, `entertainment`, `communication`, `administration`, `system`, `idle`, `unknown`;
- `activity.secondary`: `coding`, `research`, `meeting`, `chat`, `video`, `livestream`, `music`, `browsing`;
- `project`: entity reference;
- `repository`: entity reference;
- `service`: entity reference;
- `tool`: entity reference;
- `account`: entity reference;
- `agent.harness`: stable string such as `pi`;
- `agent.session`: internal `agent_sessions.id` where available.

`activity.primary` is exclusive after precedence resolution. Other dimensions are multi-valued and should use an adjusted key if multiple values per dimension are required.

## A4. Add data-driven labeling rules

Rules live in SQLite or a user-editable config imported into SQLite; they do not live as project-name conditionals in code.

```sql
CREATE TABLE label_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  priority INTEGER NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  origin TEXT NOT NULL,
  conditions_json TEXT NOT NULL,
  outputs_json TEXT NOT NULL,
  stop_dimensions_json TEXT NOT NULL DEFAULT '[]',
  created_ts INTEGER NOT NULL,
  updated_ts INTEGER NOT NULL
);
```

Example service rule:

```json
{
  "conditions": [
    { "fact": "browser.domain", "operator": "domainSuffix", "value": "youtube.com" }
  ],
  "outputs": [
    { "dimension": "service", "entity": "service:youtube.com", "confidence": 1.0 },
    { "dimension": "activity.secondary", "value": "video", "confidence": 0.9 }
  ]
}
```

The rule should not necessarily set `activity.primary=entertainment`. A separate user rule can define whether YouTube defaults to entertainment, research, or unknown.

### Supported condition fields

Initial stable fields:

- `app.id`;
- `window.title`;
- `browser.domain`;
- `browser.origin`;
- `browser.mediaPlaying`;
- `terminal.session`;
- `terminal.window`;
- `terminal.command`;
- `repository.entity`;
- `project.entity`;
- `agent.type`;
- `agent.session`;
- `agent.state`;
- `account.entity`;
- local hour and weekday, only for policy rules rather than semantic labeling.

Initial operators:

- exact;
- case-insensitive exact;
- contains;
- prefix/suffix;
- domain suffix with label-boundary validation;
- glob;
- bounded regular expression;
- entity relationship;
- exists/not exists;
- boolean equality.

Reject dangerous or unbounded expressions. Compile and validate rules before activation.

### Precedence

Recommended precedence:

1. manual labels;
2. explicit user rules;
3. user-confirmed entity aliases;
4. exact integration facts;
5. built-in generic service/tool rules;
6. title/path heuristics;
7. unknown.

A lower-priority rule may still add a different dimension. It may not override an already stopped dimension.

## A5. Incremental and historical labeling

At capture:

1. persist the normal environment sample;
2. compare normalized focus facts with the current open context interval;
3. extend that interval in place when facts are unchanged, otherwise close it and insert one new interval;
4. apply the current compiled rules only when a context interval is created or its exact integration facts materially change;
5. insert interval-level computed labels in a small bounded transaction;
6. update/open reconciled activity intervals.

Do not insert another computed label row merely because another 30-second sample confirms the same state.

For history:

- enqueue relabel jobs when rules, aliases, or entity relationships change;
- process context/activity intervals in bounded batches outside the battery write transaction;
- report progress and errors;
- support a timestamp/entity-scoped relabel instead of always rebuilding 365 days;
- build replacement labels under a new run/version, switch readers after success, then prune the previous derived version;
- never alter original focused titles or agent telemetry;
- make relabeling idempotent.

## A6. Materialized attention intervals

`focus_context_intervals` preserve source facts. Build a second, compact set of reconciled activity intervals for duration queries. Create a new activity interval only when its effective labels, metric state, confidence, or provenance change.

```sql
CREATE TABLE activity_intervals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  source TEXT NOT NULL,
  source_context_interval_id INTEGER,
  activity TEXT,
  project_entity_id INTEGER,
  service_entity_id INTEGER,
  tool_entity_id INTEGER,
  agent_session_id INTEGER,
  confidence REAL NOT NULL,
  labeler_version INTEGER NOT NULL,
  evidence_json TEXT NOT NULL DEFAULT '{}',
  FOREIGN KEY(source_context_interval_id) REFERENCES focus_context_intervals(id) ON DELETE SET NULL
);

CREATE INDEX idx_activity_intervals_time
  ON activity_intervals(start_ts, end_ts);
CREATE INDEX idx_activity_intervals_project_time
  ON activity_intervals(project_entity_id, start_ts);
CREATE INDEX idx_activity_intervals_service_time
  ON activity_intervals(service_entity_id, start_ts);
```

Rules for interval construction:

- cap OS-sample contribution at the expected poll interval plus a small tolerance;
- do not fill monitoring gaps or suspend gaps;
- close browser intervals when heartbeats become stale;
- split intervals at local midnight for daily aggregation, including breaks that continue into the next day;
- handle timezone changes and DST explicitly;
- never double-count browser extension time and OS focused-window time;
- preserve separate metrics for browser focus, visible tab, media playback, audible playback, and verified focused playback;
- merge adjacent intervals only when all effective dimensions, confidence, provenance, and metric states match.

## A7. Measured storage budget and guardrails

### Live baseline

Measured against the live database after 35.1 days:

- SQLite main file: approximately 1.04 GiB;
- WAL at measurement time: approximately 37 MiB;
- environment/focus samples: 55.9k, or 1,593 observed samples/day;
- projected samples at the observed active-machine rate: approximately 581k/year;
- app/title context runs: 388/day, or approximately 142k/year.

Current database allocation is dominated by existing aggregate tables:

| Existing category | Current size | Rough annual density at the observed 35-day rate |
| --- | ---: | ---: |
| Process group totals and indexes | 800 MiB | 8.1 GiB/year |
| App totals and indexes | 144 MiB | 1.5 GiB/year |
| Environment samples and index | 35 MiB | 0.36 GiB/year |
| Agent telemetry and indexes | 19 MiB | roughly 0.1–0.2 GiB/year |
| Battery samples and index | 4 MiB | 0.04 GiB/year |

The annual values are capacity estimates, not exact forecasts: activity level, retention pruning, page reuse, and agent history spans differ. They show that existing process/app aggregates—not contextual labeling—are the primary database growth risk.

### Rejected naïve design

Do not implement all of the following together:

- one wide context row for every environment sample;
- three to five computed label rows per sample;
- a durable browser row for every 10–20 second heartbeat;
- multiple retained copies of all computed labels after relabeling.

At the observed sampling rate, that design would add roughly 0.5–1.5 GiB/year:

| Naïve component | Approximate annual addition |
| --- | ---: |
| 581k wide focus-context rows | 100–250 MiB |
| 1.7–2.9m per-sample label rows | 200–600 MiB |
| Persisted browser heartbeats | 150–600 MiB |
| Browser transitions and agent state | 30–100 MiB |

### Target compact budget

With change-driven context intervals, interval-level labels, transition-only browser history, in-place heartbeat leases, and current-version-only derived labels, budget:

| Compact component | Approximate annual addition |
| --- | ---: |
| Terminal/project context intervals | 25–60 MiB |
| Activity labels and reconciled intervals | 20–50 MiB |
| Browser transitions and attention intervals | 30–80 MiB |
| Pi/other agent state transitions | 2–10 MiB |
| Entities, aliases, rules, and policies | under 5 MiB |
| **Total target** | **approximately 100–200 MiB/year** |

Use **100–250 MiB/year** as the conservative capacity budget, excluding short-lived WAL growth. At 365-day context retention, logical live data should plateau near that range. SQLite may retain previously allocated pages after pruning, but should reuse them; shrinking the physical file requires an explicit maintenance operation such as `VACUUM`.

This target is approximately 1–2.5% of the current rough annual database density. The implementation must benchmark representative synthetic and copied live-shape data before schema freeze. Exceeding 250 MiB/year requires a design review, not merely a larger retention warning.

### Storage metrics

Expose at least:

- rows and allocated bytes by context/browser/label table and index;
- open versus closed interval counts;
- context transitions/day and browser transitions/day;
- heartbeat upserts versus persisted transitions;
- current and previous derived-label version bytes;
- estimated annualized growth from the latest 30 complete days;
- compaction/pruning lag and reusable SQLite pages.

# Plan B: terminal and project enrichment

## B1. Add a terminal context extractor interface

Create a generic interface under a new module such as `src/context/terminal/`:

```rust
trait TerminalContextExtractor {
    fn supports(&self, focused: &FocusedWindow) -> bool;
    fn extract(
        &self,
        focused: &FocusedWindow,
        processes: &ProcessSnapshot,
    ) -> Result<TerminalContext>;
}
```

The first implementation is `TmuxExtractor`; Ghostty detection is one trigger, not part of the normalized output contract. Later implementations can support shells, Zellij, Kitty, WezTerm, VS Code terminals, or remote sessions.

## B2. Resolve focused Ghostty to tmux

When the focused application is Ghostty:

1. identify all tmux client processes descending from the focused PID;
2. query current-user tmux sockets with strict timeouts;
3. match `tmux list-clients` records by client PID;
4. if exactly one eligible client exists, resolve its current session;
5. query the session's current window and pane;
6. capture session name, window name/index, pane ID/index/PID, current path, current executable, and pane title;
7. inspect the pane process descendants for recognized foreground tools and active presence leases;
8. record match method, candidate count, and confidence.

Use argument-array process invocation; never invoke a shell. Bound command output and execution time. A missing tmux server is a normal `not applicable` result, not a collection error.

### Multi-client handling

Confidence examples:

- `1.0`: one tmux client PID is a descendant of focused PID and is listed as attached;
- `0.8`: several descendants exist but only one client has recent activity and a current attached session;
- `0.5`: pane inferred from terminal title or session activity without exact client mapping;
- unresolved: candidates remain tied or stale.

Analytics should normally require at least `0.8` for project attribution unless the user chooses a more permissive threshold.

## B3. Detect tools without command content

Capture executable basenames and process structure only:

- `pi`;
- `claude`;
- `codex`;
- `aider`;
- `nvim`/`vim`;
- shells;
- future configured tools.

The recognized tool registry must be data-driven or adapter-provided. Do not persist executable arguments for long-term focus context. The existing detailed process subsystem may continue its short-retention command behavior independently.

If several agent processes exist in the pane tree, prefer a presence lease matching PID/process start identity. Otherwise expose all candidates and mark the foreground selection ambiguous.

## B4. Discover repositories and projects dynamically

For a resolved pane CWD or agent CWD:

1. canonicalize within configured safety limits;
2. find the nearest Git worktree root;
3. resolve the Git common directory so worktrees map to one repository;
4. create or update a `repository` entity;
5. derive a default project entity from repository identity;
6. add CWD, repository basename, tmux window, and tmux session as aliases with source-specific confidence;
7. allow user merges/splits and parent-child project relations.

Do not contact Git remotes. If remote metadata is used, read local configuration only and store a hash by default. Repository basename is the initial display name.

### Non-Git fallback

Use configured workspace roots and marker files. A fallback project key can be based on the first directory below a workspace root. This remains lower confidence than Git identity.

Example configuration:

```json
{
  "workspaceRoots": [
    "/data-shared/workspace",
    "/home/user/workspace"
  ]
}
```

New directories under those roots become entities automatically.

### Caching

Repository resolution must be cached by canonical path and marker metadata. Do not run `git` every 30 seconds for unchanged paths. Invalidate on path changes, cache expiry, or relevant `.git` metadata changes.

## B5. Terminal acceptance criteria

- Focusing the current `bms/watchdog` pane records project `watchdog`, tool `pi`, and pane `%9` within one poll.
- Switching tmux windows updates context within one poll.
- Detached background tmux sessions are not labeled as focused work.
- A focused shell with no agent remains coding/work only if a project/tool rule supports that conclusion.
- Multiple eligible Ghostty clients produce an ambiguity record rather than an arbitrary pane.
- No pane scrollback, command arguments, prompts, or shell history enter SQLite or logs.
- tmux failure adds negligible latency and never aborts battery collection.

# Plan C: exact coding-agent presence and Pi session attribution

## C1. Add a generic presence contract

Do not add Pi-only columns to focus collection. Define a normalized lease payload:

```json
{
  "schemaVersion": 1,
  "eventId": "random-id",
  "harness": "pi",
  "sourceKey": "local",
  "instanceId": "random-process-instance",
  "pid": 4923,
  "processStartIdentity": "linux-start-ticks-or-random-instance",
  "nativeSessionId": "session-uuid",
  "cwd": "/data-shared/workspace/bms/watchdog",
  "state": "idle",
  "observedAt": 1785340300000,
  "expiresAt": 1785340360000
}
```

Allowed states initially:

- `idle`: session open, agent not running;
- `busy`: agent loop active;
- `settling`: optional future state;
- `closed`: explicit lease removal.

No event may contain prompt text, message content, tool metadata, model payloads, or commands.

## C2. Use user-runtime presence files for Pi

Recommended first transport: atomic lease files under a current-user runtime directory, for example:

```text
$XDG_RUNTIME_DIR/bms-watchdog/agent-presence/pi-<pid>-<instance>.json
```

Why runtime files rather than requiring HTTP:

- works with `--no-serve`;
- no CORS or bearer token;
- same-user filesystem permissions provide the boundary;
- easy crash recovery through TTL;
- the collector can read them during a normal sample;
- no long-lived socket is required inside Pi.

Security requirements:

- directory mode `0700`, files `0600`;
- atomic write/rename;
- reject symlinks, non-regular files, oversized files, wrong owner, invalid schema, stale timestamps, and unexpected harness names;
- cap file count and payload size;
- verify PID existence and process start identity where available;
- expire stale leases even if shutdown cleanup never runs.

A future authenticated HTTP or Unix-socket transport can implement the same contract.

## C3. Implement an optional Pi extension

Place a maintained extension package under a repository integration directory and add an installer command or documented copy step to install it globally under Pi's extension directory.

Pi exposes the exact required APIs:

- `ctx.sessionManager.getSessionId()`;
- `ctx.sessionManager.getSessionFile()`;
- `ctx.sessionManager.getCwd()` / `ctx.cwd`;
- `session_start` for startup, reload, new, resume, and fork;
- `agent_start`;
- `agent_settled`;
- `session_info_changed`;
- `session_shutdown`.

Behavior:

1. on `session_start`, create an instance nonce and write an `idle` lease with the exact native session ID, PID, and CWD;
2. on `agent_start`, update to `busy`;
3. on `agent_settled`, update to `idle`;
4. on session name change, optionally update the non-sensitive session display name;
5. heartbeat the current state at a bounded interval while the session is alive;
6. on `session_shutdown`, remove or close the lease idempotently;
7. do not append extension entries to Pi sessions;
8. do not notify in the TUI during normal operation;
9. treat watchdog absence or write failure as non-fatal and rate-limit diagnostics.

Pi documentation requires long-lived resources to start at `session_start` and be cleaned up idempotently at `session_shutdown`; follow that lifecycle.

### Session ID semantics

For focused activity, link to the **currently active native Pi session**, including a fork or clone. Do not substitute the canonical origin session used to deduplicate copied token history. Usage-origin attribution and human-focus attribution answer different questions.

The presence resolver looks up:

```text
agent_sources(agent_type=pi, source_key=local)
    + agent_sessions.native_session_id
```

If the JSONL adapter has not indexed the session yet, preserve the native session ID in `focus_context_intervals` and backfill the foreign key after indexing.

In-memory Pi sessions may never appear in `agent_sessions`; retain a temporary external coding-session entity or a nullable link.

## C4. Add no-extension fallback correlation

Without a Pi extension:

1. detect `pi` in the focused pane process tree;
2. read its process CWD and start identity;
3. find agent sessions with matching project path and plausible creation/activity times;
4. assign an exact session only if one candidate is uniquely recent;
5. otherwise keep `agent_type=pi`, project/tool context, and a null session;
6. record method `cwd_recent_session` and lower confidence.

The running Pi process does not currently keep its JSONL session file open, so `/proc/<pid>/fd` is not an exact session source. Do not claim exact attribution from it.

## C5. Presence tables

Use one current-state table and an optional transition history:

```sql
CREATE TABLE agent_presence_leases (
  harness TEXT NOT NULL,
  source_key TEXT NOT NULL,
  instance_id TEXT NOT NULL,
  pid INTEGER NOT NULL,
  process_start_identity TEXT,
  native_session_id TEXT NOT NULL,
  cwd_key BLOB,
  state TEXT NOT NULL,
  observed_ts INTEGER NOT NULL,
  expires_ts INTEGER NOT NULL,
  PRIMARY KEY(harness, source_key, instance_id)
);

CREATE TABLE agent_state_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  event_key BLOB NOT NULL UNIQUE,
  harness TEXT NOT NULL,
  source_key TEXT NOT NULL,
  native_session_id TEXT NOT NULL,
  event_ts INTEGER NOT NULL,
  state TEXT NOT NULL
);
```

Do not persist every lease heartbeat as an event. Update `observed_ts` in place and insert only meaningful state transitions.

## C6. Future Claude Code integration

Initial support can detect the `claude` executable and CWD in the focused pane. Exact session IDs should later use Claude Code's supported hook/plugin mechanism and emit the same generic presence contract.

Adding Claude Code must not require a new focus table or activity chart. Only the presence producer and agent telemetry adapter are harness-specific.

## C7. Agent acceptance criteria

- `/new`, `/resume`, `/fork`, and `/clone` change focused session attribution to the new exact native Pi session.
- `busy` begins on `agent_start` and ends on `agent_settled` without inspecting content.
- Killing Pi leaves no valid lease after TTL.
- A stale lease cannot attach to a reused PID.
- Multiple Pi processes in different panes are distinguished by PID and pane ancestry.
- Fork-deduplicated token attribution remains unchanged.
- The feature continues to work at project/tool level when the extension is absent.

# Plan D: browser extension telemetry foundation

## D1. Scope and browser order

Target Zen/Firefox first because it is the current browser, while keeping shared WebExtension code portable to Chromium.

Suggested repository layout:

```text
browser-extension/
  package.json
  src/
    background.ts
    content/media.ts
    state.ts
    transport.ts
    policy.ts
    options/
    blocked/
  manifests/
    firefox.json
    chromium.json
  tests/
```

Use a small TypeScript build driven by Bun, consistent with the existing JavaScript test environment. Keep browser-specific code behind narrow adapters.

## D2. Privacy modes

Offer explicit modes:

1. `off`;
2. `domains-only` — recommended default;
3. `domains-and-titles`;
4. `selected-sites-media` — media details only for user-approved domains;
5. `full-debug` — temporary and clearly warned, never default.

Default persisted browser facts:

- normalized origin/domain;
- browser profile/client identity;
- active/focused/visible state;
- media playing/paused/audible state;
- timestamps and event identity;
- optional service entity;
- no full URL, query, fragment, form input, page text, cookies, or request contents.

Incognito/private telemetry is disabled by default. If the user explicitly enables the extension in private mode, the extension still marks private events and the server discards or heavily redacts them unless separately opted in.

## D3. Permissions strategy

Separate baseline telemetry permissions from intervention permissions.

Baseline needs:

- tab/window focus and activation;
- tab title/origin access according to selected privacy mode;
- local extension storage;
- access to `http://127.0.0.1:<configured-port>/api/integrations/browser/v1/*`.

Media telemetry and blocking need content-script access on selected origins. Request optional host permissions when the user enables a service policy instead of requiring all-site content access on install where browser APIs permit.

Provide a permissions/data preview page explaining exactly what each mode sends.

## D4. Event-driven browser state machine

Do not pollute SQLite with a heartbeat row every few seconds. The extension sends transitions immediately and heartbeats only to keep an open state lease and interval alive. The ingestion path upserts lease timestamps for a heartbeat; it inserts historical rows only for material state transitions. If a heartbeat reveals changed state after a missed transition, synthesize one transition and continue from it.

Observe:

- active-tab changes;
- browser-window focus changes;
- navigation/URL origin changes;
- tab close;
- document visibility;
- media play, pause, ended, seeking, and picture-in-picture;
- audible state;
- periodic heartbeat while an eligible state remains active.

Event types:

```text
snapshot
window_focus
active_tab
navigation
visibility
media_play
media_pause
media_end
heartbeat
tab_close
```

Each event includes:

- extension-generated UUID/event key;
- monotonic client sequence;
- event timestamp;
- profile/client ID;
- ephemeral browser window/tab identifiers;
- normalized domain/origin;
- active, browser-focused, document-visible, audible, and media state;
- optional media duration/current position only when media telemetry is enabled.

Use browser tab/window IDs only for short-term state reconstruction; they are not durable identities.

### Watch-time metrics

Keep separate counters:

- `tab_active_seconds`;
- `browser_focused_seconds`;
- `document_visible_seconds`;
- `media_playback_seconds`;
- `audible_playback_seconds`;
- `focused_playback_seconds` — recommended blocker default;
- `background_playback_seconds`.

Do not call all of these `watch time`. Policies must name the metric they consume.

## D5. Content/media adapter model

Start with a generic HTML5 `<video>` observer and add service adapters only when necessary.

```typescript
interface SiteAdapter {
  matches(origin: string): boolean;
  readMediaState(document: Document): MediaState | undefined;
  applyIntervention(decision: PolicyDecision): void;
}
```

Initial adapters:

- generic HTML5 video;
- YouTube, including SPA navigation and player replacement;
- Twitch;
- Kick;
- a no-op generic domain adapter for focus-only telemetry.

Service adapter registration is data/config driven where possible. Site-specific DOM behavior remains code because player APIs differ, but service classification rules remain editable data.

Do not store video titles or IDs by default. They can be an opt-in future capability for distinguishing technical research from entertainment.

## D6. Offline queue and delivery

Browser service workers suspend unpredictably. Implement a durable bounded queue in extension local storage:

- send transitions immediately;
- batch events, for example up to 100 events or 128 KiB;
- retry with exponential backoff and jitter;
- preserve ordering per client sequence;
- deduplicate server-side by event key;
- cap queue age and size;
- prefer retaining transitions over redundant heartbeats when compacting;
- expose queue depth and last successful sync in extension options.

A heartbeat can be every 10–20 seconds during focused playback and less frequent during simple tab focus. Exact values should be measured in the Zen spike.

## D7. Browser database model

```sql
CREATE TABLE browser_clients (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  client_key BLOB NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  browser_family TEXT NOT NULL,
  extension_version TEXT NOT NULL,
  secret_hash BLOB NOT NULL,
  allowed_origin TEXT,
  privacy_mode TEXT NOT NULL,
  paired_ts INTEGER NOT NULL,
  last_seen_ts INTEGER,
  revoked_ts INTEGER
);

CREATE TABLE browser_current_states (
  client_id INTEGER PRIMARY KEY,
  lease_key BLOB NOT NULL CHECK(length(lease_key) = 32),
  client_sequence INTEGER NOT NULL,
  state_started_ts INTEGER NOT NULL,
  last_observed_ts INTEGER NOT NULL,
  expires_ts INTEGER NOT NULL,
  domain TEXT,
  origin TEXT,
  title TEXT,
  browser_focused INTEGER,
  tab_active INTEGER,
  document_visible INTEGER,
  audible INTEGER,
  media_state TEXT,
  private INTEGER NOT NULL DEFAULT 0,
  payload_version INTEGER NOT NULL,
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE CASCADE
);

CREATE TABLE browser_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  client_id INTEGER NOT NULL,
  event_key BLOB NOT NULL,
  client_sequence INTEGER NOT NULL,
  event_ts INTEGER NOT NULL,
  received_ts INTEGER NOT NULL,
  event_type TEXT NOT NULL,
  domain TEXT,
  origin TEXT,
  title TEXT,
  browser_focused INTEGER,
  tab_active INTEGER,
  document_visible INTEGER,
  audible INTEGER,
  media_state TEXT,
  media_position REAL,
  media_duration REAL,
  playback_rate REAL,
  private INTEGER NOT NULL DEFAULT 0,
  payload_version INTEGER NOT NULL,
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE CASCADE,
  UNIQUE(client_id, event_key)
);

CREATE INDEX idx_browser_events_time ON browser_events(event_ts);
CREATE INDEX idx_browser_events_domain_time ON browser_events(domain, event_ts);
```

`browser_current_states` is updated in place by normal heartbeats. `browser_events` stores transitions, recovery snapshots, and intervention-relevant events—not unchanged heartbeats. Closed browser states are compacted into reconciled activity intervals before transition pruning.

### Browser retention

Recommended initial defaults:

- queued extension events: maximum 24 hours offline;
- unchanged server-side heartbeat rows: none;
- current browser state leases: one row per paired client/profile;
- browser state transitions: 30–90 days;
- normalized domains and derived attention intervals: match context retention, initially 365 days;
- titles/content identifiers: opt-in retention, independently configurable.

Historical relabeling requires retaining normalized domain/service facts even after high-frequency raw events are removed.

# Plan E: browser pairing, ingestion API, and security

## E1. Threat model

The server currently exposes read-only APIs and may be configured beyond localhost. Browser ingestion adds state-changing endpoints and a potentially powerful future policy channel.

Protect against:

- arbitrary websites posting fake activity to localhost;
- permissive CORS;
- leaked pairing or bearer secrets;
- oversized event batches;
- event replay;
- timestamp abuse;
- untrusted extension origins;
- accidentally accepting remote network ingestion when the dashboard binds publicly;
- secrets or sensitive payloads entering logs.

## E2. Pairing flow

1. Dashboard generates a random one-time pairing code valid for five minutes.
2. User enters or confirms it in the extension options page.
3. Extension calls the pairing endpoint from an extension origin.
4. Server validates code, loopback peer, expected origin shape, expiry, and rate limits.
5. Server returns a high-entropy client token once.
6. Extension stores it in extension-local storage.
7. Server stores only a hash.
8. Dashboard shows client, permissions/privacy mode, last sync, and revoke action.

Do not use cookies. Every extension request carries an authorization header and client identity. Revocation invalidates the token immediately.

## E3. API contract

Version endpoints from the beginning:

```text
POST   /api/integrations/browser/v1/pair
POST   /api/integrations/browser/v1/events:batch
GET    /api/integrations/browser/v1/config
GET    /api/integrations/browser/v1/policy-state
POST   /api/integrations/browser/v1/interventions
DELETE /api/integrations/browser/v1/client
```

The event response returns:

- accepted and duplicate counts;
- rejected event indexes/reasons;
- server timestamp;
- current configuration/policy revision;
- current policy decision for the active domain when applicable.

## E4. Server controls

- Accept integration writes only from loopback peer addresses even if the dashboard binds to another interface.
- Add explicit, narrow CORS handling only for paired extension origins.
- Never use `Access-Control-Allow-Origin: *`.
- Require JSON content type and authorization.
- Limit batch size, body bytes, event count, domain/title length, and nesting.
- Validate domains with label boundaries and normalize IDNs safely.
- Reject full URLs when privacy mode does not permit them.
- Permit old timestamps only within the configured offline queue window.
- Deduplicate events transactionally.
- Parse and validate outside the SQLite write transaction, then apply bounded batches.
- Use the existing SQLite busy timeout and avoid blocking the battery collector.
- Do not log tokens, titles, URLs, or event payloads.
- Add request-rate and pairing-attempt limits.

All future dashboard state-changing endpoints for labels and policies also need CSRF/origin protection; localhost alone is not sufficient because arbitrary websites can target localhost.

# Plan F: browser/OS reconciliation and analytics

## F1. Reconcile browser reports with OS focus

The extension has better tab/media timing; Niri confirms which application is actually focused. Use both:

- an extension browser-window-focus event is strong browser-local evidence;
- a recent Niri sample matching that browser raises confidence to verified OS focus;
- if the OS sample shows another app, count playback separately as background playback;
- if Niri is missing, keep extension time with lower confidence rather than discarding it;
- never count the same interval once from title samples and again from browser events.

Reconcile browser and OS context by timestamp overlap and focused browser identity. Preserve source interval IDs in derived evidence rather than adding a browser-event foreign key to every OS sample or context confirmation.

## F2. Duration and cross-day semantics

For all daily summaries:

- split intervals at local midnight;
- allow an entertainment episode to start after work and continue into the following day;
- do not assign the entire cross-midnight episode only to its starting day;
- keep episode identity so analyses can distinguish one continuous break from two episodes;
- exclude partial dataset boundary dates from daily averages;
- do not fill collector/extension gaps;
- report observed coverage alongside durations.

## F3. Analytics APIs

Add agent-neutral endpoints:

```text
GET /api/activity/summary?from=&to=&group=activity|project|service|tool|agentSession
GET /api/activity/timeline?from=&to=&resolution=
GET /api/activity/episodes?kind=entertainment&minSeconds=
GET /api/context/unknown?from=&to=&limit=
GET /api/context/entities?kind=
GET /api/context/rules
POST /api/context/rules
POST /api/context/manual-labels
POST /api/context/relabel
```

Every aggregate response should include:

- observed duration;
- unknown/unclassified duration;
- monitoring coverage;
- attribution confidence distribution;
- sample/episode count;
- classifier and rules revision;
- filters and metric definition.

Project break analysis must normalize by project exposure hours and active project days, not rank only raw break totals.

## F4. Dashboard views

### Context inspector

Show the latest focused context:

- app/title according to privacy settings;
- browser domain/service and media state;
- terminal session/window/pane;
- repository/project;
- foreground tool;
- harness/session and idle/busy state;
- labels, confidence, and evidence source;
- ambiguity warnings.

### Unknown review queue

Rank unlabeled or low-confidence time by duration. The user can:

- assign an activity;
- attach/merge a project or service entity;
- apply to one interval, exact context, domain, repository, title pattern, or future occurrences;
- preview how much history a generated rule changes;
- undo a rule and relabel affected history.

### Activity analytics

Support grouping by:

- activity;
- project;
- service;
- tool;
- harness;
- exact agent session;
- day/weekday/time of day.

Keep focus time, media playback, and coding-agent busy time as distinct metrics.

# Plan G: watch-time budgets and blocking

Implement only after browser telemetry and interval accounting are validated. A blocker built on inaccurate counters will be frustrating and easy to distrust.

## G1. Policy model

```sql
CREATE TABLE attention_policies (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  scope_json TEXT NOT NULL,
  metric TEXT NOT NULL,
  window_kind TEXT NOT NULL,
  budget_seconds INTEGER NOT NULL,
  warning_seconds INTEGER,
  enforcement TEXT NOT NULL,
  schedule_json TEXT NOT NULL DEFAULT '{}',
  grace_json TEXT NOT NULL DEFAULT '{}',
  revision INTEGER NOT NULL,
  created_ts INTEGER NOT NULL,
  updated_ts INTEGER NOT NULL
);

CREATE TABLE intervention_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  policy_id INTEGER NOT NULL,
  client_id INTEGER,
  event_ts INTEGER NOT NULL,
  action TEXT NOT NULL,
  service_entity_id INTEGER,
  remaining_seconds INTEGER,
  decision_revision INTEGER NOT NULL,
  FOREIGN KEY(policy_id) REFERENCES attention_policies(id) ON DELETE CASCADE
);
```

Scopes can target:

- service entity/domain;
- activity label;
- weekday/time schedule;
- project context;
- content mode;
- combinations.

Initial windows:

- local calendar day;
- rolling N hours;
- per-session continuous duration;
- future weekly budget.

## G2. Enforcement levels

- `observe`: count only;
- `warn`: non-blocking warning;
- `soft-block`: overlay with explicit continue/snooze;
- `hard-block`: pause media and cover/redirect the page until reset or authorized override.

Start with observe and warn. Hard blocking should be opt-in per service.

## G3. Enforcement mechanics

For selected sites, the content script can:

- pause HTML5 video;
- display an extension-owned full-page overlay;
- prevent immediate autoplay restart;
- redirect to an extension blocked page if necessary;
- use dynamic declarative network rules only if a whole-domain block is explicitly selected.

YouTube requires SPA navigation handling. Twitch/Kick may require service-specific player adapters.

## G4. Offline behavior

The extension retains the latest policy revision and a local short-term counter so service-worker/server outages do not reset the budget.

Recommended default is fail-open for telemetry failures during early rollout, with a visible degraded-state warning. A user may later choose fail-closed for selected hard-block policies.

Server and extension reconcile queued events idempotently. A clock correction must not double charge time.

## G5. User controls

- warning thresholds;
- grace period;
- snooze length and daily snooze count;
- schedule;
- temporary disable;
- project/research allowlist;
- emergency override;
- metric selection;
- clear explanation of counted time.

Store override action and duration, but not free-text reasons by default.

## G6. Blocker acceptance criteria

- A 60-minute `focused_playback` budget does not count a paused YouTube tab.
- Background audio is counted only if the selected metric includes it.
- Switching away from the browser stops focused-playback accumulation promptly.
- Crossing midnight resets calendar-day budget while preserving the continuous episode for analytics.
- Browser/server restart does not lose or duplicate accumulated time.
- Technical/research allowlist rules can exempt a domain/context without deleting telemetry.
- Revoking the extension token stops policy access and event ingestion.

# Plan H: configuration, retention, and privacy controls

## H1. Runtime configuration

Add settings equivalent to:

```text
--no-context-tracking
--context-retention-days <DAYS>
--context-path-mode full|basename|hash
--no-terminal-context
--tmux-socket <PATH>                 # repeatable override
--workspace-root <PATH>              # repeatable
--browser-event-retention-days <DAYS>
--browser-title-retention-days <DAYS>
--no-browser-integration
--agent-presence-dir <PATH>
```

Installer propagation must include non-default values. Keep privacy-impacting options visible in `README.md` and dashboard status.

## H2. Retention ordering

Prune in dependency-safe order:

1. close or mark stale open leases and compact closed transition history into intervals;
2. delete expired browser transitions; unchanged heartbeats have no historical rows;
3. delete superseded computed-label versions after a successful reader switch;
4. delete old activity and focus-context intervals;
5. clear old optional environment anchors, then delete old environment samples;
6. delete unreferenced entities only if archived and outside a conservative grace period;
7. retain label rules, manual labels, policy definitions, and pairing audit metadata unless explicitly deleted.

Context and activity intervals follow context retention independently of raw browser-transition retention. Optional environment anchors use `ON DELETE SET NULL`, so retaining a derived interval never forces retention of an old raw environment sample.

## H3. Export and deletion

Add local controls to:

- preview what is stored for the current tab/window/pane;
- export normalized activity intervals and entities;
- delete browser titles while retaining domains and durations;
- delete one browser client/profile and its events;
- delete/merge a project entity;
- revoke extension pairing;
- disable and remove the Pi presence extension;
- rebuild derived labels/intervals after deletion.

## H4. Logging policy

Normal logs may include:

- source type;
- event counts;
- timing;
- schema/rules revision;
- sanitized error category.

Normal logs must not include:

- browser titles or URLs;
- terminal paths under `basename`/`hash` mode;
- tmux pane titles if they are not retained;
- native agent session IDs unless debug mode is explicitly enabled;
- pairing secrets;
- extension payloads.

# Plan I: implementation sequence

## Phase 0: focused spikes and decisions

Deliverables:

1. tmux extractor prototype with one and multiple Ghostty clients;
2. repository/worktree identity prototype;
3. Pi presence extension prototype using `getSessionId()` and runtime leases;
4. Zen/Firefox extension prototype that reports domain/focus/media transitions;
5. localhost pairing/CORS prototype;
6. event-volume and service-worker suspension measurements;
7. a representative SQLite storage benchmark comparing per-sample and interval schemas;
8. finalized privacy defaults and retention budget.

Exit criteria:

- exact current tmux pane resolution works in normal use;
- Pi `/new`, `/resume`, and `/fork` are observed exactly;
- browser state survives extension service-worker suspension;
- no full URL or content is needed for the baseline product;
- compact projected growth is at or below the 100–250 MiB/year target and unchanged heartbeats produce no historical rows.

## Phase 1: context schema, entity resolver, and offline labeler

Deliverables:

- migration for entities, change-driven focus-context intervals, interval labels, rules, and reconciled activity intervals;
- Rust types and repository interfaces;
- dynamic service/project entity creation;
- rule validation, precedence, and versioning;
- historical title-based backfill;
- unknown coverage API;
- retention implementation.

Historical backfill can recover services and some title-derived activity labels. It cannot reconstruct old tmux panes or exact focused Pi sessions. Temporal Pi/project guesses must be marked low confidence.

Exit criteria:

- current hand-written entertainment/work SQL can be expressed through labels;
- adding `kick.com` is a rule/entity operation, not a Rust code change;
- relabeling changes aggregates reproducibly;
- existing battery and agent tests remain green.

## Phase 2: terminal and dynamic project enrichment

Deliverables:

- terminal extractor interface;
- tmux client/pane resolution;
- process-tree tool detection;
- Git common-directory/worktree resolver;
- project merge/split/alias support;
- terminal context inspector UI.

Exit criteria: all Plan B acceptance criteria pass.

## Phase 3: exact Pi presence integration

Deliverables:

- generic runtime presence contract and secure lease reader;
- Pi extension package;
- install/uninstall/status support;
- exact session backfill after agent indexer catches up;
- busy/idle intervals;
- no-extension fallback.

Exit criteria: all Plan C acceptance criteria pass.

## Phase 4: browser extension telemetry MVP

Deliverables:

- Zen/Firefox build;
- pairing and revocation UI;
- event ingestion API;
- domain-only active-tab transitions;
- generic and YouTube media state;
- bounded offline queue;
- browser/OS reconciliation;
- extension health/status UI.

No blocking in this phase.

Exit criteria:

- measured focused YouTube time is accurate within the heartbeat tolerance;
- inactive/background tabs are distinguishable;
- queued events replay once;
- arbitrary webpages cannot ingest events;
- browser absence does not affect battery polling.

## Phase 5: labeling UI and activity analytics

Deliverables:

- unknown review queue;
- rule editor and impact preview;
- entity alias/merge controls;
- activity/project/service/session timelines;
- exposure-normalized break analysis;
- cross-midnight episodes;
- confidence and coverage displays.

Exit criteria:

- project and service summaries no longer require ad hoc SQL name lists;
- every duration reports observed coverage;
- immediate and next-day break analyses consume the same normalized intervals.

## Phase 6: Chromium portability and selected-site media adapters

Deliverables:

- Chromium manifest/build;
- cross-browser API adapter;
- Twitch and Kick media adapters;
- optional host permission flow;
- browser-specific integration tests.

## Phase 7: watch budgets and interventions

Deliverables:

- policy schema/API/UI;
- observe and warning modes;
- local policy cache/counters;
- YouTube soft block;
- optional hard block;
- Twitch/Kick intervention adapters;
- intervention audit timeline.

Exit criteria: all Plan G acceptance criteria pass.

## Phase 8: more applications and agents

Candidates:

- Claude Code exact presence and usage adapter;
- editor/LSP context;
- Zoom meeting identity/duration;
- communication-account adapters;
- terminal-title integration for multi-Ghostty-client disambiguation;
- local optional title classifier for technical video versus entertainment.

# Plan J: testing strategy

## J1. Rust unit tests

### Entity resolution

- stable typed hashing;
- same Git common directory across worktrees;
- same basename but different repositories;
- entity merge/split and alias precedence;
- workspace-root fallback;
- path privacy modes.

### Rules

- every condition operator;
- domain boundary safety (`notyoutube.com` must not match `youtube.com`);
- priority and stopped dimensions;
- manual override precedence;
- invalid regex/rule rejection;
- deterministic relabel output;
- version migration.

### Terminal

- tmux format parsing;
- focused PID ancestry;
- one-client exact match;
- multi-client ambiguity;
- stale client activity;
- missing socket and timeout;
- pane process foreground selection;
- output/path bounds.

### Agent presence

- file ownership/mode/symlink checks;
- malformed/oversized lease;
- TTL;
- PID reuse/start identity;
- busy/idle transitions;
- native session lookup before and after index;
- current fork session versus canonical usage origin.

### Browser ingestion

- pairing expiry and one-time use;
- bearer verification and revocation;
- loopback-only enforcement;
- strict CORS;
- batch/body limits;
- event deduplication;
- timestamp skew and offline replay window;
- privacy-mode field rejection;
- unchanged heartbeats update `browser_current_states` without increasing `browser_events` rows;
- a missed transition discovered by a heartbeat creates exactly one recovery transition;
- bounded SQLite transactions.

### Intervals

- normal sample cadence;
- repeated unchanged samples extend one context interval rather than inserting rows;
- app/title, pane, project, agent session/state, gap, and midnight changes close intervals correctly;
- monitoring gaps;
- suspend gaps;
- stale browser heartbeat;
- source reconciliation/no double count;
- cross-midnight split;
- DST transition;
- ongoing interval closure;
- relabel rebuild.

## J2. Extension tests

Use Bun unit tests with browser API fakes for:

- tab/window state reducer;
- domain/origin normalization;
- private mode exclusion;
- media state reducer;
- transition versus heartbeat emission;
- queue compaction and ordering;
- retry/backoff;
- token storage/revocation;
- policy counter and reset semantics;
- intervention state machine;
- YouTube SPA/media adapter.

Run manifest validation and linting for each browser target.

## J3. End-to-end tests

Extend existing Bun E2E tests with:

- migration creation and retention;
- synthetic focused-window JSON and fake tmux command runner;
- runtime presence fixture linked to a synthetic Pi session;
- browser pairing and authenticated batch ingestion;
- duplicate replay;
- activity summary and unknown API shapes;
- rule creation, historical relabel, and aggregate change;
- midnight-crossing interval;
- server under concurrent battery, agent indexer, browser, and relabel writes;
- representative one-year synthetic storage benchmark stays within the 100–250 MiB context budget and reports table/index contributions.

## J4. Manual acceptance matrix

Test on real Zen/Firefox and Ghostty/tmux:

- focused browser, inactive tab, minimized browser, another application focused;
- paused, playing, muted, background, fullscreen, and picture-in-picture video;
- YouTube SPA navigation;
- extension/browser/server restart;
- private browsing;
- tmux pane/window/session switch;
- one and multiple Ghostty windows;
- Pi startup, reload, new, resume, fork, clone, busy, settled, and crash;
- system suspend and midnight crossing;
- pairing revoke/re-pair;
- blocker warn/snooze/override.

# Plan K: observability and rollout safety

Expose source health without exposing sensitive values:

- context extractor version and last success/error category;
- tmux match rate and ambiguous count;
- labeler version/rules revision and relabel progress;
- percentage of observed time classified by dimension/confidence;
- Pi presence extension installed/last lease/expired count;
- browser client version/privacy mode/last event/queue depth;
- browser accepted/duplicate/rejected counts;
- interval compaction lag;
- policy revision and degraded enforcement status.

Add feature flags so each source can be enabled independently. Roll out in observe-only mode before any blocking.

Never make battery sampling depend on:

- tmux availability;
- Git resolution;
- Pi extension presence;
- browser connectivity;
- relabel completion;
- policy evaluation.

# Key product decisions to confirm before implementation

1. **Browser default:** recommend domains-only, no titles, no content IDs.
2. **YouTube default activity:** recommend `service=youtube` plus `content=video`, with `activity.primary=unknown` until a user rule decides entertainment/research.
3. **Terminal paths:** recommend basename plus stable local hash.
4. **Tmux names:** recommend storing session/window names; pane title off or truncated by default because applications can set arbitrary titles.
5. **Pi integration:** recommend optional global Pi extension using runtime lease files, with process/CWD fallback.
6. **Browser transport:** recommend authenticated loopback HTTP for browser portability.
7. **Blocker metric:** recommend focused, visible media playback rather than domain focus or audible playback alone.
8. **Enforcement rollout:** recommend observe → warn → soft block; hard block remains opt-in.
9. **Raw browser retention:** recommend never persisting unchanged heartbeats, retaining transitions for 30–90 days, and retaining normalized intervals for 365 days.
10. **Manual correction model:** recommend corrections generate previewable reusable rules, while one-off interval overrides remain possible.

## Definition of success

The system is successful when a future query can accurately ask:

```text
How much verified entertainment playback followed work on each dynamically
identified project, including breaks that crossed midnight, grouped by exact
Pi session when available, normalized by project exposure, with confidence
and monitoring coverage?
```

—and answer it without adding a project name to source code, parsing raw titles in ad hoc SQL, double-counting browser time, or collecting private work/conversation content.
