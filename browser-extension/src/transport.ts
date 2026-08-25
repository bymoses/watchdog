import { assertNoFullUrls } from "./privacy";
import type { DurableEventQueue } from "./queue";
import { SCHEMA_VERSION } from "./types";
import type { BrowserEvent, ClientIdentity, EventBatch, ExtensionSettings, PolicyDecision, PrivacyMode, QueueHealth } from "./types";
import type { StorageArea } from "./webext";

export type Fetcher = (input: string, init?: RequestInit) => Promise<Response>;

export interface PairRequest {
  schemaVersion: typeof SCHEMA_VERSION;
  pairingCode: string;
  clientId: string;
  displayName: string;
  browserFamily: string;
  extensionVersion: string;
  privacyMode: PrivacyMode;
  selectedMediaOrigins: string[];
}
export interface PairResponse {
  schemaVersion: typeof SCHEMA_VERSION;
  clientId: string;
  token: string;
  pairedTimestamp: number;
  policyRevision: number;
}
export interface BatchResponse {
  schemaVersion: typeof SCHEMA_VERSION;
  accepted: number;
  duplicates: number;
  rejected: Array<{ index: number; code: string; reason: string; permanent: boolean }>;
  serverTimestamp: number;
  policyRevision: number;
  policyDecision?: PolicyDecision;
}

export function serializePairRequest(input: Omit<PairRequest, "schemaVersion">): PairRequest {
  return { schemaVersion: SCHEMA_VERSION, ...input };
}
export function serializeEventBatch(events: BrowserEvent[], queueHealth: QueueHealth = emptyQueueHealth()): EventBatch {
  return { schemaVersion: SCHEMA_VERSION, queueHealth: boundedQueueHealth(queueHealth), events };
}
export function serializeConfigRequest(clientId: string, settings: ExtensionSettings, queueHealth: QueueHealth) {
  return {
    schemaVersion: SCHEMA_VERSION,
    clientId,
    privacyMode: settings.privacyMode,
    selectedMediaOrigins: [...settings.selectedMediaOrigins],
    queueHealth: boundedQueueHealth(queueHealth),
  };
}

export class PairingStore {
  static readonly KEY = "clientIdentityV1";
  private operation: Promise<unknown> = Promise.resolve();
  constructor(private readonly storage: StorageArea) {}

  async get(): Promise<ClientIdentity | undefined> {
    return (await this.storage.get(PairingStore.KEY))[PairingStore.KEY] as ClientIdentity | undefined;
  }

  ensure(): Promise<ClientIdentity> {
    return this.exclusive(() => this.readOrCreate());
  }

  reserveSequence(): Promise<{ clientId: string; sequence: number }> {
    return this.exclusive(async () => {
      const identity = await this.readOrCreate();
      const sequence = identity.nextSequence;
      await this.storage.set({ [PairingStore.KEY]: { ...identity, nextSequence: sequence + 1 } });
      return { clientId: identity.clientId, sequence };
    });
  }

  setToken(response: Pick<PairResponse, "clientId" | "token">): Promise<ClientIdentity> {
    return this.exclusive(async () => {
      const existing = await this.readOrCreate();
      const identity: ClientIdentity = { clientId: response.clientId, nextSequence: existing.nextSequence, token: response.token };
      await this.storage.set({ [PairingStore.KEY]: identity });
      return identity;
    });
  }

  revokeLocal(): Promise<void> {
    return this.exclusive(async () => {
      const identity = await this.readOrCreate();
      await this.storage.set({ [PairingStore.KEY]: { clientId: crypto.randomUUID(), nextSequence: identity.nextSequence } });
    });
  }

  private async readOrCreate(): Promise<ClientIdentity> {
    const existing = await this.get();
    if (existing) return existing;
    const identity: ClientIdentity = { clientId: crypto.randomUUID(), nextSequence: 1 };
    await this.storage.set({ [PairingStore.KEY]: identity });
    return identity;
  }

