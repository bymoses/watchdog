import { Database } from 'bun:sqlite';
import { mkdtemp, readdir, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const root = resolve(import.meta.dir, '..');
const dir = await mkdtemp(join(tmpdir(), 'bms-watchdog-storage-'));
const compactPath = join(dir, 'compact.sqlite');
const naivePath = join(dir, 'naive.sqlite');
const DAY = 86_400_000;
const YEAR_DAYS = 365;
const BUDGET_BYTES = 250 * 1024 * 1024;

// No live database is opened. The OS-focus rates come from the documented Plan 002
// read-only baseline; browser/agent rates are explicit capacity assumptions until
// an isolated real-browser transition measurement is approved.
const rates = {
  activeMachineSamplesPerDay: 1_593,
  focusTransitionsPerDay: 388,
  browserTransitionsPerDay: 300,
  browserAttentionIntervalsPerDay: 200,
  agentStateTransitionsPerDay: 28,
};
const counts = {
  naiveSamples: rates.activeMachineSamplesPerDay * YEAR_DAYS,
  focusIntervals: rates.focusTransitionsPerDay * YEAR_DAYS,
  browserTransitions: rates.browserTransitionsPerDay * YEAR_DAYS,
  browserAttentionIntervals: rates.browserAttentionIntervalsPerDay * YEAR_DAYS,
  agentStateEvents: rates.agentStateTransitionsPerDay * YEAR_DAYS,
};

const compact = new Database(compactPath, { create: true });
let naive: Database | undefined;
try {
  compact.exec(
    'PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA wal_autocheckpoint=0;',
  );
  const migrationNames = (await readdir(join(root, 'migrations')))
    .filter((name) => /^\d+_.+\.sql$/.test(name))
    .sort();
  for (const migration of migrationNames) {
    compact.exec(await Bun.file(join(root, 'migrations', migration)).text());
  }

  const now = Date.now();
  seedDimensions(compact, now);
  populateCompactYear(compact, now);
  compact.exec('ANALYZE; PRAGMA wal_checkpoint(TRUNCATE);');

  const beforeRelabel = databasePages(compact);
  const beforeRelabelAllocations = objectAllocations(compact);
  const walBeforeRelabel = await fileBytes(`${compactPath}-wal`);

  compact.exec('BEGIN IMMEDIATE;');
  compact.run(
    "INSERT INTO labeler_runs(id,labeler_version,rules_revision,started_ts,range_start_ts,range_end_ts,status) VALUES(2,2,20,?,?,?,'running')",
    [now, now - YEAR_DAYS * DAY, now],
  );
  compact.exec(
    `INSERT INTO focus_interval_labels(context_interval_id,dimension,label_key,value,entity_id,confidence,source,rule_id,labeler_run_id,labeler_version,evidence_json)
     SELECT context_interval_id,dimension,label_key,value,entity_id,confidence,source,rule_id,2,2,evidence_json
     FROM focus_interval_labels WHERE labeler_version=1;
     INSERT INTO activity_intervals(start_ts,end_ts,source,source_context_interval_id,activity,project_entity_id,service_entity_id,tool_entity_id,agent_session_id,confidence,labeler_version,evidence_json,episode_key)
     SELECT start_ts,end_ts,source,source_context_interval_id,activity,project_entity_id,service_entity_id,tool_entity_id,agent_session_id,confidence,2,evidence_json,episode_key
     FROM activity_intervals WHERE labeler_version=1;
     INSERT OR IGNORE INTO activity_context_sources(activity_interval_id,context_interval_id)
     SELECT id,source_context_interval_id FROM activity_intervals
     WHERE labeler_version=2 AND source_context_interval_id IS NOT NULL;`,
  );
  compact.run(
    "INSERT INTO relabel_jobs(rules_revision,labeler_version,labeler_run_id,range_start_ts,range_end_ts,cursor_start_ts,cursor_interval_id,processed_intervals,status,created_ts,started_ts,base_labeler_version) VALUES(20,2,2,?,?,?,9223372036854775807,?,'running',?,?,1)",
    [now - YEAR_DAYS * DAY, now, now, counts.focusIntervals, now, now],
  );
  compact.exec('COMMIT;');
  const stagedReplacement = databasePages(compact);
  const walAfterReplacementBuild = await fileBytes(`${compactPath}-wal`);

  compact.exec('BEGIN IMMEDIATE;');
  compact.run(
    'UPDATE context_runtime_state SET active_labeler_version=2,rules_revision=20,updated_ts=? WHERE id=1',
    [now],
  );
  compact.run(
    "UPDATE labeler_runs SET completed_ts=?,status='completed' WHERE id=2",
    [now],
  );
  compact.run(
    "UPDATE relabel_jobs SET status='completed',completed_ts=? WHERE labeler_run_id=2",
    [now],
  );
  compact.run(
    "INSERT INTO derived_version_prune_jobs(labeler_version,phase,status,created_ts,started_ts) VALUES(1,'labels','running',?,?)",
    [now, now],
  );
  compact.exec('DELETE FROM focus_interval_labels WHERE labeler_version < 2;');
  compact.exec('DELETE FROM activity_intervals WHERE labeler_version < 2;');
  compact.run(
    "UPDATE derived_version_prune_jobs SET phase='completed',status='completed',deleted_labels=?,deleted_activity=?,completed_ts=? WHERE labeler_version=1",
    [Math.floor(counts.focusIntervals * 2.5), counts.focusIntervals, now],
  );
  compact.exec('COMMIT;');
  const switchedAndPruned = databasePages(compact);
  const walAfterVersionPrune = await fileBytes(`${compactPath}-wal`);
  compact.exec('PRAGMA wal_checkpoint(TRUNCATE);');

  // Exercise retention churn and page reuse with a 10% transition prune/refill.
  compact.exec('BEGIN IMMEDIATE;');
  compact.exec('DELETE FROM browser_events WHERE id % 10 = 0;');
  compact.exec('DELETE FROM browser_event_receipts WHERE client_sequence % 10 = 0;');
  compact.exec('DELETE FROM browser_attention_intervals WHERE id % 10 = 0;');
  compact.exec('DELETE FROM agent_state_events WHERE id % 10 = 0;');
  compact.exec('COMMIT;');
  const afterRetentionPrune = databasePages(compact);
  const walAfterRetentionPrune = await fileBytes(`${compactPath}-wal`);
  refillTransitionChurn(compact, now);
  const afterRefillBeforeMaintenance = databasePages(compact);
  compact.exec('PRAGMA wal_checkpoint(TRUNCATE); VACUUM;');
  const afterVacuumMaintenance = databasePages(compact);

  const finalAllocations = objectAllocations(compact);
  const compactAllocation = summarizeContextAllocation(compact, finalAllocations);

  naive = new Database(naivePath, { create: true });
  naive.exec('PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF;');
  populateNaiveYear(naive, now);
  naive.exec('ANALYZE;');
  const naivePages = databasePages(naive);
  const naiveAllocation = objectAllocations(naive);
  const naiveBytes =
    naiveAllocation.capability === 'dbstat'
      ? naiveAllocation.objects.reduce((sum, object) => sum + object.allocatedBytes, 0)
      : naivePages.liveBytes;

  const report = {
    methodology: {
      fixture: 'one-year synthetic interval model with realistic privacy-safe payload shapes',
      liveDatabaseAccess: false,
      rateSources: {
        activeMachineSamplesPerDay: 'Plan 002 documented 35.1-day baseline',
        focusTransitionsPerDay: 'Plan 002 documented app/title context runs',
        browserTransitionsPerDay:
          'explicit capacity scenario; no copied-live claim and no browser profile access',
        browserAttentionIntervalsPerDay:
          'explicit capacity scenario after heartbeat compaction',
        agentStateTransitionsPerDay: 'explicit busy/idle capacity scenario',
      },
      rates,
      counts,
      payloads:
        'varied app/title, tmux, path display, ambiguity, evidence, domains, origins, optional title/media, entities, aliases, relations, policies, leases, health, and all relevant indexes',
    },
    allocationCapability: finalAllocations.capability,
    allocationFallback:
      finalAllocations.capability === 'dbstat'
        ? null
        : 'dbstat unavailable: reports whole-database live page allocation only; no proportional per-object estimates',
    compact: {
      allocatedBytes: compactAllocation.totalBytes,
      allocatedMiB: compactAllocation.totalBytes / 1024 / 1024,
      budgetMiB: BUDGET_BYTES / 1024 / 1024,
      withinBudget: compactAllocation.totalBytes <= BUDGET_BYTES,
      categories: compactAllocation.categories,
      objects:
        finalAllocations.capability === 'dbstat' ? finalAllocations.objects : undefined,
      pages: databasePages(compact),
    },
    naivePerSample: {
      samples: counts.naiveSamples,
      labels: counts.naiveSamples * 3,
      allocatedBytes: naiveBytes,
      allocatedMiB: naiveBytes / 1024 / 1024,
      allocationCapability: naiveAllocation.capability,
      comparisonRatioToCompact: naiveBytes / Math.max(1, compactAllocation.totalBytes),
    },
    relabelAndRetentionChurn: {
      beforeRelabel,
      beforeRelabelObjectBytes:
        beforeRelabelAllocations.capability === 'dbstat'
          ? beforeRelabelAllocations.objects.reduce(
              (sum, object) => sum + object.allocatedBytes,
              0,
            )
          : null,
      stagedReplacement,
      switchedAndPruned,
      afterRetentionPrune,
      afterRefillBeforeMaintenance,
      afterVacuumMaintenance,
      freelistPagesReleasedByVersionPrune:
        switchedAndPruned.freelistPages - stagedReplacement.freelistPages,
      freelistPagesReleasedByRetention:
        afterRetentionPrune.freelistPages - switchedAndPruned.freelistPages,
      freelistPagesReusedOnRefill:
        afterRetentionPrune.freelistPages - afterRefillBeforeMaintenance.freelistPages,
      walBytes: {
        beforeRelabel: walBeforeRelabel,
        afterReplacementBuild: walAfterReplacementBuild,
        afterVersionPrune: walAfterVersionPrune,
        afterRetentionPrune: walAfterRetentionPrune,
      },
      maintenance:
        'VACUUM is measured explicitly after checkpoint; it is the operation that shrinks/compacts allocated pages, while ordinary pruning only makes freelist pages reusable',
      semantics:
        'replacement version is fully staged while version 1 remains active; reader version switches in one transaction; only then is version 1 pruned',
    },
  };
  console.log(JSON.stringify(report, null, 2));
  if (!report.compact.withinBudget) {
    throw new Error(
      `context storage ${report.compact.allocatedMiB.toFixed(1)} MiB exceeds 250 MiB/year`,
    );
  }
} finally {
  naive?.close();
  compact.close();
  await rm(dir, { recursive: true, force: true });
}

function seedDimensions(db: Database, now: number): void {
  db.transaction(() => {
    db.run(
      "INSERT INTO agent_sources(id,agent_type,source_key,display_name,adapter_version,capabilities_json,limitations_json,initial_scan_complete,last_scan_ts) VALUES(1,'pi','local','Pi',1,'[\"usage\",\"presence\"]','[]',1,?)",
      [now],
    );
    for (let i = 0; i < 120; i++) {
      db.run(
        'INSERT INTO agent_sessions(source_id,native_session_id,name,project_path,created_ts,first_activity_ts,last_activity_ts) VALUES(1,?,?,?,?,?,?)',
        [
          `session-${i.toString().padStart(3, '0')}`,
          `Benchmark session ${i}`,
          `/workspace/project-${i % 40}`,
          now - (i + 30) * DAY,
          now - (i + 29) * DAY,
          now - i * 60_000,
        ],
      );
    }
    const entity = db.prepare(
      'INSERT INTO context_entities(kind,canonical_key,display_name,metadata_json,first_seen_ts,last_seen_ts,user_confirmed) VALUES(?,?,?,?,?,?,?)',
    );
    const alias = db.prepare(
      "INSERT INTO context_entity_aliases(entity_id,alias_kind,alias_value,source,confidence,created_ts) VALUES(?,?,?,?,?,?)",
    );
    const relation = db.prepare(
      "INSERT INTO context_entity_relations(parent_entity_id,child_entity_id,relation,confidence,source) VALUES(?,?,?,?,?)",
    );
    for (let i = 0; i < 80; i++) {
      entity.run('project', key(i, 1), `project-${i}`, '{"origin":"workspace"}', now - YEAR_DAYS * DAY, now, i < 12 ? 1 : 0);
      const projectId = lastInsertId(db);
      entity.run('repository', key(i, 2), `project-${i}`, '{"vcs":"git","worktrees":2}', now - YEAR_DAYS * DAY, now, 0);
      const repositoryId = lastInsertId(db);
      alias.run(projectId, 'workspace_child', `project-${i}`, 'benchmark', 0.9, now);
      alias.run(repositoryId, 'repository_basename', `project-${i}`, 'benchmark', 1, now);
      relation.run(projectId, repositoryId, 'project_has_repository', 1, 'benchmark');
    }
    for (let i = 0; i < 40; i++) {
      entity.run('service', key(i, 3), `service-${i}.example`, '{"kind":"web"}', now - YEAR_DAYS * DAY, now, 0);
    }
    for (const [i, tool] of ['pi', 'claude', 'codex', 'nvim', 'shell'].entries()) {
      entity.run('tool', key(i, 4), tool, '{"source":"executable"}', now - YEAR_DAYS * DAY, now, 1);
    }
    db.run(
      "INSERT INTO labeler_runs(id,labeler_version,rules_revision,started_ts,completed_ts,range_start_ts,range_end_ts,status) VALUES(1,1,10,?,?,?,?,'completed')",
      [now - DAY, now - DAY + 1_000, now - YEAR_DAYS * DAY, now],
    );
    for (let i = 0; i < 20; i++) {
      db.run(
        "INSERT INTO label_rules(name,priority,enabled,origin,conditions_json,outputs_json,stop_dimensions_json,revision,created_ts,updated_ts) VALUES(?, ?,1,'user',?,?,?,1,?,?)",
        [
          `Rule ${i}`,
          100 - i,
          JSON.stringify([{ fact: 'app.id', operator: 'contains', value: `app-${i}` }]),
          JSON.stringify([{ dimension: 'activity.primary', value: i % 4 === 0 ? 'entertainment' : 'work', confidence: 0.9 }]),
          '["activity.primary"]',
          now,
          now,
        ],
      );
    }
    db.run('UPDATE context_runtime_state SET rules_revision=10,updated_ts=? WHERE id=1', [now]);
    db.run("UPDATE context_backfill_state SET processed_samples=?,status='completed',updated_ts=? WHERE id=1", [counts.naiveSamples, now]);
    for (let day = 0; day < YEAR_DAYS; day++) {
      db.run(
        "INSERT INTO manual_activity_labels(start_ts,end_ts,dimension,value,note,created_ts) VALUES(?,?,'activity.primary',?,'representative bounded correction',?)",
        [now - (day + 1) * DAY, now - (day + 1) * DAY + 15 * 60_000, day % 7 === 0 ? 'administration' : 'work', now - day * DAY],
      );
    }
    db.run(
      "INSERT INTO browser_clients(client_key,client_instance_id,display_name,browser_family,extension_version,secret_hash,allowed_origin,privacy_mode,paired_ts,last_seen_ts) VALUES(?,?,?,?,?,?,?,?,?,?)",
      [key(1, 5), 'benchmark-client-instance', 'Benchmark Firefox', 'firefox', '2.4.1', key(2, 5), 'moz-extension://benchmark-client', 'domains-and-titles', now - YEAR_DAYS * DAY, now],
    );
    db.run(
      "INSERT INTO browser_current_states(client_id,window_id,tab_id,lease_key,last_event_key,episode_key,client_sequence,state_started_ts,last_observed_ts,expires_ts,domain,origin,title,browser_focused,tab_active,document_visible,audible,media_state,picture_in_picture,private,payload_version,heartbeat_upserts) VALUES(1,1,1,?,?,?,?,?,?,?,'video.example','https://video.example','Current benchmark tab',1,1,1,1,'playing',0,0,1,?)",
      [key(3, 5), key(4, 5), key(5, 5), counts.browserTransitions, now - 20_000, now, now + 30_000, YEAR_DAYS * 1_200],
    );
    db.run(
      "INSERT INTO agent_presence_leases(harness,source_key,instance_id,pid,process_start_identity,native_session_id,cwd_key,state,observed_ts,expires_ts,agent_session_id) VALUES('pi','local','benchmark-instance',4242,'linux:12345','session-000',?,'busy',?,?,1)",
      [key(6, 5), now, now + 30_000],
    );
    db.run(
      'INSERT INTO browser_pairing_codes(code_hash,created_ts,expires_ts,used_ts,attempts) VALUES(?,?,?,?,1)',
      [key(7, 5), now - 60_000, now + 240_000, now - 30_000],
    );
    db.run('INSERT INTO browser_pairing_rate_limits(bucket_ts,rate_key,attempts) VALUES(?,?,1)', [now - 60_000, key(8, 5)]);
    db.run('INSERT INTO browser_request_rate_limits(client_id,bucket_ts,requests) VALUES(1,?,12)', [now - 60_000]);
    db.run("INSERT INTO source_health(source,last_success_ts,success_count,detail_json) VALUES('terminal',?,?,'{\"matches\":141620,\"ambiguous\":1840}')", [now, counts.focusIntervals]);
    for (let i = 0; i < 8; i++) {
      db.run(
        "INSERT INTO attention_policies(name,scope_json,metric,window_kind,budget_seconds,warning_seconds,enforcement,schedule_json,grace_json,revision,created_ts,updated_ts) VALUES(?,?,?,?,?,?,?,?,?,1,?,?)",
        [`Policy ${i}`, JSON.stringify({ domain: `service-${i}.example` }), i % 2 ? 'focused_playback_seconds' : 'tab_active_seconds', i % 3 ? 'calendar-day' : 'rolling', 3_600 + i * 300, 600, ['observe', 'warn', 'soft-block', 'hard-block'][i % 4], '{"weekdays":[1,2,3,4,5]}', '{"snoozeSeconds":300}', now, now],
      );
    }
    db.run("INSERT INTO policy_overrides(policy_id,client_id,action,start_ts,end_ts,created_ts) VALUES(1,1,'snooze',?,?,?)", [now - 60_000, now + 240_000, now]);
    db.run("INSERT INTO intervention_events(policy_id,client_id,event_ts,action,service_entity_id,remaining_seconds,decision_revision) VALUES(1,1,?,'warn',161,300,1)", [now]);
  })();
}

function populateCompactYear(db: Database, now: number): void {
  const focus = db.prepare(
    `INSERT INTO focus_context_intervals(start_ts,end_ts,last_observed_ts,context_key,extractor_version,closed_reason,focused_app,focused_title,focused_pid,ambiguity_json,terminal_kind,terminal_match_method,terminal_match_confidence,terminal_candidate_count,tmux_client_pid,tmux_session,tmux_window,tmux_pane,tmux_pane_pid,terminal_cwd_key,terminal_cwd_display,foreground_executable,repository_entity_id,project_entity_id,agent_type,agent_process_pid,agent_session_id,native_agent_session_id,agent_state,agent_match_method,agent_match_confidence)
     VALUES(?,?,?,?,2,'transition',?,?,?,?, 'tmux','exact_descendant_client',0.98,1,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
  );
  const label = db.prepare(
    "INSERT INTO focus_interval_labels(context_interval_id,dimension,label_key,value,entity_id,confidence,source,rule_id,labeler_run_id,labeler_version,evidence_json) VALUES(?,?,?,?,?,?,'rule',1,1,1,?)",
  );
  const activity = db.prepare(
    "INSERT INTO activity_intervals(start_ts,end_ts,source,source_context_interval_id,activity,project_entity_id,service_entity_id,tool_entity_id,agent_session_id,confidence,labeler_version,evidence_json,episode_key) VALUES(?,?,'os_focus',?,?,?,?,?,?,0.91,1,?,?)",
  );
  const source = db.prepare(
    'INSERT INTO activity_context_sources(activity_interval_id,context_interval_id) VALUES(?,?)',
  );
  db.transaction(() => {
    for (let i = 0; i < counts.focusIntervals; i++) {
      const start = now - YEAR_DAYS * DAY + Math.floor((i * YEAR_DAYS * DAY) / counts.focusIntervals);
      const end = start + 25_000 + (i % 8) * 5_000;
      const project = (i % 80) * 2 + 1;
      const repository = (i % 80) * 2 + 2;
      const tool = 201 + (i % 5);
      const session = (i % 120) + 1;
      focus.run(
        start,
        end,
        end,
        key(i, 10),
        `app-${i % 24}.desktop`,
        i % 6 === 0 ? `Document ${i % 300} — Editor` : null,
        2_000 + (i % 32),
        i % 97 === 0 ? '{"candidateCount":2,"reason":"multiple_agents"}' : '{}',
        4_000 + (i % 8),
        `session-${i % 12}`,
        `project-${i % 80}`,
        `%${i % 16}`,
        5_000 + (i % 32),
        key(project, 11),
        `project-${i % 80}`,
        ['pi', 'nvim', 'shell', 'claude', 'codex'][i % 5],
        repository,
        project,
        i % 3 === 0 ? 'pi' : null,
        i % 3 === 0 ? 6_000 + (i % 16) : null,
        i % 3 === 0 ? session : null,
        i % 3 === 0 ? `session-${(session - 1).toString().padStart(3, '0')}` : null,
        i % 9 === 0 ? 'busy' : 'idle',
        i % 3 === 0 ? 'presence_lease' : null,
        i % 3 === 0 ? 1 : null,
      );
      const contextId = lastInsertId(db);
      const evidence = '[{"fact":"app.id","v":2}]';
      label.run(contextId, 'activity.primary', key(i, 20), i % 11 === 0 ? 'entertainment' : 'work', null, 0.92, evidence);
      label.run(contextId, 'project', key(i % 80, 21), null, project, 1, evidence);
      if (i % 2 === 0) label.run(contextId, 'tool', key(i % 5, 22), null, tool, 0.96, evidence);
      activity.run(start, end, contextId, i % 11 === 0 ? 'entertainment' : 'work', project, i % 11 === 0 ? 161 + (i % 40) : null, tool, i % 3 === 0 ? session : null, '{"sources":["os","tmux"]}', key(Math.floor(i / 3), 23));
      const activityId = lastInsertId(db);
      source.run(activityId, contextId);
    }
  })();

  insertBrowserRows(db, now, 0, counts.browserTransitions, 30);
  insertAttentionRows(db, now, 0, counts.browserAttentionIntervals, 40);
  insertAgentRows(db, now, 0, counts.agentStateEvents, 50);
  db.run(
    'UPDATE browser_ingestion_metrics SET accepted_events=?,heartbeat_upserts=?,persisted_transitions=?,last_ingest_ts=? WHERE id=1',
    [counts.browserTransitions, YEAR_DAYS * 1_200, counts.browserTransitions, now],
  );
}

function insertBrowserRows(db: Database, now: number, offset: number, amount: number, salt: number): void {
  const insert = db.prepare(
    "INSERT INTO browser_events(client_id,event_key,client_sequence,window_id,tab_id,event_ts,received_ts,event_type,domain,origin,title,browser_focused,tab_active,document_visible,audible,media_state,picture_in_picture,media_position,media_duration,playback_rate,private,payload_version) VALUES(1,?,?,1,?,?,?,?,?,?,?,?,?,?,?,?,0,?,?,?,0,1)",
  );
  const receipt = db.prepare(
    'INSERT INTO browser_event_receipts(client_id,event_key,client_sequence,received_ts) VALUES(1,?,?,?)',
  );
  db.transaction(() => {
    for (let j = 0; j < amount; j++) {
      const i = offset + j;
      const ts = now - YEAR_DAYS * DAY + Math.floor((j * YEAR_DAYS * DAY) / Math.max(1, amount));
      const service = i % 40;
      const eventKey = key(i, salt);
      const received = ts + 20 + (i % 80);
      insert.run(eventKey, i + 1, (i % 24) + 1, ts, received, ['active_tab', 'navigation', 'media_play', 'media_pause'][i % 4], `service-${service}.example`, `https://service-${service}.example`, i % 7 === 0 ? `Opt-in title ${i % 500}` : null, 1, 1, i % 13 ? 1 : 0, i % 5 === 0 ? 1 : 0, i % 5 === 0 ? 'playing' : 'paused', i % 5 === 0 ? i % 3_600 : null, i % 5 === 0 ? 3_600 : null, i % 9 === 0 ? 1.25 : 1);
      receipt.run(eventKey, i + 1, received);
    }
  })();
}

