use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::{
    cli::StartArgs,
    collect::*,
    config::Config,
    db::*,
    util::*,
};

pub(crate) struct App {
    pub(crate) cfg: Config,
    pub(crate) db: Connection,
    pub(crate) collector: Collector,
    pub(crate) db_path: PathBuf,
    pub(crate) last_retention_at: i64,
}

pub(crate) type SharedApp = Arc<Mutex<App>>;

fn is_browser_app(app: &str) -> bool {
    matches!(app, "Zen Browser" | "Firefox" | "Chrome/Chromium")
}

fn file_size(path: &PathBuf) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn normalize_focused_app_for_series(raw: &str, apps: &[String]) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let normalized = normalize_stored_app(trimmed);
    let lower = normalized.to_lowercase();
    if let Some(app) = apps.iter().find(|app| app.to_lowercase() == lower) {
        return app.clone();
    }
    if let Some(app) = apps.iter().find(|app| {
        let candidate = app.to_lowercase();
        candidate.len() >= 3 && (lower.contains(&candidate) || candidate.contains(&lower))
    }) {
        return app.clone();
    }

    normalized
}

#[derive(Clone, Default)]
pub(crate) struct Collector {
    pub(crate) self_pid: i32,
    pub(crate) cpu_count: usize,
    pub(crate) prev_total_jiffies: Option<u64>,
    pub(crate) prev_poll_ts: Option<i64>,
    pub(crate) prev_procs: HashMap<i32, ProcPrev>,
    pub(crate) prev_battery: Option<BatterySample>,
    pub(crate) prev_net: Option<NetCountersAt>,
    pub(crate) cached_theme: Option<(i64, String, String)>,
}

#[derive(Debug, Clone)]
pub(crate) struct BatterySample {
    pub(crate) ts: i64,
    pub(crate) on_battery: bool,
    pub(crate) status: String,
    pub(crate) capacity: Option<f64>,
    pub(crate) energy_wh: Option<f64>,
    pub(crate) power_w: Option<f64>,
    pub(crate) source: String,
}

#[derive(Clone)]
pub(crate) struct EnvironmentSample {
    pub(crate) sample_id: i64,
    pub(crate) ts: i64,
    pub(crate) theme: String,
    pub(crate) theme_detail: String,
    pub(crate) brightness_percent: Option<f64>,
    pub(crate) brightness_source: String,
    pub(crate) audio_playing: Option<bool>,
    pub(crate) audio_detail: String,
    pub(crate) video_streaming: Option<bool>,
    pub(crate) video_detail: String,
    pub(crate) net_rx_mbps: f64,
    pub(crate) net_tx_mbps: f64,
    pub(crate) focused_app: String,
    pub(crate) focused_title: String,
    pub(crate) focused_pid: Option<i64>,
    pub(crate) lid_closed: Option<bool>,
    pub(crate) lid_detail: String,
    pub(crate) screen_locked: Option<bool>,
    pub(crate) screen_lock_detail: String,
    pub(crate) fan_rpm: Option<f64>,
    pub(crate) fan_source: String,
    pub(crate) usb_power_source: Option<bool>,
    pub(crate) usb_power_w: Option<f64>,
    pub(crate) usb_power_detail: String,
}

#[derive(Clone)]
pub(crate) struct ProcNow {
    pub(crate) pid: i32,
    pub(crate) ppid: i32,
    pub(crate) name: String,
    pub(crate) app: String,
    pub(crate) cmd: String,
    pub(crate) ticks: u64,
    pub(crate) start_time: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
    pub(crate) rss_mb: f64,
    pub(crate) is_self: bool,
}

#[derive(Clone)]
pub(crate) struct ProcPrev {
    pub(crate) ticks: u64,
    pub(crate) start_time: u64,
    pub(crate) read_bytes: u64,
    pub(crate) write_bytes: u64,
}

#[derive(Clone)]
pub(crate) struct ProcRow {
    pub(crate) now: ProcNow,
    pub(crate) cpu_percent: f64,
    pub(crate) cpu_seconds: f64,
    pub(crate) io_mb: f64,
    pub(crate) score: f64,
    pub(crate) estimated_watts: f64,
}

