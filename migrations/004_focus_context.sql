CREATE TABLE context_entities (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  kind TEXT NOT NULL,
  canonical_key BLOB NOT NULL CHECK(length(canonical_key) = 32),
  display_name TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  first_seen_ts INTEGER NOT NULL,
  last_seen_ts INTEGER NOT NULL,
  user_confirmed INTEGER NOT NULL DEFAULT 0 CHECK(user_confirmed IN (0, 1)),
  archived INTEGER NOT NULL DEFAULT 0 CHECK(archived IN (0, 1)),
  UNIQUE(kind, canonical_key)
);

CREATE INDEX idx_context_entities_kind_name
  ON context_entities(kind, display_name);

CREATE TABLE context_entity_aliases (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_id INTEGER NOT NULL,
  alias_kind TEXT NOT NULL,
  alias_value TEXT NOT NULL,
  source TEXT NOT NULL,
  confidence REAL NOT NULL CHECK(confidence >= 0 AND confidence <= 1),
  created_ts INTEGER NOT NULL,
  UNIQUE(alias_kind, alias_value),
  FOREIGN KEY(entity_id) REFERENCES context_entities(id) ON DELETE CASCADE
);

CREATE INDEX idx_context_entity_aliases_entity
  ON context_entity_aliases(entity_id);

CREATE TABLE context_entity_relations (
  parent_entity_id INTEGER NOT NULL,
  child_entity_id INTEGER NOT NULL,
  relation TEXT NOT NULL,
  confidence REAL NOT NULL CHECK(confidence >= 0 AND confidence <= 1),
  source TEXT NOT NULL,
  PRIMARY KEY(parent_entity_id, child_entity_id, relation),
  CHECK(parent_entity_id <> child_entity_id),
  FOREIGN KEY(parent_entity_id) REFERENCES context_entities(id) ON DELETE CASCADE,
  FOREIGN KEY(child_entity_id) REFERENCES context_entities(id) ON DELETE CASCADE
);

CREATE INDEX idx_context_entity_relations_child
  ON context_entity_relations(child_entity_id, relation);

CREATE TABLE focus_context_intervals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  last_observed_ts INTEGER NOT NULL,
  start_environment_id INTEGER,
  end_environment_id INTEGER,
  context_key BLOB NOT NULL CHECK(length(context_key) = 32),
  extractor_version INTEGER NOT NULL,
  closed_reason TEXT,
  focused_app TEXT NOT NULL DEFAULT '',
  focused_title TEXT,
  focused_pid INTEGER,
  ambiguity_json TEXT NOT NULL DEFAULT '{}',
  CHECK(end_ts >= start_ts),
  CHECK(last_observed_ts >= start_ts),
  FOREIGN KEY(start_environment_id) REFERENCES environment_samples(id) ON DELETE SET NULL,
  FOREIGN KEY(end_environment_id) REFERENCES environment_samples(id) ON DELETE SET NULL
);

CREATE UNIQUE INDEX idx_focus_context_single_open
  ON focus_context_intervals((1)) WHERE closed_reason IS NULL;
CREATE INDEX idx_focus_context_interval_start
  ON focus_context_intervals(start_ts);
CREATE INDEX idx_focus_context_interval_end
  ON focus_context_intervals(end_ts);
CREATE INDEX idx_focus_context_interval_key_end
  ON focus_context_intervals(context_key, end_ts);

CREATE TABLE label_rules (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL,
  priority INTEGER NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1 CHECK(enabled IN (0, 1)),
  origin TEXT NOT NULL,
  conditions_json TEXT NOT NULL,
  outputs_json TEXT NOT NULL,
  stop_dimensions_json TEXT NOT NULL DEFAULT '[]',
  revision INTEGER NOT NULL DEFAULT 1,
  created_ts INTEGER NOT NULL,
  updated_ts INTEGER NOT NULL
);

CREATE INDEX idx_label_rules_enabled_priority
  ON label_rules(enabled, priority DESC, id);

