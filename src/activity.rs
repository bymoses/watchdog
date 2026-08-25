//! Query and repository core for Plan 002 activity analytics and policy decisions.
//!
//! This module deliberately owns no HTTP concerns. All writes are bounded and
//! transactional; all duration queries clip intervals to the requested range and
//! reconcile overlaps before aggregation so callers cannot accidentally double
//! count OS and integration sources.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

const MAX_QUERY_RANGE_MS: i64 = 5 * 366 * 86_400_000;
const MAX_TIMELINE_BUCKETS: usize = 2_000;
const MAX_RULE_CONDITIONS: usize = 16;
const MAX_RULE_OUTPUTS: usize = 16;
const MAX_RULE_STOPS: usize = 16;
const MAX_REGEX_PATTERN_CHARS: usize = 256;
const MAX_REGEX_INPUT_CHARS: usize = 512;
const MAX_REGEX_TOKENS: usize = 128;
const MAX_REGEX_REPEAT: usize = 512;
const MAX_REGEX_WORK: usize = 100_000;
const MAX_POLICY_LOOKBACK_MS: i64 = 366 * 86_400_000;
const ACTIVITY_VALUES: &[&str] = &[
    "work",
    "entertainment",
    "communication",
    "administration",
    "system",
    "idle",
    "unknown",
];
const DIMENSIONS: &[&str] = &[
    "activity.primary",
    "activity.secondary",
    "project",
    "repository",
    "service",
    "tool",
    "account",
    "agent.harness",
    "agent.session",
    "content",
];
const LABEL_FACTS: &[&str] = &[
    "app.id",
    "window.title",
    "terminal.session",
    "terminal.window",
    "terminal.command",
    "repository.entity",
    "project.entity",
    "agent.type",
    "agent.session",
    "agent.state",
    "account.entity",
];
const BOOLEAN_FACTS: &[&str] = &[];
const ENTITY_FACTS: &[&str] = &[
    "repository.entity",
    "project.entity",
    "agent.session",
    "account.entity",
];

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ActivityGroup {
    Activity,
    Project,
    Service,
    Tool,
    AgentSession,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfidenceDistribution {
    pub(crate) low_ms: i64,
    pub(crate) medium_ms: i64,
    pub(crate) high_ms: i64,
}

impl Default for ConfidenceDistribution {
    fn default() -> Self {
        Self {
            low_ms: 0,
            medium_ms: 0,
            high_ms: 0,
        }
    }
}

impl ConfidenceDistribution {
    fn add(&mut self, confidence: f64, duration: i64) {
        if confidence < 0.5 {
            self.low_ms += duration;
        } else if confidence < 0.8 {
            self.medium_ms += duration;
        } else {
            self.high_ms += duration;
        }
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AnalyticsMeta {
    pub(crate) from_ts: i64,
    pub(crate) to_ts: i64,
    pub(crate) metric: String,
    pub(crate) observed_ms: i64,
    pub(crate) unknown_ms: i64,
    pub(crate) monitoring_coverage: f64,
    pub(crate) confidence: ConfidenceDistribution,
    pub(crate) sample_count: usize,
    pub(crate) episode_count: usize,
    pub(crate) classifier_version: i64,
    pub(crate) rules_revision: i64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SummaryGroup {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) duration_ms: i64,
    pub(crate) episode_count: usize,
    pub(crate) confidence: ConfidenceDistribution,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivitySummary {
    pub(crate) group: ActivityGroup,
    pub(crate) groups: Vec<SummaryGroup>,
    pub(crate) meta: AnalyticsMeta,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SegmentProvenance {
    pub(crate) source: String,
    pub(crate) activity_interval_ids: Vec<i64>,
    pub(crate) source_context_interval_ids: Vec<i64>,
    pub(crate) evidence: Value,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivitySegment {
    pub(crate) start_ts: i64,
    pub(crate) end_ts: i64,
    pub(crate) activity: Option<String>,
    pub(crate) project_entity_id: Option<i64>,
    pub(crate) service_entity_id: Option<i64>,
    pub(crate) tool_entity_id: Option<i64>,
    pub(crate) agent_session_id: Option<i64>,
    pub(crate) confidence: f64,
    pub(crate) labeler_version: i64,
    pub(crate) episode_key: String,
    pub(crate) provenance: SegmentProvenance,
}

impl ActivitySegment {
    fn duration_ms(&self) -> i64 {
        self.end_ts.saturating_sub(self.start_ts)
    }

    fn same_effective_state(&self, other: &Self) -> bool {
        self.activity == other.activity
            && self.project_entity_id == other.project_entity_id
            && self.service_entity_id == other.service_entity_id
            && self.tool_entity_id == other.tool_entity_id
            && self.agent_session_id == other.agent_session_id
            && self.confidence.to_bits() == other.confidence.to_bits()
            && self.labeler_version == other.labeler_version
            && self.episode_key == other.episode_key
            && self.provenance.source == other.provenance.source
            && canonical_json(&self.provenance.evidence)
                == canonical_json(&other.provenance.evidence)
    }
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TimelineBucket {
    pub(crate) start_ts: i64,
    pub(crate) end_ts: i64,
    pub(crate) observed_ms: i64,
    pub(crate) unknown_ms: i64,
    pub(crate) groups_ms: BTreeMap<String, i64>,
    pub(crate) confidence: ConfidenceDistribution,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivityTimeline {
    pub(crate) resolution_ms: i64,
    pub(crate) buckets: Vec<TimelineBucket>,
    pub(crate) meta: AnalyticsMeta,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ActivityEpisode {
    pub(crate) episode_key: String,
    pub(crate) start_ts: i64,
    pub(crate) end_ts: i64,
    pub(crate) duration_ms: i64,
    pub(crate) activity: String,
    pub(crate) project_entity_id: Option<i64>,
    pub(crate) service_entity_id: Option<i64>,
    pub(crate) confidence: f64,
    pub(crate) crosses_local_midnight: bool,
    pub(crate) segment_count: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntityRecord {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) canonical_key_hex: String,
    pub(crate) display_name: String,
    pub(crate) metadata: Value,
    pub(crate) first_seen_ts: i64,
    pub(crate) last_seen_ts: i64,
    pub(crate) user_confirmed: bool,
    pub(crate) archived: bool,
    pub(crate) aliases: Vec<EntityAlias>,
    pub(crate) relations: Vec<EntityRelation>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntityAlias {
    pub(crate) id: i64,
    pub(crate) kind: String,
    pub(crate) value: String,
    pub(crate) source: String,
    pub(crate) confidence: f64,
    pub(crate) created_ts: i64,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntityRelation {
    pub(crate) parent_entity_id: i64,
    pub(crate) child_entity_id: i64,
    pub(crate) relation: String,
    pub(crate) confidence: f64,
    pub(crate) source: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ManualLabelInput {
    pub(crate) start_ts: i64,
    pub(crate) end_ts: i64,
    pub(crate) dimension: String,
    #[serde(default)]
    pub(crate) value: Option<String>,
    #[serde(default)]
    pub(crate) entity_id: Option<i64>,
    #[serde(default)]
    pub(crate) note: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct RuleCondition {
    pub(crate) fact: String,
    pub(crate) operator: String,
    #[serde(default)]
    pub(crate) value: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub(crate) struct RuleOutput {
    pub(crate) dimension: String,
    #[serde(default)]
    pub(crate) value: Option<String>,
    #[serde(default)]
    pub(crate) entity: Option<String>,
    #[serde(default)]
    pub(crate) confidence: Option<f64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuleInput {
    pub(crate) name: String,
    pub(crate) priority: i64,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    #[serde(default = "default_user_origin")]
    pub(crate) origin: String,
    pub(crate) conditions: Vec<RuleCondition>,
    pub(crate) outputs: Vec<RuleOutput>,
    #[serde(default)]
    pub(crate) stop_dimensions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuleRecord {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) priority: i64,
    pub(crate) enabled: bool,
    pub(crate) origin: String,
    pub(crate) conditions: Vec<RuleCondition>,
    pub(crate) outputs: Vec<RuleOutput>,
    pub(crate) stop_dimensions: Vec<String>,
    pub(crate) revision: i64,
    pub(crate) created_ts: i64,
    pub(crate) updated_ts: i64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EntitySplitInput {
    pub(crate) display_name: String,
    pub(crate) identity: String,
    #[serde(default)]
    pub(crate) alias_ids: Vec<i64>,
    #[serde(default)]
    pub(crate) context_interval_ids: Vec<i64>,
    #[serde(default)]
    pub(crate) manual_label_ids: Vec<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PolicyInput {
    #[serde(default = "default_schema_version")]
    pub(crate) schema_version: i64,
    pub(crate) name: String,
    #[serde(default = "default_true")]
    pub(crate) enabled: bool,
    pub(crate) scope: Value,
    pub(crate) metric: String,
    pub(crate) window_kind: String,
    pub(crate) budget_seconds: i64,
    #[serde(default)]
    pub(crate) warning_seconds: Option<i64>,
    pub(crate) enforcement: String,
    #[serde(default)]
    pub(crate) schedule: Value,
    #[serde(default)]
    pub(crate) grace: Value,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TableStorageMetric {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) rows: Option<i64>,
    pub(crate) allocated_bytes: Option<i64>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageMetrics {
    pub(crate) objects: Vec<TableStorageMetric>,
    pub(crate) allocation_capability: String,
    pub(crate) page_size: i64,
    pub(crate) page_count: i64,
    pub(crate) freelist_pages: i64,
    pub(crate) reusable_bytes: i64,
    pub(crate) open_context_intervals: i64,
    pub(crate) closed_context_intervals: i64,
    pub(crate) context_transitions_per_day: f64,
    pub(crate) browser_transitions_per_day: f64,
    pub(crate) heartbeat_upserts: Option<i64>,
    pub(crate) persisted_browser_transitions: Option<i64>,
    pub(crate) current_label_version: i64,
    pub(crate) current_label_bytes: Option<i64>,
    pub(crate) previous_label_version: Option<i64>,
    pub(crate) previous_label_bytes: Option<i64>,
    pub(crate) estimated_annualized_bytes: Option<i64>,
    pub(crate) observed_complete_days: i64,
    pub(crate) pruning_lag_ms: i64,
    pub(crate) compaction_lag_ms: i64,
}

pub(crate) struct ActivityRepository<'a> {
    db: &'a mut Connection,
}

impl<'a> ActivityRepository<'a> {
    pub(crate) fn new(db: &'a mut Connection) -> Self {
        Self { db }
    }

    pub(crate) fn summary(
        &self,
        from_ts: i64,
        to_ts: i64,
        group: ActivityGroup,
    ) -> Result<ActivitySummary> {
        validate_range(from_ts, to_ts)?;
        let segments = load_reconciled_segments(self.db, from_ts, to_ts)?;
        let names = entity_names(self.db)?;
        let sessions = session_names(self.db)?;
        let mut groups: BTreeMap<String, SummaryGroup> = BTreeMap::new();
        for segment in &segments {
            let duration = segment.duration_ms();
            let (key, label) = segment_group(segment, group, &names, &sessions);
            let entry = groups.entry(key.clone()).or_insert_with(|| SummaryGroup {
                key,
                label,
                duration_ms: 0,
                episode_count: 0,
                confidence: ConfidenceDistribution::default(),
            });
            entry.duration_ms += duration;
            entry.confidence.add(segment.confidence, duration);
        }
        for value in groups.values_mut() {
            value.episode_count = count_group_episodes(&segments, group, &value.key);
        }
        let meta = analytics_meta(self.db, from_ts, to_ts, &segments)?;
        let mut groups = groups.into_values().collect::<Vec<_>>();
        groups.sort_by(|a, b| {
            b.duration_ms
                .cmp(&a.duration_ms)
                .then_with(|| a.label.cmp(&b.label))
        });
        Ok(ActivitySummary {
            group,
            groups,
            meta,
        })
    }

    pub(crate) fn timeline(
        &self,
        from_ts: i64,
        to_ts: i64,
        resolution_ms: i64,
    ) -> Result<ActivityTimeline> {
        validate_range(from_ts, to_ts)?;
        if resolution_ms <= 0 {
            bail!("timeline resolution must be positive");
        }
        let bucket_count = ((to_ts - from_ts + resolution_ms - 1) / resolution_ms) as usize;
        if bucket_count > MAX_TIMELINE_BUCKETS {
            bail!("timeline exceeds {MAX_TIMELINE_BUCKETS} buckets");
        }
        let segments = load_reconciled_segments(self.db, from_ts, to_ts)?;
        let mut buckets = Vec::with_capacity(bucket_count);
        for index in 0..bucket_count {
            let start = from_ts + index as i64 * resolution_ms;
            let end = (start + resolution_ms).min(to_ts);
            let mut bucket = TimelineBucket {
                start_ts: start,
                end_ts: end,
                observed_ms: 0,
                unknown_ms: 0,
                groups_ms: BTreeMap::new(),
                confidence: ConfidenceDistribution::default(),
            };
            for segment in &segments {
                let duration = overlap_ms(start, end, segment.start_ts, segment.end_ts);
                if duration == 0 {
                    continue;
                }
                bucket.observed_ms += duration;
                let activity = segment
                    .activity
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .unwrap_or("unknown");
                if activity == "unknown" {
                    bucket.unknown_ms += duration;
                }
                *bucket.groups_ms.entry(activity.to_string()).or_default() += duration;
                bucket.confidence.add(segment.confidence, duration);
            }
            buckets.push(bucket);
        }
        let meta = analytics_meta(self.db, from_ts, to_ts, &segments)?;
        Ok(ActivityTimeline {
            resolution_ms,
            buckets,
            meta,
        })
    }

    pub(crate) fn episodes(
        &self,
        from_ts: i64,
        to_ts: i64,
        kind: &str,
        min_seconds: i64,
    ) -> Result<Vec<ActivityEpisode>> {
        validate_range(from_ts, to_ts)?;
        validate_activity(kind)?;
        if min_seconds < 0 || min_seconds > MAX_QUERY_RANGE_MS / 1_000 {
            bail!("invalid minimum episode duration");
        }
        let segments = load_reconciled_segments(self.db, from_ts, to_ts)?;
        Ok(build_episodes(&segments, kind)
            .into_iter()
            .filter(|episode| episode.duration_ms >= min_seconds * 1_000)
            .collect())
    }

    pub(crate) fn entities(&self, kind: Option<&str>) -> Result<Vec<EntityRecord>> {
        if let Some(kind) = kind {
            validate_identifier(kind, 40, "entity kind")?;
        }
        let mut stmt = self.db.prepare(
            "SELECT id,kind,canonical_key,display_name,metadata_json,first_seen_ts,last_seen_ts,user_confirmed,archived \
             FROM context_entities WHERE (?1 IS NULL OR kind=?1) ORDER BY archived,kind,display_name,id",
        )?;
        let rows = stmt.query_map(params![kind], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)? != 0,
                row.get::<_, i64>(8)? != 0,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (
                id,
                kind,
                canonical_key,
                display_name,
                metadata,
                first_seen_ts,
                last_seen_ts,
                confirmed,
                archived,
            ) = row?;
            result.push(EntityRecord {
                id,
                kind,
                canonical_key_hex: hex(&canonical_key),
                display_name,
                metadata: serde_json::from_str(&metadata).unwrap_or_else(|_| json!({})),
                first_seen_ts,
                last_seen_ts,
                user_confirmed: confirmed,
                archived,
                aliases: load_aliases(self.db, id)?,
                relations: load_relations(self.db, id)?,
            });
        }
        Ok(result)
    }

    pub(crate) fn add_manual_label(&mut self, input: &ManualLabelInput, now: i64) -> Result<i64> {
        validate_manual_label(self.db, input)?;
        self.db.execute(
            "INSERT INTO manual_activity_labels \
             (start_ts,end_ts,dimension,value,entity_id,note,created_ts) VALUES (?,?,?,?,?,?,?)",
            params![
                input.start_ts,
                input.end_ts,
                input.dimension,
                input.value,
                input.entity_id,
                bounded_optional(&input.note, 500),
                now
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub(crate) fn delete_manual_label(&mut self, id: i64) -> Result<bool> {
        Ok(self
            .db
            .execute("DELETE FROM manual_activity_labels WHERE id=?", params![id])?
            == 1)
    }

    pub(crate) fn rules(&self) -> Result<Vec<RuleRecord>> {
        let mut stmt = self.db.prepare(
            "SELECT id,name,priority,enabled,origin,conditions_json,outputs_json,stop_dimensions_json,revision,created_ts,updated_ts \
             FROM label_rules ORDER BY enabled DESC,priority DESC,id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })?;
        rows.map(|row| {
            let (
                id,
                name,
                priority,
                enabled,
                origin,
                conditions,
                outputs,
                stops,
                revision,
                created_ts,
                updated_ts,
            ) = row?;
            Ok(RuleRecord {
                id,
                name,
                priority,
                enabled,
                origin,
                conditions: parse_wrapped_list(&conditions, "conditions")?,
                outputs: parse_wrapped_list(&outputs, "outputs")?,
                stop_dimensions: parse_wrapped_list(&stops, "stopDimensions")?,
                revision,
                created_ts,
                updated_ts,
            })
        })
        .collect()
    }

    pub(crate) fn save_rule(&mut self, input: &RuleInput, now: i64) -> Result<RuleRecord> {
        validate_rule(input)?;
        let tx = self.db.transaction()?;
        let revision = latest_rules_revision(&tx)? + 1;
        tx.execute(
            "INSERT INTO label_rules \
             (name,priority,enabled,origin,conditions_json,outputs_json,stop_dimensions_json,revision,created_ts,updated_ts) \
             VALUES (?,?,?,?,?,?,?,?,?,?)",
            params![
                input.name.trim(),
                input.priority,
                bool_i(input.enabled),
                input.origin,
                serde_json::to_string(&input.conditions)?,
                serde_json::to_string(&input.outputs)?,
                serde_json::to_string(&input.stop_dimensions)?,
                revision,
                now,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(RuleRecord {
            id,
            name: input.name.trim().to_string(),
            priority: input.priority,
            enabled: input.enabled,
            origin: input.origin.clone(),
            conditions: input.conditions.clone(),
            outputs: input.outputs.clone(),
            stop_dimensions: input.stop_dimensions.clone(),
            revision,
            created_ts: now,
            updated_ts: now,
        })
    }

    pub(crate) fn add_entity_alias(
        &mut self,
        entity_id: i64,
        kind: &str,
        value: &str,
        source: &str,
        confidence: f64,
        now: i64,
    ) -> Result<i64> {
        ensure_entity(self.db, entity_id)?;
        validate_identifier(kind, 40, "alias kind")?;
        let value = bounded_text(value, 500, "alias value")?;
        let source = bounded_text(source, 80, "alias source")?;
        validate_confidence(confidence)?;
        if let Some((id, existing_source, existing_confidence, confirmed)) = self
            .db
            .query_row(
                "SELECT a.id,a.source,a.confidence,e.user_confirmed \
                 FROM context_entity_aliases a JOIN context_entities e ON e.id=a.entity_id \
                 WHERE a.alias_kind=? AND a.alias_value=?",
                params![kind, value],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .optional()?
        {
            let existing_rank = alias_precedence(&existing_source, confirmed);
            let new_confirmed: bool = self.db.query_row(
                "SELECT user_confirmed<>0 FROM context_entities WHERE id=?",
                params![entity_id],
                |row| row.get(0),
            )?;
            let new_rank = alias_precedence(source, new_confirmed);
            if existing_rank > new_rank
                || (existing_rank == new_rank && existing_confidence > confidence)
            {
                return Ok(id);
            }
        }
        self.db.execute(
            "INSERT INTO context_entity_aliases \
             (entity_id,alias_kind,alias_value,source,confidence,created_ts) VALUES (?,?,?,?,?,?) \
             ON CONFLICT(alias_kind,alias_value) DO UPDATE SET \
               entity_id=excluded.entity_id,source=excluded.source,confidence=excluded.confidence",
            params![entity_id, kind, value, source, confidence, now],
        )?;
        self.db
            .query_row(
                "SELECT id FROM context_entity_aliases WHERE alias_kind=? AND alias_value=?",
                params![kind, value],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    pub(crate) fn add_entity_relation(
        &mut self,
        parent_id: i64,
        child_id: i64,
        relation: &str,
        source: &str,
        confidence: f64,
    ) -> Result<()> {
        if parent_id == child_id {
            bail!("an entity cannot relate to itself");
        }
        ensure_entity(self.db, parent_id)?;
        ensure_entity(self.db, child_id)?;
        validate_identifier(relation, 80, "relation")?;
        bounded_text(source, 80, "relation source")?;
        validate_confidence(confidence)?;
        if relation_reaches(self.db, child_id, parent_id, relation)? {
            bail!("relation would create a cycle");
        }
        self.db.execute(
            "INSERT INTO context_entity_relations \
             (parent_entity_id,child_entity_id,relation,confidence,source) VALUES (?,?,?,?,?) \
             ON CONFLICT(parent_entity_id,child_entity_id,relation) DO UPDATE SET \
               confidence=excluded.confidence,source=excluded.source",
            params![parent_id, child_id, relation, confidence, source],
        )?;
        Ok(())
    }

    pub(crate) fn merge_entities(&mut self, target_id: i64, source_id: i64) -> Result<()> {
        if target_id == source_id {
            return Ok(());
        }
        let tx = self.db.transaction()?;
        merge_entities_tx(&tx, target_id, source_id)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn split_entity(
        &mut self,
        source_id: i64,
        input: &EntitySplitInput,
        now: i64,
    ) -> Result<i64> {
        if input.alias_ids.is_empty()
            && input.context_interval_ids.is_empty()
            && input.manual_label_ids.is_empty()
        {
            bail!("split must move at least one alias or labeled interval");
        }
        bounded_text(&input.display_name, 240, "entity display name")?;
        bounded_text(&input.identity, 500, "entity identity")?;
        let tx = self.db.transaction()?;
        let kind: String = tx
            .query_row(
                "SELECT kind FROM context_entities WHERE id=?",
                params![source_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("source entity not found"))?;
        let key = typed_hash(&kind, &input.identity);
        tx.execute(
            "INSERT INTO context_entities \
             (kind,canonical_key,display_name,first_seen_ts,last_seen_ts,user_confirmed) VALUES (?,?,?,?,?,1)",
            params![kind, key.as_slice(), input.display_name.trim(), now, now],
        )?;
        let target_id = tx.last_insert_rowid();
        move_ids(
            &tx,
            "context_entity_aliases",
            "id",
            "entity_id",
            target_id,
            source_id,
            &input.alias_ids,
        )?;
        move_manual_labels(&tx, source_id, target_id, &input.manual_label_ids)?;
        move_computed_labels(&tx, source_id, target_id, &key, &input.context_interval_ids)?;
        update_activity_entity_for_contexts(
            &tx,
            &kind,
            source_id,
            target_id,
            &input.context_interval_ids,
        )?;
        update_focus_entity_for_contexts(
            &tx,
            &kind,
            source_id,
            target_id,
            &input.context_interval_ids,
        )?;
        enqueue_entity_relabel_tx(&tx, source_id, now)?;
        enqueue_entity_relabel_tx(&tx, target_id, now)?;
        tx.commit()?;
        Ok(target_id)
    }

    pub(crate) fn enqueue_relabel(
        &mut self,
        from_ts: i64,
        to_ts: i64,
        scope: Value,
        now: i64,
    ) -> Result<i64> {
        validate_range(from_ts, to_ts)?;
        validate_scope_size(&scope)?;
        let revision = current_rules_revision(self.db)?;
        let entity_id = scope.get("entityId").and_then(Value::as_i64);
        if let Some(entity_id) = entity_id {
            ensure_entity(self.db, entity_id)?;
        }
        let _ = (revision, now);
        crate::context::enqueue_relabel_job(self.db, from_ts, to_ts, entity_id)
    }

    pub(crate) fn save_policy(&mut self, input: &PolicyInput, now: i64) -> Result<i64> {
        validate_policy(input)?;
        let revision: i64 = self.db.query_row(
            "SELECT COALESCE(MAX(revision),0)+1 FROM attention_policies",
            [],
            |row| row.get(0),
        )?;
        self.db.execute(
            "INSERT INTO attention_policies \
             (name,enabled,scope_json,metric,window_kind,budget_seconds,warning_seconds,enforcement,schedule_json,grace_json,revision,created_ts,updated_ts) \
             VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                input.name.trim(),
                bool_i(input.enabled),
                canonical_json(&input.scope),
                input.metric,
                input.window_kind,
                input.budget_seconds,
                input.warning_seconds,
                input.enforcement,
                canonical_json(&input.schedule),
                canonical_json(&input.grace),
                revision,
                now,
                now
            ],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub(crate) fn snooze_policy(
        &mut self,
        policy_id: i64,
        client_id: Option<i64>,
        service_entity_id: Option<i64>,
        now: i64,
    ) -> Result<i64> {
        let policy = load_policy(self.db, policy_id)?;
        let seconds = policy
            .grace
            .get("snoozeSeconds")
            .and_then(Value::as_i64)
            .unwrap_or(300)
            .clamp(1, 86_400);
        let maximum = policy
            .grace
            .get("maxDailySnoozes")
            .and_then(Value::as_i64)
            .unwrap_or(1)
            .clamp(0, 100);
        let (day_start, day_end) = local_day_bounds(now);
        let used: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM policy_overrides \
             WHERE policy_id=? AND action='snooze' AND start_ts>=? AND start_ts<?",
            params![policy_id, day_start, day_end],
            |row| row.get(0),
        )?;
        if used >= maximum {
            bail!("daily snooze limit reached");
        }
        insert_policy_exception(
            self.db,
            &policy,
            client_id,
            service_entity_id,
            now,
            "snooze",
            seconds,
        )
    }

    pub(crate) fn temporarily_disable_policy(
        &mut self,
        policy_id: i64,
        client_id: Option<i64>,
        duration_seconds: i64,
        now: i64,
    ) -> Result<i64> {
        if !(1..=604_800).contains(&duration_seconds) {
            bail!("temporary disable duration must be between 1 second and 7 days");
        }
        let policy = load_policy(self.db, policy_id)?;
        insert_policy_exception(
            self.db,
            &policy,
            client_id,
            None,
            now,
            "continue",
            duration_seconds,
        )
    }

    pub(crate) fn override_policy(
        &mut self,
        policy_id: i64,
        client_id: Option<i64>,
        service_entity_id: Option<i64>,
        duration_seconds: i64,
        now: i64,
    ) -> Result<i64> {
        if !(1..=86_400).contains(&duration_seconds) {
            bail!("override duration must be between 1 and 86400 seconds");
        }
        let policy = load_policy(self.db, policy_id)?;
        insert_policy_exception(
            self.db,
            &policy,
            client_id,
            service_entity_id,
            now,
            "override",
            duration_seconds,
        )
    }

    pub(crate) fn export_normalized(&self, from_ts: i64, to_ts: i64) -> Result<Value> {
        validate_range(from_ts, to_ts)?;
        let segments = load_reconciled_segments(self.db, from_ts, to_ts)?;
        let entities = self.entities(None)?;
        Ok(json!({
            "schemaVersion": 1,
            "fromTs": from_ts,
            "toTs": to_ts,
            "activityIntervals": segments,
            "entities": entities,
            "classifierVersion": classifier_version(self.db)?,
            "rulesRevision": current_rules_revision(self.db)?,
        }))
    }

    pub(crate) fn archive_entity(&mut self, entity_id: i64) -> Result<bool> {
        let tx = self.db.transaction()?;
        let changed = tx.execute(
            "UPDATE context_entities SET archived=1 WHERE id=?",
            params![entity_id],
        )? == 1;
        if changed {
            enqueue_entity_relabel_tx(&tx, entity_id, crate::util::now_ms())?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn storage_metrics(&self, now: i64) -> Result<StorageMetrics> {
        storage_metrics(self.db, now)
    }
}

#[derive(Clone)]
struct RawActivity {
    id: i64,
    start_ts: i64,
    end_ts: i64,
    source: String,
    source_context_interval_id: Option<i64>,
    activity: Option<String>,
    project_entity_id: Option<i64>,
    service_entity_id: Option<i64>,
    tool_entity_id: Option<i64>,
    agent_session_id: Option<i64>,
    confidence: f64,
    labeler_version: i64,
    episode_key: Option<Vec<u8>>,
    evidence: Value,
}

fn load_raw_activity(db: &Connection, from_ts: i64, to_ts: i64) -> Result<Vec<RawActivity>> {
    let mut stmt = db.prepare(
        "SELECT id,start_ts,end_ts,source,source_context_interval_id,activity,project_entity_id,service_entity_id,tool_entity_id,agent_session_id,confidence,labeler_version,episode_key,evidence_json \
         FROM active_activity_intervals WHERE start_ts < ? AND end_ts > ? AND end_ts > start_ts ORDER BY start_ts,end_ts,id",
    )?;
    let rows = stmt
        .query_map(params![to_ts, from_ts], |row| {
            let evidence: String = row.get(13)?;
            Ok(RawActivity {
                id: row.get(0)?,
                start_ts: row.get(1)?,
                end_ts: row.get(2)?,
                source: row.get(3)?,
                source_context_interval_id: row.get(4)?,
                activity: row.get(5)?,
                project_entity_id: row.get(6)?,
                service_entity_id: row.get(7)?,
                tool_entity_id: row.get(8)?,
                agent_session_id: row.get(9)?,
                confidence: row.get::<_, f64>(10)?.clamp(0.0, 1.0),
                labeler_version: row.get(11)?,
                episode_key: row.get(12)?,
                evidence: serde_json::from_str(&evidence).unwrap_or_else(|_| json!({})),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn load_reconciled_segments(
    db: &Connection,
    from_ts: i64,
    to_ts: i64,
) -> Result<Vec<ActivitySegment>> {
    let raw = load_raw_activity(db, from_ts, to_ts)?;
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let mut boundaries = vec![from_ts, to_ts];
    for row in &raw {
        boundaries.push(row.start_ts.max(from_ts));
        boundaries.push(row.end_ts.min(to_ts));
    }
    boundaries.sort_unstable();
    boundaries.dedup();
    let mut output: Vec<ActivitySegment> = Vec::new();
    for pair in boundaries.windows(2) {
        let (start, end) = (pair[0], pair[1]);
        if end <= start {
            continue;
        }
        let winner = raw
            .iter()
            .filter(|row| row.start_ts < end && row.end_ts > start)
            .max_by(|a, b| {
                source_rank(&a.source)
                    .cmp(&source_rank(&b.source))
                    .then_with(|| a.confidence.total_cmp(&b.confidence))
                    .then_with(|| a.labeler_version.cmp(&b.labeler_version))
                    .then_with(|| a.id.cmp(&b.id))
            });
        let Some(row) = winner else { continue };
        let mut segment = ActivitySegment {
            start_ts: start,
            end_ts: end,
            activity: row.activity.clone(),
            project_entity_id: row.project_entity_id,
            service_entity_id: row.service_entity_id,
            tool_entity_id: row.tool_entity_id,
            agent_session_id: row.agent_session_id,
            confidence: row.confidence,
            labeler_version: row.labeler_version,
            episode_key: row
                .episode_key
                .as_ref()
                .map(|key| hex(key))
                .unwrap_or_else(|| {
                    let identity = row.source_context_interval_id.unwrap_or(row.id);
                    let key = hash_parts(&[
                        b"activity-episode-fallback-v1",
                        row.source.as_bytes(),
                        &identity.to_be_bytes(),
                        &row.start_ts.to_be_bytes(),
                    ]);
                    hex(&key[..16])
                }),
            provenance: SegmentProvenance {
                source: row.source.clone(),
                activity_interval_ids: vec![row.id],
                source_context_interval_ids: row.source_context_interval_id.into_iter().collect(),
                evidence: evidence_base(&row.evidence),
            },
        };
        if let Some(previous) = output.last_mut() {
            if previous.end_ts == segment.start_ts && previous.same_effective_state(&segment) {
                previous.end_ts = segment.end_ts;
                append_unique(
                    &mut previous.provenance.activity_interval_ids,
                    &segment.provenance.activity_interval_ids,
                );
                append_unique(
                    &mut previous.provenance.source_context_interval_ids,
                    &segment.provenance.source_context_interval_ids,
                );
                continue;
            }
        }
        segment.provenance.activity_interval_ids.sort_unstable();
        output.push(segment);
    }
    Ok(output)
}

fn source_rank(source: &str) -> i32 {
    match source {
        "browser_os_reconciled" | "reconciled" => 50,
        "browser_verified" => 45,
        "browser" => 40,
        "agent_presence" => 35,
        "os_focus" => 30,
        "historical_title" => 10,
        _ => 20,
    }
}

fn analytics_meta(
    db: &Connection,
    from_ts: i64,
    to_ts: i64,
    segments: &[ActivitySegment],
) -> Result<AnalyticsMeta> {
    let observed_ms = segments.iter().map(ActivitySegment::duration_ms).sum();
    let unknown_ms = segments
        .iter()
        .filter(|segment| {
            segment
                .activity
                .as_deref()
                .is_none_or(|value| value == "unknown")
        })
        .map(ActivitySegment::duration_ms)
        .sum();
    let mut confidence = ConfidenceDistribution::default();
    for segment in segments {
        confidence.add(segment.confidence, segment.duration_ms());
    }
    let episode_count = count_all_episodes(segments);
    Ok(AnalyticsMeta {
        from_ts,
        to_ts,
        metric: "attention_interval_seconds".to_string(),
        observed_ms,
        unknown_ms,
        monitoring_coverage: observed_ms as f64 / (to_ts - from_ts) as f64,
        confidence,
        sample_count: segments.len(),
        episode_count,
        classifier_version: classifier_version(db)?,
        rules_revision: current_rules_revision(db)?,
    })
}

fn segment_group(
    segment: &ActivitySegment,
    group: ActivityGroup,
    names: &HashMap<i64, String>,
    sessions: &HashMap<i64, String>,
) -> (String, String) {
    match group {
        ActivityGroup::Activity => {
            let value = segment
                .activity
                .clone()
                .unwrap_or_else(|| "unknown".to_string());
            (value.clone(), value)
        }
        ActivityGroup::Project => entity_group(segment.project_entity_id, "unknown-project", names),
        ActivityGroup::Service => entity_group(segment.service_entity_id, "unknown-service", names),
        ActivityGroup::Tool => entity_group(segment.tool_entity_id, "unknown-tool", names),
        ActivityGroup::AgentSession => match segment.agent_session_id {
            Some(id) => (
                id.to_string(),
                sessions
                    .get(&id)
                    .cloned()
                    .unwrap_or_else(|| format!("Session {id}")),
            ),
            None => ("unknown-session".to_string(), "Unknown session".to_string()),
        },
    }
}

fn entity_group(id: Option<i64>, unknown: &str, names: &HashMap<i64, String>) -> (String, String) {
    match id {
        Some(id) => (
            id.to_string(),
            names
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("Entity {id}")),
        ),
        None => (unknown.to_string(), unknown.replace('-', " ")),
    }
}

fn count_group_episodes(segments: &[ActivitySegment], group: ActivityGroup, key: &str) -> usize {
    let names = HashMap::new();
    let sessions = HashMap::new();
    let mut count = 0;
    let mut active = false;
    for segment in segments {
        let segment_key = segment_group(segment, group, &names, &sessions).0;
        if segment_key == key {
            if !active {
                count += 1;
                active = true;
            }
        } else {
            active = false;
        }
    }
    count
}

fn count_all_episodes(segments: &[ActivitySegment]) -> usize {
    let mut count = 0;
    let mut last: Option<&str> = None;
    for segment in segments {
        let current = segment.activity.as_deref().unwrap_or("unknown");
        if last != Some(current) {
            count += 1;
            last = Some(current);
        }
    }
    count
}

fn build_episodes(segments: &[ActivitySegment], kind: &str) -> Vec<ActivityEpisode> {
    let mut result: Vec<ActivityEpisode> = Vec::new();
    for segment in segments
        .iter()
        .filter(|segment| segment.activity.as_deref() == Some(kind))
    {
        if let Some(previous) = result.last_mut() {
            if previous.end_ts == segment.start_ts && previous.episode_key == segment.episode_key {
                previous.end_ts = segment.end_ts;
                previous.duration_ms += segment.duration_ms();
                previous.segment_count += 1;
                previous.confidence = previous.confidence.min(segment.confidence);
                previous.crosses_local_midnight = local_date_key(previous.start_ts)
                    != local_date_key(previous.end_ts.saturating_sub(1));
                if previous.project_entity_id != segment.project_entity_id {
                    previous.project_entity_id = None;
                }
                if previous.service_entity_id != segment.service_entity_id {
                    previous.service_entity_id = None;
                }
                continue;
            }
        }
        result.push(ActivityEpisode {
            episode_key: segment.episode_key.clone(),
            start_ts: segment.start_ts,
            end_ts: segment.end_ts,
            duration_ms: segment.duration_ms(),
            activity: kind.to_string(),
            project_entity_id: segment.project_entity_id,
            service_entity_id: segment.service_entity_id,
            confidence: segment.confidence,
            crosses_local_midnight: local_date_key(segment.start_ts)
                != local_date_key(segment.end_ts.saturating_sub(1)),
            segment_count: 1,
        });
    }
    result
}

pub(crate) fn validate_rule(rule: &RuleInput) -> Result<()> {
    bounded_text(&rule.name, 160, "rule name")?;
    if !matches!(
        rule.origin.as_str(),
        "user" | "built_in" | "heuristic" | "integration"
    ) {
        bail!("unsupported rule origin");
    }
    if rule.conditions.is_empty() || rule.conditions.len() > MAX_RULE_CONDITIONS {
        bail!("rules require 1..={MAX_RULE_CONDITIONS} conditions");
    }
    if rule.outputs.is_empty() || rule.outputs.len() > MAX_RULE_OUTPUTS {
        bail!("rules require 1..={MAX_RULE_OUTPUTS} outputs");
    }
    if rule.stop_dimensions.len() > MAX_RULE_STOPS {
        bail!("too many stopped dimensions");
    }
    for condition in &rule.conditions {
        validate_condition(condition)?;
    }
    for output in &rule.outputs {
        validate_output(output)?;
    }
    for dimension in &rule.stop_dimensions {
        validate_dimension(dimension)?;
    }
    Ok(())
}

fn validate_condition(condition: &RuleCondition) -> Result<()> {
    if !LABEL_FACTS.contains(&condition.fact.as_str()) {
        bail!("unsupported labeling fact {}", condition.fact);
    }
    match condition.operator.as_str() {
        "exists" | "notExists" | "not_exists" => {
            if condition
                .value
                .as_ref()
                .is_some_and(|value| !value.is_null())
            {
                bail!("{} does not accept a value", condition.operator);
            }
        }
        "exact"
        | "caseInsensitiveExact"
        | "ciExact"
        | "contains"
        | "prefix"
        | "suffix"
        | "glob" => {
            let value = condition
                .value
                .as_ref()
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("{} requires a string value", condition.operator))?;
            bounded_text(value, 500, "condition value")?;
        }
        "domainSuffix" => {
            if !matches!(condition.fact.as_str(), "browser.domain" | "browser.origin") {
                bail!("domainSuffix is only valid for browser domains/origins");
            }
            let domain = condition
                .value
                .as_ref()
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("domainSuffix requires a string"))?;
            validate_domain(domain)?;
        }
        "booleanEqual" | "booleanEquality" => {
            if !BOOLEAN_FACTS.contains(&condition.fact.as_str()) {
                bail!("boolean equality requires a boolean fact");
            }
            if !condition.value.as_ref().is_some_and(Value::is_boolean) {
                bail!("boolean equality requires a boolean value");
            }
        }
        "boundedRegex" | "regex" => {
            let pattern = condition
                .value
                .as_ref()
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("boundedRegex requires a string"))?;
            SafeRegex::compile(pattern)?;
        }
        "entityRelationship" => {
            if !ENTITY_FACTS.contains(&condition.fact.as_str()) {
                bail!("entityRelationship requires an entity fact");
            }
            let object = condition
                .value
                .as_ref()
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("entityRelationship requires an object"))?;
            let relation = object
                .get("relation")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("entityRelationship.relation is required"))?;
            validate_identifier(relation, 80, "entity relation")?;
            if object
                .get("entityId")
                .and_then(Value::as_i64)
                .is_none_or(|id| id <= 0)
            {
                bail!("entityRelationship.entityId must be positive");
            }
        }
        other => bail!("unsupported condition operator {other}"),
    }
    Ok(())
}

fn validate_output(output: &RuleOutput) -> Result<()> {
    validate_dimension(&output.dimension)?;
    if output.value.is_some() == output.entity.is_some() {
        bail!("rule output must set exactly one of value or entity");
    }
    if let Some(value) = &output.value {
        bounded_text(value, 240, "rule output value")?;
        if output.dimension == "activity.primary" {
            validate_activity(value)?;
        }
    }
    if let Some(entity) = &output.entity {
        let (kind, identity) = entity
            .split_once(':')
            .ok_or_else(|| anyhow!("entity output must use kind:identity"))?;
        validate_identifier(kind, 40, "entity kind")?;
        bounded_text(identity, 500, "entity identity")?;
        if !matches!(
            output.dimension.as_str(),
            "project" | "repository" | "service" | "tool" | "account" | "agent.session"
        ) {
            bail!("dimension {} does not accept an entity", output.dimension);
        }
    }
    validate_confidence(output.confidence.unwrap_or(1.0))?;
    Ok(())
}

pub(crate) fn condition_matches_value(
    condition: &RuleCondition,
    actual: Option<&Value>,
) -> Result<bool> {
    condition_matches_fact(None, condition, actual)
}

pub(crate) fn condition_matches_fact(
    db: Option<&Connection>,
    condition: &RuleCondition,
    actual: Option<&Value>,
) -> Result<bool> {
    validate_condition(condition)?;
    Ok(match condition.operator.as_str() {
        "exists" => actual.is_some_and(|value| !value.is_null()),
        "notExists" | "not_exists" => actual.is_none_or(Value::is_null),
        "booleanEqual" | "booleanEquality" => {
            actual.and_then(Value::as_bool) == condition.value.as_ref().and_then(Value::as_bool)
        }
        "entityRelationship" => {
            let Some(db) = db else { return Ok(false) };
            let Some(actual_id) = actual.and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.get("entityId").and_then(Value::as_i64))
            }) else {
                return Ok(false);
            };
            let expected = condition
                .value
                .as_ref()
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("entityRelationship requires an object"))?;
            let relation = expected
                .get("relation")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("entityRelationship.relation is required"))?;
            let target = expected
                .get("entityId")
                .and_then(Value::as_i64)
                .ok_or_else(|| anyhow!("entityRelationship.entityId is required"))?;
            actual_id == target || relation_reaches(db, target, actual_id, relation)?
        }
        operator => {
            let Some(actual) = actual.and_then(Value::as_str) else {
                return Ok(false);
            };
            let expected = condition
                .value
                .as_ref()
                .and_then(Value::as_str)
                .unwrap_or_default();
            match operator {
                "exact" => actual == expected,
                "caseInsensitiveExact" | "ciExact" => actual.eq_ignore_ascii_case(expected),
                "contains" => actual.contains(expected),
                "prefix" => actual.starts_with(expected),
                "suffix" => actual.ends_with(expected),
                "domainSuffix" => domain_suffix_matches(actual, expected),
                "glob" => bounded_glob(expected, actual),
                "boundedRegex" | "regex" => SafeRegex::compile(expected)?.is_match(actual),
                _ => false,
            }
        }
    })
}

pub(crate) fn domain_suffix_matches(actual: &str, suffix: &str) -> bool {
    let actual = normalized_domain(actual);
    let suffix = normalized_domain(suffix);
    if actual.is_empty() || suffix.is_empty() {
        return false;
    }
    actual == suffix
        || actual
            .strip_suffix(&suffix)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn normalized_domain(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('.').to_ascii_lowercase();
    let without_scheme = trimmed
        .split_once("://")
        .map_or(trimmed.as_str(), |(_, rest)| rest);
    without_scheme
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_string()
}

fn bounded_glob(pattern: &str, value: &str) -> bool {
    if pattern.chars().count() > MAX_REGEX_PATTERN_CHARS
        || value.chars().count() > MAX_REGEX_INPUT_CHARS
    {
        return false;
    }
    let p = pattern.chars().collect::<Vec<_>>();
    let v = value.chars().collect::<Vec<_>>();
    let (mut pi, mut vi, mut star, mut retry) = (0, 0, None, 0);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            retry = vi;
        } else if let Some(star_pos) = star {
            pi = star_pos + 1;
            retry += 1;
            vi = retry;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[derive(Clone, Debug)]
struct SafeRegex {
    start_anchor: bool,
    end_anchor: bool,
    tokens: Vec<RegexToken>,
}

#[derive(Clone, Debug)]
struct RegexToken {
    atom: RegexAtom,
    min: usize,
    max: usize,
}

#[derive(Clone, Debug)]
enum RegexAtom {
    Literal(char),
    Any,
    Digit,
    Word,
    Space,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
}

impl RegexAtom {
    fn matches(&self, value: char) -> bool {
        match self {
            Self::Literal(expected) => *expected == value,
            Self::Any => true,
            Self::Digit => value.is_ascii_digit(),
            Self::Word => value.is_ascii_alphanumeric() || value == '_',
            Self::Space => value.is_ascii_whitespace(),
            Self::Class { negated, ranges } => {
                let found = ranges
                    .iter()
                    .any(|(start, end)| *start <= value && value <= *end);
                found != *negated
            }
        }
    }
}

impl SafeRegex {
    fn compile(pattern: &str) -> Result<Self> {
        let chars = pattern.chars().collect::<Vec<_>>();
        if chars.len() > MAX_REGEX_PATTERN_CHARS {
            bail!("regular expression exceeds {MAX_REGEX_PATTERN_CHARS} characters");
        }
        let mut index = 0usize;
        let start_anchor = chars.first() == Some(&'^');
        if start_anchor {
            index += 1;
        }
        let end_anchor = chars.last() == Some(&'$') && !is_escaped(&chars, chars.len() - 1);
        let stop = chars.len().saturating_sub(usize::from(end_anchor));
        let mut tokens = Vec::new();
        while index < stop {
            if tokens.len() >= MAX_REGEX_TOKENS {
                bail!("regular expression has too many tokens");
            }
            let atom = match chars[index] {
                '.' => {
                    index += 1;
                    RegexAtom::Any
                }
                '\\' => {
                    index += 1;
                    let escaped = *chars
                        .get(index)
                        .ok_or_else(|| anyhow!("trailing regular expression escape"))?;
                    index += 1;
                    match escaped {
                        'd' => RegexAtom::Digit,
                        'w' => RegexAtom::Word,
                        's' => RegexAtom::Space,
                        other => RegexAtom::Literal(other),
                    }
                }
                '[' => parse_regex_class(&chars, &mut index, stop)?,
                '(' | ')' | '|' => {
                    bail!("groups and alternation are not supported by boundedRegex")
                }
                '*' | '+' | '?' | '{' | '}' => bail!("quantifier has no preceding token"),
                '^' | '$' => bail!("anchors are only allowed at expression boundaries"),
                literal => {
                    index += 1;
                    RegexAtom::Literal(literal)
                }
            };
            let (min, max) = parse_quantifier(&chars, &mut index, stop)?;
            tokens.push(RegexToken { atom, min, max });
        }
        Ok(Self {
            start_anchor,
            end_anchor,
            tokens,
        })
    }

    fn is_match(&self, value: &str) -> bool {
        let value = value
            .chars()
            .take(MAX_REGEX_INPUT_CHARS + 1)
            .collect::<Vec<_>>();
        if value.len() > MAX_REGEX_INPUT_CHARS {
            return false;
        }
        let starts: Box<dyn Iterator<Item = usize>> = if self.start_anchor {
            Box::new(std::iter::once(0))
        } else {
            Box::new(0..=value.len())
        };
        let mut work = 0usize;
        for start in starts {
            let mut positions = BTreeSet::from([start]);
            for token in &self.tokens {
                let mut accepted = BTreeSet::new();
                for position in &positions {
                    let mut cursor = *position;
                    if token.min == 0 {
                        accepted.insert(cursor);
                    }
                    for repeat in 1..=token.max.min(value.len().saturating_sub(*position)) {
                        work += 1;
                        if work > MAX_REGEX_WORK || !token.atom.matches(value[cursor]) {
                            break;
                        }
                        cursor += 1;
                        if repeat >= token.min {
                            accepted.insert(cursor);
                        }
                    }
                }
                positions = accepted;
                if positions.is_empty() {
                    break;
                }
            }
            if positions
                .iter()
                .any(|position| !self.end_anchor || *position == value.len())
            {
                return true;
            }
            if work > MAX_REGEX_WORK {
                return false;
            }
        }
        false
    }
}

fn is_escaped(chars: &[char], index: usize) -> bool {
    let mut slashes = 0usize;
    let mut cursor = index;
    while cursor > 0 && chars[cursor - 1] == '\\' {
        slashes += 1;
        cursor -= 1;
    }
    slashes % 2 == 1
}

fn parse_regex_class(chars: &[char], index: &mut usize, stop: usize) -> Result<RegexAtom> {
    *index += 1;
    let negated = chars.get(*index) == Some(&'^');
    if negated {
        *index += 1;
    }
    let mut values = Vec::new();
    while *index < stop && chars[*index] != ']' {
        let first = parse_class_char(chars, index, stop)?;
        if *index + 1 < stop && chars[*index] == '-' && chars[*index + 1] != ']' {
            *index += 1;
            let last = parse_class_char(chars, index, stop)?;
            if first > last {
                bail!("descending character class range");
            }
            values.push((first, last));
        } else {
            values.push((first, first));
        }
    }
    if chars.get(*index) != Some(&']') || values.is_empty() {
        bail!("unterminated or empty character class");
    }
    *index += 1;
    Ok(RegexAtom::Class {
        negated,
        ranges: values,
    })
}

fn parse_class_char(chars: &[char], index: &mut usize, stop: usize) -> Result<char> {
    let mut value = *chars
        .get(*index)
        .ok_or_else(|| anyhow!("invalid character class"))?;
    *index += 1;
    if value == '\\' {
        value = *chars
            .get(*index)
            .ok_or_else(|| anyhow!("trailing class escape"))?;
        *index += 1;
    }
    if *index > stop {
        bail!("invalid character class");
    }
    Ok(value)
}

fn parse_quantifier(chars: &[char], index: &mut usize, stop: usize) -> Result<(usize, usize)> {
    if *index >= stop {
        return Ok((1, 1));
    }
    match chars[*index] {
        '?' => {
            *index += 1;
            Ok((0, 1))
        }
        '*' => {
            *index += 1;
            Ok((0, MAX_REGEX_REPEAT))
        }
        '+' => {
            *index += 1;
            Ok((1, MAX_REGEX_REPEAT))
        }
        '{' => {
            *index += 1;
            let min = parse_usize(chars, index, stop)?;
            let max = if chars.get(*index) == Some(&',') {
                *index += 1;
                if chars.get(*index) == Some(&'}') {
                    bail!("open-ended brace quantifiers are not allowed");
                }
                parse_usize(chars, index, stop)?
            } else {
                min
            };
            if chars.get(*index) != Some(&'}') {
                bail!("unterminated quantifier");
            }
            *index += 1;
            if min > max || max > MAX_REGEX_REPEAT {
                bail!("invalid or excessive quantifier bounds");
            }
            Ok((min, max))
        }
        _ => Ok((1, 1)),
    }
}

fn parse_usize(chars: &[char], index: &mut usize, stop: usize) -> Result<usize> {
    let start = *index;
    let mut value = 0usize;
    while *index < stop && chars[*index].is_ascii_digit() {
        value = value
            .checked_mul(10)
            .and_then(|old| old.checked_add(chars[*index].to_digit(10).unwrap_or(0) as usize))
            .ok_or_else(|| anyhow!("quantifier overflow"))?;
        *index += 1;
    }
    if *index == start {
        bail!("quantifier bound is required");
    }
    Ok(value)
}

fn merge_entities_tx(tx: &Transaction<'_>, target_id: i64, source_id: i64) -> Result<()> {
    let (target_kind, target_key): (String, Vec<u8>) = tx
        .query_row(
            "SELECT kind,canonical_key FROM context_entities WHERE id=?",
            params![target_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| anyhow!("target entity not found"))?;
    let source_kind: String = tx
        .query_row(
            "SELECT kind FROM context_entities WHERE id=?",
            params![source_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("source entity not found"))?;
    if target_kind != source_kind {
        bail!("only entities of the same kind can be merged");
    }
    tx.execute(
        "UPDATE context_entity_aliases SET entity_id=? WHERE entity_id=?",
        params![target_id, source_id],
    )?;
    let relations = {
        let mut stmt = tx.prepare(
            "SELECT parent_entity_id,child_entity_id,relation,confidence,source \
             FROM context_entity_relations WHERE parent_entity_id=? OR child_entity_id=?",
        )?;
        let rows = stmt
            .query_map(params![source_id, source_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    tx.execute(
        "DELETE FROM context_entity_relations WHERE parent_entity_id=? OR child_entity_id=?",
        params![source_id, source_id],
    )?;
    for (parent, child, relation, confidence, relation_source) in relations {
        let parent = if parent == source_id {
            target_id
        } else {
            parent
        };
        let child = if child == source_id { target_id } else { child };
        if parent == child {
            continue;
        }
        tx.execute(
            "INSERT INTO context_entity_relations \
             (parent_entity_id,child_entity_id,relation,confidence,source) VALUES (?,?,?,?,?) \
             ON CONFLICT(parent_entity_id,child_entity_id,relation) DO UPDATE SET \
               confidence=MAX(confidence,excluded.confidence)",
            params![parent, child, relation, confidence, relation_source],
        )?;
    }
    rewrite_computed_entity(tx, source_id, target_id, &target_key)?;
    tx.execute(
        "UPDATE manual_activity_labels SET entity_id=? WHERE entity_id=?",
        params![target_id, source_id],
    )?;
    for (table, column) in [
        ("activity_intervals", entity_activity_column(&target_kind)),
        ("focus_context_intervals", entity_focus_column(&target_kind)),
        (
            "browser_attention_intervals",
            (target_kind == "service").then_some("service_entity_id"),
        ),
        (
            "browser_current_states",
            (target_kind == "service").then_some("service_entity_id"),
        ),
        (
            "browser_events",
            (target_kind == "service").then_some("service_entity_id"),
        ),
        (
            "intervention_events",
            (target_kind == "service").then_some("service_entity_id"),
        ),
        ("relabel_jobs", Some("entity_id")),
    ] {
        if let Some(column) = column {
            if table_exists(tx, table)? && column_exists(tx, table, column)? {
                tx.execute(
                    &format!("UPDATE {table} SET {column}=? WHERE {column}=?"),
                    params![target_id, source_id],
                )?;
            }
        }
    }
    if table_exists(tx, "attention_policies")? {
        let policies = {
            let mut stmt = tx.prepare("SELECT id,scope_json FROM attention_policies")?;
            let values = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            values
        };
        for (id, encoded) in policies {
            let mut scope: Value = serde_json::from_str(&encoded).unwrap_or_else(|_| json!({}));
            if rewrite_json_entity_id(&mut scope, source_id, target_id) {
                tx.execute(
                    "UPDATE attention_policies SET scope_json=? WHERE id=?",
                    params![canonical_json(&scope), id],
                )?;
            }
        }
    }
    tx.execute(
        "UPDATE context_entities SET first_seen_ts=MIN(first_seen_ts,(SELECT first_seen_ts FROM context_entities WHERE id=?)), \
         last_seen_ts=MAX(last_seen_ts,(SELECT last_seen_ts FROM context_entities WHERE id=?)),user_confirmed=1 WHERE id=?",
        params![source_id, source_id, target_id],
    )?;
    enqueue_entity_relabel_tx(tx, target_id, crate::util::now_ms())?;
    tx.execute(
        "DELETE FROM context_entities WHERE id=?",
        params![source_id],
    )?;
    Ok(())
}

fn rewrite_json_entity_id(value: &mut Value, source_id: i64, target_id: i64) -> bool {
    match value {
        Value::Object(values) => values.iter_mut().fold(false, |changed, (key, value)| {
            let replaced = if (key == "entityId" || key.ends_with("EntityId"))
                && value.as_i64() == Some(source_id)
            {
                *value = json!(target_id);
                true
            } else {
                rewrite_json_entity_id(value, source_id, target_id)
            };
            changed || replaced
        }),
        Value::Array(values) => values.iter_mut().fold(false, |changed, value| {
            rewrite_json_entity_id(value, source_id, target_id) || changed
        }),
        _ => false,
    }
}

fn json_references_entity(value: &Value, entity_id: i64) -> bool {
    match value {
        Value::Object(values) => values.iter().any(|(key, value)| {
            ((key == "entityId" || key.ends_with("EntityId")) && value.as_i64() == Some(entity_id))
                || json_references_entity(value, entity_id)
        }),
        Value::Array(values) => values
            .iter()
            .any(|value| json_references_entity(value, entity_id)),
        _ => false,
    }
}

pub(crate) fn entity_reference_count(db: &Connection, entity_id: i64) -> Result<i64> {
    let mut count = 0_i64;
    for (table, column) in [
        ("focus_interval_labels", "entity_id"),
        ("manual_activity_labels", "entity_id"),
        ("focus_context_intervals", "repository_entity_id"),
        ("focus_context_intervals", "project_entity_id"),
        ("activity_intervals", "project_entity_id"),
        ("activity_intervals", "service_entity_id"),
        ("activity_intervals", "tool_entity_id"),
        ("browser_attention_intervals", "service_entity_id"),
        ("browser_current_states", "service_entity_id"),
        ("browser_events", "service_entity_id"),
        ("intervention_events", "service_entity_id"),
        ("relabel_jobs", "entity_id"),
    ] {
        if table_exists(db, table)? && column_exists(db, table, column)? {
            count += db.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column}=?"),
                params![entity_id],
                |row| row.get::<_, i64>(0),
            )?;
        }
    }
    if table_exists(db, "context_entity_relations")? {
        count += db.query_row(
            "SELECT COUNT(*) FROM context_entity_relations WHERE parent_entity_id=? OR child_entity_id=?",
            params![entity_id,entity_id],
            |row| row.get::<_, i64>(0),
        )?;
    }
    if table_exists(db, "attention_policies")? {
        let mut statement = db.prepare("SELECT scope_json FROM attention_policies")?;
        for encoded in statement.query_map([], |row| row.get::<_, String>(0))? {
            let value = serde_json::from_str::<Value>(&encoded?)
                .context("parse attention_policies.scope_json")?;
            count += i64::from(json_references_entity(&value, entity_id));
        }
    }
    Ok(count)
}

fn enqueue_entity_relabel_tx(tx: &Transaction<'_>, entity_id: i64, now: i64) -> Result<()> {
    if !table_exists(tx, "relabel_jobs")? || !table_exists(tx, "context_runtime_state")? {
        return Ok(());
    }
    let range: (Option<i64>, Option<i64>) = tx.query_row(
        "SELECT MIN(start_ts),MAX(end_ts) FROM focus_context_intervals f WHERE f.repository_entity_id=? OR f.project_entity_id=? OR EXISTS(SELECT 1 FROM focus_interval_labels l WHERE l.context_interval_id=f.id AND l.entity_id=?)",
        params![entity_id,entity_id,entity_id],
        |row| Ok((row.get(0)?,row.get(1)?)),
    )?;
    let (Some(from_ts), Some(to_ts)) = range else {
        return Ok(());
    };
    if to_ts <= from_ts {
        return Ok(());
    }
    let (revision, base_version): (i64, i64) = tx.query_row(
        "SELECT rules_revision,active_labeler_version FROM context_runtime_state WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let target_version: i64 = tx.query_row(
        "SELECT COALESCE(MAX(version),1)+1 FROM (SELECT active_labeler_version AS version FROM context_runtime_state UNION ALL SELECT labeler_version FROM relabel_jobs UNION ALL SELECT labeler_version FROM labeler_runs)",
        [],
        |row| row.get(0),
    )?;
    tx.execute(
        "INSERT INTO relabel_jobs(rules_revision,labeler_version,base_labeler_version,range_start_ts,range_end_ts,entity_id,cursor_start_ts,status,created_ts) VALUES(?,?,?,?,?,?,?,'queued',?)",
        params![revision,target_version,base_version,from_ts,to_ts,entity_id,from_ts.saturating_sub(1),now],
    )?;
    Ok(())
}

fn rewrite_computed_entity(
    tx: &Transaction<'_>,
    source_id: i64,
    target_id: i64,
    key: &[u8],
) -> Result<()> {
    let rows = {
        let mut stmt = tx.prepare(
            "SELECT id,dimension FROM focus_interval_labels WHERE entity_id=? ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![source_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (id, dimension) in rows {
        let label_key = hash_parts(&[b"entity-label", dimension.as_bytes(), key]);
        let changed = tx.execute(
            "UPDATE OR IGNORE focus_interval_labels SET entity_id=?,label_key=? WHERE id=?",
            params![target_id, label_key.as_slice(), id],
        )?;
        if changed == 0 {
            tx.execute("DELETE FROM focus_interval_labels WHERE id=?", params![id])?;
        }
    }
    Ok(())
}

fn move_computed_labels(
    tx: &Transaction<'_>,
    source_id: i64,
    target_id: i64,
    target_key: &[u8],
    context_ids: &[i64],
) -> Result<()> {
    if context_ids.is_empty() {
        return Ok(());
    }
    let placeholders = sql_placeholders(context_ids.len());
    let mut values = vec![rusqlite::types::Value::Integer(source_id)];
    values.extend(
        context_ids
            .iter()
            .copied()
            .map(rusqlite::types::Value::Integer),
    );
    let rows = {
        let mut stmt = tx.prepare(&format!(
            "SELECT id,dimension FROM focus_interval_labels WHERE entity_id=? AND context_interval_id IN ({placeholders})"
        ))?;
        let rows = stmt
            .query_map(params_from_iter(values), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (id, dimension) in rows {
        let label_key = hash_parts(&[b"entity-label", dimension.as_bytes(), target_key]);
        let changed = tx.execute(
            "UPDATE OR IGNORE focus_interval_labels SET entity_id=?,label_key=? WHERE id=?",
            params![target_id, label_key.as_slice(), id],
        )?;
        if changed == 0 {
            tx.execute("DELETE FROM focus_interval_labels WHERE id=?", params![id])?;
        }
    }
    Ok(())
}

fn move_manual_labels(
    tx: &Transaction<'_>,
    source_id: i64,
    target_id: i64,
    ids: &[i64],
) -> Result<()> {
    move_ids(
        tx,
        "manual_activity_labels",
        "id",
        "entity_id",
        target_id,
        source_id,
        ids,
    )
}

fn move_ids(
    tx: &Transaction<'_>,
    table: &str,
    id_column: &str,
    value_column: &str,
    target_id: i64,
    source_id: i64,
    ids: &[i64],
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = sql_placeholders(ids.len());
    let mut values = vec![
        rusqlite::types::Value::Integer(target_id),
        rusqlite::types::Value::Integer(source_id),
    ];
    values.extend(ids.iter().copied().map(rusqlite::types::Value::Integer));
    tx.execute(
        &format!(
            "UPDATE {table} SET {value_column}=? WHERE {value_column}=? AND {id_column} IN ({placeholders})"
        ),
        params_from_iter(values),
    )?;
    Ok(())
}

fn update_activity_entity_for_contexts(
    tx: &Transaction<'_>,
    kind: &str,
    source_id: i64,
    target_id: i64,
    context_ids: &[i64],
) -> Result<()> {
    let Some(column) = entity_activity_column(kind) else {
        return Ok(());
    };
    if context_ids.is_empty() {
        return Ok(());
    }
    let placeholders = sql_placeholders(context_ids.len());
    let mut values = vec![
        rusqlite::types::Value::Integer(target_id),
        rusqlite::types::Value::Integer(source_id),
    ];
    values.extend(
        context_ids
            .iter()
            .copied()
            .map(rusqlite::types::Value::Integer),
    );
    tx.execute(
        &format!(
            "UPDATE activity_intervals SET {column}=? WHERE {column}=? AND source_context_interval_id IN ({placeholders})"
        ),
        params_from_iter(values),
    )?;
    if let Some(focus_column) = entity_focus_column(kind) {
        let mut focus_values = vec![
            rusqlite::types::Value::Integer(target_id),
            rusqlite::types::Value::Integer(source_id),
        ];
        focus_values.extend(
            context_ids
                .iter()
                .copied()
                .map(rusqlite::types::Value::Integer),
        );
        tx.execute(
            &format!("UPDATE focus_context_intervals SET {focus_column}=? WHERE {focus_column}=? AND id IN ({placeholders})"),
            params_from_iter(focus_values),
        )?;
    }
    Ok(())
}

fn update_focus_entity_for_contexts(
    tx: &Transaction<'_>,
    kind: &str,
    source_id: i64,
    target_id: i64,
    context_ids: &[i64],
) -> Result<()> {
    let Some(column) = entity_focus_column(kind) else {
        return Ok(());
    };
    if context_ids.is_empty() {
        return Ok(());
    }
    let placeholders = sql_placeholders(context_ids.len());
    let mut values = vec![
        rusqlite::types::Value::Integer(target_id),
        rusqlite::types::Value::Integer(source_id),
    ];
    values.extend(
        context_ids
            .iter()
            .copied()
            .map(rusqlite::types::Value::Integer),
    );
    tx.execute(
        &format!("UPDATE focus_context_intervals SET {column}=? WHERE {column}=? AND id IN ({placeholders})"),
        params_from_iter(values),
    )?;
    Ok(())
}

fn entity_activity_column(kind: &str) -> Option<&'static str> {
    match kind {
        "project" => Some("project_entity_id"),
        "service" => Some("service_entity_id"),
        "tool" => Some("tool_entity_id"),
        _ => None,
    }
}

fn entity_focus_column(kind: &str) -> Option<&'static str> {
    match kind {
        "project" => Some("project_entity_id"),
        "repository" => Some("repository_entity_id"),
        _ => None,
    }
}

fn relation_reaches(db: &Connection, from: i64, target: i64, relation: &str) -> Result<bool> {
    let found: bool = db.query_row(
        "WITH RECURSIVE descendants(id) AS ( \
           SELECT child_entity_id FROM context_entity_relations WHERE parent_entity_id=? AND relation=? \
           UNION SELECT r.child_entity_id FROM context_entity_relations r JOIN descendants d ON r.parent_entity_id=d.id WHERE r.relation=? \
         ) SELECT EXISTS(SELECT 1 FROM descendants WHERE id=?)",
        params![from, relation, relation, target],
        |row| row.get(0),
    )?;
    Ok(found)
}

#[derive(Clone)]
struct StoredPolicy {
    id: i64,
    grace: Value,
    revision: i64,
}

fn load_policy(db: &Connection, id: i64) -> Result<StoredPolicy> {
    db.query_row(
        "SELECT id,grace_json,revision FROM attention_policies WHERE id=?",
        params![id],
        |row| {
            let grace: String = row.get(1)?;
            Ok(StoredPolicy {
                id: row.get(0)?,
                grace: serde_json::from_str(&grace).unwrap_or_else(|_| json!({})),
                revision: row.get(2)?,
            })
        },
    )
    .optional()?
    .ok_or_else(|| anyhow!("policy not found"))
}

fn validate_policy(policy: &PolicyInput) -> Result<()> {
    bounded_text(&policy.name, 160, "policy name")?;
    validate_scope_size(&policy.scope)?;
    if !matches!(
        policy.metric.as_str(),
        "tab_active_seconds"
            | "browser_focused_seconds"
            | "document_visible_seconds"
            | "media_playback_seconds"
            | "audible_playback_seconds"
            | "focused_playback_seconds"
            | "verified_focused_playback_seconds"
            | "background_playback_seconds"
    ) {
        bail!("unsupported policy metric");
    }
    policy_window(&policy.window_kind, 0, &policy.scope)?;
    if !(1..=31_536_000).contains(&policy.budget_seconds) {
        bail!("policy budget must be between 1 second and 1 year");
    }
    if policy
        .warning_seconds
        .is_some_and(|value| value < 0 || value > policy.budget_seconds)
    {
        bail!("warning threshold must be within the budget");
    }
    if !matches!(
        policy.enforcement.as_str(),
        "observe" | "warn" | "soft-block" | "hard-block"
    ) {
        bail!("unsupported enforcement level");
    }
    validate_scope_size(&policy.schedule)?;
    validate_scope_size(&policy.grace)?;
    if let Some(days) = policy.schedule.get("weekdays").and_then(Value::as_array) {
        if days
            .iter()
            .any(|day| day.as_i64().is_none_or(|day| !(1..=7).contains(&day)))
        {
            bail!("schedule weekdays must be ISO values 1 through 7");
        }
    }
    for key in ["start", "end"] {
        if let Some(value) = policy.schedule.get(key).and_then(Value::as_str) {
            parse_hhmm(value)?;
        }
    }
    Ok(())
}

fn policy_window(kind: &str, now: i64, scope: &Value) -> Result<(i64, i64)> {
    if kind == "calendar-day" {
        return Ok(local_day_bounds(now));
    }
    if kind == "session" {
        return Ok((
            now.saturating_sub(MAX_POLICY_LOOKBACK_MS),
            now.saturating_add(1),
        ));
    }
    if kind == "rolling" {
        let hours = scope
            .get("rollingHours")
            .and_then(Value::as_i64)
            .unwrap_or(24);
        if !(1..=24 * 30).contains(&hours) {
            bail!("rolling policy window must be 1..=720 hours");
        }
        return Ok((now.saturating_sub(hours * 3_600_000), now.saturating_add(1)));
    }
    bail!("unsupported policy window");
}

fn insert_policy_exception(
    db: &mut Connection,
    policy: &StoredPolicy,
    client_id: Option<i64>,
    service_entity_id: Option<i64>,
    now: i64,
    action: &str,
    duration_seconds: i64,
) -> Result<i64> {
    let tx = db.transaction()?;
    let end_ts = now.saturating_add(duration_seconds.saturating_mul(1_000));
    tx.execute(
        "INSERT INTO policy_overrides \
         (policy_id,client_id,action,start_ts,end_ts,created_ts) VALUES (?,?,?,?,?,?)",
        params![policy.id, client_id, action, now, end_ts, now],
    )?;
    let override_id = tx.last_insert_rowid();
    tx.execute(
        "INSERT INTO intervention_events \
         (policy_id,client_id,event_ts,action,service_entity_id,remaining_seconds,decision_revision) \
         VALUES (?,?,?,?,?,?,?)",
        params![
            policy.id,
            client_id,
            now,
            action,
            service_entity_id,
            duration_seconds,
            policy.revision
        ],
    )?;
    tx.commit()?;
    Ok(override_id)
}

fn storage_metrics(db: &Connection, now: i64) -> Result<StorageMetrics> {
    let page_size = pragma_i64(db, "page_size")?;
    let page_count = pragma_i64(db, "page_count")?;
    let freelist_pages = pragma_i64(db, "freelist_count")?;
    let known_tables = [
        "context_entities",
        "context_entity_aliases",
        "context_entity_relations",
        "focus_context_intervals",
        "label_rules",
        "labeler_runs",
        "focus_interval_labels",
        "manual_activity_labels",
        "activity_intervals",
        "relabel_jobs",
        "derived_version_prune_jobs",
        "context_backfill_state",
        "browser_pairing_codes",
        "browser_pairing_rate_limits",
        "browser_clients",
        "browser_request_rate_limits",
        "browser_current_states",
        "browser_event_receipts",
        "browser_events",
        "browser_ingestion_metrics",
        "browser_attention_intervals",
        "agent_presence_leases",
        "agent_state_events",
        "attention_policies",
        "policy_overrides",
        "intervention_events",
        "source_health",
    ];
    let (allocation_capability, bytes) = match crate::db::sqlite_object_allocations(db) {
        crate::db::DbstatCapability::Available(allocations) => (
            "dbstat".to_string(),
            allocations
                .into_iter()
                .map(|allocation| (allocation.name, allocation.allocated_bytes))
                .collect::<HashMap<_, _>>(),
        ),
        crate::db::DbstatCapability::Unavailable { category } => {
            (category.to_string(), HashMap::new())
        }
    };
    let mut objects = Vec::new();
    for table in known_tables {
        if !table_exists(db, table)? {
            continue;
        }
        let rows = db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })?;
        objects.push(TableStorageMetric {
            name: table.to_string(),
            kind: "table".to_string(),
            rows: Some(rows),
            allocated_bytes: bytes.get(table).copied(),
        });
    }
    let mut indexes =
        db.prepare("SELECT name,tbl_name FROM sqlite_master WHERE type='index' ORDER BY name")?;
    for row in indexes.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (name, table) = row?;
        if known_tables.contains(&table.as_str()) {
            objects.push(TableStorageMetric {
                name: name.clone(),
                kind: "index".to_string(),
                rows: None,
                allocated_bytes: bytes.get(&name).copied(),
            });
        }
    }
    let open_context_intervals =
        optional_count(db, "focus_context_intervals", "closed_reason IS NULL")?;
    let closed_context_intervals =
        optional_count(db, "focus_context_intervals", "closed_reason IS NOT NULL")?;
    let thirty_days_ago = now.saturating_sub(30 * 86_400_000);
    let context_recent = optional_count(
        db,
        "focus_context_intervals",
        &format!("start_ts>={thirty_days_ago}"),
    )?;
    let browser_recent = optional_count(
        db,
        "browser_events",
        &format!("event_ts>={thirty_days_ago}"),
    )?;
    let current_label_version = if table_exists(db, "context_runtime_state")? {
        active_labeler_version(db)?
    } else if table_exists(db, "focus_interval_labels")? {
        db.query_row(
            "SELECT COALESCE(MAX(labeler_version),0) FROM focus_interval_labels",
            [],
            |row| row.get(0),
        )?
    } else {
        0
    };
    let previous_label_version = if current_label_version > 0 {
        db.query_row(
            "SELECT MAX(labeler_version) FROM focus_interval_labels WHERE labeler_version<?",
            params![current_label_version],
            |row| row.get(0),
        )?
    } else {
        None
    };
    let context_bytes = (allocation_capability == "dbstat").then(|| {
        objects
            .iter()
            .filter_map(|object| object.allocated_bytes)
            .sum::<i64>()
    });
    let observed_bounds: (Option<i64>, Option<i64>) = db.query_row(
        "SELECT MIN(start_ts),MAX(end_ts) FROM (SELECT start_ts,end_ts FROM focus_context_intervals UNION ALL SELECT start_ts,end_ts FROM browser_attention_intervals)",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let observed_complete_days = observed_bounds
        .0
        .zip(observed_bounds.1)
        .map(|(first, last)| {
            let date_span = crate::context::local_date(last).to_julian_day()
                - crate::context::local_date(first).to_julian_day();
            (date_span - i32::from(first != crate::context::local_midnight_ms(first))).max(0) as i64
        })
        .unwrap_or(0);
    let estimated_annualized_bytes = context_bytes.and_then(|bytes| {
        (observed_complete_days > 0)
            .then(|| (bytes as f64 * 365.0 / observed_complete_days as f64) as i64)
    });
    let oldest_prune_work: Option<i64> = db.query_row(
        "SELECT MIN(created_ts) FROM (SELECT created_ts FROM relabel_jobs WHERE status IN ('queued','running') UNION ALL SELECT created_ts FROM derived_version_prune_jobs WHERE status IN ('queued','running'))",
        [],
        |row| row.get(0),
    )?;
    let oldest_stale_state: Option<i64> = db.query_row(
        "SELECT MIN(expires_ts) FROM browser_current_states WHERE expires_ts<?",
        params![now],
        |row| row.get(0),
    )?;
    let rate_days = observed_complete_days.clamp(1, 30) as f64;
    Ok(StorageMetrics {
        objects,
        allocation_capability,
        page_size,
        page_count,
        freelist_pages,
        reusable_bytes: page_size.saturating_mul(freelist_pages),
        open_context_intervals,
        closed_context_intervals,
        context_transitions_per_day: context_recent as f64 / rate_days,
        browser_transitions_per_day: browser_recent as f64 / rate_days,
        heartbeat_upserts: optional_scalar(db, "browser_current_states", "heartbeat_upserts")?,
        persisted_browser_transitions: table_exists(db, "browser_events")?
            .then_some(optional_count(db, "browser_events", "1=1").unwrap_or(0)),
        current_label_version,
        current_label_bytes: None,
        previous_label_version,
        previous_label_bytes: None,
        estimated_annualized_bytes,
        observed_complete_days,
        pruning_lag_ms: oldest_prune_work.map_or(0, |ts| now.saturating_sub(ts)),
        compaction_lag_ms: oldest_stale_state.map_or(0, |ts| now.saturating_sub(ts)),
    })
}

fn pragma_i64(db: &Connection, name: &str) -> Result<i64> {
    Ok(db.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))?)
}

fn optional_count(db: &Connection, table: &str, predicate: &str) -> Result<i64> {
    if !table_exists(db, table)? {
        return Ok(0);
    }
    Ok(db.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
        [],
        |row| row.get(0),
    )?)
}

fn optional_scalar(db: &Connection, table: &str, column: &str) -> Result<Option<i64>> {
    if !table_exists(db, table)? || !column_exists(db, table, column)? {
        return Ok(None);
    }
    Ok(
        db.query_row(&format!("SELECT SUM({column}) FROM {table}"), [], |row| {
            row.get(0)
        })?,
    )
}

fn current_rules_revision(db: &Connection) -> Result<i64> {
    if table_exists(db, "context_runtime_state")? {
        return Ok(db.query_row(
            "SELECT rules_revision FROM context_runtime_state WHERE id=1",
            [],
            |row| row.get(0),
        )?);
    }
    if !table_exists(db, "label_rules")? {
        return Ok(0);
    }
    Ok(db.query_row(
        "SELECT COALESCE(MAX(revision),0) FROM label_rules WHERE enabled=1",
        [],
        |row| row.get(0),
    )?)
}

fn active_labeler_version(db: &Connection) -> Result<i64> {
    if table_exists(db, "context_runtime_state")? {
        return Ok(db.query_row(
            "SELECT active_labeler_version FROM context_runtime_state WHERE id=1",
            [],
            |row| row.get(0),
        )?);
    }
    classifier_version(db)
}

fn latest_rules_revision(db: &Connection) -> Result<i64> {
    if !table_exists(db, "label_rules")? {
        return Ok(0);
    }
    Ok(db.query_row(
        "SELECT COALESCE(MAX(revision),0) FROM label_rules",
        [],
        |row| row.get(0),
    )?)
}

fn classifier_version(db: &Connection) -> Result<i64> {
    active_labeler_version(db)
}

fn entity_names(db: &Connection) -> Result<HashMap<i64, String>> {
    let mut stmt = db.prepare("SELECT id,display_name FROM context_entities")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

fn session_names(db: &Connection) -> Result<HashMap<i64, String>> {
    if !table_exists(db, "agent_sessions")? {
        return Ok(HashMap::new());
    }
    let mut stmt =
        db.prepare("SELECT id,COALESCE(name,project_path,native_session_id) FROM agent_sessions")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

fn load_aliases(db: &Connection, entity_id: i64) -> Result<Vec<EntityAlias>> {
    let mut stmt = db.prepare(
        "SELECT id,alias_kind,alias_value,source,confidence,created_ts \
         FROM context_entity_aliases WHERE entity_id=? ORDER BY alias_kind,alias_value,id",
    )?;
    let rows = stmt
        .query_map(params![entity_id], |row| {
            Ok(EntityAlias {
                id: row.get(0)?,
                kind: row.get(1)?,
                value: row.get(2)?,
                source: row.get(3)?,
                confidence: row.get(4)?,
                created_ts: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn load_relations(db: &Connection, entity_id: i64) -> Result<Vec<EntityRelation>> {
    let mut stmt = db.prepare(
        "SELECT parent_entity_id,child_entity_id,relation,confidence,source \
         FROM context_entity_relations WHERE parent_entity_id=? OR child_entity_id=? \
         ORDER BY relation,parent_entity_id,child_entity_id",
    )?;
    let rows = stmt
        .query_map(params![entity_id, entity_id], |row| {
            Ok(EntityRelation {
                parent_entity_id: row.get(0)?,
                child_entity_id: row.get(1)?,
                relation: row.get(2)?,
                confidence: row.get(3)?,
                source: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn validate_manual_label(db: &Connection, input: &ManualLabelInput) -> Result<()> {
    if input.end_ts <= input.start_ts || input.end_ts - input.start_ts > MAX_QUERY_RANGE_MS {
        bail!("invalid manual label range");
    }
    validate_dimension(&input.dimension)?;
    if input.value.is_some() == input.entity_id.is_some() {
        bail!("manual label must set exactly one of value or entityId");
    }
    if let Some(value) = &input.value {
        bounded_text(value, 240, "manual label value")?;
        if input.dimension == "activity.primary" {
            validate_activity(value)?;
        }
    }
    if let Some(entity_id) = input.entity_id {
        ensure_entity(db, entity_id)?;
        if !matches!(
            input.dimension.as_str(),
            "project" | "repository" | "service" | "tool" | "account" | "agent.session"
        ) {
            bail!("manual label dimension does not accept an entity");
        }
    }
    if let Some(note) = &input.note {
        bounded_text(note, 500, "manual label note")?;
    }
    Ok(())
}

fn ensure_entity(db: &Connection, id: i64) -> Result<()> {
    if !db.query_row(
        "SELECT EXISTS(SELECT 1 FROM context_entities WHERE id=?)",
        params![id],
        |row| row.get::<_, bool>(0),
    )? {
        bail!("entity not found");
    }
    Ok(())
}

fn validate_range(from_ts: i64, to_ts: i64) -> Result<()> {
    let span = to_ts
        .checked_sub(from_ts)
        .filter(|span| *span > 0)
        .ok_or_else(|| anyhow!("range end must be after start"))?;
    if span > MAX_QUERY_RANGE_MS {
        bail!("range exceeds the maximum supported query span");
    }
    Ok(())
}

fn validate_activity(value: &str) -> Result<()> {
    if !ACTIVITY_VALUES.contains(&value) {
        bail!("invalid primary activity {value}");
    }
    Ok(())
}

fn validate_dimension(value: &str) -> Result<()> {
    if !DIMENSIONS.contains(&value) {
        bail!("unsupported label dimension {value}");
    }
    Ok(())
}

fn validate_confidence(value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        bail!("confidence must be finite and between zero and one");
    }
    Ok(())
}

fn alias_precedence(source: &str, entity_confirmed: bool) -> i32 {
    if entity_confirmed || source == "user" {
        50
    } else {
        match source {
            "exact" | "integration" => 40,
            "built_in" => 30,
            "heuristic" => 20,
            _ => 10,
        }
    }
}

fn validate_identifier(value: &str, max: usize, label: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > max
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        bail!("invalid {label}");
    }
    Ok(())
}

fn validate_domain(value: &str) -> Result<()> {
    let value = normalized_domain(value);
    if value.is_empty() || value.len() > 253 || !value.contains('.') {
        bail!("invalid domain");
    }
    for label in value.split('.') {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            bail!("invalid domain label");
        }
    }
    Ok(())
}

fn validate_scope_size(value: &Value) -> Result<()> {
    let encoded = serde_json::to_vec(value)?;
    if encoded.len() > 16 * 1024 {
        bail!("JSON object exceeds 16 KiB");
    }
    if json_depth(value) > 8 {
        bail!("JSON object exceeds maximum nesting");
    }
    Ok(())
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

fn bounded_text<'a>(value: &'a str, max: usize, label: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max {
        bail!("{label} must contain 1..={max} characters");
    }
    Ok(value)
}

fn bounded_optional(value: &Option<String>, max: usize) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(max).collect())
}

fn parse_wrapped_list<T: for<'de> Deserialize<'de>>(raw: &str, key: &str) -> Result<Vec<T>> {
    let value: Value = serde_json::from_str(raw)?;
    let value = if value.is_array() {
        value
    } else {
        value
            .get(key)
            .cloned()
            .ok_or_else(|| anyhow!("missing {key} list"))?
    };
    Ok(serde_json::from_value(value)?)
}

fn canonical_json(value: &Value) -> String {
    fn canonical(value: &Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut keys = object.keys().collect::<Vec<_>>();
                keys.sort();
                let mut output = Map::new();
                for key in keys {
                    output.insert(key.clone(), canonical(&object[key]));
                }
                Value::Object(output)
            }
            Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&canonical(value)).unwrap_or_else(|_| "null".to_string())
}

fn evidence_base(value: &Value) -> Value {
    let mut value = value.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("sourceContextIntervalIds");
        object.remove("activityIntervalIds");
    }
    value
}

fn typed_hash(kind: &str, identity: &str) -> [u8; 32] {
    hash_parts(&[kind.as_bytes(), identity.as_bytes()])
}

fn hash_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((*part).len().to_be_bytes());
        digest.update(part);
    }
    digest.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn sql_placeholders(count: usize) -> String {
    std::iter::repeat("?")
        .take(count)
        .collect::<Vec<_>>()
        .join(",")
}

fn table_exists(db: &Connection, table: &str) -> Result<bool> {
    if table == "dbstat" {
        return Ok(db.prepare("SELECT name FROM dbstat LIMIT 0").is_ok());
    }
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        params![table],
        |row| row.get(0),
    )?)
}

fn column_exists(db: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|name| name == column))
}

fn overlap_ms(a_start: i64, a_end: i64, b_start: i64, b_end: i64) -> i64 {
    a_end.min(b_end).saturating_sub(a_start.max(b_start)).max(0)
}

fn bool_i(value: bool) -> i64 {
    i64::from(value)
}

fn append_unique(target: &mut Vec<i64>, values: &[i64]) {
    for value in values {
        if !target.contains(value) {
            target.push(*value);
        }
    }
    target.sort_unstable();
}

fn local_datetime(ts_ms: i64) -> OffsetDateTime {
    let utc = OffsetDateTime::from_unix_timestamp_nanos(ts_ms as i128 * 1_000_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let offset = UtcOffset::local_offset_at(utc).unwrap_or(UtcOffset::UTC);
    utc.to_offset(offset)
}

fn local_date_key(ts_ms: i64) -> String {
    local_datetime(ts_ms).date().to_string()
}

fn local_day_bounds(ts_ms: i64) -> (i64, i64) {
    let local = local_datetime(ts_ms);
    let primitive = PrimitiveDateTime::new(local.date(), Time::MIDNIGHT);
    let local_offset = local.offset();
    let start_offset =
        UtcOffset::local_offset_at(primitive.assume_offset(local_offset)).unwrap_or(local_offset);
    let start = primitive.assume_offset(start_offset);
    let next_primitive = PrimitiveDateTime::new(
        local.date().next_day().unwrap_or(local.date()),
        Time::MIDNIGHT,
    );
    let next_offset = UtcOffset::local_offset_at(next_primitive.assume_offset(start_offset))
        .unwrap_or(start_offset);
    let end = next_primitive.assume_offset(next_offset);
    (
        start.unix_timestamp_nanos() as i64 / 1_000_000,
        end.unix_timestamp_nanos() as i64 / 1_000_000,
    )
}

fn parse_hhmm(value: &str) -> Result<i64> {
    let (hour, minute) = value
        .split_once(':')
        .ok_or_else(|| anyhow!("schedule time must be HH:MM"))?;
    let hour: i64 = hour.parse().context("invalid schedule hour")?;
    let minute: i64 = minute.parse().context("invalid schedule minute")?;
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
        bail!("schedule time is out of range");
    }
    Ok(hour * 60 + minute)
}

fn default_schema_version() -> i64 {
    1
}

fn default_true() -> bool {
    true
}

fn default_user_origin() -> String {
    "user".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::init_db;

    fn db() -> Connection {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        db.execute(
            "UPDATE context_runtime_state SET active_labeler_version=2 WHERE id=1",
            [],
        )
        .unwrap();
        db
    }

    fn entity(db: &Connection, kind: &str, name: &str) -> i64 {
        let key = typed_hash(kind, name);
        db.execute(
            "INSERT INTO context_entities (kind,canonical_key,display_name,first_seen_ts,last_seen_ts) VALUES (?,?,?,?,?)",
            params![kind, key.as_slice(), name, 0, 10_000],
        )
        .unwrap();
        db.last_insert_rowid()
    }

    fn activity(
        db: &Connection,
        start: i64,
        end: i64,
        source: &str,
        kind: Option<&str>,
        project: Option<i64>,
        confidence: f64,
        evidence: Value,
    ) {
        db.execute(
            "INSERT INTO activity_intervals (start_ts,end_ts,source,activity,project_entity_id,confidence,labeler_version,evidence_json) VALUES (?,?,?,?,?,?,?,?)",
            params![start, end, source, kind, project, confidence, 2, evidence.to_string()],
        )
        .unwrap();
    }

    #[test]
    fn domains_require_a_label_boundary() {
        assert!(domain_suffix_matches("youtube.com", "youtube.com"));
        assert!(domain_suffix_matches("www.youtube.com", "youtube.com"));
        assert!(domain_suffix_matches(
            "https://www.youtube.com/watch",
            "youtube.com"
        ));
        assert!(!domain_suffix_matches("notyoutube.com", "youtube.com"));
        assert!(!domain_suffix_matches(
            "youtube.com.evil.test",
            "youtube.com"
        ));
    }

    #[test]
    fn bounded_regex_supports_safe_subset_and_rejects_unbounded_constructs() {
        let regex = SafeRegex::compile(r"^you[a-z]+\.com$").unwrap();
        assert!(regex.is_match("youtube.com"));
        assert!(!regex.is_match("notyoutube.com"));
        assert!(SafeRegex::compile("(a+)+$").is_err());
        assert!(SafeRegex::compile("a{1,}").is_err());
        assert!(!SafeRegex::compile("a*")
            .unwrap()
            .is_match(&"a".repeat(MAX_REGEX_INPUT_CHARS + 1)));
    }

    #[test]
    fn rule_validation_rejects_unmaterialized_browser_facts_and_bad_activity() {
        let valid = RuleInput {
            name: "application".into(),
            priority: 1,
            enabled: true,
            origin: "user".into(),
            conditions: vec![RuleCondition {
                fact: "app.id".into(),
                operator: "exact".into(),
                value: Some(json!("browser")),
            }],
            outputs: vec![RuleOutput {
                dimension: "activity.primary".into(),
                value: Some("entertainment".into()),
                entity: None,
                confidence: Some(0.9),
            }],
            stop_dimensions: vec!["activity.primary".into()],
        };
        validate_rule(&valid).unwrap();
        for fact in ["browser.domain", "browser.origin", "browser.mediaPlaying"] {
            let mut invalid = valid.clone();
            invalid.conditions[0].fact = fact.into();
            assert!(
                validate_rule(&invalid).is_err(),
                "accepted no-op fact {fact}"
            );
        }
        let mut invalid = valid;
        invalid.outputs[0].value = Some("video-service".into());
        assert!(validate_rule(&invalid).is_err());
    }

    #[test]
    fn overlap_reconciliation_prevents_double_counting_and_reports_coverage() {
        let mut db = db();
        activity(
            &db,
            0,
            10_000,
            "os_focus",
            Some("work"),
            None,
            0.8,
            json!({}),
        );
        activity(
            &db,
            2_000,
            7_000,
            "browser_verified",
            Some("entertainment"),
            None,
            1.0,
            json!({"focusedPlayback":true}),
        );
        let repo = ActivityRepository::new(&mut db);
        let summary = repo.summary(0, 10_000, ActivityGroup::Activity).unwrap();
        assert_eq!(summary.meta.observed_ms, 10_000);
        assert_eq!(summary.meta.monitoring_coverage, 1.0);
        assert_eq!(
            summary
                .groups
                .iter()
                .map(|group| group.duration_ms)
                .sum::<i64>(),
            10_000
        );
        assert_eq!(
            summary
                .groups
                .iter()
                .find(|group| group.key == "entertainment")
                .unwrap()
                .duration_ms,
            5_000
        );
    }

    #[test]
    fn episodes_cross_midnight_without_becoming_two_episodes() {
        let mut db = db();
        let midnight = local_day_bounds(1_700_000_000_000).1;
        activity(
            &db,
            midnight - 2_000,
            midnight + 2_000,
            "browser_verified",
            Some("entertainment"),
            None,
            1.0,
            json!({}),
        );
        let repo = ActivityRepository::new(&mut db);
        let episodes = repo
            .episodes(midnight - 10_000, midnight + 10_000, "entertainment", 1)
            .unwrap();
        assert_eq!(episodes.len(), 1);
        assert!(episodes[0].crosses_local_midnight);
    }

    #[test]
    fn reconciliation_preserves_adjacent_distinct_persisted_episode_keys() {
        let mut db = db();
        for (start, episode) in [(0, vec![1_u8; 32]), (1_000, vec![2_u8; 32])] {
            db.execute(
                "INSERT INTO activity_intervals(start_ts,end_ts,source,activity,confidence,labeler_version,episode_key,evidence_json) VALUES(?,?,'fixture','work',1,1,?,'{}')",
                params![start, start + 1_000, episode],
            )
            .unwrap();
        }
        let episodes = ActivityRepository::new(&mut db)
            .episodes(0, 2_000, "work", 0)
            .unwrap();
        assert_eq!(episodes.len(), 2);
        assert_ne!(episodes[0].episode_key, episodes[1].episode_key);
    }

    #[test]
    fn manual_labels_are_validated_and_preserved_separately() {
        let mut db = db();
        let project = entity(&db, "project", "watchdog");
        let mut repo = ActivityRepository::new(&mut db);
        let id = repo
            .add_manual_label(
                &ManualLabelInput {
                    start_ts: 1,
                    end_ts: 2,
                    dimension: "project".into(),
                    value: None,
                    entity_id: Some(project),
                    note: Some("correction".into()),
                },
                3,
            )
            .unwrap();
        assert!(id > 0);
        assert!(repo
            .add_manual_label(
                &ManualLabelInput {
                    start_ts: 1,
                    end_ts: 2,
                    dimension: "activity.primary".into(),
                    value: Some("not-real".into()),
                    entity_id: None,
                    note: None,
                },
                3,
            )
            .is_err());
    }

    #[test]
    fn merge_and_split_rewrite_aliases_and_scoped_labels() {
        let mut db = db();
        let a = entity(&db, "project", "a");
        let b = entity(&db, "project", "b");
        let mut repo = ActivityRepository::new(&mut db);
        let alias = repo
            .add_entity_alias(b, "repository", "/b", "exact", 1.0, 1)
            .unwrap();
        repo.merge_entities(a, b).unwrap();
        let owner: i64 = repo
            .db
            .query_row(
                "SELECT entity_id FROM context_entity_aliases WHERE id=?",
                params![alias],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, a);
        let split = repo
            .split_entity(
                a,
                &EntitySplitInput {
                    display_name: "split".into(),
                    identity: "/split".into(),
                    alias_ids: vec![alias],
                    context_interval_ids: vec![],
                    manual_label_ids: vec![],
                },
                10,
            )
            .unwrap();
        assert_ne!(split, a);
        let owner: i64 = repo
            .db
            .query_row(
                "SELECT entity_id FROM context_entity_aliases WHERE id=?",
                params![alias],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, split);
    }

    #[test]
    fn higher_precedence_alias_cannot_be_stolen_by_a_heuristic() {
        let mut db = db();
        let confirmed = entity(&db, "project", "confirmed");
        let heuristic = entity(&db, "project", "heuristic");
        let mut repo = ActivityRepository::new(&mut db);
        let alias_id = repo
            .add_entity_alias(confirmed, "repository", "shared", "user", 1.0, 1)
            .unwrap();
        assert_eq!(
            repo.add_entity_alias(heuristic, "repository", "shared", "heuristic", 1.0, 2)
                .unwrap(),
            alias_id
        );
        let owner: i64 = repo
            .db
            .query_row(
                "SELECT entity_id FROM context_entity_aliases WHERE id=?",
                params![alias_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, confirmed);
    }

    #[test]
    fn storage_metrics_include_reusable_pages_and_context_rows() {
        let mut db = db();
        db.execute("INSERT INTO browser_clients(client_key,client_instance_id,display_name,browser_family,extension_version,secret_hash,allowed_origin,privacy_mode,paired_ts)VALUES(randomblob(32),'allocation-client','Allocation client','firefox','1',randomblob(32),'moz-extension://allocation','domains-only',1)", []).unwrap();
        let metrics = ActivityRepository::new(&mut db)
            .storage_metrics(10_000)
            .unwrap();
        assert!(metrics.page_size > 0);
        assert!(metrics
            .objects
            .iter()
            .any(|object| object.name == "activity_intervals"));
        if metrics.allocation_capability == "dbstat" {
            assert!(metrics.objects.iter().any(|object| {
                object.kind == "index"
                    && object.name.starts_with("sqlite_autoindex_browser_clients_")
                    && object.allocated_bytes.is_some_and(|bytes| bytes > 0)
            }));
            let table_names = metrics
                .objects
                .iter()
                .filter(|object| object.kind == "table")
                .map(|object| object.name.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let mut relevant_names = table_names.clone();
            let mut statement = db
                .prepare("SELECT name,tbl_name FROM sqlite_master WHERE type='index'")
                .unwrap();
            for row in statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .unwrap()
            {
                let (name, table) = row.unwrap();
                if table_names.contains(&table) {
                    relevant_names.insert(name);
                }
            }
            let expected = match crate::db::sqlite_object_allocations(&db) {
                crate::db::DbstatCapability::Available(rows) => rows
                    .into_iter()
                    .filter(|row| relevant_names.contains(&row.name))
                    .map(|row| row.allocated_bytes)
                    .sum::<i64>(),
                crate::db::DbstatCapability::Unavailable { .. } => unreachable!(),
            };
            let reported = metrics
                .objects
                .iter()
                .filter_map(|object| object.allocated_bytes)
                .sum::<i64>();
            assert_eq!(reported, expected);
            assert_eq!(metrics.estimated_annualized_bytes, None);
        }
    }
}