function insertAttentionRows(db: Database, now: number, offset: number, amount: number, salt: number): void {
  const insert = db.prepare(
    "INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,origin,service_entity_id,browser_focused,tab_active,document_visible,media_playing,audible,picture_in_picture,os_focus_verified,os_focus_state,confidence,closed_reason) VALUES(1,1,?,?,?,?,?,?,?,1,1,?,?,?,0,?,?,?,'transition')",
  );
  db.transaction(() => {
    for (let j = 0; j < amount; j++) {
      const i = offset + j;
      const ts = now - YEAR_DAYS * DAY + Math.floor((j * YEAR_DAYS * DAY) / Math.max(1, amount));
      const media = i % 4 === 0 ? 1 : 0;
      const verified = i % 9 === 0 ? 0 : 1;
      insert.run((i % 24) + 1, ts, ts + 20_000 + (i % 50) * 1_000, key(Math.floor(i / 3), salt), `service-${i % 40}.example`, `https://service-${i % 40}.example`, 161 + (i % 40), i % 8 ? 1 : 0, media, media, verified, verified ? 'verified' : 'unknown', verified ? 0.98 : 0.72);
    }
  })();
}

function insertAgentRows(db: Database, now: number, offset: number, amount: number, salt: number): void {
  const insert = db.prepare(
    "INSERT INTO agent_state_events(event_key,harness,source_key,native_session_id,event_ts,state,instance_id,pid,process_start_identity,lease_expires_ts,transition_kind) VALUES(?,'pi','local',?,?,?,'benchmark-instance',4242,'linux:12345',?,'state_change')",
  );
  db.transaction(() => {
    for (let j = 0; j < amount; j++) {
      const i = offset + j;
      const eventTs = now - YEAR_DAYS * DAY + Math.floor((j * YEAR_DAYS * DAY) / Math.max(1, amount));
      insert.run(key(i, salt), `session-${(i % 120).toString().padStart(3, '0')}`, eventTs, i % 2 === 0 ? 'busy' : 'idle', eventTs + 30_000);
    }
  })();
}

