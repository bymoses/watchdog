# Unified agent usage chart implementation plan

## Goal

Add one dashboard chart that shows cumulative coding-agent token usage and cost over time, aggregated by session.

Build the feature around a normalized agent telemetry interface so Pi is only the first adapter. Adding another agent later must not require a new chart, API shape, or set of core database tables.

The implementation must:

- import existing Pi session history;
- update while sessions are running;
- avoid double-counting history copied by fork/clone;
- preserve token and cost breakdowns when the source provides them;
- support sources with incomplete token or cost information;
- store no prompt, response, tool argument, or tool result content;
- remain useful without installing a Pi extension;
- make a second agent an adapter-level change rather than a dashboard rewrite.

## Architecture

Use three layers:

```text
Agent adapters
    ↓ normalized sessions and usage events
Agent telemetry store and aggregation API
    ↓ agent-neutral response
Unified Agent usage chart
```

### Agent adapter responsibilities

Each adapter owns source-specific behavior:

- discovery of local files, databases, sockets, or extension events;
- parsing native session formats;
- generating stable native event keys;
- identifying copied/inherited events;
- mapping native token and cost semantics into the normalized contract;
- maintaining incremental scan state;
- reporting source capabilities and limitations.

### Core telemetry responsibilities

The core owns behavior shared by every agent:

- normalized persistence;
- global/session aggregation;
- range bucketing;
- top-session and `Other` grouping;
- agent filtering;
- chart API;
- privacy policy;
- retention;
- indexer status and errors.

### Frontend responsibilities

The frontend only consumes the normalized API. It must not know where Pi stores JSONL files or how a particular agent represents usage.

## Normalized telemetry contract

Add `src/agent_telemetry.rs` for shared types and orchestration, with adapters under `src/agent_adapters/`.

Suggested Rust interface:

```rust
pub(crate) trait AgentTelemetryAdapter: Send + Sync {
    fn descriptor(&self) -> AgentDescriptor;
    fn discover(&self) -> anyhow::Result<Vec<AgentSource>>;
    fn scan(
        &self,
        source: &AgentSource,
        state: Option<&AgentScanState>,
        sink: &mut dyn AgentTelemetrySink,
    ) -> anyhow::Result<AgentScanResult>;
}

pub(crate) trait AgentTelemetrySink {
    fn upsert_session(&mut self, session: NormalizedAgentSession) -> anyhow::Result<()>;
    fn upsert_usage(&mut self, event: NormalizedUsageEvent) -> anyhow::Result<()>;
    fn add_session_event_ref(&mut self, event_ref: SessionUsageRef) -> anyhow::Result<()>;
}
```

The exact borrowing model can change during implementation, but adapters must emit normalized records rather than writing adapter-specific tables directly.

### Agent descriptor

```rust
pub(crate) struct AgentDescriptor {
    pub(crate) agent_type: String,       // stable machine ID, e.g. "pi"
    pub(crate) display_name: String,     // e.g. "Pi"
    pub(crate) adapter_version: u32,
    pub(crate) capabilities: AgentCapabilities,
    pub(crate) limitations: Vec<String>,
}

pub(crate) struct AgentCapabilities {
    pub(crate) historical_import: bool,
    pub(crate) live_updates: bool,
    pub(crate) input_tokens: bool,
    pub(crate) output_tokens: bool,
    pub(crate) reasoning_tokens: bool,
    pub(crate) cache_tokens: bool,
    pub(crate) cost: bool,
    pub(crate) session_forks: bool,
}
```

Capabilities let the API and UI distinguish zero from unavailable data.

### Normalized session

```rust
pub(crate) struct NormalizedAgentSession {
    pub(crate) agent_type: String,
    pub(crate) source_key: String,
    pub(crate) native_session_id: String,
    pub(crate) parent_native_session_id: Option<String>,
    pub(crate) parent_native_path: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) project_path: Option<String>,
    pub(crate) created_ts: i64,
    pub(crate) first_activity_ts: Option<i64>,
    pub(crate) last_activity_ts: Option<i64>,
}
```