#[derive(Clone, Copy)]
pub(crate) struct NetCountersAt {
    pub(crate) ts: i64,
    pub(crate) rx_bytes: u64,
    pub(crate) tx_bytes: u64,
}

#[derive(serde::Deserialize)]
pub(crate) struct SeriesQuery { pub(crate) hours: Option<f64>, pub(crate) top: Option<f64>, pub(crate) after_ts: Option<i64> }
#[derive(serde::Deserialize)]
pub(crate) struct GroupsQuery { pub(crate) hours: Option<f64> }
#[derive(serde::Deserialize)]
pub(crate) struct ProcessesQuery { pub(crate) sample_id: Option<i64> }

pub(crate) async fn start(args: StartArgs) -> Result<()> {
    let cfg = Config::from_args(args.runtime);
    fs::create_dir_all(&cfg.data_dir).with_context(|| format!("create {}", cfg.data_dir.display()))?;
    let db_path = cfg.data_dir.join("bms-watchdog.sqlite");
    let mut db = Connection::open(&db_path).with_context(|| format!("open {}", db_path.display()))?;
    init_db(&mut db)?;
    let prev_battery = load_latest_battery_from_db(&db).unwrap_or(None);
    let app = App {
        cfg: cfg.clone(),
        db,
        collector: Collector {
            self_pid: cfg.self_pid.unwrap_or(std::process::id() as i32),
            cpu_count: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).max(1),
            prev_battery,
            ..Collector::default()
        },
        db_path: db_path.clone(),
        last_retention_at: 0,
    };
    let shared: SharedApp = Arc::new(Mutex::new(app));
    {
        let mut app = shared.lock().map_err(|_| anyhow!("state lock poisoned"))?;
        println!("[bms-watchdog] db={}", app.db_path.display());
        println!(
            "[bms-watchdog] polling every {}s; proc={}; power={}; self pid={}",
            app.cfg.poll_interval.as_secs(),
            app.cfg.proc_root.display(),
            app.cfg.power_root.display(),
            app.collector.self_pid
        );
        app.poll_once().context("first poll failed")?;
    }

    if args.once {
        return Ok(());
    }

    let poll_shared = Arc::clone(&shared);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(cfg.poll_interval);
        interval.tick().await;
        loop {
            interval.tick().await;
            match poll_shared.lock() {
                Ok(mut app) => {
                    if let Err(err) = app.poll_once() {
                        eprintln!("[bms-watchdog] poll failed: {err:?}");
                    }
                }
                Err(_) => eprintln!("[bms-watchdog] poll skipped: state lock poisoned"),
            }
        }
    });

    if args.no_serve {
        tokio::signal::ctrl_c().await?;
        return Ok(());
    }

    crate::server::serve(shared).await
}

impl App {
    pub(crate) fn poll_once(&mut self) -> Result<()> {
        let ts = now_ms();
        let battery = read_battery(&self.cfg, &self.collector, ts);
        record_sleep_gap_if_needed(&self.cfg, &mut self.db, self.collector.prev_battery.as_ref(), &battery)?;
        let sample_id = insert_battery(&self.db, &battery)?;
        let should_collect = battery.on_battery || self.cfg.record_when_plugged || self.cfg.force_collect;

        let total_jiffies = read_total_jiffies(&self.cfg.proc_root);
        let procs = read_processes(&self.cfg, &self.collector);
        if should_collect {
            let rows = self.compute_proc_rows(procs.clone(), total_jiffies, battery.power_w)?;
            insert_process_rows(&mut self.db, sample_id, ts, &rows)?;
            let env = self.read_environment(sample_id, ts, &rows);
            insert_environment(&self.db, &env)?;
            let self_w = rows.iter().find(|r| r.now.is_self).map(|r| r.estimated_watts).unwrap_or(0.0);
            println!("[bms-watchdog] {} {} {} {} rows={} self={:.2}W", ts, if battery.on_battery { "unplugged" } else { "plugged" }, fmt_pct(battery.capacity), fmt_w(battery.power_w), rows.len(), self_w);
        } else {
            let env = self.read_environment(sample_id, ts, &[]);
            insert_environment(&self.db, &env)?;
            println!("[bms-watchdog] {} plugged {}; skipped process snapshot", ts, fmt_pct(battery.capacity));
        }
        self.collector.prev_total_jiffies = Some(total_jiffies);
        self.collector.prev_poll_ts = Some(ts);
        self.collector.prev_procs = procs.iter().map(|p| (p.pid, ProcPrev {
            ticks: p.ticks,
            start_time: p.start_time,
            read_bytes: p.read_bytes,
            write_bytes: p.write_bytes,
        })).collect();

        self.collector.prev_battery = Some(battery);
        if ts - self.last_retention_at > 3_600_000 {
            prune_old(&self.db, &self.cfg, ts)?;
            self.last_retention_at = ts;
        }
        Ok(())
    }

