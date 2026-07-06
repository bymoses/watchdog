use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bms-watchdog", version, about = "Local battery/process monitor")]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Commands,
}

#[derive(Subcommand)]
pub(crate) enum Commands {
    Start(StartArgs),
    Install(InstallArgs),
    Migrate(MigrateArgs),
}

#[derive(Args, Clone)]
pub(crate) struct StartArgs {
    #[command(flatten)]
    pub(crate) runtime: RuntimeArgs,
    #[arg(long, help = "Collect without serving the UI/API")]
    pub(crate) no_serve: bool,
    #[arg(long, help = "Run one collection pass and exit")]
    pub(crate) once: bool,
}

#[derive(Args)]
pub(crate) struct InstallArgs {
    #[command(subcommand)]
    pub(crate) target: InstallTarget,
}

#[derive(Args)]
pub(crate) struct MigrateArgs {
    #[command(subcommand)]
    pub(crate) target: MigrateTarget,
}

#[derive(Subcommand)]
pub(crate) enum MigrateTarget {
    #[command(name = "old-db")]
    OldDb(OldDbArgs),
}

#[derive(Args)]
pub(crate) struct OldDbArgs {
    #[arg(long, value_name = "FILE")]
    pub(crate) source: PathBuf,
    #[arg(long, value_name = "DIR")]
    pub(crate) data_dir: Option<PathBuf>,
    #[arg(long, help = "Delete the destination DB before migrating")]
    pub(crate) replace: bool,
}

#[derive(Subcommand)]
pub(crate) enum InstallTarget {
    #[command(name = "systemctl-unit")]
    SystemctlUnit(SystemctlUnitArgs),
}

#[derive(Args, Clone)]
pub(crate) struct SystemctlUnitArgs {
    #[command(flatten)]
    pub(crate) runtime: RuntimeArgs,
    #[arg(long)]
    pub(crate) no_serve: bool,
    #[arg(long, help = "Write the unit but do not run systemctl")]
    pub(crate) dry_run: bool,
    #[arg(long, value_name = "DIR", help = "Override user systemd unit directory")]
    pub(crate) unit_dir: Option<PathBuf>,
}

#[derive(Args, Clone)]
pub(crate) struct RuntimeArgs {
    #[arg(long, value_name = "DIR")]
    pub(crate) data_dir: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1")]
    pub(crate) host: String,
    #[arg(long, default_value_t = 24923)]
    pub(crate) port: u16,
    #[arg(long, default_value_t = 30)]
    pub(crate) poll_interval_seconds: u64,
    #[arg(long, default_value = "/proc")]
    pub(crate) proc_root: PathBuf,
    #[arg(long, default_value = "/sys/class/power_supply")]
    pub(crate) power_root: PathBuf,
    #[arg(long, value_name = "DIR")]
    pub(crate) host_config_dir: Option<PathBuf>,
    #[arg(long, value_name = "FILE")]
    pub(crate) focused_window_file: Option<PathBuf>,
    #[arg(long)]
    pub(crate) record_when_plugged: bool,
    #[arg(long)]
    pub(crate) force_collect: bool,
    #[arg(long, default_value_t = 365, help = "Days to keep battery/environment/app aggregate history")]
    pub(crate) retention_days: i64,
    #[arg(long, default_value_t = 2, help = "Days to keep detailed per-process snapshots")]
    pub(crate) detailed_process_retention_days: i64,
    #[arg(long, default_value = "adaptive")]
    pub(crate) baseline_mode: String,
    #[arg(long, default_value_t = 4.0)]
    pub(crate) baseline_watts: f64,
    #[arg(long, default_value_t = 2.0)]
    pub(crate) baseline_min_watts: f64,
    #[arg(long, default_value_t = 6.0)]
    pub(crate) baseline_max_watts: f64,
    #[arg(long, default_value_t = 24.0)]
    pub(crate) baseline_lookback_hours: f64,
    #[arg(long, default_value_t = 120)]
    pub(crate) suspend_gap_seconds: u64,
    #[arg(long, default_value_t = 1.0)]
    pub(crate) video_rx_mbps_threshold: f64,
    #[arg(long, default_value_t = 0)]
    pub(crate) max_processes_per_sample: usize,
    #[arg(long, default_value_t = 100)]
    pub(crate) clk_tck: u64,
    #[arg(long, help = "Never store browser focused-window titles")]
    pub(crate) redact_browser_titles: bool,
    #[arg(long, help = "Store private/incognito browser titles verbatim")]
    pub(crate) allow_private_browser_titles: bool,
    #[arg(long, hide = true)]
    pub(crate) self_pid: Option<i32>,
}
