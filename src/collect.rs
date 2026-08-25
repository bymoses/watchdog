use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;

use crate::{
    app::{BatterySample, Collector, NetCountersAt, ProcNow},
    config::Config,
    util::*,
};

const BROWSER_KEYWORDS: &[&str] = &[
    "zen", "firefox", "chrome", "chromium", "brave", "vivaldi", "edge", "browser",
];

pub(crate) fn read_battery(cfg: &Config, collector: &Collector, ts: i64) -> BatterySample {
    if !cfg.power_root.exists() {
        return BatterySample {
            ts,
            on_battery: false,
            status: "no power_supply".into(),
            capacity: None,
            energy_wh: None,
            power_w: None,
            source: cfg.power_root.display().to_string(),
        };
    }
    let entries = safe_readdir(&cfg.power_root);
    let mut battery_dirs = Vec::new();
    for name in &entries {
        let dir = cfg.power_root.join(name);
        if safe_read_trim(dir.join("type")) == "Battery" || name.starts_with("BAT") {
            battery_dirs.push(dir);
        }
    }
    let ac_online = entries
        .iter()
        .map(|n| cfg.power_root.join(n))
        .filter(|d| !battery_dirs.contains(d))
        .any(|d| safe_read_trim(d.join("online")) == "1");
    if battery_dirs.is_empty() {
        return BatterySample {
            ts,
            on_battery: cfg.force_collect,
            status: if ac_online { "AC" } else { "no battery" }.into(),
            capacity: None,
            energy_wh: None,
            power_w: None,
            source: cfg.power_root.display().to_string(),
        };
    }
    let mut energy_wh = 0.0;
    let mut full_wh = 0.0;
    let mut power_w = 0.0;
    let mut have_energy = false;
    let mut have_power = false;
    let mut capacities = Vec::new();
    let mut statuses = Vec::new();
    for dir in &battery_dirs {
        let status =
            nonempty(safe_read_trim(dir.join("status"))).unwrap_or_else(|| "Unknown".into());
        statuses.push(status);
        if let Some(c) = read_num(dir.join("capacity")) {
            capacities.push(c);
        }
        if let Some(e) = read_energy_wh(dir, "now") {
            energy_wh += e;
            have_energy = true;
        }
        if let Some(e) = read_energy_wh(dir, "full") {
            full_wh += e;
        }
        if let Some(p) = read_power_w(dir) {
            power_w += p;
            have_power = true;
        }
    }
    let capacity = if have_energy && full_wh > 0.0 {
        Some(energy_wh / full_wh * 100.0)
    } else if capacities.is_empty() {
        None
    } else {
        Some(capacities.iter().sum::<f64>() / capacities.len() as f64)
    };
    let mut status_seen = Vec::new();
    for s in statuses {
        if !status_seen.contains(&s) {
            status_seen.push(s);
        }
    }
    let status = status_seen.join(", ");
    let mut on_battery = status_seen
        .iter()
        .any(|s| s.eq_ignore_ascii_case("discharging"));
    if !on_battery && !ac_online {
        on_battery = !status_seen
            .iter()
            .all(|s| matches!(s.to_lowercase().as_str(), "full" | "charging"));
    }
    let mut final_power = if have_power { Some(power_w) } else { None };
    if final_power.is_none() && on_battery && have_energy {
        if let Some(prev) = &collector.prev_battery {
            if let Some(prev_e) = prev.energy_wh {
                let dt_h = ((ts - prev.ts) as f64 / 3_600_000.0).max(1.0 / 3600.0);
                let delta = prev_e - energy_wh;
                if delta >= 0.0 {
                    final_power = Some(delta / dt_h);
                }
            }
        }
    }
    BatterySample {
        ts,
        on_battery,
        status,
        capacity,
        energy_wh: have_energy.then_some(energy_wh),
        power_w: final_power,
        source: battery_dirs
            .iter()
            .filter_map(|d| d.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(","),
    }
}

fn read_energy_wh(dir: &Path, kind: &str) -> Option<f64> {
    if let Some(e) = read_num(dir.join(format!("energy_{kind}"))) {
        return Some(e / 1_000_000.0);
    }
    let charge = read_num(dir.join(format!("charge_{kind}")))?;
    let voltage = read_num(dir.join("voltage_now"))?;
    Some(charge * voltage / 1_000_000_000_000.0)
}
fn read_power_w(dir: &Path) -> Option<f64> {
    if let Some(p) = read_num(dir.join("power_now")) {
        return Some(p.abs() / 1_000_000.0);
    }
    let current = read_num(dir.join("current_now"))?;
    let voltage = read_num(dir.join("voltage_now"))?;
    Some((current * voltage).abs() / 1_000_000_000_000.0)
}

pub(crate) fn read_processes(cfg: &Config, collector: &Collector) -> Vec<ProcNow> {
    let mut out = Vec::new();
    for name in safe_readdir(&cfg.proc_root) {
        if !name.as_bytes().iter().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let pid = name.parse::<i32>().unwrap_or(0);
        let dir = cfg.proc_root.join(&name);
        let stat_text = safe_read_trim(dir.join("stat"));
        let Some(parsed) = parse_proc_stat(&stat_text) else {
            continue;
        };
        let comm = nonempty(safe_read_trim(dir.join("comm"))).unwrap_or(parsed.comm);
        let cmd = nonempty(read_cmdline(dir.join("cmdline"))).unwrap_or_else(|| comm.clone());
        let io = read_proc_io(dir.join("io"));
        let is_self = pid == collector.self_pid || looks_like_self(cfg, pid, &cmd, &comm);
        out.push(ProcNow {
            pid,
            ppid: parsed.ppid,
            name: comm.chars().take(80).collect(),
            app: String::new(),
            cmd: cmd.chars().take(300).collect(),
            ticks: parsed.utime + parsed.stime,
            start_time: parsed.start_time,
            read_bytes: io.0,
            write_bytes: io.1,
            rss_mb: parsed.rss_pages as f64 * 4096.0 / 1024.0 / 1024.0,
            is_self,
        });
    }
    assign_process_groups(&mut out);
    out
}

struct ParsedStat {
    comm: String,
    ppid: i32,
    utime: u64,
    stime: u64,
    start_time: u64,
    rss_pages: i64,
}
fn parse_proc_stat(text: &str) -> Option<ParsedStat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text[open + 1..close].to_string();
    let rest: Vec<&str> = text.get(close + 2..)?.split_whitespace().collect();
    if rest.len() < 22 {
        return None;
    }
    Some(ParsedStat {
        comm,
        ppid: rest[1].parse().unwrap_or(0),
        utime: rest[11].parse().unwrap_or(0),
        stime: rest[12].parse().unwrap_or(0),
        start_time: rest[19].parse().unwrap_or(0),
        rss_pages: rest[21].parse().unwrap_or(0),
    })
}

