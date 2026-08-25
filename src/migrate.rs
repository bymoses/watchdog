use std::fs;

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection};

use crate::{
    cli::{MigrateArgs, MigrateTarget, OldDbArgs},
    config::default_data_dir,
    db::init_db,
};

pub(crate) fn migrate(args: MigrateArgs) -> Result<()> {
    match args.target {
        MigrateTarget::OldDb(args) => migrate_old_db(args),
    }
}

fn migrate_old_db(args: OldDbArgs) -> Result<()> {
    if !args.source.exists() {
        return Err(anyhow!(
            "source DB does not exist: {}",
            args.source.display()
        ));
    }
    let data_dir = args.data_dir.unwrap_or_else(default_data_dir);
    fs::create_dir_all(&data_dir).with_context(|| format!("create {}", data_dir.display()))?;
    let dest = data_dir.join("bms-watchdog.sqlite");
    if args.replace && dest.exists() {
        fs::remove_file(&dest).with_context(|| format!("remove {}", dest.display()))?;
        for suffix in ["-wal", "-shm"] {
            let sidecar = format!("{}{}", dest.display(), suffix);
            let _ = fs::remove_file(sidecar);
        }
    }

    let mut db = Connection::open(&dest).with_context(|| format!("open {}", dest.display()))?;
    init_db(&mut db)?;
    ensure_empty_destination(&db)?;
    db.execute(
        "ATTACH DATABASE ? AS old",
        params![args.source.display().to_string()],
    )?;

    let tx = db.transaction()?;
    copy_table(
        &tx,
        "battery_samples",
        "id,ts,on_battery,status,capacity,energy_wh,power_w,source",
        "id,ts,on_battery,status,capacity,energy_wh,power_w,source",
    )?;
    copy_table(&tx, "sleep_events", "id,start_ts,end_ts,duration_sec,kind,start_capacity,end_capacity,capacity_delta,start_energy_wh,end_energy_wh,energy_delta_wh,avg_power_w,avg_percent_per_hour", "id,start_ts,end_ts,duration_sec,kind,start_capacity,end_capacity,capacity_delta,start_energy_wh,end_energy_wh,energy_delta_wh,avg_power_w,avg_percent_per_hour")?;
    tx.execute("INSERT INTO process_samples (id,sample_id,ts,pid,ppid,name,app,cmd,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self)
        SELECT id,sample_id,ts,pid,ppid,name,CASE WHEN app='battery-monitor' THEN 'bms-watchdog' ELSE app END,cmd,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self FROM old.process_samples", [])?;
    tx.execute("INSERT INTO process_identities (id,app,name,cmd)
        SELECT id,CASE WHEN app='battery-monitor' THEN 'bms-watchdog' ELSE app END,name,cmd FROM old.process_identities", [])?;
    copy_table(&tx, "process_samples_v2", "id,sample_id,ts,pid,ppid,process_id,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self", "id,sample_id,ts,pid,ppid,process_id,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self")?;
    tx.execute("INSERT INTO sample_app_totals (id,sample_id,ts,app,watts)
        SELECT id,sample_id,ts,CASE WHEN app='battery-monitor' THEN 'bms-watchdog' ELSE app END,watts FROM old.sample_app_totals", [])?;
    copy_table(
        &tx,
        "sample_process_totals",
        "id,sample_id,ts,process_id,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count",
        "id,sample_id,ts,process_id,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count",
    )?;
    tx.execute("INSERT INTO sample_group_totals (id,sample_id,ts,app,child_name,cmd,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count)
        SELECT id,sample_id,ts,CASE WHEN app='battery-monitor' THEN 'bms-watchdog' ELSE app END,child_name,cmd,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count FROM old.sample_group_totals", [])?;
    copy_table(&tx, "environment_samples", "id,sample_id,ts,theme,theme_detail,brightness_percent,brightness_source,audio_playing,audio_detail,video_streaming,video_detail,net_rx_mbps,net_tx_mbps,focused_app,focused_title,focused_pid,lid_closed,lid_detail,screen_locked,screen_lock_detail,fan_rpm,fan_source", "id,sample_id,ts,theme,theme_detail,brightness_percent,brightness_source,audio_playing,audio_detail,video_streaming,video_detail,net_rx_mbps,net_tx_mbps,focused_app,focused_title,focused_pid,lid_closed,lid_detail,screen_locked,screen_lock_detail,fan_rpm,fan_source")?;
    tx.commit()?;
    db.execute("DETACH DATABASE old", [])?;
    println!("migrated {} -> {}", args.source.display(), dest.display());
    Ok(())
}

fn ensure_empty_destination(db: &Connection) -> Result<()> {
    let count: i64 = db.query_row("SELECT COUNT(*) FROM battery_samples", [], |r| r.get(0))?;
    if count > 0 {
        return Err(anyhow!(
            "destination DB already contains data; use --replace or an empty --data-dir"
        ));
    }
    Ok(())
}

fn copy_table(db: &Connection, table: &str, dest_cols: &str, source_cols: &str) -> Result<()> {
    db.execute(
        &format!("INSERT INTO {table} ({dest_cols}) SELECT {source_cols} FROM old.{table}"),
        [],
    )?;
    Ok(())
}