function refillTransitionChurn(db: Database, now: number): void {
  insertBrowserRows(db, now, counts.browserTransitions, Math.floor(counts.browserTransitions / 10), 31);
  insertAttentionRows(db, now, counts.browserAttentionIntervals, Math.floor(counts.browserAttentionIntervals / 10), 41);
  insertAgentRows(db, now, counts.agentStateEvents, Math.floor(counts.agentStateEvents / 10), 51);
}

function populateNaiveYear(db: Database, now: number): void {
  db.exec(`
    CREATE TABLE naive_samples(
      id INTEGER PRIMARY KEY,start_ts INTEGER NOT NULL,focused_app TEXT NOT NULL,focused_title TEXT,
      terminal_session TEXT,terminal_window TEXT,terminal_pane TEXT,cwd_display TEXT,foreground_tool TEXT,
      repository_key BLOB,project_key BLOB,agent_session TEXT,ambiguity_json TEXT NOT NULL,evidence_json TEXT NOT NULL
    );
    CREATE INDEX idx_naive_samples_start ON naive_samples(start_ts);
    CREATE INDEX idx_naive_samples_app_start ON naive_samples(focused_app,start_ts);
    CREATE TABLE naive_labels(
      id INTEGER PRIMARY KEY,sample_id INTEGER NOT NULL,dimension TEXT NOT NULL,value TEXT,label_key BLOB NOT NULL,
      confidence REAL NOT NULL,source TEXT NOT NULL,evidence_json TEXT NOT NULL
    );
    CREATE INDEX idx_naive_labels_sample ON naive_labels(sample_id);
    CREATE INDEX idx_naive_labels_dimension_value ON naive_labels(dimension,value,sample_id);
  `);
  const sample = db.prepare(
    'INSERT INTO naive_samples(start_ts,focused_app,focused_title,terminal_session,terminal_window,terminal_pane,cwd_display,foreground_tool,repository_key,project_key,agent_session,ambiguity_json,evidence_json) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)',
  );
  const label = db.prepare(
    "INSERT INTO naive_labels(sample_id,dimension,value,label_key,confidence,source,evidence_json) VALUES(?,?,?,?,0.92,'rule',?)",
  );
  db.transaction(() => {
    for (let i = 0; i < counts.naiveSamples; i++) {
      sample.run(now - YEAR_DAYS * DAY + i * 30_000, `app-${i % 24}.desktop`, i % 6 === 0 ? `Document ${i % 300} — Editor` : null, `session-${i % 12}`, `project-${i % 80}`, `%${i % 16}`, `project-${i % 80}`, ['pi', 'nvim', 'shell'][i % 3], key(i % 80, 60), key(i % 80, 61), i % 3 === 0 ? `session-${i % 120}` : null, '{"candidateCount":1}', '{"provenance":["os_focus","tmux"]}');
      const id = lastInsertId(db);
      const evidence = '[{"fact":"app.id","extractorVersion":2}]';
      label.run(id, 'activity.primary', i % 11 === 0 ? 'entertainment' : 'work', key(i, 62), evidence);
      label.run(id, 'project', `project-${i % 80}`, key(i % 80, 63), evidence);
      label.run(id, 'tool', ['pi', 'nvim', 'shell'][i % 3], key(i % 3, 64), evidence);
    }
  })();
}

