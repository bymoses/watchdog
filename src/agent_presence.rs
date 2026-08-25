use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const PRESENCE_SCHEMA_VERSION: u32 = 1;
pub(crate) const DEFAULT_SOURCE_KEY: &str = "local";
pub(crate) const EXACT_PRESENCE_METHOD: &str = "presence_native_session";
pub(crate) const CWD_RECENT_SESSION_METHOD: &str = "cwd_recent_session";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PresenceState {
    Idle,
    Busy,
    Settling,
    Closed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NormalizedPresenceLease {
    pub(crate) schema_version: u32,
    pub(crate) event_id: String,
    pub(crate) harness: String,
    pub(crate) source_key: String,
    pub(crate) instance_id: String,
    pub(crate) pid: i64,
    pub(crate) process_start_identity: String,
    pub(crate) native_session_id: String,
    pub(crate) cwd: String,
    pub(crate) state: PresenceState,
    pub(crate) observed_at: i64,
    pub(crate) expires_at: i64,
}

impl NormalizedPresenceLease {
    fn identity(&self) -> LeaseIdentity {
        LeaseIdentity {
            harness: self.harness.clone(),
            source_key: self.source_key.clone(),
            instance_id: self.instance_id.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PresenceReadLimits {
    pub(crate) max_files: usize,
    pub(crate) max_payload_bytes: u64,
    pub(crate) max_ttl_ms: i64,
    pub(crate) max_future_skew_ms: i64,
    pub(crate) expected_uid: u32,
    pub(crate) allowed_harnesses: BTreeSet<String>,
}

impl PresenceReadLimits {
    pub(crate) fn for_uid(expected_uid: u32) -> Self {
        Self {
            max_files: 64,
            max_payload_bytes: 16 * 1024,
            max_ttl_ms: 120_000,
            max_future_skew_ms: 5_000,
            expected_uid,
            allowed_harnesses: BTreeSet::from(["pi".to_string(), "claude".to_string()]),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LeaseRejection {
    InvalidDirectory,
    DirectoryOwner,
    DirectoryMode,
    EntryLimit,
    InvalidFileName,
    OpenFailed,
    NotRegularFile,
    FileOwner,
    FileMode,
    Empty,
    Oversized,
    MalformedJson,
    InvalidSchema,
    UnexpectedHarness,
    InvalidField,
    InvalidTimestamp,
    Expired,
    PidMissing,
    ProcessInspectionFailed,
    ProcessIdentityMismatch,
    DuplicateInstance,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RejectedLease {
    /// File names are included for local diagnostics; payload fields are never included.
    pub(crate) file_name: Option<String>,
    pub(crate) reason: LeaseRejection,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PresenceReadReport {
    pub(crate) leases: Vec<NormalizedPresenceLease>,
    pub(crate) rejected: Vec<RejectedLease>,
    pub(crate) entries_seen: usize,
    pub(crate) entry_limit_reached: bool,
}

pub(crate) trait ProcessInspector {
    fn process_exists(&self, pid: i64) -> io::Result<bool>;
    fn process_start_identity(&self, pid: i64) -> io::Result<Option<String>>;
    fn process_cwd(&self, pid: i64) -> io::Result<Option<PathBuf>>;
}

#[derive(Clone, Debug)]
pub(crate) struct LinuxProcessInspector {
    proc_root: PathBuf,
}

impl Default for LinuxProcessInspector {
    fn default() -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
        }
    }
}

impl LinuxProcessInspector {
    pub(crate) fn with_proc_root(proc_root: PathBuf) -> Self {
        Self { proc_root }
    }
}

impl ProcessInspector for LinuxProcessInspector {
    fn process_exists(&self, pid: i64) -> io::Result<bool> {
        match fs::metadata(self.proc_root.join(pid.to_string())) {
            Ok(metadata) => Ok(metadata.is_dir()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn process_start_identity(&self, pid: i64) -> io::Result<Option<String>> {
        let stat = match fs::read_to_string(self.proc_root.join(pid.to_string()).join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(parse_linux_start_ticks(&stat).map(|ticks| format!("linux:{ticks}")))
    }

    fn process_cwd(&self, pid: i64) -> io::Result<Option<PathBuf>> {
        match fs::read_link(self.proc_root.join(pid.to_string()).join("cwd")) {
            Ok(path) => Ok(Some(path)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn parse_linux_start_ticks(stat: &str) -> Option<u64> {
    let close = stat.rfind(')')?;
    // Fields after the command begin with field 3 (state); starttime is field 22.
    stat.get(close + 1..)?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

pub(crate) fn current_uid() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| {
        line.strip_prefix("Uid:")?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    })
}

pub(crate) fn read_presence_dir(
    directory: &Path,
    now_ms: i64,
    inspector: &dyn ProcessInspector,
    limits: &PresenceReadLimits,
) -> PresenceReadReport {
    let mut report = PresenceReadReport::default();
    if let Err(reason) = validate_directory(directory, limits.expected_uid) {
        report.rejected.push(RejectedLease {
            file_name: None,
            reason,
        });
        return report;
    }

    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(_) => {
            report.rejected.push(RejectedLease {
                file_name: None,
                reason: LeaseRejection::InvalidDirectory,
            });
            return report;
        }
    };
    let mut by_instance = BTreeMap::<LeaseIdentity, NormalizedPresenceLease>::new();
    for entry in entries {
        if report.entries_seen >= limits.max_files {
            report.entry_limit_reached = true;
            report.rejected.push(RejectedLease {
                file_name: None,
                reason: LeaseRejection::EntryLimit,
            });
            break;
        }
        report.entries_seen += 1;
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                report.rejected.push(RejectedLease {
                    file_name: None,
                    reason: LeaseRejection::OpenFailed,
                });
                continue;
            }
        };
        let file_name = entry.file_name().to_str().map(str::to_string);
        let lease = match read_one_lease(
            &entry.path(),
            file_name.as_deref(),
            now_ms,
            inspector,
            limits,
        ) {
            Ok(lease) => lease,
            Err(reason) => {
                report.rejected.push(RejectedLease { file_name, reason });
                continue;
            }
        };
        let identity = lease.identity();
        match by_instance.get(&identity) {
            Some(existing) if existing.observed_at >= lease.observed_at => {
                report.rejected.push(RejectedLease {
                    file_name,
                    reason: LeaseRejection::DuplicateInstance,
                });
            }
            Some(_) => {
                by_instance.insert(identity, lease);
                report.rejected.push(RejectedLease {
                    file_name: None,
                    reason: LeaseRejection::DuplicateInstance,
                });
            }
            None => {
                by_instance.insert(identity, lease);
            }
        }
    }
    report.leases = by_instance.into_values().collect();
    report
}

#[cfg(unix)]
fn validate_directory(path: &Path, uid: u32) -> std::result::Result<(), LeaseRejection> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path).map_err(|_| LeaseRejection::InvalidDirectory)?;
    if !metadata.file_type().is_dir() {
        return Err(LeaseRejection::InvalidDirectory);
    }
    if metadata.uid() != uid {
        return Err(LeaseRejection::DirectoryOwner);
    }
    if metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(LeaseRejection::DirectoryMode);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_directory(_path: &Path, _uid: u32) -> std::result::Result<(), LeaseRejection> {
    Err(LeaseRejection::InvalidDirectory)
}

fn read_one_lease(
    path: &Path,
    file_name: Option<&str>,
    now_ms: i64,
    inspector: &dyn ProcessInspector,
    limits: &PresenceReadLimits,
) -> std::result::Result<NormalizedPresenceLease, LeaseRejection> {
    let file_name = file_name.ok_or(LeaseRejection::InvalidFileName)?;
    if file_name.len() > 255 || !file_name.ends_with(".json") {
        return Err(LeaseRejection::InvalidFileName);
    }
    let mut file = open_nofollow(path).map_err(|_| LeaseRejection::OpenFailed)?;
    validate_file(&file, limits.expected_uid, limits.max_payload_bytes)?;
    let mut payload = Vec::new();
    file.by_ref()
        .take(limits.max_payload_bytes + 1)
        .read_to_end(&mut payload)
        .map_err(|_| LeaseRejection::OpenFailed)?;
    if payload.is_empty() {
        return Err(LeaseRejection::Empty);
    }
    if payload.len() as u64 > limits.max_payload_bytes {
        return Err(LeaseRejection::Oversized);
    }
    let lease: NormalizedPresenceLease =
        serde_json::from_slice(&payload).map_err(|_| LeaseRejection::MalformedJson)?;
    validate_lease(&lease, file_name, now_ms, inspector, limits)?;
    Ok(lease)
}

#[cfg(unix)]
fn open_nofollow(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    // Linux O_NOFOLLOW. Presence files are a Linux user-runtime transport.
    const O_NOFOLLOW: i32 = 0x20_000;
    OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_nofollow(_path: &Path) -> io::Result<File> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "presence leases require Unix no-follow file opens",
    ))
}

#[cfg(unix)]
fn validate_file(file: &File, uid: u32, max_bytes: u64) -> std::result::Result<(), LeaseRejection> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = file
        .metadata()
        .map_err(|_| LeaseRejection::NotRegularFile)?;
    if !metadata.file_type().is_file() {
        return Err(LeaseRejection::NotRegularFile);
    }
    if metadata.uid() != uid {
        return Err(LeaseRejection::FileOwner);
    }
    if metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(LeaseRejection::FileMode);
    }
    if metadata.len() == 0 {
        return Err(LeaseRejection::Empty);
    }
    if metadata.len() > max_bytes {
        return Err(LeaseRejection::Oversized);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_file(
    _file: &File,
    _uid: u32,
    _max_bytes: u64,
) -> std::result::Result<(), LeaseRejection> {
    Err(LeaseRejection::NotRegularFile)
}

fn validate_lease(
    lease: &NormalizedPresenceLease,
    file_name: &str,
    now_ms: i64,
    inspector: &dyn ProcessInspector,
    limits: &PresenceReadLimits,
) -> std::result::Result<(), LeaseRejection> {
    if lease.schema_version != PRESENCE_SCHEMA_VERSION {
        return Err(LeaseRejection::InvalidSchema);
    }
    if !limits.allowed_harnesses.contains(&lease.harness) {
        return Err(LeaseRejection::UnexpectedHarness);
    }
    if !valid_token(&lease.harness, 32)
        || !valid_token(&lease.source_key, 64)
        || !valid_token(&lease.event_id, 128)
        || !valid_token(&lease.instance_id, 128)
        || !valid_bounded_text(&lease.process_start_identity, 128)
        || !valid_bounded_text(&lease.native_session_id, 256)
        || !valid_cwd(&lease.cwd)
        || lease.pid <= 0
    {
        return Err(LeaseRejection::InvalidField);
    }
    let expected_name = format!("{}-{}-{}.json", lease.harness, lease.pid, lease.instance_id);
    if file_name != expected_name {
        return Err(LeaseRejection::InvalidFileName);
    }
    if lease.observed_at < 0
        || lease.expires_at <= lease.observed_at
        || lease.expires_at - lease.observed_at > limits.max_ttl_ms
        || lease.observed_at > now_ms.saturating_add(limits.max_future_skew_ms)
    {
        return Err(LeaseRejection::InvalidTimestamp);
    }
    if lease.expires_at <= now_ms {
        return Err(LeaseRejection::Expired);
    }
    let exists = inspector
        .process_exists(lease.pid)
        .map_err(|_| LeaseRejection::ProcessInspectionFailed)?;
    if !exists {
        return Err(LeaseRejection::PidMissing);
    }
    let actual_identity = inspector
        .process_start_identity(lease.pid)
        .map_err(|_| LeaseRejection::ProcessInspectionFailed)?;
    match actual_identity.as_deref() {
        Some(actual) if actual == lease.process_start_identity => Ok(()),
        Some(_) => Err(LeaseRejection::ProcessIdentityMismatch),
        None => Err(LeaseRejection::ProcessInspectionFailed),
    }
}

fn valid_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_bounded_text(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.chars().any(|character| character.is_control())
}

fn valid_cwd(value: &str) -> bool {
    value.starts_with('/')
        && value.len() <= 4_096
        && !value.chars().any(|character| character.is_control())
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LeaseIdentity {
    harness: String,
    source_key: String,
    instance_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PresenceTransitionKind {
    Opened,
    StateChanged,
    SessionChanged,
    Closed,
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PresenceTransition {
    pub(crate) kind: PresenceTransitionKind,
    pub(crate) harness: String,
    pub(crate) source_key: String,
    pub(crate) instance_id: String,
    pub(crate) native_session_id: String,
    pub(crate) event_id: String,
    pub(crate) pid: i64,
    pub(crate) state: PresenceState,
    pub(crate) event_ts: i64,
}

impl PresenceTransition {
    pub(crate) fn event_key(&self) -> [u8; 32] {
        let mut digest = Sha256::new();
        for value in [
            self.harness.as_bytes(),
            self.source_key.as_bytes(),
            self.instance_id.as_bytes(),
            self.native_session_id.as_bytes(),
            self.event_id.as_bytes(),
            format!("{:?}", self.kind).as_bytes(),
        ] {
            digest.update(value.len().to_be_bytes());
            digest.update(value);
        }
        digest.finalize().into()
    }
}

pub(crate) fn derive_presence_transitions(
    previous: &[NormalizedPresenceLease],
    current: &[NormalizedPresenceLease],
    now_ms: i64,
) -> Vec<PresenceTransition> {
    let old = previous
        .iter()
        .map(|lease| (lease.identity(), lease))
        .collect::<BTreeMap<_, _>>();
    let new = current
        .iter()
        .map(|lease| (lease.identity(), lease))
        .collect::<BTreeMap<_, _>>();
    let mut transitions = Vec::new();

    for (identity, prior) in &old {
        if !new.contains_key(identity) && prior.state != PresenceState::Closed {
            transitions.push(transition_from(
                prior,
                if prior.expires_at <= now_ms {
                    PresenceTransitionKind::Expired
                } else {
                    PresenceTransitionKind::Closed
                },
                PresenceState::Closed,
                now_ms.min(prior.expires_at),
            ));
        }
    }
    for (identity, lease) in &new {
        let Some(prior) = old.get(identity) else {
            transitions.push(transition_from(
                lease,
                if lease.state == PresenceState::Closed {
                    PresenceTransitionKind::Closed
                } else {
                    PresenceTransitionKind::Opened
                },
                lease.state,
                lease.observed_at,
            ));
            continue;
        };
        let kind = if prior.native_session_id != lease.native_session_id {
            Some(PresenceTransitionKind::SessionChanged)
        } else if prior.state != lease.state {
            Some(if lease.state == PresenceState::Closed {
                PresenceTransitionKind::Closed
            } else {
                PresenceTransitionKind::StateChanged
            })
        } else {
            None
        };
        if let Some(kind) = kind {
            transitions.push(transition_from(lease, kind, lease.state, lease.observed_at));
        }
    }
    transitions
}

fn transition_from(
    lease: &NormalizedPresenceLease,
    kind: PresenceTransitionKind,
    state: PresenceState,
    event_ts: i64,
) -> PresenceTransition {
    PresenceTransition {
        kind,
        harness: lease.harness.clone(),
        source_key: lease.source_key.clone(),
        instance_id: lease.instance_id.clone(),
        native_session_id: lease.native_session_id.clone(),
        event_id: lease.event_id.clone(),
        pid: lease.pid,
        state,
        event_ts,
    }
}

fn presence_state_name(state: PresenceState) -> &'static str {
    match state {
        PresenceState::Idle => "idle",
        PresenceState::Busy => "busy",
        PresenceState::Settling => "settling",
        PresenceState::Closed => "closed",
    }
}

fn persisted_presence_leases(db: &Connection) -> Result<Vec<NormalizedPresenceLease>> {
    let mut statement = db.prepare(
        "SELECT harness,source_key,instance_id,pid,COALESCE(process_start_identity,''),native_session_id,COALESCE(cwd,''),state,observed_ts,expires_ts,COALESCE(event_id,'') FROM agent_presence_leases ORDER BY harness,source_key,instance_id",
    )?;
    let rows = statement
        .query_map([], |row| {
            let state: String = row.get(7)?;
            let state = match state.as_str() {
                "busy" => PresenceState::Busy,
                "settling" => PresenceState::Settling,
                "closed" => PresenceState::Closed,
                _ => PresenceState::Idle,
            };
            Ok(NormalizedPresenceLease {
                schema_version: PRESENCE_SCHEMA_VERSION,
                event_id: row.get(10)?,
                harness: row.get(0)?,
                source_key: row.get(1)?,
                instance_id: row.get(2)?,
                pid: row.get(3)?,
                process_start_identity: row.get(4)?,
                native_session_id: row.get(5)?,
                cwd: row.get(6)?,
                state,
                observed_at: row.get(8)?,
                expires_at: row.get(9)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub(crate) fn reconcile_persisted_presence(
    db: &mut Connection,
    current: &[NormalizedPresenceLease],
    now_ms: i64,
) -> Result<Vec<PresenceTransition>> {
    let previous = persisted_presence_leases(db)?;
    let transitions = derive_presence_transitions(&previous, current, now_ms);
    let previous_by_identity = previous
        .iter()
        .map(|lease| (lease.identity(), lease))
        .collect::<BTreeMap<_, _>>();
    let tx = db.transaction()?;
    for transition in &transitions {
        let lease = current
            .iter()
            .find(|lease| {
                lease.identity()
                    == LeaseIdentity {
                        harness: transition.harness.clone(),
                        source_key: transition.source_key.clone(),
                        instance_id: transition.instance_id.clone(),
                    }
            })
            .or_else(|| {
                previous_by_identity
                    .get(&LeaseIdentity {
                        harness: transition.harness.clone(),
                        source_key: transition.source_key.clone(),
                        instance_id: transition.instance_id.clone(),
                    })
                    .copied()
            });
        let event_key = transition.event_key();
        tx.execute(
            "INSERT OR IGNORE INTO agent_state_events(event_key,harness,source_key,native_session_id,event_ts,state,instance_id,pid,process_start_identity,lease_expires_ts,transition_kind) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
            params![event_key.as_slice(),transition.harness,transition.source_key,transition.native_session_id,transition.event_ts,presence_state_name(transition.state),transition.instance_id,transition.pid,lease.map(|value| value.process_start_identity.as_str()),lease.map(|value| value.expires_at),format!("{:?}",transition.kind).to_ascii_lowercase()],
        )?;
    }
    tx.execute("DELETE FROM agent_presence_leases", [])?;
    for lease in current
        .iter()
        .filter(|lease| lease.state != PresenceState::Closed && lease.expires_at > now_ms)
    {
        let session = lookup_exact_agent_session(
            &tx,
            &lease.harness,
            &lease.source_key,
            &lease.native_session_id,
        )?;
        tx.execute(
            "INSERT INTO agent_presence_leases(harness,source_key,instance_id,pid,process_start_identity,native_session_id,cwd_key,state,observed_ts,expires_ts,agent_session_id,event_id,cwd) VALUES(?,?,?,?,?,?,NULL,?,?,?,?,?,?)",
            params![lease.harness,lease.source_key,lease.instance_id,lease.pid,lease.process_start_identity,lease.native_session_id,presence_state_name(lease.state),lease.observed_at,lease.expires_at,session,lease.event_id,lease.cwd],
        )?;
    }
    tx.commit()?;
    Ok(transitions)
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedAgentPresence {
    pub(crate) harness: String,
    pub(crate) source_key: String,
    pub(crate) pid: i64,
    pub(crate) native_session_id: String,
    pub(crate) agent_session_id: Option<i64>,
    pub(crate) cwd: String,
    pub(crate) state: PresenceState,
    pub(crate) match_method: &'static str,
    pub(crate) confidence: f64,
}

/// Exact focus attribution intentionally uses the currently active native session ID.
/// It never follows parent sessions or usage-event origin_session_id.
pub(crate) fn resolve_exact_presence(
    db: &Connection,
    lease: &NormalizedPresenceLease,
) -> Result<ResolvedAgentPresence> {
    let agent_session_id = lookup_exact_agent_session(
        db,
        &lease.harness,
        &lease.source_key,
        &lease.native_session_id,
    )?;
    Ok(ResolvedAgentPresence {
        harness: lease.harness.clone(),
        source_key: lease.source_key.clone(),
        pid: lease.pid,
        native_session_id: lease.native_session_id.clone(),
        agent_session_id,
        cwd: lease.cwd.clone(),
        state: lease.state,
        match_method: EXACT_PRESENCE_METHOD,
        confidence: 1.0,
    })
}

pub(crate) fn lookup_exact_agent_session(
    db: &Connection,
    harness: &str,
    source_key: &str,
    native_session_id: &str,
) -> Result<Option<i64>> {
    db.query_row(
        "SELECT se.id FROM agent_sessions se
         JOIN agent_sources source ON source.id=se.source_id
         WHERE source.agent_type=? AND source.source_key=? AND se.native_session_id=?",
        params![harness, source_key, native_session_id],
        |row| row.get(0),
    )
    .optional()
    .context("look up exact native agent session")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentProcessSurface {
    pub(crate) pid: i64,
    pub(crate) executable: String,
    pub(crate) cwd: PathBuf,
    pub(crate) process_started_at: Option<i64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HarnessExecutable {
    pub(crate) harness: &'static str,
    pub(crate) executable: &'static str,
}

pub(crate) const GENERIC_HARNESS_EXECUTABLES: &[HarnessExecutable] = &[
    HarnessExecutable {
        harness: "pi",
        executable: "pi",
    },
    HarnessExecutable {
        harness: "claude",
        executable: "claude",
    },
];

pub(crate) fn harness_for_executable(executable: &str) -> Option<&'static str> {
    let basename = Path::new(executable)
        .file_name()
        .and_then(|value| value.to_str())?;
    GENERIC_HARNESS_EXECUTABLES
        .iter()
        .find(|surface| surface.executable == basename)
        .map(|surface| surface.harness)
}

pub(crate) fn inspect_agent_process_surface(
    inspector: &dyn ProcessInspector,
    pid: i64,
    executable: &str,
    process_started_at: Option<i64>,
) -> io::Result<Option<AgentProcessSurface>> {
    if pid <= 0 || harness_for_executable(executable).is_none() || !inspector.process_exists(pid)? {
        return Ok(None);
    }
    let Some(cwd) = inspector.process_cwd(pid)? else {
        return Ok(None);
    };
    Ok(Some(AgentProcessSurface {
        pid,
        executable: executable.to_string(),
        cwd,
        process_started_at,
    }))
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FallbackPolicy {
    pub(crate) recent_within_ms: i64,
    pub(crate) future_skew_ms: i64,
    pub(crate) uniqueness_margin_ms: i64,
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        Self {
            recent_within_ms: 5 * 60_000,
            future_skew_ms: 5_000,
            uniqueness_margin_ms: 60_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FallbackResolution {
    NotAgent,
    Unindexed { harness: String },
    Ambiguous { harness: String, candidates: usize },
    Matched(ResolvedAgentPresence),
}

/// Low-confidence fallback for installs without a presence extension. Project path
/// and recency may identify one session, but an open JSONL file descriptor is never
/// treated as an exact source.
pub(crate) fn resolve_process_fallback(
    db: &Connection,
    process: &AgentProcessSurface,
    observed_at: i64,
    policy: FallbackPolicy,
) -> Result<FallbackResolution> {
    let Some(harness) = harness_for_executable(&process.executable) else {
        return Ok(FallbackResolution::NotAgent);
    };
    let Some(cwd) = process.cwd.to_str() else {
        return Ok(FallbackResolution::Unindexed {
            harness: harness.to_string(),
        });
    };
    let recent_cutoff = observed_at.saturating_sub(policy.recent_within_ms.max(1));
    let process_cutoff = process
        .process_started_at
        .map(|started| started.saturating_sub(policy.future_skew_ms.max(0)))
        .unwrap_or(i64::MIN);
    let upper = observed_at.saturating_add(policy.future_skew_ms.max(0));
    let mut statement = db.prepare(
        "SELECT se.id,se.native_session_id,
                COALESCE(se.last_activity_ts,se.first_activity_ts,se.created_ts) AS recent_ts
         FROM agent_sessions se
         JOIN agent_sources source ON source.id=se.source_id
         WHERE source.agent_type=? AND source.source_key=? AND se.project_path=?
           AND se.created_ts<=?
           AND COALESCE(se.last_activity_ts,se.first_activity_ts,se.created_ts)>=?
           AND COALESCE(se.last_activity_ts,se.first_activity_ts,se.created_ts)>=?
         ORDER BY recent_ts DESC,se.created_ts DESC,se.id DESC
         LIMIT 3",
    )?;
    let candidates = statement
        .query_map(
            params![
                harness,
                DEFAULT_SOURCE_KEY,
                cwd,
                upper,
                recent_cutoff,
                process_cutoff
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if candidates.is_empty() {
        return Ok(FallbackResolution::Unindexed {
            harness: harness.to_string(),
        });
    }
    let unique = candidates.len() == 1
        || candidates[0].2.saturating_sub(candidates[1].2) >= policy.uniqueness_margin_ms.max(1);
    if !unique {
        return Ok(FallbackResolution::Ambiguous {
            harness: harness.to_string(),
            candidates: candidates.len(),
        });
    }
    let (session_id, native_session_id, _) = &candidates[0];
    Ok(FallbackResolution::Matched(ResolvedAgentPresence {
        harness: harness.to_string(),
        source_key: DEFAULT_SOURCE_KEY.to_string(),
        pid: process.pid,
        native_session_id: native_session_id.clone(),
        agent_session_id: Some(*session_id),
        cwd: cwd.to_string(),
        state: PresenceState::Idle,
        match_method: CWD_RECENT_SESSION_METHOD,
        confidence: 0.55,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, os::unix::fs::PermissionsExt};

    #[derive(Default)]
    struct FakeInspector {
        identities: HashMap<i64, String>,
        cwds: HashMap<i64, PathBuf>,
    }

    impl ProcessInspector for FakeInspector {
        fn process_exists(&self, pid: i64) -> io::Result<bool> {
            Ok(self.identities.contains_key(&pid))
        }

        fn process_start_identity(&self, pid: i64) -> io::Result<Option<String>> {
            Ok(self.identities.get(&pid).cloned())
        }

        fn process_cwd(&self, pid: i64) -> io::Result<Option<PathBuf>> {
            Ok(self.cwds.get(&pid).cloned())
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bms-watchdog-presence-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn lease(state: PresenceState, observed_at: i64) -> NormalizedPresenceLease {
        NormalizedPresenceLease {
            schema_version: 1,
            event_id: format!("event-{observed_at}"),
            harness: "pi".into(),
            source_key: "local".into(),
            instance_id: "instance-one".into(),
            pid: 42,
            process_start_identity: "linux:99".into(),
            native_session_id: "native-current".into(),
            cwd: "/workspace/current".into(),
            state,
            observed_at,
            expires_at: observed_at + 45_000,
        }
    }

    fn write_lease(directory: &Path, lease: &NormalizedPresenceLease) -> PathBuf {
        let path = directory.join(format!(
            "{}-{}-{}.json",
            lease.harness, lease.pid, lease.instance_id
        ));
        fs::write(&path, serde_json::to_vec(lease).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn limits(directory: &Path) -> PresenceReadLimits {
        use std::os::unix::fs::MetadataExt;
        PresenceReadLimits::for_uid(fs::metadata(directory).unwrap().uid())
    }

    fn inspector() -> FakeInspector {
        FakeInspector {
            identities: HashMap::from([(42, "linux:99".into())]),
            cwds: HashMap::from([(42, PathBuf::from("/workspace/current"))]),
        }
    }

    #[test]
    fn linux_stat_parser_handles_spaces_and_parentheses() {
        let stat = "42 (a tricky ) name) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 987 20 21";
        assert_eq!(parse_linux_start_ticks(stat), Some(987));
    }

    #[test]
    fn secure_reader_accepts_only_live_same_process_leases() {
        let directory = temp_dir("valid");
        write_lease(&directory, &lease(PresenceState::Busy, 1_000));
        let report = read_presence_dir(&directory, 2_000, &inspector(), &limits(&directory));
        assert_eq!(report.leases.len(), 1);
        assert_eq!(report.leases[0].state, PresenceState::Busy);
        assert!(report.rejected.is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn ownership_and_entry_limits_fail_closed() {
        use std::os::unix::fs::MetadataExt;

        let directory = temp_dir("owner-limit");
        let actual_uid = fs::metadata(&directory).unwrap().uid();
        assert_eq!(
            validate_directory(&directory, actual_uid.saturating_add(1)),
            Err(LeaseRejection::DirectoryOwner)
        );
        let path = write_lease(&directory, &lease(PresenceState::Idle, 1_000));
        let file = File::open(path).unwrap();
        assert_eq!(
            validate_file(&file, actual_uid.saturating_add(1), 16 * 1024),
            Err(LeaseRejection::FileOwner)
        );
        let mut bounded = limits(&directory);
        bounded.max_files = 0;
        let report = read_presence_dir(&directory, 2_000, &inspector(), &bounded);
        assert!(report.entry_limit_reached);
        assert_eq!(report.rejected[0].reason, LeaseRejection::EntryLimit);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn secure_reader_rejects_modes_symlinks_oversize_and_pid_reuse() {
        let directory = temp_dir("reject");
        let path = write_lease(&directory, &lease(PresenceState::Idle, 1_000));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let report = read_presence_dir(&directory, 2_000, &inspector(), &limits(&directory));
        assert_eq!(report.rejected[0].reason, LeaseRejection::FileMode);

        fs::remove_file(&path).unwrap();
        let target = directory.join("target");
        fs::write(&target, b"{}").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let report = read_presence_dir(&directory, 2_000, &inspector(), &limits(&directory));
        assert!(report
            .rejected
            .iter()
            .any(|rejected| rejected.reason == LeaseRejection::OpenFailed));

        fs::remove_file(&path).unwrap();
        fs::remove_file(&target).unwrap();
        let mut reused = lease(PresenceState::Idle, 1_000);
        reused.process_start_identity = "linux:old".into();
        write_lease(&directory, &reused);
        let report = read_presence_dir(&directory, 2_000, &inspector(), &limits(&directory));
        assert_eq!(
            report.rejected[0].reason,
            LeaseRejection::ProcessIdentityMismatch
        );

        fs::remove_file(directory.join("pi-42-instance-one.json")).unwrap();
        let oversized = directory.join("oversized.json");
        fs::write(&oversized, vec![b'x'; 16 * 1024 + 1]).unwrap();
        fs::set_permissions(&oversized, fs::Permissions::from_mode(0o600)).unwrap();
        let report = read_presence_dir(&directory, 2_000, &inspector(), &limits(&directory));
        assert_eq!(report.rejected[0].reason, LeaseRejection::Oversized);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_and_content_bearing_unknown_fields_are_rejected() {
        let directory = temp_dir("schema");
        let mut stale = lease(PresenceState::Idle, 1_000);
        stale.expires_at = 2_000;
        write_lease(&directory, &stale);
        let report = read_presence_dir(&directory, 2_000, &inspector(), &limits(&directory));
        assert_eq!(report.rejected[0].reason, LeaseRejection::Expired);

        let path = directory.join("pi-42-instance-one.json");
        let mut value = serde_json::to_value(lease(PresenceState::Idle, 5_000)).unwrap();
        value["prompt"] = serde_json::json!("must not be accepted");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let report = read_presence_dir(&directory, 5_100, &inspector(), &limits(&directory));
        assert_eq!(report.rejected[0].reason, LeaseRejection::MalformedJson);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn heartbeats_do_not_create_transitions_but_state_and_expiry_do() {
        let idle = lease(PresenceState::Idle, 1_000);
        let heartbeat = lease(PresenceState::Idle, 2_000);
        assert!(derive_presence_transitions(&[idle.clone()], &[heartbeat], 2_000).is_empty());
        let busy = lease(PresenceState::Busy, 3_000);
        let transitions = derive_presence_transitions(&[idle.clone()], &[busy], 3_000);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].kind, PresenceTransitionKind::StateChanged);
        let transitions = derive_presence_transitions(&[idle], &[], 50_000);
        assert_eq!(transitions[0].kind, PresenceTransitionKind::Expired);
    }

    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE agent_sources (
               id INTEGER PRIMARY KEY, agent_type TEXT NOT NULL, source_key TEXT NOT NULL,
               UNIQUE(agent_type,source_key));
             CREATE TABLE agent_sessions (
               id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL, native_session_id TEXT NOT NULL,
               project_path TEXT, created_ts INTEGER NOT NULL, first_activity_ts INTEGER,
               last_activity_ts INTEGER, UNIQUE(source_id,native_session_id));",
        )
        .unwrap();
        db
    }

    #[test]
    fn exact_lookup_uses_harness_source_and_current_native_id_only() {
        let db = database();
        db.execute("INSERT INTO agent_sources VALUES (1,'pi','local')", [])
            .unwrap();
        db.execute("INSERT INTO agent_sources VALUES (2,'pi','remote')", [])
            .unwrap();
        db.execute("INSERT INTO agent_sources VALUES (3,'claude','local')", [])
            .unwrap();
        for row in [
            (10, 1, "current", "/work", 1),
            (11, 1, "parent", "/work", 1),
            (12, 2, "current", "/work", 1),
            (13, 3, "current", "/work", 1),
        ] {
            db.execute(
                "INSERT INTO agent_sessions (id,source_id,native_session_id,project_path,created_ts) VALUES (?,?,?,?,?)",
                params![row.0, row.1, row.2, row.3, row.4],
            )
            .unwrap();
        }
        assert_eq!(
            lookup_exact_agent_session(&db, "pi", "local", "current").unwrap(),
            Some(10)
        );
        assert_eq!(
            lookup_exact_agent_session(&db, "pi", "local", "not-indexed").unwrap(),
            None
        );
    }

    #[test]
    fn fallback_is_unique_recent_only_and_exposes_claude_generically() {
        assert_eq!(harness_for_executable("/usr/bin/pi"), Some("pi"));
        assert_eq!(harness_for_executable("claude"), Some("claude"));
        assert_eq!(harness_for_executable("pi-helper"), None);
        assert_eq!(
            inspect_agent_process_surface(&inspector(), 42, "pi", Some(8_000))
                .unwrap()
                .unwrap()
                .cwd,
            PathBuf::from("/workspace/current")
        );
        let db = database();
        db.execute("INSERT INTO agent_sources VALUES (1,'pi','local')", [])
            .unwrap();
        db.execute("INSERT INTO agent_sources VALUES (2,'claude','local')", [])
            .unwrap();
        db.execute("INSERT INTO agent_sessions (id,source_id,native_session_id,project_path,created_ts,last_activity_ts) VALUES (10,1,'recent','/work',1000,9900)", []).unwrap();
        let process = AgentProcessSurface {
            pid: 42,
            executable: "pi".into(),
            cwd: PathBuf::from("/work"),
            process_started_at: Some(8_000),
        };
        let matched =
            resolve_process_fallback(&db, &process, 10_000, FallbackPolicy::default()).unwrap();
        assert!(matches!(matched, FallbackResolution::Matched(_)));

        db.execute("INSERT INTO agent_sessions (id,source_id,native_session_id,project_path,created_ts,last_activity_ts) VALUES (11,1,'also-recent','/work',1000,9890)", []).unwrap();
        let ambiguous =
            resolve_process_fallback(&db, &process, 10_000, FallbackPolicy::default()).unwrap();
        assert!(matches!(
            ambiguous,
            FallbackResolution::Ambiguous { candidates: 2, .. }
        ));

        let claude = AgentProcessSurface {
            executable: "claude".into(),
            ..process
        };
        assert!(matches!(
            resolve_process_fallback(&db, &claude, 10_000, FallbackPolicy::default()).unwrap(),
            FallbackResolution::Unindexed { harness } if harness == "claude"
        ));
    }

    #[test]
    fn restart_reconstructs_missing_and_expired_busy_leases_with_bounded_events() {
        fn full_db() -> Connection {
            let mut db = Connection::open_in_memory().unwrap();
            crate::db::init_db(&mut db).unwrap();
            db
        }
        let busy = lease(PresenceState::Busy, 1_000);
        let mut missing_db = full_db();
        reconcile_persisted_presence(&mut missing_db, &[busy.clone()], 2_000).unwrap();
        let transitions = reconcile_persisted_presence(&mut missing_db, &[], 3_000).unwrap();
        assert_eq!(transitions[0].kind, PresenceTransitionKind::Closed);
        assert_eq!(transitions[0].event_ts, 3_000);
        let bounded: i64 = missing_db
            .query_row(
                "SELECT bounded_end_ts FROM bounded_agent_state_events WHERE state='busy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bounded, 3_000);

        let mut expired_db = full_db();
        reconcile_persisted_presence(&mut expired_db, &[busy.clone()], 2_000).unwrap();
        let transitions =
            reconcile_persisted_presence(&mut expired_db, &[], busy.expires_at + 1_000).unwrap();
        assert_eq!(transitions[0].kind, PresenceTransitionKind::Expired);
        assert_eq!(transitions[0].event_ts, busy.expires_at);
        let bounded: i64 = expired_db
            .query_row(
                "SELECT bounded_end_ts FROM bounded_agent_state_events WHERE state='busy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bounded, busy.expires_at);
    }

    #[test]
    fn live_pid_with_unverifiable_start_identity_fails_closed() {
        struct Unverifiable;
        impl ProcessInspector for Unverifiable {
            fn process_exists(&self, _pid: i64) -> io::Result<bool> {
                Ok(true)
            }
            fn process_start_identity(&self, _pid: i64) -> io::Result<Option<String>> {
                Ok(None)
            }
            fn process_cwd(&self, _pid: i64) -> io::Result<Option<PathBuf>> {
                Ok(Some(PathBuf::from("/workspace")))
            }
        }
        let directory = temp_dir("unverifiable");
        write_lease(&directory, &lease(PresenceState::Idle, 1_000));
        let report = read_presence_dir(&directory, 2_000, &Unverifiable, &limits(&directory));
        assert!(report.leases.is_empty());
        assert_eq!(
            report.rejected[0].reason,
            LeaseRejection::ProcessInspectionFailed
        );
        let _ = fs::remove_dir_all(directory);
    }
}
