use std::collections::{BTreeMap, HashMap};

use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

pub(crate) use crate::activity::{EntitySplitInput, RuleInput};

use crate::{
    activity::{
        self, ActivityGroup, ActivityRepository, ManualLabelInput as CoreManualLabel,
        PolicyInput as CorePolicyInput,
    },
    util::now_ms,
};

const MAX_RANGE_MS: i64 = 366 * 86_400_000;
const MAX_RESULT_ROWS: i64 = 2_000;
const MAX_INTEGRATION_INTERVALS: i64 = 250_000;
const DELETE_BATCH_ROWS: i64 = 500;
const MAX_PREVIEW_ROWS: i64 = 5_000;

#[derive(Clone, Deserialize)]
pub(crate) struct RangeQuery {
    pub(crate) from: Option<i64>,
    pub(crate) to: Option<i64>,
    pub(crate) group: Option<String>,
    pub(crate) metric: Option<String>,
    pub(crate) resolution: Option<String>,
    pub(crate) limit: Option<i64>,
    #[serde(rename = "minSeconds")]
    pub(crate) min_seconds: Option<i64>,
    pub(crate) kind: Option<String>,
    pub(crate) format: Option<String>,
    #[serde(rename = "includePartialDays")]
    pub(crate) include_partial_days: Option<bool>,
    #[serde(rename = "maxFollowGapSeconds")]
    pub(crate) max_follow_gap_seconds: Option<i64>,
}

fn range(query: &RangeQuery) -> Result<(i64, i64)> {
    let to = query.to.unwrap_or_else(now_ms);
    let from = query.from.unwrap_or(to - 7 * 86_400_000);
    if from >= to {
        bail!("from must be before to");
    }
    if to - from > MAX_RANGE_MS {
        bail!("range exceeds 366 days");
    }
    Ok((from, to))
}

fn activity_group(value: Option<&str>) -> Result<ActivityGroup> {
    match value.unwrap_or("activity") {
        "activity" => Ok(ActivityGroup::Activity),
        "project" => Ok(ActivityGroup::Project),
        "service" => Ok(ActivityGroup::Service),
        "tool" => Ok(ActivityGroup::Tool),
        "agentSession" | "agent_session" => Ok(ActivityGroup::AgentSession),
        _ => bail!("unsupported activity group"),
    }
}

fn focus_metric(value: Option<&str>) -> bool {
    matches!(value.unwrap_or("focus"), "focus" | "focus_time")
}

pub(crate) fn activity_summary(db: &mut Connection, query: &RangeQuery) -> Result<Value> {
    let (from, to) = range(query)?;
    let metric = query.metric.as_deref().unwrap_or("focus");
    if !focus_metric(Some(metric)) {
        return integration_metric_summary(db, query, from, to, metric);
    }
    let group = activity_group(query.group.as_deref())?;
    let summary = ActivityRepository::new(db).summary(from, to, group)?;
    let mut payload = serde_json::to_value(summary)?;
    if payload["groups"]
        .as_array()
        .is_some_and(|groups| groups.len() > MAX_RESULT_ROWS as usize)
    {
        bail!("summary contains more than {MAX_RESULT_ROWS} distinct groups; narrow the range or filters");
    }
    let base_meta = payload["meta"].clone();
    let observed_ms = base_meta["observedMs"].as_i64().unwrap_or(0);
    let unknown_ms = base_meta["unknownMs"].as_i64().unwrap_or(0);
    payload["meta"] = aggregate_meta(
        db,
        from,
        to,
        from,
        to,
        "focus_time",
        observed_ms,
        observed_ms,
        unknown_ms,
        base_meta["confidence"].clone(),
        base_meta["sampleCount"].as_u64().unwrap_or(0) as usize,
        base_meta["episodeCount"].as_u64().unwrap_or(0) as usize,
        json!({"metric":"focus_time","group":query.group.as_deref().unwrap_or("activity")}),
        false,
        partial_boundary_count(from, to),
        observed_ms,
        0,
        observed_ms,
    )?;
    payload["metric"] = json!("focus_time");
    payload["metricDefinition"] =
        json!("Overlap-reconciled foreground attention; monitoring gaps are excluded.");
    payload["boundarySemantics"] = json!(
        "Intervals are clipped to [from,to); partial boundary days are identified, not inferred."
    );
    payload["partialBoundaryDays"] = json!(partial_boundary_count(from, to));
    Ok(payload)
}

#[derive(Clone)]
struct MetricInterval {
    raw_start: i64,
    raw_end: i64,
    start: i64,
    end: i64,
    key: String,
    label: String,
    confidence: f64,
    episode_key: String,
}

fn canonical_metric(metric: &str) -> Result<String> {
    let base = metric.strip_suffix("_seconds").unwrap_or(metric);
    browser_metric_predicate(base)?;
    Ok(format!("{base}_seconds"))
}

fn browser_metric_predicate(metric: &str) -> Result<&'static str> {
    match metric.strip_suffix("_seconds").unwrap_or(metric) {
        "tab_active" => Ok("b.tab_active=1"),
        "browser_focused" => Ok("b.browser_focused=1"),
        "document_visible" => Ok("b.document_visible=1"),
        "media_playback" => Ok("b.media_playing=1"),
        "audible_playback" => Ok("b.media_playing=1 AND b.audible=1"),
        "focused_playback" => Ok("b.browser_focused=1 AND b.tab_active=1 AND b.document_visible=1 AND b.media_playing=1 AND b.os_focus_state<>'contradicted'"),
        "verified_focused_playback" => Ok("b.browser_focused=1 AND b.tab_active=1 AND b.document_visible=1 AND b.media_playing=1 AND b.os_focus_state='verified'"),
        "background_playback" => Ok("b.media_playing=1 AND (b.browser_focused=0 OR b.tab_active=0 OR b.document_visible=0 OR b.os_focus_state='contradicted')"),
        _ => bail!("unsupported metric"),
    }
}

fn enforce_integration_ceiling(
    db: &Connection,
    table: &str,
    from: i64,
    to: i64,
    predicate: &str,
) -> Result<()> {
    let count: i64 = db.query_row(
        &format!(
            "SELECT COUNT(*) FROM {table} b WHERE b.end_ts>? AND b.start_ts<? AND {predicate}"
        ),
        params![from, to],
        |row| row.get(0),
    )?;
    if count > MAX_INTEGRATION_INTERVALS {
        bail!(
            "metric range contains {count} intervals; hard ceiling is {MAX_INTEGRATION_INTERVALS}"
        );
    }
    Ok(())
}

fn load_browser_metric_intervals(
    db: &Connection,
    from: i64,
    to: i64,
    metric: &str,
) -> Result<Vec<MetricInterval>> {
    let predicate = browser_metric_predicate(metric)?;
    enforce_integration_ceiling(db, "browser_attention_intervals", from, to, predicate)?;
    let sql = format!(
        "SELECT b.start_ts,b.end_ts,MAX(b.start_ts,?),MIN(b.end_ts,?),\
                CASE WHEN b.service_entity_id IS NOT NULL THEN 'service:'||b.service_entity_id ELSE 'domain:'||COALESCE(lower(b.domain),'unknown') END,\
                COALESCE(e.display_name,b.domain,'unknown'),b.confidence,hex(b.episode_key) \
         FROM browser_attention_intervals b LEFT JOIN context_entities e ON e.id=b.service_entity_id \
         WHERE b.end_ts>? AND b.start_ts<? AND {predicate} ORDER BY b.start_ts,b.id"
    );
    let mut stmt = db.prepare(&sql)?;
    let mut rows = stmt
        .query_map(params![from, to, from, to], |row| {
            Ok(MetricInterval {
                raw_start: row.get(0)?,
                raw_end: row.get(1)?,
                start: row.get(2)?,
                end: row.get(3)?,
                key: row.get(4)?,
                label: row.get(5)?,
                confidence: row.get::<_, f64>(6)?.clamp(0.0, 1.0),
                episode_key: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let canonical = canonical_metric(metric)?;
    let mut states = db.prepare(
        "SELECT s.state_started_ts,s.last_observed_ts,s.expires_ts,s.domain,s.browser_focused,s.tab_active,s.document_visible,s.audible,s.media_state,hex(s.episode_key),c.browser_family,\
                (SELECT b.service_entity_id FROM browser_attention_intervals b WHERE b.domain=s.domain AND b.service_entity_id IS NOT NULL ORDER BY b.end_ts DESC LIMIT 1) \
         FROM browser_current_states s JOIN browser_clients c ON c.id=s.client_id \
         WHERE s.expires_ts>? AND s.state_started_ts<? ORDER BY s.state_started_ts,s.client_id,s.window_id,s.tab_id",
    )?;
    for row in states.query_map(params![from, to], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)? != 0,
            row.get::<_, i64>(5)? != 0,
            row.get::<_, i64>(6)? != 0,
            row.get::<_, i64>(7)? != 0,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, Option<i64>>(11)?,
        ))
    })? {
        let (
            raw_start,
            observed,
            expires,
            domain,
            focused,
            active,
            visible,
            audible,
            media,
            episode_key,
            family,
            service_id,
        ) = row?;
        let raw_end = expires.min(observed.saturating_add(crate::browser_api::LEASE_MS));
        let start = raw_start.max(from);
        let end = raw_end.min(to);
        if end <= start {
            continue;
        }
        let (os_state, _) = crate::browser_api::reconcile_os(db, &family, start, end)?;
        if !crate::browser_api::metric_counts(
            &canonical,
            focused,
            active,
            visible,
            audible,
            matches!(media.as_str(), "playing" | "seeking"),
            os_state,
        ) {
            continue;
        }
        let domain = domain.unwrap_or_else(|| "unknown".into());
        rows.push(MetricInterval {
            raw_start,
            raw_end,
            start,
            end,
            key: service_id.map_or_else(
                || format!("domain:{}", domain.to_ascii_lowercase()),
                |id| format!("service:{id}"),
            ),
            label: domain,
            confidence: match os_state {
                "verified" => 0.98,
                "contradicted" => 0.95,
                "mixed" => 0.65,
                _ => 0.5,
            },
            episode_key,
        });
    }
    rows.sort_by_key(|row| {
        (
            row.raw_start,
            row.raw_end,
            row.episode_key.clone(),
            row.key.clone(),
        )
    });
    rows.dedup_by(|left, right| {
        left.raw_start == right.raw_start
            && left.raw_end == right.raw_end
            && left.episode_key == right.episode_key
    });
    if rows.len() as i64 > MAX_INTEGRATION_INTERVALS {
        bail!("metric range contains more than {MAX_INTEGRATION_INTERVALS} persisted/current intervals");
    }
    Ok(rows)
}

