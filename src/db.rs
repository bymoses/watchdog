use std::collections::HashMap;

use anyhow::{anyhow, Result};
use include_dir::{include_dir, Dir};
use rusqlite::{params, Connection, OptionalExtension};
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

pub(crate) fn init_db(db: &mut Connection) -> Result<()> {
    db.execute_batch(
        "PRAGMA journal_mode = WAL;\nPRAGMA synchronous = NORMAL;\nPRAGMA foreign_keys = ON;\n\
         CREATE TABLE IF NOT EXISTS schema_migrations (\
           name TEXT PRIMARY KEY,\
           applied_at INTEGER NOT NULL\
         );",
    )?;

    let mut files = MIGRATIONS.files().collect::<Vec<_>>();
    files.sort_by_key(|f| f.path().to_string_lossy().to_string());
    for file in files {
        let name = file
            .path()
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .ok_or_else(|| anyhow!("invalid migration path {}", file.path().display()))?;
        let already_applied: Option<String> = db
            .query_row("SELECT name FROM schema_migrations WHERE name=?", params![name], |row| row.get(0))
            .optional()?;
        if already_applied.is_some() {
            continue;
        }
        let sql = file
            .contents_utf8()
            .ok_or_else(|| anyhow!("migration {name} is not valid UTF-8"))?;
        db.execute_batch(sql)?;
        db.execute(
            "INSERT INTO schema_migrations (name, applied_at) VALUES (?, ?)",
            params![name, now_ms()],
        )?;
    }
    Ok(())
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

pub(crate) fn insert_process_rows(db: &mut Connection, sample_id: i64, ts: i64, rows: &[ProcRow]) -> Result<()> {
    let tx = db.transaction()?;
    {
        let mut identity_insert = tx.prepare("INSERT OR IGNORE INTO process_identities (app,name,cmd) VALUES (?,?,?)")?;
        let mut identity_select = tx.prepare("SELECT id FROM process_identities WHERE app=? AND name=? AND cmd=?")?;
        let mut sample_insert = tx.prepare("INSERT INTO process_samples_v2 (sample_id,ts,pid,ppid,process_id,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)")?;
        let mut app_totals: HashMap<String, f64> = HashMap::new();
        let mut group_totals: HashMap<(String, String), GroupTotal> = HashMap::new();
        for r in rows {
            identity_insert.execute(params![r.now.app, r.now.name, r.now.cmd])?;
            let process_id: i64 = identity_select.query_row(params![r.now.app, r.now.name, r.now.cmd], |row| row.get(0))?;
            sample_insert.execute(params![sample_id, ts, r.now.pid, r.now.ppid, process_id, r.cpu_percent, r.cpu_seconds, r.io_mb, r.now.rss_mb, r.score, r.estimated_watts, if r.now.is_self {1} else {0}])?;
            *app_totals.entry(r.now.app.clone()).or_default() += r.estimated_watts;
            let child = subprocess_label(&r.now.name, &r.now.cmd);
            let g = group_totals.entry((r.now.app.clone(), child.clone())).or_insert_with(|| GroupTotal { app: r.now.app.clone(), child_name: child, cmd: r.now.cmd.clone(), ..Default::default() });
            g.watts += r.estimated_watts; g.cpu_seconds += r.cpu_seconds; g.io_mb += r.io_mb; g.rss_mb_sum += r.now.rss_mb; g.rss_rows += 1; g.row_count += 1;
            if g.cmd.is_empty() && !r.now.cmd.is_empty() { g.cmd = r.now.cmd.clone(); }
        }
        let mut total_insert = tx.prepare("INSERT INTO sample_app_totals (sample_id,ts,app,watts) VALUES (?,?,?,?) ON CONFLICT(sample_id, app) DO UPDATE SET watts=excluded.watts, ts=excluded.ts")?;
        for (app, watts) in app_totals { total_insert.execute(params![sample_id, ts, app, watts])?; }
        let mut group_insert = tx.prepare("INSERT INTO sample_group_totals (sample_id,ts,app,child_name,cmd,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count) VALUES (?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(sample_id, app, child_name) DO UPDATE SET cmd=excluded.cmd,watts=excluded.watts,cpu_seconds=excluded.cpu_seconds,io_mb=excluded.io_mb,rss_mb_sum=excluded.rss_mb_sum,rss_rows=excluded.rss_rows,row_count=excluded.row_count,ts=excluded.ts")?;
        for g in group_totals.values() { group_insert.execute(params![sample_id, ts, g.app, g.child_name, g.cmd, g.watts, g.cpu_seconds, g.io_mb, g.rss_mb_sum, g.rss_rows, g.row_count])?; }
    }
    tx.commit()?;
    Ok(())
}

#[derive(Default)]
struct GroupTotal { app: String, child_name: String, cmd: String, watts: f64, cpu_seconds: f64, io_mb: f64, rss_mb_sum: f64, rss_rows: i64, row_count: i64 }

pub(crate) fn insert_environment(db: &Connection, e: &EnvironmentSample) -> Result<()> {
    db.execute("INSERT INTO environment_samples (sample_id,ts,theme,theme_detail,brightness_percent,brightness_source,audio_playing,audio_detail,video_streaming,video_detail,net_rx_mbps,net_tx_mbps,focused_app,focused_title,focused_pid,lid_closed,lid_detail,screen_locked,screen_lock_detail,fan_rpm,fan_source,usb_power_source,usb_power_w,usb_power_detail) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![e.sample_id,e.ts,e.theme,e.theme_detail,e.brightness_percent,e.brightness_source,opt_bool_i(e.audio_playing),e.audio_detail,opt_bool_i(e.video_streaming),e.video_detail,e.net_rx_mbps,e.net_tx_mbps,e.focused_app,e.focused_title,e.focused_pid,opt_bool_i(e.lid_closed),e.lid_detail,opt_bool_i(e.screen_locked),e.screen_lock_detail,e.fan_rpm,e.fan_source,opt_bool_i(e.usb_power_source),e.usb_power_w,e.usb_power_detail])?;
    Ok(())
}

pub(crate) fn record_sleep_gap_if_needed(cfg: &Config, db: &mut Connection, prev: Option<&BatterySample>, current: &BatterySample) -> Result<()> {
    let Some(prev) = prev else { return Ok(()); };
    let duration_ms = current.ts - prev.ts;
    if duration_ms < cfg.suspend_gap.as_millis() as i64 { return Ok(()); }
    let duration_sec = duration_ms as f64 / 1000.0;
    let duration_hours = duration_ms as f64 / 3_600_000.0;
    let capacity_delta = match (current.capacity, prev.capacity) { (Some(c), Some(p)) => Some(c - p), _ => None };
    let energy_delta = match (current.energy_wh, prev.energy_wh) { (Some(c), Some(p)) => Some(c - p), _ => None };
    let avg_power_w = energy_delta.map(|d| d / duration_hours);
    let avg_percent = capacity_delta.map(|d| d / duration_hours);
    let measure = energy_delta.or(capacity_delta).unwrap_or(0.0);
    let kind = if measure > 0.0 { "suspend charge" } else if measure < 0.0 { "suspend discharge" } else { "sample gap" };
    db.execute("INSERT OR IGNORE INTO sleep_events (start_ts,end_ts,duration_sec,kind,start_capacity,end_capacity,capacity_delta,start_energy_wh,end_energy_wh,energy_delta_wh,avg_power_w,avg_percent_per_hour) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        params![prev.ts,current.ts,duration_sec,kind,prev.capacity,current.capacity,capacity_delta,prev.energy_wh,current.energy_wh,energy_delta,avg_power_w,avg_percent])?;
    Ok(())
}

pub(crate) fn prune_old(db: &Connection, cfg: &Config, now: i64) -> Result<()> {
    let cutoff = now - cfg.retention_days * 24 * 3_600_000;
    let detailed_days = cfg.detailed_process_retention_days.min(cfg.retention_days).max(1);
    let detailed_cutoff = now - detailed_days * 24 * 3_600_000;

    for sql in [
        "DELETE FROM sample_app_totals WHERE ts < ?",
        "DELETE FROM sample_group_totals WHERE ts < ?",
        "DELETE FROM sleep_events WHERE end_ts < ?",
        "DELETE FROM environment_samples WHERE ts < ?",
        "DELETE FROM battery_samples WHERE ts < ?",
    ] { db.execute(sql, params![cutoff])?; }

    // Detailed per-process snapshots are only used for recent/latest process views.
    db.execute("DELETE FROM process_samples WHERE ts < ?", params![detailed_cutoff])?;
    db.execute("DELETE FROM process_samples_v2 WHERE ts < ?", params![detailed_cutoff])?;

    // sample_process_totals is no longer written or queried; purge legacy rows opportunistically.
    db.execute("DELETE FROM sample_process_totals", [])?;
    db.execute("DELETE FROM process_identities WHERE id NOT IN (SELECT DISTINCT process_id FROM process_samples_v2)", [])?;
    Ok(())
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
    let mut stmt = db.prepare("SELECT app,child_name AS name,MIN(cmd) AS cmd,SUM(watts) AS watts,SUM(cpu_seconds) AS cpu_seconds,SUM(io_mb) AS io_mb,SUM(rss_mb_sum) AS rss_mb_sum,SUM(rss_rows) AS rss_rows,SUM(row_count) AS rows,COUNT(*) AS samples FROM sample_group_totals INDEXED BY idx_sample_group_totals_ts WHERE ts >= ? GROUP BY app,child_name")?;
    let rows = stmt
        .query_map(params![since], |r| Ok(GroupRow { app: r.get(0)?, name: r.get(1)?, cmd: r.get(2)?, watts: r.get(3)?, cpu_seconds: r.get(4)?, io_mb: r.get(5)?, rss_mb_sum: r.get(6)?, rss_rows: r.get(7)?, rows: r.get(8)?, samples: r.get(9)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) struct SeriesRow {
    pub(crate) id: i64, pub(crate) ts: i64, pub(crate) capacity: Option<f64>, pub(crate) power_w: Option<f64>, pub(crate) on_battery: i64, pub(crate) status: String,
    pub(crate) focused_app: Option<String>, pub(crate) focused_title: Option<String>, pub(crate) focused_pid: Option<i64>, pub(crate) lid_closed: Option<i64>, pub(crate) lid_detail: Option<String>, pub(crate) screen_locked: Option<i64>, pub(crate) screen_lock_detail: Option<String>,
    pub(crate) brightness_percent: Option<f64>, pub(crate) brightness_source: Option<String>, pub(crate) theme: Option<String>, pub(crate) theme_detail: Option<String>, pub(crate) video_streaming: Option<i64>, pub(crate) video_detail: Option<String>, pub(crate) net_rx_mbps: Option<f64>, pub(crate) net_tx_mbps: Option<f64>,
    pub(crate) usb_power_source: Option<i64>, pub(crate) usb_power_w: Option<f64>, pub(crate) usb_power_detail: Option<String>,
}
pub(crate) fn query_series_rows(db: &Connection, sql: &str, ts: i64) -> Result<Vec<SeriesRow>> {
    let mut stmt = db.prepare(sql)?;
    let rows = stmt
        .query_map(params![ts], |r| Ok(SeriesRow { id: r.get(0)?, ts: r.get(1)?, capacity: r.get(2)?, power_w: r.get(3)?, on_battery: r.get(4)?, status: r.get(5)?, focused_app: r.get(6)?, focused_title: r.get(7)?, focused_pid: r.get(8)?, lid_closed: r.get(9)?, lid_detail: r.get(10)?, screen_locked: r.get(11)?, screen_lock_detail: r.get(12)?, brightness_percent: r.get(13)?, brightness_source: r.get(14)?, theme: r.get(15)?, theme_detail: r.get(16)?, video_streaming: r.get(17)?, video_detail: r.get(18)?, net_rx_mbps: r.get(19)?, net_tx_mbps: r.get(20)?, usb_power_source: r.get(21)?, usb_power_w: r.get(22)?, usb_power_detail: r.get(23)? }))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn query_one_json(db: &Connection, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<Value> {
    Ok(query_all_json(db, sql, values)?.into_iter().next().unwrap_or(Value::Null))
}
pub(crate) fn query_all_json(db: &Connection, sql: &str, values: &[&dyn rusqlite::ToSql]) -> Result<Vec<Value>> {
    let mut stmt = db.prepare(sql)?;
    let names: Vec<String> = stmt.column_names().into_iter().map(ToString::to_string).collect();
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
    match v { Null => Value::Null, Integer(i) => json!(i), Real(f) => json!(f), Text(t) => json!(String::from_utf8_lossy(t)), Blob(_) => Value::Null }
}
