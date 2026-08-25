import { afterEach, describe, expect, test } from 'bun:test';
import { chmod, mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';
import { dlopen, FFIType, ptr } from 'bun:ffi';
import { Database } from 'bun:sqlite';
import { serializeConfigRequest, serializeEventBatch, serializePairRequest } from '../browser-extension/src/transport';
import type { BrowserEvent } from '../browser-extension/src/types';

const root = resolve(import.meta.dir, '..');
const bin = process.env.BMS_WATCHDOG_BIN || join(root, 'target/debug/bms-watchdog');
const children: ReturnType<typeof Bun.spawn>[] = [];

const libc = dlopen('libc.so.6', {
  wait4: {
    args: [FFIType.i32, FFIType.ptr, FFIType.i32, FFIType.ptr],
    returns: FFIType.i32,
  },
});

afterEach(async () => {
  for (const child of children.splice(0)) {
    child.kill();
    try { await child.exited; } catch {}
  }
});

describe('bms-watchdog e2e', () => {
  test('start --once initializes storage and captures a sample', async () => {
    const fx = await makeFixture();
    const result = Bun.spawnSync([bin, 'start', '--once', ...fixtureArgs(fx)]);
    expect(result.exitCode).toBe(0);
    const dbPath = join(fx.dir, 'data/bms-watchdog.sqlite');
    expect(existsSync(dbPath)).toBe(true);
    expect(new TextDecoder().decode(result.stdout)).toContain('rows=');
    const db = new Database(dbPath, { readonly: true });
    expect(db.query("SELECT COUNT(*) AS n FROM schema_migrations WHERE name='001_initial.sql'").get().n).toBe(1);
    expect(db.query("SELECT COUNT(*) AS n FROM schema_migrations WHERE name='002_usb_power_context.sql'").get().n).toBe(1);
    expect(db.query("SELECT COUNT(*) AS n FROM schema_migrations WHERE name='003_agent_usage.sql'").get().n).toBe(1);
    expect(db.query("SELECT COUNT(*) AS n FROM schema_migrations WHERE name='004_focus_context.sql'").get().n).toBe(1);
    expect(db.query("SELECT COUNT(*) AS n FROM schema_migrations WHERE name='005_context_integrations.sql'").get().n).toBe(1);
    expect(db.query('SELECT COUNT(*) AS n FROM focus_context_intervals').get().n).toBe(1);
    expect(db.query('SELECT COUNT(*) AS n FROM activity_intervals').get().n).toBe(1);
    expect(db.query('PRAGMA foreign_key_check').all()).toEqual([]);
    db.close();
    await rm(fx.dir, { recursive: true, force: true });
  });

  test('serves static frontend and compatible API shapes', async () => {
    const fx = await makeFixture();
    const port = await freePort();
    const child = Bun.spawn([bin, 'start', '--port', String(port), '--poll-interval-seconds', '1', ...fixtureArgs(fx)], {
      stdout: 'pipe',
      stderr: 'pipe',
    });
    children.push(child);
    await waitFor(`http://127.0.0.1:${port}/healthz`);

    const html = await fetch(`http://127.0.0.1:${port}/`).then(r => r.text());
    expect(html).toContain('/assets/app.js');

    const status = await jsonFetch(`http://127.0.0.1:${port}/api/status`);
    expect(status.latestBattery.capacity).toBeCloseTo(80, 1);
    expect(status.latestBattery.on_battery).toBe(1);
    expect(status.latestEnvironment.brightness_percent).toBe(50);
    expect(status.latestEnvironment.usb_power_source).toBe(1);
    expect(status.latestEnvironment.usb_power_w).toBe(5);
    expect(status.latestEnvironment.focused_app).toBe('org.example.Editor');
    expect(status.context.trackingEnabled).toBe(true);
    expect(status.context.latest.focusedApp).toBe('org.example.editor');
    expect(status.context.latest.focusedTitle).toBe('Fixture workspace');
    expect(status.processRows).toBeGreaterThan(0);
    expect(status.dbStats.spanDays).toBeGreaterThanOrEqual(0);
    expect(status.dbStats.sizeBytes).toBeGreaterThan(0);

    const series = await jsonFetch(`http://127.0.0.1:${port}/api/series?hours=1&top=8`);
    expect(Array.isArray(series.apps)).toBe(true);
    expect(series.apps).toContain('bms-watchdog');
    expect(series.apps).toContain('System / baseline');
    expect(series.points.length).toBeGreaterThan(0);
    expect(series.points.at(-1).apps['bms-watchdog']).toBeDefined();

    const afterTs = series.points.at(-1).ts - 1;
    const incremental = await jsonFetch(`http://127.0.0.1:${port}/api/series?hours=1&top=8&after_ts=${afterTs}`);
    expect(incremental.incremental).toBe(true);
    expect(incremental.points.every((p: any) => p.ts > afterTs)).toBe(true);

    const groups = await jsonFetch(`http://127.0.0.1:${port}/api/groups?hours=1`);
    expect(Array.isArray(groups.groups)).toBe(true);
    expect(groups.groups.some((g: any) => g.app === 'bms-watchdog')).toBe(true);

    const processes = await jsonFetch(`http://127.0.0.1:${port}/api/processes`);
    expect(processes.rows.some((r: any) => r.is_self === 1 && r.app === 'bms-watchdog')).toBe(true);

    const context = await jsonFetch(`http://127.0.0.1:${port}/api/context/latest`);
    expect(context.trackingEnabled).toBe(true);
    expect(context.latest.focusedPid).toBe(4242);
    expect(Array.isArray(context.latest.labels)).toBe(true);
    const summary = await jsonFetch(`http://127.0.0.1:${port}/api/activity/summary?group=activity`);
    expect(summary.metric).toBe('focus_time');
    expect(summary.meta.rulesRevision).toBeGreaterThanOrEqual(0);
    const timeline = await jsonFetch(`http://127.0.0.1:${port}/api/activity/timeline`);
    expect(Array.isArray(timeline.buckets)).toBe(true);
    const unknown = await jsonFetch(`http://127.0.0.1:${port}/api/context/unknown`);
    expect(Array.isArray(unknown.unknown)).toBe(true);
    const storage = await jsonFetch(`http://127.0.0.1:${port}/api/context/storage`);
    expect(storage.pageSize * storage.pageCount).toBeGreaterThan(0);
    expect(storage.objects.find((item:any)=>item.name==='focus_context_intervals')?.rows).toBeGreaterThan(0);

    const rejectedRule = await fetch(`http://127.0.0.1:${port}/api/context/rules`, { method: 'POST', headers: {'content-type':'application/json'}, body: JSON.stringify(ruleFixture()) });
    expect(rejectedRule.status).toBe(403);
    const unsupportedBrowserRule={...ruleFixture(),conditions:[{fact:'browser.domain',operator:'domainSuffix',value:'youtube.com'}]};
    const unsupportedRuleResponse=await fetch(`http://127.0.0.1:${port}/api/context/rules`,{method:'POST',headers:{'content-type':'application/json','origin':`http://127.0.0.1:${port}`,'x-watchdog-csrf':'1'},body:JSON.stringify(unsupportedBrowserRule)});expect(unsupportedRuleResponse.status).toBe(400);
    const createdRule = await postJson(`http://127.0.0.1:${port}/api/context/rules`, ruleFixture(), port);
    expect(createdRule.id).toBeGreaterThan(0);
    expect(createdRule.rule).toBeDefined();
    const relabel = await jsonFetch(`http://127.0.0.1:${port}/api/context/relabel`);
    expect(relabel.jobs.some((job:any)=>job.id===createdRule.relabelJobId)).toBe(true);
    const deletedRule = await fetch(`http://127.0.0.1:${port}/api/context/rules/${createdRule.id}`, { method: 'DELETE', headers: { origin:`http://127.0.0.1:${port}`, 'x-watchdog-csrf':'1' } }).then(response => response.json()) as any;
    expect(deletedRule).toMatchObject({ deleted:true, id:createdRule.id });

    const unauthConfig = await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/config`);
    expect(unauthConfig.ok).toBeFalse();
    const unauthClients = await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/clients`);
    expect(unauthClients.status).toBe(403);
    const pairCode = await postJson(`http://127.0.0.1:${port}/api/integrations/browser/v1/pair-code`, {}, port);
    const extensionOrigin = 'moz-extension://fixture-extension';
    const clientId = '11111111-1111-4111-8111-111111111111';
    const pairBody = serializePairRequest({ pairingCode: pairCode.code, clientId, displayName: 'Fixture browser', browserFamily: 'firefox', extensionVersion: '1.0.0', privacyMode: 'domains-only', selectedMediaOrigins: [] });
    const pairedResponse = await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/pair`, {method:'POST',headers:{'content-type':'application/json','origin':extensionOrigin},body:JSON.stringify(pairBody)});
    expect(pairedResponse.ok).toBe(true);
    expect(pairedResponse.headers.get('access-control-allow-origin')).toBe(extensionOrigin);
    const paired = await pairedResponse.json() as any;
    const queueHealth={depth:2,bytes:1024,dropped:1,permanentRejected:0,recoverySnapshots:1,oldestTimestamp:Date.now()-1000,lastSuccessfulSync:Date.now()-2000,lastDataLoss:Date.now()-3000};
    const configUpdate=await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/config`,{method:'POST',headers:{'content-type':'application/json','origin':extensionOrigin,'authorization':`Bearer ${paired.token}`},body:JSON.stringify(serializeConfigRequest(clientId,{endpoint:`http://127.0.0.1:${port}`,privacyMode:'domains-only',includePrivate:false,selectedMediaOrigins:['https://youtube.com'],displayName:'Fixture browser',failClosed:false},queueHealth))});
    expect(configUpdate.ok).toBeTrue();
    const authenticatedConfig=await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/config`,{headers:{origin:extensionOrigin,authorization:`Bearer ${paired.token}`}}).then(response=>response.json()) as any;
    expect(authenticatedConfig.selectedMediaOrigins).toEqual(['https://youtube.com']);
    expect(authenticatedConfig.queueHealth).toMatchObject({depth:2,bytes:1024,dropped:1,recoverySnapshots:1});
    const eventTs=Date.now();
    const browserEvent=(eventKey:string,clientSequence:number,eventType:BrowserEvent['eventType'],extra:Partial<BrowserEvent>={}):BrowserEvent=>({payloadVersion:1,eventKey,clientSequence,eventTimestamp:eventTs+clientSequence,clientId,eventType,windowId:1,tabId:2,domain:'example.com',origin:'https://example.com',active:true,browserFocused:true,documentVisible:true,audible:false,mediaState:'none',pictureInPicture:false,private:false,...extra});
    const sendEvents=(events:BrowserEvent[])=>fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/events:batch`,{method:'POST',headers:{'content-type':'application/json','origin':extensionOrigin,'authorization':`Bearer ${paired.token}`},body:JSON.stringify(serializeEventBatch(events,queueHealth))}).then(async r=>({response:r,body:await r.json()}));
    let ingest=await sendEvents([browserEvent('event-0001',1,'snapshot')]);expect(ingest.response.ok).toBe(true);expect(ingest.body.accepted).toBe(1);
    ingest=await sendEvents([browserEvent('event-0001',1,'snapshot')]);expect(ingest.body.duplicates).toBe(1);
    ingest=await sendEvents([browserEvent('heartbeat-0002',2,'heartbeat')]);expect(ingest.body.heartbeatUpserts).toBe(1);
    ingest=await sendEvents([browserEvent('private-0003',3,'snapshot',{private:true as false})]);expect(ingest.body.rejected).toHaveLength(1);
    const hostile = serializeEventBatch([browserEvent('hostile-0004',4,'snapshot')]) as any; hostile.events[0].url='https://example.com/private';
    expect((await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/events:batch`,{method:'POST',headers:{'content-type':'application/json','origin':extensionOrigin,'authorization':`Bearer ${paired.token}`},body:JSON.stringify(hostile)})).status).toBe(422);
    const sourceHealth=await jsonFetch(`http://127.0.0.1:${port}/api/context/source-health`);const browserHealth=sourceHealth.sources.find((item:any)=>item.source==='browser'&&typeof item.detail?.queueDepth==='number');expect(browserHealth.detail).toMatchObject({queueDepth:2,queueBytes:1024,queueDropped:1,recoverySnapshots:1});
    const fixtureDb=new Database(join(fx.data,'bms-watchdog.sqlite'),{readonly:true});expect(fixtureDb.query('SELECT COUNT(*) n FROM browser_events').get().n).toBe(1);fixtureDb.close();
    const revoke=await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/client`,{method:'DELETE',headers:{'origin':extensionOrigin,'authorization':`Bearer ${paired.token}`}});expect(revoke.ok).toBe(true);
    const revokedConfig=await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/config`,{headers:{'origin':extensionOrigin,'authorization':`Bearer ${paired.token}`}});expect(revokedConfig.ok).toBe(false);

    const agentUsage = await jsonFetch(`http://127.0.0.1:${port}/api/agents/usage?range=24h&group=harness&agent=all&top=8`);
    expect(agentUsage.agents).toEqual([]);
    expect(agentUsage.groupBy).toBe('harness');
    expect(agentUsage.groups).toEqual([]);
    expect(agentUsage.summary.totalTokens).toBe(0);
    expect(agentUsage.points[0].totalTokens).toBe(0);

    await rm(fx.dir, { recursive: true, force: true });
  });

  test('dashboard correction, entity, export, and deletion routes use production state', async () => {
    const fx = await makeFixture();
    expect(Bun.spawnSync([bin, 'start', '--once', ...fixtureArgs(fx)]).exitCode).toBe(0);
    const dbPath = join(fx.data, 'bms-watchdog.sqlite');
    const setup = new Database(dbPath);
    const context = setup.query('SELECT id,start_ts,end_ts FROM focus_context_intervals ORDER BY id DESC LIMIT 1').get() as any;
    setup.run("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts)VALUES('project',randomblob(32),'Source project',?,?)", [context.start_ts, context.end_ts]);
    const sourceId = Number(setup.query('SELECT last_insert_rowid() id').get().id);
    setup.run("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts)VALUES('project',randomblob(32),'Target project',?,?)", [context.start_ts, context.end_ts]);
    const targetId = Number(setup.query('SELECT last_insert_rowid() id').get().id);
    setup.run("INSERT INTO context_entity_aliases(entity_id,alias_kind,alias_value,source,confidence,created_ts)VALUES(?,'user','source-alias','user',1,?)", [sourceId, Date.now()]);
    setup.run("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts,archived)VALUES('service',randomblob(32),'Bulk archived service',?,?,1)", [context.start_ts, context.end_ts]);
    const bulkEntityId = Number(setup.query('SELECT last_insert_rowid() id').get().id);
    setup.run("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts,archived)VALUES('service',randomblob(32),'Policy blocked service',?,?,1)", [context.start_ts, context.end_ts]);
    const blockedEntityId = Number(setup.query('SELECT last_insert_rowid() id').get().id);
    setup.run("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts)VALUES('service',randomblob(32),'Unarchived service',?,?)", [context.start_ts, context.end_ts]);
    const unarchivedEntityId = Number(setup.query('SELECT last_insert_rowid() id').get().id);
    const bulkAlias = setup.prepare("INSERT INTO context_entity_aliases(entity_id,alias_kind,alias_value,source,confidence,created_ts)VALUES(?,'fixture',?,'user',1,?)");
    const bulkActivity = setup.prepare("INSERT INTO activity_intervals(start_ts,end_ts,source,activity,service_entity_id,confidence,labeler_version,evidence_json)VALUES(?,?,'fixture','unknown',?,0.2,1,'{}')");
    setup.exec('BEGIN');
    for (let index = 0; index < 1201; index++) { bulkAlias.run(bulkEntityId, `bulk-alias-${index}`, Date.now()); bulkActivity.run(context.start_ts + index * 2, context.start_ts + index * 2 + 1, bulkEntityId); }
    setup.exec('COMMIT');
    setup.run("INSERT INTO context_entity_aliases(entity_id,alias_kind,alias_value,source,confidence,created_ts)VALUES(?,'fixture','blocked-alias','user',1,?)", [blockedEntityId, Date.now()]);
    setup.run("INSERT INTO attention_policies(name,scope_json,metric,window_kind,budget_seconds,enforcement,revision,created_ts,updated_ts)VALUES('entity block',?,'tab_active_seconds','calendar-day',60,'observe',1,?,?)", [JSON.stringify({ nested: { serviceEntityId: blockedEntityId } }), Date.now(), Date.now()]);
    const blockedPolicyId = Number(setup.query('SELECT last_insert_rowid() id').get().id);
    const aliasId = Number(setup.query('SELECT last_insert_rowid() id').get().id);
    setup.run('UPDATE focus_context_intervals SET project_entity_id=? WHERE id=?', [sourceId, context.id]);
    setup.run('UPDATE activity_intervals SET project_entity_id=? WHERE source_context_interval_id=?', [sourceId, context.id]);
    setup.close();

    const port = await freePort();
    const child = Bun.spawn([bin, 'start', '--port', String(port), '--poll-interval-seconds', '30', ...fixtureArgs(fx)], { stdout: 'pipe', stderr: 'pipe' });
    children.push(child);
    await waitFor(`http://127.0.0.1:${port}/healthz`);
    const base = `http://127.0.0.1:${port}`;
    const headers = { 'content-type': 'application/json', origin: base, 'x-watchdog-csrf': '1' };

    const unarchivedDelete = await fetch(`${base}/api/context/entities/${unarchivedEntityId}`, { method: 'DELETE', headers });
    expect(unarchivedDelete.status).toBe(400);
    const blockedDelete = await fetch(`${base}/api/context/entities/${blockedEntityId}`, { method: 'DELETE', headers });
    expect(blockedDelete.status).toBe(400);
    const blockedDb = new Database(dbPath); expect(blockedDb.query('SELECT COUNT(*) n FROM context_entity_aliases WHERE entity_id=?').get(blockedEntityId).n).toBe(1); blockedDb.run('DELETE FROM attention_policies WHERE id=?', [blockedPolicyId]); blockedDb.close();
    const bulkDeleteResponse = await fetch(`${base}/api/context/entities/${bulkEntityId}`, { method: 'DELETE', headers });
    const bulkDelete = await bulkDeleteResponse.json() as any;
    expect(bulkDeleteResponse.ok, JSON.stringify(bulkDelete)).toBeTrue();
    expect(bulkDelete.receipt.deletedRows).toMatchObject({ context_entities: 1, context_entity_aliases: 1201 });
    expect(bulkDelete.receipt.clearedReferences['activity_intervals.service_entity_id']).toBe(1201);
    expect(bulkDelete.receipt.maxRowsInTransactionObserved).toBe(500);
    expect(bulkDelete.receipt.batchCount).toBeGreaterThanOrEqual(7);
    const malformedDb = new Database(dbPath); malformedDb.run("INSERT INTO attention_policies(name,scope_json,metric,window_kind,budget_seconds,enforcement,revision,created_ts,updated_ts)VALUES('malformed','{','tab_active_seconds','calendar-day',60,'observe',2,?,?)", [Date.now(), Date.now()]); malformedDb.close();
    const malformedDelete = await fetch(`${base}/api/context/entities/${blockedEntityId}`, { method: 'DELETE', headers });
    expect(malformedDelete.status).toBe(400);
    const malformedVerify = new Database(dbPath); expect(malformedVerify.query('SELECT COUNT(*) n FROM context_entity_aliases WHERE entity_id=?').get(blockedEntityId).n).toBe(1); expect(malformedVerify.query('PRAGMA foreign_key_check').all()).toHaveLength(0); malformedVerify.run("DELETE FROM attention_policies WHERE name='malformed'"); malformedVerify.close();

    const wrongOrigin = await fetch(`${base}/api/context/manual-labels`, { method: 'POST', headers: { ...headers, origin: 'http://localhost:1' }, body: JSON.stringify({ contextIntervalId: context.id, startTs: context.start_ts, endTs: context.end_ts, dimension: 'activity.primary', value: 'work', scope: 'interval' }) });
    expect(wrongOrigin.status).toBe(403);
    const widenedScope = await fetch(`${base}/api/context/manual-labels`, { method: 'POST', headers, body: JSON.stringify({ contextIntervalId: context.id, startTs: context.start_ts, endTs: context.end_ts, dimension: 'activity.primary', value: 'work', scope: 'domain' }) });
    expect(widenedScope.status).toBe(400);
    const mismatchedMetadata = await fetch(`${base}/api/context/manual-labels`, { method: 'POST', headers, body: JSON.stringify({ contextIntervalId: context.id, startTs: context.start_ts + 1, endTs: context.end_ts, dimension: 'activity.primary', value: 'work', scope: 'interval' }) });
    expect(mismatchedMetadata.status).toBe(400);
    const correction = await postJson(`${base}/api/context/manual-labels`, { contextIntervalId: context.id, startTs: context.start_ts, endTs: context.end_ts, dimension: 'activity.primary', value: 'work', scope: 'interval', note: 'fixture' }, port);
    expect(correction.undo.url).toBe(`/api/context/manual-labels/${correction.id}`);
    const history = await jsonFetch(`${base}/api/context/manual-labels`);
    expect(history.labels.some((label: any) => label.id === correction.id)).toBeTrue();
    const undo = await fetch(`${base}${correction.undo.url}`, { method: 'DELETE', headers }).then(response => response.json()) as any;
    expect(undo.deleted).toBeTrue();

    const entityPreview = await postJson(`${base}/api/context/deletions`, { kind: 'entity', entityId: sourceId, preview: true }, port);
    expect(entityPreview.counts).toMatchObject({ context_entities: 1, context_entity_aliases: 1, 'focus_context_intervals.project': 1, 'activity_intervals.project': 1 });
    expect(entityPreview.counts.context_entities).not.toBe(2);
    const beforeMerge = await jsonFetch(`${base}/api/activity/summary?group=project&from=${context.start_ts}&to=${context.end_ts + 1}`);
    expect(beforeMerge.groups.some((group: any) => group.label === 'Source project' && group.durationMs > 0)).toBeTrue();
    await postJson(`${base}/api/context/entities/merge`, { sourceId, targetId }, port);
    const afterMerge = await jsonFetch(`${base}/api/activity/summary?group=project&from=${context.start_ts}&to=${context.end_ts + 1}`);
    expect(afterMerge.groups.some((group: any) => group.label === 'Target project' && group.durationMs > 0)).toBeTrue();

    const projectLabel = await postJson(`${base}/api/context/manual-labels`, { contextIntervalId: context.id, startTs: context.start_ts, endTs: context.end_ts, dimension: 'project', entityId: targetId, scope: 'interval' }, port);
    const split = await postJson(`${base}/api/context/entities/${targetId}/split`, { displayName: 'Split project', identity: '/fixture/split', aliasIds: [aliasId], contextIntervalIds: [context.id], manualLabelIds: [projectLabel.id] }, port);
    const splitSummary = await jsonFetch(`${base}/api/activity/summary?group=project&from=${context.start_ts}&to=${context.end_ts + 1}`);
    expect(splitSummary.groups.some((group: any) => group.label === 'Split project')).toBeTrue();
    await postJson(`${base}/api/context/entities/${split.id}/archive`, {}, port);
    const deletedEntityResponse = await fetch(`${base}/api/context/entities/${split.id}`, { method: 'DELETE', headers });
    const deletedEntity = await deletedEntityResponse.json() as any;
    expect(deletedEntityResponse.ok, JSON.stringify(deletedEntity)).toBeTrue();
    expect(deletedEntity.deleted).toBeTrue();

    await postJson(`${base}/api/context/entities/${targetId}/aliases`, { aliasKind: 'user', aliasValue: 'canonical-alias', confidence: 1 }, port);
    const jsonExport = await postJson(`${base}/api/context/export`, { kind: 'entities', format: 'json', from: context.start_ts, to: context.end_ts + 1 }, port);
    expect(jsonExport.data.entities.find((entity: any) => entity.id === targetId).aliases.some((alias: any) => alias.value === 'canonical-alias')).toBeTrue();
    const csvExport = await postJson(`${base}/api/context/export`, { kind: 'entities_and_aliases', format: 'csv', from: context.start_ts, to: context.end_ts + 1 }, port);
    expect(csvExport.content).toContain('canonical-alias');
    const ambiguousCsv = await fetch(`${base}/api/context/export`, { method: 'POST', headers, body: JSON.stringify({ kind: 'all', format: 'csv', from: context.start_ts, to: context.end_ts + 1 }) });
    expect(ambiguousCsv.status).toBe(400);
    const preview = await postJson(`${base}/api/context/deletions`, { kind: 'derived_labels', preview: true, rebuildDerived: true }, port);
    expect(preview).toMatchObject({ preview: true, requiresConfirmation: true, rebuildDerived: true });
    expect(preview.counts).toHaveProperty('activity_intervals');
    await rm(fx.dir, { recursive: true, force: true });
  });

  test('large metric ranges, exact sessions, health, storage, and bounded deletions stay complete', async () => {
    const fx = await makeFixture();
    expect(Bun.spawnSync([bin, 'start', '--once', ...fixtureArgs(fx)]).exitCode).toBe(0);
    const dbPath = join(fx.data, 'bms-watchdog.sqlite');
    const db = new Database(dbPath);
    const now = Date.now();
    const bulkFrom = now - 4_000_000;
    const bulkCount = 2505;
    const bulkTo = bulkFrom + bulkCount * 1000;
    db.run("INSERT INTO browser_clients(client_key,client_instance_id,display_name,browser_family,extension_version,secret_hash,allowed_origin,privacy_mode,paired_ts,last_seen_ts,queue_depth,queue_health_observed_ts)VALUES(randomblob(32),'large-client','Large fixture','firefox','1.0.0',randomblob(32),'moz-extension://large','domains-only',?,?,3,?)", [now, now, now - 300_000]);
    const clientId = Number(db.query('SELECT last_insert_rowid() id').get().id);
    db.run("INSERT INTO browser_clients(client_key,client_instance_id,display_name,browser_family,extension_version,secret_hash,allowed_origin,privacy_mode,paired_ts,last_seen_ts)VALUES(randomblob(32),'other-client','Other fixture','firefox','1.0.0',randomblob(32),'moz-extension://other','domains-only',?,?)", [now, now]);
    const otherClientId = Number(db.query('SELECT last_insert_rowid() id').get().id);
    const attention = db.prepare("INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,origin,browser_focused,tab_active,document_visible,media_playing,audible,picture_in_picture,os_focus_verified,os_focus_state,confidence,closed_reason)VALUES(?,1,?,?,?,?, 'large.example','https://large.example',1,1,1,1,0,0,1,'verified',1,'fixture')");
    db.exec('BEGIN');
    for (let index = 0; index < bulkCount; index++) attention.run(clientId, index + 1, bulkFrom + index * 1000, bulkFrom + (index + 1) * 1000, crypto.getRandomValues(new Uint8Array(32)));
    db.exec('COMMIT');
    const midnightDate = new Date(now); const offsetText = new TextDecoder().decode(Bun.spawnSync(['date', '+%z']).stdout).trim(); const offsetSign = offsetText.startsWith('-') ? -1 : 1; const offsetMinutes = offsetSign * (Number(offsetText.slice(1, 3)) * 60 + Number(offsetText.slice(3, 5))); const midnight = Date.UTC(midnightDate.getUTCFullYear(), midnightDate.getUTCMonth(), midnightDate.getUTCDate()) - offsetMinutes * 60_000;
    const episodeKey = new Uint8Array(32).fill(77);
    attention.run(clientId, 3001, midnight - 1000, midnight, episodeKey);
    attention.run(clientId, 3001, midnight, midnight + 1000, episodeKey);
    attention.run(clientId, 4001, now - 4 * 86_400_000, now - 4 * 86_400_000 + 1000, crypto.getRandomValues(new Uint8Array(32)));
    db.run("INSERT INTO browser_current_states(client_id,window_id,tab_id,lease_key,last_event_key,episode_key,client_sequence,state_started_ts,last_observed_ts,expires_ts,domain,origin,title,browser_focused,tab_active,document_visible,audible,media_state,private,payload_version)VALUES(?,1,99,randomblob(32),randomblob(32),randomblob(32),1,?,?,?,'large.example','https://large.example','Current title',1,1,1,0,'none',0,1)", [clientId, now - 1000, now, now + 45_000]);
    const receiptInsert = db.prepare("INSERT INTO browser_event_receipts(client_id,event_key,client_sequence,received_ts)VALUES(?,randomblob(32),?,?)");
    const bulkEventInsert = db.prepare("INSERT INTO browser_events(client_id,event_key,client_sequence,window_id,tab_id,event_ts,received_ts,event_type,domain,origin,title,media_state,payload_version)VALUES(?,randomblob(32),?,2,?,?,?,'snapshot','large.example','https://large.example','Bulk title','none',1)");
    const bulkStateInsert = db.prepare("INSERT INTO browser_current_states(client_id,window_id,tab_id,lease_key,last_event_key,episode_key,client_sequence,state_started_ts,last_observed_ts,expires_ts,domain,origin,title,browser_focused,tab_active,document_visible,audible,media_state,private,payload_version)VALUES(?,2,?,randomblob(32),randomblob(32),randomblob(32),?,?,?,?, 'large.example','https://large.example','Bulk current title',1,1,1,0,'none',0,1)");
    db.exec('BEGIN');
    for (let index = 0; index < 1201; index++) {
      receiptInsert.run(clientId, index + 2, now);
      bulkEventInsert.run(clientId, index + 2, index + 10_000, now, now);
      bulkStateInsert.run(clientId, index + 10_000, index + 2, now - 1000, now, now + 45_000);
    }
    db.exec('COMMIT');
    db.run("INSERT INTO browser_event_receipts(client_id,event_key,client_sequence,received_ts)VALUES(?,randomblob(32),1,?)", [clientId, now]);
    const browserEventSql = "INSERT INTO browser_events(client_id,event_key,client_sequence,window_id,tab_id,event_ts,received_ts,event_type,domain,origin,title,media_state,payload_version)VALUES(?,randomblob(32),1,1,1,?,?,'snapshot','large.example','https://large.example',?,'none',1)";
    db.run(browserEventSql, [clientId, now, now, 'Client title']);
    db.run(browserEventSql, [otherClientId, now, now, 'Other title']);

    db.run("INSERT INTO agent_sources(agent_type,source_key,display_name,adapter_version,capabilities_json,limitations_json)VALUES('pi','local','Pi',1,'{}','{}')");
    const sourceId = Number(db.query('SELECT last_insert_rowid() id').get().id);
    db.run("INSERT INTO agent_sessions(source_id,native_session_id,name,created_ts)VALUES(?,'exact-native','Exact busy session',?)", [sourceId, now - 60_000]);
    const sessionId = Number(db.query('SELECT last_insert_rowid() id').get().id);
    const busyStart = now - 20_000;
    db.run("INSERT INTO agent_state_events(event_key,harness,source_key,native_session_id,event_ts,state,instance_id,pid,process_start_identity,lease_expires_ts,transition_kind)VALUES(randomblob(32),'pi','local','exact-native',?,'busy','instance-exact',42,'linux:42',?,'opened')", [busyStart, busyStart + 5000]);
    db.run("INSERT INTO agent_state_events(event_key,harness,source_key,native_session_id,event_ts,state,instance_id,pid,process_start_identity,lease_expires_ts,transition_kind)VALUES(randomblob(32),'pi','local','exact-native',?,'idle','instance-exact',42,'linux:42',?,'expired')", [busyStart + 10_000, busyStart + 15_000]);
    db.run("INSERT INTO source_health(source,last_success_ts,last_error_ts,success_count,error_count,detail_json)VALUES('terminal',?,?,4,1,'{\"resolvedAttempts\":2,\"ambiguousAttempts\":1,\"lastOutcome\":\"error\"}') ON CONFLICT(source)DO UPDATE SET last_success_ts=excluded.last_success_ts,last_error_ts=excluded.last_error_ts,success_count=4,error_count=1,detail_json=excluded.detail_json", [now - 1000, now]);
    const activityInsert = db.prepare("INSERT INTO activity_intervals(start_ts,end_ts,source,activity,confidence,labeler_version,evidence_json)VALUES(?,?,'fixture','unknown',0.2,1,'{}')");
    db.exec('BEGIN');
    for (let index = 0; index < 1201; index++) activityInsert.run(now - 10_000 - index * 2, now - 9999 - index * 2);
    db.exec('COMMIT');
    db.close();

    const port = await freePort();
    const child = Bun.spawn([bin, 'start', '--port', String(port), '--poll-interval-seconds', '30', ...fixtureArgs(fx)], { stdout: 'pipe', stderr: 'pipe' });
    children.push(child);
    await waitFor(`http://127.0.0.1:${port}/healthz`);
    const base = `http://127.0.0.1:${port}`;

    const summary = await jsonFetch(`${base}/api/activity/summary?metric=focused_playback&group=service&from=${bulkFrom}&to=${bulkTo}`);
    expect(summary.meta.observedMs).toBe(bulkCount * 1000);
    expect(summary.groups.find((group: any) => group.label === 'large.example').durationMs).toBe(bulkCount * 1000);
    expect(summary).toMatchObject({ truncated: false, hardIntervalCeiling: 250000 });
    const timeline = await jsonFetch(`${base}/api/activity/timeline?metric=focused_playback&resolution=15m&from=${bulkFrom}&to=${bulkTo}`);
    expect(timeline.buckets.reduce((sum: number, bucket: any) => sum + bucket.observedMs, 0)).toBe(bulkCount * 1000);
    const episodes = await jsonFetch(`${base}/api/activity/episodes?metric=focused_playback&minSeconds=1&from=${midnight - 2000}&to=${midnight + 2000}`);
    expect(episodes.episodes).toHaveLength(1);
    expect(episodes.episodes[0]).toMatchObject({ durationMs: 2000, crossesLocalMidnight: true, persistedEpisodeKey: Buffer.from(episodeKey).toString('hex').toUpperCase() });

    const busySummary = await jsonFetch(`${base}/api/activity/summary?metric=agent_busy&group=agentSession&from=${busyStart - 1000}&to=${busyStart + 20_000}`);
    expect(busySummary.groups).toContainEqual(expect.objectContaining({ key: `agent-session:${sessionId}`, label: 'Exact busy session', durationMs: 5000 }));
    expect(busySummary.meta).toMatchObject({ metricDurationMs: 5000, observedMs: 10_000 });
    const busyTimeline = await jsonFetch(`${base}/api/activity/timeline?metric=agent_busy&group=agentSession&resolution=1s&from=${busyStart}&to=${busyStart + 20_000}`);
    expect(busyTimeline.buckets.reduce((sum: number, bucket: any) => sum + bucket.observedMs, 0)).toBe(5000);
    expect(busyTimeline.meta).toMatchObject({ metricDurationMs: 5000, observedMs: 10_000 });
    expect(busyTimeline.buckets.reduce((sum: number, bucket: any) => sum + (bucket.groupsMs[`agent-session:${sessionId}`] || 0), 0)).toBe(5000);
    expect(busyTimeline.groupLabels[`agent-session:${sessionId}`]).toBe('Exact busy session');
    const busyEpisodes = await jsonFetch(`${base}/api/activity/episodes?metric=agent_busy&group=agentSession&from=${busyStart}&to=${busyStart + 10_000}`);
    expect(busyEpisodes.episodes[0]).toMatchObject({ label: 'Exact busy session', durationMs: 5000 });

    const health = await jsonFetch(`${base}/api/context/health`);
    const terminal = health.sources.find((source: any) => source.source === 'terminal');
    expect(terminal.detail).toMatchObject({ resolvedAttempts: 2, producerStatus: 'observed' });
    expect(terminal.detail.attempts).toBeGreaterThanOrEqual(5);
    expect(terminal.detail.matchRate).toBeCloseTo(terminal.detail.resolvedAttempts / terminal.detail.attempts);
    const presence = health.sources.find((source: any) => source.source === 'presence');
    expect(presence.detail.expiredCount).toBe(1);
    expect(presence.detail.lastEventTs).toBeGreaterThan(0);
    expect(health.sources.find((source: any) => source.source === 'policy').detail.degraded).toBeTrue();
    expect(health.activeVersionCoverage).toHaveProperty('observedMs');
    expect(health.intervalMaintenance).toMatchObject({ staleBrowserStates: 0 });

    const storage = await jsonFetch(`${base}/api/context/storage`);
    expect(['dbstat', 'dbstat_unavailable']).toContain(storage.allocationCapability);
    if (storage.allocationCapability === 'dbstat') {
      expect(storage.objects.find((object: any) => object.name === 'browser_attention_intervals').allocatedBytes).toBeGreaterThan(0);
      expect(storage.objects.some((object: any) => object.kind === 'index' && object.name.startsWith('sqlite_autoindex_browser_clients_') && object.allocatedBytes > 0)).toBeTrue();
      expect(storage.annualizedGrowthBytes).toBeGreaterThan(0);
    } else {
      expect(storage.objects.every((object: any) => object.allocatedBytes == null)).toBeTrue();
      expect(storage.annualizedGrowthBytes).toBeNull();
    }
    expect(storage.currentLabelBytes).toBeNull();
    expect(storage).toHaveProperty('pruningLagMs');
    expect(storage).toHaveProperty('compactionLagMs');

    const preview = await postJson(`${base}/api/context/deletions`, { kind: 'browser_client', clientId, preview: true }, port);
    expect(preview.counts).toMatchObject({ browser_clients: 1, browser_event_receipts: 1202, browser_events: 1202, 'browser_events.titles': 1202, 'browser_current_states.titles': 1202, browser_attention_intervals: bulkCount + 3 });
    const allPreview = await postJson(`${base}/api/context/deletions`, { kind: 'all_context', preview: true }, port);
    expect(allPreview.counts).toHaveProperty('browser_event_receipts', 1202);
    expect(allPreview.counts).toHaveProperty('browser_pairing_codes');
    expect(allPreview.counts).toHaveProperty('source_health');

    const titleDeletion = await postJson(`${base}/api/context/deletions`, { kind: 'browser_titles' }, port);
    expect(titleDeletion.receipt.clearedFields).toMatchObject({ 'browser_events.title': 1203, 'browser_current_states.title': 1202 });
    expect(titleDeletion.receipt.batchCount).toBe(6);
    expect(titleDeletion.receipt.maxRowsInTransactionObserved).toBe(500);
    const restoreTitles = new Database(dbPath);
    restoreTitles.run("UPDATE browser_events SET title='Restored selected title' WHERE client_id=?", [clientId]);
    restoreTitles.run("UPDATE browser_current_states SET title='Restored selected current title' WHERE client_id=?", [clientId]);
    restoreTitles.close();
    const clientDeletion = await postJson(`${base}/api/context/deletions`, { kind: 'browser_client', clientId }, port);
    expect(clientDeletion.receipt.deletedRows).toMatchObject({ browser_clients: 1, browser_events: 1202, browser_event_receipts: 1202, browser_current_states: 1202, browser_attention_intervals: bulkCount + 3 });
    expect(clientDeletion.receipt.clearedFields).toMatchObject({ 'browser_events.title': 1202, 'browser_current_states.title': 1202 });
    expect(clientDeletion.receipt.batchCount).toBeGreaterThanOrEqual(17);
    expect(clientDeletion.receipt.maxRowsInTransactionObserved).toBe(500);
    const clientDeletedDb = new Database(dbPath, { readonly: true });
    expect(clientDeletedDb.query('PRAGMA foreign_key_check').all()).toHaveLength(0);
    expect(clientDeletedDb.query('SELECT COUNT(*) n FROM browser_clients WHERE id=?').get(otherClientId).n).toBe(1);
    clientDeletedDb.close();

    const deletion = await postJson(`${base}/api/context/deletions`, { kind: 'derived_labels', rebuildDerived: true }, port);
    expect(deletion.receipt).toMatchObject({ maxRowsPerTransaction: 500 });
    expect(deletion.receipt.batchCount).toBeGreaterThanOrEqual(3);
    expect(deletion).toMatchObject({ rebuildStatus: 'completed' });
    const rebuilt = await jsonFetch(`${base}/api/activity/summary?metric=focus&from=${now - 60_000}&to=${now + 1000}`);
    expect(rebuilt.meta.sampleCount).toBeGreaterThan(0);
    const allDeletion = await postJson(`${base}/api/context/deletions`, { kind: 'all_context' }, port);
    expect(allDeletion.receipt.maxRowsPerTransaction).toBe(500);
    expect(allDeletion.receipt.deletedCounts.browser_attention_intervals).toBe(0);
    expect(allDeletion.receipt.batchCount).toBeGreaterThanOrEqual(1);
    const deletedDb = new Database(dbPath, { readonly: true });
    expect(deletedDb.query('SELECT COUNT(*) n FROM browser_event_receipts').get().n).toBe(0);
    expect(deletedDb.query('SELECT status,last_environment_id,processed_samples FROM context_backfill_state WHERE id=1').get()).toMatchObject({ status: 'pending', last_environment_id: 0, processed_samples: 0 });
    expect(deletedDb.query('SELECT accepted_events,heartbeat_upserts FROM browser_ingestion_metrics WHERE id=1').get()).toMatchObject({ accepted_events: 0, heartbeat_upserts: 0 });
    deletedDb.close();
    await rm(fx.dir, { recursive: true, force: true });
  });

  test('adversarial aggregate identity and metric-aware project breaks are exact', async () => {
    const fx = await makeFixture();
    expect(Bun.spawnSync([bin, 'start', '--once', ...fixtureArgs(fx)], { env: { ...process.env, TZ: 'UTC' } }).exitCode).toBe(0);
    const dbPath = join(fx.data, 'bms-watchdog.sqlite');
    const db = new Database(dbPath);
    const current = new Date();
    const day = Date.UTC(current.getUTCFullYear(), current.getUTCMonth(), current.getUTCDate());
    const key = (value: number) => new Uint8Array(32).fill(value);
    db.run("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts)VALUES('service',randomblob(32),'Shared service name',?,?)", [day, day + 3 * 86_400_000]);
    const serviceOne = Number(db.query('SELECT last_insert_rowid() id').get().id);
    db.run("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts)VALUES('service',randomblob(32),'Shared service name',?,?)", [day, day + 3 * 86_400_000]);
    const serviceTwo = Number(db.query('SELECT last_insert_rowid() id').get().id);
    db.run("INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts)VALUES('project',randomblob(32),'Dynamic project',?,?)", [day, day + 3 * 86_400_000]);
    const projectId = Number(db.query('SELECT last_insert_rowid() id').get().id);
    db.run("INSERT INTO browser_clients(client_key,client_instance_id,display_name,browser_family,extension_version,secret_hash,allowed_origin,privacy_mode,paired_ts,last_seen_ts)VALUES(randomblob(32),'analytics-client','Analytics','firefox','1',randomblob(32),'moz-extension://analytics','domains-only',?,?)", [day, day]);
    const clientId = Number(db.query('SELECT last_insert_rowid() id').get().id);
    const activity = db.prepare("INSERT INTO activity_intervals(start_ts,end_ts,source,activity,project_entity_id,confidence,labeler_version,evidence_json)VALUES(?,?,'fixture',?,?,?,1,'{}')");
    activity.run(day + 3_600_000, day + 7_200_000, 'work', projectId, 0.9);
    activity.run(day + 25 * 3_600_000, day + 25 * 3_600_000 + 1_800_000, 'work', projectId, 0.9);
    activity.run(day + 7_500_000, day + 7_620_000, 'entertainment', null, 0.8);
    const attention = db.prepare("INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,service_entity_id,browser_focused,tab_active,document_visible,media_playing,audible,picture_in_picture,os_focus_verified,os_focus_state,confidence,closed_reason)VALUES(?,1,?,?,?,?,?,?,1,1,1,1,0,0,?,?,?,'fixture')");
    const add = (tab: number, start: number, end: number, episode: Uint8Array, service: number | null, state = 'verified', confidence = 1, domain = 'stable.example') => attention.run(clientId, tab, start, end, episode, domain, service, state === 'verified' ? 1 : 0, state, confidence);
    add(1, day + 3 * 3_600_000, day + 3 * 3_600_000 + 10_000, key(10), serviceOne, 'verified', 0.95);
    add(2, day + 3 * 3_600_000 + 2_000, day + 3 * 3_600_000 + 3_000, key(10), null, 'verified', 0.2, 'changed-label.example');
    add(3, day + 3 * 3_600_000 + 9_000, day + 3 * 3_600_000 + 12_000, key(10), serviceOne, 'verified', 0.95);
    add(4, day + 4 * 3_600_000, day + 4 * 3_600_000 + 60_000, key(11), serviceOne);
    add(5, day + 4 * 3_600_000, day + 4 * 3_600_000 + 60_000, key(12), serviceTwo);
    add(6, day + 86_400_000 - 1000, day + 86_400_000, key(13), serviceOne);
    add(6, day + 86_400_000, day + 86_400_000 + 1000, key(13), serviceOne);
    const states = ['verified', 'mixed', 'unknown', 'contradicted'];
    states.forEach((state, index) => add(20 + index, day + 6 * 3_600_000 + index * 10_000, day + 6 * 3_600_000 + (index + 1) * 10_000, key(20 + index), serviceOne, state));
    add(30, day + 7_500_000, day + 7_560_000, key(30), serviceOne, 'verified', 0.9);
    add(31, day + 7_500_000, day + 7_590_000, key(31), serviceTwo, 'mixed', 0.7);
    const openStart = day + 5 * 3_600_000;
    db.run("INSERT INTO focus_context_intervals(start_ts,end_ts,last_observed_ts,context_key,extractor_version,focused_app,closed_reason)VALUES(?,?,?,randomblob(32),1,'firefox','fixture')", [openStart, openStart + 10_000, openStart + 10_000]);
    db.run("INSERT INTO browser_current_states(client_id,window_id,tab_id,lease_key,last_event_key,episode_key,client_sequence,state_started_ts,last_observed_ts,expires_ts,domain,browser_focused,tab_active,document_visible,audible,media_state,payload_version)VALUES(?,1,99,randomblob(32),randomblob(32),?,1,?,?,?,'stable.example',1,1,1,0,'playing',1)", [clientId, key(40), openStart, openStart + 10_000, openStart + 10_000]);
    add(99, openStart, openStart + 10_000, key(40), serviceOne, 'verified', 0.98);
    db.close();

    const port = await freePort();
    const child = Bun.spawn([bin, 'start', '--port', String(port), '--poll-interval-seconds', '30', ...fixtureArgs(fx)], { stdout: 'pipe', stderr: 'pipe', env: { ...process.env, TZ: 'UTC' } });
    children.push(child);
    await waitFor(`http://127.0.0.1:${port}/healthz`);
    const base = `http://127.0.0.1:${port}`;
    const assertMeta = (payload: any) => {
      expect(payload.meta).toMatchObject({ schemaVersion: 1, classifierVersion: expect.any(Number), rulesRevision: expect.any(Number), filters: expect.any(Object), completeness: { truncated: false, hardIntervalCeiling: 250000 }, coverage: expect.any(Object) });
      expect(payload.meta.monitoringCoverage).toBeGreaterThanOrEqual(0);
      expect(payload.meta.monitoringCoverage).toBeLessThanOrEqual(1);
    };
    const identityFrom = day + 3 * 3_600_000;
    const identityTo = day + 5 * 3_600_000;
    const summary = await jsonFetch(`${base}/api/activity/summary?metric=verified_focused_playback&group=service&from=${identityFrom}&to=${identityTo}`);
    assertMeta(summary);
    expect(summary.meta.metricDurationMs).toBe(72_000);
    expect(summary.meta.episodeCount).toBe(3);
    expect(summary.meta.sampleCount).toBe(5);
    expect(summary.groups.filter((group: any) => group.label === 'Shared service name')).toHaveLength(2);
    expect(summary.groups.find((group: any) => group.key === `service:${serviceOne}`).confidence.highMs).toBe(72_000);
    const timeline = await jsonFetch(`${base}/api/activity/timeline?metric=verified_focused_playback&group=service&resolution=1m&from=${identityFrom}&to=${identityTo}`);
    assertMeta(timeline);
    expect(timeline.groupLabels[`service:${serviceOne}`]).toBe('Shared service name');
    expect(timeline.groupLabels[`service:${serviceTwo}`]).toBe('Shared service name');
    const episodePayload = await jsonFetch(`${base}/api/activity/episodes?metric=verified_focused_playback&from=${identityFrom}&to=${identityTo}`);
    assertMeta(episodePayload);
    const nested = episodePayload.episodes.find((episode: any) => episode.persistedEpisodeKey === Buffer.from(key(10)).toString('hex').toUpperCase());
    expect(nested.durationMs).toBe(12_000);
    expect(nested.labels).toContain('changed-label.example');
    expect(episodePayload.episodeCount).toBe(3);
    const cross = await jsonFetch(`${base}/api/activity/episodes?metric=verified_focused_playback&from=${day + 86_400_000 - 2000}&to=${day + 86_400_000 + 2000}`);
    expect(cross.episodes).toHaveLength(1);
    expect(cross.episodes[0]).toMatchObject({ durationMs: 2000, crossesLocalMidnight: true });
    const beforeKey = summary.groups.find((group: any) => group.key === `service:${serviceOne}`).key;
    const renamedDb = new Database(dbPath); renamedDb.run("UPDATE context_entities SET display_name='Renamed service' WHERE id=?", [serviceOne]); renamedDb.close();
    const renamed = await jsonFetch(`${base}/api/activity/summary?metric=verified_focused_playback&group=service&from=${identityFrom}&to=${identityTo}`);
    expect(renamed.groups.find((group: any) => group.key === beforeKey).label).toBe('Renamed service');

    const semanticsFrom = day + 6 * 3_600_000;
    const semanticsTo = semanticsFrom + 40_000;
    const focused = await jsonFetch(`${base}/api/activity/summary?metric=focused_playback&from=${semanticsFrom}&to=${semanticsTo}`);
    const verified = await jsonFetch(`${base}/api/activity/summary?metric=verified_focused_playback_seconds&from=${semanticsFrom}&to=${semanticsTo}`);
    const background = await jsonFetch(`${base}/api/activity/summary?metric=background_playback&from=${semanticsFrom}&to=${semanticsTo}`);
    expect(focused.meta).toMatchObject({ metricDurationMs: 30_000, observedMs: 40_000, unknownMs: 10_000, coverage: { browserObservedMs: 40_000, osObservedMs: 30_000, jointObservedMs: 30_000 } });
    expect(verified.meta.metricDurationMs).toBe(10_000);
    expect(background.meta.metricDurationMs).toBe(10_000);
    expect(focused.meta.metricDurationMs + background.meta.metricDurationMs).toBe(40_000);
    const focusedTimeline = await jsonFetch(`${base}/api/activity/timeline?metric=focused_playback&resolution=10s&from=${semanticsFrom}&to=${semanticsTo}`);
    expect(focusedTimeline.meta).toMatchObject({ metricDurationMs: 30_000, observedMs: 40_000, unknownMs: 10_000, coverage: { browserObservedMs: 40_000, osObservedMs: 30_000, jointObservedMs: 30_000 } });
    const filteredEpisodes = await jsonFetch(`${base}/api/activity/episodes?metric=verified_focused_playback&minSeconds=30&from=${identityFrom}&to=${identityTo}`);
    expect(filteredEpisodes.episodes).toHaveLength(2);
    expect(filteredEpisodes.meta).toMatchObject({ metricDurationMs: 60_000, sampleCount: 2, episodeCount: 2, confidence: { lowMs: 0, mediumMs: 0, highMs: 60_000 } });
    const unsupportedGroup = await fetch(`${base}/api/activity/summary?metric=focused_playback&group=project&from=${semanticsFrom}&to=${semanticsTo}`);
    expect(unsupportedGroup.status).toBe(400);
    const open = await jsonFetch(`${base}/api/activity/summary?metric=verified_focused_playback&from=${openStart}&to=${openStart + 10_000}`);
    expect(open.meta.metricDurationMs).toBe(10_000);
    expect(open.meta.sampleCount).toBe(1);

    const requestFrom = day - 12 * 3_600_000;
    const requestTo = day + 60 * 3_600_000;
    const belowGap = await jsonFetch(`${base}/api/activity/project-breaks?metric=focused_playback&maxFollowGapSeconds=299&from=${requestFrom}&to=${requestTo}`);
    expect(belowGap.projects.find((project: any) => project.projectEntityId === projectId).breakMs).toBe(0);
    const projectFocused = await jsonFetch(`${base}/api/activity/project-breaks?metric=focused_playback&maxFollowGapSeconds=300&from=${requestFrom}&to=${requestTo}`);
    assertMeta(projectFocused);
    const project = projectFocused.projects.find((row: any) => row.projectEntityId === projectId);
    expect(project).toMatchObject({ key: `project:${projectId}`, exposureMs: 5_400_000, activeCompleteDays: 2, breakMs: 90_000, episodeCount: 2, confidence: { lowMs: 0, mediumMs: 30_000, highMs: 60_000 } });
    expect(project.breakSecondsPerExposureHour).toBeCloseTo(60);
    expect(projectFocused.meta).toMatchObject({ metricDurationMs: 90_000, observedMs: 90_000, unknownMs: 0, confidence: { lowMs: 0, mediumMs: 30_000, highMs: 60_000 }, coverage: { jointObservedMs: 90_000 } });
    expect(projectFocused).toMatchObject({ fromTs: day, toTs: day + 2 * 86_400_000, excludedPartialBoundaryDays: 2 });
    const projectVerified = await jsonFetch(`${base}/api/activity/project-breaks?metric=verified_focused_playback&maxFollowGapSeconds=300&from=${requestFrom}&to=${requestTo}`);
    expect(projectVerified.projects.find((row: any) => row.projectEntityId === projectId)).toMatchObject({ breakMs: 60_000, episodeCount: 1 });
    const partial = await jsonFetch(`${base}/api/activity/project-breaks?metric=focused_playback&from=${day + 1000}&to=${day + 2000}`);
    expect(partial.projects).toHaveLength(0);
    expect(partial.meta.completeness).toMatchObject({ completeDaysOnly: true, excludedPartialBoundaryDays: 1 });
    const ceilingDb = new Database(dbPath);
    const outputTs = day + 9 * 3_600_000;
    ceilingDb.run("WITH RECURSIVE seq(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM seq WHERE x<2000) INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,browser_focused,tab_active,document_visible,media_playing,audible,picture_in_picture,os_focus_verified,os_focus_state,confidence,closed_reason) SELECT ?,8,x,?,?,randomblob(32),printf('output-%d.example',x),1,1,1,1,0,0,1,'verified',1,'output-limit' FROM seq", [clientId, outputTs, outputTs + 1]);
    const listNow = Date.now();
    ceilingDb.run("WITH RECURSIVE seq(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM seq WHERE x<2000) INSERT INTO context_entities(kind,canonical_key,display_name,first_seen_ts,last_seen_ts) SELECT 'service',randomblob(32),printf('Output entity %d',x),?,? FROM seq", [listNow, listNow]);
    ceilingDb.run("WITH RECURSIVE seq(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM seq WHERE x<2000) INSERT INTO label_rules(name,priority,enabled,origin,conditions_json,outputs_json,stop_dimensions_json,revision,created_ts,updated_ts) SELECT printf('output-rule-%d',x),1,1,'user','[]','[]','[]',x+100,?,? FROM seq", [listNow, listNow]);
    ceilingDb.run("WITH RECURSIVE seq(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM seq WHERE x<2000) INSERT INTO attention_policies(name,scope_json,metric,window_kind,budget_seconds,enforcement,revision,created_ts,updated_ts) SELECT printf('output-policy-%d',x),'{\"domains\":[\"limit.example\"]}','tab_active_seconds','calendar-day',60,'observe',x+100,?,? FROM seq", [listNow, listNow]);
    ceilingDb.close();
    for (const url of [
      `${base}/api/activity/summary?metric=verified_focused_playback&from=${outputTs - 1}&to=${outputTs + 2}`,
      `${base}/api/activity/timeline?metric=verified_focused_playback&resolution=1s&from=${outputTs - 1}&to=${outputTs + 2}`,
      `${base}/api/activity/episodes?metric=verified_focused_playback&from=${outputTs - 1}&to=${outputTs + 2}`,
      `${base}/api/context/entities`, `${base}/api/context/rules`, `${base}/api/attention/policies`,
    ]) {
      const response = await fetch(url); const error = await response.json() as any;
      expect(response.status, `${url}: ${JSON.stringify(error)}`).toBe(400);
      expect(error.error).toMatch(/more than|output limit/);
      expect(error.truncated).toBeUndefined();
    }
    const sourceCeilingDb = new Database(dbPath);
    const ceilingTs = day + 8 * 3_600_000;
    sourceCeilingDb.run("WITH RECURSIVE seq(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM seq WHERE x<250000) INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,service_entity_id,browser_focused,tab_active,document_visible,media_playing,audible,picture_in_picture,os_focus_verified,os_focus_state,confidence,closed_reason) SELECT ?,9,x,?,?,randomblob(32),'ceiling.example',?,1,1,1,0,0,0,1,'verified',1,'nonmatching-ceiling' FROM seq", [clientId, ceilingTs, ceilingTs + 1, serviceOne]);
    sourceCeilingDb.close();
    const ceilingResponse = await fetch(`${base}/api/activity/summary?metric=verified_focused_playback&from=${ceilingTs - 1}&to=${ceilingTs + 2}`);
    const ceilingError = await ceilingResponse.json() as any;
    expect(ceilingResponse.status).toBe(400);
    expect(ceilingError.error).toContain('source evidence');
    await rm(fx.dir, { recursive: true, force: true });
  });

  test('canonical browser policy lifecycle evaluates and audits every enforcement mode', async () => {
    const fx = await makeFixture();
    await writeFile(fx.focused, JSON.stringify({ app_id: 'zen-browser', title: 'Fixture browser', pid: 4242 }));
    const port = await freePort();
    const child = Bun.spawn([bin, 'start', '--port', String(port), '--poll-interval-seconds', '1', ...fixtureArgs(fx)], { stdout: 'pipe', stderr: 'pipe' });
    children.push(child);
    await waitFor(`http://127.0.0.1:${port}/healthz`);

    const policyFixture = await Bun.file(join(root, 'contracts/browser/v1/policy-create.json')).json() as any;
    const created: Array<{ id: number; revision: number; enforcement: string }> = [];
    for (const enforcement of ['observe', 'warn', 'soft-block', 'hard-block']) {
      const response = await postJson(`http://127.0.0.1:${port}/api/attention/policies`, { ...policyFixture, name: `${enforcement} fixture`, budgetSeconds: 1, warningSeconds: 1, enforcement, grace: { ...policyFixture.grace, graceSeconds: 0 } }, port);
      created.push({ ...response, enforcement });
    }

    const code = await postJson(`http://127.0.0.1:${port}/api/integrations/browser/v1/pair-code`, {}, port);
    const origin = 'moz-extension://policy-fixture';
    const clientId = '44444444-4444-4444-8444-444444444444';
    const pairResponse = await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/pair`, { method: 'POST', headers: { 'content-type': 'application/json', origin }, body: JSON.stringify(serializePairRequest({ pairingCode: code.code, clientId, displayName: 'Policy browser', browserFamily: 'zen', extensionVersion: '1.0.0', privacyMode: 'selected-sites-media', selectedMediaOrigins: ['https://youtube.com'] })) });
    expect(pairResponse.ok).toBeTrue();
    const paired = await pairResponse.json() as any;
    const start = Date.now() - 2_000;
    const event = (eventKey: string, clientSequence: number, eventTimestamp: number, eventType: BrowserEvent['eventType'], mediaState: BrowserEvent['mediaState']): BrowserEvent => ({ payloadVersion: 1, eventKey, clientSequence, eventTimestamp, clientId, eventType, windowId: 1, tabId: 1, domain: 'youtube.com', origin: 'https://youtube.com', active: true, browserFocused: true, documentVisible: true, audible: mediaState === 'playing', mediaState, pictureInPicture: false, private: false });
    const eventResponse = await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/events:batch`, { method: 'POST', headers: { 'content-type': 'application/json', origin, authorization: `Bearer ${paired.token}` }, body: JSON.stringify(serializeEventBatch([event('policy-play-0001', 1, start, 'media_play', 'playing'), event('policy-pause-0002', 2, start + 2_000, 'media_pause', 'paused')])) });
    expect(eventResponse.ok).toBeTrue();
    const hostile={...event('policy-hostile-0003',3,start+2_001,'media_play','playing'),domain:'example.com',origin:'https://example.com'};
    const hostileResult=await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/events:batch`,{method:'POST',headers:{'content-type':'application/json',origin,authorization:`Bearer ${paired.token}`},body:JSON.stringify(serializeEventBatch([hostile]))}).then(response=>response.json()) as any;
    expect(hostileResult.rejected[0].reason).toContain('not selected');

    const state = await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/policy-state?domain=youtube.com`, { headers: { origin, authorization: `Bearer ${paired.token}` } }).then(response => response.json()) as any;
    expect(state.schemaVersion).toBe(1);
    expect(state.policies).toHaveLength(4);
    expect(new Set(state.decisions.map((decision: any) => decision.action))).toEqual(new Set(['observe', 'warn', 'soft-block', 'hard-block']));
    for (const action of ['warn', 'soft-block', 'hard-block']) {
      const decision = state.decisions.find((item: any) => item.action === action);
      const response = await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/interventions`, { method: 'POST', headers: { 'content-type': 'application/json', origin, authorization: `Bearer ${paired.token}` }, body: JSON.stringify({ schemaVersion: 1, policyId: decision.policyId, action, decisionRevision: decision.revision, eventTimestamp: Date.now(), remainingSeconds: decision.remainingSeconds }) });
      expect(response.ok).toBeTrue();
    }
    const soft = state.decisions.find((item: any) => item.action === 'soft-block');
    const snooze = await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/interventions`, { method: 'POST', headers: { 'content-type': 'application/json', origin, authorization: `Bearer ${paired.token}` }, body: JSON.stringify({ schemaVersion: 1, policyId: soft.policyId, action: 'snooze', decisionRevision: soft.revision, eventTimestamp: Date.now(), remainingSeconds: 0 }) });
    expect(snooze.ok).toBeTrue();
    const audit = await jsonFetch(`http://127.0.0.1:${port}/api/interventions?from=${start - 1000}&to=${Date.now() + 1000}`);
    expect(audit.events.map((item: any) => item.action)).toEqual(expect.arrayContaining(['warn', 'soft-block', 'hard-block', 'snooze']));
    await rm(fx.dir, { recursive: true, force: true });
  });

  test('indexes copied Pi parent/child history once through the unified API', async () => {
    const fx = await makeFixture();
    const sessionsDir = join(fx.dir, 'pi-sessions');
    await mkdir(sessionsDir, { recursive: true });
    const parentPath = join(sessionsDir, 'parent.jsonl');
    const childPath = join(sessionsDir, 'child.jsonl');
    const now = Date.now();
    const parentHeader = { type: 'session', version: 3, id: 'parent-session-12345678', timestamp: new Date(now - 60_000).toISOString(), cwd: '/workspace/parent-project' };
    const parentInfo = { type: 'session_info', id: 'parent-info', timestamp: new Date(now - 59_000).toISOString(), name: 'Parent work' };
    const copied = assistantEntry('parent-response', 'parent-entry', now - 50_000, 100, 0.25);
    const childHeader = { type: 'session', version: 3, id: 'child-session-abcdefgh', timestamp: new Date(now - 40_000).toISOString(), cwd: '/workspace/child-project', parentSession: parentPath };
    const childOnly = assistantEntry('child-response', 'child-entry', now - 30_000, 200, 0.5);
    await writeFile(parentPath, [parentHeader, parentInfo, copied].map(JSON.stringify).join('\n') + '\n');
    await writeFile(childPath, [childHeader, copied, childOnly].map(JSON.stringify).join('\n') + '\n');
    const lease={schemaVersion:1,eventId:'presence-event',harness:'pi',sourceKey:'local',instanceId:'fixture-instance',pid:4242,processStartIdentity:'linux:12345',nativeSessionId:childHeader.id,cwd:childHeader.cwd,state:'busy',observedAt:Date.now(),expiresAt:Date.now()+60_000};
    const leasePath=join(fx.presence,'pi-4242-fixture-instance.json');await writeFile(leasePath,JSON.stringify(lease));await chmod(leasePath,0o600);

    const port = await freePort();
    const args = fixtureArgs(fx).filter(arg => arg !== '--no-agent-tracking');
    const child = Bun.spawn([bin, 'start', '--port', String(port), '--poll-interval-seconds', '1', '--pi-sessions-dir', sessionsDir, ...args], { stdout: 'pipe', stderr: 'pipe' });
    children.push(child);
    await waitFor(`http://127.0.0.1:${port}/healthz`);
    const usage = await waitForAgentUsage(`http://127.0.0.1:${port}/api/agents/usage?range=7d&group=harness&agent=pi&top=8`, 2);

    expect(usage.agents.map((agent: any) => agent.type)).toEqual(['pi']);
    expect(usage.agents[0].indexer.initialScanComplete).toBe(true);
    expect(usage.summary.totalTokens).toBe(300);
    expect(usage.summary.responses).toBe(2);
    expect(usage.summary.totalCost).toBeCloseTo(0.75);
    expect(usage.coverage.costComplete).toBe(true);
    expect(usage.groupBy).toBe('harness');
    expect(usage.groups).toHaveLength(1);
    expect(usage.groups[0].label).toBe('Pi');
    expect(usage.groups[0].totalTokens).toBe(300);
    expect(usage.points.at(-1).groups['harness:pi'].tokens).toBe(300);
    const modelUsage = await jsonFetch(`http://127.0.0.1:${port}/api/agents/usage?range=7d&group=model&agent=pi&top=8`);
    expect(modelUsage.groupBy).toBe('model');
    expect(modelUsage.groups).toHaveLength(1);
    expect(modelUsage.groups[0].label).toBe('fixture-model');
    expect(modelUsage.groups[0].totalTokens).toBe(300);
    expect(modelUsage.points.at(-1).groups['model:fixture-model'].tokens).toBe(300);
    const sessionUsage = await jsonFetch(`http://127.0.0.1:${port}/api/agents/usage?range=7d&group=session&agent=pi&top=8`);
    expect(sessionUsage.groupBy).toBe('session');
    expect(sessionUsage.groups).toHaveLength(2);
    expect(sessionUsage.groups.find((group: any) => group.label === 'Parent work').totalTokens).toBe(100);
    expect(sessionUsage.groups.some((group: any) => group.label.startsWith('child-project · '))).toBe(true);
    expect(sessionUsage.groups.reduce((sum: number, group: any) => sum + group.totalTokens, 0)).toBe(300);
    const parent = usage.sessions.find((session: any) => session.nativeId === parentHeader.id);
    const childSession = usage.sessions.find((session: any) => session.nativeId === childHeader.id);
    expect(parent.totalTokens).toBe(100);
    expect(childSession.totalTokens).toBe(200);
    expect(usage.points[0].totalTokens).toBe(0);
    expect(usage.points.at(-1).totalTokens).toBe(300);
    await Bun.sleep(1100);
    const focusedContext=await jsonFetch(`http://127.0.0.1:${port}/api/context/latest`);
    expect(focusedContext.latest.agentType).toBe('pi');
    expect(focusedContext.latest.nativeAgentSessionId).toBe(childHeader.id);
    expect(focusedContext.latest.agentState).toBe('busy');
    expect(focusedContext.latest.agentSessionId).toBe(childSession.id);

    const html = await fetch(`http://127.0.0.1:${port}/`).then(response => response.text());
    expect(html).toContain('agentUsageChart');
    expect(html).toContain('data-agent-group="harness"');
    expect(html).toContain('data-agent-group="model"');
    expect(html).toContain('data-agent-group="session"');
    expect(html).not.toContain('data-agent-metric');
    const chartModule = await fetch(`http://127.0.0.1:${port}/assets/agent-usage-chart.js`).then(response => response.text());
    expect(chartModule).toContain('drawAgentUsageChart');
    expect(chartModule).toContain('positionTipAtCursor');
    expect(chartModule).not.toContain('pi-sessions');

    const db = new Database(join(fx.data, 'bms-watchdog.sqlite'), { readonly: true });
    expect(db.query('SELECT COUNT(*) AS n FROM agent_usage_events').get().n).toBe(2);
    expect(db.query('SELECT COUNT(*) AS n FROM agent_session_usage_refs').get().n).toBe(3);
    expect(db.query('SELECT length(event_key) AS n FROM agent_usage_events LIMIT 1').get().n).toBe(32);
    db.close();
    const dbText = new TextDecoder().decode(await Bun.file(join(fx.data, 'bms-watchdog.sqlite')).arrayBuffer());
    const walFile = Bun.file(join(fx.data, 'bms-watchdog.sqlite-wal'));
    const walText = await walFile.exists() ? new TextDecoder().decode(await walFile.arrayBuffer()) : '';
    expect(dbText + walText).not.toContain('private-parent-response');
    expect(dbText + walText).not.toContain('parent-response');
    await rm(fx.dir, { recursive: true, force: true });
  });

  test('battery polling, agent indexing, browser ingestion, and relabeling coexist', async () => {
    const fx=await makeFixture();const sessionsDir=join(fx.dir,'concurrent-sessions');await mkdir(sessionsDir,{recursive:true});const now=Date.now();
    expect(Bun.spawnSync([bin,'start','--once',...fixtureArgs(fx)]).exitCode).toBe(0);
    const seedDb=new Database(join(fx.data,'bms-watchdog.sqlite'));seedDb.run("INSERT INTO browser_clients(client_key,client_instance_id,display_name,browser_family,extension_version,secret_hash,allowed_origin,privacy_mode,paired_ts)VALUES(randomblob(32),'slow-query-client','Slow query','firefox','1',randomblob(32),'moz-extension://slow','domains-only',?)",[now]);const slowClient=Number(seedDb.query('SELECT last_insert_rowid() id').get().id);const slowInsert=seedDb.prepare("INSERT INTO browser_attention_intervals(client_id,window_id,tab_id,start_ts,end_ts,episode_key,domain,browser_focused,tab_active,document_visible,media_playing,audible,os_focus_verified,os_focus_state,confidence,closed_reason)VALUES(?,1,1,?,?,randomblob(32),'slow.example',1,1,1,1,0,1,'verified',1,'fixture')");seedDb.exec('BEGIN');for(let index=0;index<50_000;index++)slowInsert.run(slowClient,now-100_000+index*2,now-100_000+index*2+2);seedDb.exec('COMMIT');seedDb.close();
    await writeFile(join(sessionsDir,'session.jsonl'),[{type:'session',version:3,id:'concurrent-session',timestamp:new Date(now-10_000).toISOString(),cwd:'/workspace/concurrent'},assistantEntry('concurrent-response','entry',now-5_000,50,0.1)].map(JSON.stringify).join('\n')+'\n');
    const port=await freePort();const args=fixtureArgs(fx).filter(arg=>arg!=='--no-agent-tracking');const child=Bun.spawn([bin,'start','--port',String(port),'--poll-interval-seconds','1','--pi-sessions-dir',sessionsDir,...args],{stdout:'pipe',stderr:'pipe'});children.push(child);await waitFor(`http://127.0.0.1:${port}/healthz`);
    const code=await retryPostJson(`http://127.0.0.1:${port}/api/integrations/browser/v1/pair-code`,{},port);const origin='moz-extension://concurrency';const clientId='33333333-3333-4333-8333-333333333333';const pairRes=await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/pair`,{method:'POST',headers:{'content-type':'application/json','origin':origin},body:JSON.stringify(serializePairRequest({pairingCode:code.code,clientId,displayName:'Concurrency',browserFamily:'firefox',extensionVersion:'1.0.0',privacyMode:'domains-only',selectedMediaOrigins:[]}))});const paired=await pairRes.json() as any;
    const slowQuery=fetch(`http://127.0.0.1:${port}/api/activity/summary?metric=verified_focused_playback&from=${now-100_001}&to=${now+1}`);
    const started=performance.now();for(let i=0;i<20;i++){const event:BrowserEvent={payloadVersion:1,eventKey:`concurrent-${i}`,clientSequence:i+1,eventTimestamp:Date.now()+i,clientId,eventType:i?'active_tab':'snapshot',windowId:1,tabId:1,domain:'example.com',origin:'https://example.com',active:true,browserFocused:true,documentVisible:true,audible:false,mediaState:'none',pictureInPicture:false,private:false};const response=await fetch(`http://127.0.0.1:${port}/api/integrations/browser/v1/events:batch`,{method:'POST',headers:{'content-type':'application/json','origin':origin,'authorization':`Bearer ${paired.token}`},body:JSON.stringify(serializeEventBatch([event]))});const body=await response.json() as any;expect(response.ok,JSON.stringify(body)).toBeTrue();expect(body).toMatchObject({accepted:1,duplicates:0});expect(body.rejected).toHaveLength(0);}
    expect((await fetch(`http://127.0.0.1:${port}/healthz`)).ok).toBeTrue();expect((await fetch(`http://127.0.0.1:${port}/api/series?hours=1`)).ok).toBeTrue();expect(performance.now()-started).toBeLessThan(4_000);const slowResponse=await slowQuery;expect(slowResponse.ok).toBeTrue();const slowSummary=await slowResponse.json() as any;expect(slowSummary.meta).toMatchObject({sampleCount:50_000,metricDurationMs:100_000});await retryPostJson(`http://127.0.0.1:${port}/api/context/rules`,ruleFixture(),port);await retryPostJson(`http://127.0.0.1:${port}/api/context/relabel`,{fromTs:now-60_000,toTs:now+60_000},port);await Bun.sleep(2200);await waitForAgentUsage(`http://127.0.0.1:${port}/api/agents/usage?range=24h&agent=pi`,1);
    const db=new Database(join(fx.data,'bms-watchdog.sqlite'),{readonly:true});expect(db.query('SELECT COUNT(*) n FROM battery_samples').get().n).toBeGreaterThanOrEqual(2);expect(db.query('SELECT COUNT(*) n FROM browser_events').get().n).toBeGreaterThan(0);expect(db.query('SELECT COUNT(*) n FROM relabel_jobs').get().n).toBeGreaterThan(0);expect(db.query('PRAGMA foreign_key_check').all()).toEqual([]);db.close();await rm(fx.dir,{recursive:true,force:true});
  });

  test('single collection run stays within startup/resource budget', async () => {
    const fx = await makeFixture();
    const attempts = [await measureSingleRun(fx), await measureSingleRun(fx), await measureSingleRun(fx)];
    expect(attempts.every(metrics => metrics.exitCode === 0)).toBeTrue();
    expect(Math.min(...attempts.map(metrics => metrics.wallSeconds))).toBeLessThanOrEqual(0.1);
    expect(Math.min(...attempts.map(metrics => metrics.cpuSeconds))).toBeLessThanOrEqual(0.1);
    // The statically linked debug binary includes and exercises context plus agent telemetry.
    expect(Math.max(...attempts.map(metrics => metrics.maxRssKb))).toBeLessThanOrEqual(128 * 1024);
    await rm(fx.dir, { recursive: true, force: true });
  });

  test('sleep gap is recorded on the next collection run', async () => {
    const fx = await makeFixture();
    let result = Bun.spawnSync([bin, 'start', '--once', '--suspend-gap-seconds', '1', ...fixtureArgs(fx)]);
    expect(result.exitCode).toBe(0);
    const dbPath = join(fx.data, 'bms-watchdog.sqlite');
    let db = new Database(dbPath);
    db.run('UPDATE battery_samples SET ts=?, capacity=81, energy_wh=41 WHERE id=1', [Date.now() - 3000]);
    db.close();

    result = Bun.spawnSync([bin, 'start', '--once', '--suspend-gap-seconds', '1', ...fixtureArgs(fx)]);
    expect(result.exitCode).toBe(0);
    db = new Database(dbPath, { readonly: true });
    const event = db.query('SELECT kind, capacity_delta, energy_delta_wh FROM sleep_events').get() as any;
    expect(event.kind).toBe('suspend discharge');
    expect(event.capacity_delta).toBeLessThan(0);
    expect(event.energy_delta_wh).toBeLessThan(0);
    db.close();
    await rm(fx.dir, { recursive: true, force: true });
  });

  test('migrate old-db copies existing data and renames self app', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'bms-watchdog-migrate-'));
    const oldDb = join(dir, 'old.sqlite');
    const newData = join(dir, 'new-data');
    await createOldDbFixture(oldDb);
    const result = Bun.spawnSync([bin, 'migrate', 'old-db', '--source', oldDb, '--data-dir', newData]);
    expect(result.exitCode).toBe(0);

    const db = new Database(join(newData, 'bms-watchdog.sqlite'), { readonly: true });
    expect(db.query('SELECT COUNT(*) AS n FROM battery_samples').get().n).toBe(1);
    expect(db.query("SELECT app FROM process_identities WHERE id=1").get().app).toBe('bms-watchdog');
    expect(db.query("SELECT app FROM sample_app_totals WHERE id=1").get().app).toBe('bms-watchdog');
    expect(db.query("SELECT app FROM sample_group_totals WHERE id=1").get().app).toBe('bms-watchdog');
    db.close();
    await rm(dir, { recursive: true, force: true });
  });

  test('install systemctl-unit supports dry-run temp unit dir', async () => {
    const dir = await mkdtemp(join(tmpdir(), 'bms-watchdog-install-'));
    const unitDir = join(dir, 'systemd-user');
    const dataDir = join(dir, 'data');
    const result = Bun.spawnSync([
      bin, 'install', 'systemctl-unit',
      '--dry-run',
      '--unit-dir', unitDir,
      '--data-dir', dataDir,
      '--port', '25001',
      '--no-serve',
    ]);
    expect(result.exitCode).toBe(0);
    const unit = await Bun.file(join(unitDir, 'bms-watchdog.service')).text();
    expect(unit).toContain('ExecStart=');
    expect(unit).toContain(' start ');
    expect(unit).toContain('--no-serve');
    expect(unit).toContain('--port 25001');
    expect(unit).toContain('--retention-days 365');
    expect(unit).toContain('--detailed-process-retention-days 2');
    expect(unit).toContain('--agent-retention-days 365');
    expect(unit).toContain('--context-retention-days 365');
    expect(unit).toContain('--pi-sessions-dir');
    expect(unit).toContain('--baseline-mode adaptive');
    expect(unit).toContain('--suspend-gap-seconds 120');
    await rm(dir, { recursive: true, force: true });
  });
});