pub(crate) fn read_total_jiffies(proc_root: &Path) -> u64 {
    let text = safe_read_trim(proc_root.join("stat"));
    text.lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse::<u64>().ok())
        .sum()
}
fn read_proc_io(file: PathBuf) -> (u64, u64) {
    let text = safe_read_trim(file);
    let mut r = 0;
    let mut w = 0;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            r = v.trim().parse().unwrap_or(0);
        }
        if let Some(v) = line.strip_prefix("write_bytes:") {
            w = v.trim().parse().unwrap_or(0);
        }
    }
    (r, w)
}
fn read_cmdline(file: PathBuf) -> String {
    let Ok(mut f) = fs::File::open(file) else {
        return String::new();
    };
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf)
        .replace('\0', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
fn looks_like_self(cfg: &Config, pid: i32, cmd: &str, comm: &str) -> bool {
    if !(comm.contains("bms-watchdog") || cmd.contains("bms-watchdog")) {
        return false;
    }
    if cmd.contains("bms-watchdog") {
        return true;
    }
    fs::read_link(cfg.proc_root.join(pid.to_string()).join("cwd"))
        .map(|cwd| cwd.to_string_lossy().contains("watchdog"))
        .unwrap_or(false)
}

fn assign_process_groups(procs: &mut [ProcNow]) {
    let by_pid: HashMap<i32, ProcNow> = procs.iter().map(|p| (p.pid, p.clone())).collect();
    for proc in procs {
        if proc.is_self {
            proc.app = "bms-watchdog".into();
            continue;
        }
        let inherited = ancestor_owner_group(proc, &by_pid);
        let direct = direct_process_group(&proc.name, &proc.cmd);
        if let Some(inh) = inherited.filter(|inh| {
            is_browser_helper(&proc.name, &proc.cmd)
                || inh == "Docker"
                || is_electron_helper(&proc.name, &proc.cmd)
        }) {
            proc.app = inh;
        } else {
            proc.app = direct.unwrap_or_else(|| fallback_app_name(&proc.name, &proc.cmd));
        }
    }
}
fn ancestor_owner_group(proc: &ProcNow, by_pid: &HashMap<i32, ProcNow>) -> Option<String> {
    let mut current = by_pid.get(&proc.ppid);
    let mut seen = HashSet::new();
    for _ in 0..12 {
        let c = current?;
        if !seen.insert(c.pid) {
            break;
        }
        if c.is_self {
            return Some("bms-watchdog".into());
        }
        if let Some(owner) = direct_owner_group(c) {
            return Some(owner);
        }
        current = by_pid.get(&c.ppid);
    }
    None
}
fn direct_owner_group(proc: &ProcNow) -> Option<String> {
    let direct = direct_process_group(&proc.name, &proc.cmd)?;
    [
        "Zen Browser",
        "Firefox",
        "Chrome/Chromium",
        "Docker",
        "VS Code",
        "Slack",
        "Discord",
        "Spotify",
    ]
    .contains(&direct.as_str())
    .then_some(direct)
}
fn direct_process_group(name: &str, cmd: &str) -> Option<String> {
    let c = format!("{name} {cmd}").to_lowercase();
    if c.contains("zen-bin") || name == "zen" {
        return Some("Zen Browser".into());
    }
    if c.contains("firefox") || c.contains("librewolf") || c.contains("waterfox") {
        return Some("Firefox".into());
    }
    if c.contains("google-chrome")
        || c.contains("chrome --")
        || c.contains("chromium")
        || c.contains("brave-browser")
    {
        return Some("Chrome/Chromium".into());
    }
    if c.contains("docker")
        || c.contains("dockerd")
        || c.contains("containerd")
        || c.contains("runc")
        || c.contains("buildkit")
    {
        return Some("Docker".into());
    }
    if name.starts_with("kworker") {
        return Some("Kernel workers".into());
    }
    if c.contains("code")
        && (c.contains("vscode") || c.contains("visual studio code") || name == "code")
    {
        return Some("VS Code".into());
    }
    if c.contains("slack") {
        return Some("Slack".into());
    }
    if c.contains("discord") {
        return Some("Discord".into());
    }
    if c.contains("spotify") {
        return Some("Spotify".into());
    }
    if c.contains("wayland")
        || c.contains("kwin")
        || c.contains("gnome-shell")
        || c.contains("niri")
    {
        return Some("Desktop shell".into());
    }
    if c.contains("xorg") || c.contains("xwayland") {
        return Some("Display server".into());
    }
    if c.contains("node ") || name == "node" {
        return Some("Node.js".into());
    }
    if name == "bun" {
        return Some("Bun".into());
    }
    None
}
fn is_browser_helper(name: &str, cmd: &str) -> bool {
    let c = format!("{name} {cmd}").to_lowercase();
    [
        "isolated web",
        "web content",
        "webextensions",
        "web extension",
        "socket process",
        "rdd process",
        "utility process",
        "gpu process",
        "privileged cont",
        "preallocated",
    ]
    .iter()
    .any(|s| c.contains(s))
}
fn is_electron_helper(name: &str, cmd: &str) -> bool {
    let c = format!("{name} {cmd}").to_lowercase();
    [
        "--type=renderer",
        "--type=gpu-process",
        "--type=utility",
        "--type=zygote",
    ]
    .iter()
    .any(|s| c.contains(s))
}
fn fallback_app_name(comm: &str, cmd: &str) -> String {
    nonempty(comm.to_string())
        .unwrap_or_else(|| first_word(cmd))
        .if_empty("unknown")
}

pub(crate) fn read_theme(cfg: &Config) -> (String, String) {
    if let Ok(out) = Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
    {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
            if text.contains("dark") {
                return ("dark".into(), "gsettings color-scheme".into());
            }
            if text.contains("light") {
                return ("light".into(), "gsettings color-scheme".into());
            }
        }
    }
    for file in [
        cfg.host_config_dir.join("gtk-3.0/settings.ini"),
        cfg.host_config_dir.join("gtk-4.0/settings.ini"),
        cfg.host_config_dir.join("kdeglobals"),
    ] {
        let text = safe_read_trim(&file);
        if text.is_empty() {
            continue;
        }
        let lower = text.to_lowercase();
        let base = file
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if lower.contains("gtk-application-prefer-dark-theme=true")
            || lower.contains("gtk-application-prefer-dark-theme=1")
        {
            return ("dark".into(), base);
        }
        if lower.contains("gtk-application-prefer-dark-theme=false")
            || lower.contains("gtk-application-prefer-dark-theme=0")
        {
            return ("light".into(), base);
        }
        if let Some(line) = lower
            .lines()
            .find(|l| l.contains("theme") || l.contains("colorscheme") || l.contains("lookandfeel"))
        {
            if line.contains("dark") {
                return (
                    "dark".into(),
                    format!("{base}: {}", &line[..line.len().min(80)]),
                );
            }
            if line.contains("light") {
                return (
                    "light".into(),
                    format!("{base}: {}", &line[..line.len().min(80)]),
                );
            }
        }
    }
    (
        "unknown".into(),
        format!(
            "no readable theme config in {}",
            cfg.host_config_dir.display()
        ),
    )
}
pub(crate) fn read_brightness(cfg: &Config) -> (Option<f64>, String) {
    let root = cfg
        .power_root
        .parent()
        .unwrap_or(Path::new("/sys/class"))
        .join("backlight");
    let mut rows = Vec::new();
    for name in safe_readdir(&root) {
        let dir = root.join(&name);
        if let (Some(cur), Some(max)) = (
            read_num(dir.join("brightness")),
            read_num(dir.join("max_brightness")),
        ) {
            if max > 0.0 {
                rows.push((name, cur / max * 100.0));
            }
        }
    }
    if rows.is_empty() {
        return (None, root.display().to_string());
    }
    rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    (Some(rows[0].1), rows[0].0.clone())
}