The database assigns an internal session ID. Native IDs are only unique within an agent source.

### Normalized usage event

```rust
pub(crate) struct NormalizedUsageEvent {
    pub(crate) agent_type: String,
    pub(crate) source_key: String,
    pub(crate) native_event_key: String,
    pub(crate) native_entry_id: Option<String>,
    pub(crate) origin_native_session_id: Option<String>,
    pub(crate) event_ts: i64,
    pub(crate) provider: Option<String>,
    pub(crate) api: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) usage: NormalizedTokenUsage,
    pub(crate) cost: Option<NormalizedCost>,
}

pub(crate) struct NormalizedTokenUsage {
    pub(crate) input: Option<i64>,
    pub(crate) output: Option<i64>,
    pub(crate) reasoning: Option<i64>,
    pub(crate) cache_read: Option<i64>,
    pub(crate) cache_write: Option<i64>,
    pub(crate) total: i64,
}

pub(crate) struct NormalizedCost {
    pub(crate) input: Option<f64>,
    pub(crate) output: Option<f64>,
    pub(crate) cache_read: Option<f64>,
    pub(crate) cache_write: Option<f64>,
    pub(crate) total: f64,
    pub(crate) currency: String,          // initially "USD"
    pub(crate) kind: CostKind,            // Reported or Estimated
}
```

Normalized semantics:

- `total` is the source-reported total when available, otherwise the adapter computes it from non-overlapping components;
- reasoning is always treated as a subset of output;
- unavailable components are `None`, not zero;
- unavailable cost is `None`, not a zero-cost event;
- zero is preserved when an agent explicitly reports zero;
- currencies are never summed together.

### Stable source and event identity

`agent_type` identifies the adapter, such as `pi`.

`source_key` identifies one logical local installation/account source. Multiple roots belonging to the same local Pi installation should use the same source key so cross-root forks can deduplicate.

The globally unique database key is a 32-byte SHA-256 value derived from:

```text
agent_type + source_key + native_event_key
```

Each adapter defines `native_event_key` according to its source guarantees. The native key exists only while importing; persist the compact 32-byte hash as a SQLite `BLOB`, not response IDs or 64-character hex strings.

## Product decisions

### Chart

Add an `Agent usage` panel below the power timeline and above the process tabs.

The panel contains one canvas and:

- agent filter: `All agents` plus every discovered agent type;
- range selector: `24h`, `7d`, `30d`, `All`;
- primary metric toggle: `Tokens` / `Cost`;
- cumulative stacked areas for the top sessions plus `Other`;
- a total overlay line for the other metric when available;
- summary values for total tokens, cost, response count, and cache hit rate;
- a session legend with an agent badge and stable session color;
- a hover tooltip showing cumulative token and cost totals plus per-session values.

In `Tokens` mode:

- left axis and stacked areas: cumulative total tokens by session;
- right axis and line: cumulative cost, when cost is available for the selected sources.

In `Cost` mode:

- left axis and stacked areas: cumulative cost by session;
- right axis and line: cumulative total tokens.

Disable Cost mode with an explanatory label when none of the selected agents provide cost. If only some selected agents provide cost, clearly show cost coverage rather than presenting the value as complete.

The accumulation starts at zero at the selected range boundary. `All` starts at the earliest retained event.

Use at most eight named sessions in the stack. Aggregate the remainder into `Other`. Rank sessions by the selected primary metric.

### Agent and session labels

Session labels use this priority:

1. explicit session name;
2. project directory basename plus short native session ID;
3. agent display name plus short native session ID.

Every legend and tooltip entry includes the agent display name or badge, for example `Pi · watchdog · 019f56a1`.

Do not use the first prompt as a fallback label.

### Token semantics

Retain all available components independently for summaries and tooltips.

