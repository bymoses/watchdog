CREATE TABLE context_runtime_state (
  id INTEGER PRIMARY KEY CHECK(id = 1),
  rules_revision INTEGER NOT NULL,
  active_labeler_version INTEGER NOT NULL,
  updated_ts INTEGER NOT NULL
);
INSERT INTO context_runtime_state (id,rules_revision,active_labeler_version,updated_ts)
VALUES (1,(SELECT COUNT(*) FROM label_rules),1,CAST(strftime('%s','now') AS INTEGER) * 1000);

CREATE TRIGGER label_rules_revision_insert AFTER INSERT ON label_rules BEGIN
  UPDATE context_runtime_state SET rules_revision=rules_revision+1,updated_ts=MAX(updated_ts,NEW.updated_ts) WHERE id=1;
END;
CREATE TRIGGER label_rules_revision_update AFTER UPDATE OF name,priority,enabled,origin,conditions_json,outputs_json,stop_dimensions_json ON label_rules BEGIN
  UPDATE context_runtime_state SET rules_revision=rules_revision+1,updated_ts=MAX(updated_ts,NEW.updated_ts) WHERE id=1;
END;
CREATE TRIGGER label_rules_revision_delete AFTER DELETE ON label_rules BEGIN
  UPDATE context_runtime_state SET rules_revision=rules_revision+1,updated_ts=CAST(strftime('%s','now') AS INTEGER) * 1000 WHERE id=1;
END;

CREATE TABLE relabel_jobs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  rules_revision INTEGER NOT NULL,
  labeler_version INTEGER NOT NULL,
  labeler_run_id INTEGER,
  range_start_ts INTEGER NOT NULL,
  range_end_ts INTEGER NOT NULL,
  entity_id INTEGER,
  cursor_start_ts INTEGER,
  cursor_interval_id INTEGER NOT NULL DEFAULT 0,
  processed_intervals INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL CHECK(status IN ('queued','running','completed','failed')),
  created_ts INTEGER NOT NULL,
  started_ts INTEGER,
  completed_ts INTEGER,
  error TEXT,
  FOREIGN KEY(labeler_run_id) REFERENCES labeler_runs(id) ON DELETE SET NULL,
  FOREIGN KEY(entity_id) REFERENCES context_entities(id) ON DELETE SET NULL
);
CREATE INDEX idx_relabel_jobs_status_id ON relabel_jobs(status,id);

CREATE TABLE context_backfill_state (
  id INTEGER PRIMARY KEY CHECK(id=1),
  last_environment_id INTEGER NOT NULL DEFAULT 0,
  processed_samples INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','running','completed','failed')),
  updated_ts INTEGER NOT NULL,
  error TEXT
);
INSERT INTO context_backfill_state(id,updated_ts) VALUES(1,CAST(strftime('%s','now') AS INTEGER)*1000);

ALTER TABLE focus_context_intervals ADD COLUMN terminal_kind TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN terminal_match_method TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN terminal_match_confidence REAL;
ALTER TABLE focus_context_intervals ADD COLUMN terminal_candidate_count INTEGER;
ALTER TABLE focus_context_intervals ADD COLUMN tmux_client_pid INTEGER;
ALTER TABLE focus_context_intervals ADD COLUMN tmux_session TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN tmux_window TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN tmux_pane TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN tmux_pane_pid INTEGER;
ALTER TABLE focus_context_intervals ADD COLUMN terminal_cwd_key BLOB;
ALTER TABLE focus_context_intervals ADD COLUMN terminal_cwd_display TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN foreground_executable TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN repository_entity_id INTEGER REFERENCES context_entities(id) ON DELETE SET NULL;
ALTER TABLE focus_context_intervals ADD COLUMN project_entity_id INTEGER REFERENCES context_entities(id) ON DELETE SET NULL;
ALTER TABLE focus_context_intervals ADD COLUMN agent_type TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN agent_process_pid INTEGER;
ALTER TABLE focus_context_intervals ADD COLUMN agent_session_id INTEGER REFERENCES agent_sessions(id) ON DELETE SET NULL;
ALTER TABLE focus_context_intervals ADD COLUMN native_agent_session_id TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN agent_state TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN agent_match_method TEXT;
ALTER TABLE focus_context_intervals ADD COLUMN agent_match_confidence REAL;
CREATE INDEX idx_focus_context_project_start ON focus_context_intervals(project_entity_id,start_ts);
CREATE INDEX idx_focus_context_agent_start ON focus_context_intervals(agent_session_id,start_ts);