type Fixture = { dir: string; proc: string; power: string; config: string; data: string; focused: string; presence:string };

async function makeFixture(): Promise<Fixture> {
  const dir = await mkdtemp(join(tmpdir(), 'bms-watchdog-e2e-'));
  const proc = join(dir, 'proc');
  const sysClass = join(dir, 'sys/class');
  const power = join(sysClass, 'power_supply');
  const config = join(dir, 'config');
  const data = join(dir, 'data');
  const focused = join(dir, 'focused-window.json');
  const presence = join(dir,'presence');
  await mkdir(join(proc, '4242'), { recursive: true });
  await mkdir(join(proc, 'net'), { recursive: true });
  await mkdir(join(proc, 'asound'), { recursive: true });
  await mkdir(join(power, 'BAT0'), { recursive: true });
  await mkdir(join(power, 'AC'), { recursive: true });
  await mkdir(join(sysClass, 'backlight/intel_backlight'), { recursive: true });
  await mkdir(join(sysClass, 'typec/port0'), { recursive: true });
  await mkdir(join(config, 'gtk-3.0'), { recursive: true });
  await mkdir(data, { recursive: true });
  await mkdir(presence,{recursive:true,mode:0o700});
  await chmod(presence,0o700);

  await writeFile(join(proc, 'stat'), 'cpu  1000 0 1000 100000 0 0 0 0 0 0\n');
  await writeFile(join(proc, 'net/dev'), 'Inter-|   Receive                                                |  Transmit\n face |bytes packets errs drop fifo frame compressed multicast|bytes packets errs drop fifo colls carrier compressed\n  eth0: 1000000 0 0 0 0 0 0 0 2000000 0 0 0 0 0 0 0\n');
  await writeFile(join(proc, '4242/stat'), '4242 (bms-watchdog) S 1 0 0 0 0 0 0 0 0 0 100 50 0 0 20 0 1 0 12345 1000000 100\n');
  await writeFile(join(proc, '4242/comm'), 'bms-watchdog\n');
  await writeFile(join(proc, '4242/cmdline'), `bms-watchdog\0start\0`);
  await writeFile(join(proc, '4242/io'), 'read_bytes: 1024\nwrite_bytes: 2048\n');

  await writeFile(join(power, 'BAT0/type'), 'Battery\n');
  await writeFile(join(power, 'BAT0/status'), 'Discharging\n');
  await writeFile(join(power, 'BAT0/capacity'), '80\n');
  await writeFile(join(power, 'BAT0/energy_now'), '40000000\n');
  await writeFile(join(power, 'BAT0/energy_full'), '50000000\n');
  await writeFile(join(power, 'BAT0/power_now'), '8000000\n');
  await writeFile(join(power, 'AC/online'), '0\n');
  await mkdir(join(power, 'USB-C'), { recursive: true });
  await writeFile(join(power, 'USB-C/type'), 'USB\n');
  await writeFile(join(power, 'USB-C/power_now'), '5000000\n');
  await writeFile(join(sysClass, 'typec/port0/power_role'), 'source\n');

  await writeFile(join(sysClass, 'backlight/intel_backlight/brightness'), '50\n');
  await writeFile(join(sysClass, 'backlight/intel_backlight/max_brightness'), '100\n');
  await writeFile(join(config, 'gtk-3.0/settings.ini'), 'gtk-application-prefer-dark-theme=1\n');
  await writeFile(focused, JSON.stringify({ app_id: 'org.example.Editor', title: 'Fixture workspace', pid: 4242 }));

  return { dir, proc, power, config, data, focused, presence };
}

