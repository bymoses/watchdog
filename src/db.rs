use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, Context, Result};
use include_dir::{include_dir, Dir};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Map, Value};

use crate::{
    app::{BatterySample, EnvironmentSample, ProcRow},
    collect::subprocess_label,
    config::Config,
    util::*,
};

pub(crate) const PROCESS_ROWS_VIEW_SQL: &str = r#"
  SELECT sample_id,ts,pid,ppid,name,app,cmd,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self FROM process_samples
  UNION ALL
  SELECT s.sample_id,s.ts,s.pid,s.ppid,i.name,i.app,i.cmd,s.cpu_percent,s.cpu_seconds,s.io_mb,s.rss_mb,s.score,s.estimated_watts,s.is_self
    FROM process_samples_v2 s JOIN process_identities i ON i.id = s.process_id
"#;

static MIGRATIONS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/migrations");
const ARCHIVED_ENTITY_PRUNE_BATCH: usize = 32;

pub(crate) fn open_request_connection(
    path: &std::path::Path,
    query_only: bool,
) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("PRAGMA foreign_keys=ON;")?;
    if query_only {
        db.execute_batch("PRAGMA query_only=ON;")?;
    }
    Ok(db)
}

pub(crate) fn init_db(db: &mut Connection) -> Result<()> {
    // Connection-level setup cannot run inside the schema transaction (notably
    // journal_mode). Migration 001 repeats these pragmas for standalone use;
    // migration_body removes those repetitions before applying its schema.
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch(
        "PRAGMA journal_mode = WAL;\nPRAGMA synchronous = NORMAL;\nPRAGMA foreign_keys = ON;\n\
         CREATE TABLE IF NOT EXISTS schema_migrations (\
           name TEXT PRIMARY KEY,\
           applied_at INTEGER NOT NULL\
         );",
    )?;

    let mut files = MIGRATIONS.files().collect::<Vec<_>>();
    files.sort_by_key(|file| file.path().to_string_lossy().to_string());
    apply_migrations(
        db,
        files.into_iter().map(|file| {
            let name = file
                .path()
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .ok_or_else(|| anyhow!("invalid migration path {}", file.path().display()))?;
            let sql = file
                .contents_utf8()
                .ok_or_else(|| anyhow!("migration {name} is not valid UTF-8"))?;
            Ok((name, sql))
        }),
        |_| Ok(()),
    )
}

fn apply_migrations<'a, I, F>(db: &mut Connection, migrations: I, mut after_body: F) -> Result<()>
where
    I: IntoIterator<Item = Result<(String, &'a str)>>,
    F: FnMut(&str) -> Result<()>,
{
    for migration in migrations {
        let (name, sql) = migration?;
        if db
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE name=?",
                params![name],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            continue;
        }

        let partial = has_unshipped_migration_footprint(db, &name)?;
        let body = migration_body(db, &name, sql, partial)?;
        let tx = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .with_context(|| format!("begin migration {name}"))?;
        tx.execute_batch(&body)
            .with_context(|| format!("apply migration body {name}"))?;
        validate_migration_schema(&tx, &name)
            .with_context(|| format!("validate migration {name}"))?;
        after_body(&name).with_context(|| format!("migration {name} interrupted"))?;
        tx.execute(
            "INSERT INTO schema_migrations (name, applied_at) VALUES (?, ?)",
            params![name, now_ms()],
        )?;
        tx.commit()
            .with_context(|| format!("commit migration {name}"))?;
    }
    Ok(())
}

/// Removes connection-only setup from a migration and, for the currently
/// unshipped 004/005 schemas, turns a detected old non-atomic partial attempt
/// into an idempotent completion. Validation still runs before the marker is
/// inserted, so IF NOT EXISTS never substitutes for checking the final shape.
fn migration_body(db: &Connection, name: &str, sql: &str, repair_partial: bool) -> Result<String> {
    let mut lines = Vec::new();
    for line in sql.lines() {
        let trimmed = line.trim_start();
        let upper = trimmed.to_ascii_uppercase();
        if upper.starts_with("PRAGMA JOURNAL_MODE")
            || upper.starts_with("PRAGMA SYNCHRONOUS")
            || upper.starts_with("PRAGMA FOREIGN_KEYS")
        {
            continue;
        }
        if repair_partial
            && matches!(
                name,
                "004_focus_context.sql" | "005_context_integrations.sql"
            )
        {
            if upper.starts_with("ALTER TABLE ") && upper.contains(" ADD COLUMN ") {
                let words = trimmed.split_whitespace().collect::<Vec<_>>();
                if words.len() >= 6 && column_exists(db, words[2], words[5])? {
                    continue;
                }
            }
            let repaired = if trimmed.starts_with("CREATE UNIQUE INDEX ") {
                line.replacen(
                    "CREATE UNIQUE INDEX ",
                    "CREATE UNIQUE INDEX IF NOT EXISTS ",
                    1,
                )
            } else if trimmed.starts_with("CREATE INDEX ") {
                line.replacen("CREATE INDEX ", "CREATE INDEX IF NOT EXISTS ", 1)
            } else if trimmed.starts_with("CREATE TABLE ") {
                line.replacen("CREATE TABLE ", "CREATE TABLE IF NOT EXISTS ", 1)
            } else if trimmed.starts_with("CREATE TRIGGER ") {
                line.replacen("CREATE TRIGGER ", "CREATE TRIGGER IF NOT EXISTS ", 1)
            } else if trimmed.starts_with("INSERT INTO ") {
                line.replacen("INSERT INTO ", "INSERT OR IGNORE INTO ", 1)
            } else {
                line.to_string()
            };
            lines.push(repaired);
        } else {
            lines.push(line.to_string());
        }
    }
    let mut body = lines.join("\n");
    let trimmed = body.trim();
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("BEGIN;") && upper.ends_with("COMMIT;") {
        let start = trimmed.find(';').unwrap_or(0) + 1;
        let end = trimmed.len().saturating_sub("COMMIT;".len());
        body = trimmed[start..end].to_string();
    }
    if body.trim().is_empty() {
        bail!("migration {name} has no transactional schema body");
    }
    Ok(body)
}

fn has_unshipped_migration_footprint(db: &Connection, name: &str) -> Result<bool> {
    let objects: &[&str] = match name {
        "004_focus_context.sql" => &[
            "context_entities",
            "context_entity_aliases",
            "context_entity_relations",
            "focus_context_intervals",
            "label_rules",
            "labeler_runs",
            "focus_interval_labels",
            "manual_activity_labels",
            "activity_intervals",
        ],
        "005_context_integrations.sql" => &[
            "context_runtime_state",
            "relabel_jobs",
            "context_backfill_state",
            "activity_context_sources",
            "agent_presence_leases",
            "agent_state_events",
            "browser_clients",
            "browser_events",
            "browser_attention_intervals",
            "attention_policies",
            "source_health",
        ],
        _ => return Ok(false),
    };
    for object in objects {
        if object_exists(db, "table", object)? {
            return Ok(true);
        }
    }
    Ok(name == "005_context_integrations.sql"
        && column_exists(db, "focus_context_intervals", "terminal_kind")?)
}