CREATE TABLE activity_context_sources (
  activity_interval_id INTEGER NOT NULL,
  context_interval_id INTEGER NOT NULL,
  PRIMARY KEY(activity_interval_id,context_interval_id),
  FOREIGN KEY(activity_interval_id) REFERENCES activity_intervals(id) ON DELETE CASCADE,
  FOREIGN KEY(context_interval_id) REFERENCES focus_context_intervals(id) ON DELETE CASCADE
);
CREATE INDEX idx_activity_context_sources_context ON activity_context_sources(context_interval_id);

CREATE TABLE agent_presence_leases (
  harness TEXT NOT NULL,
  source_key TEXT NOT NULL,
  instance_id TEXT NOT NULL,
  pid INTEGER NOT NULL,
  process_start_identity TEXT,
  native_session_id TEXT NOT NULL,
  cwd_key BLOB CHECK(cwd_key IS NULL OR length(cwd_key)=32),
  state TEXT NOT NULL CHECK(state IN ('idle','busy','settling','closed')),
  observed_ts INTEGER NOT NULL,
  expires_ts INTEGER NOT NULL,
  agent_session_id INTEGER,
  PRIMARY KEY(harness,source_key,instance_id),
  FOREIGN KEY(agent_session_id) REFERENCES agent_sessions(id) ON DELETE SET NULL
);
CREATE INDEX idx_agent_presence_expiry ON agent_presence_leases(expires_ts);
CREATE INDEX idx_agent_presence_pid ON agent_presence_leases(pid,expires_ts);

CREATE TABLE agent_state_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  event_key BLOB NOT NULL UNIQUE CHECK(length(event_key)=32),
  harness TEXT NOT NULL,
  source_key TEXT NOT NULL,
  native_session_id TEXT NOT NULL,
  event_ts INTEGER NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('idle','busy','settling','closed'))
);
CREATE INDEX idx_agent_state_events_time ON agent_state_events(event_ts);

CREATE TABLE browser_pairing_codes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  code_hash BLOB NOT NULL UNIQUE CHECK(length(code_hash)=32),
  created_ts INTEGER NOT NULL,
  expires_ts INTEGER NOT NULL,
  used_ts INTEGER,
  attempts INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_browser_pairing_expiry ON browser_pairing_codes(expires_ts);
CREATE TABLE browser_pairing_rate_limits (
  bucket_ts INTEGER NOT NULL,
  rate_key BLOB NOT NULL CHECK(length(rate_key)=32),
  attempts INTEGER NOT NULL,
  PRIMARY KEY(bucket_ts,rate_key)
);

CREATE TABLE browser_clients (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  client_key BLOB NOT NULL UNIQUE CHECK(length(client_key)=32),
  client_instance_id TEXT NOT NULL UNIQUE,
  display_name TEXT NOT NULL,
  browser_family TEXT NOT NULL,
  extension_version TEXT NOT NULL,
  secret_hash BLOB NOT NULL CHECK(length(secret_hash)=32),
  allowed_origin TEXT NOT NULL,
  privacy_mode TEXT NOT NULL CHECK(privacy_mode IN ('off','domains-only','domains-and-titles','selected-sites-media','full-debug')),
  selected_media_origins_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(selected_media_origins_json)),
  paired_ts INTEGER NOT NULL,
  last_seen_ts INTEGER,
  last_sequence INTEGER NOT NULL DEFAULT 0,
  queue_depth INTEGER NOT NULL DEFAULT 0 CHECK(queue_depth BETWEEN 0 AND 1000),
  queue_bytes INTEGER NOT NULL DEFAULT 0 CHECK(queue_bytes BETWEEN 0 AND 2097152),
  queue_dropped INTEGER NOT NULL DEFAULT 0 CHECK(queue_dropped BETWEEN 0 AND 2147483647),
  queue_permanent_rejected INTEGER NOT NULL DEFAULT 0 CHECK(queue_permanent_rejected BETWEEN 0 AND 2147483647),
  queue_recovery_snapshots INTEGER NOT NULL DEFAULT 0 CHECK(queue_recovery_snapshots BETWEEN 0 AND 2147483647),
  queue_oldest_ts INTEGER,
  queue_last_success_ts INTEGER,
  queue_last_data_loss_ts INTEGER,
  queue_health_observed_ts INTEGER,
  revoked_ts INTEGER
);