    fn current_baseline_watts(&self, total_power_w: f64) -> Result<f64> {
        if total_power_w <= 0.0 {
            return Ok(0.0);
        }
        if self.cfg.baseline_mode != "adaptive" {
            return Ok(self.cfg.baseline_watts.min(total_power_w));
        }
        let since = now_ms() - (self.cfg.baseline_lookback_hours * 3_600_000.0) as i64;
        let observed: Option<f64> = self.db.query_row(
            "SELECT MIN(power_w) FROM battery_samples WHERE on_battery=1 AND power_w > 0 AND ts >= ?",
            params![since],
            |row| row.get(0),
        ).optional()?.flatten();
        Ok(total_power_w.min(clamp(observed.unwrap_or(self.cfg.baseline_watts), self.cfg.baseline_min_watts, self.cfg.baseline_max_watts)))
    }

    fn compute_proc_rows(&self, procs: Vec<ProcNow>, total_jiffies: u64, total_power_w: Option<f64>) -> Result<Vec<ProcRow>> {
        let total_delta = self.collector.prev_total_jiffies.map(|p| total_jiffies.saturating_sub(p)).unwrap_or(0);
        let elapsed_sec = self.collector.prev_poll_ts.map(|p| ((now_ms() - p).max(1000) as f64) / 1000.0).unwrap_or(self.cfg.poll_interval.as_secs_f64());
        let mut rows: Vec<ProcRow> = procs.into_iter().map(|p| {
            let prev = self.collector.prev_procs.get(&p.pid);
            let same = prev.map(|x| x.start_time == p.start_time).unwrap_or(false);
            let delta_ticks = if same { p.ticks.saturating_sub(prev.unwrap().ticks) } else { 0 };
            let read_delta = if same { p.read_bytes.saturating_sub(prev.unwrap().read_bytes) } else { 0 };
            let write_delta = if same { p.write_bytes.saturating_sub(prev.unwrap().write_bytes) } else { 0 };
            let cpu_seconds = delta_ticks as f64 / self.cfg.clk_tck as f64;
            let cpu_percent = if total_delta > 0 {
                (delta_ticks as f64 / total_delta as f64) * self.collector.cpu_count as f64 * 100.0
            } else {
                (cpu_seconds / elapsed_sec) * 100.0
            };
            let io_mb = (read_delta + write_delta) as f64 / 1024.0 / 1024.0;
            let score = cpu_seconds + io_mb * 0.02;
            ProcRow { now: p, cpu_percent, cpu_seconds, io_mb, score, estimated_watts: 0.0 }
        }).filter(|r| r.score > 0.0 || r.now.is_self).collect();

        if self.cfg.max_processes_per_sample > 0 && rows.len() > self.cfg.max_processes_per_sample {
            rows.sort_by(|a, b| b.score.total_cmp(&a.score));
            let mut kept: Vec<ProcRow> = rows.iter().filter(|r| r.now.is_self).cloned().collect();
            for r in rows.into_iter().filter(|r| !r.now.is_self).take(self.cfg.max_processes_per_sample.saturating_sub(kept.len())) {
                kept.push(r);
            }
            rows = kept;
        }

        let power = total_power_w.unwrap_or(0.0).max(0.0);
        let baseline = self.current_baseline_watts(power)?;
        let dynamic = (power - baseline).max(0.0);
        let score_sum: f64 = rows.iter().map(|r| r.score).sum();
        if dynamic > 0.0 && score_sum > 0.0 {
            for row in &mut rows {
                row.estimated_watts = dynamic * (row.score / score_sum);
            }
        }
        rows.push(ProcRow {
            now: ProcNow {
                pid: 0,
                ppid: 0,
                name: "system-baseline".to_string(),
                app: "System / baseline".to_string(),
                cmd: format!("Estimated idle/platform baseline ({baseline:.2} W, {} mode)", self.cfg.baseline_mode),
                ticks: 0,
                start_time: 0,
                read_bytes: 0,
                write_bytes: 0,
                rss_mb: 0.0,
                is_self: false,
            },
            cpu_percent: 0.0,
            cpu_seconds: 0.0,
            io_mb: 0.0,
            score: 0.0,
            estimated_watts: baseline,
        });
        rows.sort_by(|a, b| b.estimated_watts.total_cmp(&a.estimated_watts));
        Ok(rows)
    }

