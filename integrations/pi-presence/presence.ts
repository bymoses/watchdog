import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import { randomUUID } from "node:crypto";
import { constants as fsConstants } from "node:fs";
import { chmod, mkdir, open, readFile, rename, unlink } from "node:fs/promises";
import { join } from "node:path";

export const SCHEMA_VERSION = 1;
export const HEARTBEAT_MS = 15_000;
export const LEASE_TTL_MS = 45_000;
const DIAGNOSTIC_INTERVAL_MS = 5 * 60_000;

export type PresenceState = "idle" | "busy" | "settling" | "closed";

export interface PresenceLease {
  schemaVersion: 1;
  eventId: string;
  harness: "pi";
  sourceKey: "local";
  instanceId: string;
  pid: number;
  processStartIdentity: string;
  nativeSessionId: string;
  cwd: string;
  state: PresenceState;
  observedAt: number;
  expiresAt: number;
}

export interface LeaseFileOps {
  now(): number;
  runtimeRoot(): string | undefined;
  processStartIdentity(pid: number, instanceId: string): Promise<string>;
  ensurePrivateDirectory(path: string): Promise<void>;
  writeAtomic(path: string, payload: string): Promise<void>;
  remove(path: string): Promise<void>;
  setInterval(callback: () => void, milliseconds: number): ReturnType<typeof setInterval>;
  clearInterval(timer: ReturnType<typeof setInterval>): void;
  diagnostic(message: string): void;
}

export class NodeLeaseFileOps implements LeaseFileOps {
  now(): number {
    return Date.now();
  }

  runtimeRoot(): string | undefined {
    const configured = process.env.XDG_RUNTIME_DIR;
    if (configured?.startsWith("/")) return configured;
    if (typeof process.getuid === "function") return `/run/user/${process.getuid()}`;
    return undefined;
  }

  async processStartIdentity(pid: number, instanceId: string): Promise<string> {
    try {
      const stat = await readFile(`/proc/${pid}/stat`, "utf8");
      const close = stat.lastIndexOf(")");
      if (close < 0) throw new Error("invalid proc stat");
      const fields = stat.slice(close + 1).trim().split(/\s+/);
      const startTicks = fields[19];
      if (!startTicks || !/^\d+$/.test(startTicks)) throw new Error("missing start ticks");
      return `linux:${startTicks}`;
    } catch {
      // Non-Linux systems cannot provide kernel start ticks. The random process
      // instance still prevents two concurrently written leases from colliding.
      return `instance:${instanceId}`;
    }
  }

  async ensurePrivateDirectory(path: string): Promise<void> {
    await mkdir(path, { recursive: true, mode: 0o700 });
    await chmod(path, 0o700);
  }

  async writeAtomic(path: string, payload: string): Promise<void> {
    const temporary = `${path}.tmp-${randomUUID()}`;
    let handle: Awaited<ReturnType<typeof open>> | undefined;
    try {
      handle = await open(
        temporary,
        fsConstants.O_CREAT | fsConstants.O_EXCL | fsConstants.O_WRONLY,
        0o600,
      );
      await handle.writeFile(payload, "utf8");
      await handle.sync();
      await handle.close();
      handle = undefined;
      await rename(temporary, path);
      await chmod(path, 0o600);
    } catch (error) {
      await handle?.close().catch(() => undefined);
      await unlink(temporary).catch(() => undefined);
      throw error;
    }
  }

  async remove(path: string): Promise<void> {
    await unlink(path).catch((error: unknown) => {
      if ((error as { code?: string }).code !== "ENOENT") throw error;
    });
  }

  setInterval(callback: () => void, milliseconds: number): ReturnType<typeof setInterval> {
    const timer = globalThis.setInterval(callback, milliseconds);
    (timer as { unref?: () => void }).unref?.();
    return timer;
  }

  clearInterval(timer: ReturnType<typeof setInterval>): void {
    globalThis.clearInterval(timer);
  }

  diagnostic(message: string): void {
    console.error(message);
  }
}

type PresenceContext = Pick<ExtensionContext, "cwd" | "sessionManager">;