Important display rules:

- reasoning is a subset of output and must not be added to output again;
- total tokens must not be stacked alongside its token components;
- cache hit rate is only shown for sources with cache telemetry;
- the normalized cache hit calculation is:

```text
cacheRead / (input + cacheRead + cacheWrite)
```

The Pi adapter uses Pi's `totalTokens` directly and maps its complete component breakdown.

### Cost semantics

Cost is agent/source dependent. Store:

- component breakdown when available;
- total;
- currency;
- whether the source reports or estimates the amount.

The first version supports display aggregation for USD. If another currency appears, return separate currency totals and require a currency filter rather than converting silently.

The Pi adapter marks catalog-derived Pi cost as `Estimated` and uses USD.

## Pi adapter: first implementation

Add `src/agent_adapters/pi.rs`.

### Discovery

Read Pi JSONL sessions from configurable roots.

Default resolution order:

1. explicit repeated `--pi-sessions-dir <DIR>` options;
2. `PI_CODING_AGENT_SESSION_DIR`;
3. `$PI_CODING_AGENT_DIR/sessions`;
4. `~/.pi/agent/sessions`.

Add `--no-agent-tracking` as the global off switch. Pi-specific discovery can additionally be omitted by providing no valid Pi roots; do not introduce separate UI behavior for disabled Pi tracking.

### Mapping

Relevant Pi JSONL entries:

- `session`: native ID, creation timestamp, cwd, parent session path;
- `session_info`: current display name;
- assistant `message`: model, provider, response ID, stop reason, usage, and cost;
- `model_change`, `thinking_level_change`, and `compaction`: ignored by the first chart but available for future normalized event types.

Use the session-entry timestamp as the usage event time because it is written when the finalized message is appended.

Pi token mapping:

```text
input       ← usage.input
output      ← usage.output
reasoning   ← usage.reasoning (optional and already included in output)
cache read  ← usage.cacheRead
cache write ← usage.cacheWrite
total       ← usage.totalTokens
```

Pi cost mapping uses `usage.cost.*`, currency USD, and kind `Estimated`.

### Pi event deduplication

Pi fork/clone operations copy prior entries into a new file. A per-file sum is incorrect.

Create the Pi `native_event_key` as:

1. when `responseId` exists: SHA-256 of `provider + "\0" + responseId`;
2. otherwise: SHA-256 of a canonical serialization of the assistant message.

Only the hash is retained. Do not persist assistant content used by the fallback hash.

Preserve a mapping between a usage event and every session containing it. The canonical event is counted once and attributed to its origin session.

Origin resolution:

- if the same event already exists, retain its existing origin;
- if an entry exists in the parent session, inherit the parent's origin;
- otherwise, an entry timestamp older than the child session header indicates copied history;
- newly observed non-inherited entries originate in the current session;
- reconcile inferred origins if a parent file is indexed later.

Pi entry IDs are stored for diagnostics but are not the global key because they are short and collision-checked only within a session.

All responses generated inside one Pi session count toward it, including abandoned in-file branches. Active-branch-only accounting is out of scope for the first version.

### Pi limitation

Pi persists normal finalized assistant usage, which is what its footer sums. Internal compaction and branch-summary calls invoke the model separately and currently discard their returned usage.

Expose this through the Pi adapter descriptor:

```text
Internal compaction and branch-summary model usage may not be recorded by Pi.
```

The unified UI displays source limitations without hard-coding Pi-specific text.

## Database migration

Add `migrations/003_agent_usage.sql`.

Use only agent-neutral core tables:

