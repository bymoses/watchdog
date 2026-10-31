use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use crate::{app::EnvironmentSample, util::now_ms};

pub(crate) const EXTRACTOR_VERSION: i64 = 1;
const INITIAL_LABELER_VERSION: i64 = 1;
const MAX_RELABEL_BATCH: usize = 100;
const MAX_ACTIVE_RULES: usize = 256;
const MAX_LABELS_PER_INTERVAL: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FocusFacts {
    pub(crate) focused_app: String,
    pub(crate) focused_title: Option<String>,
    pub(crate) focused_pid: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FocusEnrichment {
    pub(crate) terminal_kind: Option<String>,
    pub(crate) terminal_match_method: Option<String>,
    pub(crate) terminal_match_confidence: Option<f64>,
    pub(crate) terminal_candidate_count: Option<i64>,
    pub(crate) tmux_client_pid: Option<i64>,
    pub(crate) tmux_session: Option<String>,
    pub(crate) tmux_window: Option<String>,
    pub(crate) tmux_pane: Option<String>,
    pub(crate) tmux_pane_pid: Option<i64>,
    pub(crate) cwd_key: Option<[u8; 32]>,
    pub(crate) cwd_display: Option<String>,
    pub(crate) foreground_executable: Option<String>,
    pub(crate) tool: Option<String>,
    pub(crate) repository: Option<([u8; 32], String)>,
    pub(crate) project: Option<([u8; 32], String)>,
    pub(crate) agent_type: Option<String>,
    pub(crate) agent_process_pid: Option<i64>,
    pub(crate) agent_session_id: Option<i64>,
    pub(crate) native_agent_session_id: Option<String>,
    pub(crate) agent_state: Option<String>,
    pub(crate) agent_match_method: Option<String>,
    pub(crate) agent_match_confidence: Option<f64>,
    pub(crate) ambiguity_json: String,
}

impl FocusFacts {
    pub(crate) fn from_environment(sample: &EnvironmentSample) -> Self {
        Self::new(
            &sample.focused_app,
            &sample.focused_title,
            sample.focused_pid,
        )
    }

    pub(crate) fn new(app: &str, title: &str, pid: Option<i64>) -> Self {
        let app = normalize_text(app, 120).to_lowercase();
        let title = normalize_text(title, 240);
        Self {
            focused_app: app,
            focused_title: (!title.is_empty()).then_some(title),
            focused_pid: pid.filter(|pid| *pid > 0),
        }
    }

    pub(crate) fn context_key(&self) -> [u8; 32] {
        self.context_key_with(&FocusEnrichment::default())
    }
    fn context_key_with(&self, enrichment: &FocusEnrichment) -> [u8; 32] {
        let mut digest = Sha256::new();
        for value in [
            self.focused_app.as_str(),
            self.focused_title.as_deref().unwrap_or(""),
            enrichment.terminal_kind.as_deref().unwrap_or(""),
            enrichment.terminal_match_method.as_deref().unwrap_or(""),
            enrichment.tmux_session.as_deref().unwrap_or(""),
            enrichment.tmux_window.as_deref().unwrap_or(""),
            enrichment.tmux_pane.as_deref().unwrap_or(""),
            enrichment.foreground_executable.as_deref().unwrap_or(""),
            enrichment.tool.as_deref().unwrap_or(""),
            enrichment.agent_type.as_deref().unwrap_or(""),
            enrichment.native_agent_session_id.as_deref().unwrap_or(""),
            enrichment.agent_state.as_deref().unwrap_or(""),
            enrichment.agent_match_method.as_deref().unwrap_or(""),
        ] {
            digest.update(value.len().to_be_bytes());
            digest.update(value.as_bytes());
        }
        for value in [
            self.focused_pid,
            enrichment.tmux_client_pid,
            enrichment.tmux_pane_pid,
            enrichment.agent_process_pid,
            enrichment.agent_session_id,
            enrichment.terminal_candidate_count,
        ] {
            digest.update(value.unwrap_or_default().to_be_bytes());
        }
        for value in [
            enrichment.terminal_match_confidence,
            enrichment.agent_match_confidence,
        ] {
            digest.update(value.map(f64::to_bits).unwrap_or_default().to_be_bytes());
        }
        digest.update(canonical_json_string(&enrichment.ambiguity_json).as_bytes());
        if let Some(key) = enrichment.cwd_key {
            digest.update(key);
        }
        if let Some((key, _)) = &enrichment.repository {
            digest.update(key);
        }
        if let Some((key, _)) = &enrichment.project {
            digest.update(key);
        }
        digest.finalize().into()
    }
}

fn normalize_text(value: &str, max_chars: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn canonical_json_string(raw: &str) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
            Value::Object(values) => {
                let mut entries = values.iter().collect::<Vec<_>>();
                entries.sort_by(|(left, _), (right, _)| left.cmp(right));
                let mut object = serde_json::Map::new();
                for (key, value) in entries {
                    object.insert(key.clone(), canonical(value));
                }
                Value::Object(object)
            }
            other => other.clone(),
        }
    }
    serde_json::from_str::<Value>(raw)
        .map(|value| canonical(&value).to_string())
        .unwrap_or_else(|_| "{}".to_string())
}

fn hash_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((*part).len().to_be_bytes());
        digest.update(part);
    }
    digest.finalize().into()
}

#[derive(Clone, Copy)]
pub(crate) struct CaptureSettings {
    pub(crate) poll_interval_ms: i64,
    pub(crate) suspend_gap_ms: i64,
}

impl CaptureSettings {
    fn continuity_ms(self) -> i64 {
        self.poll_interval_ms.max(1) + (self.poll_interval_ms / 2).max(5_000)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CaptureOutcome {
    Started(i64),
    Extended(i64),
    Replaced {
        closed: i64,
        started: i64,
        reason: String,
    },
    IgnoredOutOfOrder(i64),
}

struct OpenInterval {
    id: i64,
    start_ts: i64,
    end_ts: i64,
    last_observed_ts: i64,
    context_key: Vec<u8>,
}

pub(crate) fn capture_environment_focus_enriched(
    db: &mut Connection,
    sample: &EnvironmentSample,
    environment_id: i64,
    settings: CaptureSettings,
    enrichment: &FocusEnrichment,
) -> Result<CaptureOutcome> {
    capture_focus_with_enrichment(
        db,
        &FocusFacts::from_environment(sample),
        sample.ts,
        Some(environment_id),
        settings,
        enrichment,
    )
}

#[cfg(test)]
pub(crate) fn capture_focus(
    db: &mut Connection,
    facts: &FocusFacts,
    observed_ts: i64,
    environment_id: Option<i64>,
    settings: CaptureSettings,
) -> Result<CaptureOutcome> {
    capture_focus_with_enrichment(
        db,
        facts,
        observed_ts,
        environment_id,
        settings,
        &FocusEnrichment::default(),
    )
}

fn capture_focus_with_enrichment(
    db: &mut Connection,
    facts: &FocusFacts,
    observed_ts: i64,
    environment_id: Option<i64>,
    settings: CaptureSettings,
    enrichment: &FocusEnrichment,
) -> Result<CaptureOutcome> {
    let key = facts.context_key_with(enrichment);
    // Rule loading, bounded JSON parsing and matching happen before acquiring the
    // SQLite write transaction. The resulting plan contains only compact values.
    let label_plan = prepare_label_plan(db, facts, enrichment)?;
    let active_version = active_labeler_version(db)?;
    let mut materialization_versions = vec![active_version];
    {
        let mut statement = db.prepare("SELECT labeler_version FROM relabel_jobs WHERE status='running' AND rules_revision=? AND COALESCE(base_labeler_version,1)=? ORDER BY id")?;
        for version in statement.query_map(params![label_plan.revision, active_version], |row| {
            row.get::<_, i64>(0)
        })? {
            let version = version?;
            if !materialization_versions.contains(&version) {
                materialization_versions.push(version);
            }
        }
    }
    let tx = db.transaction()?;
    let open = tx.query_row(
        "SELECT id,start_ts,end_ts,last_observed_ts,context_key FROM focus_context_intervals WHERE closed_reason IS NULL LIMIT 1",
        [],
        |row| Ok(OpenInterval {
            id: row.get(0)?,
            start_ts: row.get(1)?,
            end_ts: row.get(2)?,
            last_observed_ts: row.get(3)?,
            context_key: row.get(4)?,
        }),
    ).optional()?;

    let outcome = if let Some(open) = open {
        if observed_ts < open.last_observed_ts {
            CaptureOutcome::IgnoredOutOfOrder(open.id)
        } else {
            let elapsed = observed_ts - open.last_observed_ts;
            let gap_reason = if elapsed >= settings.suspend_gap_ms.max(1) {
                Some("suspend_gap")
            } else if elapsed > settings.continuity_ms() {
                Some("monitoring_gap")
            } else {
                None
            };
            let day_changed = local_date(observed_ts) != local_date(open.last_observed_ts);
            let same_context = open.context_key.as_slice() == key;

            if gap_reason.is_none() && !day_changed && same_context {
                tx.execute(
                    "UPDATE focus_context_intervals SET end_ts=?,last_observed_ts=?,end_environment_id=? WHERE id=?",
                    params![observed_ts, observed_ts, environment_id, open.id],
                )?;
                for version in &materialization_versions {
                    materialize_activity(&tx, open.id, *version)?;
                }
                CaptureOutcome::Extended(open.id)
            } else {
                let reason = gap_reason.map(str::to_string).unwrap_or_else(|| {
                    if day_changed {
                        "local_midnight".to_string()
                    } else {
                        "facts_changed".to_string()
                    }
                });
                let gap = gap_reason.is_some();
                let boundary = if day_changed && !gap {
                    local_midnight_ms(observed_ts).clamp(open.start_ts, observed_ts)
                } else if gap {
                    open.end_ts
                } else {
                    observed_ts
                };
                tx.execute(
                    "UPDATE focus_context_intervals SET end_ts=?,closed_reason=? WHERE id=?",
                    params![boundary, reason, open.id],
                )?;
                for version in &materialization_versions {
                    materialize_activity(&tx, open.id, *version)?;
                }
                let start_ts = if day_changed && !gap {
                    boundary
                } else {
                    observed_ts
                };
                let new_id = insert_interval(
                    &tx,
                    facts,
                    enrichment,
                    &key,
                    start_ts,
                    observed_ts,
                    environment_id,
                )?;
                for version in &materialization_versions {
                    apply_label_plan(&tx, new_id, &label_plan, None, *version)?;
                    apply_enrichment_labels(&tx, new_id, enrichment, *version)?;
                }
                CaptureOutcome::Replaced {
                    closed: open.id,
                    started: new_id,
                    reason,
                }
            }
        }
    } else {
        let id = insert_interval(
            &tx,
            facts,
            enrichment,
            &key,
            observed_ts,
            observed_ts,
            environment_id,
        )?;
        for version in &materialization_versions {
            apply_label_plan(&tx, id, &label_plan, None, *version)?;
            apply_enrichment_labels(&tx, id, enrichment, *version)?;
        }
        CaptureOutcome::Started(id)
    };
    tx.commit()?;
    Ok(outcome)
}

fn insert_interval(
    tx: &Transaction<'_>,
    facts: &FocusFacts,
    enrichment: &FocusEnrichment,
    key: &[u8; 32],
    start_ts: i64,
    observed_ts: i64,
    environment_id: Option<i64>,
) -> Result<i64> {
    let repository_id = enrichment
        .repository
        .as_ref()
        .map(|(key, name)| upsert_entity_key(tx, "repository", key, name, observed_ts))
        .transpose()?;
    let project_id = enrichment
        .project
        .as_ref()
        .map(|(key, name)| upsert_entity_key(tx, "project", key, name, observed_ts))
        .transpose()?;
    if let (Some(repository), Some(project)) = (repository_id, project_id) {
        tx.execute("INSERT OR IGNORE INTO context_entity_relations(parent_entity_id,child_entity_id,relation,confidence,source)VALUES(?,?,'project_has_repository',?,'git')",params![project,repository,enrichment.terminal_match_confidence.unwrap_or(0.8)])?;
    }
    let ambiguity = if enrichment.ambiguity_json.is_empty() {
        "{}"
    } else {
        enrichment.ambiguity_json.as_str()
    };
    tx.execute(
        "INSERT INTO focus_context_intervals (start_ts,end_ts,last_observed_ts,start_environment_id,end_environment_id,context_key,extractor_version,focused_app,focused_title,focused_pid,terminal_kind,terminal_match_method,terminal_match_confidence,terminal_candidate_count,tmux_client_pid,tmux_session,tmux_window,tmux_pane,tmux_pane_pid,terminal_cwd_key,terminal_cwd_display,foreground_executable,repository_entity_id,project_entity_id,agent_type,agent_process_pid,agent_session_id,native_agent_session_id,agent_state,agent_match_method,agent_match_confidence,ambiguity_json) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![start_ts,observed_ts,observed_ts,environment_id,environment_id,key.as_slice(),EXTRACTOR_VERSION,facts.focused_app,facts.focused_title,facts.focused_pid,enrichment.terminal_kind,enrichment.terminal_match_method,enrichment.terminal_match_confidence,enrichment.terminal_candidate_count,enrichment.tmux_client_pid,enrichment.tmux_session,enrichment.tmux_window,enrichment.tmux_pane,enrichment.tmux_pane_pid,enrichment.cwd_key.as_ref().map(|k|k.as_slice()),enrichment.cwd_display,enrichment.foreground_executable,repository_id,project_id,enrichment.agent_type,enrichment.agent_process_pid,enrichment.agent_session_id,enrichment.native_agent_session_id,enrichment.agent_state,enrichment.agent_match_method,enrichment.agent_match_confidence,ambiguity],
    )?;
    let interval_id = tx.last_insert_rowid();
    for (alias_kind, alias_value, entity_id, confidence) in [
        (
            "tmux.session",
            enrichment.tmux_session.as_deref(),
            project_id,
            enrichment.terminal_match_confidence,
        ),
        (
            "tmux.window",
            enrichment.tmux_window.as_deref(),
            project_id,
            enrichment.terminal_match_confidence,
        ),
    ] {
        if let (Some(value), Some(entity), Some(confidence)) = (alias_value, entity_id, confidence)
        {
            tx.execute("INSERT OR IGNORE INTO context_entity_aliases(entity_id,alias_kind,alias_value,source,confidence,created_ts)VALUES(?,?,?,'tmux',?,?)",params![entity,alias_kind,value,confidence,observed_ts])?;
        }
    }
    Ok(interval_id)
}

pub(crate) fn local_date(ts_ms: i64) -> time::Date {
    let utc = OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let offset = UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC);
    utc.to_offset(offset).date()
}

pub(crate) fn local_midnight_ms(ts_ms: i64) -> i64 {
    let utc = OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let sample_offset = UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC);
    let local = utc.to_offset(sample_offset);
    let primitive = PrimitiveDateTime::new(local.date(), Time::MIDNIGHT);
    let first = primitive.assume_offset(sample_offset);
    let midnight_offset = UtcOffset::local_offset_at(first).unwrap_or(sample_offset);
    primitive
        .assume_offset(midnight_offset)
        .unix_timestamp_nanos() as i64
        / 1_000_000
}