type ObjectAllocationResult =
  | { capability: 'dbstat'; objects: { name: string; table: string; kind: string; allocatedBytes: number }[] }
  | { capability: 'database-live-pages-fallback'; reason: 'dbstat_unavailable' };

function objectAllocations(db: Database): ObjectAllocationResult {
  try {
    const bytes = new Map<string, number>();
    for (const row of db.query('SELECT name,SUM(pgsize) bytes FROM dbstat GROUP BY name').all() as { name: string; bytes: number }[]) {
      bytes.set(row.name, Number(row.bytes));
    }
    const objects = (db.query("SELECT name,tbl_name,type FROM sqlite_master WHERE type IN ('table','index') ORDER BY name").all() as { name: string; tbl_name: string; type: string }[])
      .filter((row) => bytes.has(row.name))
      .map((row) => ({ name: row.name, table: row.tbl_name, kind: row.type, allocatedBytes: bytes.get(row.name) ?? 0 }));
    return { capability: 'dbstat', objects };
  } catch {
    return { capability: 'database-live-pages-fallback', reason: 'dbstat_unavailable' };
  }
}

const categories: Record<string, Set<string>> = {
  entitiesAndContext: new Set(['context_entities', 'context_entity_aliases', 'context_entity_relations', 'focus_context_intervals', 'activity_intervals', 'activity_context_sources']),
  labelsAndRelabeling: new Set(['label_rules', 'labeler_runs', 'focus_interval_labels', 'manual_activity_labels', 'context_runtime_state', 'relabel_jobs', 'context_backfill_state', 'derived_version_prune_jobs']),
  browserTransitionsAndAttention: new Set(['browser_pairing_codes', 'browser_pairing_rate_limits', 'browser_clients', 'browser_request_rate_limits', 'browser_current_states', 'browser_event_receipts', 'browser_events', 'browser_ingestion_metrics', 'browser_attention_intervals']),
  agentPresence: new Set(['agent_presence_leases', 'agent_state_events']),
  policiesAndInterventions: new Set(['attention_policies', 'policy_overrides', 'intervention_events']),
  sourceHealth: new Set(['source_health']),
};

