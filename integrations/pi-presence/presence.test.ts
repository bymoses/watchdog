import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  HEARTBEAT_MS,
  NodeLeaseFileOps,
  PresenceLeaseWriter,
  type LeaseFileOps,
  type PresenceLease,
} from "./presence.ts";

class FakeOps implements LeaseFileOps {
  time = 1_000;
  root = "/runtime/user";
  writes: Array<{ path: string; lease: PresenceLease }> = [];
  removed: string[] = [];
  directories: string[] = [];
  diagnostics: string[] = [];
  callback: (() => void) | undefined;
  interval: number | undefined;
  failWrites = false;

  now(): number {
    return this.time;
  }
  runtimeRoot(): string | undefined {
    return this.root;
  }
  async processStartIdentity(): Promise<string> {
    return "linux:123";
  }
  async ensurePrivateDirectory(path: string): Promise<void> {
    this.directories.push(path);
  }
  async writeAtomic(path: string, payload: string): Promise<void> {
    if (this.failWrites) throw new Error("fixture failure with sensitive detail");
    this.writes.push({ path, lease: JSON.parse(payload) as PresenceLease });
  }
  async remove(path: string): Promise<void> {
    this.removed.push(path);
  }
  setInterval(callback: () => void, milliseconds: number): ReturnType<typeof setInterval> {
    this.callback = callback;
    this.interval = milliseconds;
    return 1 as unknown as ReturnType<typeof setInterval>;
  }
  clearInterval(): void {
    this.callback = undefined;
  }
  diagnostic(message: string): void {
    this.diagnostics.push(message);
  }
}

function context(sessionId: string, cwd = "/workspace/project") {
  return {
    cwd,
    sessionManager: {
      getSessionId: () => sessionId,
      getCwd: () => cwd,
    },
  } as never;
}

describe("Pi presence lifecycle", () => {
  test("writes exact idle/busy/current-session leases and cleans up", async () => {
    const ops = new FakeOps();
    const writer = new PresenceLeaseWriter(ops);
    await writer.start(context("session-one"));

    expect(ops.directories).toEqual(["/runtime/user/bms-watchdog/agent-presence"]);
    expect(ops.interval).toBe(HEARTBEAT_MS);
    expect(ops.writes).toHaveLength(1);
    const initial = ops.writes[0]!.lease;
    expect(initial.schemaVersion).toBe(1);
    expect(initial.harness).toBe("pi");
    expect(initial.sourceKey).toBe("local");
    expect(initial.state).toBe("idle");
    expect(initial.nativeSessionId).toBe("session-one");
    expect(initial.processStartIdentity).toBe("linux:123");
    expect(initial.expiresAt - initial.observedAt).toBe(45_000);

    ops.time = 2_000;
    await writer.setState("busy", context("session-two", "/workspace/two"));
    expect(ops.writes.at(-1)!.lease.state).toBe("busy");
    expect(ops.writes.at(-1)!.lease.nativeSessionId).toBe("session-two");
    expect(ops.writes.at(-1)!.lease.cwd).toBe("/workspace/two");

    ops.time = 3_000;
    ops.callback?.();
    await Promise.resolve();
    await Promise.resolve();
    expect(ops.writes.at(-1)!.lease.observedAt).toBe(3_000);

    await writer.setState("idle", context("session-two", "/workspace/two"));
    expect(ops.writes.at(-1)!.lease.state).toBe("idle");
    await writer.stop();
    expect(ops.removed).toHaveLength(1);
  });

  test("lease has an allowlisted schema and no conversation or tool fields", async () => {
    const ops = new FakeOps();
    const writer = new PresenceLeaseWriter(ops);
    await writer.start(context("native-id"));
    expect(Object.keys(ops.writes[0]!.lease).sort()).toEqual([
      "cwd",
      "eventId",
      "expiresAt",
      "harness",
      "instanceId",
      "nativeSessionId",
      "observedAt",
      "pid",
      "processStartIdentity",
      "schemaVersion",
      "sourceKey",
      "state",
    ]);
    const serialized = JSON.stringify(ops.writes[0]!.lease);
    for (const forbidden of ["prompt", "content", "message", "tool", "arguments", "result"]) {
      expect(serialized.toLowerCase()).not.toContain(`\"${forbidden}`);
    }
    await writer.stop();
  });

  test("write failures are non-fatal, sanitized, and rate limited", async () => {
    const ops = new FakeOps();
    ops.failWrites = true;
    const writer = new PresenceLeaseWriter(ops);
    await writer.start(context("secret-native-id", "/secret/path"));
    await writer.setState("busy", context("secret-native-id", "/secret/path"));
    expect(ops.diagnostics).toHaveLength(1);
    expect(ops.diagnostics[0]).not.toContain("secret");
    ops.time += 5 * 60_000;
    await writer.setState("idle", context("secret-native-id", "/secret/path"));
    expect(ops.diagnostics).toHaveLength(2);
    await writer.stop();
  });
});

describe("atomic file operations", () => {
  test("creates a 0700 directory and a 0600 complete lease", async () => {
    const root = await mkdtemp(join(tmpdir(), "pi-presence-test-"));
    const ops = new NodeLeaseFileOps();
    const directory = join(root, "presence");
    const file = join(directory, "lease.json");
    await ops.ensurePrivateDirectory(directory);
    await ops.writeAtomic(file, '{"complete":true}\n');
    expect((await stat(directory)).mode & 0o777).toBe(0o700);
    expect((await stat(file)).mode & 0o777).toBe(0o600);
    expect(await readFile(file, "utf8")).toBe('{"complete":true}\n');
  });
});
