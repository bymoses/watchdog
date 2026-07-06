use std::{env, fs, process::Command};

use anyhow::{anyhow, Context, Result};

use crate::{
    cli::{InstallArgs, InstallTarget, SystemctlUnitArgs},
    config::{default_user_unit_dir, Config},
    util::shell_join,
};

pub(crate) fn install(args: InstallArgs) -> Result<()> {
    match args.target {
        InstallTarget::SystemctlUnit(args) => install_systemctl_unit(args),
    }
}

fn install_systemctl_unit(args: SystemctlUnitArgs) -> Result<()> {
    let exe = env::current_exe().context("locate current executable")?;
    let cfg = Config::from_args(args.runtime.clone());
    let unit_dir = args.unit_dir.unwrap_or_else(default_user_unit_dir);
    fs::create_dir_all(&unit_dir).with_context(|| format!("create {}", unit_dir.display()))?;
    let unit_path = unit_dir.join("bms-watchdog.service");
    let options: [(&str, String); 18] = [
        ("--data-dir", cfg.data_dir.display().to_string()),
        ("--host", cfg.host.clone()),
        ("--port", cfg.port.to_string()),
        ("--poll-interval-seconds", cfg.poll_interval.as_secs().to_string()),
        ("--proc-root", cfg.proc_root.display().to_string()),
        ("--power-root", cfg.power_root.display().to_string()),
        ("--host-config-dir", cfg.host_config_dir.display().to_string()),
        ("--retention-days", cfg.retention_days.to_string()),
        ("--detailed-process-retention-days", cfg.detailed_process_retention_days.to_string()),
        ("--baseline-mode", cfg.baseline_mode.clone()),
        ("--baseline-watts", cfg.baseline_watts.to_string()),
        ("--baseline-min-watts", cfg.baseline_min_watts.to_string()),
        ("--baseline-max-watts", cfg.baseline_max_watts.to_string()),
        ("--baseline-lookback-hours", cfg.baseline_lookback_hours.to_string()),
        ("--suspend-gap-seconds", cfg.suspend_gap.as_secs().to_string()),
        ("--video-rx-mbps-threshold", cfg.video_rx_mbps_threshold.to_string()),
        ("--max-processes-per-sample", cfg.max_processes_per_sample.to_string()),
        ("--clk-tck", cfg.clk_tck.to_string()),
    ];
    let mut cmd = vec![exe.display().to_string(), "start".to_string()];
    for (flag, value) in options {
        cmd.push(flag.to_string());
        cmd.push(value);
    }
    if args.no_serve { cmd.push("--no-serve".to_string()); }
    if cfg.record_when_plugged { cmd.push("--record-when-plugged".to_string()); }
    if cfg.force_collect { cmd.push("--force-collect".to_string()); }
    if cfg.redact_browser_titles { cmd.push("--redact-browser-titles".to_string()); }
    if !cfg.redact_private_browser_titles { cmd.push("--allow-private-browser-titles".to_string()); }
    if let Some(file) = &cfg.focused_window_file {
        cmd.push("--focused-window-file".to_string());
        cmd.push(file.display().to_string());
    }
    let unit = format!(
        "[Unit]\nDescription=Battery/process monitor\nAfter=default.target\n\n[Service]\nType=simple\nExecStart={}\nRestart=on-failure\nRestartSec=5\n\n[Install]\nWantedBy=default.target\n",
        shell_join(&cmd)
    );
    fs::write(&unit_path, unit).with_context(|| format!("write {}", unit_path.display()))?;
    println!("wrote {}", unit_path.display());
    if !args.dry_run {
        run_systemctl(&["--user", "daemon-reload"])?;
        run_systemctl(&["--user", "enable", "--now", "bms-watchdog.service"])?;
    }
    Ok(())
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .args(args)
        .status()
        .with_context(|| format!("run systemctl {}", args.join(" ")))?;
    if !status.success() {
        return Err(anyhow!("systemctl {} exited with {status}", args.join(" ")));
    }
    Ok(())
}