fn validate_migration_schema(db: &Connection, name: &str) -> Result<()> {
    let required_tables: &[&str] = match name {
        "004_focus_context.sql" => &[
            "context_entities",
            "context_entity_aliases",
            "context_entity_relations",
            "focus_context_intervals",
            "label_rules",
            "labeler_runs",
            "focus_interval_labels",
            "manual_activity_labels",
            "activity_intervals",
        ],
        "005_context_integrations.sql" => &[
            "context_runtime_state",
            "relabel_jobs",
            "context_backfill_state",
            "activity_context_sources",
            "agent_presence_leases",
            "agent_state_events",
            "browser_pairing_codes",
            "browser_pairing_rate_limits",
            "browser_clients",
            "browser_request_rate_limits",
            "browser_current_states",
            "browser_event_receipts",
            "browser_events",
            "browser_ingestion_metrics",
            "browser_attention_intervals",
            "attention_policies",
            "policy_overrides",
            "intervention_events",
            "source_health",
        ],
        _ => &[],
    };
    for table in required_tables {
        if !object_exists(db, "table", table)? {
            bail!("required table {table} is missing");
        }
    }

    let required_columns: &[(&str, &[&str])] = match name {
        "004_focus_context.sql" => &[
            (
                "context_entities",
                &[
                    "id",
                    "kind",
                    "canonical_key",
                    "display_name",
                    "metadata_json",
                    "first_seen_ts",
                    "last_seen_ts",
                    "user_confirmed",
                    "archived",
                ],
            ),
            (
                "context_entity_aliases",
                &[
                    "id",
                    "entity_id",
                    "alias_kind",
                    "alias_value",
                    "source",
                    "confidence",
                    "created_ts",
                ],
            ),
            (
                "context_entity_relations",
                &[
                    "parent_entity_id",
                    "child_entity_id",
                    "relation",
                    "confidence",
                    "source",
                ],
            ),
            (
                "focus_context_intervals",
                &[
                    "id",
                    "start_ts",
                    "end_ts",
                    "last_observed_ts",
                    "start_environment_id",
                    "end_environment_id",
                    "context_key",
                    "extractor_version",
                    "closed_reason",
                    "focused_app",
                    "focused_title",
                    "focused_pid",
                    "ambiguity_json",
                ],
            ),
            (
                "label_rules",
                &[
                    "id",
                    "name",
                    "priority",
                    "enabled",
                    "origin",
                    "conditions_json",
                    "outputs_json",
                    "stop_dimensions_json",
                    "revision",
                    "created_ts",
                    "updated_ts",
                ],
            ),
            (
                "labeler_runs",
                &[
                    "id",
                    "labeler_version",
                    "rules_revision",
                    "started_ts",
                    "completed_ts",
                    "range_start_ts",
                    "range_end_ts",
                    "status",
                    "error",
                ],
            ),
            (
                "focus_interval_labels",
                &[
                    "id",
                    "context_interval_id",
                    "dimension",
                    "label_key",
                    "value",
                    "entity_id",
                    "confidence",
                    "source",
                    "rule_id",
                    "labeler_run_id",
                    "labeler_version",
                    "evidence_json",
                ],
            ),
            (
                "manual_activity_labels",
                &[
                    "id",
                    "start_ts",
                    "end_ts",
                    "dimension",
                    "value",
                    "entity_id",
                    "note",
                    "created_ts",
                ],
            ),
            (
                "activity_intervals",
                &[
                    "id",
                    "start_ts",
                    "end_ts",
                    "source",
                    "source_context_interval_id",
                    "activity",
                    "project_entity_id",
                    "service_entity_id",
                    "tool_entity_id",
                    "agent_session_id",
                    "confidence",
                    "labeler_version",
                    "evidence_json",
                ],
            ),
        ],
        "005_context_integrations.sql" => &[
            (
                "context_runtime_state",
                &[
                    "id",
                    "rules_revision",
                    "active_labeler_version",
                    "updated_ts",
                ],
            ),
            (
                "relabel_jobs",
                &[
                    "id",
                    "rules_revision",
                    "labeler_version",
                    "labeler_run_id",
                    "range_start_ts",
                    "range_end_ts",
                    "entity_id",
                    "cursor_start_ts",
                    "cursor_interval_id",
                    "processed_intervals",
                    "status",
                ],
            ),
            (
                "browser_clients",
                &[
                    "id",
                    "client_key",
                    "client_instance_id",
                    "secret_hash",
                    "allowed_origin",
                    "privacy_mode",
                    "last_sequence",
                    "revoked_ts",
                ],
            ),
            (
                "browser_current_states",
                &[
                    "client_id",
                    "window_id",
                    "tab_id",
                    "lease_key",
                    "last_event_key",
                    "episode_key",
                    "client_sequence",
                    "expires_ts",
                    "heartbeat_upserts",
                ],
            ),
            (
                "browser_event_receipts",
                &["client_id", "event_key", "client_sequence", "received_ts"],
            ),
            (
                "browser_events",
                &[
                    "id",
                    "client_id",
                    "event_key",
                    "client_sequence",
                    "window_id",
                    "tab_id",
                    "event_ts",
                    "event_type",
                    "media_state",
                    "picture_in_picture",
                ],
            ),
            (
                "browser_attention_intervals",
                &[
                    "id",
                    "client_id",
                    "window_id",
                    "tab_id",
                    "start_ts",
                    "end_ts",
                    "episode_key",
                    "service_entity_id",
                    "picture_in_picture",
                    "os_focus_verified",
                    "os_focus_state",
                    "os_context_interval_id",
                ],
            ),
        ],
        _ => &[],
    };
    for (table, columns) in required_columns {
        for column in *columns {
            if !column_exists(db, table, column)? {
                bail!("required column {table}.{column} is missing");
            }
        }
    }

    let required_indexes: &[&str] = match name {
        "004_focus_context.sql" => &[
            "idx_context_entities_kind_name",
            "idx_context_entity_aliases_entity",
            "idx_context_entity_relations_child",
            "idx_focus_context_single_open",
            "idx_focus_context_interval_start",
            "idx_focus_context_interval_end",
            "idx_focus_context_interval_key_end",
            "idx_label_rules_enabled_priority",
            "idx_labeler_runs_status_version",
            "idx_focus_interval_labels_interval_version",
            "idx_focus_interval_labels_dimension_entity",
            "idx_focus_interval_labels_dimension_value",
            "idx_manual_activity_labels_time",
            "idx_activity_intervals_source_context",
            "idx_activity_intervals_time",
            "idx_activity_intervals_project_time",
            "idx_activity_intervals_service_time",
        ],
        "005_context_integrations.sql" => &[
            "idx_relabel_jobs_status_id",
            "idx_focus_context_project_start",
            "idx_focus_context_agent_start",
            "idx_activity_context_sources_context",
            "idx_agent_presence_expiry",
            "idx_agent_presence_pid",
            "idx_agent_state_events_time",
            "idx_browser_pairing_expiry",
            "idx_browser_current_expiry",
            "idx_browser_receipts_received",
            "idx_browser_events_time",
            "idx_browser_events_domain_time",
            "idx_browser_events_client_sequence",
            "idx_browser_attention_time",
            "idx_browser_attention_domain_time",
            "idx_browser_attention_episode",
            "idx_attention_policies_enabled",
            "idx_policy_overrides_active",
            "idx_intervention_events_time",
        ],
        _ => &[],
    };
    for index in required_indexes {
        if !object_exists(db, "index", index)? {
            bail!("required index {index} is missing");
        }
    }

    if name == "004_focus_context.sql" {
        for (table, column) in [
            ("context_entity_aliases", "entity_id"),
            ("context_entity_relations", "parent_entity_id"),
            ("context_entity_relations", "child_entity_id"),
            ("focus_interval_labels", "entity_id"),
            ("manual_activity_labels", "entity_id"),
            ("activity_intervals", "project_entity_id"),
            ("activity_intervals", "service_entity_id"),
            ("activity_intervals", "tool_entity_id"),
        ] {
            if !foreign_key_exists(db, table, column, "context_entities")? {
                bail!("required foreign key {table}.{column} -> context_entities is missing");
            }
        }
    }
    if name == "005_context_integrations.sql" {
        for column in [
            "terminal_kind",
            "terminal_match_method",
            "terminal_match_confidence",
            "terminal_candidate_count",
            "tmux_client_pid",
            "tmux_session",
            "tmux_window",
            "tmux_pane",
            "tmux_pane_pid",
            "terminal_cwd_key",
            "terminal_cwd_display",
            "foreground_executable",
            "repository_entity_id",
            "project_entity_id",
            "agent_type",
            "agent_process_pid",
            "agent_session_id",
            "native_agent_session_id",
            "agent_state",
            "agent_match_method",
            "agent_match_confidence",
        ] {
            if !column_exists(db, "focus_context_intervals", column)? {
                bail!("required focus_context_intervals.{column} is missing");
            }
        }
        for trigger in [
            "label_rules_revision_insert",
            "label_rules_revision_update",
            "label_rules_revision_delete",
        ] {
            if !object_exists(db, "trigger", trigger)? {
                bail!("required trigger {trigger} is missing");
            }
        }
        for table in [
            "context_runtime_state",
            "context_backfill_state",
            "browser_ingestion_metrics",
        ] {
            let count: i64 = db.query_row(
                &format!(
                    "SELECT COUNT(*) FROM {} WHERE id=1",
                    quote_identifier(table)
                ),
                [],
                |row| row.get(0),
            )?;
            if count != 1 {
                bail!("required singleton row {table}.id=1 is missing");
            }
        }
    }
    let foreign_key_errors: i64 =
        db.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if foreign_key_errors != 0 {
        bail!("foreign_key_check found {foreign_key_errors} violation(s)");
    }
    Ok(())
}

