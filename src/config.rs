use std::{env, path::PathBuf, time::Duration};

use crate::cli::RuntimeArgs;

#[derive(Clone)]
pub(crate) struct Config {
    pub(crate) data_dir: PathBuf,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) poll_interval: Duration,
    pub(crate) proc_root: PathBuf,
    pub(crate) power_root: PathBuf,
    pub(crate) host_config_dir: PathBuf,
    pub(crate) focused_window_file: Option<PathBuf>,
    pub(crate) record_when_plugged: bool,
    pub(crate) force_collect: bool,
    pub(crate) retention_days: i64,
    pub(crate) detailed_process_retention_days: i64,
    pub(crate) baseline_mode: String,
    pub(crate) baseline_watts: f64,
    pub(crate) baseline_min_watts: f64,
    pub(crate) baseline_max_watts: f64,
    pub(crate) baseline_lookback_hours: f64,
    pub(crate) suspend_gap: Duration,
    pub(crate) video_rx_mbps_threshold: f64,
    pub(crate) max_processes_per_sample: usize,
    pub(crate) clk_tck: u64,
    pub(crate) redact_browser_titles: bool,
    pub(crate) redact_private_browser_titles: bool,
    pub(crate) self_pid: Option<i32>,
}

impl Config {
    pub(crate) fn from_args(args: RuntimeArgs) -> Self {
        Self {
            data_dir: args.data_dir.unwrap_or_else(default_data_dir),
            host: args.host,
            port: args.port,
            poll_interval: Duration::from_secs(args.poll_interval_seconds.max(1)),
            proc_root: args.proc_root,
            power_root: args.power_root,
            host_config_dir: args.host_config_dir.unwrap_or_else(default_config_dir),
            focused_window_file: args.focused_window_file,
            record_when_plugged: args.record_when_plugged,
            force_collect: args.force_collect,
            retention_days: args.retention_days.max(1),
            detailed_process_retention_days: args.detailed_process_retention_days.max(1),
            baseline_mode: args.baseline_mode,
            baseline_watts: args.baseline_watts,
            baseline_min_watts: args.baseline_min_watts,
            baseline_max_watts: args.baseline_max_watts,
            baseline_lookback_hours: args.baseline_lookback_hours,
            suspend_gap: Duration::from_secs(args.suspend_gap_seconds.max(1)),
            video_rx_mbps_threshold: args.video_rx_mbps_threshold,
            max_processes_per_sample: args.max_processes_per_sample,
            clk_tck: args.clk_tck.max(1),
            redact_browser_titles: args.redact_browser_titles,
            redact_private_browser_titles: !args.allow_private_browser_titles,
            self_pid: args.self_pid,
        }
    }
}

pub(crate) fn default_data_dir() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("data"))
        .join("bms-watchdog")
}

pub(crate) fn default_config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

pub(crate) fn default_user_unit_dir() -> PathBuf {
    default_config_dir().join("systemd/user")
}
