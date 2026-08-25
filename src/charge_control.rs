//! Battery charge-type control (`/sys/class/power_supply/<BAT>/charge_types`).
//!
//! This is the one place where watchdog actuates hardware rather than observing
//! it. The kernel advertises the supported enum itself, so nothing here is
//! device-specific: the write set is always validated against what the running
//! kernel reports, never against a hardcoded list.
//!
//! The attribute is root-owned `0644` by default. Granting the dashboard user
//! write access is a deliberate, separately installed step (see
//! `devices/<device-id>/battery-charge-types-install.sh`), so an unprivileged
//! daemon stays unprivileged unless the operator opts in.

use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    config::Config,
    util::{safe_read_trim, safe_readdir},
};

const ATTR: &str = "charge_types";

pub(crate) const PERMISSION_HINT: &str = concat!(
    "charge_types is not writable by this user. Install the udev rule ",
    "(devices/<device-id>/battery-charge-types-install.sh) to grant group write access, ",
    "then replug or reboot."
);

#[derive(Debug, Deserialize)]
pub(crate) struct ChargeTypeInput {
    pub(crate) value: String,
}

/// First battery under `power_root` that exposes `charge_types`.
///
/// Sorted so multi-battery hosts resolve deterministically instead of following
/// readdir order.
pub(crate) fn attribute_path(cfg: &Config) -> Option<PathBuf> {
    if !cfg.power_root.exists() {
        return None;
    }
    let mut names = safe_readdir(&cfg.power_root);
    names.sort();
    names.into_iter().find_map(|name| {
        let dir = cfg.power_root.join(&name);
        let is_battery = safe_read_trim(dir.join("type")) == "Battery" || name.starts_with("BAT");
        let path = dir.join(ATTR);
        (is_battery && path.exists()).then_some(path)
    })
}

/// Parse the kernel enum format: `[Fast] Standard Long_Life`.
///
/// The bracketed token is the active value and is also a valid option. Absent
/// brackets means the kernel exposes choices but reports no active selection.
pub(crate) fn parse_charge_types(raw: &str) -> (Option<String>, Vec<String>) {
    let mut current = None;
    let mut options = Vec::new();
    for token in raw.split_whitespace() {
        match token
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            Some("") => {}
            Some(inner) => {
                current = Some(inner.to_string());
                options.push(inner.to_string());
            }
            None => options.push(token.to_string()),
        }
    }
    (current, options)
}

/// Definitive writability probe. `access(2)`-style checks lie under some LSMs;
/// opening the attribute for write does not. Opening without writing is inert.
fn is_writable(path: &Path) -> bool {
    fs::OpenOptions::new().write(true).open(path).is_ok()
}

fn describe(path: &Path, raw: &str) -> Value {
    let (current, options) = parse_charge_types(raw);
    let writable = is_writable(path);
    json!({
        "supported": true,
        "path": path.display().to_string(),
        "raw": raw,
        "current": current,
        "options": options,
        "writable": writable,
        "reason": if writable { Value::Null } else { json!(PERMISSION_HINT) },
    })
}

pub(crate) fn status_json(cfg: &Config) -> Value {
    let Some(path) = attribute_path(cfg) else {
        return json!({
            "supported": false,
            "current": Value::Null,
            "options": [],
            "writable": false,
            "reason": "no battery on this host exposes a charge_types attribute",
        });
    };
    let raw = safe_read_trim(&path);
    if raw.is_empty() {
        return json!({
            "supported": false,
            "path": path.display().to_string(),
            "current": Value::Null,
            "options": [],
            "writable": false,
            "reason": "charge_types exists but could not be read",
        });
    }
    describe(&path, &raw)
}

pub(crate) fn apply(cfg: &Config, requested: &str) -> Result<Value> {
    let Some(path) = attribute_path(cfg) else {
        bail!("no battery on this host exposes a charge_types attribute");
    };

    let before = safe_read_trim(&path);
    if before.is_empty() {
        bail!("charge_types exists but could not be read");
    }
    let (_, options) = parse_charge_types(&before);

    // Accept case-insensitively but write back the kernel's own spelling.
    let wanted = requested.trim();
    let value = options
        .iter()
        .find(|option| option.eq_ignore_ascii_case(wanted))
        .cloned()
        .ok_or_else(|| {
            anyhow!(
                "unsupported charge type {wanted:?}; this kernel advertises: {}",
                options.join(", ")
            )
        })?;

    fs::write(&path, value.as_bytes()).map_err(|error| match error.kind() {
        ErrorKind::PermissionDenied => anyhow!("{PERMISSION_HINT}"),
        _ => anyhow!("failed writing {}: {error}", path.display()),
    })?;

    // sysfs accepts a write and silently ignores it when the EC refuses, so the
    // only trustworthy confirmation is reading the value back.
    let after = safe_read_trim(&path);
    let (current, _) = parse_charge_types(&after);
    if !current
        .as_deref()
        .is_some_and(|active| active.eq_ignore_ascii_case(&value))
    {
        bail!(
            "kernel did not accept {value}; charge type is now {}",
            current.unwrap_or_else(|| "unknown".into())
        );
    }

    Ok(describe(&path, &after))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_active_and_available_values() {
        let (current, options) = parse_charge_types("[Fast] Standard Long_Life");
        assert_eq!(current.as_deref(), Some("Fast"));
        assert_eq!(options, vec!["Fast", "Standard", "Long_Life"]);
    }

    #[test]
    fn parses_selection_that_is_not_first() {
        let (current, options) = parse_charge_types("Fast Standard [Long_Life]");
        assert_eq!(current.as_deref(), Some("Long_Life"));
        assert_eq!(options, vec!["Fast", "Standard", "Long_Life"]);
    }

    #[test]
    fn tolerates_absent_selection() {
        let (current, options) = parse_charge_types("Fast Standard");
        assert_eq!(current, None);
        assert_eq!(options, vec!["Fast", "Standard"]);
    }

    #[test]
    fn tolerates_empty_input() {
        let (current, options) = parse_charge_types("");
        assert_eq!(current, None);
        assert!(options.is_empty());
    }
}