pub(crate) struct AudioState {
    pub(crate) playing: Option<bool>,
    pub(crate) browser_playing: Option<bool>,
    pub(crate) detail: String,
}
pub(crate) fn read_audio_state(cfg: &Config) -> AudioState {
    let asound = cfg.proc_root.join("asound");
    if !asound.exists() {
        return AudioState {
            playing: None,
            browser_playing: None,
            detail: "no /proc/asound".into(),
        };
    }
    for card in safe_readdir(&asound)
        .into_iter()
        .filter(|n| n.starts_with("card"))
    {
        let card_dir = asound.join(&card);
        for pcm in safe_readdir(&card_dir)
            .into_iter()
            .filter(|n| n.starts_with("pcm") && n.ends_with('p'))
        {
            let status = safe_read_trim(card_dir.join(&pcm).join("sub0/status"));
            if status.contains("RUNNING") {
                return AudioState {
                    playing: Some(true),
                    browser_playing: None,
                    detail: format!("{card}/{pcm} RUNNING"),
                };
            }
        }
    }
    AudioState {
        playing: Some(false),
        browser_playing: None,
        detail: "all playback PCM devices idle/suspended".into(),
    }
}
pub(crate) fn read_pactl_audio_state() -> AudioState {
    let Ok(out) = Command::new("pactl")
        .args(["-f", "json", "list", "sink-inputs"])
        .output()
    else {
        return AudioState {
            playing: None,
            browser_playing: None,
            detail: "pactl unavailable".into(),
        };
    };
    if !out.status.success() {
        return AudioState {
            playing: None,
            browser_playing: None,
            detail: "pactl failed".into(),
        };
    }
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
        return AudioState {
            playing: None,
            browser_playing: None,
            detail: "pactl parse failed".into(),
        };
    };
    let arr = v.as_array().cloned().unwrap_or_default();
    let mut apps = Vec::new();
    let mut media = Vec::new();
    let mut browser = false;
    let mut audible = 0;
    for item in arr {
        if value_truthy(item.get("corked")) || value_truthy(item.get("mute")) {
            continue;
        }
        let props = item.get("properties").and_then(Value::as_object);
        let app = props
            .and_then(|p| p.get("application.name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let bin = props
            .and_then(|p| p.get("application.process.binary"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let name = props
            .and_then(|p| p.get("media.name").or_else(|| p.get("node.name")))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let text = format!("{app} {bin} {name}").to_lowercase();
        let is_browser = BROWSER_KEYWORDS.iter().any(|s| text.contains(s));
        let generic = [
            "",
            "zoom",
            "audio stream",
            "playback",
            "audio",
            "webrtc",
            "web audio",
        ]
        .contains(&name.trim().to_lowercase().as_str())
            || name.trim().eq_ignore_ascii_case(app.trim());
        if is_browser && generic {
            continue;
        }
        audible += 1;
        if is_browser {
            browser = true;
        }
        if !app.is_empty() && !apps.contains(&app) {
            apps.push(app);
        }
        if !name.is_empty() && !media.contains(&name) {
            media.push(name);
        }
    }
    let detail = if audible == 0 {
        "no audible streams".into()
    } else {
        apps.iter()
            .chain(media.iter())
            .take(6)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    };
    AudioState {
        playing: Some(audible > 0),
        browser_playing: Some(browser),
        detail,
    }
}
fn value_truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => matches!(s.to_lowercase().as_str(), "yes" | "true" | "1"),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        _ => false,
    }
}

pub(crate) struct NetRates {
    pub(crate) rx_mbps: f64,
    pub(crate) tx_mbps: f64,
}
pub(crate) fn read_network_rates(cfg: &Config, collector: &mut Collector, ts: i64) -> NetRates {
    let counters = read_network_counters(cfg);
    let Some(prev) = collector.prev_net else {
        collector.prev_net = Some(NetCountersAt {
            ts,
            rx_bytes: counters.0,
            tx_bytes: counters.1,
        });
        return NetRates {
            rx_mbps: 0.0,
            tx_mbps: 0.0,
        };
    };
    let dt = ((ts - prev.ts) as f64 / 1000.0).max(1.0);
    let rx = counters.0.saturating_sub(prev.rx_bytes) as f64 * 8.0 / dt / 1_000_000.0;
    let tx = counters.1.saturating_sub(prev.tx_bytes) as f64 * 8.0 / dt / 1_000_000.0;
    collector.prev_net = Some(NetCountersAt {
        ts,
        rx_bytes: counters.0,
        tx_bytes: counters.1,
    });
    NetRates {
        rx_mbps: rx,
        tx_mbps: tx,
    }
}
fn read_network_counters(cfg: &Config) -> (u64, u64) {
    let text = safe_read_trim(cfg.proc_root.join("net/dev"));
    let mut rx = 0;
    let mut tx = 0;
    for line in text.lines().filter(|l| l.contains(':')) {
        let Some((iface, rest)) = line.split_once(':') else {
            continue;
        };
        let iface = iface.trim();
        if iface.is_empty()
            || iface == "lo"
            || iface.starts_with("docker")
            || iface.starts_with("br-")
            || iface.starts_with("veth")
        {
            continue;
        }
        let parts: Vec<u64> = rest
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        if parts.len() >= 16 {
            rx += parts[0];
            tx += parts[8];
        }
    }
    (rx, tx)
}

pub(crate) struct Focused {
    pub(crate) app: String,
    pub(crate) title: String,
    pub(crate) pid: Option<i64>,
}
pub(crate) fn read_focused_window(cfg: &Config) -> Focused {
    if let Some(file) = &cfg.focused_window_file {
        if let Some(f) = parse_focused_json(&safe_read_trim(file)) {
            return sanitize_focused_window(cfg, f);
        }
    }
    let runtime_dir = niri_runtime_dir();
    let sockets = niri_socket_candidates(
        env::var_os("NIRI_SOCKET").map(PathBuf::from),
        runtime_dir.as_deref(),
    );
    if let Some(focused) = query_focused_candidates(&sockets, |socket| {
        let out = Command::new("niri")
            .args(["msg", "-j", "focused-window"])
            .env("NIRI_SOCKET", socket)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| parse_focused_json(&String::from_utf8_lossy(&out.stdout)))
            .flatten()
    }) {
        return sanitize_focused_window(cfg, focused);
    }
    Focused {
        app: String::new(),
        title: String::new(),
        pid: None,
    }
}