function fixtureArgs(fx: Fixture): string[] {
  return [
    '--data-dir', fx.data,
    '--proc-root', fx.proc,
    '--power-root', fx.power,
    '--host-config-dir', fx.config,
    '--focused-window-file', fx.focused,
    '--agent-presence-dir',fx.presence,
    '--force-collect',
    '--self-pid', '4242',
    '--no-agent-tracking',
  ];
}

function assistantEntry(responseId: string, id: string, timestamp: number, totalTokens: number, totalCost: number) {
  return {
    type: 'message', id, timestamp: new Date(timestamp).toISOString(),
    message: {
      role: 'assistant', content: [{ type: 'text', text: `private-${responseId}` }],
      provider: 'fixture-provider', api: 'fixture-api', model: 'fixture-model', responseId,
      usage: { input: 10, output: 20, reasoning: 5, cacheRead: totalTokens - 30, cacheWrite: 0, totalTokens, cost: { input: totalCost / 2, output: totalCost / 2, cacheRead: 0, cacheWrite: 0, total: totalCost } },
      stopReason: 'stop',
    },
  };
}

async function waitForAgentUsage(url: string, responses: number): Promise<any> {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const value = await fetch(url).then(response => response.json()).catch(() => null);
    if (value?.summary?.responses === responses && value?.agents?.[0]?.indexer?.initialScanComplete) return value;
    await Bun.sleep(100);
  }
  throw new Error('timed out waiting for agent telemetry indexing');
}