pub(crate) fn next_local_midnight_ms(ts_ms: i64) -> i64 {
    let utc = OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let sample_offset = UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC);
    let next_date = utc
        .to_offset(sample_offset)
        .date()
        .next_day()
        .unwrap_or(time::Date::MAX);
    let primitive = PrimitiveDateTime::new(next_date, Time::MIDNIGHT);
    let first = primitive.assume_offset(sample_offset);
    let midnight_offset = UtcOffset::local_offset_at(first).unwrap_or(sample_offset);
    primitive
        .assume_offset(midnight_offset)
        .unix_timestamp_nanos() as i64
        / 1_000_000
}

#[derive(Clone, Deserialize, serde::Serialize)]
struct Condition {
    fact: String,
    operator: String,
    #[serde(default)]
    value: Option<Value>,
}

#[derive(Clone, Deserialize, serde::Serialize)]
struct Output {
    dimension: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    entity: Option<String>,
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Clone)]
struct Rule {
    id: i64,
    name: String,
    origin: String,
    conditions: Vec<Condition>,
    outputs: Vec<Output>,
    stop_dimensions: Vec<String>,
}

#[derive(Clone)]
struct PendingEntity {
    kind: String,
    identity: String,
    canonical_key: [u8; 32],
}

#[derive(Clone)]
struct PendingLabel {
    dimension: String,
    value: Option<String>,
    entity: Option<PendingEntity>,
    confidence: f64,
    source: String,
    rule_id: i64,
    evidence_json: String,
}

struct LabelPlan {
    revision: i64,
    labels: Vec<PendingLabel>,
}

fn current_rules_revision(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT rules_revision FROM context_runtime_state WHERE id=1",
        [],
        |row| row.get(0),
    )?)
}

pub(crate) fn active_labeler_version(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT active_labeler_version FROM context_runtime_state WHERE id=1",
        [],
        |row| row.get(0),
    )?)
}

fn ensure_capture_run(db: &Connection, revision: i64, version: i64) -> Result<i64> {
    if let Some(id) = db.query_row(
        "SELECT id FROM labeler_runs WHERE status='completed' AND labeler_version=? AND rules_revision=? ORDER BY id DESC LIMIT 1",
        params![version, revision],
        |row| row.get(0),
    ).optional()? {
        return Ok(id);
    }
    let ts = now_ms();
    db.execute(
        "INSERT INTO labeler_runs (labeler_version,rules_revision,started_ts,completed_ts,status) VALUES (?,?,?,?, 'completed')",
        params![version, revision, ts, ts],
    )?;
    Ok(db.last_insert_rowid())
}

