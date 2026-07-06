import { afterEach, describe, expect, test } from 'bun:test';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import net from 'node:net';
import { dlopen, FFIType, ptr } from 'bun:ffi';
import { Database } from 'bun:sqlite';

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

    await rm(fx.dir, { recursive: true, force: true });
  });

  test('single collection run stays within startup/resource budget', async () => {
    const fx = await makeFixture();
    const metrics = await measureSingleRun(fx);
    expect(metrics.exitCode).toBe(0);
    expect(metrics.wallSeconds).toBeLessThanOrEqual(0.1);
    expect(metrics.cpuSeconds).toBeLessThanOrEqual(0.1);
    expect(metrics.maxRssKb).toBeLessThanOrEqual(80 * 1024);
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
    expect(unit).toContain('--baseline-mode adaptive');
    expect(unit).toContain('--suspend-gap-seconds 120');
    await rm(dir, { recursive: true, force: true });
  });
});

type Fixture = { dir: string; proc: string; power: string; config: string; data: string };

async function makeFixture(): Promise<Fixture> {
  const dir = await mkdtemp(join(tmpdir(), 'bms-watchdog-e2e-'));
  const proc = join(dir, 'proc');
  const sysClass = join(dir, 'sys/class');
  const power = join(sysClass, 'power_supply');
  const config = join(dir, 'config');
  const data = join(dir, 'data');
  await mkdir(join(proc, '4242'), { recursive: true });
  await mkdir(join(proc, 'net'), { recursive: true });
  await mkdir(join(proc, 'asound'), { recursive: true });
  await mkdir(join(power, 'BAT0'), { recursive: true });
  await mkdir(join(power, 'AC'), { recursive: true });
  await mkdir(join(sysClass, 'backlight/intel_backlight'), { recursive: true });
  await mkdir(join(sysClass, 'typec/port0'), { recursive: true });
  await mkdir(join(config, 'gtk-3.0'), { recursive: true });
  await mkdir(data, { recursive: true });

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

  return { dir, proc, power, config, data };
}

function fixtureArgs(fx: Fixture): string[] {
  return [
    '--data-dir', fx.data,
    '--proc-root', fx.proc,
    '--power-root', fx.power,
    '--host-config-dir', fx.config,
    '--force-collect',
    '--self-pid', '4242',
  ];
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

async function jsonFetch(url: string): Promise<any> {
  const res = await fetch(url);
  expect(res.ok).toBe(true);
  return res.json();
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
