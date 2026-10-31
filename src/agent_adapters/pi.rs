use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    agent_telemetry::{
        AgentCapabilities, AgentDescriptor, AgentScanResult, AgentScanState, AgentSource,
        AgentTelemetryAdapter, AgentTelemetrySink, CostKind, NormalizedAgentSession,
        NormalizedCost, NormalizedTokenUsage, NormalizedUsageEvent, SessionUsageRef,
        SqliteTelemetrySink,
    },
    util::now_ms,
};

pub(crate) struct PiAdapter {
    roots: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct PiFile {
    path: PathBuf,
    file_key: String,
    native_session_id: String,
    parent_native_path: Option<String>,
    parent_native_session_id: Option<String>,
    project_path: Option<String>,
    created_ts: i64,
}

#[derive(Debug)]
struct ScanFileState {
    file_identity: Option<String>,
    file_size: u64,
    file_mtime_ms: i64,
    scan_offset: u64,
    checkpoint_offset: Option<u64>,
    checkpoint_hash: Option<Vec<u8>>,
}

#[derive(Debug)]
struct PendingUsage {
    native_entry_id: String,
    event: NormalizedUsageEvent,
}

#[derive(Debug)]
struct ParsedPiSegment {
    scan_offset: u64,
    checkpoint_offset: Option<u64>,
    checkpoint_hash: Option<Vec<u8>>,
    name: Option<String>,
    first_activity_ts: Option<i64>,
    last_activity_ts: Option<i64>,
    malformed_lines: usize,
    incomplete_trailing_line: bool,
    events: Vec<PendingUsage>,
}

const APPLY_CHUNK_SIZE: usize = 250;

impl PiAdapter {
    pub(crate) fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    pub(crate) fn scan_source(
        db: &mut Connection,
        source_id: i64,
        source: &AgentSource,
        descriptor: &AgentDescriptor,
    ) -> Result<AgentScanResult> {
        let paths = discover_jsonl_files(&source.roots)?;
        let mut cached = load_cached_files(db, source_id)?;
        let mut changed = HashSet::new();
        let mut files = Vec::new();
        let mut by_path = HashMap::new();
        for path in paths {
            let key = path_key(&path);
            let cached_file = cached.remove(&key).and_then(|(state, file)| {
                let metadata = fs::metadata(&path).ok()?;
                let mtime = metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as i64;
                (metadata.len() == state.0 && mtime == state.1 && file_identity(&metadata) == state.2).then_some(file)
            });
            let header = match cached_file {
                Some(file) => Ok(file),
                None => { changed.insert(key); read_header(&path) }
            };
            match header {
                Ok(mut file) => {
                    by_path.insert(path_key(&path), file.native_session_id.clone());
                    if let Ok(canonical) = fs::canonicalize(&path) {
                        by_path.insert(path_key(&canonical), file.native_session_id.clone());
                    }
                    file.file_key = path_key(&path);
                    files.push(file);
                }
                Err(err) => eprintln!("[bms-watchdog] skip Pi session {}: {err:#}", path.display()),
            }
        }
        for file in &mut files {
            let previous_parent = file.parent_native_session_id.clone();
            file.parent_native_session_id = file.parent_native_path.as_ref().and_then(|parent| {
                by_path
                    .get(&path_key(Path::new(parent)))
                    .cloned()
                    .or_else(|| {
                        fs::canonicalize(parent)
                            .ok()
                            .and_then(|path| by_path.get(&path_key(&path)).cloned())
                    })
            });
            if file.parent_native_session_id != previous_parent {
                changed.insert(file.file_key.clone());
            }
        }
        sort_parent_first(&mut files);

        // Register changed/new headers before messages so copied events can point
        // at a parent. Unchanged parent headers are already in the store.
        if !changed.is_empty() {
            let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            {
                let mut sink = SqliteTelemetrySink::new(
                    &tx,
                    source_id,
                    &descriptor.agent_type,
                    &source.source_key,
                );
                for file in files.iter().filter(|file| changed.contains(&file.file_key)) {
                    sink.upsert_session(file.session(None, None, None))?;
                }
            }
            tx.commit()?;
        }

        let mut events = 0;
        let mut malformed_lines = 0;
        for file in files.iter().filter(|file| changed.contains(&file.file_key)) {
            let result = scan_file(db, source_id, source, descriptor, file)
                .with_context(|| format!("scan Pi session {}", file.path.display()));
            match result {
                Ok((new_events, malformed)) => {
                    events += new_events;
                    malformed_lines += malformed;
                }
                Err(err) => {
                    db.execute(
                        "INSERT INTO agent_scan_files (source_id,adapter_file_key,native_session_id,file_path,last_scanned_ts,last_error)
                         VALUES (?,?,?,?,?,?) ON CONFLICT(source_id,adapter_file_key) DO UPDATE SET
                         native_session_id=excluded.native_session_id,file_path=excluded.file_path,last_scanned_ts=excluded.last_scanned_ts,last_error=excluded.last_error",
                        params![source_id, file.file_key, file.native_session_id, file.path.display().to_string(), now_ms(), err.to_string()],
                    )?;
                    eprintln!("[bms-watchdog] {err:#}");
                }
            }
        }
        if !changed.is_empty() {
            db.execute(
                "DELETE FROM agent_usage_events WHERE source_id=? AND id NOT IN (SELECT usage_event_id FROM agent_session_usage_refs)",
                params![source_id],
            )?;
        }
        Ok(AgentScanResult {
            files: files.len(),
            events,
            malformed_lines,
        })
    }
}

type CachedPiFiles = HashMap<String, ((u64, i64, Option<String>), PiFile)>;

fn load_cached_files(db: &Connection, source_id: i64) -> Result<CachedPiFiles> {
    let mut stmt = db.prepare(
        "SELECT f.adapter_file_key,f.file_path,f.file_size,f.file_mtime_ms,f.file_identity,
         s.native_session_id,s.parent_native_path,s.parent_native_session_id,s.project_path,s.created_ts
         FROM agent_scan_files f JOIN agent_sessions s ON s.source_id=f.source_id AND s.native_session_id=f.native_session_id
         WHERE f.source_id=? AND f.last_error IS NULL")?;
    let rows = stmt.query_map(params![source_id], |row| {
        let key: String = row.get(0)?;
        Ok((key.clone(), ((row.get(2)?, row.get(3)?, row.get(4)?), PiFile {
            path: PathBuf::from(row.get::<_, String>(1)?), file_key: key,
            native_session_id: row.get(5)?, parent_native_path: row.get(6)?,
            parent_native_session_id: row.get(7)?, project_path: row.get(8)?, created_ts: row.get(9)?,
        })))
    })?.collect::<rusqlite::Result<HashMap<_, _>>>()?;
    Ok(rows)
}

impl AgentTelemetryAdapter for PiAdapter {
    fn descriptor(&self) -> AgentDescriptor {
        AgentDescriptor {
            agent_type: "pi".to_string(),
            display_name: "Pi".to_string(),
            adapter_version: 1,
            capabilities: AgentCapabilities {
                historical_import: true,
                live_updates: true,
                input_tokens: true,
                output_tokens: true,
                reasoning_tokens: true,
                cache_tokens: true,
                cost: true,
                session_forks: true,
            },
            limitations: vec![
                "Internal compaction and branch-summary model usage may not be recorded by Pi."
                    .to_string(),
            ],
        }
    }

    fn discover(&self) -> Result<Vec<AgentSource>> {
        let roots = self
            .roots
            .iter()
            .filter(|root| root.is_dir())
            .cloned()
            .collect::<Vec<_>>();
        if roots.is_empty() {
            Ok(vec![])
        } else {
            Ok(vec![AgentSource {
                source_key: "local".to_string(),
                roots,
            }])
        }
    }

    fn scan(
        &self,
        _source: &AgentSource,
        state: Option<&AgentScanState>,
        _sink: &mut dyn AgentTelemetrySink,
    ) -> Result<AgentScanResult> {
        if let Some(state) = state {
            let _ = (
                &state.file_key,
                state.scan_offset,
                state.checkpoint_offset,
                &state.checkpoint_hash,
            );
        }
        Ok(AgentScanResult {
            files: 0,
            events: 0,
            malformed_lines: 0,
        })
    }

    fn scan_persisted(
        &self,
        db: &mut Connection,
        source_id: i64,
        source: &AgentSource,
        descriptor: &AgentDescriptor,
    ) -> Result<AgentScanResult> {
        Self::scan_source(db, source_id, source, descriptor)
    }
}

impl PiFile {
    fn session(
        &self,
        name: Option<String>,
        first: Option<i64>,
        last: Option<i64>,
    ) -> NormalizedAgentSession {
        NormalizedAgentSession {
            native_session_id: self.native_session_id.clone(),
            parent_native_session_id: self.parent_native_session_id.clone(),
            parent_native_path: self.parent_native_path.clone(),
            name,
            project_path: self.project_path.clone(),
            created_ts: self.created_ts,
            first_activity_ts: first,
            last_activity_ts: last,
        }
    }
}

fn discover_jsonl_files(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut output = Vec::new();
    for root in roots {
        visit(root, &mut output)?;
    }
    output.sort();
    output.dedup();
    Ok(output)
}

fn visit(path: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            visit(&entry.path(), output)?;
        } else if kind.is_file()
            && entry.path().extension().and_then(|v| v.to_str()) == Some("jsonl")
        {
            output.push(entry.path());
        }
    }
    Ok(())
}