fn object_exists(db: &Connection, kind: &str, name: &str) -> Result<bool> {
    Ok(db
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type=? AND name=?",
            params![kind, name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn column_exists(db: &Connection, table: &str, column: &str) -> Result<bool> {
    if !object_exists(db, "table", table)? {
        return Ok(false);
    }
    let sql = format!("PRAGMA table_info({})", quote_identifier(table));
    let mut statement = db.prepare(&sql)?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|candidate| candidate == column))
}

fn foreign_key_exists(db: &Connection, table: &str, column: &str, target: &str) -> Result<bool> {
    let sql = format!("PRAGMA foreign_key_list({})", quote_identifier(table));
    let mut statement = db.prepare(&sql)?;
    let keys = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(2)?, row.get::<_, String>(3)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(keys.iter().any(|(candidate_target, candidate_column)| {
        candidate_target == target && candidate_column == column
    }))
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

pub(crate) fn load_latest_battery_from_db(db: &Connection) -> Result<Option<BatterySample>> {
    db.query_row(
        "SELECT ts,on_battery,status,capacity,energy_wh,power_w,source FROM battery_samples ORDER BY ts DESC LIMIT 1",
        [],
        |r| Ok(BatterySample { ts: r.get(0)?, on_battery: r.get::<_, i64>(1)? != 0, status: r.get(2)?, capacity: r.get(3)?, energy_wh: r.get(4)?, power_w: r.get(5)?, source: r.get(6)? }),
    ).optional().map_err(Into::into)
}

pub(crate) fn insert_battery(db: &Connection, b: &BatterySample) -> Result<i64> {
    db.execute("INSERT INTO battery_samples (ts,on_battery,status,capacity,energy_wh,power_w,source) VALUES (?,?,?,?,?,?,?)",
        params![b.ts, if b.on_battery {1} else {0}, b.status, b.capacity, b.energy_wh, b.power_w, b.source])?;
    Ok(db.last_insert_rowid())
}

pub(crate) fn insert_process_rows(
    db: &mut Connection,
    sample_id: i64,
    ts: i64,
    rows: &[ProcRow],
) -> Result<()> {
    let tx = db.transaction()?;
    {
        let mut identity_insert =
            tx.prepare("INSERT OR IGNORE INTO process_identities (app,name,cmd) VALUES (?,?,?)")?;
        let mut identity_select =
            tx.prepare("SELECT id FROM process_identities WHERE app=? AND name=? AND cmd=?")?;
        let mut sample_insert = tx.prepare("INSERT INTO process_samples_v2 (sample_id,ts,pid,ppid,process_id,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)")?;
        let mut app_totals: HashMap<String, f64> = HashMap::new();
        let mut group_totals: HashMap<(String, String), GroupTotal> = HashMap::new();
        for r in rows {
            identity_insert.execute(params![r.now.app, r.now.name, r.now.cmd])?;
            let process_id: i64 = identity_select
                .query_row(params![r.now.app, r.now.name, r.now.cmd], |row| row.get(0))?;
            sample_insert.execute(params![
                sample_id,
                ts,
                r.now.pid,
                r.now.ppid,
                process_id,
                r.cpu_percent,
                r.cpu_seconds,
                r.io_mb,
                r.now.rss_mb,
                r.score,
                r.estimated_watts,
                if r.now.is_self { 1 } else { 0 }
            ])?;
            *app_totals.entry(r.now.app.clone()).or_default() += r.estimated_watts;
            let child = subprocess_label(&r.now.name, &r.now.cmd);
            let g = group_totals
                .entry((r.now.app.clone(), child.clone()))
                .or_insert_with(|| GroupTotal {
                    app: r.now.app.clone(),
                    child_name: child,
                    cmd: r.now.cmd.clone(),
                    ..Default::default()
                });
            g.watts += r.estimated_watts;
            g.cpu_seconds += r.cpu_seconds;
            g.io_mb += r.io_mb;
            g.rss_mb_sum += r.now.rss_mb;
            g.rss_rows += 1;
            g.row_count += 1;
            if g.cmd.is_empty() && !r.now.cmd.is_empty() {
                g.cmd = r.now.cmd.clone();
            }
        }
        let mut total_insert = tx.prepare("INSERT INTO sample_app_totals (sample_id,ts,app,watts) VALUES (?,?,?,?) ON CONFLICT(sample_id, app) DO UPDATE SET watts=excluded.watts, ts=excluded.ts")?;
        for (app, watts) in app_totals {
            total_insert.execute(params![sample_id, ts, app, watts])?;
        }
        let mut group_insert = tx.prepare("INSERT INTO sample_group_totals (sample_id,ts,app,child_name,cmd,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count) VALUES (?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(sample_id, app, child_name) DO UPDATE SET cmd=excluded.cmd,watts=excluded.watts,cpu_seconds=excluded.cpu_seconds,io_mb=excluded.io_mb,rss_mb_sum=excluded.rss_mb_sum,rss_rows=excluded.rss_rows,row_count=excluded.row_count,ts=excluded.ts")?;
        for g in group_totals.values() {
            group_insert.execute(params![
                sample_id,
                ts,
                g.app,
                g.child_name,
                g.cmd,
                g.watts,
                g.cpu_seconds,
                g.io_mb,
                g.rss_mb_sum,
                g.rss_rows,
                g.row_count
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

#[derive(Default)]
struct GroupTotal {
    app: String,
    child_name: String,
    cmd: String,
    watts: f64,
    cpu_seconds: f64,
    io_mb: f64,
    rss_mb_sum: f64,
    rss_rows: i64,
    row_count: i64,
}

pub(crate) fn insert_environment(db: &Connection, e: &EnvironmentSample) -> Result<i64> {
    db.execute("INSERT INTO environment_samples (sample_id,ts,theme,theme_detail,brightness_percent,brightness_source,audio_playing,audio_detail,video_streaming,video_detail,net_rx_mbps,net_tx_mbps,focused_app,focused_title,focused_pid,lid_closed,lid_detail,screen_locked,screen_lock_detail,fan_rpm,fan_source,usb_power_source,usb_power_w,usb_power_detail) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![e.sample_id,e.ts,e.theme,e.theme_detail,e.brightness_percent,e.brightness_source,opt_bool_i(e.audio_playing),e.audio_detail,opt_bool_i(e.video_streaming),e.video_detail,e.net_rx_mbps,e.net_tx_mbps,e.focused_app,e.focused_title,e.focused_pid,opt_bool_i(e.lid_closed),e.lid_detail,opt_bool_i(e.screen_locked),e.screen_lock_detail,e.fan_rpm,e.fan_source,opt_bool_i(e.usb_power_source),e.usb_power_w,e.usb_power_detail])?;
    Ok(db.last_insert_rowid())
}

pub(crate) fn record_sleep_gap_if_needed(
    cfg: &Config,
    db: &mut Connection,
    prev: Option<&BatterySample>,
    current: &BatterySample,
) -> Result<()> {
    let Some(prev) = prev else {
        return Ok(());
    };
    let duration_ms = current.ts - prev.ts;
    if duration_ms < cfg.suspend_gap.as_millis() as i64 {
        return Ok(());
    }
    let duration_sec = duration_ms as f64 / 1000.0;
    let duration_hours = duration_ms as f64 / 3_600_000.0;
    let capacity_delta = match (current.capacity, prev.capacity) {
        (Some(c), Some(p)) => Some(c - p),
        _ => None,
    };
    let energy_delta = match (current.energy_wh, prev.energy_wh) {
        (Some(c), Some(p)) => Some(c - p),
        _ => None,
    };
    let avg_power_w = energy_delta.map(|d| d / duration_hours);
    let avg_percent = capacity_delta.map(|d| d / duration_hours);
    let measure = energy_delta.or(capacity_delta).unwrap_or(0.0);
    let kind = if measure > 0.0 {
        "suspend charge"
    } else if measure < 0.0 {
        "suspend discharge"
    } else {
        "sample gap"
    };
    db.execute("INSERT OR IGNORE INTO sleep_events (start_ts,end_ts,duration_sec,kind,start_capacity,end_capacity,capacity_delta,start_energy_wh,end_energy_wh,energy_delta_wh,avg_power_w,avg_percent_per_hour) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        params![prev.ts,current.ts,duration_sec,kind,prev.capacity,current.capacity,capacity_delta,prev.energy_wh,current.energy_wh,energy_delta,avg_power_w,avg_percent])?;
    Ok(())
}

pub(crate) fn prune_old(db: &Connection, cfg: &Config, now: i64) -> Result<()> {
    let cutoff = now - cfg.retention_days * 24 * 3_600_000;
    let detailed_days = cfg
        .detailed_process_retention_days
        .min(cfg.retention_days)
        .max(1);
    let detailed_cutoff = now - detailed_days * 24 * 3_600_000;

    let context_cutoff = now - cfg.context_retention_days * 24 * 3_600_000;
    let browser_cutoff = now - cfg.browser_event_retention_days * 24 * 3_600_000;
    let title_cutoff = now - cfg.browser_title_retention_days * 24 * 3_600_000;

    // Close stale leases/attention before pruning their transition evidence.
    crate::browser_api::expire_stale_states(db, now, 500)?;
    db.execute("UPDATE browser_attention_intervals SET closed_reason='lease_expired' WHERE closed_reason IS NULL AND client_id IN (SELECT client_id FROM browser_current_states WHERE expires_ts < ?)", params![now])?;
    db.execute(
        "DELETE FROM browser_current_states WHERE expires_ts < ?",
        params![now],
    )?;
    db.execute(
        "DELETE FROM agent_presence_leases WHERE expires_ts < ?",
        params![now],
    )?;
    db.execute(
        "DELETE FROM browser_events WHERE event_ts < ?",
        params![browser_cutoff],
    )?;
    db.execute(
        "UPDATE browser_events SET title=NULL WHERE event_ts < ?",
        params![title_cutoff],
    )?;
    db.execute(
        "UPDATE browser_current_states SET title=NULL WHERE last_observed_ts < ?",
        params![title_cutoff],
    )?;
    db.execute(
        "DELETE FROM browser_pairing_codes WHERE expires_ts < ?",
        params![now],
    )?;
    db.execute(
        "DELETE FROM browser_pairing_rate_limits WHERE bucket_ts < ?",
        params![now - 3_600_000],
    )?;
    db.execute(
        "DELETE FROM browser_request_rate_limits WHERE bucket_ts < ?",
        params![now - 3_600_000],
    )?;
    db.execute(
        "DELETE FROM intervention_events WHERE event_ts < ?",
        params![context_cutoff],
    )?;
    db.execute(
        "DELETE FROM agent_state_events WHERE event_ts < ?",
        params![context_cutoff],
    )?;
    db.execute(
        "DELETE FROM browser_attention_intervals WHERE end_ts < ?",
        params![context_cutoff],
    )?;
    prune_superseded_label_versions(db)?;
    crate::context::prune_context(db, context_cutoff)?;
    delete_unreferenced_archived_entities(db, context_cutoff - 30 * 24 * 3_600_000)?;

    for sql in [
        "DELETE FROM sample_app_totals WHERE ts < ?",
        "DELETE FROM sample_group_totals WHERE ts < ?",
        "DELETE FROM sleep_events WHERE end_ts < ?",
        "DELETE FROM environment_samples WHERE ts < ?",
        "DELETE FROM battery_samples WHERE ts < ?",
    ] {
        db.execute(sql, params![cutoff])?;
    }

    // Detailed per-process snapshots are only used for recent/latest process views.
    db.execute(
        "DELETE FROM process_samples WHERE ts < ?",
        params![detailed_cutoff],
    )?;
    db.execute(
        "DELETE FROM process_samples_v2 WHERE ts < ?",
        params![detailed_cutoff],
    )?;

    // sample_process_totals is no longer written or queried; purge legacy rows opportunistically.
    db.execute("DELETE FROM sample_process_totals", [])?;
    db.execute("DELETE FROM process_identities WHERE id NOT IN (SELECT DISTINCT process_id FROM process_samples_v2)", [])?;
    Ok(())
}

/// Enqueue superseded versions for bounded maintenance pruning. Retention never
/// deletes a version-sized label/activity set in its own transaction.
fn prune_superseded_label_versions(db: &Connection) -> Result<usize> {
    if !object_exists(db, "table", "context_runtime_state")?
        || !object_exists(db, "table", "derived_version_prune_jobs")?
    {
        return Ok(0);
    }
    let active: i64 = db.query_row(
        "SELECT active_labeler_version FROM context_runtime_state WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    Ok(db.execute(
        "INSERT OR IGNORE INTO derived_version_prune_jobs(labeler_version,status,created_ts) SELECT DISTINCT labeler_version,'queued',? FROM labeler_runs WHERE labeler_version<?",
        params![now_ms(),active],
    )?)
}

fn context_entity_foreign_keys(db: &Connection) -> Result<Vec<(String, String)>> {
    let tables = {
        let mut statement = db.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
        )?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut references = HashSet::new();
    for table in tables {
        let sql = format!("PRAGMA foreign_key_list({})", quote_identifier(&table));
        let mut statement = db.prepare(&sql)?;
        for row in statement.query_map([], |row| {
            Ok((row.get::<_, String>(2)?, row.get::<_, String>(3)?))
        })? {
            let (target, column) = row?;
            if target == "context_entities" {
                references.insert((table.clone(), column));
            }
        }
    }
    let mut references = references.into_iter().collect::<Vec<_>>();
    references.sort();
    Ok(references)
}

fn entity_is_referenced(
    db: &Connection,
    entity_id: i64,
    references: &[(String, String)],
) -> Result<bool> {
    for (table, column) in references {
        let sql = format!(
            "SELECT 1 FROM {} WHERE {}=? LIMIT 1",
            quote_identifier(table),
            quote_identifier(column)
        );
        if db
            .query_row(&sql, params![entity_id], |_| Ok(()))
            .optional()?
            .is_some()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn delete_unreferenced_archived_entities(db: &Connection, before_ts: i64) -> Result<usize> {
    if !object_exists(db, "table", "context_entities")? {
        return Ok(0);
    }
    // Keep retention writer holds short and recheck every reference in the same
    // transaction as deletion. A later maintenance pass continues at the next
    // bounded candidate batch.
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    let references = context_entity_foreign_keys(&tx)?;
    let candidates = {
        let mut sql = String::from(
            "SELECT entity.id FROM context_entities entity \
             WHERE entity.archived=1 AND entity.last_seen_ts < ?",
        );
        for (table, column) in &references {
            sql.push_str(&format!(
                " AND NOT EXISTS (SELECT 1 FROM {} reference WHERE reference.{}=entity.id)",
                quote_identifier(table),
                quote_identifier(column)
            ));
        }
        if object_exists(&tx, "table", "attention_policies")? {
            // json_tree is both a strict JSON parse and a recursive semantic
            // reference check matching entityId/*EntityId policy fields.
            sql.push_str(
                " AND NOT EXISTS (SELECT 1 FROM attention_policies policy, \
                 json_tree(policy.scope_json) item WHERE item.type='integer' \
                 AND (item.key='entityId' OR substr(item.key,-8)='EntityId') \
                 AND item.atom=entity.id)",
            );
        }
        sql.push_str(" ORDER BY entity.id LIMIT ?");
        let mut statement = tx.prepare(&sql)?;
        let rows = statement
            .query_map(
                params![before_ts, ARCHIVED_ENTITY_PRUNE_BATCH as i64],
                |row| row.get::<_, i64>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut deleted = 0;
    for entity_id in candidates {
        let semantic_references = crate::activity::entity_reference_count(&tx, entity_id)?;
        if semantic_references == 0 && !entity_is_referenced(&tx, entity_id, &references)? {
            deleted += tx.execute(
                "DELETE FROM context_entities WHERE id=?",
                params![entity_id],
            )?;
        }
    }
    tx.commit()?;
    Ok(deleted)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SqliteObjectAllocation {
    pub(crate) name: String,
    pub(crate) allocated_bytes: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DbstatCapability {
    Available(Vec<SqliteObjectAllocation>),
    Unavailable { category: &'static str },
}

/// Query real SQLite page ownership. Callers must preserve `Unavailable` as a
/// capability result and must not relabel proportional row estimates as object
/// allocation.
pub(crate) fn sqlite_object_allocations(db: &Connection) -> DbstatCapability {
    let mut statement = match db
        .prepare("SELECT name,COALESCE(SUM(pgsize),0) FROM dbstat GROUP BY name ORDER BY name")
    {
        Ok(statement) => statement,
        Err(_) => {
            return DbstatCapability::Unavailable {
                category: "dbstat_unavailable",
            }
        }
    };
    let rows = match statement.query_map([], |row| {
        Ok(SqliteObjectAllocation {
            name: row.get(0)?,
            allocated_bytes: row.get(1)?,
        })
    }) {
        Ok(rows) => rows,
        Err(_) => {
            return DbstatCapability::Unavailable {
                category: "dbstat_unavailable",
            }
        }
    };
    match rows.collect::<rusqlite::Result<Vec<_>>>() {
        Ok(rows) => DbstatCapability::Available(rows),
        Err(_) => DbstatCapability::Unavailable {
            category: "dbstat_unavailable",
        },
    }
}

#[derive(Default)]
pub(crate) struct GroupAgg {
    pub(crate) app: String,
    pub(crate) watt_samples: f64,
    pub(crate) cpu_seconds: f64,
    pub(crate) io_mb: f64,
    pub(crate) rss_mb_sum: f64,
    pub(crate) rss_rows: i64,
    pub(crate) rows: i64,
    pub(crate) children: HashMap<String, ChildAgg>,
}
#[derive(Default)]
pub(crate) struct ChildAgg {
    pub(crate) name: String,
    pub(crate) cmd: String,
    pub(crate) watt_samples: f64,
    pub(crate) cpu_seconds: f64,
    pub(crate) io_mb: f64,
    pub(crate) rss_mb_sum: f64,
    pub(crate) rss_rows: i64,
    pub(crate) rows: i64,
    pub(crate) samples: i64,
}
pub(crate) struct GroupRow {
    pub(crate) app: String,
    pub(crate) name: String,
    pub(crate) cmd: String,
    pub(crate) watts: f64,
    pub(crate) cpu_seconds: f64,
    pub(crate) io_mb: f64,
    pub(crate) rss_mb_sum: f64,
    pub(crate) rss_rows: i64,
    pub(crate) rows: i64,
    pub(crate) samples: i64,
}
pub(crate) fn query_group_rows(db: &Connection, since: i64) -> Result<Vec<GroupRow>> {
    // SQLite's GROUP BY sorter spills every matching raw row (including cmd)
    // to disk. Stream in timestamp order instead: memory scales with distinct
    // groups, not history, and a read-only dashboard refresh writes no temp DB.
    let mut stmt = db.prepare("SELECT app,child_name,cmd,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count FROM sample_group_totals INDEXED BY idx_sample_group_totals_ts WHERE ts >= ?")?;
    let mut rows = stmt.query(params![since])?;
    let mut groups: HashMap<(String, String), GroupRow> = HashMap::new();
    while let Some(row) = rows.next()? {
        let app: String = row.get(0)?;
        let name: String = row.get(1)?;
        let cmd: String = row.get(2)?;
        let group = groups.entry((app.clone(), name.clone())).or_insert_with(|| GroupRow {
            app, name, cmd: cmd.clone(), watts: 0.0, cpu_seconds: 0.0,
            io_mb: 0.0, rss_mb_sum: 0.0, rss_rows: 0, rows: 0, samples: 0,
        });
        if cmd < group.cmd { group.cmd = cmd; }
        group.watts += row.get::<_, f64>(3)?;
        group.cpu_seconds += row.get::<_, f64>(4)?;
        group.io_mb += row.get::<_, f64>(5)?;
        group.rss_mb_sum += row.get::<_, f64>(6)?;
        group.rss_rows += row.get::<_, i64>(7)?;
        group.rows += row.get::<_, i64>(8)?;
        group.samples += 1;
    }
    let mut rows = groups.into_values().collect::<Vec<_>>();
    rows.sort_by(|a, b| (&a.app, &a.name).cmp(&(&b.app, &b.name)));
    Ok(rows)
}

pub(crate) struct SeriesRow {
    pub(crate) id: i64,
    pub(crate) ts: i64,
    pub(crate) capacity: Option<f64>,
    pub(crate) power_w: Option<f64>,
    pub(crate) on_battery: i64,
    pub(crate) status: String,
    pub(crate) focused_app: Option<String>,
    pub(crate) focused_title: Option<String>,
    pub(crate) focused_pid: Option<i64>,
    pub(crate) lid_closed: Option<i64>,
    pub(crate) lid_detail: Option<String>,
    pub(crate) screen_locked: Option<i64>,
    pub(crate) screen_lock_detail: Option<String>,
    pub(crate) brightness_percent: Option<f64>,
    pub(crate) brightness_source: Option<String>,
    pub(crate) theme: Option<String>,
    pub(crate) theme_detail: Option<String>,
    pub(crate) video_streaming: Option<i64>,
    pub(crate) video_detail: Option<String>,
    pub(crate) net_rx_mbps: Option<f64>,
    pub(crate) net_tx_mbps: Option<f64>,
    pub(crate) usb_power_source: Option<i64>,
    pub(crate) usb_power_w: Option<f64>,
    pub(crate) usb_power_detail: Option<String>,
}
pub(crate) fn query_series_rows(db: &Connection, sql: &str, ts: i64) -> Result<Vec<SeriesRow>> {
    let mut stmt = db.prepare(sql)?;
    let rows = stmt
        .query_map(params![ts], |r| {
            Ok(SeriesRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                capacity: r.get(2)?,
                power_w: r.get(3)?,
                on_battery: r.get(4)?,
                status: r.get(5)?,
                focused_app: r.get(6)?,
                focused_title: r.get(7)?,
                focused_pid: r.get(8)?,
                lid_closed: r.get(9)?,
                lid_detail: r.get(10)?,
                screen_locked: r.get(11)?,
                screen_lock_detail: r.get(12)?,
                brightness_percent: r.get(13)?,
                brightness_source: r.get(14)?,
                theme: r.get(15)?,
                theme_detail: r.get(16)?,
                video_streaming: r.get(17)?,
                video_detail: r.get(18)?,
                net_rx_mbps: r.get(19)?,
                net_tx_mbps: r.get(20)?,
                usb_power_source: r.get(21)?,
                usb_power_w: r.get(22)?,
                usb_power_detail: r.get(23)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn query_one_json(
    db: &Connection,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<Value> {
    Ok(query_all_json(db, sql, values)?
        .into_iter()
        .next()
        .unwrap_or(Value::Null))
}
pub(crate) fn query_all_json(
    db: &Connection,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Value>> {
    let mut stmt = db.prepare(sql)?;
    let names: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(ToString::to_string)
        .collect();
    let rows = stmt.query_map(values, |row| {
        let mut obj = Map::new();
        for (idx, name) in names.iter().enumerate() {
            let v = row.get_ref(idx)?;
            obj.insert(name.clone(), sql_value_to_json(v));
        }
        Ok(Value::Object(obj))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
fn sql_value_to_json(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef::*;
    match v {
        Null => Value::Null,
        Integer(i) => json!(i),
        Real(f) => json!(f),
        Text(t) => json!(String::from_utf8_lossy(t)),
        Blob(_) => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn streaming_groups_match_sql_aggregates() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE sample_group_totals(ts INTEGER,app TEXT,child_name TEXT,cmd TEXT,watts REAL,cpu_seconds REAL,io_mb REAL,rss_mb_sum REAL,rss_rows INTEGER,row_count INTEGER); CREATE INDEX idx_sample_group_totals_ts ON sample_group_totals(ts);").unwrap();
        for i in 0..1000 {
            db.execute("INSERT INTO sample_group_totals VALUES(?,?,?,?,?,?,?,?,?,?)", rusqlite::params![i, format!("app{}",i%3), format!("child{}",i%7), format!("cmd{}",i%11), i as f64/7.0, 0.3, 2.5, 32.0, 2, 3]).unwrap();
        }
        let actual = super::query_group_rows(&db, 200).unwrap();
        let mut stmt = db.prepare("SELECT app,child_name,MIN(cmd),SUM(watts),SUM(cpu_seconds),SUM(io_mb),SUM(rss_mb_sum),SUM(rss_rows),SUM(row_count),COUNT(*) FROM sample_group_totals INDEXED BY idx_sample_group_totals_ts WHERE ts>=200 GROUP BY app,child_name").unwrap();
        let expected = stmt.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,f64>(3)?,r.get::<_,f64>(4)?,r.get::<_,f64>(5)?,r.get::<_,f64>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?,r.get::<_,i64>(9)?))).unwrap().collect::<rusqlite::Result<Vec<_>>>().unwrap();
        assert_eq!(actual.len(), expected.len());
        for (a,e) in actual.iter().zip(expected) {
            assert_eq!((&a.app,&a.name,&a.cmd),(&e.0,&e.1,&e.2));
            for (x,y) in [(a.watts,e.3),(a.cpu_seconds,e.4),(a.io_mb,e.5),(a.rss_mb_sum,e.6)] { assert!((x-y).abs()<1e-9); }
            assert_eq!((a.rss_rows,a.rows,a.samples),(e.7,e.8,e.9));
        }
    }
    use super::*;

    fn migration_connection() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "PRAGMA foreign_keys=ON;\
             CREATE TABLE schema_migrations(name TEXT PRIMARY KEY,applied_at INTEGER NOT NULL);",
        )
        .unwrap();
        db
    }

    #[test]
    fn request_connections_enforce_busy_timeout_and_foreign_keys() {
        let path = std::env::temp_dir().join(format!(
            "watchdog-request-{}-{}.sqlite",
            std::process::id(),
            crate::util::now_ms()
        ));
        for query_only in [false, true] {
            let db = open_request_connection(&path, query_only).unwrap();
            let busy: i64 = db
                .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                .unwrap();
            let foreign_keys: i64 = db
                .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
                .unwrap();
            let read_only: i64 = db
                .query_row("PRAGMA query_only", [], |row| row.get(0))
                .unwrap();
            assert_eq!(busy, 5_000);
            assert_eq!(foreign_keys, 1);
            assert_eq!(read_only, i64::from(query_only));
        }
        let _ = std::fs::remove_file(path);
    }

    fn embedded_migrations_through(maximum: &str) -> Vec<Result<(String, &'static str)>> {
        let mut files = MIGRATIONS.files().collect::<Vec<_>>();
        files.sort_by_key(|file| file.path().to_string_lossy().to_string());
        files
            .into_iter()
            .filter_map(|file| {
                let name = file.path().file_name()?.to_string_lossy().to_string();
                (name.as_str() <= maximum)
                    .then(|| Ok((name, file.contents_utf8().expect("migration must be UTF-8"))))
            })
            .collect()
    }

    #[test]
    fn migration_body_and_marker_roll_back_together_then_retry() {
        let mut db = migration_connection();
        let migration = "CREATE TABLE interrupted(id INTEGER PRIMARY KEY);";
        let error = apply_migrations(
            &mut db,
            [Ok(("900_interrupted.sql".to_string(), migration))],
            |_| Err(anyhow!("injected interruption after body")),
        )
        .unwrap_err();
        assert!(error.to_string().contains("interrupted"));
        assert!(!object_exists(&db, "table", "interrupted").unwrap());
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            0
        );

        apply_migrations(
            &mut db,
            [Ok(("900_interrupted.sql".to_string(), migration))],
            |_| Ok(()),
        )
        .unwrap();
        assert!(object_exists(&db, "table", "interrupted").unwrap());
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name='900_interrupted.sql'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn failing_body_rolls_back_ddl_and_can_retry() {
        let mut db = migration_connection();
        let failing =
            "CREATE TABLE retryable(id INTEGER PRIMARY KEY); INSERT INTO missing VALUES(1);";
        assert!(apply_migrations(
            &mut db,
            [Ok(("901_retry.sql".to_string(), failing))],
            |_| Ok(()),
        )
        .is_err());
        assert!(!object_exists(&db, "table", "retryable").unwrap());

        apply_migrations(
            &mut db,
            [Ok((
                "901_retry.sql".to_string(),
                "BEGIN; CREATE TABLE retryable(id INTEGER PRIMARY KEY); COMMIT;",
            ))],
            |_| Ok(()),
        )
        .unwrap();
        assert!(object_exists(&db, "table", "retryable").unwrap());
    }

    #[test]
    fn upgrades_from_003_with_all_atomic_context_migrations() {
        let mut db = migration_connection();
        apply_migrations(
            &mut db,
            embedded_migrations_through("003_agent_usage.sql"),
            |_| Ok(()),
        )
        .unwrap();
        assert!(!object_exists(&db, "table", "context_entities").unwrap());

        init_db(&mut db).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            super::MIGRATIONS.files().count() as i64
        );
        assert!(object_exists(&db, "index", "idx_environment_sample").unwrap());
        assert!(column_exists(&db, "focus_context_intervals", "agent_session_id").unwrap());
        assert!(object_exists(&db, "view", "active_activity_intervals").unwrap());
        validate_migration_schema(&db, "004_focus_context.sql").unwrap();
        validate_migration_schema(&db, "005_context_integrations.sql").unwrap();
    }

    #[test]
    fn repairs_unmarked_partial_004_and_005_then_is_idempotent() {
        let mut db = migration_connection();
        apply_migrations(
            &mut db,
            embedded_migrations_through("003_agent_usage.sql"),
            |_| Ok(()),
        )
        .unwrap();
        let migration_004 = MIGRATIONS
            .get_file("004_focus_context.sql")
            .unwrap()
            .contents_utf8()
            .unwrap();
        let partial_004 = migration_004
            .split("CREATE INDEX idx_context_entities_kind_name")
            .next()
            .unwrap();
        db.execute_batch(partial_004).unwrap();
        init_db(&mut db).unwrap();
        validate_migration_schema(&db, "005_context_integrations.sql").unwrap();

        db.execute(
            "DELETE FROM schema_migrations WHERE name='005_context_integrations.sql'",
            [],
        )
        .unwrap();
        db.execute_batch(
            "DROP TABLE source_health; \
             DROP TRIGGER label_rules_revision_delete; \
             DROP INDEX idx_browser_receipts_received;",
        )
        .unwrap();
        init_db(&mut db).unwrap();
        validate_migration_schema(&db, "005_context_integrations.sql").unwrap();
        init_db(&mut db).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name IN ('004_focus_context.sql','005_context_integrations.sql')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
    }

    #[test]
    fn malformed_partial_004_is_detected_without_marking_applied() {
        let mut db = migration_connection();
        apply_migrations(
            &mut db,
            embedded_migrations_through("003_agent_usage.sql"),
            |_| Ok(()),
        )
        .unwrap();
        db.execute_batch("CREATE TABLE context_entities(id INTEGER PRIMARY KEY);")
            .unwrap();
        let error = init_db(&mut db).unwrap_err();
        assert!(error.to_string().contains("004_focus_context.sql"));
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE name='004_focus_context.sql'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn real_005_interruption_rolls_back_every_schema_change() {
        let mut db = migration_connection();
        apply_migrations(
            &mut db,
            embedded_migrations_through("004_focus_context.sql"),
            |_| Ok(()),
        )
        .unwrap();
        let migration_005 = MIGRATIONS
            .get_file("005_context_integrations.sql")
            .unwrap()
            .contents_utf8()
            .unwrap();
        assert!(apply_migrations(
            &mut db,
            [Ok((
                "005_context_integrations.sql".to_string(),
                migration_005
            ))],
            |_| Err(anyhow!("injected stop")),
        )
        .is_err());
        assert!(!object_exists(&db, "table", "context_runtime_state").unwrap());
        assert!(!column_exists(&db, "focus_context_intervals", "terminal_kind").unwrap());

        apply_migrations(
            &mut db,
            [Ok((
                "005_context_integrations.sql".to_string(),
                migration_005,
            ))],
            |_| Ok(()),
        )
        .unwrap();
        validate_migration_schema(&db, "005_context_integrations.sql").unwrap();
    }

    #[test]
    fn entity_retention_discovers_every_schema_foreign_key() {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        let references = context_entity_foreign_keys(&db).unwrap();
        let actual = references.into_iter().collect::<HashSet<_>>();
        let expected = [
            ("activity_intervals", "project_entity_id"),
            ("activity_intervals", "service_entity_id"),
            ("activity_intervals", "tool_entity_id"),
            ("browser_attention_intervals", "service_entity_id"),
            ("context_entity_aliases", "entity_id"),
            ("context_entity_relations", "child_entity_id"),
            ("context_entity_relations", "parent_entity_id"),
            ("focus_context_intervals", "project_entity_id"),
            ("focus_context_intervals", "repository_entity_id"),
            ("focus_interval_labels", "entity_id"),
            ("intervention_events", "service_entity_id"),
            ("manual_activity_labels", "entity_id"),
            ("relabel_jobs", "entity_id"),
        ]
        .into_iter()
        .map(|(table, column)| (table.to_string(), column.to_string()))
        .collect::<HashSet<_>>();
        assert_eq!(actual, expected);

        for (id, key) in [(1_i64, 1_u8), (2, 2), (3, 3)] {
            let mut canonical = [0_u8; 32];
            canonical[0] = key;
            db.execute(
                "INSERT INTO context_entities(id,kind,canonical_key,display_name,first_seen_ts,last_seen_ts,archived) VALUES(?,'project',?,? ,0,0,1)",
                params![id, canonical.as_slice(), format!("entity-{id}")],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO context_entity_aliases(entity_id,alias_kind,alias_value,source,confidence,created_ts) VALUES(2,'name','kept','test',1,0)",
            [],
        )
        .unwrap();
        db.execute(
            "INSERT INTO attention_policies(name,scope_json,metric,window_kind,budget_seconds,enforcement,revision,created_ts,updated_ts) VALUES('kept policy','{\"any\":[{\"projectEntityId\":3}]}','tab_active_seconds','calendar-day',60,'observe',1,0,0)",
            [],
        )
        .unwrap();
        assert_eq!(delete_unreferenced_archived_entities(&db, 1).unwrap(), 1);
        assert!(!db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM context_entities WHERE id=1)",
                [],
                |row| row.get::<_, bool>(0)
            )
            .unwrap());
        for id in [2_i64, 3] {
            assert!(db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM context_entities WHERE id=?)",
                    params![id],
                    |row| row.get::<_, bool>(0)
                )
                .unwrap());
        }
    }

    #[test]
    fn entity_retention_fails_closed_on_malformed_policy_scope() {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        db.execute(
            "INSERT INTO context_entities(id,kind,canonical_key,display_name,first_seen_ts,last_seen_ts,archived) VALUES(1,'project',?,'candidate',0,0,1)",
            params![[1_u8; 32].as_slice()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO attention_policies(name,scope_json,metric,window_kind,budget_seconds,enforcement,revision,created_ts,updated_ts) VALUES('malformed','{','tab_active_seconds','calendar-day',60,'observe',1,0,0)",
            [],
        )
        .unwrap();
        assert!(delete_unreferenced_archived_entities(&db, 1).is_err());
        assert!(db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM context_entities WHERE id=1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap());
    }

    #[test]
    fn entity_retention_deletes_at_most_one_bounded_batch() {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        for id in 1..=(ARCHIVED_ENTITY_PRUNE_BATCH as i64 + 5) {
            let mut canonical = [0_u8; 32];
            canonical[..8].copy_from_slice(&id.to_be_bytes());
            db.execute(
                "INSERT INTO context_entities(id,kind,canonical_key,display_name,first_seen_ts,last_seen_ts,archived) VALUES(?,'project',?,? ,0,0,1)",
                params![id, canonical.as_slice(), format!("candidate-{id}")],
            )
            .unwrap();
        }
        assert_eq!(
            delete_unreferenced_archived_entities(&db, 1).unwrap(),
            ARCHIVED_ENTITY_PRUNE_BATCH
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM context_entities", [], |row| row
                .get::<_, usize>(0))
                .unwrap(),
            5
        );
    }

    #[test]
    fn retention_queues_only_versions_older_than_active_reader() {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        db.execute(
            "INSERT INTO focus_context_intervals(start_ts,end_ts,last_observed_ts,context_key,extractor_version,closed_reason) VALUES(0,1,1,?,1,'transition')",
            params![[0_u8; 32].as_slice()],
        )
        .unwrap();
        for version in [1_i64, 2, 3] {
            let mut key = [0_u8; 32];
            key[0] = version as u8;
            db.execute(
                "INSERT INTO focus_interval_labels(context_interval_id,dimension,label_key,value,confidence,source,labeler_version) VALUES(1,'activity.primary',?,'work',1,'test',?)",
                params![key.as_slice(), version],
            )
            .unwrap();
            db.execute(
                "INSERT INTO labeler_runs(labeler_version,rules_revision,started_ts,completed_ts,status) VALUES(?,1,0,1,'completed')",
                params![version],
            )
            .unwrap();
        }
        db.execute(
            "UPDATE context_runtime_state SET active_labeler_version=2 WHERE id=1",
            [],
        )
        .unwrap();
        assert_eq!(prune_superseded_label_versions(&db).unwrap(), 1);
        let versions = db
            .prepare("SELECT labeler_version FROM focus_interval_labels ORDER BY labeler_version")
            .unwrap()
            .query_map([], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(versions, vec![1, 2, 3]);
        let queued = db
            .prepare("SELECT labeler_version,status FROM derived_version_prune_jobs ORDER BY labeler_version")
            .unwrap()
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(queued, vec![(1, "queued".to_string())]);
    }

    #[test]
    fn dbstat_reports_real_allocations_or_explicit_capability_fallback() {
        let mut db = Connection::open_in_memory().unwrap();
        init_db(&mut db).unwrap();
        match sqlite_object_allocations(&db) {
            DbstatCapability::Available(objects) => {
                assert!(objects.iter().any(|object| {
                    object.name == "schema_migrations" && object.allocated_bytes > 0
                }));
            }
            DbstatCapability::Unavailable { category } => {
                assert_eq!(category, "dbstat_unavailable")
            }
        }
    }
}
