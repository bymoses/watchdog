use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{json, Value};

pub(crate) trait IfEmpty {
    fn if_empty(self, fallback: &str) -> String;
}

impl IfEmpty for String {
    fn if_empty(self, fallback: &str) -> String {
        if self.is_empty() {
            fallback.into()
        } else {
            self
        }
    }
}

pub(crate) fn safe_readdir(dir: impl AsRef<Path>) -> Vec<String> {
    fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn safe_read_trim(file: impl AsRef<Path>) -> String {
    fs::read_to_string(file)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

pub(crate) fn read_num(file: impl AsRef<Path>) -> Option<f64> {
    safe_read_trim(file).parse::<f64>().ok()
}

pub(crate) fn nonempty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

pub(crate) fn first_word(s: &str) -> String {
    s.split_whitespace()
        .next()
        .and_then(|w| Path::new(w).file_name())
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub(crate) fn clamp(n: f64, min: f64, max: f64) -> f64 {
    if n.is_finite() {
        n.max(min).min(max)
    } else {
        min
    }
}

pub(crate) fn opt_bool_i(v: Option<bool>) -> Option<i64> {
    v.map(|b| if b { 1 } else { 0 })
}

pub(crate) fn opt_bool(v: Option<i64>) -> Value {
    v.map(|x| json!(x != 0)).unwrap_or(Value::Null)
}

pub(crate) fn fmt_w(w: Option<f64>) -> String {
    w.map(|w| format!("{w:.2}W")).unwrap_or_else(|| "?W".into())
}

pub(crate) fn fmt_pct(p: Option<f64>) -> String {
    p.map(|p| format!("{p:.1}%")).unwrap_or_else(|| "?%".into())
}

pub(crate) fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if a.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_/.:=".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