fn read_header(path: &Path) -> Result<PiFile> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let value: Value = serde_json::from_str(line.trim_end()).context("parse session header")?;
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return Err(anyhow!("first entry is not a session header"));
    }
    let native_session_id = value
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("session header has no id"))?
        .to_string();
    let created_ts = value
        .get("timestamp")
        .and_then(parse_timestamp)
        .ok_or_else(|| anyhow!("session header has invalid timestamp"))?;
    let parent_native_path = value
        .get("parentSession")
        .or_else(|| value.get("parentSessionPath"))
        .and_then(Value::as_str)
        .map(ToString::to_string);
    Ok(PiFile {
        path: path.to_path_buf(),
        file_key: String::new(),
        native_session_id,
        parent_native_path,
        parent_native_session_id: None,
        project_path: value
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        created_ts,
    })
}

fn sort_parent_first(files: &mut [PiFile]) {
    let parent_by_id = files
        .iter()
        .map(|f| {
            (
                f.native_session_id.clone(),
                f.parent_native_session_id.clone(),
            )
        })
        .collect::<HashMap<_, _>>();
    fn depth(
        id: &str,
        parents: &HashMap<String, Option<String>>,
        visiting: &mut HashSet<String>,
    ) -> usize {
        if !visiting.insert(id.to_string()) {
            return 0;
        }
        let result = parents
            .get(id)
            .and_then(|p| p.as_ref())
            .map(|p| 1 + depth(p, parents, visiting))
            .unwrap_or(0);
        visiting.remove(id);
        result
    }
    files.sort_by_key(|f| {
        (
            depth(&f.native_session_id, &parent_by_id, &mut HashSet::new()),
            f.created_ts,
        )
    });
}