async function createOldDbFixture(path: string) {
  const db = new Database(path, { create: true });
  db.exec(await Bun.file(join(root, 'migrations/001_initial.sql')).text());
  const ts = Date.now();
  db.run("INSERT INTO battery_samples (id,ts,on_battery,status,capacity,energy_wh,power_w,source) VALUES (1,?,1,'Discharging',80,40,8,'BAT0')", [ts]);
  db.run("INSERT INTO process_identities (id,app,name,cmd) VALUES (1,'battery-monitor','battery-monitor','battery-monitor start')");
  db.run("INSERT INTO process_samples_v2 (id,sample_id,ts,pid,ppid,process_id,cpu_percent,cpu_seconds,io_mb,rss_mb,score,estimated_watts,is_self) VALUES (1,1,?,4242,1,1,1,0.1,0.01,10,0.1,0.5,1)", [ts]);
  db.run("INSERT INTO sample_app_totals (id,sample_id,ts,app,watts) VALUES (1,1,?,'battery-monitor',0.5)", [ts]);
  db.run("INSERT INTO sample_process_totals (id,sample_id,ts,process_id,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count) VALUES (1,1,?,1,0.5,0.1,0.01,10,1,1)", [ts]);
  db.run("INSERT INTO sample_group_totals (id,sample_id,ts,app,child_name,cmd,watts,cpu_seconds,io_mb,rss_mb_sum,rss_rows,row_count) VALUES (1,1,?,'battery-monitor','battery-monitor','battery-monitor start',0.5,0.1,0.01,10,1,1)", [ts]);
  db.run("INSERT INTO environment_samples (id,sample_id,ts,theme,theme_detail,brightness_percent,brightness_source,audio_playing,audio_detail,video_streaming,video_detail,net_rx_mbps,net_tx_mbps,focused_app,focused_title,focused_pid,lid_closed,lid_detail,screen_locked,screen_lock_detail,fan_rpm,fan_source) VALUES (1,1,?,'dark','fixture',50,'fixture',0,'fixture',0,'fixture',0,0,'','','',0,'fixture',0,'fixture',NULL,'fixture')", [ts]);
  db.close();
}