  private exclusive<T>(work: () => Promise<T>): Promise<T> {
    const next = this.operation.then(work, work);
    this.operation = next.catch(() => undefined);
    return next;
  }
}

export class BrowserTransport {
  private flushing: Promise<BatchResponse | undefined> | undefined;

  constructor(
    private readonly settings: () => Promise<ExtensionSettings>,
    private readonly pairing: PairingStore,
    private readonly queue: DurableEventQueue,
    private readonly fetcher: Fetcher = (input, init) => fetch(input, init),
  ) {}

  async pair(code: string, browserFamily: string, extensionVersion: string): Promise<PairResponse> {
    if (!/^[A-Za-z0-9-]{4,128}$/.test(code)) throw new Error("Invalid pairing code format");
    const settings = await this.settings();
    const provisional = await this.pairing.ensure();
    const body = serializePairRequest({
      pairingCode: code,
      clientId: provisional.clientId,
      displayName: settings.displayName.slice(0, 80),
      browserFamily,
      extensionVersion,
      privacyMode: settings.privacyMode,
      selectedMediaOrigins: settings.selectedMediaOrigins,
    });
    const response = await this.fetchJson<PairResponse>(`${endpoint(settings)}/api/integrations/browser/v1/pair`, body);
    if (!response.clientId || !response.token) throw new Error("Pairing response missing credentials");
    await this.pairing.setToken(response);
    return response;
  }

  async enqueue(event: BrowserEvent): Promise<BatchResponse | undefined> {
    assertNoFullUrls(event);
    await this.queue.enqueue(event);
    return this.flush();
  }

  flush(now = Date.now()): Promise<BatchResponse | undefined> {
    if (this.flushing) return this.flushing;
    const operation = this.flushOnce(now);
    this.flushing = operation;
    void operation.finally(() => {
      if (this.flushing === operation) this.flushing = undefined;
    }).catch(() => undefined);
    return operation;
  }

  private async flushOnce(now: number): Promise<BatchResponse | undefined> {
    const identity = await this.pairing.get();
    if (!identity?.token) return undefined;
    const batch = await this.queue.peekBatch(now);
    if (!batch.length) return undefined;
    const events = batch.map((item) => item.event);
    assertNoFullUrls(events);
    try {
      const settings = await this.settings();
      const response = await this.fetchJson<BatchResponse>(
        `${endpoint(settings)}/api/integrations/browser/v1/events:batch`,
        serializeEventBatch(events, await this.queue.status(now)),
        identity.token,
      );
      const rejected = response.rejected ?? [];
      const permanent = new Set(rejected.filter((entry) => entry.permanent).map((entry) => entry.index));
      const retryable = new Set(rejected.filter((entry) => !entry.permanent).map((entry) => entry.index));
      const completed = events.filter((_, index) => !permanent.has(index) && !retryable.has(index)).map((event) => event.eventKey);
      await this.queue.acknowledge(completed, now);
      if (permanent.size) {
        await this.queue.discardPermanent(events.filter((_, index) => permanent.has(index)).map((event) => event.eventKey), now);
      }
      if (retryable.size) {
        await this.queue.fail(events.filter((_, index) => retryable.has(index)).map((event) => event.eventKey), now);
      }
      return response;
    } catch (error) {
      await this.queue.fail(events.map((event) => event.eventKey), now);
      throw error;
    }
  }

  async getConfig<T>(): Promise<T> {
    return this.authorizedGet<T>("config");
  }

  async updateConfig(): Promise<void> {
    const settings = await this.settings();
    const identity = await this.requireIdentity();
    await this.fetchJson(
      `${endpoint(settings)}/api/integrations/browser/v1/config`,
      serializeConfigRequest(identity.clientId, settings, await this.queue.status()),
      identity.token,
    );
  }

  async getPolicyState<T>(domain?: string): Promise<T> {
    return this.authorizedGet<T>(`policy-state${domain ? `?domain=${encodeURIComponent(domain)}` : ""}`);
  }

