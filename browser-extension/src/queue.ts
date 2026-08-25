import type { BrowserEvent } from "./types";
import type { StorageArea } from "./webext";

export interface QueueItem {
  event: BrowserEvent;
  attempts: number;
  nextAttemptAt: number;
}

export interface QueueStatus {
  depth: number;
  bytes: number;
  oldestTimestamp?: number;
  lastSuccessfulSync?: number;
  dropped: number;
  permanentRejected: number;
  recoverySnapshots: number;
  lastDataLoss?: number;
}

interface QueueRecord {
  items: QueueItem[];
  lastSuccessfulSync?: number;
  dropped: number;
  permanentRejected: number;
  recoverySnapshots: number;
  lastDataLoss?: number;
}

export interface QueueOptions {
  key?: string;
  maxItems?: number;
  maxBytes?: number;
  maxAgeMs?: number;
  random?: () => number;
}

const DEFAULT_RECORD: QueueRecord = { items: [], dropped: 0, permanentRejected: 0, recoverySnapshots: 0 };

export class DurableEventQueue {
  private readonly key: string;
  private readonly maxItems: number;
  private readonly maxBytes: number;
  private readonly maxAgeMs: number;
  private readonly random: () => number;
  private operation: Promise<unknown> = Promise.resolve();

  constructor(private readonly storage: StorageArea, options: QueueOptions = {}) {
    this.key = options.key ?? "eventQueueV1";
    this.maxItems = options.maxItems ?? 1_000;
    this.maxBytes = options.maxBytes ?? 2 * 1024 * 1024;
    this.maxAgeMs = options.maxAgeMs ?? 24 * 60 * 60 * 1_000;
    this.random = options.random ?? Math.random;
  }

  async enqueue(events: BrowserEvent | BrowserEvent[], now = Date.now()): Promise<QueueStatus> {
    return this.exclusive(async () => {
      const record = await this.load();
      const known = new Set(record.items.map((item) => item.event.eventKey));
      for (const event of Array.isArray(events) ? events : [events]) {
        if (!known.has(event.eventKey)) {
          record.items.push({ event, attempts: 0, nextAttemptAt: now });
          known.add(event.eventKey);
        }
      }
      this.compact(record, now);
      await this.save(record);
      return statusOf(record);
    });
  }

  async peekBatch(now = Date.now(), maxEvents = 100, maxBytes = 128 * 1024): Promise<QueueItem[]> {
    return this.exclusive(async () => {
      const record = await this.load();
      this.compact(record, now);
      await this.save(record);
      const first = record.items[0];
      if (!first || first.nextAttemptAt > now) return [];
      const batch: QueueItem[] = [];
      let bytes = 2;
      for (const item of record.items) {
        if (item.nextAttemptAt > now) break;
        const itemBytes = byteLength(item.event) + 1;
        if (batch.length && (batch.length >= maxEvents || bytes + itemBytes > maxBytes)) break;
        if (!batch.length && itemBytes > maxBytes) {
          batch.push(item);
          break;
        }
        batch.push(item);
        bytes += itemBytes;
      }
      return batch;
    });
  }

  async acknowledge(eventKeys: string[], now = Date.now()): Promise<QueueStatus> {
    return this.exclusive(async () => {
      const record = await this.load();
      const accepted = new Set(eventKeys);
      record.items = record.items.filter((item) => !accepted.has(item.event.eventKey));
      record.lastSuccessfulSync = now;
      await this.save(record);
      return statusOf(record);
    });
  }

  async discardPermanent(eventKeys: string[], now = Date.now()): Promise<QueueStatus> {
    return this.exclusive(async () => {
      const record = await this.load();
      const rejected = new Set(eventKeys);
      const removed = record.items.filter((item) => rejected.has(item.event.eventKey));
      record.items = record.items.filter((item) => !rejected.has(item.event.eventKey));
      record.permanentRejected += removed.length;
      record.dropped += removed.length;
      if (removed.length) record.lastDataLoss = now;
      this.promoteRecoverySnapshots(record, removed);
      await this.save(record);
      return statusOf(record);
    });
  }

  async fail(eventKeys: string[], now = Date.now()): Promise<number> {
    return this.exclusive(async () => {
      const record = await this.load();
      const failed = new Set(eventKeys);
      let next = Number.POSITIVE_INFINITY;
      for (const item of record.items) {
        if (!failed.has(item.event.eventKey)) continue;
        item.attempts += 1;
        const base = Math.min(15 * 60_000, 1_000 * 2 ** Math.min(item.attempts - 1, 10));
        const jitter = 0.75 + this.random() * 0.5;
        item.nextAttemptAt = now + Math.round(base * jitter);
        next = Math.min(next, item.nextAttemptAt);
      }
      await this.save(record);
      return next;
    });
  }