fn query_focused_candidates(
    sockets: &[PathBuf],
    mut query: impl FnMut(&Path) -> Option<Focused>,
) -> Option<Focused> {
    sockets.iter().find_map(|socket| query(socket))
}

fn niri_runtime_dir() -> Option<PathBuf> {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
    {
        return Some(dir);
    }
    current_uid()
        .map(|uid| PathBuf::from(format!("/run/user/{uid}")))
        .filter(|p| p.is_dir())
}

fn niri_socket_candidates(env_socket: Option<PathBuf>, runtime_dir: Option<&Path>) -> Vec<PathBuf> {
    let uid = current_uid();
    let mut sockets = Vec::new();
    if let Some(path) = env_socket.filter(|path| is_user_niri_socket(path, uid)) {
        sockets.push(path);
    }
    if let Some(runtime_dir) = runtime_dir {
        let mut discovered = safe_readdir(runtime_dir)
            .into_iter()
            .filter(|name| name.starts_with("niri.") && name.ends_with(".sock"))
            .map(|name| runtime_dir.join(name))
            .filter(|path| is_user_niri_socket(path, uid))
            .collect::<Vec<_>>();
        discovered.sort();
        for path in discovered {
            if !sockets.contains(&path) {
                sockets.push(path);
            }
        }
    }
    sockets
}