CREATE TABLE browser_request_rate_limits (
  client_id INTEGER NOT NULL,
  bucket_ts INTEGER NOT NULL,
  requests INTEGER NOT NULL,
  PRIMARY KEY(client_id,bucket_ts),
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE CASCADE
);

CREATE TABLE browser_current_states (
  client_id INTEGER NOT NULL,
  window_id INTEGER NOT NULL,
  tab_id INTEGER NOT NULL,
  lease_key BLOB NOT NULL CHECK(length(lease_key)=32),
  last_event_key BLOB NOT NULL CHECK(length(last_event_key)=32),
  episode_key BLOB NOT NULL CHECK(length(episode_key)=32),
  client_sequence INTEGER NOT NULL,
  state_started_ts INTEGER NOT NULL,
  last_observed_ts INTEGER NOT NULL,
  expires_ts INTEGER NOT NULL,
  domain TEXT,
  origin TEXT,
  title TEXT,
  browser_focused INTEGER NOT NULL,
  tab_active INTEGER NOT NULL,
  document_visible INTEGER NOT NULL,
  audible INTEGER NOT NULL,
  media_state TEXT NOT NULL CHECK(media_state IN ('none','paused','playing','ended','seeking')),
  picture_in_picture INTEGER NOT NULL DEFAULT 0,
  private INTEGER NOT NULL DEFAULT 0,
  payload_version INTEGER NOT NULL,
  heartbeat_upserts INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(client_id,window_id,tab_id),
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE CASCADE
);
CREATE INDEX idx_browser_current_expiry ON browser_current_states(expires_ts);

CREATE TABLE browser_event_receipts (
  client_id INTEGER NOT NULL,
  event_key BLOB NOT NULL CHECK(length(event_key)=32),
  client_sequence INTEGER NOT NULL,
  received_ts INTEGER NOT NULL,
  PRIMARY KEY(client_id,event_key),
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE CASCADE
);
CREATE INDEX idx_browser_receipts_received ON browser_event_receipts(received_ts);

CREATE TABLE browser_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  client_id INTEGER NOT NULL,
  event_key BLOB NOT NULL CHECK(length(event_key)=32),
  client_sequence INTEGER NOT NULL,
  window_id INTEGER NOT NULL,
  tab_id INTEGER NOT NULL,
  event_ts INTEGER NOT NULL,
  received_ts INTEGER NOT NULL,
  event_type TEXT NOT NULL,
  domain TEXT,
  origin TEXT,
  title TEXT,
  browser_focused INTEGER,
  tab_active INTEGER,
  document_visible INTEGER,
  audible INTEGER,
  media_state TEXT NOT NULL CHECK(media_state IN ('none','paused','playing','ended','seeking')),
  picture_in_picture INTEGER NOT NULL DEFAULT 0,
  media_position REAL,
  media_duration REAL,
  playback_rate REAL,
  private INTEGER NOT NULL DEFAULT 0,
  payload_version INTEGER NOT NULL,
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE CASCADE,
  UNIQUE(client_id,event_key)
);
CREATE INDEX idx_browser_events_time ON browser_events(event_ts);
CREATE INDEX idx_browser_events_domain_time ON browser_events(domain,event_ts);
CREATE INDEX idx_browser_events_client_sequence ON browser_events(client_id,client_sequence);
CREATE TABLE browser_ingestion_metrics (
  id INTEGER PRIMARY KEY CHECK(id=1),
  accepted_events INTEGER NOT NULL DEFAULT 0,
  duplicate_events INTEGER NOT NULL DEFAULT 0,
  rejected_events INTEGER NOT NULL DEFAULT 0,
  heartbeat_upserts INTEGER NOT NULL DEFAULT 0,
  persisted_transitions INTEGER NOT NULL DEFAULT 0,
  recovery_transitions INTEGER NOT NULL DEFAULT 0,
  last_ingest_ts INTEGER
);
INSERT INTO browser_ingestion_metrics(id) VALUES(1);