fn scan_file(
    db: &mut Connection,
    source_id: i64,
    source: &AgentSource,
    descriptor: &AgentDescriptor,
    file: &PiFile,
) -> Result<(usize, usize)> {
    let metadata = fs::metadata(&file.path)?;
    let size = metadata.len();
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|v| v.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|v| v.as_millis() as i64)
        .unwrap_or(0);
    let identity = file_identity(&metadata);
    let state = load_scan_state(db, source_id, &file.file_key)?;
    if let Some(state) = &state {
        if state.file_size == size
            && state.file_mtime_ms == mtime_ms
            && state.file_identity == identity
        {
            return Ok((0, 0));
        }
    }
    let rewrite = match &state {
        Some(state) => {
            state.file_size > size
                || state.file_identity != identity
                || !checkpoint_matches(&file.path, state)?
        }
        None => false,
    };
    let start_offset = if rewrite {
        0
    } else {
        state.as_ref().map(|s| s.scan_offset).unwrap_or(0)
    };

    // File I/O, JSON parsing, fallback hashing, and normalization happen before
    // any SQLite write transaction. Parsed records contain no conversation/tool content.
    let prior_checkpoint = if rewrite { None } else { state.as_ref() };
    let parsed = parse_file_segment(&file.path, start_offset, prior_checkpoint)?;
    if rewrite && parsed.incomplete_trailing_line {
        // A truncate/rewrite may be observed while Pi is still writing it. Keep
        // the old session refs and checkpoint untouched until the rewrite is complete.
        return Ok((0, parsed.malformed_lines));
    }
    let current_entry_ids = parsed
        .events
        .iter()
        .map(|pending| pending.native_entry_id.as_str())
        .collect::<HashSet<_>>();
    let stale_entry_ids = if rewrite {
        load_stale_entry_ids(db, source_id, &file.native_session_id, &current_entry_ids)?
    } else {
        Vec::new()
    };

    for chunk in parsed.events.chunks(APPLY_CHUNK_SIZE) {
        apply_event_chunk(db, source_id, source, descriptor, file, chunk)?;
    }
    // Rewritten-file refs remain untouched until parsing and every replacement
    // event have succeeded. Delete only refs proven absent from the new file.
    delete_stale_refs(db, source_id, &file.native_session_id, &stale_entry_ids)?;
    finalize_scan_file(
        db, source_id, source, descriptor, file, &parsed, identity, size, mtime_ms,
    )?;
    Ok((parsed.events.len(), parsed.malformed_lines))
}

