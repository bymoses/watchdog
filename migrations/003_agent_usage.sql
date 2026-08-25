CREATE TABLE agent_sources (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  agent_type TEXT NOT NULL,
  source_key TEXT NOT NULL,
  display_name TEXT NOT NULL,
  adapter_version INTEGER NOT NULL,
  capabilities_json TEXT NOT NULL,
  limitations_json TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  initial_scan_complete INTEGER NOT NULL DEFAULT 0,
  last_scan_ts INTEGER,
  last_scan_error TEXT,
  UNIQUE(agent_type, source_key)
);

CREATE TABLE agent_sessions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id INTEGER NOT NULL,
  native_session_id TEXT NOT NULL,
  parent_native_session_id TEXT,
  parent_native_path TEXT,
  name TEXT,
  project_path TEXT,
  created_ts INTEGER NOT NULL,
  first_activity_ts INTEGER,
  last_activity_ts INTEGER,
  FOREIGN KEY(source_id) REFERENCES agent_sources(id) ON DELETE CASCADE,
  UNIQUE(source_id, native_session_id)
);

CREATE TABLE agent_usage_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id INTEGER NOT NULL,
  event_key BLOB NOT NULL CHECK(length(event_key) = 32),
  origin_session_id INTEGER,
  event_ts INTEGER NOT NULL,
  provider TEXT,
  api TEXT,
  model TEXT,
  stop_reason TEXT,
  input_tokens INTEGER,
  output_tokens INTEGER,
  reasoning_tokens INTEGER,
  cache_read_tokens INTEGER,
  cache_write_tokens INTEGER,
  total_tokens INTEGER NOT NULL,
  input_cost REAL,
  output_cost REAL,
  cache_read_cost REAL,
  cache_write_cost REAL,
  total_cost REAL,
  cost_currency TEXT,
  cost_kind TEXT,
  FOREIGN KEY(source_id) REFERENCES agent_sources(id) ON DELETE CASCADE,
  FOREIGN KEY(origin_session_id) REFERENCES agent_sessions(id) ON DELETE SET NULL,
  UNIQUE(source_id, event_key)
);

CREATE TABLE agent_session_usage_refs (
  session_id INTEGER NOT NULL,
  native_entry_id TEXT NOT NULL,
  usage_event_id INTEGER NOT NULL,
  inherited INTEGER NOT NULL,
  PRIMARY KEY(session_id, native_entry_id),
  FOREIGN KEY(session_id) REFERENCES agent_sessions(id) ON DELETE CASCADE,
  FOREIGN KEY(usage_event_id) REFERENCES agent_usage_events(id) ON DELETE CASCADE
);

CREATE TABLE agent_scan_files (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_id INTEGER NOT NULL,
  adapter_file_key TEXT NOT NULL,
  native_session_id TEXT,
  file_path TEXT NOT NULL,
  file_identity TEXT,
  file_size INTEGER NOT NULL DEFAULT 0,
  file_mtime_ms INTEGER NOT NULL DEFAULT 0,
  scan_offset INTEGER NOT NULL DEFAULT 0,
  checkpoint_offset INTEGER,
  checkpoint_hash BLOB,
  last_scanned_ts INTEGER,
  last_error TEXT,
  FOREIGN KEY(source_id) REFERENCES agent_sources(id) ON DELETE CASCADE,
  UNIQUE(source_id, adapter_file_key)
);

CREATE INDEX idx_agent_usage_ts ON agent_usage_events(event_ts);
CREATE INDEX idx_agent_usage_origin_ts ON agent_usage_events(origin_session_id, event_ts);
CREATE INDEX idx_agent_usage_source_ts ON agent_usage_events(source_id, event_ts);
CREATE INDEX idx_agent_refs_event ON agent_session_usage_refs(usage_event_id);
CREATE INDEX idx_agent_sessions_activity ON agent_sessions(last_activity_ts);
CREATE INDEX idx_agent_scan_source ON agent_scan_files(source_id);