CREATE TABLE browser_attention_intervals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  client_id INTEGER NOT NULL,
  window_id INTEGER NOT NULL,
  tab_id INTEGER NOT NULL,
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  episode_key BLOB NOT NULL CHECK(length(episode_key)=32),
  domain TEXT,
  origin TEXT,
  service_entity_id INTEGER,
  browser_focused INTEGER NOT NULL,
  tab_active INTEGER NOT NULL,
  document_visible INTEGER NOT NULL,
  media_playing INTEGER NOT NULL,
  audible INTEGER NOT NULL,
  picture_in_picture INTEGER NOT NULL DEFAULT 0,
  os_focus_verified INTEGER NOT NULL,
  os_focus_state TEXT NOT NULL CHECK(os_focus_state IN ('verified','contradicted','mixed','unknown')),
  os_context_interval_id INTEGER,
  confidence REAL NOT NULL,
  closed_reason TEXT NOT NULL,
  source_first_event_id INTEGER,
  source_last_event_id INTEGER,
  CHECK(end_ts>=start_ts),
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE CASCADE,
  FOREIGN KEY(service_entity_id) REFERENCES context_entities(id) ON DELETE SET NULL,
  FOREIGN KEY(os_context_interval_id) REFERENCES focus_context_intervals(id) ON DELETE SET NULL,
  FOREIGN KEY(source_first_event_id) REFERENCES browser_events(id) ON DELETE SET NULL,
  FOREIGN KEY(source_last_event_id) REFERENCES browser_events(id) ON DELETE SET NULL
);
CREATE INDEX idx_browser_attention_time ON browser_attention_intervals(start_ts,end_ts);
CREATE INDEX idx_browser_attention_domain_time ON browser_attention_intervals(domain,start_ts);
CREATE INDEX idx_browser_attention_episode ON browser_attention_intervals(episode_key,start_ts);

CREATE TABLE attention_policies (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  scope_json TEXT NOT NULL,
  metric TEXT NOT NULL CHECK(metric IN ('tab_active_seconds','browser_focused_seconds','document_visible_seconds','media_playback_seconds','audible_playback_seconds','focused_playback_seconds','verified_focused_playback_seconds','background_playback_seconds')),
  window_kind TEXT NOT NULL CHECK(window_kind IN ('calendar-day','rolling','session')),
  budget_seconds INTEGER NOT NULL CHECK(budget_seconds>0),
  warning_seconds INTEGER,
  enforcement TEXT NOT NULL CHECK(enforcement IN ('observe','warn','soft-block','hard-block')),
  schedule_json TEXT NOT NULL DEFAULT '{}',
  grace_json TEXT NOT NULL DEFAULT '{}',
  revision INTEGER NOT NULL,
  created_ts INTEGER NOT NULL,
  updated_ts INTEGER NOT NULL
);
CREATE INDEX idx_attention_policies_enabled ON attention_policies(enabled,id);

CREATE TABLE policy_overrides (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  policy_id INTEGER NOT NULL,
  client_id INTEGER,
  action TEXT NOT NULL CHECK(action IN ('continue','snooze','override')),
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  created_ts INTEGER NOT NULL,
  FOREIGN KEY(policy_id) REFERENCES attention_policies(id) ON DELETE CASCADE,
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE CASCADE
);
CREATE INDEX idx_policy_overrides_active ON policy_overrides(policy_id,start_ts,end_ts);

CREATE TABLE intervention_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  policy_id INTEGER NOT NULL,
  client_id INTEGER,
  event_ts INTEGER NOT NULL,
  action TEXT NOT NULL CHECK(action IN ('allow','observe','warn','soft-block','hard-block','continue','snooze','override')),
  service_entity_id INTEGER,
  remaining_seconds INTEGER,
  decision_revision INTEGER NOT NULL,
  FOREIGN KEY(policy_id) REFERENCES attention_policies(id) ON DELETE CASCADE,
  FOREIGN KEY(client_id) REFERENCES browser_clients(id) ON DELETE SET NULL,
  FOREIGN KEY(service_entity_id) REFERENCES context_entities(id) ON DELETE SET NULL
);
CREATE INDEX idx_intervention_events_time ON intervention_events(event_ts);

CREATE TABLE source_health (
  source TEXT PRIMARY KEY,
  last_success_ts INTEGER,
  last_error_ts INTEGER,
  last_error_category TEXT,
  success_count INTEGER NOT NULL DEFAULT 0,
  error_count INTEGER NOT NULL DEFAULT 0,
  detail_json TEXT NOT NULL DEFAULT '{}'
);