export class PresenceLeaseWriter {
  private readonly ops: LeaseFileOps;
  private readonly instanceId = randomUUID();
  private state: PresenceState = "idle";
  private leasePath: string | undefined;
  private processIdentity: string | undefined;
  private heartbeat: ReturnType<typeof setInterval> | undefined;
  private context: PresenceContext | undefined;
  private active = false;
  private writeQueue: Promise<void> = Promise.resolve();
  private lastDiagnosticAt = Number.NEGATIVE_INFINITY;

  constructor(ops: LeaseFileOps = new NodeLeaseFileOps()) {
    this.ops = ops;
  }

  async start(ctx: PresenceContext): Promise<void> {
    await this.stop();
    const runtimeRoot = this.ops.runtimeRoot();
    if (!runtimeRoot) {
      this.rateLimitedDiagnostic("secure user runtime directory is unavailable");
      return;
    }
    const directory = join(runtimeRoot, "bms-watchdog", "agent-presence");
    try {
      await this.ops.ensurePrivateDirectory(directory);
      this.processIdentity = await this.ops.processStartIdentity(process.pid, this.instanceId);
      this.leasePath = join(directory, `pi-${process.pid}-${this.instanceId}.json`);
      this.context = ctx;
      this.state = "idle";
      this.active = true;
      await this.enqueueWrite();
      this.heartbeat = this.ops.setInterval(() => {
        void this.enqueueWrite();
      }, HEARTBEAT_MS);
    } catch {
      this.active = false;
      await this.removeLease();
      this.rateLimitedDiagnostic("presence lease initialization failed");
    }
  }

  async setState(state: Extract<PresenceState, "idle" | "busy">, ctx: PresenceContext): Promise<void> {
    if (!this.active) return;
    this.context = ctx;
    this.state = state;
    await this.enqueueWrite();
  }

  async refresh(ctx: PresenceContext): Promise<void> {
    if (!this.active) return;
    this.context = ctx;
    await this.enqueueWrite();
  }

  async stop(): Promise<void> {
    this.active = false;
    if (this.heartbeat) {
      this.ops.clearInterval(this.heartbeat);
      this.heartbeat = undefined;
    }
    await this.writeQueue.catch(() => undefined);
    await this.removeLease();
    this.context = undefined;
  }

  private enqueueWrite(): Promise<void> {
    this.writeQueue = this.writeQueue
      .catch(() => undefined)
      .then(async () => {
        if (!this.active || !this.leasePath || !this.processIdentity || !this.context) return;
        const observedAt = this.ops.now();
        const lease: PresenceLease = {
          schemaVersion: SCHEMA_VERSION,
          eventId: randomUUID(),
          harness: "pi",
          sourceKey: "local",
          instanceId: this.instanceId,
          pid: process.pid,
          processStartIdentity: this.processIdentity,
          nativeSessionId: this.context.sessionManager.getSessionId(),
          cwd: this.context.sessionManager.getCwd() || this.context.cwd,
          state: this.state,
          observedAt,
          expiresAt: observedAt + LEASE_TTL_MS,
        };
        try {
          await this.ops.writeAtomic(this.leasePath, `${JSON.stringify(lease)}\n`);
        } catch {
          this.rateLimitedDiagnostic("presence lease write failed");
        }
      });
    return this.writeQueue;
  }

  private async removeLease(): Promise<void> {
    if (!this.leasePath) return;
    const path = this.leasePath;
    this.leasePath = undefined;
    try {
      await this.ops.remove(path);
    } catch {
      this.rateLimitedDiagnostic("presence lease cleanup failed");
    }
  }

  private rateLimitedDiagnostic(category: string): void {
    const now = this.ops.now();
    if (now - this.lastDiagnosticAt < DIAGNOSTIC_INTERVAL_MS) return;
    this.lastDiagnosticAt = now;
    // Do not include paths, session IDs, payloads, prompts, or tool data.
    this.ops.diagnostic(`[bms-watchdog pi-presence] ${category}`);
  }
}