function summarizeContextAllocation(db: Database, allocation: ObjectAllocationResult): { totalBytes: number; categories: { category: string; allocatedBytes: number; objects?: string[] }[] } {
  if (allocation.capability !== 'dbstat') {
    const pages = databasePages(db);
    return { totalBytes: pages.liveBytes, categories: [{ category: 'wholeDatabaseLivePagesFallback', allocatedBytes: pages.liveBytes }] };
  }
  const result = Object.entries(categories).map(([category, tables]) => {
    const objects = allocation.objects.filter((object) => tables.has(object.table));
    return { category, allocatedBytes: objects.reduce((sum, object) => sum + object.allocatedBytes, 0), objects: objects.map((object) => object.name) };
  });
  return { totalBytes: result.reduce((sum, category) => sum + category.allocatedBytes, 0), categories: result };
}

function databasePages(db: Database): { pageSize: number; pageCount: number; freelistPages: number; allocatedBytes: number; reusableBytes: number; liveBytes: number } {
  const pageSize = Number((db.query('PRAGMA page_size').get() as { page_size: number }).page_size);
  const pageCount = Number((db.query('PRAGMA page_count').get() as { page_count: number }).page_count);
  const freelistPages = Number((db.query('PRAGMA freelist_count').get() as { freelist_count: number }).freelist_count);
  return { pageSize, pageCount, freelistPages, allocatedBytes: pageSize * pageCount, reusableBytes: pageSize * freelistPages, liveBytes: pageSize * (pageCount - freelistPages) };
}

async function fileBytes(path: string): Promise<number> {
  try {
    return (await stat(path)).size;
  } catch {
    return 0;
  }
}

function lastInsertId(db: Database): number {
  const row = db.query('SELECT last_insert_rowid() id').get() as { id: number };
  return Number(row.id);
}

function key(value: number, salt: number): Uint8Array {
  const output = new Uint8Array(32);
  const view = new DataView(output.buffer);
  view.setUint32(0, salt);
  view.setBigUint64(8, BigInt(value));
  return output;
}