  async clear(): Promise<void> {
    await this.exclusive(() => this.storage.remove(this.key));
  }

  async status(now = Date.now()): Promise<QueueStatus> {
    return this.exclusive(async () => {
      const record = await this.load();
      this.compact(record, now);
      await this.save(record);
      return statusOf(record);
    });
  }

  private compact(record: QueueRecord, now: number): void {
    const expired = record.items.filter((item) => now - item.event.eventTimestamp > this.maxAgeMs);
    record.items = record.items
      .filter((item) => now - item.event.eventTimestamp <= this.maxAgeMs)
      .sort((a, b) => a.event.clientSequence - b.event.clientSequence);
    record.dropped += expired.length;
    if (expired.length) record.lastDataLoss = now;

    const latestHeartbeat = new Map<string, QueueItem>();
    for (const item of record.items) {
      if (item.event.eventType === "heartbeat") {
        latestHeartbeat.set(`${item.event.windowId}:${item.event.tabId}`, item);
      }
    }
    const beforeHeartbeat = record.items.length;
    record.items = record.items.filter((item) =>
      item.event.eventType !== "heartbeat" || latestHeartbeat.get(`${item.event.windowId}:${item.event.tabId}`) === item
    );
    record.dropped += beforeHeartbeat - record.items.length;

    const pressureDropped: QueueItem[] = [];
    while (record.items.length > this.maxItems || byteLength(record.items) > this.maxBytes) {
      const heartbeatIndex = record.items.findIndex((item) => item.event.eventType === "heartbeat");
      const [removed] = record.items.splice(heartbeatIndex >= 0 ? heartbeatIndex : 0, 1);
      if (removed) pressureDropped.push(removed);
      record.dropped += 1;
    }
    if (pressureDropped.length) {
      record.lastDataLoss = now;
      this.promoteRecoverySnapshots(record, pressureDropped.filter((item) => item.event.eventType !== "heartbeat"));
    }
  }

  private promoteRecoverySnapshots(record: QueueRecord, dropped: QueueItem[]): void {
    const tabs = new Set(dropped.map((item) => `${item.event.windowId}:${item.event.tabId}`));
    for (const tab of tabs) {
      const latest = [...record.items].reverse().find((item) => `${item.event.windowId}:${item.event.tabId}` === tab && item.event.eventType !== "tab_close");
      if (latest && latest.event.eventType !== "snapshot") {
        latest.event = { ...latest.event, eventType: "snapshot" };
        record.recoverySnapshots += 1;
      }
    }
  }

  private async load(): Promise<QueueRecord> {
    const raw = (await this.storage.get(this.key))[this.key] as QueueRecord | undefined;
    if (!raw || !Array.isArray(raw.items)) return structuredClone(DEFAULT_RECORD);
    return {
      items: raw.items,
      dropped: raw.dropped ?? 0,
      permanentRejected: raw.permanentRejected ?? 0,
      recoverySnapshots: raw.recoverySnapshots ?? 0,
      ...(raw.lastSuccessfulSync ? { lastSuccessfulSync: raw.lastSuccessfulSync } : {}),
      ...(raw.lastDataLoss ? { lastDataLoss: raw.lastDataLoss } : {}),
    };
  }

  private save(record: QueueRecord): Promise<void> {
    return this.storage.set({ [this.key]: record });
  }

  private exclusive<T>(work: () => Promise<T>): Promise<T> {
    const next = this.operation.then(work, work);
    this.operation = next.catch(() => undefined);
    return next;
  }
}

function byteLength(value: unknown): number {
  return new TextEncoder().encode(JSON.stringify(value)).byteLength;
}

function statusOf(record: QueueRecord): QueueStatus {
  const first = record.items[0];
  return {
    depth: record.items.length,
    bytes: byteLength(record.items),
    ...(first ? { oldestTimestamp: first.event.eventTimestamp } : {}),
    ...(record.lastSuccessfulSync ? { lastSuccessfulSync: record.lastSuccessfulSync } : {}),
    dropped: record.dropped,
    permanentRejected: record.permanentRejected,
    recoverySnapshots: record.recoverySnapshots,
    ...(record.lastDataLoss ? { lastDataLoss: record.lastDataLoss } : {}),
  };
}