async function measureSingleRun(fx: Fixture): Promise<{ exitCode: number; wallSeconds: number; cpuSeconds: number; maxRssKb: number }> {
  const child = Bun.spawn([bin, 'start', '--once', ...fixtureArgs(fx)], {
    stdout: 'ignore',
    stderr: 'ignore',
  });
  const status = new Int32Array(1);
  const usage = new Uint8Array(256);
  const started = performance.now();
  const waitedPid = libc.symbols.wait4(child.pid, ptr(status), 0, ptr(usage));
  const wallSeconds = (performance.now() - started) / 1000;
  expect(waitedPid).toBe(child.pid);

  const view = new DataView(usage.buffer);
  const userSeconds = timevalSeconds(view, 0);
  const systemSeconds = timevalSeconds(view, 16);
  return {
    exitCode: exitCodeFromWaitStatus(status[0]),
    wallSeconds,
    cpuSeconds: userSeconds + systemSeconds,
    maxRssKb: Number(view.getBigInt64(32, true)),
  };
}

function timevalSeconds(view: DataView, offset: number): number {
  const seconds = Number(view.getBigInt64(offset, true));
  const micros = Number(view.getBigInt64(offset + 8, true));
  return seconds + micros / 1_000_000;
}

function exitCodeFromWaitStatus(status: number): number {
  return (status & 0x7f) === 0 ? (status >> 8) & 0xff : 128 + (status & 0x7f);
}