fn load_rules(db: &Connection) -> Result<Vec<Rule>> {
    let mut stmt = db.prepare(
        "SELECT id,name,origin,conditions_json,outputs_json,stop_dimensions_json FROM label_rules WHERE enabled=1 ORDER BY CASE origin WHEN 'user' THEN 3 WHEN 'built_in' THEN 2 WHEN 'heuristic' THEN 1 ELSE 0 END DESC, priority DESC, id ASC LIMIT ?",
    )?;
    let raw = stmt
        .query_map(params![MAX_ACTIVE_RULES as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(|(id, name, origin, conditions, outputs, stops)| {
            let conditions = parse_list::<Condition>(&conditions, "conditions")
                .ok_or_else(|| anyhow!("rule {id} has invalid conditions"))?;
            let outputs = parse_list::<Output>(&outputs, "outputs")
                .ok_or_else(|| anyhow!("rule {id} has invalid outputs"))?;
            let stop_dimensions = serde_json::from_str::<Vec<String>>(&stops)
                .map_err(|_| anyhow!("rule {id} has invalid stop dimensions"))?;
            let shared = crate::activity::RuleInput {
                name: name.clone(),
                priority: 0,
                enabled: true,
                origin: origin.clone(),
                conditions: conditions
                    .iter()
                    .map(|condition| crate::activity::RuleCondition {
                        fact: condition.fact.clone(),
                        operator: condition.operator.clone(),
                        value: condition.value.clone(),
                    })
                    .collect(),
                outputs: outputs
                    .iter()
                    .map(|output| crate::activity::RuleOutput {
                        dimension: output.dimension.clone(),
                        value: output.value.clone(),
                        entity: output.entity.clone(),
                        confidence: output.confidence,
                    })
                    .collect(),
                stop_dimensions: stop_dimensions.clone(),
            };
            crate::activity::validate_rule(&shared)?;
            Ok(Rule {
                id,
                name,
                origin,
                conditions,
                outputs,
                stop_dimensions,
            })
        })
        .collect()
}

fn parse_list<T: for<'de> Deserialize<'de>>(raw: &str, field: &str) -> Option<Vec<T>> {
    let value: Value = serde_json::from_str(raw).ok()?;
    if value.is_array() {
        serde_json::from_value(value).ok()
    } else {
        serde_json::from_value(value.get(field)?.clone()).ok()
    }
}

fn prepare_label_plan(
    db: &Connection,
    facts: &FocusFacts,
    enrichment: &FocusEnrichment,
) -> Result<LabelPlan> {
    let revision = current_rules_revision(db)?;
    let rules = load_rules(db)?;
    Ok(LabelPlan {
        revision,
        labels: evaluate_rules(db, facts, enrichment, rules)?,
    })
}

fn apply_label_plan(
    db: &Connection,
    interval_id: i64,
    plan: &LabelPlan,
    run_id: Option<i64>,
    labeler_version: i64,
) -> Result<()> {
    let run_id = match run_id {
        Some(id) => id,
        None => ensure_capture_run(db, plan.revision, labeler_version)?,
    };
    db.execute(
        "DELETE FROM focus_interval_labels WHERE context_interval_id=? AND labeler_version=? AND source NOT LIKE 'integration:%'",
        params![interval_id, labeler_version],
    )?;
    for label in &plan.labels {
        let (entity_id, label_key) = if let Some(entity) = &label.entity {
            let entity_id = upsert_entity(
                db,
                &entity.kind,
                &entity.identity,
                &entity.identity,
                now_ms(),
            )?
            .0;
            (
                Some(entity_id),
                hash_parts(&[
                    b"entity-label",
                    label.dimension.as_bytes(),
                    &entity.canonical_key,
                ]),
            )
        } else {
            (
                None,
                hash_parts(&[
                    b"value-label",
                    label.dimension.as_bytes(),
                    label.value.as_deref().unwrap_or("").as_bytes(),
                ]),
            )
        };
        db.execute(
            "INSERT OR IGNORE INTO focus_interval_labels (context_interval_id,dimension,label_key,value,entity_id,confidence,source,rule_id,labeler_run_id,labeler_version,evidence_json) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            params![interval_id, label.dimension, label_key.as_slice(), label.value, entity_id, label.confidence, label.source, label.rule_id, run_id, labeler_version, label.evidence_json],
        )?;
    }
    materialize_activity(db, interval_id, labeler_version)?;
    Ok(())
}

fn evaluate_rules(
    db: &Connection,
    facts: &FocusFacts,
    enrichment: &FocusEnrichment,
    rules: Vec<Rule>,
) -> Result<Vec<PendingLabel>> {
    let mut labels = Vec::new();
    let mut exclusive = HashSet::new();
    let mut seen = HashSet::new();
    let mut stopped = HashSet::new();
    'rules: for rule in rules {
        if labels.len() >= MAX_LABELS_PER_INTERVAL {
            break;
        }
        if !rule
            .conditions
            .iter()
            .all(|condition| condition_matches(db, condition, facts, enrichment))
        {
            continue;
        }
        let evidence = json!({
            "rule": rule.name,
            "facts": rule.conditions.iter().map(|condition| condition.fact.as_str()).collect::<Vec<_>>()
        }).to_string();
        for output in rule.outputs {
            if labels.len() >= MAX_LABELS_PER_INTERVAL {
                break 'rules;
            }
            if stopped.contains(&output.dimension) || output.dimension.len() > 80 {
                continue;
            }
            if output.dimension == "activity.primary" && !exclusive.insert(output.dimension.clone())
            {
                continue;
            }
            let confidence = output.confidence.unwrap_or(1.0).clamp(0.0, 1.0);
            let (value, entity, identity) = match (output.value, output.entity) {
                (Some(value), None) if !value.is_empty() && value.len() <= 240 => {
                    let identity = format!("value:{value}");
                    (Some(value), None, identity)
                }
                (None, Some(entity_ref)) => {
                    let Some((kind, identity)) = valid_entity_ref(&entity_ref) else {
                        continue;
                    };
                    let canonical_key = hash_parts(&[kind.as_bytes(), identity.as_bytes()]);
                    let pending = PendingEntity {
                        kind: kind.to_string(),
                        identity: identity.to_string(),
                        canonical_key,
                    };
                    (None, Some(pending), format!("entity:{kind}:{identity}"))
                }
                _ => continue,
            };
            if !seen.insert((output.dimension.clone(), identity)) {
                continue;
            }
            labels.push(PendingLabel {
                dimension: output.dimension,
                value,
                entity,
                confidence,
                source: format!("rule:{}", rule.origin),
                rule_id: rule.id,
                evidence_json: evidence.clone(),
            });
        }
        for dimension in rule.stop_dimensions {
            stopped.insert(dimension);
        }
    }
    Ok(labels)
}

fn valid_entity_ref(value: &str) -> Option<(&str, &str)> {
    let (kind, identity) = value.split_once(':')?;
    if kind.is_empty()
        || identity.is_empty()
        || kind.len() > 40
        || identity.len() > 240
        || !kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((kind, identity))
}

fn upsert_entity(
    db: &Connection,
    kind: &str,
    identity: &str,
    display_name: &str,
    ts: i64,
) -> Result<(i64, [u8; 32])> {
    let key = hash_parts(&[kind.as_bytes(), identity.as_bytes()]);
    db.execute(
        "INSERT INTO context_entities (kind,canonical_key,display_name,first_seen_ts,last_seen_ts) VALUES (?,?,?,?,?) ON CONFLICT(kind,canonical_key) DO UPDATE SET last_seen_ts=MAX(last_seen_ts,excluded.last_seen_ts)",
        params![kind, key.as_slice(), display_name, ts, ts],
    )?;
    let id = db.query_row(
        "SELECT id FROM context_entities WHERE kind=? AND canonical_key=?",
        params![kind, key.as_slice()],
        |row| row.get(0),
    )?;
    Ok((id, key))
}

fn upsert_entity_key(
    db: &Connection,
    kind: &str,
    key: &[u8; 32],
    display_name: &str,
    ts: i64,
) -> Result<i64> {
    db.execute("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts)VALUES(?,?,?,?,?) ON CONFLICT(kind,canonical_key)DO UPDATE SET last_seen_ts=MAX(last_seen_ts,excluded.last_seen_ts),display_name=CASE WHEN user_confirmed=0 THEN excluded.display_name ELSE display_name END",params![kind,key.as_slice(),display_name,ts,ts])?;
    Ok(db.query_row(
        "SELECT id FROM context_entities WHERE kind=? AND canonical_key=?",
        params![kind, key.as_slice()],
        |r| r.get(0),
    )?)
}

fn apply_enrichment_labels(
    db: &Connection,
    interval_id: i64,
    enrichment: &FocusEnrichment,
    labeler_version: i64,
) -> Result<()> {
    let (repository, project): (Option<i64>, Option<i64>) = db.query_row(
        "SELECT repository_entity_id,project_entity_id FROM focus_context_intervals WHERE id=?",
        params![interval_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let confidence = enrichment
        .terminal_match_confidence
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    let tool = enrichment
        .tool
        .as_deref()
        .map(|name| upsert_entity(db, "tool", name, name, now_ms()).map(|value| value.0))
        .transpose()?;
    let agent_session_value = enrichment.agent_session_id.map(|id| id.to_string());
    db.execute(
        "DELETE FROM focus_interval_labels WHERE context_interval_id=? AND labeler_version=? AND source LIKE 'integration:%'",
        params![interval_id, labeler_version],
    )?;
    for (dimension, value, entity, source, label_confidence) in [
        (
            "repository",
            None,
            repository,
            "integration:repository",
            confidence,
        ),
        ("project", None, project, "integration:project", confidence),
        ("tool", None, tool, "integration:terminal", confidence),
        (
            "agent.harness",
            enrichment.agent_type.as_deref(),
            None,
            "integration:presence",
            enrichment.agent_match_confidence.unwrap_or(confidence),
        ),
        (
            "agent.session",
            agent_session_value.as_deref(),
            None,
            "integration:presence",
            enrichment.agent_match_confidence.unwrap_or(confidence),
        ),
    ] {
        if value.is_none() && entity.is_none() {
            continue;
        }
        let identity = value
            .map(str::to_string)
            .unwrap_or_else(|| format!("entity:{}", entity.unwrap()));
        let key = hash_parts(&[
            b"integration-label",
            dimension.as_bytes(),
            identity.as_bytes(),
        ]);
        db.execute("INSERT OR IGNORE INTO focus_interval_labels(context_interval_id,dimension,label_key,value,entity_id,confidence,source,labeler_version,evidence_json)VALUES(?,?,?,?,?,?,?,?,?)",params![interval_id,dimension,key.as_slice(),value,entity,label_confidence.clamp(0.0,1.0),source,labeler_version,json!({"exactIntegration":true}).to_string()])?;
    }
    materialize_activity(db, interval_id, labeler_version)?;
    Ok(())
}

fn condition_matches(
    db: &Connection,
    condition: &Condition,
    facts: &FocusFacts,
    enrichment: &FocusEnrichment,
) -> bool {
    let entity_id = |kind: &str, entity: &Option<([u8; 32], String)>| -> Option<i64> {
        let key = entity.as_ref()?.0;
        db.query_row(
            "SELECT id FROM context_entities WHERE kind=? AND canonical_key=?",
            params![kind, key.as_slice()],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
    };
    let actual = match condition.fact.as_str() {
        "app.id" => Some(json!(facts.focused_app)),
        "window.title" => facts.focused_title.as_ref().map(|value| json!(value)),
        "terminal.session" => enrichment.tmux_session.as_ref().map(|value| json!(value)),
        "terminal.window" => enrichment.tmux_window.as_ref().map(|value| json!(value)),
        "terminal.command" => enrichment
            .foreground_executable
            .as_ref()
            .map(|value| json!(value)),
        "repository.entity" if condition.operator == "entityRelationship" => {
            entity_id("repository", &enrichment.repository).map(|value| json!(value))
        }
        "repository.entity" => enrichment
            .repository
            .as_ref()
            .map(|(_, value)| json!(value)),
        "project.entity" if condition.operator == "entityRelationship" => {
            entity_id("project", &enrichment.project).map(|value| json!(value))
        }
        "project.entity" => enrichment.project.as_ref().map(|(_, value)| json!(value)),
        "agent.type" => enrichment.agent_type.as_ref().map(|value| json!(value)),
        "agent.session" => enrichment
            .native_agent_session_id
            .as_ref()
            .map(|value| json!(value)),
        "agent.state" => enrichment.agent_state.as_ref().map(|value| json!(value)),
        _ => None,
    };
    let shared = crate::activity::RuleCondition {
        fact: condition.fact.clone(),
        operator: condition.operator.clone(),
        value: condition.value.clone(),
    };
    crate::activity::condition_matches_fact(Some(db), &shared, actual.as_ref()).unwrap_or(false)
}

fn materialize_activity(db: &Connection, interval_id: i64, labeler_version: i64) -> Result<()> {
    type Effective = (Option<String>, Option<i64>, f64, bool);
    type Manual = (i64, i64, i64, String, Option<String>, Option<i64>);
    let (start_ts, end_ts, agent_session_id, context_key): (i64, i64, Option<i64>, Vec<u8>) = db.query_row(
        "SELECT start_ts,end_ts,agent_session_id,context_key FROM focus_context_intervals WHERE id=?",
        params![interval_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let mut computed = HashMap::<String, Effective>::new();
    {
        let mut statement = db.prepare(
            "SELECT dimension,value,entity_id,confidence FROM focus_interval_labels WHERE context_interval_id=? AND labeler_version=? ORDER BY id",
        )?;
        for row in statement.query_map(params![interval_id, labeler_version], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, f64>(3)?,
            ))
        })? {
            let (dimension, value, entity, confidence) = row?;
            computed
                .entry(dimension)
                .or_insert((value, entity, confidence, false));
        }
    }
    let manuals = {
        let mut statement = db.prepare(
            "SELECT id,start_ts,end_ts,dimension,value,entity_id FROM manual_activity_labels WHERE start_ts<? AND end_ts>? ORDER BY created_ts DESC,id DESC",
        )?;
        let rows = statement
            .query_map(params![end_ts.max(start_ts + 1), start_ts], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<Manual>>>()?;
        rows
    };
    let mut boundaries = vec![start_ts, end_ts];
    for (_, manual_start, manual_end, _, _, _) in &manuals {
        boundaries.push((*manual_start).clamp(start_ts, end_ts));
        boundaries.push((*manual_end).clamp(start_ts, end_ts));
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let ranges = if start_ts == end_ts {
        vec![(start_ts, end_ts)]
    } else {
        boundaries
            .windows(2)
            .filter_map(|pair| (pair[1] > pair[0]).then_some((pair[0], pair[1])))
            .collect()
    };
    let episode_key: Vec<u8> = db.query_row(
        "SELECT activity.episode_key FROM activity_intervals activity JOIN focus_context_intervals prior ON prior.id=activity.source_context_interval_id WHERE activity.source='os_focus' AND activity.labeler_version=? AND activity.end_ts=? AND prior.closed_reason='local_midnight' AND prior.context_key=? AND activity.episode_key IS NOT NULL ORDER BY activity.id DESC LIMIT 1",
        params![labeler_version, start_ts, context_key],
        |row| row.get(0),
    ).optional()?.unwrap_or_else(|| hash_parts(&[b"os-focus-episode", &interval_id.to_be_bytes(), &start_ts.to_be_bytes()]).to_vec());

    db.execute(
        "DELETE FROM activity_intervals WHERE source='os_focus' AND source_context_interval_id=? AND labeler_version=?",
        params![interval_id, labeler_version],
    )?;
    for (segment_start, segment_end) in ranges {
        let mut effective = computed.clone();
        let mut manual_ids = Vec::new();
        for (manual_id, manual_start, manual_end, dimension, value, entity) in &manuals {
            if *manual_start < segment_end.max(segment_start + 1) && *manual_end > segment_start {
                if !effective.get(dimension).is_some_and(|entry| entry.3) {
                    effective.insert(dimension.clone(), (value.clone(), *entity, 1.0, true));
                    manual_ids.push(*manual_id);
                }
            }
        }
        let activity = effective
            .get("activity.primary")
            .and_then(|value| value.0.clone());
        let project = effective.get("project").and_then(|value| value.1);
        let service = effective.get("service").and_then(|value| value.1);
        let tool = effective.get("tool").and_then(|value| value.1);
        let confidence = effective
            .get("activity.primary")
            .map(|value| value.2)
            .or_else(|| {
                effective
                    .values()
                    .map(|value| value.2)
                    .max_by(f64::total_cmp)
            })
            .unwrap_or(0.0);
        let evidence = json!({"manualOverride": !manual_ids.is_empty(), "manualLabelIds": manual_ids, "contextIntervalId": interval_id}).to_string();
        db.execute(
            "INSERT INTO activity_intervals(start_ts,end_ts,source,source_context_interval_id,activity,project_entity_id,service_entity_id,tool_entity_id,agent_session_id,confidence,labeler_version,evidence_json,episode_key) VALUES(?,?,'os_focus',?,?,?,?,?,?,?,?,?,?)",
            params![segment_start,segment_end,interval_id,activity,project,service,tool,agent_session_id,confidence,labeler_version,evidence,episode_key],
        )?;
        let activity_id = db.last_insert_rowid();
        db.execute(
            "INSERT OR IGNORE INTO activity_context_sources(activity_interval_id,context_interval_id) VALUES(?,?)",
            params![activity_id, interval_id],
        )?;
    }
    Ok(())
}

pub(crate) fn enqueue_relabel_job(
    db: &Connection,
    from_ts: i64,
    to_ts: i64,
    entity_id: Option<i64>,
) -> Result<i64> {
    if to_ts <= from_ts {
        return Err(anyhow!("relabel range end must be after start"));
    }
    let revision = current_rules_revision(db)?;
    let base_version = active_labeler_version(db)?;
    let target_version: i64 = db.query_row(
        "SELECT COALESCE(MAX(version),?) + 1 FROM (SELECT active_labeler_version AS version FROM context_runtime_state UNION ALL SELECT labeler_version FROM relabel_jobs UNION ALL SELECT labeler_version FROM labeler_runs)",
        params![INITIAL_LABELER_VERSION],
        |row| row.get(0),
    )?;
    db.execute(
        "INSERT INTO relabel_jobs (rules_revision,labeler_version,base_labeler_version,range_start_ts,range_end_ts,entity_id,cursor_start_ts,status,created_ts) VALUES (?,?,?,?,?,?,?,'queued',?)",
        params![revision, target_version, base_version, from_ts, to_ts, entity_id, from_ts.saturating_sub(1), now_ms()],
    )?;
    Ok(db.last_insert_rowid())
}

fn load_focus_enrichment(db: &Connection, id: i64) -> Result<FocusEnrichment> {
    db.query_row("SELECT f.terminal_kind,f.terminal_match_method,f.terminal_match_confidence,f.terminal_candidate_count,f.tmux_client_pid,f.tmux_session,f.tmux_window,f.tmux_pane,f.tmux_pane_pid,f.terminal_cwd_key,f.terminal_cwd_display,f.foreground_executable,re.canonical_key,re.display_name,pe.canonical_key,pe.display_name,f.agent_type,f.agent_process_pid,f.agent_session_id,f.native_agent_session_id,f.agent_state,f.agent_match_method,f.agent_match_confidence,f.ambiguity_json FROM focus_context_intervals f LEFT JOIN context_entities re ON re.id=f.repository_entity_id LEFT JOIN context_entities pe ON pe.id=f.project_entity_id WHERE f.id=?",params![id],|r|{let cwd:Option<Vec<u8>>=r.get(9)?;let repository_key:Option<Vec<u8>>=r.get(12)?;let project_key:Option<Vec<u8>>=r.get(14)?;Ok(FocusEnrichment{terminal_kind:r.get(0)?,terminal_match_method:r.get(1)?,terminal_match_confidence:r.get(2)?,terminal_candidate_count:r.get(3)?,tmux_client_pid:r.get(4)?,tmux_session:r.get(5)?,tmux_window:r.get(6)?,tmux_pane:r.get(7)?,tmux_pane_pid:r.get(8)?,cwd_key:cwd.and_then(|v|v.try_into().ok()),cwd_display:r.get(10)?,foreground_executable:r.get(11)?,tool:r.get(11)?,repository:repository_key.and_then(|v|v.try_into().ok()).zip(r.get::<_,Option<String>>(13)?),project:project_key.and_then(|v|v.try_into().ok()).zip(r.get::<_,Option<String>>(15)?),agent_type:r.get(16)?,agent_process_pid:r.get(17)?,agent_session_id:r.get(18)?,native_agent_session_id:r.get(19)?,agent_state:r.get(20)?,agent_match_method:r.get(21)?,agent_match_confidence:r.get(22)?,ambiguity_json:r.get(23)?})}) .map_err(Into::into)
}

pub(crate) fn process_relabel_job_batch(
    db: &mut Connection,
    job_id: i64,
    limit: usize,
) -> Result<usize> {
    let limit = limit.clamp(1, MAX_RELABEL_BATCH);
    let job = db.query_row(
        "SELECT rules_revision,range_start_ts,range_end_ts,cursor_start_ts,cursor_interval_id,status,labeler_run_id,labeler_version,COALESCE(base_labeler_version,1),entity_id,build_phase,clone_label_cursor,clone_activity_cursor FROM relabel_jobs WHERE id=?",
        params![job_id],
        |row| Ok((row.get::<_, i64>(0)?,row.get::<_, i64>(1)?,row.get::<_, i64>(2)?,row.get::<_, Option<i64>>(3)?,row.get::<_, i64>(4)?,row.get::<_, String>(5)?,row.get::<_, Option<i64>>(6)?,row.get::<_, i64>(7)?,row.get::<_, i64>(8)?,row.get::<_, Option<i64>>(9)?,row.get::<_, String>(10)?,row.get::<_, i64>(11)?,row.get::<_, i64>(12)?)),
    )?;
    if job.5 == "completed" {
        return Ok(0);
    }
    if job.5 == "failed" {
        return Err(anyhow!("relabel job has failed"));
    }
    if current_rules_revision(db)? != job.0 || active_labeler_version(db)? != job.8 {
        let tx = db.transaction()?;
        let ts = now_ms();
        tx.execute("UPDATE relabel_jobs SET status='failed',build_phase='failed',completed_ts=?,error='rules revision or active version changed' WHERE id=?", params![ts, job_id])?;
        if let Some(run_id) = job.6 {
            tx.execute("UPDATE labeler_runs SET status='failed',completed_ts=?,error='revision mismatch' WHERE id=?", params![ts, run_id])?;
        }
        enqueue_version_prune(&tx, job.7, ts)?;
        tx.commit()?;
        return Err(anyhow!(
            "rules revision or active label version changed during relabel"
        ));
    }
    if job.6.is_none() {
        let tx = db.transaction()?;
        let ts = now_ms();
        tx.execute(
            "INSERT INTO labeler_runs(labeler_version,rules_revision,started_ts,range_start_ts,range_end_ts,status) VALUES(?,?,?,?,?,'running')",
            params![job.7,job.0,ts,job.1,job.2],
        )?;
        let run_id = tx.last_insert_rowid();
        tx.execute("UPDATE relabel_jobs SET labeler_run_id=?,status='running',started_ts=COALESCE(started_ts,?) WHERE id=? AND labeler_run_id IS NULL", params![run_id,ts,job_id])?;
        tx.commit()?;
        return Ok(0);
    }
    if job.10 == "clone_labels" {
        return clone_label_version_batch(db, job_id, job.6.unwrap(), job.8, job.7, job.11, limit);
    }
    if job.10 == "clone_activity" {
        return clone_activity_version_batch(db, job_id, job.8, job.7, job.12, limit);
    }
    if job.10 == "switch" {
        return switch_relabel_version(db, job_id, job.6.unwrap(), job.0, job.8, job.7);
    }
    if job.10 != "relabel" {
        return Err(anyhow!("relabel job has invalid build phase {}", job.10));
    }

    let rules = load_rules(db)?;
    let cursor_start = job.3.unwrap_or(i64::MIN);
    let entity_filter = job.9;
    let mut statement = db.prepare(
        "SELECT id,start_ts,focused_app,focused_title,focused_pid FROM focus_context_intervals f WHERE f.end_ts>=? AND f.start_ts<? AND (f.start_ts>? OR (f.start_ts=? AND f.id>?)) AND (? IS NULL OR f.repository_entity_id=? OR f.project_entity_id=? OR EXISTS(SELECT 1 FROM focus_interval_labels l WHERE l.context_interval_id=f.id AND l.labeler_version=? AND l.entity_id=?)) ORDER BY f.start_ts,f.id LIMIT ?",
    )?;
    let rows = statement
        .query_map(
            params![
                job.1,
                job.2,
                cursor_start,
                cursor_start,
                job.4,
                entity_filter,
                entity_filter,
                entity_filter,
                job.8,
                entity_filter,
                limit as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    FocusFacts {
                        focused_app: row.get(2)?,
                        focused_title: row.get(3)?,
                        focused_pid: row.get(4)?,
                    },
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    let mut prepared = Vec::with_capacity(rows.len());
    for (id, start_ts, facts) in rows {
        let enrichment = load_focus_enrichment(db, id)?;
        let plan = LabelPlan {
            revision: job.0,
            labels: evaluate_rules(db, &facts, &enrichment, load_rules_from_compiled(&rules))?,
        };
        prepared.push((id, start_ts, plan, enrichment));
    }

    let tx = db.transaction()?;
    let state: (i64, i64) = tx.query_row(
        "SELECT rules_revision,active_labeler_version FROM context_runtime_state WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if state != (job.0, job.8) {
        return Err(anyhow!("relabel compare-and-swap precondition changed"));
    }
    let run_id = job
        .6
        .ok_or_else(|| anyhow!("relabel run was not initialized"))?;
    for (interval_id, _, plan, enrichment) in &prepared {
        apply_label_plan(&tx, *interval_id, plan, Some(run_id), job.7)?;
        apply_enrichment_labels(&tx, *interval_id, enrichment, job.7)?;
    }
    if let Some((last_id, last_start, _, _)) = prepared.last() {
        tx.execute("UPDATE relabel_jobs SET cursor_start_ts=?,cursor_interval_id=?,processed_intervals=processed_intervals+? WHERE id=?", params![last_start,last_id,prepared.len() as i64,job_id])?;
    }
    if prepared.len() < limit {
        tx.execute(
            "UPDATE relabel_jobs SET build_phase='switch' WHERE id=? AND build_phase='relabel'",
            params![job_id],
        )?;
    }
    tx.commit()?;
    Ok(prepared.len())
}

fn switch_relabel_version(
    db: &mut Connection,
    job_id: i64,
    run_id: i64,
    rules_revision: i64,
    base_version: i64,
    target_version: i64,
) -> Result<usize> {
    let tx = db.transaction()?;
    let ts = now_ms();
    let switched = tx.execute(
        "UPDATE context_runtime_state SET active_labeler_version=?,updated_ts=? WHERE id=1 AND rules_revision=? AND active_labeler_version=?",
        params![target_version,ts,rules_revision,base_version],
    )?;
    if switched != 1 {
        return Err(anyhow!("relabel active-version compare-and-swap failed"));
    }
    tx.execute(
        "UPDATE relabel_jobs SET status='completed',build_phase='completed',completed_ts=? WHERE id=? AND status='running' AND build_phase='switch'",
        params![ts,job_id],
    )?;
    tx.execute(
        "UPDATE labeler_runs SET status='completed',completed_ts=? WHERE id=? AND status='running'",
        params![ts, run_id],
    )?;
    enqueue_version_prune(&tx, base_version, ts)?;
    tx.commit()?;
    Ok(0)
}

fn clone_label_version_batch(
    db: &mut Connection,
    job_id: i64,
    run_id: i64,
    base_version: i64,
    target_version: i64,
    cursor: i64,
    limit: usize,
) -> Result<usize> {
    type LabelRow = (
        i64,
        i64,
        String,
        Vec<u8>,
        Option<String>,
        Option<i64>,
        f64,
        String,
        Option<i64>,
        String,
    );
    let rows = {
        let mut statement = db.prepare(
            "SELECT id,context_interval_id,dimension,label_key,value,entity_id,confidence,source,rule_id,evidence_json FROM focus_interval_labels WHERE labeler_version=? AND id>? ORDER BY id LIMIT ?",
        )?;
        let rows = statement
            .query_map(params![base_version, cursor, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<LabelRow>>>()?;
        rows
    };
    let tx = db.transaction()?;
    ensure_relabel_phase(&tx, job_id, "clone_labels", base_version)?;
    for (_, context_id, dimension, key, value, entity_id, confidence, source, rule_id, evidence) in
        &rows
    {
        tx.execute(
            "INSERT OR IGNORE INTO focus_interval_labels(context_interval_id,dimension,label_key,value,entity_id,confidence,source,rule_id,labeler_run_id,labeler_version,evidence_json) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![context_id,dimension,key,value,entity_id,confidence,source,rule_id,run_id,target_version,evidence],
        )?;
    }
    if let Some((last_id, ..)) = rows.last() {
        tx.execute("UPDATE relabel_jobs SET clone_label_cursor=? WHERE id=? AND build_phase='clone_labels'", params![last_id,job_id])?;
    }
    if rows.len() < limit {
        tx.execute("UPDATE relabel_jobs SET build_phase='clone_activity' WHERE id=? AND build_phase='clone_labels'", params![job_id])?;
    }
    tx.commit()?;
    Ok(rows.len())
}

fn clone_activity_version_batch(
    db: &mut Connection,
    job_id: i64,
    base_version: i64,
    target_version: i64,
    cursor: i64,
    limit: usize,
) -> Result<usize> {
    type ActivityRow = (
        i64,
        i64,
        i64,
        String,
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        f64,
        String,
        Option<Vec<u8>>,
    );
    let rows = {
        let mut statement = db.prepare(
            "SELECT id,start_ts,end_ts,source,source_context_interval_id,activity,project_entity_id,service_entity_id,tool_entity_id,agent_session_id,confidence,evidence_json,episode_key FROM activity_intervals WHERE source='os_focus' AND labeler_version=? AND id>? ORDER BY id LIMIT ?",
        )?;
        let rows = statement
            .query_map(params![base_version, cursor, limit as i64], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<ActivityRow>>>()?;
        rows
    };
    let tx = db.transaction()?;
    ensure_relabel_phase(&tx, job_id, "clone_activity", base_version)?;
    for (
        _,
        start_ts,
        end_ts,
        source,
        context_id,
        activity,
        project,
        service,
        tool,
        session,
        confidence,
        evidence,
        episode_key,
    ) in &rows
    {
        tx.execute(
            "INSERT INTO activity_intervals(start_ts,end_ts,source,source_context_interval_id,activity,project_entity_id,service_entity_id,tool_entity_id,agent_session_id,confidence,labeler_version,evidence_json,episode_key) SELECT ?,?,?,?,?,?,?,?,?,?,?,?,? WHERE NOT EXISTS(SELECT 1 FROM activity_intervals target WHERE target.source=? AND target.labeler_version=? AND target.source_context_interval_id IS ? AND target.start_ts=? AND target.end_ts=?)",
            params![start_ts,end_ts,source,context_id,activity,project,service,tool,session,confidence,target_version,evidence,episode_key,source,target_version,context_id,start_ts,end_ts],
        )?;
    }
    if let Some((last_id, ..)) = rows.last() {
        tx.execute("UPDATE relabel_jobs SET clone_activity_cursor=? WHERE id=? AND build_phase='clone_activity'", params![last_id,job_id])?;
    }
    if rows.len() < limit {
        tx.execute("UPDATE relabel_jobs SET build_phase='relabel' WHERE id=? AND build_phase='clone_activity'", params![job_id])?;
    }
    tx.commit()?;
    Ok(rows.len())
}

fn ensure_relabel_phase(
    tx: &Transaction<'_>,
    job_id: i64,
    phase: &str,
    base_version: i64,
) -> Result<()> {
    let valid: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM relabel_jobs job JOIN context_runtime_state runtime ON runtime.id=1 WHERE job.id=? AND job.status='running' AND job.build_phase=? AND job.base_labeler_version=? AND job.rules_revision=runtime.rules_revision AND runtime.active_labeler_version=?)",
        params![job_id,phase,base_version,base_version],
        |row| row.get(0),
    )?;
    if !valid {
        return Err(anyhow!(
            "relabel phase compare-and-swap precondition changed"
        ));
    }
    Ok(())
}

fn enqueue_version_prune(db: &Connection, version: i64, ts: i64) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO derived_version_prune_jobs(labeler_version,status,created_ts) VALUES(?,'queued',?)",
        params![version,ts],
    )?;
    Ok(())
}

pub(crate) fn process_version_prune_batch(db: &mut Connection, limit: usize) -> Result<usize> {
    let limit = limit.clamp(1, MAX_RELABEL_BATCH);
    let job = db.query_row(
        "SELECT id,labeler_version,phase,label_cursor,activity_cursor FROM derived_version_prune_jobs WHERE status IN ('queued','running') ORDER BY id LIMIT 1",
        [],
        |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?)),
    ).optional()?;
    let Some((job_id, version, phase, label_cursor, activity_cursor)) = job else {
        return Ok(0);
    };
    if version == active_labeler_version(db)? {
        db.execute("UPDATE derived_version_prune_jobs SET status='failed',error='refused to prune active version',completed_ts=? WHERE id=?", params![now_ms(),job_id])?;
        return Err(anyhow!("refused to prune active labeler version"));
    }
    let (table, cursor_column, cursor, predicate) = if phase == "labels" {
        (
            "focus_interval_labels",
            "label_cursor",
            label_cursor,
            "labeler_version=? AND id>?",
        )
    } else {
        (
            "activity_intervals",
            "activity_cursor",
            activity_cursor,
            "source='os_focus' AND labeler_version=? AND id>?",
        )
    };
    let ids = {
        let mut statement = db.prepare(&format!(
            "SELECT id FROM {table} WHERE {predicate} ORDER BY id LIMIT ?"
        ))?;
        let ids = statement
            .query_map(params![version, cursor, limit as i64], |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids
    };
    let tx = db.transaction()?;
    let still_inactive: bool = tx.query_row(
        "SELECT active_labeler_version<>? FROM context_runtime_state WHERE id=1",
        params![version],
        |row| row.get(0),
    )?;
    if !still_inactive {
        return Err(anyhow!("prune version became active"));
    }
    if !ids.is_empty() {
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        tx.execute(
            &format!("DELETE FROM {table} WHERE id IN ({placeholders})"),
            params_from_iter(ids.iter().copied()),
        )?;
        let last = *ids.last().unwrap_or(&cursor);
        let deleted_column = if phase == "labels" {
            "deleted_labels"
        } else {
            "deleted_activity"
        };
        tx.execute(&format!("UPDATE derived_version_prune_jobs SET status='running',started_ts=COALESCE(started_ts,?),{cursor_column}=?,{deleted_column}={deleted_column}+? WHERE id=?"), params![now_ms(),last,ids.len() as i64,job_id])?;
    }
    if ids.len() < limit {
        if phase == "labels" {
            tx.execute("UPDATE derived_version_prune_jobs SET status='running',phase='activity',started_ts=COALESCE(started_ts,?) WHERE id=?", params![now_ms(),job_id])?;
        } else {
            tx.execute("UPDATE derived_version_prune_jobs SET status='completed',phase='completed',completed_ts=? WHERE id=?", params![now_ms(),job_id])?;
        }
    }
    tx.commit()?;
    Ok(ids.len())
}

fn load_rules_from_compiled(rules: &[Rule]) -> Vec<Rule> {
    rules
        .iter()
        .map(|rule| Rule {
            id: rule.id,
            name: rule.name.clone(),
            origin: rule.origin.clone(),
            conditions: rule.conditions.clone(),
            outputs: rule.outputs.clone(),
            stop_dimensions: rule.stop_dimensions.clone(),
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn relabel_intervals(
    db: &mut Connection,
    from_ts: i64,
    to_ts: i64,
    limit: usize,
) -> Result<usize> {
    let job_id = enqueue_relabel_job(db, from_ts, to_ts, None)?;
    loop {
        let (status, processed): (String, i64) = db.query_row(
            "SELECT status,processed_intervals FROM relabel_jobs WHERE id=?",
            params![job_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if status == "completed" {
            return Ok(processed.max(0) as usize);
        }
        process_relabel_job_batch(db, job_id, limit)?;
    }
}

pub(crate) fn close_open_context(
    db: &mut Connection,
    at_ts: i64,
    settings: CaptureSettings,
    reason: &str,
    force: bool,
) -> Result<bool> {
    let open = db.query_row(
        "SELECT id,last_observed_ts,end_ts FROM focus_context_intervals WHERE closed_reason IS NULL LIMIT 1",
        [],
        |row| Ok((row.get::<_, i64>(0)?,row.get::<_, i64>(1)?,row.get::<_, i64>(2)?)),
    ).optional()?;
    let Some((id, last_observed, end_ts)) = open else {
        return Ok(false);
    };
    if !force && at_ts - last_observed <= settings.continuity_ms() {
        return Ok(false);
    }
    let capped_end = end_ts.max(at_ts.min(last_observed.saturating_add(settings.continuity_ms())));
    let active_version = active_labeler_version(db)?;
    let tx = db.transaction()?;
    tx.execute(
        "UPDATE focus_context_intervals SET end_ts=?,closed_reason=? WHERE id=? AND closed_reason IS NULL",
        params![capped_end,reason,id],
    )?;
    materialize_activity(&tx, id, active_version)?;
    tx.commit()?;
    Ok(true)
}

pub(crate) fn backfill_historical_context_batch(
    db: &mut Connection,
    settings: CaptureSettings,
    limit: usize,
) -> Result<usize> {
    let limit = limit.clamp(1, 250);
    let (cursor, status): (i64, String) = db.query_row(
        "SELECT last_environment_id,status FROM context_backfill_state WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if status == "completed" {
        return Ok(0);
    }
    let cutoff: Option<i64> = db.query_row(
        "SELECT MIN(start_ts) FROM focus_context_intervals",
        [],
        |r| r.get(0),
    )?;
    let cutoff = cutoff.unwrap_or_else(|| now_ms() - settings.continuity_ms());
    let rows = {
        let mut stmt = db.prepare("SELECT id,ts,focused_app,focused_title,focused_pid FROM environment_samples WHERE id>? AND ts<? ORDER BY id LIMIT ?")?;
        let collected = stmt
            .query_map(params![cursor, cutoff, limit as i64], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    FocusFacts::new(
                        &row.get::<_, String>(2)?,
                        &row.get::<_, String>(3)?,
                        row.get(4)?,
                    ),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        collected
    };
    if rows.is_empty() {
        db.execute(
            "UPDATE context_backfill_state SET status='completed',updated_ts=? WHERE id=1",
            params![now_ms()],
        )?;
        return Ok(0);
    }
    struct HistoricalRun {
        start_id: i64,
        end_id: i64,
        start: i64,
        end: i64,
        facts: FocusFacts,
        plan: LabelPlan,
    }
    let mut raw: Vec<(i64, i64, i64, i64, FocusFacts)> = Vec::new();
    for (id, ts, facts) in &rows {
        if let Some(last) = raw.last_mut() {
            let same = last.4 == *facts;
            let continuous = *ts - last.3 <= settings.continuity_ms();
            let same_day = local_date(*ts) == local_date(last.3);
            if same && continuous && same_day {
                last.1 = *id;
                last.3 = *ts;
                continue;
            }
        }
        raw.push((*id, *id, *ts, *ts, facts.clone()));
    }
    let mut prepared = Vec::with_capacity(raw.len());
    for (start_id, end_id, start, end, facts) in raw {
        let mut plan = prepare_label_plan(db, &facts, &FocusEnrichment::default())?;
        for label in &mut plan.labels {
            label.confidence = (label.confidence * 0.5).min(0.5);
            label.source = format!("historical_title:{}", label.source);
        }
        prepared.push(HistoricalRun {
            start_id,
            end_id,
            start,
            end,
            facts,
            plan,
        });
    }
    let active_version = active_labeler_version(db)?;
    let tx = db.transaction()?;
    for run in prepared {
        let key = run.facts.context_key();
        tx.execute("INSERT INTO focus_context_intervals(start_ts,end_ts,last_observed_ts,start_environment_id,end_environment_id,context_key,extractor_version,closed_reason,focused_app,focused_title,focused_pid,ambiguity_json)VALUES(?,?,?,?,?,?,?,'historical_backfill',?,?,?,'{\"historicalBackfill\":true,\"confidence\":0.5}')",params![run.start,run.end,run.end,run.start_id,run.end_id,key.as_slice(),EXTRACTOR_VERSION,run.facts.focused_app,run.facts.focused_title,run.facts.focused_pid])?;
        let interval_id = tx.last_insert_rowid();
        apply_label_plan(&tx, interval_id, &run.plan, None, active_version)?;
    }
    let last_id = rows.last().map(|r| r.0).unwrap_or(cursor);
    let completed = rows.len() < limit;
    tx.execute("UPDATE context_backfill_state SET last_environment_id=?,processed_samples=processed_samples+?,status=?,updated_ts=? WHERE id=1",params![last_id,rows.len() as i64,if completed{"completed"}else{"running"},now_ms()])?;
    tx.commit()?;
    Ok(rows.len())
}

fn backfill_agent_session_links(db: &Connection) -> Result<usize> {
    let leases=db.execute("UPDATE agent_presence_leases SET agent_session_id=(SELECT se.id FROM agent_sessions se JOIN agent_sources s ON s.id=se.source_id WHERE s.agent_type=agent_presence_leases.harness AND s.source_key=agent_presence_leases.source_key AND se.native_session_id=agent_presence_leases.native_session_id) WHERE agent_session_id IS NULL AND EXISTS(SELECT 1 FROM agent_sessions se JOIN agent_sources s ON s.id=se.source_id WHERE s.agent_type=agent_presence_leases.harness AND s.source_key=agent_presence_leases.source_key AND se.native_session_id=agent_presence_leases.native_session_id)",[])?;
    let contexts=db.execute("UPDATE focus_context_intervals SET agent_session_id=(SELECT se.id FROM agent_sessions se JOIN agent_sources s ON s.id=se.source_id WHERE s.agent_type=focus_context_intervals.agent_type AND s.source_key='local' AND se.native_session_id=focus_context_intervals.native_agent_session_id) WHERE agent_session_id IS NULL AND native_agent_session_id IS NOT NULL AND EXISTS(SELECT 1 FROM agent_sessions se JOIN agent_sources s ON s.id=se.source_id WHERE s.agent_type=focus_context_intervals.agent_type AND s.source_key='local' AND se.native_session_id=focus_context_intervals.native_agent_session_id)",[])?;
    Ok(leases + contexts)
}

pub(crate) fn spawn_context_maintenance(db_path: std::path::PathBuf, settings: CaptureSettings) {
    std::thread::spawn(move || {
        let mut db = match Connection::open(&db_path) {
            Ok(mut db) => {
                if crate::db::init_db(&mut db).is_err() {
                    return;
                }
                db
            }
            Err(_) => return,
        };
        let mut last_version = -1_i64;
        let mut backfill_pending = true;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            let maintenance_now = now_ms();
            let _ = close_open_context(
                &mut db,
                maintenance_now,
                settings,
                "continuity_expired",
                false,
            );
            let _ = crate::browser_api::expire_stale_states(&db, maintenance_now, 100);
            // Keep lease/continuity expiry at 5 s; expensive reconciliation is
            // dirty-driven and naturally coalesces with collector/indexer writes.
            let version = db.query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0)).unwrap_or(-1);
            if backfill_pending || version != last_version {
                backfill_pending = backfill_historical_context_batch(&mut db, settings, 100).map(|n| n > 0).unwrap_or(true);
                let _ = backfill_agent_session_links(&db);
                last_version = version;
            }
            let job_id = db.query_row(
                "SELECT id FROM relabel_jobs WHERE status IN ('queued','running') ORDER BY id LIMIT 1",
                [],
                |row| row.get::<_,i64>(0),
            ).optional().ok().flatten();
            if let Some(job_id) = job_id {
                let _ = process_relabel_job_batch(&mut db, job_id, MAX_RELABEL_BATCH);
            }
            let _ = process_version_prune_batch(&mut db, MAX_RELABEL_BATCH);
        }
    });
}

pub(crate) fn prune_context(db: &Connection, cutoff: i64) -> Result<()> {
    db.execute(
        "DELETE FROM activity_intervals WHERE end_ts < ? AND NOT EXISTS (SELECT 1 FROM activity_context_sources m JOIN focus_context_intervals f ON f.id=m.context_interval_id WHERE m.activity_interval_id=activity_intervals.id AND f.closed_reason IS NULL) AND (source_context_interval_id IS NULL OR source_context_interval_id IN (SELECT id FROM focus_context_intervals WHERE closed_reason IS NOT NULL))",
        params![cutoff],
    )?;
    db.execute(
        "DELETE FROM focus_context_intervals WHERE closed_reason IS NOT NULL AND end_ts < ?",
        params![cutoff],
    )?;
    Ok(())
}

pub(crate) fn latest_context_json(db: &Connection) -> Result<Value> {
    let interval = db.query_row(
        "SELECT id,start_ts,end_ts,last_observed_ts,extractor_version,closed_reason,focused_app,focused_title,focused_pid,terminal_kind,terminal_match_method,terminal_match_confidence,terminal_candidate_count,tmux_session,tmux_window,tmux_pane,terminal_cwd_display,foreground_executable,repository_entity_id,project_entity_id,agent_type,agent_process_pid,agent_session_id,native_agent_session_id,agent_state,agent_match_method,agent_match_confidence,ambiguity_json FROM focus_context_intervals ORDER BY last_observed_ts DESC,id DESC LIMIT 1",
        [],
        |row| Ok(json!({
            "id": row.get::<_, i64>(0)?, "startTs": row.get::<_, i64>(1)?, "endTs": row.get::<_, i64>(2)?,
            "lastObservedTs": row.get::<_, i64>(3)?, "extractorVersion": row.get::<_, i64>(4)?,
            "closedReason": row.get::<_, Option<String>>(5)?, "focusedApp": row.get::<_, String>(6)?,
            "focusedTitle": row.get::<_, Option<String>>(7)?, "focusedPid": row.get::<_, Option<i64>>(8)?,
            "terminalKind":row.get::<_,Option<String>>(9)?,"terminalMatchMethod":row.get::<_,Option<String>>(10)?,"terminalMatchConfidence":row.get::<_,Option<f64>>(11)?,"terminalCandidateCount":row.get::<_,Option<i64>>(12)?,
            "tmuxSession":row.get::<_,Option<String>>(13)?,"tmuxWindow":row.get::<_,Option<String>>(14)?,"tmuxPane":row.get::<_,Option<String>>(15)?,"terminalCwdDisplay":row.get::<_,Option<String>>(16)?,"foregroundExecutable":row.get::<_,Option<String>>(17)?,
            "repositoryEntityId":row.get::<_,Option<i64>>(18)?,"projectEntityId":row.get::<_,Option<i64>>(19)?,"agentType":row.get::<_,Option<String>>(20)?,"agentProcessPid":row.get::<_,Option<i64>>(21)?,"agentSessionId":row.get::<_,Option<i64>>(22)?,"nativeAgentSessionId":row.get::<_,Option<String>>(23)?,"agentState":row.get::<_,Option<String>>(24)?,"agentMatchMethod":row.get::<_,Option<String>>(25)?,"agentMatchConfidence":row.get::<_,Option<f64>>(26)?,"ambiguity":serde_json::from_str::<Value>(&row.get::<_,String>(27)?).unwrap_or(json!({}))
        })),
    ).optional()?;
    let Some(mut interval) = interval else {
        return Ok(Value::Null);
    };
    let id = interval["id"].as_i64().unwrap_or_default();
    let mut stmt = db.prepare(
        "SELECT l.dimension,l.value,l.entity_id,e.kind,e.display_name,l.confidence,l.source,l.labeler_version FROM active_focus_interval_labels l LEFT JOIN context_entities e ON e.id=l.entity_id WHERE l.context_interval_id=? ORDER BY l.dimension,l.id",
    )?;
    let labels = stmt.query_map(params![id], |row| Ok(json!({
        "dimension": row.get::<_, String>(0)?, "value": row.get::<_, Option<String>>(1)?,
        "entityId": row.get::<_, Option<i64>>(2)?, "entityKind": row.get::<_, Option<String>>(3)?,
        "entityName": row.get::<_, Option<String>>(4)?, "confidence": row.get::<_, f64>(5)?,
        "source": row.get::<_, String>(6)?, "labelerVersion": row.get::<_, i64>(7)?
    })))?.collect::<rusqlite::Result<Vec<_>>>()?;
    interval["labels"] = Value::Array(labels);
    Ok(interval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;

    fn db() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        db
    }

    fn settings() -> CaptureSettings {
        CaptureSettings {
            poll_interval_ms: 30_000,
            suspend_gap_ms: 120_000,
        }
    }

    fn advance_relabel_to_phase(db: &mut Connection, job: i64, phase: &str, limit: usize) {
        for _ in 0..10_000 {
            let current: String = db
                .query_row(
                    "SELECT build_phase FROM relabel_jobs WHERE id=?",
                    params![job],
                    |row| row.get(0),
                )
                .unwrap();
            if current == phase {
                return;
            }
            process_relabel_job_batch(db, job, limit).unwrap();
        }
        panic!("relabel job did not reach phase {phase}");
    }

    fn drain_relabel(db: &mut Connection, job: i64, limit: usize) {
        for _ in 0..10_000 {
            let status: String = db
                .query_row(
                    "SELECT status FROM relabel_jobs WHERE id=?",
                    params![job],
                    |row| row.get(0),
                )
                .unwrap();
            if status == "completed" {
                return;
            }
            process_relabel_job_batch(db, job, limit).unwrap();
        }
        panic!("relabel job did not complete");
    }

    fn add_rule(
        db: &Connection,
        name: &str,
        priority: i64,
        origin: &str,
        app: &str,
        outputs: Value,
        stops: Value,
        ts: i64,
    ) {
        db.execute(
            "INSERT INTO label_rules (name,priority,origin,conditions_json,outputs_json,stop_dimensions_json,created_ts,updated_ts) VALUES (?,?,?,?,?,?,?,?)",
            params![name, priority, origin, json!([{"fact":"app.id","operator":"exact","value":app}]).to_string(), outputs.to_string(), stops.to_string(), ts, ts],
        ).unwrap();
    }

    #[test]
    fn migration_creates_context_schema_with_valid_foreign_keys() {
        let db = db();
        let tables: i64 = db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('context_entities','focus_context_intervals','labeler_runs','focus_interval_labels','manual_activity_labels','label_rules','activity_intervals')",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(tables, 7);
        let problems: i64 = db
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(problems, 0);
    }

    #[test]
    fn context_keys_are_deterministic_and_normalized() {
        let a = FocusFacts::new("  ORG.Example.App ", " Work   window ", Some(42));
        let b = FocusFacts::new("org.example.app", "Work window", Some(42));
        assert_eq!(a, b);
        assert_eq!(a.context_key(), b.context_key());
        assert_ne!(
            a.context_key(),
            FocusFacts::new("org.example.app", "Other", Some(42)).context_key()
        );
        assert_ne!(
            a.context_key(),
            FocusFacts::new("org.example.app", "Work window", Some(43)).context_key()
        );
    }

    #[test]
    fn unchanged_samples_extend_one_interval() {
        let mut db = db();
        let facts = FocusFacts::new("terminal", "editor", Some(10));
        assert!(matches!(
            capture_focus(&mut db, &facts, 1_000_000, None, settings()).unwrap(),
            CaptureOutcome::Started(_)
        ));
        assert!(matches!(
            capture_focus(&mut db, &facts, 1_030_000, None, settings()).unwrap(),
            CaptureOutcome::Extended(_)
        ));
        let (count, start, end): (i64, i64, i64) = db
            .query_row(
                "SELECT COUNT(*),MIN(start_ts),MAX(end_ts) FROM focus_context_intervals",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((count, start, end), (1, 1_000_000, 1_030_000));
        let labels: i64 = db
            .query_row("SELECT COUNT(*) FROM focus_interval_labels", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(labels, 0);
        let activities: i64 = db
            .query_row("SELECT COUNT(*) FROM activity_intervals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(activities, 1);
    }

    #[test]
    fn app_and_title_changes_close_and_open_intervals() {
        let mut db = db();
        capture_focus(
            &mut db,
            &FocusFacts::new("terminal", "one", Some(10)),
            1_000_000,
            None,
            settings(),
        )
        .unwrap();
        let outcome = capture_focus(
            &mut db,
            &FocusFacts::new("terminal", "two", Some(10)),
            1_030_000,
            None,
            settings(),
        )
        .unwrap();
        assert!(
            matches!(outcome, CaptureOutcome::Replaced { reason, .. } if reason == "facts_changed")
        );
        let rows: Vec<(i64, i64, Option<String>)> = db
            .prepare(
                "SELECT start_ts,end_ts,closed_reason FROM focus_context_intervals ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            rows,
            vec![
                (1_000_000, 1_030_000, Some("facts_changed".into())),
                (1_030_000, 1_030_000, None)
            ]
        );
    }

    #[test]
    fn equivalent_activity_merges_and_manual_relabel_splits_with_source_mapping() {
        let mut db = db();
        capture_focus(
            &mut db,
            &FocusFacts::new("app", "one", Some(1)),
            1_000,
            None,
            settings(),
        )
        .unwrap();
        capture_focus(
            &mut db,
            &FocusFacts::new("app", "two", Some(1)),
            2_000,
            None,
            settings(),
        )
        .unwrap();
        let (activities,mappings):(i64,i64)=db.query_row("SELECT (SELECT COUNT(*) FROM activity_intervals),(SELECT COUNT(*) FROM activity_context_sources)",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!((activities, mappings), (2, 2));
        db.execute("INSERT INTO manual_activity_labels(start_ts,end_ts,dimension,value,created_ts)VALUES(2000,3000,'activity.primary','work',1)",[]).unwrap();
        let job = enqueue_relabel_job(&db, 2_000, 2_001, None).unwrap();
        drain_relabel(&mut db, job, 10);
        let activities: i64 = db
            .query_row("SELECT COUNT(*) FROM active_activity_intervals", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(activities, 2);
        let work: String = db
            .query_row(
                "SELECT activity FROM active_activity_intervals WHERE activity IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(work, "work");
    }

    #[test]
    fn gaps_and_local_midnight_do_not_bridge_intervals() {
        let mut db = db();
        let facts = FocusFacts::new("terminal", "same", Some(10));
        capture_focus(&mut db, &facts, 1_000_000, None, settings()).unwrap();
        let gap = capture_focus(&mut db, &facts, 1_090_000, None, settings()).unwrap();
        assert!(
            matches!(gap, CaptureOutcome::Replaced { reason, .. } if reason == "monitoring_gap")
        );
        let old_end: i64 = db
            .query_row(
                "SELECT end_ts FROM focus_context_intervals WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_end, 1_000_000);

        let midnight = local_midnight_ms(now_ms());
        let before = midnight + 86_400_000 - 10_000;
        let suspend = capture_focus(&mut db, &facts, before, None, settings()).unwrap();
        assert!(
            matches!(suspend, CaptureOutcome::Replaced { reason, .. } if reason == "suspend_gap")
        );
        let next_midnight = local_midnight_ms(before + 20_000);
        let split = capture_focus(&mut db, &facts, before + 20_000, None, settings()).unwrap();
        assert!(
            matches!(split, CaptureOutcome::Replaced { reason, .. } if reason == "local_midnight")
        );
        let closed_end: i64 = db.query_row("SELECT end_ts FROM focus_context_intervals WHERE closed_reason='local_midnight' ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).unwrap();
        assert_eq!(closed_end, next_midnight);
    }

    #[test]
    fn rule_precedence_multivalue_entities_and_relabel_are_idempotent() {
        let mut db = db();
        add_rule(
            &db,
            "generic terminal",
            100,
            "built_in",
            "terminal",
            json!([
                {"dimension":"activity.primary","value":"work","confidence":0.7},
                {"dimension":"tool","entity":"tool:terminal","confidence":0.9}
            ]),
            json!([]),
            1,
        );
        add_rule(
            &db,
            "user choice",
            1,
            "user",
            "terminal",
            json!([
                {"dimension":"activity.primary","value":"administration","confidence":1.0},
                {"dimension":"activity.secondary","value":"coding","confidence":0.8},
                {"dimension":"activity.secondary","value":"research","confidence":0.6}
            ]),
            json!(["activity.primary"]),
            2,
        );
        capture_focus(
            &mut db,
            &FocusFacts::new("terminal", "editor", Some(10)),
            1_000_000,
            None,
            settings(),
        )
        .unwrap();
        let primary: String = db
            .query_row(
                "SELECT value FROM focus_interval_labels WHERE dimension='activity.primary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(primary, "administration");
        let secondary: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM focus_interval_labels WHERE dimension='activity.secondary'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(secondary, 2);
        let tool_entities: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM context_entities WHERE kind='tool'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tool_entities, 1);
        let before: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM active_focus_interval_labels",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(relabel_intervals(&mut db, 0, 2_000_000, 10).unwrap(), 1);
        assert_eq!(relabel_intervals(&mut db, 0, 2_000_000, 10).unwrap(), 1);
        let after: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM active_focus_interval_labels",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let activity_rows: i64 = db
            .query_row("SELECT COUNT(*) FROM active_activity_intervals", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(activity_rows, 1);
    }

    #[test]
    fn service_rule_does_not_imply_entertainment() {
        let mut db = db();
        add_rule(
            &db,
            "video service",
            10,
            "user",
            "browser",
            json!([
                {"dimension":"service","entity":"service:youtube.com","confidence":1.0},
                {"dimension":"activity.secondary","value":"video","confidence":0.9}
            ]),
            json!([]),
            1,
        );
        capture_focus(
            &mut db,
            &FocusFacts::new("browser", "Browser window", Some(20)),
            1_000,
            None,
            settings(),
        )
        .unwrap();
        let service: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM focus_interval_labels WHERE dimension='service'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let entertainment: i64 = db.query_row("SELECT COUNT(*) FROM focus_interval_labels WHERE dimension='activity.primary' AND value='entertainment'", [], |r| r.get(0)).unwrap();
        assert_eq!(service, 1);
        assert_eq!(entertainment, 0);
    }

    #[test]
    fn rule_revision_is_monotonic_when_timestamps_collide() {
        let db = db();
        let before = current_rules_revision(&db).unwrap();
        add_rule(&db, "one", 1, "user", "one", json!([]), json!([]), 7);
        let first = current_rules_revision(&db).unwrap();
        add_rule(&db, "two", 1, "user", "two", json!([]), json!([]), 7);
        let second = current_rules_revision(&db).unwrap();
        assert_eq!(first, before + 1);
        assert_eq!(second, first + 1);
    }

    #[test]
    fn rule_parsing_and_evaluation_are_read_only_before_write() {
        let db = db();
        add_rule(
            &db,
            "tool",
            1,
            "user",
            "terminal",
            json!([
                {"dimension":"tool","entity":"tool:terminal","confidence":1.0}
            ]),
            json!([]),
            1,
        );
        let plan = prepare_label_plan(
            &db,
            &FocusFacts::new("terminal", "shell", Some(1)),
            &FocusEnrichment::default(),
        )
        .unwrap();
        assert_eq!(plan.labels.len(), 1);
        let entities: i64 = db
            .query_row("SELECT COUNT(*) FROM context_entities", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(entities, 0, "compile phase must not perform writes");
    }

    #[test]
    fn relabel_job_cursor_progresses_across_batches() {
        let mut db = db();
        for i in 0..205_i64 {
            let key = hash_parts(&[b"cursor", &i.to_be_bytes()]);
            db.execute(
                "INSERT INTO focus_context_intervals (start_ts,end_ts,last_observed_ts,context_key,extractor_version,closed_reason,focused_app) VALUES (?,?,?,?,?,'fixture','app')",
                params![i * 10, i * 10 + 5, i * 10 + 5, key.as_slice(), EXTRACTOR_VERSION],
            ).unwrap();
        }
        let job = enqueue_relabel_job(&db, 0, 10_000, None).unwrap();
        advance_relabel_to_phase(&mut db, job, "relabel", 100);
        assert_eq!(process_relabel_job_batch(&mut db, job, 100).unwrap(), 100);
        let first_cursor: i64 = db
            .query_row(
                "SELECT cursor_interval_id FROM relabel_jobs WHERE id=?",
                params![job],
                |row| row.get(0),
            )
            .unwrap();
        assert!(first_cursor >= 100);
        assert_eq!(process_relabel_job_batch(&mut db, job, 100).unwrap(), 100);
        let second_cursor: i64 = db
            .query_row(
                "SELECT cursor_interval_id FROM relabel_jobs WHERE id=?",
                params![job],
                |row| row.get(0),
            )
            .unwrap();
        assert!(second_cursor > first_cursor);
        assert_eq!(process_relabel_job_batch(&mut db, job, 100).unwrap(), 5);
        assert_eq!(process_relabel_job_batch(&mut db, job, 100).unwrap(), 0);
        let (processed, status): (i64, String) = db
            .query_row(
                "SELECT processed_intervals,status FROM relabel_jobs WHERE id=?",
                params![job],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(processed, 205);
        assert_eq!(status, "completed");
    }

    #[test]
    fn stale_and_forced_closure_cap_open_intervals() {
        let mut db = db();
        capture_focus(
            &mut db,
            &FocusFacts::new("app", "title", Some(1)),
            1_000_000,
            None,
            settings(),
        )
        .unwrap();
        assert!(
            !close_open_context(&mut db, 1_030_000, settings(), "continuity_expired", false)
                .unwrap()
        );
        assert!(
            close_open_context(&mut db, 1_100_000, settings(), "continuity_expired", false)
                .unwrap()
        );
        let (end, reason): (i64, String) = db
            .query_row(
                "SELECT end_ts,closed_reason FROM focus_context_intervals",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(end, 1_045_000);
        assert_eq!(reason, "continuity_expired");

        capture_focus(
            &mut db,
            &FocusFacts::new("app", "again", Some(1)),
            1_110_000,
            None,
            settings(),
        )
        .unwrap();
        assert!(
            close_open_context(&mut db, 1_111_000, settings(), "tracking_disabled", true).unwrap()
        );
        let reason: String = db
            .query_row(
                "SELECT closed_reason FROM focus_context_intervals ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, "tracking_disabled");
    }

    #[test]
    fn historical_backfill_compacts_runs_with_low_confidence_provenance() {
        let mut db = db();
        add_rule(
            &db,
            "editor work",
            1,
            "heuristic",
            "editor",
            json!([{"dimension":"activity.primary","value":"work","confidence":0.9}]),
            json!([]),
            1,
        );
        for (sample, ts, app, title) in [
            (1, 1_000, "editor", "one"),
            (2, 1_030, "editor", "one"),
            (3, 1_060, "editor", "two"),
        ] {
            db.execute("INSERT INTO environment_samples(sample_id,ts,theme,theme_detail,brightness_source,audio_detail,video_detail,net_rx_mbps,net_tx_mbps,focused_app,focused_title,lid_detail,screen_lock_detail,fan_source,usb_power_detail)VALUES(?,?,'dark','','', '', '',0,0,?,?, '', '', '', '')",params![sample,ts,app,title]).unwrap();
        }
        capture_focus(
            &mut db,
            &FocusFacts::new("current", "now", Some(9)),
            10_000,
            None,
            settings(),
        )
        .unwrap();
        assert_eq!(
            backfill_historical_context_batch(&mut db, settings(), 100).unwrap(),
            3
        );
        let runs:i64=db.query_row("SELECT COUNT(*) FROM focus_context_intervals WHERE closed_reason='historical_backfill'",[],|r|r.get(0)).unwrap();
        assert_eq!(runs, 2);
        let (confidence,source):(f64,String)=db.query_row("SELECT confidence,source FROM focus_interval_labels WHERE source LIKE 'historical_title:%' LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert!(confidence <= 0.5);
        assert!(source.starts_with("historical_title:"));
        let open: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM focus_context_intervals WHERE closed_reason IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(open, 1);
    }

    #[test]
    fn partial_manual_label_segments_only_its_exact_boundary() {
        let mut db = db();
        let facts = FocusFacts::new("app", "one", Some(1));
        capture_focus(&mut db, &facts, 1_000, None, settings()).unwrap();
        capture_focus(&mut db, &facts, 1_100, None, settings()).unwrap();
        db.execute("INSERT INTO manual_activity_labels(start_ts,end_ts,dimension,value,created_ts) VALUES(1040,1060,'activity.primary','work',1)", []).unwrap();
        let job = enqueue_relabel_job(&db, 1_000, 1_101, None).unwrap();
        drain_relabel(&mut db, job, 10);
        let rows = db.prepare("SELECT start_ts,end_ts,activity FROM active_activity_intervals WHERE source='os_focus' ORDER BY start_ts").unwrap()
            .query_map([], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,Option<String>>(2)?))).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(
            rows,
            vec![
                (1_000, 1_040, None),
                (1_040, 1_060, Some("work".into())),
                (1_060, 1_100, None)
            ]
        );
    }

    #[test]
    fn middle_of_three_relabel_rebuilds_without_hulls_or_double_counting() {
        let mut db = db();
        for (title, ts) in [("a", 1_000), ("b", 2_000), ("c", 3_000), ("c", 4_000)] {
            capture_focus(
                &mut db,
                &FocusFacts::new("app", title, Some(1)),
                ts,
                None,
                settings(),
            )
            .unwrap();
        }
        db.execute("INSERT INTO manual_activity_labels(start_ts,end_ts,dimension,value,created_ts) VALUES(2000,3000,'activity.primary','work',1)", []).unwrap();
        let job = enqueue_relabel_job(&db, 2_000, 3_000, None).unwrap();
        drain_relabel(&mut db, job, 10);
        let rows = db.prepare("SELECT start_ts,end_ts,activity FROM active_activity_intervals WHERE source='os_focus' AND end_ts>start_ts ORDER BY start_ts,end_ts").unwrap()
            .query_map([], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,Option<String>>(2)?))).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(rows.iter().map(|row| row.1 - row.0).sum::<i64>(), 3_000);
        assert!(rows.windows(2).all(|pair| pair[0].1 <= pair[1].0));
        assert_eq!(
            rows.iter()
                .filter(|row| row.2.as_deref() == Some("work"))
                .map(|row| row.1 - row.0)
                .sum::<i64>(),
            1_000
        );
    }

    #[test]
    fn staged_relabel_crash_revision_mismatch_and_complete_switch_never_mix_readers() {
        let mut db = db();
        add_rule(
            &db,
            "classification",
            1,
            "user",
            "app",
            json!([{"dimension":"activity.primary","value":"work"}]),
            json!([]),
            1,
        );
        for (title, ts) in [("a", 1_000), ("b", 2_000), ("c", 3_000), ("c", 4_000)] {
            capture_focus(
                &mut db,
                &FocusFacts::new("app", title, Some(1)),
                ts,
                None,
                settings(),
            )
            .unwrap();
        }
        db.execute(
            "UPDATE label_rules SET outputs_json=?,updated_ts=2 WHERE name='classification'",
            params![json!([{"dimension":"activity.primary","value":"entertainment"}]).to_string()],
        )
        .unwrap();
        let failed_job = enqueue_relabel_job(&db, 0, 5_000, None).unwrap();
        advance_relabel_to_phase(&mut db, failed_job, "relabel", 1);
        assert_eq!(
            process_relabel_job_batch(&mut db, failed_job, 1).unwrap(),
            1
        );
        assert_eq!(active_labeler_version(&db).unwrap(), 1);
        let visible: i64 = db.query_row("SELECT COUNT(*) FROM active_activity_intervals WHERE source='os_focus' AND activity='work'", [], |row| row.get(0)).unwrap();
        assert_eq!(
            visible, 3,
            "a staged first batch must not leak through active readers"
        );
        let target: i64 = db
            .query_row(
                "SELECT labeler_version FROM relabel_jobs WHERE id=?",
                params![failed_job],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            db.query_row(
                "SELECT COUNT(*) FROM focus_interval_labels WHERE labeler_version=?",
                params![target],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
                > 0
        );
        db.execute(
            "UPDATE label_rules SET priority=priority+1,updated_ts=3 WHERE name='classification'",
            [],
        )
        .unwrap();
        assert!(process_relabel_job_batch(&mut db, failed_job, 1).is_err());
        assert_eq!(active_labeler_version(&db).unwrap(), 1);
        assert!(
            db.query_row(
                "SELECT COUNT(*) FROM focus_interval_labels WHERE labeler_version=?",
                params![target],
                |row| row.get::<_, i64>(0)
            )
            .unwrap()
                > 0
        );
        assert_eq!(db.query_row("SELECT COUNT(*) FROM derived_version_prune_jobs WHERE labeler_version=? AND status='queued'", params![target], |row| row.get::<_,i64>(0)).unwrap(),1);
        while db.query_row("SELECT status<>'completed' FROM derived_version_prune_jobs WHERE labeler_version=?", params![target], |row| row.get::<_,bool>(0)).unwrap() {
            assert!(process_version_prune_batch(&mut db,1).unwrap() <= 1);
        }

        let successful_job = enqueue_relabel_job(&db, 0, 7_000, None).unwrap();
        advance_relabel_to_phase(&mut db, successful_job, "relabel", 1);
        assert_eq!(
            process_relabel_job_batch(&mut db, successful_job, 1).unwrap(),
            1
        );
        capture_focus(
            &mut db,
            &FocusFacts::new("app", "d", Some(1)),
            5_000,
            None,
            settings(),
        )
        .unwrap();
        capture_focus(
            &mut db,
            &FocusFacts::new("app", "d", Some(1)),
            6_000,
            None,
            settings(),
        )
        .unwrap();
        drain_relabel(&mut db, successful_job, 1);
        let active = active_labeler_version(&db).unwrap();
        assert!(active > 1);
        assert_eq!(db.query_row("SELECT COUNT(*) FROM active_activity_intervals WHERE source='os_focus' AND activity='entertainment'", [], |row| row.get::<_,i64>(0)).unwrap(), 4);
        assert_eq!(db.query_row("SELECT COUNT(DISTINCT labeler_version) FROM active_activity_intervals WHERE source='os_focus'", [], |row| row.get::<_,i64>(0)).unwrap(), 1);
    }

    #[test]
    fn clone_crash_resumes_in_batches_and_post_switch_prune_is_bounded() {
        let mut db = db();
        add_rule(
            &db,
            "classification",
            1,
            "user",
            "app",
            json!([{"dimension":"activity.primary","value":"work"}]),
            json!([]),
            1,
        );
        for index in 0..7_i64 {
            capture_focus(
                &mut db,
                &FocusFacts::new("app", &format!("title-{index}"), Some(1)),
                1_000 + index * 1_000,
                None,
                settings(),
            )
            .unwrap();
        }
        capture_focus(
            &mut db,
            &FocusFacts::new("app", "title-6", Some(1)),
            8_000,
            None,
            settings(),
        )
        .unwrap();
        db.execute(
            "UPDATE label_rules SET outputs_json=?,updated_ts=2 WHERE name='classification'",
            params![json!([{"dimension":"activity.primary","value":"entertainment"}]).to_string()],
        )
        .unwrap();
        let job = enqueue_relabel_job(&db, 0, 10_000, None).unwrap();
        assert_eq!(
            process_relabel_job_batch(&mut db, job, 2).unwrap(),
            0,
            "run initialization is a tiny transaction"
        );
        assert_eq!(process_relabel_job_batch(&mut db, job, 2).unwrap(), 2);
        let (phase,cursor,target):(String,i64,i64)=db.query_row("SELECT build_phase,clone_label_cursor,labeler_version FROM relabel_jobs WHERE id=?",params![job],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(phase, "clone_labels");
        assert!(cursor > 0);
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM focus_interval_labels WHERE labeler_version=?",
                params![target],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(
            active_labeler_version(&db).unwrap(),
            1,
            "interrupted clone must stay hidden"
        );

        // Simulate restart by resuming only from persisted job/run cursors.
        drain_relabel(&mut db, job, 2);
        assert_eq!(active_labeler_version(&db).unwrap(), target);
        let old_labels_before = db
            .query_row(
                "SELECT COUNT(*) FROM focus_interval_labels WHERE labeler_version=1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let old_activity_before=db.query_row("SELECT COUNT(*) FROM activity_intervals WHERE source='os_focus' AND labeler_version=1",[],|row|row.get::<_,i64>(0)).unwrap();
        assert!(
            old_labels_before > 2 && old_activity_before > 2,
            "switch transaction must not prune old rows"
        );
        assert_eq!(
            db.query_row(
                "SELECT status FROM derived_version_prune_jobs WHERE labeler_version=1",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "queued"
        );

        let mut previous_labels = old_labels_before;
        let mut previous_activity = old_activity_before;
        for _ in 0..100 {
            let status: String = db
                .query_row(
                    "SELECT status FROM derived_version_prune_jobs WHERE labeler_version=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            if status == "completed" {
                break;
            }
            assert!(process_version_prune_batch(&mut db, 2).unwrap() <= 2);
            let labels = db
                .query_row(
                    "SELECT COUNT(*) FROM focus_interval_labels WHERE labeler_version=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap();
            let activity=db.query_row("SELECT COUNT(*) FROM activity_intervals WHERE source='os_focus' AND labeler_version=1",[],|row|row.get::<_,i64>(0)).unwrap();
            assert!(previous_labels - labels <= 2);
            assert!(previous_activity - activity <= 2);
            previous_labels = labels;
            previous_activity = activity;
        }
        assert_eq!(previous_labels, 0);
        assert_eq!(previous_activity, 0);
        assert_eq!(
            db.query_row(
                "SELECT status FROM derived_version_prune_jobs WHERE labeler_version=1",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "completed"
        );
    }

    #[test]
    fn exact_tool_and_agent_session_are_materialized_and_group_ready() {
        let mut db = db();
        db.execute("INSERT INTO agent_sources(agent_type,source_key,display_name,adapter_version,capabilities_json,limitations_json) VALUES('pi','local','Pi',1,'{}','{}')", []).unwrap();
        let source = db.last_insert_rowid();
        db.execute("INSERT INTO agent_sessions(source_id,native_session_id,created_ts) VALUES(?,'native',1)", params![source]).unwrap();
        let session = db.last_insert_rowid();
        let enrichment = FocusEnrichment {
            tool: Some("pi".into()),
            foreground_executable: Some("pi".into()),
            agent_type: Some("pi".into()),
            agent_session_id: Some(session),
            native_agent_session_id: Some("native".into()),
            agent_state: Some("busy".into()),
            agent_match_method: Some("presence_native_session".into()),
            agent_match_confidence: Some(1.0),
            ..Default::default()
        };
        let facts = FocusFacts::new("terminal", "work", Some(10));
        capture_focus_with_enrichment(&mut db, &facts, 1_000, None, settings(), &enrichment)
            .unwrap();
        capture_focus_with_enrichment(&mut db, &facts, 2_000, None, settings(), &enrichment)
            .unwrap();
        let (materialized_session,tool,kind):(Option<i64>,Option<i64>,String)=db.query_row("SELECT a.agent_session_id,a.tool_entity_id,e.kind FROM active_activity_intervals a JOIN context_entities e ON e.id=a.tool_entity_id WHERE a.end_ts>a.start_ts",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(materialized_session, Some(session));
        assert!(tool.is_some());
        assert_eq!(kind, "tool");
    }

    #[test]
    fn context_equality_includes_confidence_provenance_candidates_and_ambiguity() {
        let facts = FocusFacts::new("terminal", "work", Some(1));
        let base = FocusEnrichment {
            terminal_match_method: Some("exact".into()),
            terminal_match_confidence: Some(1.0),
            terminal_candidate_count: Some(1),
            agent_match_method: Some("presence".into()),
            agent_match_confidence: Some(1.0),
            ambiguity_json: "{\"b\":2,\"a\":1}".into(),
            ..Default::default()
        };
        let key = facts.context_key_with(&base);
        let mut changed = base.clone();
        changed.terminal_match_confidence = Some(0.8);
        assert_ne!(key, facts.context_key_with(&changed));
        changed = base.clone();
        changed.terminal_candidate_count = Some(2);
        assert_ne!(key, facts.context_key_with(&changed));
        changed = base.clone();
        changed.agent_match_method = Some("fallback".into());
        assert_ne!(key, facts.context_key_with(&changed));
        changed = base.clone();
        changed.ambiguity_json = "{\"a\":1,\"b\":2}".into();
        assert_eq!(key, facts.context_key_with(&changed));
        changed.ambiguity_json = "{\"a\":1,\"b\":3}".into();
        assert_ne!(key, facts.context_key_with(&changed));
    }

    #[test]
    fn midnight_activity_rows_stay_split_with_one_episode_identity() {
        let mut db = db();
        let midnight = next_local_midnight_ms(now_ms());
        let facts = FocusFacts::new("app", "same", Some(1));
        capture_focus(&mut db, &facts, midnight - 10_000, None, settings()).unwrap();
        capture_focus(&mut db, &facts, midnight + 10_000, None, settings()).unwrap();
        let rows=db.prepare("SELECT start_ts,end_ts,episode_key FROM active_activity_intervals WHERE source='os_focus' ORDER BY start_ts").unwrap().query_map([],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,Vec<u8>>(2)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, midnight);
        assert_eq!(rows[1].0, midnight);
        assert_eq!(rows[0].2, rows[1].2);
    }

    #[test]
    fn shared_rule_compiler_matches_capture_and_rejects_unadvertised_inputs() {
        let mut db = db();
        let input = crate::activity::RuleInput {
            name: "regex".into(),
            priority: 1,
            enabled: true,
            origin: "user".into(),
            conditions: vec![crate::activity::RuleCondition {
                fact: "window.title".into(),
                operator: "boundedRegex".into(),
                value: Some(json!("^Editor [0-9]+$")),
            }],
            outputs: vec![crate::activity::RuleOutput {
                dimension: "activity.primary".into(),
                value: Some("work".into()),
                entity: None,
                confidence: Some(0.9),
            }],
            stop_dimensions: vec![],
        };
        crate::activity::validate_rule(&input).unwrap();
        db.execute("INSERT INTO label_rules(name,priority,origin,conditions_json,outputs_json,created_ts,updated_ts) VALUES(?,?,?,?,?,1,1)",params![input.name,input.priority,input.origin,serde_json::to_string(&input.conditions).unwrap(),serde_json::to_string(&input.outputs).unwrap()]).unwrap();
        capture_focus(
            &mut db,
            &FocusFacts::new("editor", "Editor 42", Some(1)),
            1_000,
            None,
            settings(),
        )
        .unwrap();
        assert_eq!(
            db.query_row(
                "SELECT value FROM active_focus_interval_labels WHERE dimension='activity.primary'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "work"
        );
        let mut invalid = input;
        invalid.conditions[0].fact = "focus.pid".into();
        assert!(crate::activity::validate_rule(&invalid).is_err());
        let browser = crate::activity::RuleCondition {
            fact: "browser.domain".into(),
            operator: "domainSuffix".into(),
            value: Some(json!("youtube.com")),
        };
        assert!(crate::activity::condition_matches_fact(
            None,
            &browser,
            Some(&json!("www.youtube.com"))
        )
        .is_err());
    }

    #[test]
    fn context_retention_prunes_dependents_without_environment_dependency() {
        let mut db = db();
        capture_focus(
            &mut db,
            &FocusFacts::new("one", "old", Some(1)),
            1_000,
            None,
            settings(),
        )
        .unwrap();
        capture_focus(
            &mut db,
            &FocusFacts::new("two", "new", Some(2)),
            2_000,
            None,
            settings(),
        )
        .unwrap();
        prune_context(&db, 2_500).unwrap();
        let contexts: i64 = db
            .query_row("SELECT COUNT(*) FROM focus_context_intervals", [], |r| {
                r.get(0)
            })
            .unwrap();
        let activities: i64 = db
            .query_row("SELECT COUNT(*) FROM activity_intervals", [], |r| r.get(0))
            .unwrap();
        assert_eq!(contexts, 1);
        assert_eq!(activities, 1);
        let problems: i64 = db
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(problems, 0);
    }
}