fn load_agent_busy_intervals(
    db: &Connection,
    query: &RangeQuery,
    from: i64,
    to: i64,
) -> Result<Vec<MetricInterval>> {
    let requested_group = query.group.as_deref().unwrap_or("agentSession");
    let exact_session = match requested_group {
        "agentSession" | "agent_session" => true,
        "harness" | "activity" => false,
        _ => bail!("agent_busy supports group=agentSession or group=harness"),
    };
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM bounded_agent_state_events WHERE state='busy' AND bounded_end_ts>? AND event_ts<?",
        params![from, to],
        |row| row.get(0),
    )?;
    if count > MAX_INTEGRATION_INTERVALS {
        bail!(
            "metric range contains {count} intervals; hard ceiling is {MAX_INTEGRATION_INTERVALS}"
        );
    }
    let mut stmt = db.prepare(
        "SELECT MAX(e.event_ts,?),MIN(e.bounded_end_ts,?),e.harness,e.source_key,e.native_session_id,e.instance_id,s.id,COALESCE(s.name,s.native_session_id,e.native_session_id) \
         FROM bounded_agent_state_events e \
         LEFT JOIN agent_sources source ON source.agent_type=e.harness AND source.source_key=e.source_key \
         LEFT JOIN agent_sessions s ON s.source_id=source.id AND s.native_session_id=e.native_session_id \
         WHERE e.state='busy' AND e.bounded_end_ts>? AND e.event_ts<? ORDER BY e.event_ts,e.id",
    )?;
    let rows = stmt
        .query_map(params![from, to, from, to], |row| {
            let harness: String = row.get(2)?;
            let source: String = row.get(3)?;
            let native: String = row.get(4)?;
            let instance: String = row.get(5)?;
            let session_id: Option<i64> = row.get(6)?;
            let session_label: String = row.get(7)?;
            let (key, label) = if exact_session {
                (
                    session_id
                        .map(|id| format!("agent-session:{id}"))
                        .unwrap_or_else(|| format!("agent-native:{harness}:{source}:{native}")),
                    session_label,
                )
            } else {
                (
                    format!("harness:{}", harness.to_ascii_lowercase()),
                    harness.clone(),
                )
            };
            let start: i64 = row.get(0)?;
            let end: i64 = row.get(1)?;
            Ok(MetricInterval {
                raw_start: start,
                raw_end: end,
                start,
                end,
                key,
                label,
                confidence: 1.0,
                episode_key: format!("agent:{harness}:{source}:{native}:{instance}"),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn integration_intervals(
    db: &Connection,
    query: &RangeQuery,
    from: i64,
    to: i64,
    metric: &str,
) -> Result<Vec<MetricInterval>> {
    if metric == "agent_busy" || metric == "agent_busy_seconds" {
        load_agent_busy_intervals(db, query, from, to)
    } else {
        if !matches!(query.group.as_deref(), None | Some("service")) {
            bail!("browser metrics support only group=service");
        }
        load_browser_metric_intervals(db, from, to, metric)
    }
}

fn canonical_integration_metric(metric: &str) -> Result<String> {
    if matches!(metric, "agent_busy" | "agent_busy_seconds") {
        Ok("agent_busy_seconds".into())
    } else {
        canonical_metric(metric)
    }
}

fn confidence_distribution(intervals: &[MetricInterval]) -> Value {
    let mut events = Vec::with_capacity(intervals.len() * 2);
    for row in intervals.iter().filter(|row| row.end > row.start) {
        let band = if row.confidence < 0.5 {
            0
        } else if row.confidence < 0.8 {
            1
        } else {
            2
        };
        events.push((row.start, band, 1_i64));
        events.push((row.end, band, -1_i64));
    }
    events.sort_unstable_by_key(|event| event.0);
    let mut totals = [0_i64; 3];
    let mut active = [0_i64; 3];
    let mut cursor = events.first().map_or(0, |event| event.0);
    let mut index = 0;
    while index < events.len() {
        let timestamp = events[index].0;
        if let Some(band) = (0..3).rev().find(|band| active[*band] > 0) {
            totals[band] += timestamp.saturating_sub(cursor);
        }
        while index < events.len() && events[index].0 == timestamp {
            active[events[index].1] += events[index].2;
            index += 1;
        }
        cursor = timestamp;
    }
    json!({"lowMs":totals[0],"mediumMs":totals[1],"highMs":totals[2]})
}

fn distinct_episode_count(intervals: &[MetricInterval]) -> usize {
    intervals
        .iter()
        .map(|row| row.episode_key.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len()
}

#[derive(Default)]
struct SourceCoverage {
    observed_ms: i64,
    unknown_ms: i64,
    os_observed_ms: i64,
    browser_observed_ms: i64,
    joint_observed_ms: i64,
    browser_spans: Vec<(i64, i64)>,
}

fn agent_source_coverage(db: &Connection, from: i64, to: i64) -> Result<SourceCoverage> {
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM bounded_agent_state_events WHERE bounded_end_ts>? AND event_ts<?",
        params![from, to],
        |row| row.get(0),
    )?;
    if count > MAX_INTEGRATION_INTERVALS {
        bail!("agent source evidence contains {count} intervals; hard ceiling is {MAX_INTEGRATION_INTERVALS}");
    }
    let mut statement = db.prepare(
        "SELECT MAX(event_ts,?),MIN(bounded_end_ts,?) FROM bounded_agent_state_events WHERE bounded_end_ts>? AND event_ts<? ORDER BY event_ts,id",
    )?;
    let spans = statement
        .query_map(params![from, to, from, to], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(SourceCoverage {
        observed_ms: union_duration(spans.into_iter()),
        ..Default::default()
    })
}

fn browser_source_coverage(db: &Connection, from: i64, to: i64) -> Result<SourceCoverage> {
    let count: i64 = db.query_row(
        "SELECT (SELECT COUNT(*) FROM browser_attention_intervals WHERE end_ts>?1 AND start_ts<?2)+(SELECT COUNT(*) FROM browser_current_states WHERE expires_ts>?1 AND state_started_ts<?2)",
        params![from,to],
        |row| row.get(0),
    )?;
    if count > MAX_INTEGRATION_INTERVALS {
        bail!("browser source evidence contains {count} intervals; hard ceiling is {MAX_INTEGRATION_INTERVALS}");
    }
    let mut browser = Vec::with_capacity(count as usize);
    let mut os_observed = Vec::new();
    let mut unknown = Vec::new();
    let mut statement = db.prepare(
        "SELECT MAX(start_ts,?),MIN(end_ts,?),os_focus_state FROM browser_attention_intervals WHERE end_ts>? AND start_ts<? ORDER BY start_ts,id",
    )?;
    for row in statement.query_map(params![from, to, from, to], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (start, end, state) = row?;
        browser.push((start, end));
        if state == "unknown" {
            unknown.push((start, end));
        } else {
            os_observed.push((start, end));
        }
    }
    drop(statement);
    let mut states = db.prepare(
        "SELECT MAX(s.state_started_ts,?),MIN(s.expires_ts,s.last_observed_ts+?,?),c.browser_family FROM browser_current_states s JOIN browser_clients c ON c.id=s.client_id WHERE s.expires_ts>? AND s.state_started_ts<? ORDER BY s.state_started_ts,s.client_id,s.window_id,s.tab_id",
    )?;
    for row in states.query_map(
        params![from, crate::browser_api::LEASE_MS, to, from, to],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    )? {
        let (start, end, family) = row?;
        if end <= start {
            continue;
        }
        browser.push((start, end));
        let (state, _) = crate::browser_api::reconcile_os(db, &family, start, end)?;
        if state == "unknown" {
            unknown.push((start, end));
        } else {
            os_observed.push((start, end));
        }
    }
    let browser_observed_ms = union_duration(browser.iter().copied());
    let os_observed_ms = union_duration(os_observed.iter().copied());
    let unknown_ms = union_duration(unknown.into_iter());
    Ok(SourceCoverage {
        observed_ms: browser_observed_ms,
        unknown_ms,
        os_observed_ms,
        browser_observed_ms,
        joint_observed_ms: span_intersection_duration(&browser, &os_observed),
        browser_spans: browser,
    })
}

fn integration_source_coverage(
    db: &Connection,
    metric: &str,
    from: i64,
    to: i64,
) -> Result<SourceCoverage> {
    if metric.starts_with("agent_busy") {
        agent_source_coverage(db, from, to)
    } else {
        browser_source_coverage(db, from, to)
    }
}

#[allow(clippy::too_many_arguments)]
fn aggregate_meta(
    db: &Connection,
    requested_from: i64,
    requested_to: i64,
    from: i64,
    to: i64,
    metric: &str,
    metric_duration_ms: i64,
    observed_ms: i64,
    unknown_ms: i64,
    confidence: Value,
    sample_count: usize,
    episode_count: usize,
    filters: Value,
    complete_days_only: bool,
    excluded_partial_days: i64,
    os_observed_ms: i64,
    browser_observed_ms: i64,
    joint_observed_ms: i64,
) -> Result<Value> {
    let (classifier_version, rules_revision): (i64, i64) = db.query_row(
        "SELECT active_labeler_version,rules_revision FROM context_runtime_state WHERE id=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let window_ms = to.saturating_sub(from);
    Ok(json!({
        "schemaVersion":1,"requestedFromTs":requested_from,"requestedToTs":requested_to,
        "fromTs":from,"toTs":to,"metric":metric,"metricDurationMs":metric_duration_ms,
        "observedMs":observed_ms,"unknownMs":unknown_ms,
        "monitoringCoverage":if window_ms==0{0.0}else{observed_ms as f64/window_ms as f64},
        "confidence":confidence,"sampleCount":sample_count,"episodeCount":episode_count,
        "classifierVersion":classifier_version,"rulesRevision":rules_revision,"filters":filters,
        "completeness":{"truncated":false,"hardIntervalCeiling":MAX_INTEGRATION_INTERVALS,"completeDaysOnly":complete_days_only,"excludedPartialBoundaryDays":excluded_partial_days},
        "coverage":{"windowMs":window_ms,"osObservedMs":os_observed_ms,"browserObservedMs":browser_observed_ms,"jointObservedMs":joint_observed_ms}
    }))
}

fn integration_metric_summary(
    db: &Connection,
    query: &RangeQuery,
    from: i64,
    to: i64,
    metric: &str,
) -> Result<Value> {
    let canonical = canonical_integration_metric(metric)?;
    let intervals = integration_intervals(db, query, from, to, &canonical)?;
    let metric_duration_ms = union_duration(intervals.iter().map(|row| (row.start, row.end)));
    let mut grouped: BTreeMap<String, Vec<MetricInterval>> = BTreeMap::new();
    let mut group_labels = BTreeMap::new();
    for interval in intervals.iter().cloned() {
        group_labels.insert(interval.key.clone(), interval.label.clone());
        grouped
            .entry(interval.key.clone())
            .or_default()
            .push(interval);
    }
    if grouped.len() > MAX_RESULT_ROWS as usize {
        bail!("summary contains more than {MAX_RESULT_ROWS} distinct groups; narrow the range or filters");
    }
    let mut groups = grouped
        .into_iter()
        .map(|(key, rows)| {
            let duration = union_duration(rows.iter().map(|row| (row.start, row.end)));
            let label = group_labels.get(&key).cloned().unwrap_or_else(|| key.clone());
            json!({"key":key,"label":label,"durationMs":duration,"episodeCount":distinct_episode_count(&rows),"confidence":confidence_distribution(&rows)})
        })
        .collect::<Vec<_>>();
    groups.sort_by(|left, right| {
        right["durationMs"]
            .as_i64()
            .cmp(&left["durationMs"].as_i64())
            .then_with(|| left["key"].as_str().cmp(&right["key"].as_str()))
    });
    let group = if canonical.starts_with("agent_busy") {
        query.group.as_deref().unwrap_or("agentSession")
    } else {
        query.group.as_deref().unwrap_or("service")
    };
    let coverage = integration_source_coverage(db, &canonical, from, to)?;
    let confidence = confidence_distribution(&intervals);
    let episode_count = distinct_episode_count(&intervals);
    let meta = aggregate_meta(
        db,
        from,
        to,
        from,
        to,
        &canonical,
        metric_duration_ms,
        coverage.observed_ms,
        coverage.unknown_ms,
        confidence,
        intervals.len(),
        episode_count,
        json!({"metric":canonical,"group":group}),
        false,
        partial_boundary_count(from, to),
        coverage.os_observed_ms,
        coverage.browser_observed_ms,
        coverage.joint_observed_ms,
    )?;
    Ok(json!({
        "group":group,"groupLabels":group_labels,"metric":canonical,"groups":groups,"meta":meta,
        "metricDefinition":format!("Complete union of qualifying {canonical} intervals; concurrent spans do not double count elapsed time."),
        "boundarySemantics":"Intervals and leases are clipped to [from,to); source observation coverage is independent of metric duration.",
        "hardIntervalCeiling":MAX_INTEGRATION_INTERVALS,"truncated":false,"partialBoundaryDays":partial_boundary_count(from,to)
    }))
}

fn union_duration(ranges: impl Iterator<Item = (i64, i64)>) -> i64 {
    let mut ranges = ranges
        .filter(|(start, end)| end > start)
        .collect::<Vec<_>>();
    ranges.sort_unstable();
    let mut total = 0;
    let mut current: Option<(i64, i64)> = None;
    for (start, end) in ranges {
        match current.as_mut() {
            Some(active) if start <= active.1 => active.1 = active.1.max(end),
            Some(active) => {
                total += active.1 - active.0;
                *active = (start, end);
            }
            None => current = Some((start, end)),
        }
    }
    total + current.map_or(0, |active| active.1 - active.0)
}

pub(crate) fn activity_timeline(db: &mut Connection, query: &RangeQuery) -> Result<Value> {
    let (from, to) = range(query)?;
    let resolution_ms = parse_resolution(query.resolution.as_deref().unwrap_or("15m"))?;
    if !focus_metric(query.metric.as_deref()) {
        let requested_metric = query.metric.as_deref().unwrap_or_default();
        let metric = canonical_integration_metric(requested_metric)?;
        let intervals = integration_intervals(db, query, from, to, &metric)?;
        let bucket_count = ((to - from + resolution_ms - 1) / resolution_ms) as usize;
        if bucket_count > 2_000 {
            bail!("timeline exceeds 2000 buckets");
        }
        let group_labels = intervals
            .iter()
            .map(|interval| (interval.key.clone(), interval.label.clone()))
            .collect::<BTreeMap<_, _>>();
        if group_labels.len() > MAX_RESULT_ROWS as usize {
            bail!("timeline contains more than {MAX_RESULT_ROWS} distinct groups; narrow the range or filters");
        }
        let mut buckets = Vec::with_capacity(bucket_count);
        for index in 0..bucket_count {
            let start = from + index as i64 * resolution_ms;
            let end = (start + resolution_ms).min(to);
            let observed_ms = union_duration(
                intervals
                    .iter()
                    .map(|row| (row.start.max(start), row.end.min(end))),
            );
            let mut grouped: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
            for interval in &intervals {
                if interval.start < end && interval.end > start {
                    grouped
                        .entry(interval.key.clone())
                        .or_default()
                        .push((interval.start.max(start), interval.end.min(end)));
                }
            }
            let groups_ms = grouped
                .into_iter()
                .map(|(label, spans)| (label, union_duration(spans.into_iter())))
                .collect::<BTreeMap<_, _>>();
            buckets.push(json!({"startTs":start,"endTs":end,"observedMs":observed_ms,"unknownMs":0,"groupsMs":groups_ms}));
        }
        let metric_duration_ms = union_duration(intervals.iter().map(|row| (row.start, row.end)));
        let coverage = integration_source_coverage(db, &metric, from, to)?;
        let meta = aggregate_meta(
            db,
            from,
            to,
            from,
            to,
            &metric,
            metric_duration_ms,
            coverage.observed_ms,
            coverage.unknown_ms,
            confidence_distribution(&intervals),
            intervals.len(),
            distinct_episode_count(&intervals),
            json!({"metric":metric,"group":query.group.as_deref().unwrap_or(if metric.starts_with("agent_busy"){"agentSession"}else{"service"}),"resolutionMs":resolution_ms}),
            false,
            partial_boundary_count(from, to),
            coverage.os_observed_ms,
            coverage.browser_observed_ms,
            coverage.joint_observed_ms,
        )?;
        return Ok(json!({
            "resolutionMs":resolution_ms,"metric":metric,"buckets":buckets,"groupLabels":group_labels,"meta":meta,
            "metricDefinition":format!("Complete union of {metric} spans bucketed at the requested resolution."),
            "boundarySemantics":"Buckets are clipped to [from,to); stable group keys are independent of display labels.",
            "hardIntervalCeiling":MAX_INTEGRATION_INTERVALS,"truncated":false
        }));
    }
    let timeline = ActivityRepository::new(db).timeline(from, to, resolution_ms)?;
    let mut payload = serde_json::to_value(timeline)?;
    if payload["buckets"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|bucket| {
            bucket["groupsMs"]
                .as_object()
                .is_some_and(|groups| groups.len() > MAX_RESULT_ROWS as usize)
        })
    {
        bail!("timeline contains more than {MAX_RESULT_ROWS} distinct groups in a bucket; narrow the range or filters");
    }
    let base_meta = payload["meta"].clone();
    let observed_ms = base_meta["observedMs"].as_i64().unwrap_or(0);
    let unknown_ms = base_meta["unknownMs"].as_i64().unwrap_or(0);
    payload["meta"] = aggregate_meta(
        db,
        from,
        to,
        from,
        to,
        "focus_time",
        observed_ms,
        observed_ms,
        unknown_ms,
        base_meta["confidence"].clone(),
        base_meta["sampleCount"].as_u64().unwrap_or(0) as usize,
        base_meta["episodeCount"].as_u64().unwrap_or(0) as usize,
        json!({"metric":"focus_time","resolutionMs":resolution_ms}),
        false,
        partial_boundary_count(from, to),
        observed_ms,
        0,
        observed_ms,
    )?;
    payload["metric"] = json!("focus_time");
    payload["metricDefinition"] =
        json!("Overlap-reconciled focus duration bucketed at the requested resolution.");
    payload["partialBoundaryDays"] = json!(partial_boundary_count(from, to));
    Ok(payload)
}

fn parse_resolution(value: &str) -> Result<i64> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3_600_000)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else {
        (value, 1)
    };
    let resolution = number
        .parse::<i64>()
        .map_err(|_| anyhow!("invalid resolution"))?;
    let resolution = resolution
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("invalid resolution"))?;
    if !(1_000..=86_400_000).contains(&resolution) {
        bail!("resolution must be between 1 second and 1 day");
    }
    Ok(resolution)
}

pub(crate) fn activity_episodes(db: &mut Connection, query: &RangeQuery) -> Result<Value> {
    let (from, to) = range(query)?;
    if !focus_metric(query.metric.as_deref()) {
        let requested_metric = query.metric.as_deref().unwrap_or_default();
        let metric = canonical_integration_metric(requested_metric)?;
        let intervals = integration_intervals(db, query, from, to, &metric)?;
        let min_ms = query.min_seconds.unwrap_or(0).clamp(0, 86_400) * 1_000;
        let mut by_episode: BTreeMap<String, Vec<MetricInterval>> = BTreeMap::new();
        for interval in intervals.iter().cloned() {
            by_episode
                .entry(interval.episode_key.clone())
                .or_default()
                .push(interval);
        }
        let mut episodes = Vec::new();
        let mut filtered_intervals = Vec::new();
        for (episode_key, mut spans) in by_episode {
            spans.sort_by_key(|span| (span.raw_start, span.raw_end, span.key.clone()));
            let mut components: Vec<(i64, Vec<MetricInterval>)> = Vec::new();
            for span in spans {
                if let Some((component_end, component)) = components.last_mut() {
                    if span.raw_start <= *component_end {
                        *component_end = (*component_end).max(span.raw_end);
                        component.push(span);
                        continue;
                    }
                }
                components.push((span.raw_end, vec![span]));
            }
            let component_count = components.len();
            for (_, component) in components {
                let raw_component_start = component
                    .iter()
                    .map(|span| span.raw_start)
                    .min()
                    .unwrap_or(from);
                let start = component
                    .iter()
                    .map(|span| span.start)
                    .min()
                    .unwrap_or(from);
                let end = component.iter().map(|span| span.end).max().unwrap_or(start);
                let duration_ms =
                    union_duration(component.iter().map(|span| (span.start, span.end)));
                if duration_ms < min_ms {
                    continue;
                }
                let group_keys = component
                    .iter()
                    .map(|span| span.key.clone())
                    .collect::<std::collections::BTreeSet<_>>();
                let labels = component
                    .iter()
                    .map(|span| span.label.clone())
                    .collect::<std::collections::BTreeSet<_>>();
                let primary_key = component
                    .iter()
                    .max_by_key(|span| (span.raw_start, span.label.clone()))
                    .map(|span| span.key.clone())
                    .unwrap_or_else(|| "unknown".into());
                let label = component
                    .iter()
                    .filter(|span| span.key == primary_key)
                    .max_by_key(|span| (span.raw_start, span.label.clone()))
                    .map(|span| span.label.clone())
                    .unwrap_or_else(|| "unknown".into());
                if episodes.len() >= MAX_RESULT_ROWS as usize {
                    bail!("episodes contain more than {MAX_RESULT_ROWS} results; increase minSeconds or narrow the range");
                }
                filtered_intervals.extend(component.iter().cloned());
                episodes.push(json!({
                    "episodeId":if component_count>1{format!("{episode_key}:c:{raw_component_start}")}else{episode_key.clone()},
                    "persistedEpisodeKey":episode_key,"groupKey":primary_key,"groupKeys":group_keys,"label":label,"labels":labels,
                    "startTs":start,"endTs":end,"durationMs":duration_ms,"confidence":confidence_distribution(&component),"segmentCount":component.len(),
                    "crossesLocalMidnight":crate::context::local_date(start)!=crate::context::local_date(end.saturating_sub(1))
                }));
            }
        }
        episodes.sort_by_key(|episode| episode["startTs"].as_i64().unwrap_or(0));
        let group_labels = filtered_intervals
            .iter()
            .map(|interval| (interval.key.clone(), interval.label.clone()))
            .collect::<BTreeMap<_, _>>();
        if group_labels.len() > MAX_RESULT_ROWS as usize {
            bail!("episodes contain more than {MAX_RESULT_ROWS} distinct groups; narrow the range or filters");
        }
        let filtered_keys = episodes
            .iter()
            .filter_map(|episode| episode["persistedEpisodeKey"].as_str())
            .collect::<std::collections::HashSet<_>>()
            .len();
        let metric_duration_ms =
            union_duration(filtered_intervals.iter().map(|row| (row.start, row.end)));
        let coverage = integration_source_coverage(db, &metric, from, to)?;
        let meta = aggregate_meta(
            db,
            from,
            to,
            from,
            to,
            &metric,
            metric_duration_ms,
            coverage.observed_ms,
            coverage.unknown_ms,
            confidence_distribution(&filtered_intervals),
            filtered_intervals.len(),
            filtered_keys,
            json!({"metric":metric,"group":query.group.as_deref().unwrap_or(if metric.starts_with("agent_busy"){"agentSession"}else{"service"}),"kind":query.kind.as_deref().unwrap_or("metric"),"minSeconds":min_ms/1000}),
            false,
            partial_boundary_count(from, to),
            coverage.os_observed_ms,
            coverage.browser_observed_ms,
            coverage.joint_observed_ms,
        )?;
        return Ok(
            json!({"fromTs":from,"toTs":to,"kind":query.kind.as_deref().unwrap_or("metric"),"metric":metric,"groupLabels":group_labels,"episodes":episodes,"episodeCount":filtered_keys,"meta":meta,"hardIntervalCeiling":MAX_INTEGRATION_INTERVALS,"truncated":false,"boundarySemantics":"Persisted browser episode keys remain continuous across labels and local midnight; components use running maximum persisted bounds."}),
        );
    }
    let kind = query.kind.as_deref().unwrap_or("entertainment");
    let episodes =
        ActivityRepository::new(db).episodes(from, to, kind, query.min_seconds.unwrap_or(0))?;
    if episodes.len() > MAX_RESULT_ROWS as usize {
        bail!("episodes contain more than {MAX_RESULT_ROWS} results; increase minSeconds or narrow the range");
    }
    let summary = ActivityRepository::new(db).summary(from, to, ActivityGroup::Activity)?;
    let metric_duration_ms = episodes
        .iter()
        .map(|episode| episode.duration_ms)
        .sum::<i64>();
    let mut episode_confidence = [0_i64; 3];
    for episode in &episodes {
        let band = if episode.confidence < 0.5 {
            0
        } else if episode.confidence < 0.8 {
            1
        } else {
            2
        };
        episode_confidence[band] += episode.duration_ms;
    }
    let confidence = json!({"lowMs":episode_confidence[0],"mediumMs":episode_confidence[1],"highMs":episode_confidence[2]});
    let meta = aggregate_meta(
        db,
        from,
        to,
        from,
        to,
        "focus_time",
        metric_duration_ms,
        summary.meta.observed_ms,
        summary.meta.unknown_ms,
        confidence,
        episodes.iter().map(|episode| episode.segment_count).sum(),
        episodes.len(),
        json!({"metric":"focus_time","kind":kind,"minSeconds":query.min_seconds.unwrap_or(0)}),
        false,
        partial_boundary_count(from, to),
        summary.meta.observed_ms,
        0,
        summary.meta.observed_ms,
    )?;
    Ok(
        json!({"fromTs":from,"toTs":to,"kind":kind,"metric":"focus_time","episodes":episodes,"episodeCount":episodes.len(),"meta":meta,"boundarySemantics":"Episodes retain persisted identity across local midnight and are clipped to the requested range.","partialBoundaryDays":partial_boundary_count(from,to),"hardIntervalCeiling":MAX_INTEGRATION_INTERVALS,"truncated":false}),
    )
}

#[derive(Clone)]
struct OsActivitySpan {
    start: i64,
    end: i64,
    activity: String,
    project_id: Option<i64>,
    confidence: f64,
}

fn merged_ranges(mut ranges: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (start, end) in ranges.into_iter().filter(|(start, end)| end > start) {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    merged
}

fn span_intersection_duration(left: &[(i64, i64)], right: &[(i64, i64)]) -> i64 {
    let left = merged_ranges(left.to_vec());
    let right = merged_ranges(right.to_vec());
    let (mut i, mut j, mut total) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        let start = left[i].0.max(right[j].0);
        let end = left[i].1.min(right[j].1);
        total += (end - start).max(0);
        if left[i].1 <= right[j].1 {
            i += 1
        } else {
            j += 1
        }
    }
    total
}

pub(crate) fn project_breaks(db: &mut Connection, query: &RangeQuery) -> Result<Value> {
    let (requested_from, requested_to) = range(query)?;
    let requested_metric = query
        .metric
        .as_deref()
        .unwrap_or("verified_focused_playback");
    let metric = canonical_metric(requested_metric)?;
    if !matches!(
        metric.as_str(),
        "focused_playback_seconds" | "verified_focused_playback_seconds"
    ) {
        bail!("project breaks require focused_playback or verified_focused_playback")
    }
    let max_follow_gap_seconds = query.max_follow_gap_seconds.unwrap_or(900);
    if !(0..=3_600).contains(&max_follow_gap_seconds) {
        bail!("maxFollowGapSeconds must be between 0 and 3600")
    }
    let include_partial = query.include_partial_days.unwrap_or(false);
    let excluded = if include_partial {
        0
    } else {
        partial_boundary_count(requested_from, requested_to)
    };
    let (from, to) = if include_partial {
        (requested_from, requested_to)
    } else {
        complete_day_range(requested_from, requested_to)
    };

    let segments = if from < to {
        let export = ActivityRepository::new(db).export_normalized(from, to)?;
        export["activityIntervals"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|row| OsActivitySpan {
                start: row["startTs"].as_i64().unwrap_or(from),
                end: row["endTs"].as_i64().unwrap_or(from),
                activity: row["activity"].as_str().unwrap_or("unknown").to_string(),
                project_id: row["projectEntityId"].as_i64(),
                confidence: row["confidence"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0),
            })
            .filter(|row| row.end > row.start)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let browser = if from < to {
        integration_intervals(db, query, from, to, &metric)?
    } else {
        Vec::new()
    };
    let browser_coverage = if from < to {
        browser_source_coverage(db, from, to)?
    } else {
        SourceCoverage::default()
    };
    let browser_observed = browser_coverage.browser_spans.clone();
    let os_observed = segments
        .iter()
        .map(|row| (row.start, row.end))
        .collect::<Vec<_>>();
    let unknown_os = segments
        .iter()
        .filter(|row| row.activity == "unknown")
        .map(|row| (row.start, row.end))
        .collect::<Vec<_>>();
    let os_observed_ms = union_duration(os_observed.iter().copied());
    let browser_observed_ms = union_duration(browser_observed.iter().copied());
    let joint_observed_ms = span_intersection_duration(&os_observed, &browser_observed);
    let unknown_ms = span_intersection_duration(&unknown_os, &browser_observed);

    let work = segments
        .iter()
        .filter(|row| row.activity == "work")
        .cloned()
        .collect::<Vec<_>>();
    let entertainment = segments
        .iter()
        .filter(|row| row.activity == "entertainment")
        .cloned()
        .collect::<Vec<_>>();
    let mut exposure: BTreeMap<i64, Vec<(i64, i64)>> = BTreeMap::new();
    let mut active_days: BTreeMap<i64, std::collections::BTreeSet<i32>> = BTreeMap::new();
    for row in &work {
        let Some(project) = row.project_id else {
            continue;
        };
        exposure
            .entry(project)
            .or_default()
            .push((row.start, row.end));
        let mut cursor = row.start;
        while cursor < row.end {
            active_days
                .entry(project)
                .or_default()
                .insert(crate::context::local_date(cursor).to_julian_day());
            cursor = crate::context::next_local_midnight_ms(cursor).min(row.end);
        }
    }

    let mut intersections = Vec::new();
    for browser_row in &browser {
        for os_row in entertainment
            .iter()
            .filter(|os| os.start < browser_row.end && os.end > browser_row.start)
        {
            let start = browser_row.start.max(os_row.start);
            let end = browser_row.end.min(os_row.end);
            if end > start {
                let mut row = browser_row.clone();
                row.start = start;
                row.end = end;
                row.confidence = row.confidence.min(os_row.confidence);
                intersections.push(row);
            }
        }
    }
    let mut by_episode: BTreeMap<String, Vec<MetricInterval>> = BTreeMap::new();
    for row in intersections {
        by_episode
            .entry(row.episode_key.clone())
            .or_default()
            .push(row);
    }
    let mut attributed: BTreeMap<i64, Vec<MetricInterval>> = BTreeMap::new();
    for (_, mut rows) in by_episode {
        rows.sort_by_key(|row| (row.raw_start, row.raw_end));
        let mut components: Vec<(i64, Vec<MetricInterval>)> = Vec::new();
        for row in rows {
            if let Some((component_end, component)) = components.last_mut() {
                if row.raw_start <= *component_end {
                    *component_end = (*component_end).max(row.raw_end);
                    component.push(row);
                    continue;
                }
            }
            components.push((row.raw_end, vec![row]));
        }
        for (_, mut component) in components {
            let component_start = component
                .iter()
                .map(|row| row.raw_start)
                .min()
                .unwrap_or(from);
            let preceding = work
                .iter()
                .filter(|row| row.end <= component_start)
                .max_by_key(|row| row.end);
            let Some(preceding) = preceding else { continue };
            if component_start.saturating_sub(preceding.end) > max_follow_gap_seconds * 1000 {
                continue;
            }
            let Some(project) = preceding.project_id else {
                continue;
            };
            for row in &mut component {
                row.confidence = row.confidence.min(preceding.confidence);
            }
            attributed.entry(project).or_default().extend(component);
        }
    }

    let mut names = BTreeMap::new();
    let mut statement =
        db.prepare("SELECT id,display_name FROM context_entities WHERE kind='project'")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, name) = row?;
        names.insert(id, name);
    }
    if exposure.len() > MAX_RESULT_ROWS as usize {
        bail!("project breaks contain more than {MAX_RESULT_ROWS} projects; narrow the range");
    }
    let mut projects = exposure.iter().map(|(project, spans)| {
        let exposure_ms = union_duration(spans.iter().copied());
        let rows = attributed.get(project).cloned().unwrap_or_default();
        let break_ms = union_duration(rows.iter().map(|row|(row.start,row.end)));
        let episode_count = distinct_episode_count(&rows);
        let days = active_days.get(project).map_or(0,|days|days.len());
        let key = format!("project:{project}");
        let label = names.get(project).cloned().unwrap_or_else(||format!("Project {project}"));
        json!({"key":key,"projectEntityId":project,"label":label,"projectName":label,"exposureMs":exposure_ms,"exposureHours":exposure_ms as f64/3_600_000.0,"activeCompleteDays":days,"activeProjectDays":days,"breakMs":break_ms,"episodeCount":episode_count,"breakEpisodes":episode_count,"breakSecondsPerExposureHour":if exposure_ms==0{0.0}else{break_ms as f64*3600.0/exposure_ms as f64},"breakSecondsPerActiveDay":if days==0{0.0}else{break_ms as f64/1000.0/days as f64},"confidence":confidence_distribution(&rows)})
    }).collect::<Vec<_>>();
    projects.sort_by(|left, right| {
        right["breakSecondsPerExposureHour"]
            .as_f64()
            .unwrap_or(0.0)
            .total_cmp(&left["breakSecondsPerExposureHour"].as_f64().unwrap_or(0.0))
            .then_with(|| left["key"].as_str().cmp(&right["key"].as_str()))
    });
    let all_attributed = attributed.into_values().flatten().collect::<Vec<_>>();
    let metric_duration_ms = union_duration(all_attributed.iter().map(|row| (row.start, row.end)));
    let episode_count = distinct_episode_count(&all_attributed);
    let meta = aggregate_meta(
        db,
        requested_from,
        requested_to,
        from,
        to,
        &metric,
        metric_duration_ms,
        joint_observed_ms,
        unknown_ms,
        confidence_distribution(&all_attributed),
        browser.len(),
        episode_count,
        json!({"metric":metric,"activity":"entertainment","maxFollowGapSeconds":max_follow_gap_seconds,"includePartialDays":include_partial}),
        !include_partial,
        excluded,
        os_observed_ms,
        browser_observed_ms,
        joint_observed_ms,
    )?;
    Ok(
        json!({"fromTs":from,"toTs":to,"requestedFromTs":requested_from,"requestedToTs":requested_to,"metric":metric,"normalization":"verified entertainment playback following project work / unioned project work exposure","projects":projects,"meta":meta,"completeDaysOnly":!include_partial,"excludedPartialBoundaryDays":excluded,"hardIntervalCeiling":MAX_INTEGRATION_INTERVALS,"truncated":false,"boundarySemantics":"Partial local dates are excluded unless requested; no complete day returns an empty effective range."}),
    )
}

fn partial_boundary_count(from: i64, to: i64) -> i64 {
    let mut dates = std::collections::BTreeSet::new();
    if from != crate::context::local_midnight_ms(from) {
        dates.insert(crate::context::local_date(from).to_julian_day());
    }
    if to != crate::context::local_midnight_ms(to) {
        dates.insert(crate::context::local_date(to.saturating_sub(1)).to_julian_day());
    }
    dates.len() as i64
}

fn complete_day_range(from: i64, to: i64) -> (i64, i64) {
    let from_midnight = crate::context::local_midnight_ms(from);
    let start = if from == from_midnight {
        from
    } else {
        crate::context::local_midnight_ms(from_midnight.saturating_add(36 * 3_600_000))
    };
    let end = crate::context::local_midnight_ms(to);
    (start, end.max(start))
}

pub(crate) fn unknown_context(db: &Connection, query: &RangeQuery) -> Result<Value> {
    let (from, to) = range(query)?;
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let active_version: i64 = db.query_row(
        "SELECT active_labeler_version FROM context_runtime_state WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    let mut stmt = db.prepare(
        "SELECT f.id,f.start_ts,f.end_ts,f.focused_app,f.focused_title,f.focused_pid,hex(f.context_key), \
                COALESCE(MAX(CASE WHEN l.dimension='activity.primary' THEN l.confidence END),0), \
                MAX(CASE WHEN l.dimension='activity.primary' THEN l.value END), \
                MAX(0,MIN(f.end_ts,?)-MAX(f.start_ts,?)) duration_ms \
         FROM focus_context_intervals f LEFT JOIN focus_interval_labels l ON l.context_interval_id=f.id AND l.labeler_version=? \
         WHERE f.end_ts>? AND f.start_ts<? GROUP BY f.id \
         HAVING MAX(CASE WHEN l.dimension='activity.primary' THEN l.value END) IS NULL \
             OR MAX(CASE WHEN l.dimension='activity.primary' THEN l.value END)='unknown' \
             OR COALESCE(MAX(CASE WHEN l.dimension='activity.primary' THEN l.confidence END),0)<0.8 \
         ORDER BY duration_ms DESC,f.id LIMIT ?",
    )?;
    let rows = stmt
        .query_map(params![to, from, active_version, from, to, limit], |row| {
            let confidence = row.get::<_, f64>(7)?.clamp(0.0, 1.0);
            Ok(json!({"id":row.get::<_,i64>(0)?,"contextIntervalId":row.get::<_,i64>(0)?,"startTs":row.get::<_,i64>(1)?,"endTs":row.get::<_,i64>(2)?,"focusedApp":row.get::<_,String>(3)?,"focusedTitle":row.get::<_,Option<String>>(4)?,"focusedPid":row.get::<_,Option<i64>>(5)?,"contextKey":row.get::<_,String>(6)?,"confidence":confidence,"activity":row.get::<_,Option<String>>(8)?,"durationMs":row.get::<_,i64>(9)?,"reason":if confidence<0.8{"low_confidence"}else{"unknown"}}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(
        json!({"fromTs":from,"toTs":to,"unknown":rows,"count":rows.len(),"activeLabelerVersion":active_version,"limit":limit}),
    )
}

pub(crate) fn entities(db: &mut Connection, kind: Option<&str>) -> Result<Value> {
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM context_entities WHERE (?1 IS NULL OR kind=?1)",
        params![kind],
        |row| row.get(0),
    )?;
    if count > MAX_RESULT_ROWS {
        bail!("entity list contains {count} rows; output limit is {MAX_RESULT_ROWS}");
    }
    let entities = ActivityRepository::new(db).entities(kind)?;
    Ok(
        json!({"entities":entities,"count":entities.len(),"limit":MAX_RESULT_ROWS,"truncated":false}),
    )
}

pub(crate) fn rules(db: &mut Connection) -> Result<Value> {
    let count: i64 = db.query_row("SELECT COUNT(*) FROM label_rules", [], |row| row.get(0))?;
    if count > MAX_RESULT_ROWS {
        bail!("rule list contains {count} rows; output limit is {MAX_RESULT_ROWS}");
    }
    let rules = ActivityRepository::new(db).rules()?;
    let revision: i64 = db.query_row(
        "SELECT rules_revision FROM context_runtime_state WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    Ok(
        json!({"rulesRevision":revision,"rules":rules,"count":rules.len(),"limit":MAX_RESULT_ROWS,"truncated":false}),
    )
}

fn validate_production_rule(input: &RuleInput) -> Result<()> {
    activity::validate_rule(input)?;
    for condition in &input.conditions {
        if condition.fact.starts_with("browser.")
            || condition.fact == "account.entity"
            || condition.operator == "entityRelationship"
        {
            bail!("fact/operator is not available in the production focus labeler");
        }
    }
    Ok(())
}

pub(crate) fn preview_rule(db: &Connection, input: &RuleInput) -> Result<Value> {
    validate_production_rule(input)?;
    let mut stmt = db.prepare(
        "SELECT id,focused_app,focused_title,tmux_session,tmux_window,foreground_executable,repository_entity_id,project_entity_id,agent_type,native_agent_session_id,agent_state,start_ts,end_ts \
         FROM focus_context_intervals ORDER BY start_ts DESC LIMIT ?",
    )?;
    let rows = stmt
        .query_map(params![MAX_PREVIEW_ROWS + 1], |row| {
            let mut facts = HashMap::<String, Value>::new();
            for (name, value) in [
                ("app.id", row.get::<_, Option<String>>(1)?),
                ("window.title", row.get::<_, Option<String>>(2)?),
                ("terminal.session", row.get::<_, Option<String>>(3)?),
                ("terminal.window", row.get::<_, Option<String>>(4)?),
                ("terminal.command", row.get::<_, Option<String>>(5)?),
                ("agent.type", row.get::<_, Option<String>>(8)?),
                ("agent.session", row.get::<_, Option<String>>(9)?),
                ("agent.state", row.get::<_, Option<String>>(10)?),
            ] {
                if let Some(value) = value {
                    facts.insert(name.to_string(), json!(value));
                }
            }
            if let Some(value) = row.get::<_, Option<i64>>(6)? {
                facts.insert("repository.entity".into(), json!(value.to_string()));
            }
            if let Some(value) = row.get::<_, Option<i64>>(7)? {
                facts.insert("project.entity".into(), json!(value.to_string()));
            }
            Ok((
                row.get::<_, i64>(0)?,
                facts,
                row.get::<_, i64>(11)?,
                row.get::<_, i64>(12)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let truncated = rows.len() as i64 > MAX_PREVIEW_ROWS;
    let mut affected = 0;
    let mut duration_ms = 0;
    let mut examples = Vec::new();
    for (id, facts, start, end) in rows.into_iter().take(MAX_PREVIEW_ROWS as usize) {
        let matched = input.conditions.iter().all(|condition| {
            activity::condition_matches_value(condition, facts.get(&condition.fact))
                .unwrap_or(false)
        });
        if matched {
            affected += 1;
            duration_ms += (end - start).max(0);
            if examples.len() < 20 {
                examples.push(id);
            }
        }
    }
    Ok(
        json!({"affectedIntervals":affected,"affectedMs":duration_ms,"affectedSeconds":duration_ms as f64/1000.0,"exampleContextIntervalIds":examples,"scannedLimit":MAX_PREVIEW_ROWS,"truncated":truncated,"warning":"Source observations remain unchanged."}),
    )
}

pub(crate) fn create_rule(db: &mut Connection, input: RuleInput) -> Result<Value> {
    validate_production_rule(&input)?;
    let rule = ActivityRepository::new(db).save_rule(&input, now_ms())?;
    let bounds: (Option<i64>, Option<i64>) = db.query_row(
        "SELECT MIN(start_ts),MAX(end_ts) FROM focus_context_intervals",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let relabel_job_id = if let (Some(from), Some(to)) = bounds {
        Some(ActivityRepository::new(db).enqueue_relabel(
            from,
            to.max(from + 1),
            json!({"ruleId":rule.id}),
            now_ms(),
        )?)
    } else {
        None
    };
    Ok(
        json!({"id":rule.id,"rule":rule,"relabelJobId":relabel_job_id,"undo":{"method":"DELETE","url":format!("/api/context/rules/{}",rule.id)}}),
    )
}

pub(crate) fn delete_rule(db: &mut Connection, id: i64) -> Result<Value> {
    let changed = db.execute(
        "DELETE FROM label_rules WHERE id=? AND origin='user'",
        params![id],
    )?;
    if changed == 0 {
        bail!("user rule not found")
    }
    let bounds: (Option<i64>, Option<i64>) = db.query_row(
        "SELECT MIN(start_ts),MAX(end_ts) FROM focus_context_intervals",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let job = if let (Some(from), Some(to)) = bounds {
        Some(ActivityRepository::new(db).enqueue_relabel(
            from,
            to.max(from + 1),
            json!({"deletedRuleId":id}),
            now_ms(),
        )?)
    } else {
        None
    };
    Ok(json!({"deleted":true,"id":id,"relabelJobId":job}))
}

#[derive(Deserialize)]
pub(crate) struct ManualLabelInput {
    #[serde(rename = "contextIntervalId")]
    pub(crate) context_interval_id: Option<i64>,
    #[serde(rename = "startTs")]
    pub(crate) start_ts: Option<i64>,
    #[serde(rename = "endTs")]
    pub(crate) end_ts: Option<i64>,
    pub(crate) dimension: String,
    pub(crate) value: Option<String>,
    #[serde(rename = "entityId")]
    pub(crate) entity_id: Option<i64>,
    pub(crate) note: Option<String>,
    pub(crate) scope: Option<String>,
}

pub(crate) fn create_manual_label(db: &mut Connection, input: ManualLabelInput) -> Result<Value> {
    if input.scope.as_deref().unwrap_or("interval") != "interval" {
        bail!("only interval correction scope is supported; create a reviewed rule for recurring contexts");
    }
    let (start_ts, end_ts) = if let Some(id) = input.context_interval_id {
        let actual: (i64, i64) = db
            .query_row(
                "SELECT start_ts,end_ts FROM focus_context_intervals WHERE id=?",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("context interval not found"))?;
        if input.start_ts.is_some_and(|value| value != actual.0)
            || input.end_ts.is_some_and(|value| value != actual.1)
        {
            bail!("client interval metadata does not match the stored context");
        }
        actual
    } else {
        (
            input.start_ts.ok_or_else(|| anyhow!("startTs required"))?,
            input.end_ts.ok_or_else(|| anyhow!("endTs required"))?,
        )
    };
    let core = CoreManualLabel {
        start_ts,
        end_ts,
        dimension: input.dimension,
        value: input.value,
        entity_id: input.entity_id,
        note: input.note,
    };
    let id = ActivityRepository::new(db).add_manual_label(&core, now_ms())?;
    let job = ActivityRepository::new(db).enqueue_relabel(
        start_ts,
        end_ts,
        json!({"manualLabelId":id}),
        now_ms(),
    )?;
    Ok(
        json!({"id":id,"relabelJobId":job,"scope":"interval","undo":{"method":"DELETE","url":format!("/api/context/manual-labels/{id}")}}),
    )
}

pub(crate) fn manual_label_history(db: &Connection, limit: i64) -> Result<Value> {
    let limit = limit.clamp(1, 500);
    let mut stmt = db.prepare("SELECT m.id,m.start_ts,m.end_ts,m.dimension,m.value,m.entity_id,e.display_name,m.note,m.created_ts FROM manual_activity_labels m LEFT JOIN context_entities e ON e.id=m.entity_id ORDER BY m.created_ts DESC,m.id DESC LIMIT ?")?;
    let rows = stmt.query_map(params![limit], |row| Ok(json!({"id":row.get::<_,i64>(0)?,"startTs":row.get::<_,i64>(1)?,"endTs":row.get::<_,i64>(2)?,"dimension":row.get::<_,String>(3)?,"value":row.get::<_,Option<String>>(4)?,"entityId":row.get::<_,Option<i64>>(5)?,"entityName":row.get::<_,Option<String>>(6)?,"note":row.get::<_,Option<String>>(7)?,"createdTs":row.get::<_,i64>(8)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"labels":rows,"count":rows.len(),"limit":limit}))
}

pub(crate) fn delete_manual_label(db: &mut Connection, id: i64) -> Result<Value> {
    let bounds: Option<(i64, i64)> = db
        .query_row(
            "SELECT start_ts,end_ts FROM manual_activity_labels WHERE id=?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((from, to)) = bounds else {
        bail!("manual label not found")
    };
    ActivityRepository::new(db).delete_manual_label(id)?;
    let job = ActivityRepository::new(db).enqueue_relabel(
        from,
        to,
        json!({"undoManualLabelId":id}),
        now_ms(),
    )?;
    Ok(json!({"deleted":true,"id":id,"relabelJobId":job}))
}

#[derive(Deserialize)]
pub(crate) struct RelabelInput {
    #[serde(rename = "fromTs", alias = "from")]
    pub(crate) from_ts: i64,
    #[serde(rename = "toTs", alias = "to")]
    pub(crate) to_ts: i64,
    #[serde(rename = "entityId")]
    pub(crate) entity_id: Option<i64>,
}

pub(crate) fn create_relabel(db: &mut Connection, input: RelabelInput) -> Result<Value> {
    let scope = input
        .entity_id
        .map_or_else(|| json!({}), |id| json!({"entityId":id}));
    let id =
        ActivityRepository::new(db).enqueue_relabel(input.from_ts, input.to_ts, scope, now_ms())?;
    Ok(json!({"jobId":id,"status":"queued"}))
}

pub(crate) fn relabel_status(db: &Connection) -> Result<Value> {
    let mut stmt = db.prepare("SELECT id,rules_revision,labeler_version,range_start_ts,range_end_ts,cursor_start_ts,cursor_interval_id,processed_intervals,status,created_ts,started_ts,completed_ts,error FROM relabel_jobs ORDER BY id DESC LIMIT 100")?;
    let jobs = stmt.query_map([], |row| Ok(json!({"id":row.get::<_,i64>(0)?,"rulesRevision":row.get::<_,i64>(1)?,"labelerVersion":row.get::<_,i64>(2)?,"rangeStartTs":row.get::<_,i64>(3)?,"rangeEndTs":row.get::<_,i64>(4)?,"cursorStartTs":row.get::<_,Option<i64>>(5)?,"cursorIntervalId":row.get::<_,i64>(6)?,"processedIntervals":row.get::<_,i64>(7)?,"status":row.get::<_,String>(8)?,"createdTs":row.get::<_,i64>(9)?,"startedTs":row.get::<_,Option<i64>>(10)?,"completedTs":row.get::<_,Option<i64>>(11)?,"error":row.get::<_,Option<String>>(12)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"jobs":jobs,"count":jobs.len()}))
}

#[derive(Deserialize)]
pub(crate) struct AliasInput {
    #[serde(rename = "entityId")]
    pub(crate) entity_id: i64,
    #[serde(rename = "aliasKind")]
    pub(crate) alias_kind: String,
    #[serde(rename = "aliasValue")]
    pub(crate) alias_value: String,
    pub(crate) confidence: Option<f64>,
}

pub(crate) fn create_alias(db: &mut Connection, input: AliasInput) -> Result<Value> {
    let id = ActivityRepository::new(db).add_entity_alias(
        input.entity_id,
        &input.alias_kind,
        &input.alias_value,
        "user",
        input.confidence.unwrap_or(1.0),
        now_ms(),
    )?;
    Ok(json!({"id":id,"ok":true}))
}

#[derive(Deserialize)]
pub(crate) struct RelationInput {
    #[serde(rename = "parentEntityId")]
    pub(crate) parent: i64,
    #[serde(rename = "childEntityId")]
    pub(crate) child: i64,
    pub(crate) relation: String,
    pub(crate) confidence: Option<f64>,
}

pub(crate) fn create_relation(db: &mut Connection, input: RelationInput) -> Result<Value> {
    ActivityRepository::new(db).add_entity_relation(
        input.parent,
        input.child,
        &input.relation,
        "user",
        input.confidence.unwrap_or(1.0),
    )?;
    Ok(json!({"ok":true}))
}

#[derive(Deserialize)]
pub(crate) struct MergeInput {
    #[serde(rename = "sourceEntityId", alias = "sourceId")]
    pub(crate) source: i64,
    #[serde(rename = "targetEntityId", alias = "targetId")]
    pub(crate) target: i64,
}

pub(crate) fn merge_entity(db: &mut Connection, input: MergeInput) -> Result<Value> {
    ActivityRepository::new(db).merge_entities(input.target, input.source)?;
    Ok(json!({"ok":true,"sourceEntityId":input.source,"targetEntityId":input.target}))
}

pub(crate) fn split_entity(
    db: &mut Connection,
    source: i64,
    input: EntitySplitInput,
) -> Result<Value> {
    let id = ActivityRepository::new(db).split_entity(source, &input, now_ms())?;
    Ok(json!({"id":id,"sourceEntityId":source}))
}

pub(crate) fn archive_entity(db: &mut Connection, id: i64) -> Result<Value> {
    let archived = ActivityRepository::new(db).archive_entity(id)?;
    Ok(json!({"id":id,"archived":archived}))
}

pub(crate) fn delete_entity(db: &mut Connection, id: i64) -> Result<Value> {
    delete_context_data(
        db,
        DeletionInput {
            kind: "entity".into(),
            client_id: None,
            entity_id: Some(id),
            preview: false,
            rebuild_derived: false,
        },
    )
}

pub(crate) type PolicyInput = CorePolicyInput;

fn validate_policy(input: &PolicyInput) -> Result<()> {
    if input.schema_version != 1
        || input.name.trim().is_empty()
        || input.name.len() > 160
        || input.budget_seconds <= 0
    {
        bail!("invalid policy");
    }
    if ![
        "tab_active_seconds",
        "browser_focused_seconds",
        "document_visible_seconds",
        "media_playback_seconds",
        "audible_playback_seconds",
        "focused_playback_seconds",
        "verified_focused_playback_seconds",
        "background_playback_seconds",
    ]
    .contains(&input.metric.as_str())
    {
        bail!("unsupported policy metric");
    }
    if !["calendar-day", "rolling", "session"].contains(&input.window_kind.as_str()) {
        bail!("unsupported policy window");
    }
    if !["observe", "warn", "soft-block", "hard-block"].contains(&input.enforcement.as_str()) {
        bail!("unsupported enforcement");
    }
    let scope = input
        .scope
        .as_object()
        .ok_or_else(|| anyhow!("scope object required"))?;
    if scope
        .keys()
        .any(|key| !matches!(key.as_str(), "domains" | "excludedDomains"))
    {
        bail!("unknown policy scope field")
    }
    let domains = scope
        .get("domains")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("scope.domains required"))?;
    if domains.is_empty() || domains.len() > 100 {
        bail!("invalid policy domains")
    }
    for domain in domains.iter().chain(
        scope
            .get("excludedDomains")
            .and_then(Value::as_array)
            .into_iter()
            .flatten(),
    ) {
        let domain = domain
            .as_str()
            .ok_or_else(|| anyhow!("policy domain must be text"))?;
        if domain.is_empty() || domain.len() > 253 || domain.contains(['/', ':', '?', '#', '@']) {
            bail!("invalid policy domain")
        }
    }
    for (object, allowed) in [
        (
            &input.schedule,
            &["weekdays", "startMinute", "endMinute"][..],
        ),
        (
            &input.grace,
            &[
                "graceSeconds",
                "snoozeSeconds",
                "maxDailySnoozes",
                "sessionGapSeconds",
                "rollingSeconds",
            ][..],
        ),
    ] {
        let object = object
            .as_object()
            .ok_or_else(|| anyhow!("policy schedule/grace must be objects"))?;
        if object.keys().any(|key| !allowed.contains(&key.as_str())) {
            bail!("unknown policy schedule/grace field")
        }
    }
    Ok(())
}

pub(crate) fn create_policy(db: &mut Connection, input: PolicyInput) -> Result<Value> {
    validate_policy(&input)?;
    let id = ActivityRepository::new(db).save_policy(&input, now_ms())?;
    let revision: i64 = db.query_row(
        "SELECT revision FROM attention_policies WHERE id=?",
        params![id],
        |row| row.get(0),
    )?;
    Ok(json!({"id":id,"revision":revision}))
}

pub(crate) fn update_policy(db: &Connection, id: i64, input: PolicyInput) -> Result<Value> {
    validate_policy(&input)?;
    let revision: i64 = db.query_row(
        "SELECT COALESCE(MAX(revision),0)+1 FROM attention_policies",
        [],
        |row| row.get(0),
    )?;
    let changed = db.execute("UPDATE attention_policies SET name=?,enabled=?,scope_json=?,metric=?,window_kind=?,budget_seconds=?,warning_seconds=?,enforcement=?,schedule_json=?,grace_json=?,revision=?,updated_ts=? WHERE id=?", params![input.name.trim(),input.enabled as i64,input.scope.to_string(),input.metric,input.window_kind,input.budget_seconds,input.warning_seconds,input.enforcement,input.schedule.to_string(),input.grace.to_string(),revision,now_ms(),id])?;
    if changed == 0 {
        bail!("policy not found")
    }
    Ok(json!({"id":id,"revision":revision}))
}

pub(crate) fn policies(db: &Connection) -> Result<Value> {
    let count: i64 = db.query_row("SELECT COUNT(*) FROM attention_policies", [], |row| {
        row.get(0)
    })?;
    if count > MAX_RESULT_ROWS {
        bail!("policy list contains {count} rows; output limit is {MAX_RESULT_ROWS}");
    }
    let mut stmt = db.prepare("SELECT id,name,enabled,scope_json,metric,window_kind,budget_seconds,warning_seconds,enforcement,schedule_json,grace_json,revision FROM attention_policies ORDER BY id")?;
    let rows = stmt.query_map([], |row| Ok(json!({"id":row.get::<_,i64>(0)?,"name":row.get::<_,String>(1)?,"enabled":row.get::<_,i64>(2)?!=0,"scope":serde_json::from_str::<Value>(&row.get::<_,String>(3)?).unwrap_or(json!({})),"metric":row.get::<_,String>(4)?,"windowKind":row.get::<_,String>(5)?,"budgetSeconds":row.get::<_,i64>(6)?,"warningSeconds":row.get::<_,Option<i64>>(7)?,"enforcement":row.get::<_,String>(8)?,"schedule":serde_json::from_str::<Value>(&row.get::<_,String>(9)?).unwrap_or(json!({})),"grace":serde_json::from_str::<Value>(&row.get::<_,String>(10)?).unwrap_or(json!({})),"revision":row.get::<_,i64>(11)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"policies":rows,"count":rows.len(),"limit":MAX_RESULT_ROWS,"truncated":false}))
}

#[derive(Deserialize)]
pub(crate) struct OverrideInput {
    #[serde(rename = "policyId")]
    pub(crate) policy_id: i64,
    #[serde(rename = "clientId")]
    pub(crate) client_id: Option<i64>,
    pub(crate) action: String,
    #[serde(rename = "durationSeconds")]
    pub(crate) duration_seconds: i64,
}

pub(crate) fn create_override(db: &mut Connection, input: OverrideInput) -> Result<Value> {
    let now = now_ms();
    let id = match input.action.as_str() {
        "snooze" => ActivityRepository::new(db).snooze_policy(
            input.policy_id,
            input.client_id,
            None,
            now,
        )?,
        "continue" => ActivityRepository::new(db).temporarily_disable_policy(
            input.policy_id,
            input.client_id,
            input.duration_seconds,
            now,
        )?,
        "override" => ActivityRepository::new(db).override_policy(
            input.policy_id,
            input.client_id,
            None,
            input.duration_seconds,
            now,
        )?,
        _ => bail!("invalid override action"),
    };
    Ok(json!({"id":id,"action":input.action}))
}

pub(crate) fn interventions(db: &Connection, query: &RangeQuery) -> Result<Value> {
    let (from, to) = range(query)?;
    let mut stmt = db.prepare("SELECT i.id,i.policy_id,p.name,i.client_id,i.event_ts,i.action,i.remaining_seconds,i.decision_revision FROM intervention_events i LEFT JOIN attention_policies p ON p.id=i.policy_id WHERE i.event_ts>=? AND i.event_ts<? ORDER BY i.event_ts DESC LIMIT 500")?;
    let rows = stmt.query_map(params![from,to], |row| Ok(json!({"id":row.get::<_,i64>(0)?,"policyId":row.get::<_,i64>(1)?,"policyName":row.get::<_,Option<String>>(2)?,"clientId":row.get::<_,Option<i64>>(3)?,"eventTs":row.get::<_,i64>(4)?,"action":row.get::<_,String>(5)?,"remainingSeconds":row.get::<_,Option<i64>>(6)?,"decisionRevision":row.get::<_,i64>(7)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"events":rows,"count":rows.len(),"limit":500}))
}

#[derive(Deserialize)]
pub(crate) struct ExportInput {
    pub(crate) kind: String,
    pub(crate) format: String,
    pub(crate) from: Option<i64>,
    pub(crate) to: Option<i64>,
}

pub(crate) fn export_context(db: &mut Connection, input: &ExportInput) -> Result<Value> {
    let query = RangeQuery {
        from: input.from,
        to: input.to,
        group: None,
        metric: None,
        resolution: None,
        limit: None,
        min_seconds: None,
        kind: None,
        format: Some(input.format.clone()),
        include_partial_days: None,
        max_follow_gap_seconds: None,
    };
    let (from, to) = range(&query)?;
    let all = ActivityRepository::new(db).export_normalized(from, to)?;
    let data = match input.kind.as_str() {
        "activity" | "activity_intervals" => {
            json!({"schemaVersion":1,"fromTs":from,"toTs":to,"activityIntervals":all["activityIntervals"],"classifierVersion":all["classifierVersion"],"rulesRevision":all["rulesRevision"]})
        }
        "entities" | "entities_and_aliases" => {
            json!({"schemaVersion":1,"entities":all["entities"]})
        }
        "all" | "all_context" => all,
        _ => bail!("unsupported export kind"),
    };
    match input.format.as_str() {
        "json" => Ok(
            json!({"kind":input.kind,"format":"json","mediaType":"application/json","fileName":format!("watchdog-{}.json",input.kind),"data":data}),
        ),
        "csv" => Ok(
            json!({"kind":input.kind,"format":"csv","mediaType":"text/csv","fileName":format!("watchdog-{}.csv",input.kind),"content":export_csv(&input.kind,&data)?}),
        ),
        _ => bail!("unsupported export format"),
    }
}

fn csv_cell(value: impl ToString) -> String {
    format!("\"{}\"", value.to_string().replace('"', "\"\""))
}

fn export_csv(kind: &str, data: &Value) -> Result<String> {
    let mut output = String::new();
    match kind {
        "activity" | "activity_intervals" => {
            output.push_str("start_ts,end_ts,activity,project_entity_id,service_entity_id,tool_entity_id,agent_session_id,confidence,source\n");
            for row in data["activityIntervals"].as_array().into_iter().flatten() {
                let fields = [
                    "startTs",
                    "endTs",
                    "activity",
                    "projectEntityId",
                    "serviceEntityId",
                    "toolEntityId",
                    "agentSessionId",
                    "confidence",
                ]
                .map(|key| {
                    csv_cell(
                        row.get(key)
                            .filter(|v| !v.is_null())
                            .map(Value::to_string)
                            .unwrap_or_default(),
                    )
                });
                output.push_str(&fields.join(","));
                output.push(',');
                output.push_str(&csv_cell(
                    row.pointer("/provenance/source")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                ));
                output.push('\n');
            }
        }
        "entities" | "entities_and_aliases" => {
            output.push_str("id,kind,display_name,archived,alias_kind,alias_value\n");
            for entity in data["entities"].as_array().into_iter().flatten() {
                let aliases = entity["aliases"]
                    .as_array()
                    .filter(|rows| !rows.is_empty())
                    .cloned()
                    .unwrap_or_else(|| vec![json!({})]);
                for alias in aliases {
                    output.push_str(
                        &[
                            csv_cell(entity["id"].to_string()),
                            csv_cell(entity["kind"].as_str().unwrap_or("")),
                            csv_cell(entity["displayName"].as_str().unwrap_or("")),
                            csv_cell(entity["archived"].to_string()),
                            csv_cell(alias["kind"].as_str().unwrap_or("")),
                            csv_cell(alias["value"].as_str().unwrap_or("")),
                        ]
                        .join(","),
                    );
                    output.push('\n');
                }
            }
        }
        "all" | "all_context" => {
            bail!("CSV all-context export is ambiguous; choose activity or entities")
        }
        _ => bail!("unsupported export kind"),
    }
    Ok(output)
}

#[derive(Deserialize)]
pub(crate) struct DeletionInput {
    pub(crate) kind: String,
    #[serde(rename = "clientId")]
    pub(crate) client_id: Option<i64>,
    #[serde(rename = "entityId")]
    pub(crate) entity_id: Option<i64>,
    #[serde(default)]
    pub(crate) preview: bool,
    #[serde(rename = "rebuildDerived", default)]
    pub(crate) rebuild_derived: bool,
}

fn count_sql(db: &Connection, sql: &str, id: i64) -> Result<i64> {
    db.query_row(sql, params![id], |row| row.get(0))
        .map_err(Into::into)
}

fn table_row_count(db: &Connection, table: &str) -> Result<i64> {
    db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get(0)
    })
    .map_err(Into::into)
}

pub(crate) fn deletion_preview(db: &Connection, input: &DeletionInput) -> Result<Value> {
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    match input.kind.as_str() {
        "browser_titles" => {
            counts.insert(
                "browser_events.titles".into(),
                table_row_count_where(db, "browser_events", "title IS NOT NULL")?,
            );
            counts.insert(
                "browser_current_states.titles".into(),
                table_row_count_where(db, "browser_current_states", "title IS NOT NULL")?,
            );
        }
        "browser_client" => {
            let id = input
                .client_id
                .ok_or_else(|| anyhow!("clientId required"))?;
            for (name, sql) in [
                ("browser_clients", "SELECT COUNT(*) FROM browser_clients WHERE id=?"),
                ("browser_request_rate_limits", "SELECT COUNT(*) FROM browser_request_rate_limits WHERE client_id=?"),
                ("browser_current_states", "SELECT COUNT(*) FROM browser_current_states WHERE client_id=?"),
                ("browser_current_states.titles", "SELECT COUNT(*) FROM browser_current_states WHERE client_id=? AND title IS NOT NULL"),
                ("browser_event_receipts", "SELECT COUNT(*) FROM browser_event_receipts WHERE client_id=?"),
                ("browser_events", "SELECT COUNT(*) FROM browser_events WHERE client_id=?"),
                ("browser_events.titles", "SELECT COUNT(*) FROM browser_events WHERE client_id=? AND title IS NOT NULL"),
                ("browser_attention_intervals", "SELECT COUNT(*) FROM browser_attention_intervals WHERE client_id=?"),
                ("browser_attention_intervals.source_first_event_id_cleared", "SELECT COUNT(*) FROM browser_attention_intervals WHERE client_id<>? AND source_first_event_id IN (SELECT id FROM browser_events WHERE client_id=?)"),
                ("browser_attention_intervals.source_last_event_id_cleared", "SELECT COUNT(*) FROM browser_attention_intervals WHERE client_id<>? AND source_last_event_id IN (SELECT id FROM browser_events WHERE client_id=?)"),
                ("policy_overrides", "SELECT COUNT(*) FROM policy_overrides WHERE client_id=?"),
                ("intervention_events.client_id_cleared", "SELECT COUNT(*) FROM intervention_events WHERE client_id=?"),
            ] {
                let count = if name.starts_with("browser_attention_intervals.source_") {
                    db.query_row(sql, params![id, id], |row| row.get(0))?
                } else {
                    count_sql(db, sql, id)?
                };
                counts.insert(name.into(), count);
            }
        }
        "entity" => {
            let id = input
                .entity_id
                .ok_or_else(|| anyhow!("entityId required"))?;
            for (name, sql) in [
                ("context_entities", "SELECT COUNT(*) FROM context_entities WHERE id=?"),
                ("context_entity_aliases", "SELECT COUNT(*) FROM context_entity_aliases WHERE entity_id=?"),
                ("context_entity_relations.parent", "SELECT COUNT(*) FROM context_entity_relations WHERE parent_entity_id=?"),
                ("context_entity_relations.child", "SELECT COUNT(*) FROM context_entity_relations WHERE child_entity_id=?"),
                ("focus_interval_labels", "SELECT COUNT(*) FROM focus_interval_labels WHERE entity_id=?"),
                ("manual_activity_labels", "SELECT COUNT(*) FROM manual_activity_labels WHERE entity_id=?"),
                ("focus_context_intervals.repository", "SELECT COUNT(*) FROM focus_context_intervals WHERE repository_entity_id=?"),
                ("focus_context_intervals.project", "SELECT COUNT(*) FROM focus_context_intervals WHERE project_entity_id=?"),
                ("activity_intervals.project", "SELECT COUNT(*) FROM activity_intervals WHERE project_entity_id=?"),
                ("activity_intervals.service", "SELECT COUNT(*) FROM activity_intervals WHERE service_entity_id=?"),
                ("activity_intervals.tool", "SELECT COUNT(*) FROM activity_intervals WHERE tool_entity_id=?"),
                ("browser_attention_intervals.service", "SELECT COUNT(*) FROM browser_attention_intervals WHERE service_entity_id=?"),
                ("intervention_events.service", "SELECT COUNT(*) FROM intervention_events WHERE service_entity_id=?"),
                ("relabel_jobs.entity", "SELECT COUNT(*) FROM relabel_jobs WHERE entity_id=?"),
                ("attention_policies.scope", "SELECT COUNT(DISTINCT p.id) FROM attention_policies p,json_tree(p.scope_json) value WHERE value.atom=? AND (value.key='entityId' OR value.key LIKE '%EntityId')"),
            ] {
                counts.insert(name.into(), count_sql(db, sql, id)?);
            }
        }
        "derived_labels" => {
            for table in [
                "activity_context_sources",
                "activity_intervals",
                "focus_interval_labels",
                "labeler_runs",
                "relabel_jobs",
                "derived_version_prune_jobs",
            ] {
                counts.insert(table.into(), table_row_count(db, table)?);
            }
            counts.insert("context_backfill_state.reset".into(), 1);
        }
        "all_context" => {
            for table in all_context_tables() {
                counts.insert((*table).into(), table_row_count(db, table)?);
            }
            counts.insert("browser_ingestion_metrics.reset".into(), 1);
            counts.insert("context_runtime_state.reset".into(), 1);
            counts.insert("context_backfill_state.reset".into(), 1);
        }
        _ => bail!("unsupported deletion kind"),
    }
    let total_sensitive_fields = counts
        .iter()
        .filter(|(key, _)| key.ends_with(".titles"))
        .map(|(_, count)| *count)
        .sum::<i64>();
    let total_rows = counts
        .iter()
        .filter(|(key, _)| !key.ends_with(".titles"))
        .map(|(_, count)| *count)
        .sum::<i64>();
    Ok(
        json!({"preview":true,"kind":input.kind,"selectedClientId":input.client_id,"selectedEntityId":input.entity_id,"counts":counts,"totalRows":total_rows,"totalSensitiveFields":total_sensitive_fields,"requiresConfirmation":true,"rebuildDerived":input.rebuild_derived}),
    )
}

fn table_row_count_where(db: &Connection, table: &str, predicate: &str) -> Result<i64> {
    db.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
        [],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

fn all_context_tables() -> &'static [&'static str] {
    &[
        "intervention_events",
        "policy_overrides",
        "attention_policies",
        "browser_request_rate_limits",
        "browser_attention_intervals",
        "browser_current_states",
        "browser_event_receipts",
        "browser_events",
        "browser_clients",
        "browser_pairing_rate_limits",
        "browser_pairing_codes",
        "agent_presence_leases",
        "agent_state_events",
        "activity_context_sources",
        "activity_intervals",
        "focus_interval_labels",
        "manual_activity_labels",
        "derived_version_prune_jobs",
        "relabel_jobs",
        "labeler_runs",
        "label_rules",
        "focus_context_intervals",
        "context_entity_relations",
        "context_entity_aliases",
        "context_entities",
        "source_health",
    ]
}

fn delete_table_batched(db: &mut Connection, table: &str) -> Result<(i64, i64)> {
    mutate_batched(
        db,
        &format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} LIMIT {DELETE_BATCH_ROWS})"),
        &[],
    )
}

fn mutate_batched(
    db: &mut Connection,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<(i64, i64)> {
    let mut changed_total = 0;
    let mut batches = 0;
    loop {
        let tx = db.transaction()?;
        let changed = tx.execute(sql, values)? as i64;
        tx.commit()?;
        if changed == 0 {
            break;
        }
        changed_total += changed;
        batches += 1;
    }
    Ok((changed_total, batches))
}

fn mutate_selected_batched(
    db: &mut Connection,
    table: &str,
    action: &str,
    predicate: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<(i64, i64)> {
    mutate_batched(
        db,
        &format!(
            "{action} WHERE rowid IN (SELECT rowid FROM {table} WHERE {predicate} LIMIT {DELETE_BATCH_ROWS})"
        ),
        values,
    )
}

fn json_references_entity(value: &Value, entity_id: i64) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, child)| {
            ((key == "entityId" || key.ends_with("EntityId")) && child.as_i64() == Some(entity_id))
                || json_references_entity(child, entity_id)
        }),
        Value::Array(items) => items
            .iter()
            .any(|child| json_references_entity(child, entity_id)),
        _ => false,
    }
}

fn reject_policy_entity_references(db: &Connection, entity_id: i64) -> Result<()> {
    let mut statement = db.prepare("SELECT id,scope_json FROM attention_policies ORDER BY id")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })? {
        let (policy_id, encoded) = row?;
        let scope: Value = serde_json::from_str(&encoded)
            .map_err(|_| anyhow!("policy {policy_id} has malformed scope JSON"))?;
        if json_references_entity(&scope, entity_id) {
            bail!("entity is referenced by policy {policy_id}; edit or delete the policy first");
        }
    }
    Ok(())
}

pub(crate) fn delete_context_data(db: &mut Connection, input: DeletionInput) -> Result<Value> {
    if input.preview {
        return deletion_preview(db, &input);
    }
    let preview = deletion_preview(db, &input)?;
    let mut rebuild_job = None;
    let mut rebuild_status: Option<String> = None;
    let mut deleted_counts = BTreeMap::new();
    let mut deleted_rows = BTreeMap::new();
    let mut cleared_references = BTreeMap::new();
    let mut cleared_fields = BTreeMap::new();
    let mut batch_count = 0;
    let mut max_rows_observed = 0;
    match input.kind.as_str() {
        "browser_titles" => {
            for table in ["browser_current_states", "browser_events"] {
                let (changed, batches) = mutate_selected_batched(
                    db,
                    table,
                    &format!("UPDATE {table} SET title=NULL"),
                    "title IS NOT NULL",
                    &[],
                )?;
                let key = format!("{table}.title");
                deleted_counts.insert(key.clone(), changed);
                cleared_fields.insert(key, changed);
                batch_count += batches;
                max_rows_observed = max_rows_observed.max(changed.min(DELETE_BATCH_ROWS));
            }
        }
        "browser_client" => {
            let id = input
                .client_id
                .ok_or_else(|| anyhow!("clientId required"))?;
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let revoked = tx.execute(
                "UPDATE browser_clients SET revoked_ts=COALESCE(revoked_ts,?) WHERE id=?",
                params![now_ms(), id],
            )? as i64;
            if revoked == 0 {
                bail!("browser client not found")
            }
            tx.commit()?;
            batch_count += 1;
            max_rows_observed = max_rows_observed.max(1);
            cleared_fields.insert("browser_clients.revoked_ts".into(), revoked);

            let mut finalized = false;
            for _ in 0..100 {
                for (key, table, action, predicate, category) in [
                    ("browser_attention_intervals", "browser_attention_intervals", "DELETE FROM browser_attention_intervals", "client_id=?1", "delete"),
                    ("browser_attention_intervals.source_first_event_id", "browser_attention_intervals", "UPDATE browser_attention_intervals SET source_first_event_id=NULL", "source_first_event_id IN (SELECT id FROM browser_events WHERE client_id=?1)", "clear"),
                    ("browser_attention_intervals.source_last_event_id", "browser_attention_intervals", "UPDATE browser_attention_intervals SET source_last_event_id=NULL", "source_last_event_id IN (SELECT id FROM browser_events WHERE client_id=?1)", "clear"),
                    ("browser_events.title", "browser_events", "UPDATE browser_events SET title=NULL", "client_id=?1 AND title IS NOT NULL", "field"),
                    ("browser_current_states.title", "browser_current_states", "UPDATE browser_current_states SET title=NULL", "client_id=?1 AND title IS NOT NULL", "field"),
                    ("browser_events", "browser_events", "DELETE FROM browser_events", "client_id=?1", "delete"),
                    ("browser_current_states", "browser_current_states", "DELETE FROM browser_current_states", "client_id=?1", "delete"),
                    ("browser_event_receipts", "browser_event_receipts", "DELETE FROM browser_event_receipts", "client_id=?1", "delete"),
                    ("browser_request_rate_limits", "browser_request_rate_limits", "DELETE FROM browser_request_rate_limits", "client_id=?1", "delete"),
                    ("policy_overrides", "policy_overrides", "DELETE FROM policy_overrides", "client_id=?1", "delete"),
                    ("intervention_events.client_id", "intervention_events", "UPDATE intervention_events SET client_id=NULL", "client_id=?1", "clear"),
                ] {
                    let (changed, batches) =
                        mutate_selected_batched(db, table, action, predicate, &[&id])?;
                    *deleted_counts.entry(key.to_string()).or_insert(0) += changed;
                    match category {
                        "delete" => *deleted_rows.entry(key.to_string()).or_insert(0) += changed,
                        "field" => *cleared_fields.entry(key.to_string()).or_insert(0) += changed,
                        _ => *cleared_references.entry(key.to_string()).or_insert(0) += changed,
                    }
                    batch_count += batches;
                    max_rows_observed =
                        max_rows_observed.max(changed.min(DELETE_BATCH_ROWS));
                }
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let dependents: i64 = tx.query_row(
                    "SELECT \
                       (SELECT COUNT(*) FROM browser_request_rate_limits WHERE client_id=?1)+\
                       (SELECT COUNT(*) FROM browser_current_states WHERE client_id=?1)+\
                       (SELECT COUNT(*) FROM browser_event_receipts WHERE client_id=?1)+\
                       (SELECT COUNT(*) FROM browser_events WHERE client_id=?1)+\
                       (SELECT COUNT(*) FROM browser_attention_intervals WHERE client_id=?1)+\
                       (SELECT COUNT(*) FROM policy_overrides WHERE client_id=?1)+\
                       (SELECT COUNT(*) FROM intervention_events WHERE client_id=?1)+\
                       (SELECT COUNT(*) FROM browser_attention_intervals WHERE source_first_event_id IN (SELECT id FROM browser_events WHERE client_id=?1))+\
                       (SELECT COUNT(*) FROM browser_attention_intervals WHERE source_last_event_id IN (SELECT id FROM browser_events WHERE client_id=?1))",
                    params![id],
                    |row| row.get(0),
                )?;
                if dependents == 0 {
                    let changed =
                        tx.execute("DELETE FROM browser_clients WHERE id=?", params![id])? as i64;
                    tx.commit()?;
                    deleted_counts.insert("browser_clients".into(), changed);
                    deleted_rows.insert("browser_clients".into(), changed);
                    batch_count += i64::from(changed > 0);
                    finalized = changed == 1;
                    break;
                }
                tx.rollback()?;
            }
            if !finalized {
                bail!("browser client deletion could not drain concurrent references")
            }
        }
        "entity" => {
            let id = input
                .entity_id
                .ok_or_else(|| anyhow!("entityId required"))?;
            let archived: Option<bool> = db
                .query_row(
                    "SELECT archived<>0 FROM context_entities WHERE id=?",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            match archived {
                None => bail!("entity not found"),
                Some(false) => bail!("entity must be archived before deletion"),
                Some(true) => {}
            }
            reject_policy_entity_references(db, id)?;
            let mut finalized = false;
            for _ in 0..100 {
                for (key, table, action, predicate, category) in [
                    (
                        "context_entity_aliases",
                        "context_entity_aliases",
                        "DELETE FROM context_entity_aliases",
                        "entity_id=?1",
                        "delete",
                    ),
                    (
                        "context_entity_relations.parent",
                        "context_entity_relations",
                        "DELETE FROM context_entity_relations",
                        "parent_entity_id=?1",
                        "delete",
                    ),
                    (
                        "context_entity_relations.child",
                        "context_entity_relations",
                        "DELETE FROM context_entity_relations",
                        "child_entity_id=?1",
                        "delete",
                    ),
                    (
                        "focus_interval_labels",
                        "focus_interval_labels",
                        "DELETE FROM focus_interval_labels",
                        "entity_id=?1",
                        "delete",
                    ),
                    (
                        "manual_activity_labels",
                        "manual_activity_labels",
                        "DELETE FROM manual_activity_labels",
                        "entity_id=?1",
                        "delete",
                    ),
                    (
                        "focus_context_intervals.repository_entity_id",
                        "focus_context_intervals",
                        "UPDATE focus_context_intervals SET repository_entity_id=NULL",
                        "repository_entity_id=?1",
                        "clear",
                    ),
                    (
                        "focus_context_intervals.project_entity_id",
                        "focus_context_intervals",
                        "UPDATE focus_context_intervals SET project_entity_id=NULL",
                        "project_entity_id=?1",
                        "clear",
                    ),
                    (
                        "activity_intervals.project_entity_id",
                        "activity_intervals",
                        "UPDATE activity_intervals SET project_entity_id=NULL",
                        "project_entity_id=?1",
                        "clear",
                    ),
                    (
                        "activity_intervals.service_entity_id",
                        "activity_intervals",
                        "UPDATE activity_intervals SET service_entity_id=NULL",
                        "service_entity_id=?1",
                        "clear",
                    ),
                    (
                        "activity_intervals.tool_entity_id",
                        "activity_intervals",
                        "UPDATE activity_intervals SET tool_entity_id=NULL",
                        "tool_entity_id=?1",
                        "clear",
                    ),
                    (
                        "browser_attention_intervals.service_entity_id",
                        "browser_attention_intervals",
                        "UPDATE browser_attention_intervals SET service_entity_id=NULL",
                        "service_entity_id=?1",
                        "clear",
                    ),
                    (
                        "intervention_events.service_entity_id",
                        "intervention_events",
                        "UPDATE intervention_events SET service_entity_id=NULL",
                        "service_entity_id=?1",
                        "clear",
                    ),
                    (
                        "relabel_jobs.entity_id",
                        "relabel_jobs",
                        "UPDATE relabel_jobs SET entity_id=NULL",
                        "entity_id=?1",
                        "clear",
                    ),
                ] {
                    let (changed, batches) =
                        mutate_selected_batched(db, table, action, predicate, &[&id])?;
                    *deleted_counts.entry(key.to_string()).or_insert(0) += changed;
                    if category == "delete" {
                        *deleted_rows.entry(key.to_string()).or_insert(0) += changed;
                    } else {
                        *cleared_references.entry(key.to_string()).or_insert(0) += changed;
                    }
                    batch_count += batches;
                    max_rows_observed = max_rows_observed.max(changed.min(DELETE_BATCH_ROWS));
                }
                let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                reject_policy_entity_references(&tx, id)?;
                let state: Option<(bool, i64)> = tx
                    .query_row(
                        "SELECT archived<>0,\
                           (SELECT COUNT(*) FROM context_entity_aliases WHERE entity_id=?1)+\
                           (SELECT COUNT(*) FROM context_entity_relations WHERE parent_entity_id=?1 OR child_entity_id=?1)+\
                           (SELECT COUNT(*) FROM focus_interval_labels WHERE entity_id=?1)+\
                           (SELECT COUNT(*) FROM manual_activity_labels WHERE entity_id=?1)+\
                           (SELECT COUNT(*) FROM focus_context_intervals WHERE repository_entity_id=?1 OR project_entity_id=?1)+\
                           (SELECT COUNT(*) FROM activity_intervals WHERE project_entity_id=?1 OR service_entity_id=?1 OR tool_entity_id=?1)+\
                           (SELECT COUNT(*) FROM browser_attention_intervals WHERE service_entity_id=?1)+\
                           (SELECT COUNT(*) FROM intervention_events WHERE service_entity_id=?1)+\
                           (SELECT COUNT(*) FROM relabel_jobs WHERE entity_id=?1)\
                         FROM context_entities WHERE id=?1",
                        params![id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                match state {
                    Some((true, 0)) => {
                        let changed = tx
                            .execute("DELETE FROM context_entities WHERE id=?", params![id])?
                            as i64;
                        tx.commit()?;
                        deleted_counts.insert("context_entities".into(), changed);
                        deleted_rows.insert("context_entities".into(), changed);
                        batch_count += i64::from(changed > 0);
                        max_rows_observed = max_rows_observed.max(changed);
                        finalized = changed == 1;
                        break;
                    }
                    Some((false, _)) => bail!("entity must remain archived during deletion"),
                    None => bail!("entity not found"),
                    _ => tx.rollback()?,
                }
            }
            if !finalized {
                bail!("entity deletion could not drain concurrent references")
            }
        }
        "derived_labels" => {
            for table in [
                "derived_version_prune_jobs",
                "relabel_jobs",
                "activity_context_sources",
                "activity_intervals",
                "focus_interval_labels",
                "labeler_runs",
            ] {
                let (deleted, batches) = delete_table_batched(db, table)?;
                deleted_counts.insert(table.to_string(), deleted);
                batch_count += batches;
            }
            db.execute(
                "UPDATE context_backfill_state SET last_environment_id=0,processed_samples=0,status='pending',updated_ts=?,error=NULL WHERE id=1",
                params![now_ms()],
            )?;
            if input.rebuild_derived {
                let bounds: (Option<i64>, Option<i64>) = db.query_row(
                    "SELECT MIN(start_ts),MAX(end_ts) FROM focus_context_intervals",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                if let (Some(from), Some(to)) = bounds {
                    let job_id =
                        crate::context::enqueue_relabel_job(db, from, to.max(from + 1), None)?;
                    for _ in 0..100_000 {
                        let status: String = db.query_row(
                            "SELECT status FROM relabel_jobs WHERE id=?",
                            params![job_id],
                            |row| row.get(0),
                        )?;
                        if status == "completed" {
                            rebuild_status = Some(status);
                            break;
                        }
                        if status == "failed" {
                            bail!("derived rebuild failed");
                        }
                        crate::context::process_relabel_job_batch(
                            db,
                            job_id,
                            DELETE_BATCH_ROWS as usize,
                        )?;
                    }
                    if rebuild_status.as_deref() != Some("completed") {
                        bail!("derived rebuild exceeded bounded batch limit");
                    }
                    rebuild_job = Some(job_id);
                }
            }
        }
        "all_context" => {
            for table in all_context_tables() {
                let (deleted, batches) = delete_table_batched(db, table)?;
                deleted_counts.insert((*table).to_string(), deleted);
                batch_count += batches;
            }
            let tx = db.transaction()?;
            tx.execute("UPDATE context_runtime_state SET rules_revision=0,active_labeler_version=1,updated_ts=? WHERE id=1", params![now_ms()])?;
            tx.execute("UPDATE context_backfill_state SET last_environment_id=0,processed_samples=0,status='pending',updated_ts=?,error=NULL WHERE id=1", params![now_ms()])?;
            tx.execute("UPDATE browser_ingestion_metrics SET accepted_events=0,duplicate_events=0,rejected_events=0,heartbeat_upserts=0,persisted_transitions=0,recovery_transitions=0,last_ingest_ts=NULL WHERE id=1", [])?;
            tx.commit()?;
        }
        _ => bail!("unsupported deletion kind"),
    }
    Ok(
        json!({"deleted":true,"kind":input.kind,"preview":preview,"receipt":{"deletedCounts":deleted_counts,"deletedRows":deleted_rows,"clearedReferences":cleared_references,"clearedFields":cleared_fields,"batchCount":batch_count,"maxRowsPerTransaction":DELETE_BATCH_ROWS,"maxRowsInTransactionObserved":max_rows_observed},"relabelJobId":rebuild_job,"rebuildStatus":rebuild_status}),
    )
}

pub(crate) fn storage_metrics(db: &mut Connection) -> Result<Value> {
    let metrics = ActivityRepository::new(db).storage_metrics(now_ms())?;
    let mut value = serde_json::to_value(metrics)?;
    value["items"] = value["objects"].clone();
    value["reusablePages"] = value["freelistPages"].clone();
    value["annualizedGrowthBytes"] = value["estimatedAnnualizedBytes"].clone();
    value["pruningLagSeconds"] = json!(value["pruningLagMs"].as_i64().unwrap_or(0) / 1000);
    value["compactionLagSeconds"] = json!(value["compactionLagMs"].as_i64().unwrap_or(0) / 1000);
    Ok(value)
}

pub(crate) fn source_health(db: &Connection) -> Result<Value> {
    let now = now_ms();
    let mut stmt = db.prepare("SELECT source,last_success_ts,last_error_ts,last_error_category,success_count,error_count,detail_json FROM source_health WHERE source NOT IN ('terminal','agent_presence','browser','policy') ORDER BY source LIMIT 100")?;
    let mut rows = stmt.query_map([], |row| Ok(json!({"source":row.get::<_,String>(0)?,"lastSuccessTs":row.get::<_,Option<i64>>(1)?,"lastErrorTs":row.get::<_,Option<i64>>(2)?,"errorCategory":row.get::<_,Option<String>>(3)?,"successCount":row.get::<_,i64>(4)? ,"errorCount":row.get::<_,i64>(5)? ,"detail":serde_json::from_str::<Value>(&row.get::<_,String>(6)?).unwrap_or(json!({}))})))?.collect::<rusqlite::Result<Vec<_>>>()?;

    let terminal = db.query_row("SELECT last_success_ts,last_error_ts,success_count,error_count,detail_json FROM source_health WHERE source='terminal'", [], |row| Ok((row.get::<_,Option<i64>>(0)?,row.get::<_,Option<i64>>(1)?,row.get::<_,i64>(2)?,row.get::<_,i64>(3)?,row.get::<_,String>(4)?))).optional()?;
    let (
        terminal_last_success,
        terminal_last_error,
        terminal_success,
        terminal_errors,
        terminal_detail,
    ) = terminal
        .map(|(success, error, successes, errors, detail)| {
            (
                success,
                error,
                successes,
                errors,
                serde_json::from_str::<Value>(&detail).unwrap_or(json!({})),
            )
        })
        .unwrap_or((None, None, 0, 0, json!({})));
    let terminal_attempts = terminal_success + terminal_errors;
    let terminal_resolved = terminal_detail["resolvedAttempts"].as_i64().unwrap_or(0);
    let terminal_ambiguous = terminal_detail["ambiguousAttempts"].as_i64().unwrap_or(0);

    let presence: (i64,i64,Option<i64>,Option<i64>) = db.query_row("SELECT (SELECT COUNT(*) FROM agent_presence_leases),(SELECT COUNT(*) FROM agent_state_events WHERE transition_kind='expired'),(SELECT MAX(observed_ts) FROM agent_presence_leases),(SELECT MAX(event_ts) FROM agent_state_events)", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
    let presence_producer: (Option<i64>,Option<i64>,i64,i64) = db.query_row("SELECT last_success_ts,last_error_ts,success_count,error_count FROM source_health WHERE source='agent_presence'", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?.unwrap_or((None,None,0,0));

    let relabel: (i64,i64,Option<i64>) = db.query_row("SELECT COALESCE(SUM(status IN ('queued','running')),0),COALESCE(SUM(status='failed'),0),MIN(CASE WHEN status IN ('queued','running') THEN created_ts END) FROM relabel_jobs", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
    let prune: (i64,Option<i64>) = db.query_row("SELECT COALESCE(SUM(status IN ('queued','running')),0),MIN(CASE WHEN status IN ('queued','running') THEN created_ts END) FROM derived_version_prune_jobs", [], |row| Ok((row.get(0)?,row.get(1)?)))?;
    let stale_state: (i64, Option<i64>) = db.query_row(
        "SELECT COUNT(*),MIN(expires_ts) FROM browser_current_states WHERE expires_ts<?",
        params![now],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;

    let coverage: (i64,i64,i64,i64) = db.query_row("WITH primary_labels AS (SELECT context_interval_id,MAX(value) value,MAX(confidence) confidence FROM active_focus_interval_labels WHERE dimension='activity.primary' GROUP BY context_interval_id) SELECT COALESCE(SUM(MAX(0,f.end_ts-f.start_ts)),0),COALESCE(SUM(CASE WHEN labels.value IS NOT NULL AND labels.value<>'unknown' THEN MAX(0,f.end_ts-f.start_ts) ELSE 0 END),0),COALESCE(SUM(CASE WHEN labels.value IS NULL OR labels.value='unknown' THEN MAX(0,f.end_ts-f.start_ts) ELSE 0 END),0),COALESCE(SUM(CASE WHEN labels.confidence>=0.8 THEN MAX(0,f.end_ts-f.start_ts) ELSE 0 END),0) FROM focus_context_intervals f LEFT JOIN primary_labels labels ON labels.context_interval_id=f.id", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
    let active_version: i64 = db.query_row(
        "SELECT active_labeler_version FROM context_runtime_state WHERE id=1",
        [],
        |row| row.get(0),
    )?;

    let browser: (i64,i64,i64,i64,Option<i64>) = db.query_row("SELECT accepted_events,duplicate_events,rejected_events,heartbeat_upserts,last_ingest_ts FROM browser_ingestion_metrics WHERE id=1", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)))?;
    let browser_queue: (i64,i64,i64,i64,i64,Option<i64>,i64,i64) = db.query_row("SELECT COALESCE(SUM(queue_depth),0),COALESCE(SUM(queue_bytes),0),COALESCE(SUM(queue_dropped),0),COALESCE(SUM(queue_permanent_rejected),0),COALESCE(SUM(queue_recovery_snapshots),0),MAX(queue_last_data_loss_ts),COUNT(*),COALESCE(SUM(queue_health_observed_ts IS NULL OR queue_health_observed_ts<? OR last_seen_ts IS NULL OR last_seen_ts<? OR (queue_depth>0 AND (queue_last_success_ts IS NULL OR queue_last_success_ts<?))),0) FROM browser_clients WHERE revoked_ts IS NULL", params![now-120_000,now-120_000,now-120_000], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?,row.get(7)?)))?;
    let explicit_policy_degraded: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM source_health WHERE source='policy' AND json_extract(detail_json,'$.degraded')=1)", [], |row| row.get(0))?;
    let policy_revision: i64 = db.query_row(
        "SELECT COALESCE(MAX(revision),0) FROM attention_policies",
        [],
        |row| row.get(0),
    )?;
    let policy_degraded = explicit_policy_degraded || browser_queue.3 > 0 || browser_queue.7 > 0;
    let maintenance_lag = [relabel.2, prune.1, stale_state.1]
        .into_iter()
        .flatten()
        .map(|ts| now.saturating_sub(ts))
        .max()
        .unwrap_or(0);
    let pruning_lag = [relabel.2, prune.1]
        .into_iter()
        .flatten()
        .map(|ts| now.saturating_sub(ts))
        .max()
        .unwrap_or(0);

    rows.extend([
        json!({"source":"terminal","status":if terminal_attempts==0{"unknown"}else if terminal_errors>0{"degraded"}else{"healthy"},"lastSuccessTs":terminal_last_success,"lastErrorTs":terminal_last_error,"detail":{"producerStatus":if terminal_attempts==0{"unknown"}else{"observed"},"attempts":terminal_attempts,"resolvedAttempts":terminal_resolved,"ambiguousAttempts":terminal_ambiguous,"matchRate":if terminal_attempts==0{0.0}else{terminal_resolved as f64/terminal_attempts as f64},"lastOutcome":terminal_detail["lastOutcome"]}}),
        json!({"source":"presence","status":if presence_producer.2+presence_producer.3==0{"unknown"}else if presence_producer.3>0{"degraded"}else{"healthy"},"lastSuccessTs":presence_producer.0,"lastErrorTs":presence_producer.1,"detail":{"producerStatus":if presence_producer.2+presence_producer.3==0{"unknown"}else{"observed"},"activeLeases":presence.0,"expiredCount":presence.1,"lastLeaseTs":presence.2,"lastEventTs":presence.3}}),
        json!({"source":"relabel","status":if relabel.1>0{"degraded"}else{"healthy"},"detail":{"pendingJobs":relabel.0,"errorCount":relabel.1,"pruneJobs":prune.0,"lagMs":maintenance_lag}}),
        json!({"source":"classification","status":if coverage.0==0{"unknown"}else{"healthy"},"detail":{"activeLabelerVersion":active_version,"observedMs":coverage.0,"classifiedMs":coverage.1,"unknownMs":coverage.2,"highConfidenceMs":coverage.3,"classifiedCoverage":if coverage.0==0{0.0}else{coverage.1 as f64/coverage.0 as f64},"highConfidenceCoverage":if coverage.0==0{0.0}else{coverage.3 as f64/coverage.0 as f64}}}),
        json!({"source":"browser","status":if browser_queue.6==0{"unknown"}else if policy_degraded{"degraded"}else{"healthy"},"detail":{"producerStatus":if browser_queue.6==0{"unknown"}else{"observed"},"accepted":browser.0,"duplicates":browser.1,"rejected":browser.2,"heartbeatUpserts":browser.3,"lastIngestTs":browser.4,"queueDepth":browser_queue.0,"queueBytes":browser_queue.1,"queueDropped":browser_queue.2,"permanentRejected":browser_queue.3,"recoverySnapshots":browser_queue.4,"lastDataLossTs":browser_queue.5,"staleClients":browser_queue.7}}),
        json!({"source":"policy","status":if browser_queue.6==0{"unknown"}else if policy_degraded{"degraded"}else{"healthy"},"detail":{"revision":policy_revision,"degraded":policy_degraded,"staleClients":browser_queue.7,"explicitDegradedReport":explicit_policy_degraded}}),
        json!({"source":"interval_maintenance","status":if maintenance_lag>0{"degraded"}else{"healthy"},"detail":{"compactionLagMs":stale_state.1.map(|ts|now.saturating_sub(ts)).unwrap_or(0),"staleBrowserStates":stale_state.0,"pruningLagMs":pruning_lag,"pendingPruneJobs":prune.0}}),
    ]);
    Ok(
        json!({"sources":rows,"count":rows.len(),"generatedTs":now,"activeVersionCoverage":{"labelerVersion":active_version,"observedMs":coverage.0,"classifiedMs":coverage.1,"unknownMs":coverage.2,"highConfidenceMs":coverage.3},"intervalMaintenance":{"lagMs":maintenance_lag,"staleBrowserStates":stale_state.0,"pendingRelabelJobs":relabel.0,"pendingPruneJobs":prune.0}}),
    )
}

#[cfg(test)]
mod deletion_inventory_tests {
    use super::*;

    fn discovered_foreign_keys(
        db: &Connection,
        parent: &str,
    ) -> std::collections::BTreeSet<(String, String, String)> {
        let mut tables = db
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let tables = tables
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        let mut found = std::collections::BTreeSet::new();
        for table in tables {
            let escaped = table.replace('"', "\"\"");
            let mut foreign_keys = db
                .prepare(&format!("PRAGMA foreign_key_list(\"{escaped}\")"))
                .unwrap();
            for row in foreign_keys
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(6)?,
                    ))
                })
                .unwrap()
            {
                let (referenced, column, on_delete) = row.unwrap();
                if referenced == parent {
                    found.insert((table.clone(), column, on_delete));
                }
            }
        }
        found
    }

    fn inventory(
        rows: &[(&str, &str, &str)],
    ) -> std::collections::BTreeSet<(String, String, String)> {
        rows.iter()
            .map(|(table, column, action)| {
                (table.to_string(), column.to_string(), action.to_string())
            })
            .collect()
    }

    #[test]
    fn browser_metric_predicates_match_policy_semantics_for_every_state() {
        let db = Connection::open_in_memory().unwrap();
        for metric in [
            "tab_active_seconds",
            "browser_focused_seconds",
            "document_visible_seconds",
            "media_playback_seconds",
            "audible_playback_seconds",
            "focused_playback_seconds",
            "verified_focused_playback_seconds",
            "background_playback_seconds",
        ] {
            let predicate = browser_metric_predicate(metric).unwrap();
            let sql = format!("SELECT {predicate} FROM (SELECT ? AS browser_focused,? AS tab_active,? AS document_visible,? AS media_playing,? AS audible,? AS os_focus_state,? AS os_focus_verified) b");
            for mask in 0..32 {
                let focused = mask & 1 != 0;
                let active = mask & 2 != 0;
                let visible = mask & 4 != 0;
                let playing = mask & 8 != 0;
                let audible = mask & 16 != 0;
                for os in ["verified", "mixed", "unknown", "contradicted"] {
                    let analytics: bool = db
                        .query_row(
                            &sql,
                            params![
                                focused as i64,
                                active as i64,
                                visible as i64,
                                playing as i64,
                                audible as i64,
                                os,
                                (os == "verified") as i64
                            ],
                            |row| row.get(0),
                        )
                        .unwrap();
                    let policy = crate::browser_api::metric_counts(
                        metric, focused, active, visible, audible, playing, os,
                    );
                    assert_eq!(analytics, policy, "metric={metric} mask={mask} os={os}");
                }
            }
        }
    }

    #[test]
    fn bounded_deletion_inventory_covers_every_current_foreign_key() {
        let mut db = Connection::open_in_memory().unwrap();
        crate::db::init_db(&mut db).unwrap();
        assert_eq!(
            discovered_foreign_keys(&db, "browser_clients"),
            inventory(&[
                ("browser_attention_intervals", "client_id", "CASCADE"),
                ("browser_current_states", "client_id", "CASCADE"),
                ("browser_event_receipts", "client_id", "CASCADE"),
                ("browser_events", "client_id", "CASCADE"),
                ("browser_request_rate_limits", "client_id", "CASCADE"),
                ("intervention_events", "client_id", "SET NULL"),
                ("policy_overrides", "client_id", "CASCADE")
            ])
        );
        assert_eq!(
            discovered_foreign_keys(&db, "browser_events"),
            inventory(&[
                (
                    "browser_attention_intervals",
                    "source_first_event_id",
                    "SET NULL"
                ),
                (
                    "browser_attention_intervals",
                    "source_last_event_id",
                    "SET NULL"
                )
            ])
        );
        assert_eq!(
            discovered_foreign_keys(&db, "context_entities"),
            inventory(&[
                ("activity_intervals", "project_entity_id", "SET NULL"),
                ("activity_intervals", "service_entity_id", "SET NULL"),
                ("activity_intervals", "tool_entity_id", "SET NULL"),
                (
                    "browser_attention_intervals",
                    "service_entity_id",
                    "SET NULL"
                ),
                ("context_entity_aliases", "entity_id", "CASCADE"),
                ("context_entity_relations", "child_entity_id", "CASCADE"),
                ("context_entity_relations", "parent_entity_id", "CASCADE"),
                ("focus_context_intervals", "project_entity_id", "SET NULL"),
                (
                    "focus_context_intervals",
                    "repository_entity_id",
                    "SET NULL"
                ),
                ("focus_interval_labels", "entity_id", "CASCADE"),
                ("intervention_events", "service_entity_id", "SET NULL"),
                ("manual_activity_labels", "entity_id", "CASCADE"),
                ("relabel_jobs", "entity_id", "SET NULL")
            ])
        );
    }
}