  async reportIntervention(body: Record<string, unknown>): Promise<void> {
    const settings = await this.settings();
    const identity = await this.requireIdentity();
    await this.fetchJson(`${endpoint(settings)}/api/integrations/browser/v1/interventions`, body, identity.token);
  }

  async revoke(): Promise<void> {
    const settings = await this.settings();
    const identity = await this.pairing.get();
    try {
      if (identity?.token) {
        const response = await this.fetcher(`${endpoint(settings)}/api/integrations/browser/v1/client`, {
          method: "DELETE",
          headers: headers(identity.token),
          body: JSON.stringify({ schemaVersion: SCHEMA_VERSION, clientId: identity.clientId }),
        });
        if (!response.ok && response.status !== 401 && response.status !== 404) throw new Error(`Revocation failed (${response.status})`);
      }
    } finally {
      await this.pairing.revokeLocal();
      await this.queue.clear();
    }
  }

  private async authorizedGet<T>(path: string): Promise<T> {
    const settings = await this.settings();
    const identity = await this.requireIdentity();
    const response = await this.fetcher(`${endpoint(settings)}/api/integrations/browser/v1/${path}`, { headers: headers(identity.token) });
    if (!response.ok) throw new Error(`Request failed (${response.status})`);
    return response.json() as Promise<T>;
  }

  private async requireIdentity(): Promise<Required<ClientIdentity>> {
    const identity = await this.pairing.get();
    if (!identity?.token) throw new Error("Extension is not paired");
    return identity as Required<ClientIdentity>;
  }

  private async fetchJson<T>(url: string, body: unknown, token?: string): Promise<T> {
    const response = await this.fetcher(url, { method: "POST", headers: headers(token), body: JSON.stringify(body) });
    if (!response.ok) throw new Error(`Request failed (${response.status})`);
    return response.json() as Promise<T>;
  }
}

function endpoint(settings: ExtensionSettings): string {
  const parsed = new URL(settings.endpoint);
  if (parsed.protocol !== "http:" || parsed.hostname !== "127.0.0.1") throw new Error("Endpoint must be loopback HTTP");
  return parsed.origin;
}

function emptyQueueHealth(): QueueHealth {
  return { depth: 0, bytes: 2, dropped: 0, permanentRejected: 0, recoverySnapshots: 0 };
}
function boundedQueueHealth(value: QueueHealth): QueueHealth {
  const integer = (input: number, min: number, max: number, field: string): number => {
    if (!Number.isSafeInteger(input) || input < min || input > max) throw new Error(`Invalid queue health ${field}`);
    return input;
  };
  const timestamp = (input: number | undefined, field: string): number | undefined => input === undefined ? undefined : integer(input, 0, Number.MAX_SAFE_INTEGER, field);
  return {
    depth: integer(value.depth, 0, 1_000, "depth"),
    bytes: integer(value.bytes, 0, 2 * 1024 * 1024, "bytes"),
    dropped: integer(value.dropped, 0, 2_147_483_647, "dropped"),
    permanentRejected: integer(value.permanentRejected, 0, 2_147_483_647, "permanentRejected"),
    recoverySnapshots: integer(value.recoverySnapshots, 0, 2_147_483_647, "recoverySnapshots"),
    ...(timestamp(value.oldestTimestamp, "oldestTimestamp") !== undefined ? { oldestTimestamp: value.oldestTimestamp } : {}),
    ...(timestamp(value.lastSuccessfulSync, "lastSuccessfulSync") !== undefined ? { lastSuccessfulSync: value.lastSuccessfulSync } : {}),
    ...(timestamp(value.lastDataLoss, "lastDataLoss") !== undefined ? { lastDataLoss: value.lastDataLoss } : {}),
  };
}

function headers(token?: string): Record<string, string> {
  return {
    "Content-Type": "application/json",
    ...(token ? { Authorization: `Bearer ${token}` } : {}),
  };
}