fn parse_file_segment(
    path: &Path,
    start_offset: u64,
    state: Option<&ScanFileState>,
) -> Result<ParsedPiSegment> {
    let mut reader = BufReader::new(File::open(path)?);
    reader.seek(SeekFrom::Start(start_offset))?;
    let mut parsed = ParsedPiSegment {
        scan_offset: start_offset,
        checkpoint_offset: state.and_then(|s| s.checkpoint_offset),
        checkpoint_hash: state.and_then(|s| s.checkpoint_hash.clone()),
        name: None,
        first_activity_ts: None,
        last_activity_ts: None,
        malformed_lines: 0,
        incomplete_trailing_line: false,
        events: Vec::new(),
    };
    loop {
        let line_offset = parsed.scan_offset;
        let mut line = String::new();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if !line.ends_with('\n') {
            // Pi may be writing this record. Keep the offset at its beginning and retry.
            parsed.incomplete_trailing_line = true;
            break;
        }
        parsed.scan_offset += bytes as u64;
        parsed.checkpoint_offset = Some(line_offset);
        parsed.checkpoint_hash =
            Some(hash_bytes(line.trim_end_matches(['\r', '\n']).as_bytes()).to_vec());
        let value: Value = match serde_json::from_str(line.trim_end()) {
            Ok(value) => value,
            Err(_) => {
                parsed.malformed_lines += 1;
                continue;
            }
        };
        if let Some(ts) = value.get("timestamp").and_then(parse_timestamp) {
            parsed.first_activity_ts =
                Some(parsed.first_activity_ts.map_or(ts, |old: i64| old.min(ts)));
            parsed.last_activity_ts =
                Some(parsed.last_activity_ts.map_or(ts, |old: i64| old.max(ts)));
        }
        if value.get("type").and_then(Value::as_str) == Some("session_info") {
            if let Some(next_name) = value
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                parsed.name = Some(next_name.to_string());
            }
            continue;
        }
        let Some(event) = parse_usage_event(&value) else {
            continue;
        };
        let native_entry_id = value
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .unwrap_or_else(|| short_hex(&event.native_event_key));
        parsed.events.push(PendingUsage {
            native_entry_id,
            event,
        });
    }
    Ok(parsed)
}