```sql
CREATE TABLE agent_sources (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_type TEXT NOT NULL,
  source_key TEXT NOT NULL,
  display_name TEXT NOT NULL,
  adapter_version INTEGER NOT NULL,
  capabilities_json TEXT NOT NULL,
  limitations_json TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  last_scan_ts INTEGER,
  last_scan_error TEXT,
  UNIQUE(agent_type, source_key)
);

CREATE TABLE agent_sessions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id INTEGER NOT NULL,
  native_session_id TEXT NOT NULL,
  parent_native_session_id TEXT,
  parent_native_path TEXT,
  name TEXT,
  project_path TEXT,
  created_ts INTEGER NOT NULL,
  first_activity_ts INTEGER,
  last_activity_ts INTEGER,
  FOREIGN KEY(source_id) REFERENCES agent_sources(id),
  UNIQUE(source_id, native_session_id)
);

CREATE TABLE agent_usage_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id INTEGER NOT NULL,
  event_key BLOB NOT NULL,
  origin_session_id INTEGER,
  event_ts INTEGER NOT NULL,
  provider TEXT,
  api TEXT,
  model TEXT,
  stop_reason TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  reasoning_tokens INTEGER,
  cache_read_tokens INTEGER,
  cache_write_tokens INTEGER,
  total_tokens INTEGER NOT NULL,
  input_cost REAL,
  output_cost REAL,
  cache_read_cost REAL,
  cache_write_cost REAL,
  total_cost REAL,
  cost_currency TEXT,
  cost_kind TEXT,
  FOREIGN KEY(source_id) REFERENCES agent_sources(id),
  FOREIGN KEY(origin_session_id) REFERENCES agent_sessions(id),
  UNIQUE(source_id, event_key)
);

CREATE TABLE agent_session_usage_refs (
  session_id INTEGER NOT NULL,
  native_entry_id TEXT NOT NULL,
  usage_event_id INTEGER NOT NULL,
  inherited INTEGER NOT NULL,
  PRIMARY KEY(session_id, native_entry_id),
  FOREIGN KEY(session_id) REFERENCES agent_sessions(id),
  FOREIGN KEY(usage_event_id) REFERENCES agent_usage_events(id)
);

CREATE TABLE agent_scan_files (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id INTEGER NOT NULL,
  adapter_file_key TEXT NOT NULL,
  native_session_id TEXT,
  file_path TEXT NOT NULL,
  file_size INTEGER NOT NULL DEFAULT 0,
  file_mtime_ms INTEGER NOT NULL DEFAULT 0,
  scan_offset INTEGER NOT NULL DEFAULT 0,
  checkpoint_offset INTEGER,
  checkpoint_hash TEXT,
  last_scanned_ts INTEGER,
  last_error TEXT,
  FOREIGN KEY(source_id) REFERENCES agent_sources(id),
  UNIQUE(source_id, adapter_file_key)
);

CREATE INDEX idx_agent_usage_ts ON agent_usage_events(event_ts);
CREATE INDEX idx_agent_usage_origin_ts ON agent_usage_events(origin_session_id, event_ts);
CREATE INDEX idx_agent_usage_source_ts ON agent_usage_events(source_id, event_ts);
CREATE INDEX idx_agent_refs_event ON agent_session_usage_refs(usage_event_id);
CREATE INDEX idx_agent_sessions_activity ON agent_sessions(last_activity_ts);
CREATE INDEX idx_agent_scan_source ON agent_scan_files(source_id);
```

Adapter-specific scan state belongs in `agent_scan_files` or a versioned opaque state field added later, not in usage/session tables.

Integer token fields use 64-bit Rust/SQLite values. Costs use `f64`/SQLite `REAL` as source telemetry, not financial-ledger precision.

Do not persist native response IDs. The adapter uses them to construct `event_key`, after which they add no aggregation value and would materially increase long-term storage.

## Storage budget

The local Pi corpus currently contains approximately:

- 544 session files and 821 MiB of source JSONL;
- 42.9k assistant rows over 67.8 days;
- 21.0k assistant rows in the latest 14 days;
- 19.2k canonical usage events in the latest 14 days after fork/copy deduplication;
- 1.9k copied session references in that period;
- 184 newly created and 193 active sessions in that period.