#[cfg(unix)]
fn is_user_niri_socket(path: &Path, uid: Option<u32>) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    metadata.file_type().is_socket()
        && uid
            .map(|expected| metadata.uid() == expected)
            .unwrap_or(false)
}
#[cfg(not(unix))]
fn is_user_niri_socket(_path: &Path, _uid: Option<u32>) -> bool {
    false
}

fn current_uid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        let values = line.strip_prefix("Uid:")?;
        values.split_whitespace().next()?.parse().ok()
    })
}
fn parse_focused_json(text: &str) -> Option<Focused> {
    let v: Value = serde_json::from_str(text).ok()?;
    let app = v
        .get("app_id")
        .or_else(|| v.get("app"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(120)
        .collect();
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .chars()
        .take(240)
        .collect();
    let pid = v.get("pid").and_then(Value::as_i64);
    Some(Focused { app, title, pid })
}

fn sanitize_focused_window(cfg: &Config, mut focused: Focused) -> Focused {
    if !is_browser_like(&focused.app, &focused.title) {
        return focused;
    }
    if cfg.redact_browser_titles {
        focused.title = "Browser window".into();
    } else if cfg.redact_private_browser_titles && is_private_browser_title(&focused.title) {
        focused.title = "Private browser window".into();
    }
    focused
}

fn is_private_browser_title(title: &str) -> bool {
    let t = title.to_lowercase();
    [
        "private browsing",
        "private window",
        "incognito",
        "inprivate",
        "приват",
        "инкогнито",
    ]
    .iter()
    .any(|s| t.contains(s))
}
pub(crate) fn read_lid_state(cfg: &Config) -> (Option<bool>, String) {
    let root = cfg.proc_root.join("acpi/button/lid");
    let mut states = Vec::new();
    for lid in safe_readdir(&root) {
        let state = safe_read_trim(root.join(&lid).join("state"));
        if !state.is_empty() {
            states.push(format!(
                "{lid}: {}",
                state.split_whitespace().collect::<Vec<_>>().join(" ")
            ));
        }
    }
    if states.is_empty() {
        return (None, "no ACPI lid state".into());
    }
    let detail = states.join(", ");
    (Some(detail.to_lowercase().contains("closed")), detail)
}
pub(crate) fn read_screen_lock_state(cfg: &Config) -> (Option<bool>, String) {
    let lockers = [
        "swaylock",
        "hyprlock",
        "gtklock",
        "waylock",
        "i3lock",
        "xsecurelock",
        "kscreenlocker",
        "gnome-screensaver",
    ];
    for name in safe_readdir(&cfg.proc_root) {
        if !name.as_bytes().iter().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let dir = cfg.proc_root.join(&name);
        let comm = safe_read_trim(dir.join("comm"));
        let cmd = nonempty(read_cmdline(dir.join("cmdline"))).unwrap_or(comm.clone());
        let lower = format!("{comm} {cmd}").to_lowercase();
        if let Some(locker) = lockers.iter().find(|l| lower.contains(*l)) {
            return (Some(true), format!("{locker} pid {name}"));
        }
    }
    (Some(false), "no known lock-screen process".into())
}
pub(crate) fn read_fan_speed(cfg: &Config) -> (Option<f64>, String) {
    let hwmon_root = cfg
        .power_root
        .parent()
        .unwrap_or(Path::new("/sys/class"))
        .join("hwmon");
    let mut fans = Vec::new();
    for hwmon in safe_readdir(&hwmon_root) {
        let dir = hwmon_root.join(&hwmon);
        let chip = nonempty(safe_read_trim(dir.join("name"))).unwrap_or(hwmon);
        for file in safe_readdir(&dir) {
            if !(file.starts_with("fan") && file.ends_with("_input")) {
                continue;
            }
            if let Some(rpm) = read_num(dir.join(&file)) {
                if rpm >= 0.0 {
                    fans.push((rpm, format!("{chip}/{file}")));
                }
            }
        }
    }
    let thinkpad = safe_read_trim(cfg.proc_root.join("acpi/ibm/fan"));
    for line in thinkpad.lines() {
        if let Some(v) = line.strip_prefix("speed:") {
            if let Ok(rpm) = v.trim().parse::<f64>() {
                fans.push((rpm, "thinkpad_acpi".into()));
            }
        }
    }
    if fans.is_empty() {
        return (None, format!("no fan sensor in {}", hwmon_root.display()));
    }
    fans.sort_by(|a, b| b.0.total_cmp(&a.0));
    (Some(fans[0].0), fans[0].1.clone())
}

pub(crate) struct UsbPower {
    pub(crate) sourcing: Option<bool>,
    pub(crate) watts: Option<f64>,
    pub(crate) detail: String,
}

pub(crate) fn read_usb_power(cfg: &Config) -> UsbPower {
    let class_root = cfg.power_root.parent().unwrap_or(Path::new("/sys/class"));
    let mut details = Vec::new();
    let mut role_source = false;
    let mut role_sink = false;
    let mut active_source = false;
    for port in safe_readdir(class_root.join("typec")) {
        let dir = class_root.join("typec").join(&port);
        let role = safe_read_trim(dir.join("power_role"));
        if role.is_empty() {
            continue;
        }
        details.push(format!("{port} power_role={role}"));
        let role_lower = role.to_lowercase();
        if role_lower.contains("source") {
            role_source = true;
        } else if role_lower.contains("sink") {
            role_sink = true;
        }
    }

    let mut watts = None;
    for supply in safe_readdir(&cfg.power_root) {
        let dir = cfg.power_root.join(&supply);
        let kind = safe_read_trim(dir.join("type")).to_lowercase();
        if !(kind.contains("usb")
            || supply.to_lowercase().contains("usb")
            || supply.to_lowercase().contains("typec"))
        {
            continue;
        }
        let p = read_power_w(&dir).or_else(|| {
            let current = read_num(dir.join("current_now"))?;
            let voltage = read_num(dir.join("voltage_now"))?;
            Some((current * voltage).abs() / 1_000_000_000_000.0)
        });
        if let Some(p) = p {
            if p > 0.05 {
                active_source = true;
                watts = Some(watts.unwrap_or(0.0) + p);
            }
            details.push(format!("{supply} reports {p:.2}W"));
        } else {
            let online = safe_read_trim(dir.join("online"));
            if online == "1" {
                active_source = true;
            }
            if !online.is_empty() {
                details.push(format!("{supply} online={online}"));
            }
        }
    }

    let usb_devices = read_external_usb_devices(class_root);
    if !usb_devices.is_empty() {
        active_source = true;
        details.extend(usb_devices);
    }

    let sourcing = if active_source {
        Some(true)
    } else if role_source || role_sink || !details.is_empty() {
        Some(false)
    } else {
        None
    };
    UsbPower {
        sourcing,
        watts,
        detail: if details.is_empty() {
            format!(
                "no USB-C/typec power role in {}",
                class_root.join("typec").display()
            )
        } else {
            details.join(", ")
        },
    }
}

fn read_external_usb_devices(class_root: &Path) -> Vec<String> {
    let usb_root = class_root
        .parent()
        .unwrap_or(Path::new("/sys"))
        .join("bus/usb/devices");
    let mut devices = Vec::new();
    for dev in safe_readdir(&usb_root) {
        let dir = usb_root.join(&dev);
        let vendor = safe_read_trim(dir.join("idVendor"));
        let manufacturer = safe_read_trim(dir.join("manufacturer"));
        let product = safe_read_trim(dir.join("product"));
        let text = format!("{vendor} {manufacturer} {product}").to_lowercase();
        let interesting = vendor == "05ac"
            || [
                "iphone", "ipad", "android", "phone", "apple", "samsung", "pixel",
            ]
            .iter()
            .any(|s| text.contains(s));
        if !interesting {
            continue;
        }
        let max_power = safe_read_trim(dir.join("bMaxPower"));
        let max_hint = usb_max_power_watts(&max_power)
            .map(|w| format!(" descriptor max {w:.1}W"))
            .unwrap_or_default();
        devices.push(format!(
            "{} {} connected{}; actual draw not exposed",
            manufacturer.if_empty("USB device"),
            product.if_empty(&dev),
            max_hint
        ));
    }
    devices
}

fn usb_max_power_watts(text: &str) -> Option<f64> {
    let ma = text.trim().strip_suffix("mA")?.trim().parse::<f64>().ok()?;
    Some(ma * 5.0 / 1000.0)
}

pub(crate) fn detect_video_streaming(
    cfg: &Config,
    net_rx_mbps: f64,
    browser_watts: f64,
    browser_cpu: f64,
    focused: &Focused,
    audio_playing: Option<bool>,
    browser_audio_playing: Option<bool>,
    audio_detail: &str,
) -> (bool, String) {
    let browser_active = browser_watts > 0.3 || browser_cpu > 5.0;
    let browser_busy = browser_watts > 1.0 || browser_cpu > 15.0;
    let browser_video_busy = browser_watts > 2.5 || browser_cpu > 25.0;
    let browser_focused = is_browser_like(&focused.app, &focused.title);
    let video_title = is_video_like_title(&focused.title);
    let audio_video_title = is_video_like_title(audio_detail);
    let mut reasons = Vec::new();
    if net_rx_mbps >= cfg.video_rx_mbps_threshold && browser_active {
        reasons.push(format!("{net_rx_mbps:.2} Mbps RX + browser activity"));
    }
    if browser_focused && video_title && browser_busy {
        reasons.push(format!(
            "video page title + busy browser ({browser_watts:.2} W, {browser_cpu:.1}% CPU)"
        ));
    }
    if browser_focused && video_title && audio_playing == Some(true) && browser_active {
        reasons.push("video page title + audio playing".to_string());
    }
    if audio_video_title && browser_audio_playing == Some(true) && browser_active {
        reasons.push("browser media title looks like video".to_string());
    }
    if browser_audio_playing == Some(true) && browser_video_busy {
        reasons.push(format!("browser media playback + high browser activity ({browser_watts:.2} W, {browser_cpu:.1}% CPU)"));
    }
    let title_hint = if video_title {
        format!(
            "; title={}",
            focused.title.chars().take(90).collect::<String>()
        )
    } else if audio_video_title {
        format!(
            "; media={}",
            audio_detail.chars().take(90).collect::<String>()
        )
    } else {
        String::new()
    };
    let audio_hint = match audio_playing {
        None => "unknown",
        Some(true) => "playing",
        Some(false) => "idle",
    };
    let browser_hint = match browser_audio_playing {
        None => "unknown",
        Some(true) => "playing",
        Some(false) => "idle",
    };
    let base = format!("{net_rx_mbps:.2} Mbps RX; browser {browser_watts:.2} W, {browser_cpu:.1}% CPU; audio {audio_hint}; browser audio {browser_hint}{title_hint}");
    if reasons.is_empty() {
        (false, format!("not detected: {base}"))
    } else {
        (true, format!("probable: {} ({base})", reasons.join("; ")))
    }
}
fn is_browser_like(app: &str, title: &str) -> bool {
    let t = format!("{app} {title}").to_lowercase();
    BROWSER_KEYWORDS.iter().any(|s| t.contains(s))
}

pub(crate) fn subprocess_label(name: &str, cmd: &str) -> String {
    if name == "system-baseline" {
        return "System / baseline".into();
    }
    if cmd.is_empty() || cmd == name {
        return name.into();
    }
    if name.starts_with("Isolated")
        || name.contains("Web")
        || name.contains("Socket")
        || name.contains("Privileged")
    {
        return name.into();
    }
    let lower = cmd.to_lowercase();
    if lower.contains("-contentproc") {
        if lower.contains(" socket") {
            return "Socket Process".into();
        }
        if lower.contains(" rdd") {
            return "RDD Process".into();
        }
        if lower.contains(" utility") {
            return "Utility Process".into();
        }
        if lower.contains("-isforbrowser") {
            return "Web Content".into();
        }
        return "Content process".into();
    }
    nonempty(first_word(cmd)).unwrap_or_else(|| name.into())
}
pub(crate) fn normalize_stored_app(app: &str) -> String {
    if [
        "zen",
        "zen-bin",
        "Isolated Web Co",
        "Isolated Servic",
        "Web Content",
        "WebExtensions",
        "Socket Process",
        "Privileged Cont",
        "forkserver",
    ]
    .contains(&app)
    {
        return "Zen Browser".into();
    }
    if [
        "containerd",
        "containerd-shim",
        "dockerd",
        "docker",
        "runc",
        "docker-proxy",
    ]
    .contains(&app)
    {
        return "Docker".into();
    }
    if app.starts_with("kworker") {
        return "Kernel workers".into();
    }
    app.to_string()
}
fn is_video_like_title(title: &str) -> bool {
    let t = title.to_lowercase();
    !t.is_empty()
        && [
            "youtube",
            "youtu.be",
            "twitch",
            "netflix",
            "vimeo",
            "rumble",
            "odysee",
            "nebula",
            "peertube",
            "hulu",
            "disney+",
            "prime video",
            "hbo",
            "max",
            "peacock",
            "dailymotion",
            "bilibili",
            "watch",
            "stream",
            "live",
            "episode",
            "season",
            "movie",
            "trailer",
            "playlist",
        ]
        .iter()
        .any(|s| t.contains(s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bms-watchdog-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg(root: &Path) -> Config {
        Config {
            data_dir: root.join("data"),
            host: "127.0.0.1".into(),
            port: 0,
            poll_interval: Duration::from_secs(30),
            proc_root: root.join("proc"),
            power_root: root.join("sys/class/power_supply"),
            host_config_dir: root.join("config"),
            focused_window_file: None,
            record_when_plugged: false,
            force_collect: false,
            retention_days: 365,
            detailed_process_retention_days: 2,
            agent_retention_days: 365,
            context_retention_days: 365,
            context_tracking: true,
            context_path_mode: "basename".into(),
            terminal_context: false,
            tmux_sockets: vec![],
            workspace_roots: vec![],
            browser_event_retention_days: 90,
            browser_title_retention_days: 30,
            browser_integration: false,
            agent_presence_dir: root.join("runtime/presence"),
            agent_tracking: false,
            pi_sessions_dirs: vec![],
            baseline_mode: "adaptive".into(),
            baseline_watts: 4.0,
            baseline_min_watts: 2.0,
            baseline_max_watts: 6.0,
            baseline_lookback_hours: 24.0,
            suspend_gap: Duration::from_secs(120),
            video_rx_mbps_threshold: 1.0,
            max_processes_per_sample: 0,
            clk_tck: 100,
            redact_browser_titles: false,
            redact_private_browser_titles: true,
            self_pid: None,
        }
    }

    fn write(path: impl AsRef<Path>, value: &str) {
        let path = path.as_ref();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    #[test]
    fn battery_reads_energy_and_power_now() {
        let root = temp_root("battery-energy");
        let cfg = cfg(&root);
        let bat = cfg.power_root.join("BAT0");
        write(bat.join("type"), "Battery\n");
        write(bat.join("status"), "Discharging\n");
        write(bat.join("energy_now"), "40000000\n");
        write(bat.join("energy_full"), "50000000\n");
        write(bat.join("power_now"), "8000000\n");
        let sample = read_battery(&cfg, &Collector::default(), 1);
        assert!(sample.on_battery);
        assert_eq!(sample.capacity, Some(80.0));
        assert_eq!(sample.energy_wh, Some(40.0));
        assert_eq!(sample.power_w, Some(8.0));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn battery_reads_charge_voltage_and_current_voltage() {
        let root = temp_root("battery-charge");
        let cfg = cfg(&root);
        let bat = cfg.power_root.join("BAT0");
        write(bat.join("type"), "Battery\n");
        write(bat.join("status"), "Discharging\n");
        write(bat.join("charge_now"), "4000000\n");
        write(bat.join("charge_full"), "5000000\n");
        write(bat.join("voltage_now"), "10000000\n");
        write(bat.join("current_now"), "800000\n");
        let sample = read_battery(&cfg, &Collector::default(), 1);
        assert_eq!(sample.capacity, Some(80.0));
        assert_eq!(sample.energy_wh, Some(40.0));
        assert_eq!(sample.power_w, Some(8.0));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn process_grouping_covers_browser_docker_kernel_and_desktop() {
        assert_eq!(
            direct_process_group("zen", "zen-bin"),
            Some("Zen Browser".into())
        );
        assert_eq!(
            direct_process_group("containerd-shim", "containerd-shim-runc-v2"),
            Some("Docker".into())
        );
        assert_eq!(
            direct_process_group("kworker/0:1", ""),
            Some("Kernel workers".into())
        );
        assert_eq!(
            direct_process_group("niri", "niri --session"),
            Some("Desktop shell".into())
        );
        assert!(is_browser_helper(
            "Web Content",
            "-contentproc -isForBrowser"
        ));
    }

    #[test]
    fn focused_private_browser_title_is_redacted() {
        let root = temp_root("private-title");
        let cfg = cfg(&root);
        let focused = sanitize_focused_window(
            &cfg,
            Focused {
                app: "zen".into(),
                title: "Example - Private Browsing".into(),
                pid: Some(1),
            },
        );
        assert_eq!(focused.title, "Private browser window");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn niri_sockets_prefer_valid_env_then_sort_discovery() {
        use std::os::unix::net::UnixListener;
        let root = temp_root("niri-sockets");
        let a = root.join("niri.wayland-1.20.sock");
        let b = root.join("niri.wayland-1.10.sock");
        let env_socket = root.join("niri.env.30.sock");
        let _a = UnixListener::bind(&a).unwrap();
        let _b = UnixListener::bind(&b).unwrap();
        let _env = UnixListener::bind(&env_socket).unwrap();
        write(root.join("niri.not-a-socket.sock"), "no");
        let sockets = niri_socket_candidates(Some(env_socket.clone()), Some(&root));
        assert_eq!(sockets, vec![env_socket, b, a]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn focused_query_falls_back_after_stale_socket_failure() {
        let stale = PathBuf::from("stale");
        let live = PathBuf::from("live");
        let mut attempted = Vec::new();
        let focused = query_focused_candidates(&[stale.clone(), live.clone()], |socket| {
            attempted.push(socket.to_path_buf());
            (socket == live).then(|| Focused {
                app: "terminal".into(),
                title: "shell".into(),
                pid: Some(7),
            })
        })
        .unwrap();
        assert_eq!(attempted, vec![stale, live]);
        assert_eq!(focused.app, "terminal");
    }

    #[test]
    fn usb_power_detects_typec_source_and_power() {
        let root = temp_root("usb-power");
        let cfg = cfg(&root);
        write(root.join("sys/class/typec/port0/power_role"), "source\n");
        write(cfg.power_root.join("USB-C/type"), "USB\n");
        write(cfg.power_root.join("USB-C/power_now"), "5000000\n");
        let usb = read_usb_power(&cfg);
        assert_eq!(usb.sourcing, Some(true));
        assert_eq!(usb.watts, Some(5.0));
        assert!(usb.detail.contains("power_role=source"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn usb_power_source_role_alone_is_not_active_charging() {
        let root = temp_root("usb-power-idle");
        let cfg = cfg(&root);
        write(root.join("sys/class/typec/port0/power_role"), "[source]\n");
        let usb = read_usb_power(&cfg);
        assert_eq!(usb.sourcing, Some(false));
        assert_eq!(usb.watts, None);
        let _ = fs::remove_dir_all(root);
    }
}