    fn cached_theme(&mut self, ts: i64) -> (String, String) {
        const THEME_TTL_MS: i64 = 300_000;
        if let Some((cached_ts, theme, detail)) = &self.collector.cached_theme {
            if ts - *cached_ts < THEME_TTL_MS {
                return (theme.clone(), detail.clone());
            }
        }
        let theme = read_theme(&self.cfg);
        self.collector.cached_theme = Some((ts, theme.0.clone(), theme.1.clone()));
        theme
    }

    fn read_environment(&mut self, sample_id: i64, ts: i64, rows: &[ProcRow]) -> EnvironmentSample {
        let theme = self.cached_theme(ts);
        let brightness = read_brightness(&self.cfg);
        let proc_audio = read_audio_state(&self.cfg);
        let pactl_audio = read_pactl_audio_state();
        let audio_playing = pactl_audio.playing.or(proc_audio.playing);
        let browser_playing = pactl_audio.browser_playing;
        let audio_detail = if pactl_audio.playing.is_some() { format!("pactl: {}", pactl_audio.detail) } else { proc_audio.detail };
        let net = read_network_rates(&self.cfg, &mut self.collector, ts);
        let focused = read_focused_window(&self.cfg);
        let lid = read_lid_state(&self.cfg);
        let lock = read_screen_lock_state(&self.cfg);
        let fan = read_fan_speed(&self.cfg);
        let usb_power = read_usb_power(&self.cfg);
        let (browser_watts, browser_cpu) = rows.iter()
            .filter(|r| is_browser_app(&r.now.app))
            .fold((0.0, 0.0), |(w, c), r| (w + r.estimated_watts, c + r.cpu_percent));
        let video = detect_video_streaming(&self.cfg, net.rx_mbps, browser_watts, browser_cpu, &focused, audio_playing, browser_playing, &audio_detail);
        EnvironmentSample {
            sample_id,
            ts,
            theme: theme.0,
            theme_detail: theme.1,
            brightness_percent: brightness.0,
            brightness_source: brightness.1,
            audio_playing,
            audio_detail,
            video_streaming: Some(video.0),
            video_detail: video.1,
            net_rx_mbps: net.rx_mbps,
            net_tx_mbps: net.tx_mbps,
            focused_app: focused.app,
            focused_title: focused.title,
            focused_pid: focused.pid,
            lid_closed: lid.0,
            lid_detail: lid.1,
            screen_locked: lock.0,
            screen_lock_detail: lock.1,
            fan_rpm: fan.0,
            fan_source: fan.1,
            usb_power_source: usb_power.sourcing,
            usb_power_w: usb_power.watts,
            usb_power_detail: usb_power.detail,
        }
    }

