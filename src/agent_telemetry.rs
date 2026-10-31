use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{agent_adapters::pi::PiAdapter, config::Config, db::init_db, util::now_ms};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentDescriptor {
    #[serde(rename = "type")]
    pub(crate) agent_type: String,
    pub(crate) display_name: String,
    pub(crate) adapter_version: u32,
    pub(crate) capabilities: AgentCapabilities,
    pub(crate) limitations: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentSource {
    pub(crate) source_key: String,
    pub(crate) roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AgentScanState {
    pub(crate) file_key: String,
    pub(crate) scan_offset: u64,
    pub(crate) checkpoint_offset: Option<u64>,
    pub(crate) checkpoint_hash: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentScanResult {
    pub(crate) files: usize,
    pub(crate) events: usize,
    pub(crate) malformed_lines: usize,
}

pub(crate) trait AgentTelemetryAdapter: Send + Sync {
    fn descriptor(&self) -> AgentDescriptor;
    fn discover(&self) -> Result<Vec<AgentSource>>;
    fn scan(
        &self,
        source: &AgentSource,
        state: Option<&AgentScanState>,
        sink: &mut dyn AgentTelemetrySink,
    ) -> Result<AgentScanResult>;
    fn scan_persisted(
        &self,
        _db: &mut Connection,
        _source_id: i64,
        source: &AgentSource,
        _descriptor: &AgentDescriptor,
    ) -> Result<AgentScanResult> {
        self.scan(source, None, &mut NoopTelemetrySink)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NormalizedAgentSession {
    pub(crate) native_session_id: String,
    pub(crate) parent_native_session_id: Option<String>,
    pub(crate) parent_native_path: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) project_path: Option<String>,
    pub(crate) created_ts: i64,
    pub(crate) first_activity_ts: Option<i64>,
    pub(crate) last_activity_ts: Option<i64>,
}

#[derive(Debug, Clone)]
pub(crate) struct NormalizedTokenUsage {
    pub(crate) input: Option<i64>,
    pub(crate) output: Option<i64>,
    pub(crate) reasoning: Option<i64>,
    pub(crate) cache_read: Option<i64>,
    pub(crate) cache_write: Option<i64>,
    pub(crate) total: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CostKind {
    #[allow(dead_code)]
    // Normalized adapters may report source-billed costs; Pi currently estimates.
    Reported,
    Estimated,
}

impl CostKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Estimated => "estimated",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NormalizedCost {
    pub(crate) input: Option<f64>,
    pub(crate) output: Option<f64>,
    pub(crate) cache_read: Option<f64>,
    pub(crate) cache_write: Option<f64>,
    pub(crate) total: f64,
    pub(crate) currency: String,
    pub(crate) kind: CostKind,
}

#[derive(Debug, Clone)]
pub(crate) struct NormalizedUsageEvent {
    pub(crate) native_event_key: Vec<u8>,
    pub(crate) event_ts: i64,
    pub(crate) provider: Option<String>,
    pub(crate) api: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) usage: NormalizedTokenUsage,
    pub(crate) cost: Option<NormalizedCost>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionUsageRef {
    pub(crate) session_id: i64,
    pub(crate) native_entry_id: String,
    pub(crate) usage_event_id: i64,
    pub(crate) inherited: bool,
}

pub(crate) trait AgentTelemetrySink {
    fn upsert_session(&mut self, session: NormalizedAgentSession) -> Result<i64>;
    fn upsert_usage(&mut self, event: NormalizedUsageEvent, origin_session_id: i64) -> Result<i64>;
    fn add_session_event_ref(&mut self, event_ref: SessionUsageRef) -> Result<()>;
}

pub(crate) struct SqliteTelemetrySink<'a> {
    tx: &'a Transaction<'a>,
    source_id: i64,
    agent_type: &'a str,
    source_key: &'a str,
}

impl<'a> SqliteTelemetrySink<'a> {
    pub(crate) fn new(
        tx: &'a Transaction<'a>,
        source_id: i64,
        agent_type: &'a str,
        source_key: &'a str,
    ) -> Self {
        Self {
            tx,
            source_id,
            agent_type,
            source_key,
        }
    }
}

impl AgentTelemetrySink for SqliteTelemetrySink<'_> {
    fn upsert_session(&mut self, session: NormalizedAgentSession) -> Result<i64> {
        // Avoid even the AUTOINCREMENT sequence write on a no-op conflict.
        if let Some(id) = self.tx.query_row(
            "SELECT id FROM agent_sessions WHERE source_id=?1 AND native_session_id=?2
             AND parent_native_session_id IS COALESCE(?3,parent_native_session_id)
             AND parent_native_path IS COALESCE(?4,parent_native_path)
             AND name IS COALESCE(?5,name) AND project_path IS COALESCE(?6,project_path)
             AND created_ts<=?7
             AND (?8 IS NULL OR first_activity_ts<=?8)
             AND (?9 IS NULL OR last_activity_ts>=?9)",
            params![self.source_id, session.native_session_id, session.parent_native_session_id,
                session.parent_native_path, session.name, session.project_path, session.created_ts,
                session.first_activity_ts, session.last_activity_ts],
            |row| row.get(0),
        ).optional()? { return Ok(id); }
        self.tx.execute(
            "INSERT INTO agent_sessions
             (source_id,native_session_id,parent_native_session_id,parent_native_path,name,project_path,created_ts,first_activity_ts,last_activity_ts)
             VALUES (?,?,?,?,?,?,?,?,?)
             ON CONFLICT(source_id,native_session_id) DO UPDATE SET
               parent_native_session_id=COALESCE(excluded.parent_native_session_id,agent_sessions.parent_native_session_id),
               parent_native_path=COALESCE(excluded.parent_native_path,agent_sessions.parent_native_path),
               name=COALESCE(excluded.name,agent_sessions.name),
               project_path=COALESCE(excluded.project_path,agent_sessions.project_path),
               created_ts=MIN(agent_sessions.created_ts,excluded.created_ts),
               first_activity_ts=CASE WHEN agent_sessions.first_activity_ts IS NULL THEN excluded.first_activity_ts WHEN excluded.first_activity_ts IS NULL THEN agent_sessions.first_activity_ts ELSE MIN(agent_sessions.first_activity_ts,excluded.first_activity_ts) END,
               last_activity_ts=CASE WHEN agent_sessions.last_activity_ts IS NULL THEN excluded.last_activity_ts WHEN excluded.last_activity_ts IS NULL THEN agent_sessions.last_activity_ts ELSE MAX(agent_sessions.last_activity_ts,excluded.last_activity_ts) END",
            params![
                self.source_id,
                session.native_session_id,
                session.parent_native_session_id,
                session.parent_native_path,
                session.name,
                session.project_path,
                session.created_ts,
                session.first_activity_ts,
                session.last_activity_ts,
            ],
        )?;
        Ok(self.tx.query_row(
            "SELECT id FROM agent_sessions WHERE source_id=? AND native_session_id=?",
            params![self.source_id, session.native_session_id],
            |row| row.get(0),
        )?)
    }

    fn upsert_usage(&mut self, event: NormalizedUsageEvent, origin_session_id: i64) -> Result<i64> {
        let event_key = event_identity(self.agent_type, self.source_key, &event.native_event_key);
        let cost = event.cost.as_ref();
        self.tx.execute(
            "INSERT INTO agent_usage_events
             (source_id,event_key,origin_session_id,event_ts,provider,api,model,stop_reason,input_tokens,output_tokens,reasoning_tokens,cache_read_tokens,cache_write_tokens,total_tokens,input_cost,output_cost,cache_read_cost,cache_write_cost,total_cost,cost_currency,cost_kind)
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
             ON CONFLICT(source_id,event_key) DO UPDATE SET
               origin_session_id=COALESCE(agent_usage_events.origin_session_id,excluded.origin_session_id),
               event_ts=MIN(agent_usage_events.event_ts,excluded.event_ts),
               provider=COALESCE(agent_usage_events.provider,excluded.provider),
               api=COALESCE(agent_usage_events.api,excluded.api),
               model=COALESCE(agent_usage_events.model,excluded.model),
               stop_reason=COALESCE(agent_usage_events.stop_reason,excluded.stop_reason),
               input_tokens=COALESCE(agent_usage_events.input_tokens,excluded.input_tokens),
               output_tokens=COALESCE(agent_usage_events.output_tokens,excluded.output_tokens),
               reasoning_tokens=COALESCE(agent_usage_events.reasoning_tokens,excluded.reasoning_tokens),
               cache_read_tokens=COALESCE(agent_usage_events.cache_read_tokens,excluded.cache_read_tokens),
               cache_write_tokens=COALESCE(agent_usage_events.cache_write_tokens,excluded.cache_write_tokens),
               total_tokens=MAX(agent_usage_events.total_tokens,excluded.total_tokens),
               input_cost=COALESCE(agent_usage_events.input_cost,excluded.input_cost),
               output_cost=COALESCE(agent_usage_events.output_cost,excluded.output_cost),
               cache_read_cost=COALESCE(agent_usage_events.cache_read_cost,excluded.cache_read_cost),
               cache_write_cost=COALESCE(agent_usage_events.cache_write_cost,excluded.cache_write_cost),
               total_cost=COALESCE(agent_usage_events.total_cost,excluded.total_cost),
               cost_currency=COALESCE(agent_usage_events.cost_currency,excluded.cost_currency),
               cost_kind=COALESCE(agent_usage_events.cost_kind,excluded.cost_kind)",
            params![
                self.source_id,
                event_key.as_slice(),
                origin_session_id,
                event.event_ts,
                event.provider,
                event.api,
                event.model,
                event.stop_reason,
                event.usage.input,
                event.usage.output,
                event.usage.reasoning,
                event.usage.cache_read,
                event.usage.cache_write,
                event.usage.total,
                cost.and_then(|c| c.input),
                cost.and_then(|c| c.output),
                cost.and_then(|c| c.cache_read),
                cost.and_then(|c| c.cache_write),
                cost.map(|c| c.total),
                cost.map(|c| c.currency.as_str()),
                cost.map(|c| c.kind.as_str()),
            ],
        )?;
        Ok(self.tx.query_row(
            "SELECT id FROM agent_usage_events WHERE source_id=? AND event_key=?",
            params![self.source_id, event_key.as_slice()],
            |row| row.get(0),
        )?)
    }

    fn add_session_event_ref(&mut self, event_ref: SessionUsageRef) -> Result<()> {
        self.tx.execute(
            "INSERT INTO agent_session_usage_refs (session_id,native_entry_id,usage_event_id,inherited)
             VALUES (?,?,?,?) ON CONFLICT(session_id,native_entry_id) DO UPDATE SET
             usage_event_id=excluded.usage_event_id,inherited=excluded.inherited",
            params![
                event_ref.session_id,
                event_ref.native_entry_id,
                event_ref.usage_event_id,
                i64::from(event_ref.inherited),
            ],
        )?;
        Ok(())
    }
}

pub(crate) fn event_identity(agent_type: &str, source_key: &str, native_key: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(agent_type.as_bytes());
    hash.update([0]);
    hash.update(source_key.as_bytes());
    hash.update([0]);
    hash.update(native_key);
    hash.finalize().into()
}

pub(crate) struct AgentAdapterRegistry {
    adapters: Vec<Arc<dyn AgentTelemetryAdapter>>,
}

impl AgentAdapterRegistry {
    pub(crate) fn compiled(cfg: &Config) -> Self {
        Self {
            adapters: vec![Arc::new(PiAdapter::new(cfg.pi_sessions_dirs.clone()))],
        }
    }

    #[cfg(test)]
    fn with(adapters: Vec<Arc<dyn AgentTelemetryAdapter>>) -> Self {
        Self { adapters }
    }
}

pub(crate) fn spawn_agent_telemetry(db_path: PathBuf, cfg: Config) {
    if !cfg.agent_tracking {
        return;
    }
    tokio::spawn(async move {
        let mut last_cleanup = std::time::Instant::now() - Duration::from_secs(3600);
        let mut interval = tokio::time::interval(cfg.poll_interval);
        let mut last_scan = None::<std::time::Instant>;
        loop {
            interval.tick().await;
            let on_battery = crate::collect::on_battery_power(&cfg.power_root);
            let cadence = cfg.poll_interval.max(Duration::from_secs(if on_battery { 120 } else { 30 }));
            if last_scan.is_some_and(|last| last.elapsed() + Duration::from_millis(10) < cadence) { continue; }
            last_scan = Some(std::time::Instant::now());
            let cleanup = last_cleanup.elapsed() >= Duration::from_secs(3600);
            let path = db_path.clone();
            let scan_cfg = cfg.clone();
            let result =
                tokio::task::spawn_blocking(move || scan_registered_adapters(&path, &scan_cfg, cleanup))
                    .await;
            match result {
                Ok(Ok(())) => { if cleanup { last_cleanup = std::time::Instant::now(); } }
                Ok(Err(err)) => eprintln!("[bms-watchdog] agent telemetry scan failed: {err:#}"),
                Err(err) => eprintln!("[bms-watchdog] agent telemetry worker failed: {err}"),
            }
        }
    });
}

fn scan_registered_adapters(db_path: &Path, cfg: &Config, cleanup: bool) -> Result<()> {
    let mut db = Connection::open(db_path)
        .with_context(|| format!("open telemetry database {}", db_path.display()))?;
    init_db(&mut db)?;
    let registry = AgentAdapterRegistry::compiled(cfg);
    for adapter in registry.adapters {
        let descriptor = adapter.descriptor();
        let sources = match adapter.discover() {
            Ok(sources) => sources,
            Err(err) => {
                eprintln!(
                    "[bms-watchdog] {} discovery failed: {err:#}",
                    descriptor.display_name
                );
                continue;
            }
        };
        for source in sources {
            let source_id = ensure_source(&db, &descriptor, &source.source_key)?;
            let result = adapter.scan_persisted(&mut db, source_id, &source, &descriptor);
            let ts = now_ms();
            match result {
                Ok(scan) => {
                    db.execute(
                        "UPDATE agent_sources SET initial_scan_complete=1,last_scan_ts=?,last_scan_error=NULL,enabled=1 WHERE id=?",
                        params![ts, source_id],
                    )?;
                    if scan.events > 0 || scan.malformed_lines > 0 {
                        println!(
                            "[bms-watchdog] indexed {}: files={} events={} malformed={}",
                            descriptor.display_name, scan.files, scan.events, scan.malformed_lines
                        );
                    }
                }
                Err(err) => {
                    db.execute(
                        "UPDATE agent_sources SET last_scan_ts=?,last_scan_error=? WHERE id=?",
                        params![ts, err.to_string(), source_id],
                    )?;
                    eprintln!(
                        "[bms-watchdog] {} scan failed: {err:#}",
                        descriptor.display_name
                    );
                }
            }
        }
    }
    if cleanup {
        prune_agent_telemetry(&db, cfg.agent_retention_days, now_ms())?;
        db.execute_batch("PRAGMA wal_checkpoint(PASSIVE);")?;
    }
    Ok(())
}

struct NoopTelemetrySink;
impl AgentTelemetrySink for NoopTelemetrySink {
    fn upsert_session(&mut self, _session: NormalizedAgentSession) -> Result<i64> {
        Ok(0)
    }
    fn upsert_usage(
        &mut self,
        _event: NormalizedUsageEvent,
        _origin_session_id: i64,
    ) -> Result<i64> {
        Ok(0)
    }
    fn add_session_event_ref(&mut self, _event_ref: SessionUsageRef) -> Result<()> {
        Ok(())
    }
}

fn ensure_source(db: &Connection, descriptor: &AgentDescriptor, source_key: &str) -> Result<i64> {
    db.execute(
        "INSERT INTO agent_sources (agent_type,source_key,display_name,adapter_version,capabilities_json,limitations_json,enabled)
         VALUES (?,?,?,?,?,?,1) ON CONFLICT(agent_type,source_key) DO UPDATE SET
         display_name=excluded.display_name,adapter_version=excluded.adapter_version,
         capabilities_json=excluded.capabilities_json,limitations_json=excluded.limitations_json,enabled=1",
        params![
            descriptor.agent_type,
            source_key,
            descriptor.display_name,
            descriptor.adapter_version,
            serde_json::to_string(&descriptor.capabilities)?,
            serde_json::to_string(&descriptor.limitations)?,
        ],
    )?;
    Ok(db.query_row(
        "SELECT id FROM agent_sources WHERE agent_type=? AND source_key=?",
        params![descriptor.agent_type, source_key],
        |row| row.get(0),
    )?)
}

pub(crate) fn prune_agent_telemetry(db: &Connection, retention_days: i64, now: i64) -> Result<()> {
    if retention_days == 0 {
        return Ok(());
    }
    let cutoff = now - retention_days.max(1) * 86_400_000;
    db.execute(
        "DELETE FROM agent_session_usage_refs WHERE usage_event_id IN (SELECT id FROM agent_usage_events WHERE event_ts < ?)",
        params![cutoff],
    )?;
    db.execute(
        "DELETE FROM agent_usage_events WHERE event_ts < ?",
        params![cutoff],
    )?;
    db.execute(
        "DELETE FROM agent_sessions WHERE id NOT IN (SELECT session_id FROM agent_session_usage_refs) AND native_session_id NOT IN (SELECT native_session_id FROM agent_scan_files WHERE native_session_id IS NOT NULL)",
        [],
    )?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(crate) struct AgentUsageQuery {
    pub(crate) range: Option<String>,
    // Retained for API compatibility; the chart is now always token-based.
    #[allow(dead_code)]
    pub(crate) metric: Option<String>,
    pub(crate) group: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) top: Option<usize>,
}

#[derive(Debug)]
struct UsageRow {
    ts: i64,
    session_id: Option<i64>,
    agent_type: String,
    agent_display_name: String,
    model: Option<String>,
    session_native_id: Option<String>,
    session_name: Option<String>,
    session_project_path: Option<String>,
    total_tokens: i64,
    input: Option<i64>,
    output: Option<i64>,
    reasoning: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    cost: Option<f64>,
    currency: Option<String>,
    cost_kind: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct SessionAggregate {
    tokens: i64,
    cost: f64,
    cost_events: usize,
    responses: i64,
}

#[derive(Debug, Default, Clone)]
struct DistributionAggregate {
    label: String,
    badge: String,
    tokens: i64,
    responses: i64,
}

pub(crate) fn query_agent_usage(db: &Connection, q: AgentUsageQuery) -> Result<Value> {
    let range = match q.range.as_deref() {
        Some("24h") => "24h",
        Some("30d") => "30d",
        Some("all") => "all",
        _ => "7d",
    };
    let group_by = match q.group.as_deref() {
        Some("model") => "model",
        Some("session") => "session",
        _ => "harness",
    };
    let agent_filter = q
        .agent
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "all".to_string());
    let top = q.top.unwrap_or(8).clamp(1, 8);
    let to_ts = now_ms();
    let fixed_from = match range {
        "24h" => Some(to_ts - 24 * 3_600_000),
        "7d" => Some(to_ts - 7 * 86_400_000),
        "30d" => Some(to_ts - 30 * 86_400_000),
        _ => None,
    };
    let earliest: Option<i64> = db.query_row(
        "SELECT MIN(e.event_ts) FROM agent_usage_events e JOIN agent_sources s ON s.id=e.source_id WHERE (?='all' OR s.agent_type=?)",
        params![agent_filter, agent_filter],
        |row| row.get(0),
    )?;
    let from_ts = fixed_from.unwrap_or_else(|| earliest.unwrap_or(to_ts));
    let bucket_ms = match range {
        "24h" => 15 * 60_000,
        "7d" => 3_600_000,
        "30d" => 6 * 3_600_000,
        _ => {
            let span = (to_ts - from_ts).max(1);
            let day = 86_400_000;
            let raw = ((span + 499) / 500).max(day);
            ((raw + day - 1) / day) * day
        }
    };

    // Agent metadata is intentionally unfiltered so the same response can drive
    // the agent selector even while event/session aggregation is filtered.
    let agents = query_agents(db)?;
    let mut stmt = db.prepare(
        "SELECT e.event_ts,e.origin_session_id,s.agent_type,s.display_name,e.model,se.native_session_id,se.name,se.project_path,e.total_tokens,e.input_tokens,e.output_tokens,e.reasoning_tokens,e.cache_read_tokens,e.cache_write_tokens,e.total_cost,e.cost_currency,e.cost_kind
         FROM agent_usage_events e JOIN agent_sources s ON s.id=e.source_id
         LEFT JOIN agent_sessions se ON se.id=e.origin_session_id
         WHERE e.event_ts>=? AND e.event_ts<=? AND (?='all' OR s.agent_type=?)
         ORDER BY e.event_ts,e.id",
    )?;
    let rows = stmt
        .query_map(params![from_ts, to_ts, agent_filter, agent_filter], |row| {
            Ok(UsageRow {
                ts: row.get(0)?,
                session_id: row.get(1)?,
                agent_type: row.get(2)?,
                agent_display_name: row.get(3)?,
                model: row.get(4)?,
                session_native_id: row.get(5)?,
                session_name: row.get(6)?,
                session_project_path: row.get(7)?,
                total_tokens: row.get(8)?,
                input: row.get(9)?,
                output: row.get(10)?,
                reasoning: row.get(11)?,
                cache_read: row.get(12)?,
                cache_write: row.get(13)?,
                cost: row.get(14)?,
                currency: row.get(15)?,
                cost_kind: row.get(16)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut by_session: HashMap<i64, SessionAggregate> = HashMap::new();
    let mut by_group: HashMap<String, DistributionAggregate> = HashMap::new();
    for row in &rows {
        if let Some(id) = row.session_id {
            let agg = by_session.entry(id).or_default();
            agg.tokens = agg.tokens.saturating_add(row.total_tokens);
            if row.currency.as_deref() == Some("USD") {
                if let Some(cost) = row.cost {
                    agg.cost += cost;
                    agg.cost_events += 1;
                }
            }
            agg.responses += 1;
        }
        let (id, label, badge) = distribution_identity(row, group_by);
        let group = by_group.entry(id).or_insert_with(|| DistributionAggregate {
            label,
            badge,
            ..Default::default()
        });
        group.tokens = group.tokens.saturating_add(row.total_tokens);
        group.responses += 1;
    }
    let mut ranked_sessions = by_session.iter().collect::<Vec<_>>();
    ranked_sessions.sort_by(|a, b| b.1.tokens.cmp(&a.1.tokens));
    let top_session_ids: HashSet<i64> = ranked_sessions
        .into_iter()
        .take(top)
        .map(|(id, _)| *id)
        .collect();
    let sessions = query_sessions(db, &top_session_ids, &by_session)?;

    let mut ranked_groups = by_group.iter().collect::<Vec<_>>();
    ranked_groups.sort_by(|a, b| b.1.tokens.cmp(&a.1.tokens).then_with(|| a.0.cmp(b.0)));
    let top_group_ids = ranked_groups
        .into_iter()
        .take(top)
        .map(|(id, _)| id.clone())
        .collect::<HashSet<_>>();
    let groups = distribution_groups(&top_group_ids, &by_group);

    let coverage = coverage_json(&rows);
    let summary = summary_json(&rows);
    let points = cumulative_points(&rows, &top_group_ids, group_by, from_ts, to_ts, bucket_ms);
    Ok(json!({
        "range": range,
        "metric": "tokens",
        "groupBy": group_by,
        "agentFilter": agent_filter,
        "fromTs": from_ts,
        "toTs": to_ts,
        "bucketMs": bucket_ms,
        "agents": agents,
        "coverage": coverage,
        "summary": summary,
        "groups": groups,
        "sessions": sessions,
        "points": points,
    }))
}

fn distribution_identity(row: &UsageRow, group_by: &str) -> (String, String, String) {
    match group_by {
        "model" => {
            let model = row
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("Unknown model");
            (
                format!("model:{model}"),
                model.to_string(),
                "Model".to_string(),
            )
        }
        "session" => {
            let id = row
                .session_id
                .map(|id| format!("session:{id}"))
                .unwrap_or_else(|| "session:unknown".to_string());
            let label = row
                .session_native_id
                .as_deref()
                .map(|native_id| {
                    session_label(
                        row.session_name.as_deref(),
                        row.session_project_path.as_deref(),
                        &row.agent_display_name,
                        native_id,
                    )
                })
                .unwrap_or_else(|| "Unknown session".to_string());
            (id, label, row.agent_display_name.clone())
        }
        _ => (
            format!("harness:{}", row.agent_type),
            row.agent_display_name.clone(),
            "Harness".to_string(),
        ),
    }
}

fn distribution_groups(
    top_ids: &HashSet<String>,
    totals: &HashMap<String, DistributionAggregate>,
) -> Vec<Value> {
    let mut groups = top_ids
        .iter()
        .filter_map(|id| {
            totals.get(id).map(|total| {
                json!({
                    "id": id,
                    "label": total.label,
                    "badge": total.badge,
                    "totalTokens": total.tokens,
                    "responses": total.responses,
                })
            })
        })
        .collect::<Vec<_>>();
    groups.sort_by(|a, b| {
        b["totalTokens"]
            .as_i64()
            .cmp(&a["totalTokens"].as_i64())
            .then_with(|| a["label"].as_str().cmp(&b["label"].as_str()))
    });
    groups
}

fn query_agents(db: &Connection) -> Result<Vec<Value>> {
    let mut stmt = db.prepare(
        "SELECT s.id,s.agent_type,s.display_name,s.capabilities_json,s.limitations_json,s.enabled,s.initial_scan_complete,s.last_scan_ts,
         COALESCE(s.last_scan_error,(SELECT f.last_error FROM agent_scan_files f WHERE f.source_id=s.id AND f.last_error IS NOT NULL ORDER BY f.last_scanned_ts DESC LIMIT 1)),
         (SELECT COUNT(*) FROM agent_scan_files f WHERE f.source_id=s.id)
         FROM agent_sources s ORDER BY s.display_name,s.id",
    )?;
    let rows = stmt.query_map([], |row| {
        let capabilities: String = row.get(3)?;
        let limitations: String = row.get(4)?;
        Ok(json!({
            "type": row.get::<_, String>(1)?,
            "displayName": row.get::<_, String>(2)?,
            "capabilities": serde_json::from_str::<Value>(&capabilities).unwrap_or_else(|_| json!({})),
            "limitations": serde_json::from_str::<Value>(&limitations).unwrap_or_else(|_| json!([])),
            "indexer": {
                "enabled": row.get::<_, i64>(5)? != 0,
                "initialScanComplete": row.get::<_, i64>(6)? != 0,
                "lastScanTs": row.get::<_, Option<i64>>(7)?,
                "error": row.get::<_, Option<String>>(8)?,
                "files": row.get::<_, i64>(9)?,
            }
        }))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn query_sessions(
    db: &Connection,
    top_ids: &HashSet<i64>,
    totals: &HashMap<i64, SessionAggregate>,
) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    let mut stmt = db.prepare(
        "SELECT se.id,se.native_session_id,se.name,se.project_path,so.agent_type,so.display_name
         FROM agent_sessions se JOIN agent_sources so ON so.id=se.source_id WHERE se.id=?",
    )?;
    let mut ids = top_ids.iter().copied().collect::<Vec<_>>();
    ids.sort_unstable();
    for id in ids {
        let value = stmt
            .query_row(params![id], |row| {
                let native_id: String = row.get(1)?;
                let name: Option<String> = row.get(2)?;
                let project: Option<String> = row.get(3)?;
                let display: String = row.get(5)?;
                let label =
                    session_label(name.as_deref(), project.as_deref(), &display, &native_id);
                let total = totals.get(&id).cloned().unwrap_or_default();
                Ok(json!({
                    "id": id,
                    "agentType": row.get::<_, String>(4)?,
                    "agentDisplayName": display,
                    "nativeId": native_id,
                    "label": label,
                    "projectPath": project,
                    "totalTokens": total.tokens,
                    "totalCost": if total.cost_events > 0 { json!(total.cost) } else { Value::Null },
                    "costCurrency": if total.cost_events > 0 { json!("USD") } else { Value::Null },
                    "responses": total.responses,
                }))
            })
            .optional()?;
        if let Some(value) = value {
            output.push(value);
        }
    }
    output.sort_by(|a, b| b["totalTokens"].as_i64().cmp(&a["totalTokens"].as_i64()));
    Ok(output)
}

fn session_label(
    name: Option<&str>,
    project: Option<&str>,
    display: &str,
    native_id: &str,
) -> String {
    if let Some(name) = name.map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_string();
    }
    let short = native_id.chars().take(8).collect::<String>();
    let base = project
        .and_then(|p| Path::new(p).file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty());
    format!("{} · {}", base.unwrap_or(display), short)
}

fn coverage_json(rows: &[UsageRow]) -> Value {
    let token_events = rows.len();
    let cost_events = rows
        .iter()
        .filter(|r| r.cost.is_some() && r.currency.as_deref() == Some("USD"))
        .count();
    let cache_events = rows
        .iter()
        .filter(|r| r.cache_read.is_some() || r.cache_write.is_some())
        .count();
    let mut currencies = rows
        .iter()
        .filter_map(|r| r.currency.clone())
        .collect::<Vec<_>>();
    currencies.sort();
    currencies.dedup();
    json!({
        "tokenEvents": token_events,
        "costEvents": cost_events,
        "cacheEvents": cache_events,
        "costComplete": token_events > 0 && token_events == cost_events,
        "currencies": currencies,
    })
}

fn option_sum(rows: &[UsageRow], value: impl Fn(&UsageRow) -> Option<i64>) -> Value {
    let mut seen = false;
    let mut total = 0_i64;
    for row in rows {
        if let Some(v) = value(row) {
            seen = true;
            total = total.saturating_add(v);
        }
    }
    if seen {
        json!(total)
    } else {
        Value::Null
    }
}

fn summary_json(rows: &[UsageRow]) -> Value {
    let total_tokens = rows
        .iter()
        .fold(0_i64, |sum, row| sum.saturating_add(row.total_tokens));
    let usd_costs = rows
        .iter()
        .filter(|r| r.currency.as_deref() == Some("USD"))
        .filter_map(|r| r.cost)
        .collect::<Vec<_>>();
    let total_cost = if usd_costs.is_empty() {
        Value::Null
    } else {
        json!(usd_costs.iter().sum::<f64>())
    };
    let input = option_sum(rows, |r| r.input);
    let cache_read = option_sum(rows, |r| r.cache_read);
    let cache_write = option_sum(rows, |r| r.cache_write);
    let (cache_input, cache_reads, cache_writes, cache_complete_events) = rows.iter().fold(
        (0_i64, 0_i64, 0_i64, 0_usize),
        |(input_sum, read_sum, write_sum, count), row| match (
            row.input,
            row.cache_read,
            row.cache_write,
        ) {
            (Some(i), Some(r), Some(w)) => (
                input_sum.saturating_add(i),
                read_sum.saturating_add(r),
                write_sum.saturating_add(w),
                count + 1,
            ),
            _ => (input_sum, read_sum, write_sum, count),
        },
    );
    let cache_denominator = cache_input
        .saturating_add(cache_reads)
        .saturating_add(cache_writes);
    let cache_hit_rate = if cache_complete_events > 0 && cache_denominator > 0 {
        json!(cache_reads as f64 / cache_denominator as f64)
    } else {
        Value::Null
    };
    let mut kinds = rows
        .iter()
        .filter(|r| r.currency.as_deref() == Some("USD"))
        .filter_map(|r| r.cost_kind.clone())
        .collect::<Vec<_>>();
    kinds.sort();
    kinds.dedup();
    let cost_kind = if kinds.len() == 1 {
        json!(kinds[0])
    } else if kinds.is_empty() {
        Value::Null
    } else {
        json!("mixed")
    };
    json!({
        "totalTokens": total_tokens,
        "inputTokens": input,
        "outputTokens": option_sum(rows, |r| r.output),
        "reasoningTokens": option_sum(rows, |r| r.reasoning),
        "cacheReadTokens": cache_read,
        "cacheWriteTokens": cache_write,
        "totalCost": total_cost,
        "costCurrency": if usd_costs.is_empty() { Value::Null } else { json!("USD") },
        "costKind": cost_kind,
        "responses": rows.len(),
        "cacheHitRate": cache_hit_rate,
    })
}

fn cumulative_points(
    rows: &[UsageRow],
    top: &HashSet<String>,
    group_by: &str,
    from: i64,
    to: i64,
    bucket: i64,
) -> Vec<Value> {
    let mut boundaries = vec![from];
    let mut ts = from.saturating_add(bucket);
    while ts < to && boundaries.len() < 501 {
        boundaries.push(ts);
        ts = ts.saturating_add(bucket);
    }
    if boundaries.last().copied() != Some(to) {
        boundaries.push(to);
    }
    let mut idx = 0;
    let mut total_tokens = 0_i64;
    let mut total_cost = 0.0;
    let mut has_cost = false;
    let mut groups: BTreeMap<String, i64> = BTreeMap::new();
    let mut output = Vec::with_capacity(boundaries.len());
    for (boundary_idx, boundary) in boundaries.into_iter().enumerate() {
        if boundary_idx > 0 {
            while idx < rows.len() && rows[idx].ts <= boundary {
                let row = &rows[idx];
                total_tokens = total_tokens.saturating_add(row.total_tokens);
                let usd_cost = if row.currency.as_deref() == Some("USD") {
                    row.cost
                } else {
                    None
                };
                if let Some(cost) = usd_cost {
                    total_cost += cost;
                    has_cost = true;
                }
                let group_id = distribution_identity(row, group_by).0;
                let key = if top.contains(&group_id) {
                    group_id
                } else {
                    "__other__".to_string()
                };
                let total = groups.entry(key).or_default();
                *total = total.saturating_add(row.total_tokens);
                idx += 1;
            }
        }
        let group_json = groups
            .iter()
            .map(|(key, tokens)| (key.clone(), json!({ "tokens": tokens })))
            .collect::<serde_json::Map<_, _>>();
        output.push(json!({
            "ts": boundary,
            "totalTokens": total_tokens,
            "totalCost": if has_cost { json!(total_cost) } else { Value::Null },
            "groups": group_json,
        }));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    struct SyntheticAdapter(&'static str);
    impl AgentTelemetryAdapter for SyntheticAdapter {
        fn descriptor(&self) -> AgentDescriptor {
            AgentDescriptor {
                agent_type: self.0.to_string(),
                display_name: self.0.to_string(),
                adapter_version: 1,
                capabilities: AgentCapabilities {
                    historical_import: true,
                    live_updates: false,
                    input_tokens: false,
                    output_tokens: false,
                    reasoning_tokens: false,
                    cache_tokens: false,
                    cost: false,
                    session_forks: false,
                },
                limitations: vec![],
            }
        }
        fn discover(&self) -> Result<Vec<AgentSource>> {
            Ok(vec![])
        }
        fn scan(
            &self,
            _source: &AgentSource,
            _state: Option<&AgentScanState>,
            _sink: &mut dyn AgentTelemetrySink,
        ) -> Result<AgentScanResult> {
            Ok(AgentScanResult {
                files: 0,
                events: 0,
                malformed_lines: 0,
            })
        }
    }

    #[test]
    fn identity_is_namespaced_by_agent_and_source() {
        let key = b"same-native-event";
        assert_ne!(
            event_identity("pi", "local", key),
            event_identity("other", "local", key)
        );
        assert_ne!(
            event_identity("pi", "local", key),
            event_identity("pi", "remote", key)
        );
    }

    #[test]
    fn registry_accepts_a_second_agent_without_core_changes() {
        let registry = AgentAdapterRegistry::with(vec![
            Arc::new(SyntheticAdapter("one")),
            Arc::new(SyntheticAdapter("two")),
        ]);
        let types = registry
            .adapters
            .iter()
            .map(|a| a.descriptor().agent_type)
            .collect::<Vec<_>>();
        assert_eq!(types, vec!["one", "two"]);
    }

    #[test]
    fn reasoning_is_not_added_to_total_and_missing_values_stay_null() {
        let rows = vec![UsageRow {
            ts: 1,
            session_id: Some(1),
            agent_type: "pi".into(),
            agent_display_name: "Pi".into(),
            model: Some("test-model".into()),
            session_native_id: Some("test-session".into()),
            session_name: Some("Test session".into()),
            session_project_path: Some("/workspace/test".into()),
            total_tokens: 100,
            input: None,
            output: Some(40),
            reasoning: Some(20),
            cache_read: None,
            cache_write: None,
            cost: None,
            currency: None,
            cost_kind: None,
        }];
        let summary = summary_json(&rows);
        assert_eq!(summary["totalTokens"], 100);
        assert_eq!(summary["outputTokens"], 40);
        assert_eq!(summary["reasoningTokens"], 20);
        assert!(summary["inputTokens"].is_null());
        assert!(summary["totalCost"].is_null());
    }

    #[test]
    fn two_agents_share_the_store_and_api_with_missing_cost_preserved() {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        let descriptor = |agent: &str, cost: bool| AgentDescriptor {
            agent_type: agent.to_string(),
            display_name: agent.to_string(),
            adapter_version: 1,
            capabilities: AgentCapabilities {
                historical_import: true,
                live_updates: false,
                input_tokens: true,
                output_tokens: true,
                reasoning_tokens: false,
                cache_tokens: false,
                cost,
                session_forks: false,
            },
            limitations: vec![],
        };
        for (agent, tokens, cost) in [("first", 10, Some(0.1)), ("second", 20, None)] {
            let desc = descriptor(agent, cost.is_some());
            let source_id = ensure_source(&db, &desc, "same-source").unwrap();
            let tx = db.transaction().unwrap();
            {
                let mut sink = SqliteTelemetrySink::new(&tx, source_id, agent, "same-source");
                let session_id = sink
                    .upsert_session(NormalizedAgentSession {
                        native_session_id: "same-session".into(),
                        parent_native_session_id: None,
                        parent_native_path: None,
                        name: None,
                        project_path: None,
                        created_ts: now_ms() - 2_000,
                        first_activity_ts: None,
                        last_activity_ts: None,
                    })
                    .unwrap();
                let changes = tx.total_changes();
                let same_id = sink.upsert_session(NormalizedAgentSession {
                    native_session_id: "same-session".into(), parent_native_session_id: None,
                    parent_native_path: None, name: None, project_path: None,
                    created_ts: now_ms(), first_activity_ts: None, last_activity_ts: None,
                }).unwrap();
                assert_eq!(same_id, session_id);
                assert_eq!(tx.total_changes(), changes, "no-op session upsert must not write");
                let event_id = sink
                    .upsert_usage(
                        NormalizedUsageEvent {
                            native_event_key: b"same-event".to_vec(),
                            event_ts: now_ms() - 1_000,
                            provider: None,
                            api: None,
                            model: Some(format!("{agent}-model")),
                            stop_reason: None,
                            usage: NormalizedTokenUsage {
                                input: None,
                                output: None,
                                reasoning: None,
                                cache_read: None,
                                cache_write: None,
                                total: tokens,
                            },
                            cost: cost.map(|total| NormalizedCost {
                                input: None,
                                output: None,
                                cache_read: None,
                                cache_write: None,
                                total,
                                currency: "USD".into(),
                                kind: CostKind::Reported,
                            }),
                        },
                        session_id,
                    )
                    .unwrap();
                sink.add_session_event_ref(SessionUsageRef {
                    session_id,
                    native_entry_id: "same-entry".into(),
                    usage_event_id: event_id,
                    inherited: false,
                })
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let all = query_agent_usage(
            &db,
            AgentUsageQuery {
                range: Some("all".into()),
                metric: Some("tokens".into()),
                group: Some("harness".into()),
                agent: Some("all".into()),
                top: Some(8),
            },
        )
        .unwrap();
        assert_eq!(all["summary"]["totalTokens"], 30);
        assert_eq!(all["summary"]["responses"], 2);
        assert_eq!(all["coverage"]["costComplete"], false);
        assert_eq!(all["agents"].as_array().unwrap().len(), 2);
        assert_eq!(all["groupBy"], "harness");
        assert_eq!(all["groups"].as_array().unwrap().len(), 2);
        assert_eq!(
            all["points"].as_array().unwrap().last().unwrap()["groups"]["harness:first"]["tokens"],
            10
        );
        assert_eq!(
            all["points"].as_array().unwrap().last().unwrap()["groups"]["harness:second"]["tokens"],
            20
        );
        let second = query_agent_usage(
            &db,
            AgentUsageQuery {
                range: Some("all".into()),
                metric: Some("cost".into()),
                group: Some("model".into()),
                agent: Some("second".into()),
                top: Some(8),
            },
        )
        .unwrap();
        assert_eq!(second["summary"]["totalTokens"], 20);
        assert!(second["summary"]["totalCost"].is_null());
        assert_eq!(second["metric"], "tokens");
        assert_eq!(second["groupBy"], "model");
        assert_eq!(second["groups"][0]["label"], "second-model");

        let sessions = query_agent_usage(
            &db,
            AgentUsageQuery {
                range: Some("all".into()),
                metric: None,
                group: Some("session".into()),
                agent: Some("all".into()),
                top: Some(8),
            },
        )
        .unwrap();
        assert_eq!(sessions["groupBy"], "session");
        assert_eq!(sessions["groups"].as_array().unwrap().len(), 2);
        assert!(sessions["groups"]
            .as_array()
            .unwrap()
            .iter()
            .all(|group| group["id"].as_str().unwrap().starts_with("session:")));
    }

    #[test]
    fn cumulative_points_are_monotonic_and_bounded() {
        let rows = vec![
            UsageRow {
                ts: 2,
                session_id: Some(1),
                agent_type: "pi".into(),
                agent_display_name: "Pi".into(),
                model: Some("model-a".into()),
                session_native_id: Some("session-a".into()),
                session_name: Some("Session A".into()),
                session_project_path: None,
                total_tokens: i64::from(u32::MAX),
                input: None,
                output: None,
                reasoning: None,
                cache_read: None,
                cache_write: None,
                cost: Some(1.0),
                currency: Some("USD".into()),
                cost_kind: Some("estimated".into()),
            },
            UsageRow {
                ts: 3,
                session_id: Some(2),
                agent_type: "other".into(),
                agent_display_name: "Other".into(),
                model: Some("model-b".into()),
                session_native_id: Some("session-b".into()),
                session_name: None,
                session_project_path: Some("/workspace/project-b".into()),
                total_tokens: i64::from(u32::MAX),
                input: None,
                output: None,
                reasoning: None,
                cache_read: None,
                cache_write: None,
                cost: None,
                currency: None,
                cost_kind: None,
            },
        ];
        let points = cumulative_points(
            &rows,
            &HashSet::from(["harness:pi".to_string()]),
            "harness",
            1,
            100_000,
            1,
        );
        assert!(points.len() <= 502);
        assert_eq!(
            points.last().unwrap()["totalTokens"],
            i64::from(u32::MAX) * 2
        );
        assert!(points
            .windows(2)
            .all(|p| p[0]["totalTokens"].as_i64() <= p[1]["totalTokens"].as_i64()));
    }
}