The watchdog database stores normalized metadata only, not the 821 MiB of conversation content. With 32-byte event keys, no response IDs, 64-bit token counters, cost component fields, references, and indexes, budget approximately:

- 350–500 bytes per canonical usage event including indexes;
- 60–100 bytes per session-event reference including indexes;
- 300–700 bytes per session/scan-file pair, mainly paths and indexes;
- 10–20% SQLite page and free-space overhead.

At the observed recent high-volume rate, expected additional persistent database size is:

| Retained period | Expected addition |
| --- | ---: |
| 14 days | 9–13 MiB |
| 30 days | 20–28 MiB |
| 90 days | 60–85 MiB |
| 1 year | 240–340 MiB |
| 2 years | 480–680 MiB |
| 5 years | 1.2–1.7 GiB |

The full 67.8-day historical average is about 2.3 times lighter than the latest 14 days. At that average, one year would be roughly 100–150 MiB and five years roughly 0.5–0.75 GiB.

Treat the recent-rate column as the capacity-planning case. Actual growth is driven mainly by finalized model responses, not token count: a 1k-token and 100k-token response occupy almost the same telemetry space.

Cost support adds roughly 40–60 bytes per canonical event for component values and record metadata, or around 1 MiB per 14 days at the current rate. It is small relative to event identity and indexes.

WAL may temporarily add another 10–50 MiB during initial import or a large rescan. Run a passive checkpoint after import batches; do not run `VACUUM` in the collection loop.

Add a dedicated `--agent-retention-days` setting rather than coupling telemetry to detailed process retention. Recommended default: 365 days. Support `0` for unlimited retention. Pruning must delete old usage references/events, remove sessions with no retained usage only when their scan state is no longer needed, and leave source JSONL untouched.

## Import orchestration

The unified telemetry runner discovers all registered adapters and scans their sources.

Responsibilities:

1. register compiled-in adapters;
2. discover sources;
3. create/update `agent_sources` descriptors;
4. invoke adapters independently so one failure does not stop others;
5. batch normalized writes in SQLite transactions;
6. publish per-source status to the API;
7. keep previously imported telemetry if a native source is deleted;
8. apply shared retention without knowing native formats.

Run ingestion on a dedicated blocking task and separate SQLite connection so large historical imports do not hold the main `App` mutex or block API requests. WAL mode permits concurrent readers and a writer.

Scan on startup and every 30 seconds. The dashboard can show `Indexing agent sessions…` until initial discovery completes.

### Pi file scanner

The Pi adapter must:

1. discover `*.jsonl` files recursively beneath configured roots;
2. read headers first so parent relationships can be resolved;
3. process parent files before children when possible;
4. stream lines with `BufRead` rather than loading files into memory;
5. parse and emit only normalized metadata;
6. continue from the saved scan offset for appends;
7. detect truncation/rewrite and rebuild references for that native session;
8. skip incomplete trailing lines and retry them on the next scan.

Use a checkpoint line offset/hash for rewrite detection. If the file shrinks, inode changes, or the checkpoint no longer matches, clear that file/session's references and rescan. Garbage-collect unreferenced canonical events afterward.

## Unified API

Add:

```text
GET /api/agents/usage?range=24h|7d|30d|all&metric=tokens|cost&agent=all|pi&top=8
```

The endpoint and response remain unchanged when new adapters are added.

Suggested response:

```json
{
  "range": "7d",
  "metric": "tokens",
  "agentFilter": "all",
  "fromTs": 1710000000000,
  "toTs": 1710600000000,
  "bucketMs": 3600000,
  "agents": [
    {
      "type": "pi",
      "displayName": "Pi",
      "capabilities": {
        "cost": true,
        "cacheTokens": true,
        "reasoningTokens": true
      },
      "limitations": [
        "Internal compaction and branch-summary model usage may not be recorded by Pi"
      ],
      "indexer": {
        "enabled": true,
        "initialScanComplete": true,
        "lastScanTs": 1710600000000,
        "files": 544,
        "error": null
      }
    }
  ],
  "coverage": {
    "tokenEvents": 42,
    "costEvents": 40,
    "cacheEvents": 42,
    "costComplete": false,
    "currencies": ["USD"]
  },
  "summary": {
    "totalTokens": 123456,
    "inputTokens": 12000,
    "outputTokens": 3456,
    "reasoningTokens": 800,
    "cacheReadTokens": 108000,
    "cacheWriteTokens": 0,
    "totalCost": 1.2345,
    "costCurrency": "USD",
    "costKind": "estimated",
    "responses": 42,
    "cacheHitRate": 0.9
  },
  "sessions": [
    {
      "id": 123,
      "agentType": "pi",
      "agentDisplayName": "Pi",
      "nativeId": "session-id",
      "label": "watchdog · 019f56a1",
      "projectPath": "/path/to/project",
      "totalTokens": 100000,
      "totalCost": 1.1,
      "costCurrency": "USD",
      "responses": 30
    }
  ],
  "points": [
    {
      "ts": 1710000000000,
      "totalTokens": 1000,
      "totalCost": 0.01,
      "sessions": {
        "123": { "tokens": 1000, "cost": 0.01 }
      }
    }
  ]
}
```

Nullable summary component values mean unavailable, while numeric zero means measured zero.

`points` are cumulative from `fromTs`, not bucket deltas. Include an initial zero point at `fromTs` and final point at `toTs`.

Automatic buckets:

- 24h: 15 minutes;
- 7d: 1 hour;
- 30d: 6 hours;
- All: choose day/week intervals that cap output near 500 points.

The backend selects top sessions by the requested primary metric and aggregates the remainder under `__other__`.

When `agent=all`, session aggregation crosses all enabled sources. When a specific agent is selected, filter by `agent_type` before ranking and aggregation.

## Frontend changes

### `public/index.html`

Add the unified Agent usage panel with:

- heading and source/indexing freshness status;
- agent filter;
- summary metrics and coverage indicator;
- range and metric controls;
- `<canvas id="agentUsageChart">`;
- `<div id="agentUsageLegend">`;
- source limitation notes rendered from API data.

### `public/assets/api.js`

Add `fetchAgentUsage({ range, metric, agent, top })`.

### `public/assets/agent-usage-chart.js`

Keep the chart isolated from the power timeline and from individual agents.

Implement:

- DPR-aware canvas setup;
- stable colors based on source/session IDs;
- agent badges sourced from normalized metadata;
- stacked cumulative areas;
- secondary total line and right axis;
- compact token and currency formatting;
- coverage-aware missing-cost state;
- hover crosshair and binary-search point selection;
- tooltip with both metrics and session/agent breakdown;
- resize redraw;
- loading, empty, indexing, partial-data, and error states.

Do not import a Pi-specific module or inspect `agentType` except to display API-provided labels.

Do not add brush zoom in the first version; the dedicated range selector is sufficient.

### `public/assets/app.js`

Fetch agent usage independently from battery/process requests. A telemetry API or adapter failure must not prevent the rest of the dashboard from rendering.

Refresh every 30 seconds and refetch after range, metric, or agent-filter changes. The bounded response does not need incremental browser merging initially.

### `public/assets/styles.css`

Add styles for:

- Agent usage panel header and summary strip;
- agent filter and segmented metric control;
- 380–440px chart canvas;
- agent badges, legend, and tooltip rows;
- indexing, coverage, and limitation notes;
- responsive summary wrapping;
- dark mode and reduced motion.

## Adding another agent later

A new agent should require:

1. one `AgentTelemetryAdapter` implementation;
2. adapter registration;
3. source-specific configuration flags, if discovery cannot be automatic;
4. adapter parsing/deduplication tests;
5. one descriptor defining capabilities and limitations.