    pub(crate) fn api_status(&self) -> Result<Value> {
        let latest_battery = query_one_json(&self.db, "SELECT id,ts,on_battery,status,capacity,energy_wh,power_w,source FROM battery_samples ORDER BY ts DESC LIMIT 1", &[])?;
        let latest_environment = query_one_json(&self.db, "SELECT ts,theme,theme_detail,brightness_percent,brightness_source,audio_playing,audio_detail,video_streaming,video_detail,net_rx_mbps,net_tx_mbps,focused_app,focused_title,focused_pid,lid_closed,lid_detail,screen_locked,screen_lock_detail,fan_rpm,fan_source,usb_power_source,usb_power_w,usb_power_detail FROM environment_samples ORDER BY ts DESC LIMIT 1", &[])?;
        let self_latest_sql = format!("SELECT ts,pid,app,cpu_percent,io_mb,rss_mb,estimated_watts FROM ({}) WHERE is_self=1 ORDER BY ts DESC LIMIT 1", PROCESS_ROWS_VIEW_SQL);
        let self_latest = query_one_json(&self.db, &self_latest_sql, &[])?;
        let legacy_rows: i64 = self.db.query_row("SELECT COUNT(*) FROM process_samples", [], |r| r.get(0))?;
        let v2_rows: i64 = self.db.query_row("SELECT COUNT(*) FROM process_samples_v2", [], |r| r.get(0))?;
        Ok(json!({
            "latestBattery": latest_battery,
            "latestEnvironment": latest_environment,
            "selfLatest": self_latest,
            "dischargeEstimate": self.compute_battery_rate_estimate()?,
            "processRows": legacy_rows + v2_rows,
            "dbStats": self.db_stats(legacy_rows + v2_rows)?,
            "config": {
                "pollSeconds": self.cfg.poll_interval.as_secs(),
                "baselineMode": self.cfg.baseline_mode,
                "baselineWatts": self.cfg.baseline_watts,
                "baselineMinWatts": self.cfg.baseline_min_watts,
                "baselineMaxWatts": self.cfg.baseline_max_watts
            }
        }))
    }

