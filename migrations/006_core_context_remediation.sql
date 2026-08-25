DROP INDEX IF EXISTS idx_activity_intervals_source_context;
CREATE INDEX idx_activity_intervals_source_context
  ON activity_intervals(source_context_interval_id, labeler_version, start_ts);

ALTER TABLE activity_intervals ADD COLUMN episode_key BLOB
  CHECK(episode_key IS NULL OR length(episode_key) = 32);
CREATE INDEX idx_activity_intervals_episode
  ON activity_intervals(episode_key, start_ts);

ALTER TABLE relabel_jobs ADD COLUMN base_labeler_version INTEGER;
ALTER TABLE relabel_jobs ADD COLUMN build_phase TEXT NOT NULL DEFAULT 'clone_labels'
  CHECK(build_phase IN ('clone_labels','clone_activity','relabel','switch','completed','failed'));
ALTER TABLE relabel_jobs ADD COLUMN clone_label_cursor INTEGER NOT NULL DEFAULT 0;
ALTER TABLE relabel_jobs ADD COLUMN clone_activity_cursor INTEGER NOT NULL DEFAULT 0;
UPDATE relabel_jobs
SET base_labeler_version = labeler_version
WHERE base_labeler_version IS NULL;

CREATE INDEX idx_focus_interval_labels_version_id
  ON focus_interval_labels(labeler_version, id);
CREATE INDEX idx_activity_intervals_version_id
  ON activity_intervals(labeler_version, id);

CREATE TABLE derived_version_prune_jobs (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  labeler_version INTEGER NOT NULL UNIQUE,
  phase TEXT NOT NULL DEFAULT 'labels'
    CHECK(phase IN ('labels','activity','completed')),
  label_cursor INTEGER NOT NULL DEFAULT 0,
  activity_cursor INTEGER NOT NULL DEFAULT 0,
  deleted_labels INTEGER NOT NULL DEFAULT 0,
  deleted_activity INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'queued'
    CHECK(status IN ('queued','running','completed','failed')),
  created_ts INTEGER NOT NULL,
  started_ts INTEGER,
  completed_ts INTEGER,
  error TEXT
);
CREATE INDEX idx_derived_version_prune_status
  ON derived_version_prune_jobs(status, id);

ALTER TABLE agent_presence_leases ADD COLUMN event_id TEXT NOT NULL DEFAULT '';
ALTER TABLE agent_presence_leases ADD COLUMN cwd TEXT NOT NULL DEFAULT '';

ALTER TABLE agent_state_events ADD COLUMN instance_id TEXT NOT NULL DEFAULT '';
ALTER TABLE agent_state_events ADD COLUMN pid INTEGER;
ALTER TABLE agent_state_events ADD COLUMN process_start_identity TEXT;
ALTER TABLE agent_state_events ADD COLUMN lease_expires_ts INTEGER;
ALTER TABLE agent_state_events ADD COLUMN transition_kind TEXT;

CREATE VIEW active_focus_interval_labels AS
SELECT labels.*
FROM focus_interval_labels labels
JOIN context_runtime_state runtime
  ON runtime.id = 1
 AND labels.labeler_version = runtime.active_labeler_version;

CREATE VIEW active_activity_intervals AS
SELECT activity.*
FROM activity_intervals activity
JOIN context_runtime_state runtime
  ON runtime.id = 1
 AND (
   activity.source <> 'os_focus'
   OR activity.labeler_version = runtime.active_labeler_version
 );

CREATE VIEW bounded_agent_state_events AS
SELECT events.*,
       MIN(
         COALESCE(
           LEAD(event_ts) OVER (
             PARTITION BY harness, source_key, native_session_id, instance_id
             ORDER BY event_ts, id
           ),
           lease_expires_ts,
           event_ts
         ),
         COALESCE(lease_expires_ts, 9223372036854775807)
       ) AS bounded_end_ts
FROM agent_state_events events;