fn apply_event_chunk(
    db: &mut Connection,
    source_id: i64,
    source: &AgentSource,
    descriptor: &AgentDescriptor,
    file: &PiFile,
    events: &[PendingUsage],
) -> Result<()> {
    // Reserve the writer before the sink's no-op checks establish a snapshot.
    // A deferred read transaction cannot upgrade after a coalesced collector
    // commits, even with busy_timeout (SQLITE_BUSY_SNAPSHOT).
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    {
        let mut sink =
            SqliteTelemetrySink::new(&tx, source_id, &descriptor.agent_type, &source.source_key);
        let current_session_id = sink.upsert_session(file.session(None, None, None))?;
        let parent_session_id = match &file.parent_native_session_id {
            Some(id) => tx
                .query_row(
                    "SELECT id FROM agent_sessions WHERE source_id=? AND native_session_id=?",
                    params![source_id, id],
                    |row| row.get(0),
                )
                .optional()?,
            None => None,
        };
        for pending in events {
            let global_key = crate::agent_telemetry::event_identity(
                &descriptor.agent_type,
                &source.source_key,
                &pending.event.native_event_key,
            );
            let existing_origin: Option<(i64, i64)> = tx.query_row(
                "SELECT e.origin_session_id,s.created_ts FROM agent_usage_events e JOIN agent_sessions s ON s.id=e.origin_session_id WHERE e.source_id=? AND e.event_key=?",
                params![source_id, global_key.as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?;
            let inferred_origin = if pending.event.event_ts < file.created_ts {
                parent_session_id.unwrap_or(current_session_id)
            } else {
                current_session_id
            };
            let origin = existing_origin
                .map(|value| value.0)
                .unwrap_or(inferred_origin);
            let event_ts = pending.event.event_ts;
            let usage_event_id = sink.upsert_usage(pending.event.clone(), origin)?;
            if let Some((old_origin, old_created)) = existing_origin {
                if file.created_ts < old_created && event_ts >= file.created_ts {
                    tx.execute(
                        "UPDATE agent_usage_events SET origin_session_id=? WHERE id=?",
                        params![current_session_id, usage_event_id],
                    )?;
                } else if inferred_origin != old_origin
                    && event_ts < file.created_ts
                    && parent_session_id.is_some()
                {
                    tx.execute(
                        "UPDATE agent_usage_events SET origin_session_id=? WHERE id=?",
                        params![inferred_origin, usage_event_id],
                    )?;
                }
            }
            let final_origin: i64 = tx.query_row(
                "SELECT origin_session_id FROM agent_usage_events WHERE id=?",
                params![usage_event_id],
                |row| row.get(0),
            )?;
            sink.add_session_event_ref(SessionUsageRef {
                session_id: current_session_id,
                native_entry_id: pending.native_entry_id.clone(),
                usage_event_id,
                inherited: final_origin != current_session_id,
            })?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn load_stale_entry_ids(
    db: &Connection,
    source_id: i64,
    native_session_id: &str,
    current: &HashSet<&str>,
) -> Result<Vec<String>> {
    let mut stmt = db.prepare(
        "SELECT r.native_entry_id FROM agent_session_usage_refs r JOIN agent_sessions s ON s.id=r.session_id WHERE s.source_id=? AND s.native_session_id=?",
    )?;
    let existing = stmt
        .query_map(params![source_id, native_session_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(existing
        .into_iter()
        .filter(|entry_id| !current.contains(entry_id.as_str()))
        .collect())
}

fn delete_stale_refs(
    db: &mut Connection,
    source_id: i64,
    native_session_id: &str,
    stale_entry_ids: &[String],
) -> Result<()> {
    if stale_entry_ids.is_empty() {
        return Ok(());
    }
    let session_id: Option<i64> = db
        .query_row(
            "SELECT id FROM agent_sessions WHERE source_id=? AND native_session_id=?",
            params![source_id, native_session_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(session_id) = session_id else {
        return Ok(());
    };
    for chunk in stale_entry_ids.chunks(APPLY_CHUNK_SIZE) {
        let tx = db.transaction()?;
        {
            let mut delete = tx.prepare(
                "DELETE FROM agent_session_usage_refs WHERE session_id=? AND native_entry_id=?",
            )?;
            for entry_id in chunk {
                delete.execute(params![session_id, entry_id])?;
            }
        }
        tx.commit()?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finalize_scan_file(
    db: &mut Connection,
    source_id: i64,
    source: &AgentSource,
    descriptor: &AgentDescriptor,
    file: &PiFile,
    parsed: &ParsedPiSegment,
    identity: Option<String>,
    size: u64,
    mtime_ms: i64,
) -> Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    {
        let mut sink =
            SqliteTelemetrySink::new(&tx, source_id, &descriptor.agent_type, &source.source_key);
        sink.upsert_session(file.session(
            parsed.name.clone(),
            parsed.first_activity_ts,
            parsed.last_activity_ts,
        ))?;
    }
    tx.execute(
        "INSERT INTO agent_scan_files
         (source_id,adapter_file_key,native_session_id,file_path,file_identity,file_size,file_mtime_ms,scan_offset,checkpoint_offset,checkpoint_hash,last_scanned_ts,last_error)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(source_id,adapter_file_key) DO UPDATE SET
         native_session_id=excluded.native_session_id,file_path=excluded.file_path,file_identity=excluded.file_identity,
         file_size=excluded.file_size,file_mtime_ms=excluded.file_mtime_ms,scan_offset=excluded.scan_offset,
         checkpoint_offset=excluded.checkpoint_offset,checkpoint_hash=excluded.checkpoint_hash,
         last_scanned_ts=excluded.last_scanned_ts,last_error=excluded.last_error",
        params![
            source_id,
            file.file_key,
            file.native_session_id,
            file.path.display().to_string(),
            identity,
            size as i64,
            mtime_ms,
            parsed.scan_offset as i64,
            parsed.checkpoint_offset.map(|value| value as i64),
            parsed.checkpoint_hash,
            now_ms(),
            if parsed.malformed_lines > 0 {
                Some(format!(
                    "{} malformed complete line(s) skipped",
                    parsed.malformed_lines
                ))
            } else {
                None
            },
        ],
    )?;
    tx.commit()?;
    Ok(())
}

fn load_scan_state(
    db: &Connection,
    source_id: i64,
    file_key: &str,
) -> Result<Option<ScanFileState>> {
    Ok(db.query_row(
        "SELECT file_identity,file_size,file_mtime_ms,scan_offset,checkpoint_offset,checkpoint_hash FROM agent_scan_files WHERE source_id=? AND adapter_file_key=?",
        params![source_id, file_key],
        |row| Ok(ScanFileState {
            file_identity: row.get(0)?, file_size: row.get::<_, i64>(1)?.max(0) as u64,
            file_mtime_ms: row.get(2)?, scan_offset: row.get::<_, i64>(3)?.max(0) as u64,
            checkpoint_offset: row.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as u64), checkpoint_hash: row.get(5)?,
        }),
    ).optional()?)
}

fn checkpoint_matches(path: &Path, state: &ScanFileState) -> Result<bool> {
    let (Some(offset), Some(expected)) = (state.checkpoint_offset, state.checkpoint_hash.as_ref())
    else {
        return Ok(true);
    };
    let mut reader = BufReader::new(File::open(path)?);
    reader.seek(SeekFrom::Start(offset))?;
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 || !line.ends_with('\n') {
        return Ok(false);
    }
    Ok(
        hash_bytes(line.trim_end_matches(['\r', '\n']).as_bytes()).as_slice()
            == expected.as_slice(),
    )
}

fn parse_usage_event(entry: &Value) -> Option<NormalizedUsageEvent> {
    if entry.get("type")?.as_str()? != "message" {
        return None;
    }
    let message = entry.get("message")?;
    if message.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let usage = message.get("usage")?;
    let input = integer(usage.get("input"));
    let output = integer(usage.get("output"));
    let reasoning = integer(usage.get("reasoning"));
    let cache_read = integer(usage.get("cacheRead"));
    let cache_write = integer(usage.get("cacheWrite")).or_else(|| {
        let values = [
            integer(usage.get("cacheWrite5m")),
            integer(usage.get("cacheWrite1h")),
        ];
        values
            .iter()
            .any(Option::is_some)
            .then(|| values.into_iter().flatten().sum())
    });
    let total = integer(usage.get("totalTokens")).unwrap_or_else(|| {
        input
            .unwrap_or(0)
            .saturating_add(output.unwrap_or(0))
            .saturating_add(cache_read.unwrap_or(0))
            .saturating_add(cache_write.unwrap_or(0))
    });
    let provider = message
        .get("provider")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let native_event_key =
        if let Some(response_id) = message.get("responseId").and_then(Value::as_str) {
            let mut hash = Sha256::new();
            hash.update(provider.as_deref().unwrap_or_default().as_bytes());
            hash.update([0]);
            hash.update(response_id.as_bytes());
            hash.finalize().to_vec()
        } else {
            hash_bytes(serde_json::to_string(message).ok()?.as_bytes()).to_vec()
        };
    let cost = message
        .get("usage")
        .and_then(|u| u.get("cost"))
        .and_then(parse_cost);
    Some(NormalizedUsageEvent {
        native_event_key,
        event_ts: entry.get("timestamp").and_then(parse_timestamp)?,
        provider,
        api: message
            .get("api")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        model: message
            .get("model")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        stop_reason: message
            .get("stopReason")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        usage: NormalizedTokenUsage {
            input,
            output,
            reasoning,
            cache_read,
            cache_write,
            total,
        },
        cost,
    })
}

fn parse_cost(value: &Value) -> Option<NormalizedCost> {
    let input = number(value.get("input"));
    let output = number(value.get("output"));
    let cache_read = number(value.get("cacheRead"));
    let cache_write = number(value.get("cacheWrite")).or_else(|| {
        let values = [
            number(value.get("cacheWrite5m")),
            number(value.get("cacheWrite1h")),
        ];
        values
            .iter()
            .any(Option::is_some)
            .then(|| values.into_iter().flatten().sum())
    });
    let total = number(value.get("total")).or_else(|| {
        [input, output, cache_read, cache_write]
            .iter()
            .any(Option::is_some)
            .then(|| {
                [input, output, cache_read, cache_write]
                    .into_iter()
                    .flatten()
                    .sum()
            })
    })?;
    Some(NormalizedCost {
        input,
        output,
        cache_read,
        cache_write,
        total,
        currency: "USD".to_string(),
        kind: CostKind::Estimated,
    })
}

fn integer(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64)
}
fn number(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64)
}

fn parse_timestamp(value: &Value) -> Option<i64> {
    if let Some(ts) = value.as_i64() {
        return Some(ts);
    }
    let parsed = OffsetDateTime::parse(value.as_str()?, &Rfc3339).ok()?;
    Some((parsed.unix_timestamp_nanos() / 1_000_000) as i64)
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
fn short_hex(bytes: &[u8]) -> String {
    bytes.iter().take(12).map(|v| format!("{v:02x}")).collect()
}
fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(unix)]
fn file_identity(metadata: &fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(format!("{}:{}", metadata.dev(), metadata.ino()))
}
#[cfg(not(unix))]
fn file_identity(_metadata: &fs::Metadata) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn assistant(id: &str, response_id: &str, timestamp: i64, tokens: i64) -> String {
        format!(
            r#"{{"type":"message","id":"{id}","timestamp":{timestamp},"message":{{"role":"assistant","content":[{{"type":"text","text":"must-not-persist"}}],"provider":"fixture","responseId":"{response_id}","usage":{{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":{tokens},"cost":{{"total":0.1}}}}}}}}"#
        )
    }

    #[test]
    fn parsing_completes_while_battery_writer_lock_is_held() {
        let root = std::env::temp_dir().join(format!(
            "bms-watchdog-pi-import-lock-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let session_path = root.join("large-session.jsonl");
        let mut session = File::create(&session_path).unwrap();
        writeln!(session, "{{\"type\":\"session\",\"version\":3,\"id\":\"large-session\",\"timestamp\":1000,\"cwd\":\"/workspace/test\"}}").unwrap();
        for index in 0..2_000 {
            writeln!(
                session,
                "{}",
                assistant(
                    &format!("entry-{index}"),
                    &format!("response-{index}"),
                    2_000 + index,
                    10
                )
            )
            .unwrap();
        }
        drop(session);

        let db_path = root.join("watchdog.sqlite");
        let mut writer = Connection::open(&db_path).unwrap();
        crate::db::init_db(&mut writer).unwrap();
        let battery_tx = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        battery_tx
            .execute(
                "INSERT INTO battery_samples (ts,on_battery,status,source) VALUES (?,?,?,?)",
                params![1_i64, 1_i64, "Discharging", "fixture"],
            )
            .unwrap();

        // This completes deterministically while SQLite's main-database writer
        // lock is held because the parse phase accepts no database connection.
        let parsed = parse_file_segment(&session_path, 0, None).unwrap();
        assert_eq!(parsed.events.len(), 2_000);
        assert_eq!(
            battery_tx
                .query_row("SELECT COUNT(*) FROM battery_samples", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        battery_tx.commit().unwrap();
        drop(writer);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scanner_retries_trailing_lines_and_rebuilds_rewrites() {
        let root =
            std::env::temp_dir().join(format!("bms-watchdog-pi-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let header = r#"{"type":"session","version":3,"id":"session-one","timestamp":1000,"cwd":"/workspace/test"}"#;
        let first = assistant("entry-one", "response-one", 2000, 10);
        let trailing = assistant("entry-two", "response-two", 3000, 20);
        fs::write(
            &path,
            format!("{header}\n{first}\n{{malformed}}\n{trailing}"),
        )
        .unwrap();

        let mut db = Connection::open_in_memory().unwrap();
        crate::db::init_db(&mut db).unwrap();
        let adapter = PiAdapter::new(vec![root.clone()]);
        let descriptor = adapter.descriptor();
        db.execute(
            "INSERT INTO agent_sources (agent_type,source_key,display_name,adapter_version,capabilities_json,limitations_json) VALUES (?,?,?,?,?,?)",
            params![descriptor.agent_type, "local", descriptor.display_name, descriptor.adapter_version, serde_json::to_string(&descriptor.capabilities).unwrap(), serde_json::to_string(&descriptor.limitations).unwrap()],
        ).unwrap();
        let source_id = db.last_insert_rowid();
        let source = AgentSource {
            source_key: "local".into(),
            roots: vec![root.clone()],
        };

        let initial = PiAdapter::scan_source(&mut db, source_id, &source, &descriptor).unwrap();
        assert_eq!((initial.events, initial.malformed_lines), (1, 1));
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM agent_usage_events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file).unwrap();
        let appended = PiAdapter::scan_source(&mut db, source_id, &source, &descriptor).unwrap();
        assert_eq!(appended.events, 1);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM agent_usage_events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        let changes = db.total_changes();
        let sequence: i64 = db.query_row("SELECT seq FROM sqlite_sequence WHERE name='agent_sessions'", [], |r| r.get(0)).unwrap();
        assert_eq!(
            PiAdapter::scan_source(&mut db, source_id, &source, &descriptor)
                .unwrap()
                .events,
            0
        );
        assert_eq!(db.total_changes(), changes, "unchanged scans must not write");
        assert_eq!(db.query_row("SELECT seq FROM sqlite_sequence WHERE name='agent_sessions'", [], |r| r.get::<_, i64>(0)).unwrap(), sequence);

        let replacement = assistant("entry-three", "response-three", 4000, 30);
        fs::write(&path, format!("{header}\n{replacement}")).unwrap();
        let incomplete_rewrite =
            PiAdapter::scan_source(&mut db, source_id, &source, &descriptor).unwrap();
        assert_eq!(incomplete_rewrite.events, 0);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM agent_usage_events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file).unwrap();
        PiAdapter::scan_source(&mut db, source_id, &source, &descriptor).unwrap();
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM agent_usage_events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT total_tokens FROM agent_usage_events", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            30
        );
        let stored_content: i64 = db.query_row("SELECT COUNT(*) FROM agent_usage_events WHERE provider LIKE '%must-not-persist%' OR model LIKE '%must-not-persist%' OR stop_reason LIKE '%must-not-persist%'", [], |row| row.get(0)).unwrap();
        assert_eq!(stored_content, 0);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn parses_usage_without_retaining_content() {
        let entry: Value = serde_json::from_str(r#"{"type":"message","id":"abcd","timestamp":"2026-01-01T00:00:00Z","message":{"role":"assistant","content":[{"type":"text","text":"private"}],"provider":"test","responseId":"response-secret","usage":{"input":10,"output":5,"reasoning":2,"cacheRead":20,"cacheWrite":1,"totalTokens":36,"cost":{"total":0.1}},"stopReason":"stop"}}"#).unwrap();
        let event = parse_usage_event(&entry).unwrap();
        assert_eq!(event.usage.total, 36);
        assert_eq!(event.usage.reasoning, Some(2));
        assert_eq!(event.cost.as_ref().unwrap().total, 0.1);
        let debug = format!("{event:?}");
        assert!(!debug.contains("private"));
        assert!(!debug.contains("response-secret"));
    }

    #[test]
    fn fallback_identity_is_stable() {
        let entry: Value = serde_json::from_str(r#"{"type":"message","id":"one","timestamp":1,"message":{"role":"assistant","content":[{"type":"text","text":"x"}],"usage":{"totalTokens":3}}}"#).unwrap();
        assert_eq!(
            parse_usage_event(&entry).unwrap().native_event_key,
            parse_usage_event(&entry).unwrap().native_event_key
        );
    }

    #[test]
    fn cache_write_tiers_are_preserved_as_a_non_overlapping_total() {
        let entry: Value = serde_json::from_str(r#"{"type":"message","timestamp":1,"message":{"role":"assistant","usage":{"input":1,"output":2,"cacheWrite5m":3,"cacheWrite1h":4,"totalTokens":10}}}"#).unwrap();
        assert_eq!(
            parse_usage_event(&entry).unwrap().usage.cache_write,
            Some(7)
        );
    }
}