    fn db_stats(&self, process_rows: i64) -> Result<Value> {
        let (sample_count, min_ts, max_ts): (i64, Option<i64>, Option<i64>) = self.db.query_row(
            "SELECT COUNT(*), MIN(ts), MAX(ts) FROM battery_samples",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let span_days = match (min_ts, max_ts) {
            (Some(a), Some(b)) if b >= a => Some((b - a) as f64 / 86_400_000.0),
            _ => None,
        };
        let mut bytes = file_size(&self.db_path);
        bytes += file_size(&PathBuf::from(format!("{}-wal", self.db_path.display())));
        bytes += file_size(&PathBuf::from(format!("{}-shm", self.db_path.display())));
        Ok(json!({
            "sampleCount": sample_count,
            "processRows": process_rows,
            "firstTs": min_ts,
            "lastTs": max_ts,
            "spanDays": span_days,
            "sizeBytes": bytes,
            "path": self.db_path.display().to_string()
        }))
    }

    fn compute_battery_rate_estimate(&self) -> Result<Value> {
        let latest: Option<(i64, i64, String, f64)> = self.db.query_row(
            "SELECT ts,on_battery,status,capacity FROM battery_samples WHERE capacity IS NOT NULL ORDER BY ts DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).optional()?;
        let Some((_ts, on_battery, status, capacity)) = latest else {
            return Ok(json!({"mode":"unknown","percentPerHour":null,"hoursRemaining":null,"hoursToFull":null,"detail":"no battery samples"}));
        };
        let latest_charging = on_battery == 0 && status.to_lowercase().contains("charging");
        let mode = if on_battery != 0 { "discharging" } else if latest_charging { "charging" } else { "plugged" };
        let mut stmt = self.db.prepare("SELECT ts,on_battery,status,capacity FROM battery_samples WHERE capacity IS NOT NULL AND ts >= ? ORDER BY ts")?;
        for window_minutes in [30_i64, 120, 360] {
            let since = now_ms() - window_minutes * 60_000;
            let rows = stmt.query_map(params![since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, f64>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let matching: Vec<_> = rows.into_iter().filter(|(_, ob, st, _)| match mode {
                "discharging" => *ob != 0,
                "charging" => *ob == 0 && st.to_lowercase().contains("charging"),
                _ => *ob == 0,
            }).collect();
            if matching.len() < 2 { continue; }
            let first = matching.first().unwrap();
            let last = matching.last().unwrap();
            let hours = ((last.0 - first.0) as f64 / 3_600_000.0).max(1.0 / 60.0);
            let raw_rate = (last.3 - first.3) / hours;
            let percent_per_hour = if mode == "discharging" { -raw_rate } else { raw_rate };
            if percent_per_hour <= 0.0 { continue; }
            return Ok(json!({
                "mode": mode,
                "percentPerHour": percent_per_hour,
                "hoursRemaining": if mode == "discharging" { json!(capacity / percent_per_hour) } else { Value::Null },
                "hoursToFull": if mode == "charging" { json!((100.0 - capacity).max(0.0) / percent_per_hour) } else { Value::Null },
                "detail": format!("{} samples over {:.0} min", matching.len(), hours * 60.0)
            }));
        }
        Ok(json!({
            "mode": mode,
            "percentPerHour": null,
            "hoursRemaining": null,
            "hoursToFull": null,
            "detail": if mode == "charging" { "estimating charge rate" } else if mode == "discharging" { "estimating discharge rate" } else { "plugged" }
        }))
    }

    pub(crate) fn api_series(&self, q: SeriesQuery) -> Result<Value> {
        let hours = clamp(q.hours.unwrap_or(8.0), 1.0, 24.0 * 30.0);
        let top = clamp(q.top.unwrap_or(12.0), 1.0, 50.0) as usize;
        let since = now_ms() - (hours * 3_600_000.0) as i64;
        let after_ts = q.after_ts.filter(|v| *v > since);
        let points_since = after_ts.unwrap_or(since);

        let mut stmt = self.db.prepare("SELECT app, SUM(watts) FROM sample_app_totals WHERE ts >= ? GROUP BY app ORDER BY SUM(watts) DESC")?;
        let raw = stmt.query_map(params![since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut totals: HashMap<String, f64> = HashMap::new();
        for (app, total) in raw {
            *totals.entry(normalize_stored_app(&app)).or_default() += total;
        }
        let mut sorted: Vec<_> = totals.iter().collect();
        sorted.sort_by(|a, b| b.1.total_cmp(a.1));
        let mut apps: Vec<String> = sorted.into_iter().take(top).map(|(a, _)| a.clone()).collect();
        for must in ["bms-watchdog", "System / baseline"] {
            if totals.get(must).copied().unwrap_or(0.0) > 0.0 && !apps.iter().any(|a| a == must) {
                apps.push(must.to_string());
            }
        }

        let mut battery_rows = self.series_rows_after(points_since, false)?;
        let mut drop_first = false;
        if let Some(after) = after_ts {
            if !battery_rows.is_empty() {
                if let Some(prev) = self.series_prev_row(after)? {
                    battery_rows.insert(0, prev);
                    drop_first = true;
                }
            }
        } else {
            battery_rows = self.series_rows_after(since, true)?;
        }
        let mut points = Vec::new();
        for idx in 0..battery_rows.len() {
            let b = &battery_rows[idx];
            let prev = idx.checked_sub(1).and_then(|i| battery_rows.get(i));
            let gap_before = prev.map(|p| b.ts - p.ts >= self.cfg.suspend_gap.as_millis() as i64).unwrap_or(false);
            let battery_rate = if let Some(p) = prev {
                match (p.capacity, b.capacity) {
                    (Some(a), Some(c)) => Some((c - a) / (((b.ts - p.ts) as f64 / 3_600_000.0).max(1.0 / 3600.0))),
                    _ => None,
                }
            } else { None };
            points.push(json!({
                "sampleId": b.id,
                "ts": b.ts,
                "batteryPercent": b.capacity,
                "batteryRatePctPerHour": battery_rate,
                "gapBefore": gap_before,
                "gapDurationSec": if gap_before { prev.map(|p| (b.ts - p.ts) as f64 / 1000.0).unwrap_or(0.0) } else { 0.0 },
                "totalWatts": b.power_w,
                "onBattery": b.on_battery != 0,
                "charging": b.on_battery == 0 && b.status.to_lowercase().contains("charging"),
                "status": b.status,
                "focusedApp": normalize_focused_app_for_series(b.focused_app.as_deref().unwrap_or_default(), &apps),
                "focusedTitle": b.focused_title.clone().unwrap_or_default(),
                "focusedPid": b.focused_pid,
                "lidClosed": opt_bool(b.lid_closed),
                "lidDetail": b.lid_detail.clone().unwrap_or_default(),
                "screenLocked": opt_bool(b.screen_locked),
                "screenLockDetail": b.screen_lock_detail.clone().unwrap_or_default(),
                "brightnessPercent": b.brightness_percent,
                "brightnessSource": b.brightness_source.clone().unwrap_or_default(),
                "theme": b.theme.clone().unwrap_or_else(|| "unknown".to_string()),
                "themeDetail": b.theme_detail.clone().unwrap_or_default(),
                "videoStreaming": opt_bool(b.video_streaming),
                "videoDetail": b.video_detail.clone().unwrap_or_default(),
                "netRxMbps": b.net_rx_mbps,
                "netTxMbps": b.net_tx_mbps,
                "usbPowerSource": opt_bool(b.usb_power_source),
                "usbPowerW": b.usb_power_w,
                "usbPowerDetail": b.usb_power_detail.clone().unwrap_or_default(),
                "apps": {}
            }));
        }
        if drop_first && !points.is_empty() {
            points.remove(0);
        }
        let mut by_sample = HashMap::new();
        for (idx, p) in points.iter().enumerate() {
            if let Some(id) = p.get("sampleId").and_then(Value::as_i64) {
                by_sample.insert(id, idx);
            }
        }
        let mut other_total = 0.0;
        let mut stmt = self.db.prepare("SELECT sample_id, app, watts FROM sample_app_totals WHERE ts > ?")?;
        let agg = stmt.query_map(params![points_since], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, f64>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (sample_id, app_raw, watts) in agg {
            let Some(idx) = by_sample.get(&sample_id).copied() else { continue; };
            let app = normalize_stored_app(&app_raw);
            if let Some(obj) = points[idx].as_object_mut() {
                let apps_obj = obj.get_mut("apps").and_then(Value::as_object_mut).unwrap();
                let key = if apps.iter().any(|a| a == &app) { app } else { other_total += watts; "Other".to_string() };
                let prev = apps_obj.get(&key).and_then(Value::as_f64).unwrap_or(0.0);
                apps_obj.insert(key, json!(prev + watts));
            }
        }
        let final_apps = if other_total > 0.0 { apps.into_iter().chain(["Other".to_string()]).collect::<Vec<_>>() } else { apps };
        let sleep_events = query_all_json(&self.db, "SELECT start_ts,end_ts,duration_sec,kind,start_capacity,end_capacity,capacity_delta,start_energy_wh,end_energy_wh,energy_delta_wh,avg_power_w,avg_percent_per_hour FROM sleep_events WHERE end_ts > ? ORDER BY start_ts", &[&points_since])?;
        Ok(json!({
            "apps": final_apps,
            "points": points,
            "sleepEvents": sleep_events,
            "suspendGapSeconds": self.cfg.suspend_gap.as_secs(),
            "incremental": after_ts.is_some(),
            "since": since,
            "afterTs": after_ts
        }))
    }

    fn series_rows_after(&self, ts: i64, inclusive: bool) -> Result<Vec<SeriesRow>> {
        let op = if inclusive { ">=" } else { ">" };
        let sql = format!("SELECT b.id,b.ts,b.capacity,b.power_w,b.on_battery,b.status,
            e.focused_app,e.focused_title,e.focused_pid,e.lid_closed,e.lid_detail,e.screen_locked,e.screen_lock_detail,
            e.brightness_percent,e.brightness_source,e.theme,e.theme_detail,e.video_streaming,e.video_detail,e.net_rx_mbps,e.net_tx_mbps,
            e.usb_power_source,e.usb_power_w,e.usb_power_detail
          FROM battery_samples b LEFT JOIN environment_samples e ON e.sample_id=b.id WHERE b.ts {op} ? ORDER BY b.ts");
        query_series_rows(&self.db, &sql, ts)
    }

    fn series_prev_row(&self, ts: i64) -> Result<Option<SeriesRow>> {
        let sql = "SELECT b.id,b.ts,b.capacity,b.power_w,b.on_battery,b.status,
            e.focused_app,e.focused_title,e.focused_pid,e.lid_closed,e.lid_detail,e.screen_locked,e.screen_lock_detail,
            e.brightness_percent,e.brightness_source,e.theme,e.theme_detail,e.video_streaming,e.video_detail,e.net_rx_mbps,e.net_tx_mbps,
            e.usb_power_source,e.usb_power_w,e.usb_power_detail
          FROM battery_samples b LEFT JOIN environment_samples e ON e.sample_id=b.id WHERE b.ts <= ? ORDER BY b.ts DESC LIMIT 1";
        Ok(query_series_rows(&self.db, sql, ts)?.into_iter().next())
    }

    pub(crate) fn api_groups(&self, q: GroupsQuery) -> Result<Value> {
        let hours = clamp(q.hours.unwrap_or(8.0), 1.0, 24.0 * 30.0);
        let since = now_ms() - (hours * 3_600_000.0) as i64;
        let sample_count: i64 = self.db.query_row("SELECT COUNT(*) FROM sample_app_totals WHERE ts >= ? AND app='System / baseline'", params![since], |r| r.get(0)).unwrap_or(0).max(1);
        let sampled_hours = sample_count as f64 * self.cfg.poll_interval.as_secs_f64() / 3600.0;
        let rows = query_group_rows(&self.db, since)?;
        let mut groups: HashMap<String, GroupAgg> = HashMap::new();
        for r in rows {
            let app = normalize_stored_app(&r.app);
            let g = groups.entry(app.clone()).or_insert_with(|| GroupAgg { app: app.clone(), ..Default::default() });
            g.watt_samples += r.watts;
            g.cpu_seconds += r.cpu_seconds;
            g.io_mb += r.io_mb;
            g.rss_mb_sum += r.rss_mb_sum;
            g.rss_rows += r.rss_rows;
            g.rows += r.rows;
            let c = g.children.entry(r.name.clone()).or_insert_with(|| ChildAgg { name: r.name.clone(), cmd: r.cmd.clone(), ..Default::default() });
            c.watt_samples += r.watts;
            c.cpu_seconds += r.cpu_seconds;
            c.io_mb += r.io_mb;
            c.rss_mb_sum += r.rss_mb_sum;
            c.rss_rows += r.rss_rows;
            c.rows += r.rows;
            c.samples += r.samples;
        }
        let mut output = Vec::new();
        for g in groups.values() {
            let avg = g.watt_samples / sample_count as f64;
            let mut children: Vec<Value> = g.children.values().map(|c| {
                let cavg = c.watt_samples / sample_count as f64;
                json!({
                    "name": c.name,
                    "cmd": c.cmd,
                    "wattSamples": c.watt_samples,
                    "avgWatts": cavg,
                    "wh": cavg * sampled_hours,
                    "cpuSeconds": c.cpu_seconds,
                    "ioMb": c.io_mb,
                    "rssMb": if c.rss_rows > 0 { c.rss_mb_sum / c.rss_rows as f64 } else { 0.0 },
                    "rows": c.rows,
                    "samples": c.samples
                })
            }).collect();
            children.sort_by(|a, b| b.get("wattSamples").and_then(Value::as_f64).unwrap_or(0.0).total_cmp(&a.get("wattSamples").and_then(Value::as_f64).unwrap_or(0.0)));
            output.push(json!({
                "app": g.app,
                "wattSamples": g.watt_samples,
                "avgWatts": avg,
                "wh": avg * sampled_hours,
                "cpuSeconds": g.cpu_seconds,
                "ioMb": g.io_mb,
                "rssMb": if g.rss_rows > 0 { g.rss_mb_sum / g.rss_rows as f64 } else { 0.0 },
                "rows": g.rows,
                "children": children
            }));
        }
        output.sort_by(|a, b| b.get("wattSamples").and_then(Value::as_f64).unwrap_or(0.0).total_cmp(&a.get("wattSamples").and_then(Value::as_f64).unwrap_or(0.0)));
        Ok(json!({ "hours": hours, "sampleCount": sample_count, "sampledHours": sampled_hours, "groups": output }))
    }

    pub(crate) fn api_processes(&self, q: ProcessesQuery) -> Result<Value> {
        let sample_id = if let Some(id) = q.sample_id {
            Some(id)
        } else {
            let sql = format!("SELECT sample_id FROM ({}) ORDER BY ts DESC LIMIT 1", PROCESS_ROWS_VIEW_SQL);
            self.db.query_row(&sql, [], |r| r.get(0)).optional()?
        };
        let Some(sample_id) = sample_id else { return Ok(json!({"sampleId": null, "rows": []})); };
        let sql = format!("SELECT ts,pid,ppid,name,app,cmd,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self FROM ({}) WHERE sample_id = ? ORDER BY estimated_watts DESC LIMIT 200", PROCESS_ROWS_VIEW_SQL);
        let rows = query_all_json(&self.db, &sql, &[&sample_id])?;
        Ok(json!({"sampleId": sample_id, "rows": rows}))
    }
}