function ruleFixture() { return { name:'Fixture editor', priority:10, enabled:true, conditions:[{fact:'app.id',operator:'exact',value:'org.example.editor'}], outputs:[{dimension:'activity.primary',value:'work',confidence:1}], stopDimensions:['activity.primary'] }; }
async function postJson(url:string,body:any,port:number):Promise<any>{const res=await fetch(url,{method:'POST',headers:{'content-type':'application/json','origin':`http://127.0.0.1:${port}`,'x-watchdog-csrf':'1'},body:JSON.stringify(body)});const payload=await res.json();expect(res.ok,JSON.stringify(payload)).toBe(true);return payload;}
async function retryPostJson(url:string,body:any,port:number):Promise<any>{let last:any;for(let attempt=0;attempt<20;attempt++){const res=await fetch(url,{method:'POST',headers:{'content-type':'application/json','origin':`http://127.0.0.1:${port}`,'x-watchdog-csrf':'1'},body:JSON.stringify(body)});last=await res.json();if(res.ok)return last;if(!String(last?.error||'').includes('locked'))break;await Bun.sleep(50);}throw new Error(JSON.stringify(last));}

async function jsonFetch(url: string): Promise<any> {
  const res = await fetch(url);
  const payload = await res.json();
  expect(res.ok, JSON.stringify(payload)).toBe(true);
  return payload;
}

async function waitFor(url: string) {
  const deadline = Date.now() + 5000;
  let lastErr: unknown;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(url);
      if (res.ok) return;
    } catch (err) {
      lastErr = err;
    }
    await Bun.sleep(100);
  }
  throw lastErr || new Error(`timed out waiting for ${url}`);
}

function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.listen(0, '127.0.0.1', () => {
      const address = srv.address();
      srv.close(() => resolve(typeof address === 'object' && address ? address.port : 0));
    });
    srv.on('error', reject);
  });
}