CREATE TABLE labeler_runs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  labeler_version INTEGER NOT NULL,
  rules_revision INTEGER NOT NULL,
  started_ts INTEGER NOT NULL,
  completed_ts INTEGER,
  range_start_ts INTEGER,
  range_end_ts INTEGER,
  status TEXT NOT NULL CHECK(status IN ('running', 'completed', 'failed')),
  error TEXT
);

CREATE INDEX idx_labeler_runs_status_version
  ON labeler_runs(status, labeler_version, rules_revision, completed_ts);

CREATE TABLE focus_interval_labels (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  context_interval_id INTEGER NOT NULL,
  dimension TEXT NOT NULL,
  label_key BLOB NOT NULL CHECK(length(label_key) = 32),
  value TEXT,
  entity_id INTEGER,
  confidence REAL NOT NULL CHECK(confidence >= 0 AND confidence <= 1),
  source TEXT NOT NULL,
  rule_id INTEGER,
  labeler_run_id INTEGER,
  labeler_version INTEGER NOT NULL,
  evidence_json TEXT NOT NULL DEFAULT '[]',
  CHECK((value IS NULL) <> (entity_id IS NULL)),
  FOREIGN KEY(context_interval_id) REFERENCES focus_context_intervals(id) ON DELETE CASCADE,
  FOREIGN KEY(entity_id) REFERENCES context_entities(id) ON DELETE CASCADE,
  FOREIGN KEY(rule_id) REFERENCES label_rules(id) ON DELETE SET NULL,
  FOREIGN KEY(labeler_run_id) REFERENCES labeler_runs(id) ON DELETE SET NULL,
  UNIQUE(context_interval_id, dimension, label_key, source, labeler_version)
);

CREATE INDEX idx_focus_interval_labels_interval_version
  ON focus_interval_labels(context_interval_id, labeler_version);
CREATE INDEX idx_focus_interval_labels_dimension_entity
  ON focus_interval_labels(dimension, entity_id, context_interval_id);
CREATE INDEX idx_focus_interval_labels_dimension_value
  ON focus_interval_labels(dimension, value, context_interval_id);

CREATE TABLE manual_activity_labels (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  dimension TEXT NOT NULL,
  value TEXT,
  entity_id INTEGER,
  note TEXT,
  created_ts INTEGER NOT NULL,
  CHECK(end_ts > start_ts),
  CHECK((value IS NULL) <> (entity_id IS NULL)),
  FOREIGN KEY(entity_id) REFERENCES context_entities(id) ON DELETE CASCADE
);

CREATE INDEX idx_manual_activity_labels_time
  ON manual_activity_labels(start_ts, end_ts);

CREATE TABLE activity_intervals (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  start_ts INTEGER NOT NULL,
  end_ts INTEGER NOT NULL,
  source TEXT NOT NULL,
  source_context_interval_id INTEGER,
  activity TEXT,
  project_entity_id INTEGER,
  service_entity_id INTEGER,
  tool_entity_id INTEGER,
  agent_session_id INTEGER,
  confidence REAL NOT NULL CHECK(confidence >= 0 AND confidence <= 1),
  labeler_version INTEGER NOT NULL,
  evidence_json TEXT NOT NULL DEFAULT '{}',
  CHECK(end_ts >= start_ts),
  FOREIGN KEY(source_context_interval_id) REFERENCES focus_context_intervals(id) ON DELETE SET NULL,
  FOREIGN KEY(project_entity_id) REFERENCES context_entities(id) ON DELETE SET NULL,
  FOREIGN KEY(service_entity_id) REFERENCES context_entities(id) ON DELETE SET NULL,
  FOREIGN KEY(tool_entity_id) REFERENCES context_entities(id) ON DELETE SET NULL,
  FOREIGN KEY(agent_session_id) REFERENCES agent_sessions(id) ON DELETE SET NULL
);

CREATE UNIQUE INDEX idx_activity_intervals_source_context
  ON activity_intervals(source_context_interval_id);
CREATE INDEX idx_activity_intervals_time
  ON activity_intervals(start_ts, end_ts);
CREATE INDEX idx_activity_intervals_project_time
  ON activity_intervals(project_entity_id, start_ts);
CREATE INDEX idx_activity_intervals_service_time
  ON activity_intervals(service_entity_id, start_ts);