It should not require:

- new usage/session tables;
- a new aggregation endpoint;
- a new chart module;
- agent-specific HTML;
- frontend token/cost normalization.

If an agent supports live extension events but no historical files, its adapter can consume a local spool or normalized ingestion endpoint and emit the same records. A future write endpoint should accept the normalized contract and authenticate local producers; it remains out of scope for the initial Pi file importer.

## Privacy and security

The normalized contract intentionally excludes content.

Persist none of the following from any adapter:

- user prompts;
- assistant text or thinking text;
- images;
- tool arguments or results;
- shell commands;
- error messages from model content;
- raw provider headers.

Project path and explicit session name are the only potentially identifying normalized fields. A later global redaction option can hash project paths and suppress names consistently across adapters.

Until then, retain the existing localhost default and document that external binding exposes project/session metadata.

## Implementation sequence

1. Add normalized Rust contracts and adapter registry.
2. Add agent-neutral migration and query helpers.
3. Add global config/CLI controls and Pi root resolution.
4. Implement the Pi adapter, deduplication, incremental scanning, and tests.
5. Start unified import orchestration as an independent background task.
6. Add `/api/agents/usage` aggregation and API tests.
7. Add the agent-neutral HTML panel and chart module.
8. Add formatting, tooltip, filters, coverage, and source states.
9. Add end-to-end fixtures for history import and fork deduplication.
10. Update README with adapter architecture, flags, privacy, and accounting limitations.

## Test plan

### Normalized core

- missing components remain unavailable rather than becoming zero;
- reasoning is not counted twice;
- currencies are not mixed;
- event identity is namespaced by source;
- two agents can have the same native session/event ID without collision;
- one failing adapter does not stop another;
- top sessions plus `Other` equal summary totals;
- range points are cumulative and monotonic;
- 64-bit token totals do not overflow.

### Pi adapter

- parse session header and assistant usage;
- preserve optional reasoning and cache-write-1h data where mapped;
- malformed lines do not abort a scan;
- incomplete trailing line is retried;
- incremental append imports only new lines;
- rewrite/truncation rebuilds one session;
- response ID deduplicates copied entries;
- fallback hash deduplicates entries without response ID;
- copied fork entries are inherited and child-only responses belong to the child;
- cache hit calculation matches Pi;
- no content is retained in normalized records.

### API

- empty source registry returns a valid empty response;
- multiple agents are returned through the same schema;
- `agent` filter works before ranking;
- range and top limits are clamped;
- token and cost totals equal canonical events;
- partial cost coverage is explicit;
- Cost mode can be disabled when cost coverage is zero;
- session labels follow the documented priority;
- `all` stays near the point cap.

### End-to-end

Create temporary Pi parent/child JSONL fixtures where the child copies parent entries and adds one response. Verify:

- global total counts copied usage once;
- parent receives parent usage;
- child receives only newly generated usage;
- `/api/agents/usage` reports agent type `pi` and both sessions;
- filtering `agent=pi` preserves totals;
- dashboard HTML and generic chart module are served;
- no Pi-specific chart or endpoint exists.

Add a synthetic second adapter fixture in core tests to prove IDs, missing cost, and aggregation work without changing the API/UI.

## Acceptance criteria

- Existing Pi history appears after the initial background scan.
- New finalized Pi responses appear within one dashboard refresh interval.
- Forked/cloned history is not counted twice globally or against a child session.
- Token and cost totals match deduplicated normalized source records.
- Switching Tokens/Cost changes the session stack and retains the other metric when available.
- The tooltip shows both cumulative tokens and cumulative cost with coverage status.
- Agent filtering and labels are driven entirely by API metadata.
- Adding a test agent requires no database, endpoint, or chart changes.
- No conversation or tool content is written to the watchdog database.
- Battery/process collection and APIs continue working when tracking is disabled, unavailable, indexing, malformed, or one adapter fails.
