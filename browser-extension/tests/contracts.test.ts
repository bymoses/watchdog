import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { serializeConfigRequest, serializeEventBatch, serializePairRequest } from "../src/transport";
import { DEFAULT_ENDPOINT, SCHEMA_VERSION } from "../src/types";
import type { BrowserEvent, Policy, PolicyDecision } from "../src/types";
import { PolicyEngine } from "../src/policy";
import { MemoryStorage } from "./helpers";

const contracts = join(import.meta.dir, "..", "..", "contracts", "browser", "v1");
const fixture = <T>(name: string): Promise<T> => Bun.file(join(contracts, name)).json() as Promise<T>;

describe("canonical browser v1 fixtures", () => {
  test("production pair serializer exactly matches the shared fixture", async () => {
    const expected = await fixture<any>("pair-request.json");
    expect(serializePairRequest({
      pairingCode: expected.pairingCode,
      clientId: expected.clientId,
      displayName: expected.displayName,
      browserFamily: expected.browserFamily,
      extensionVersion: expected.extensionVersion,
      privacyMode: expected.privacyMode,
      selectedMediaOrigins: expected.selectedMediaOrigins,
    })).toEqual(expected);
  });

  test("production event batch serializer exactly matches the shared fixture", async () => {
    const expected = await fixture<any>("event-batch.json") as { schemaVersion: 1; queueHealth: any; events: BrowserEvent[] };
    expect(serializeEventBatch(expected.events, (expected as any).queueHealth)).toEqual(expected);
    expect(expected.schemaVersion).toBe(SCHEMA_VERSION);
  });

  test("production config serializer matches selected origins and queue-health fixture", async () => {
    const expected = await fixture<any>("config-request.json");
    expect(serializeConfigRequest(expected.clientId, {
      endpoint: DEFAULT_ENDPOINT,
      displayName: "Fixture browser",
      includePrivate: false,
      failClosed: false,
      privacyMode: expected.privacyMode,
      selectedMediaOrigins: expected.selectedMediaOrigins,
    }, expected.queueHealth)).toEqual(expected);
    expect(() => serializeEventBatch([], { ...expected.queueHealth, depth: 1001 })).toThrow("Invalid queue health depth");
  });

  test("production policy cache consumes the shared server response fixture", async () => {
    const expected = await fixture<{ schemaVersion: 1; revision: number; serverTimestamp: number; policies: Policy[]; decisions: PolicyDecision[] }>("policy-state-response.json");
    const engine = new PolicyEngine(new MemoryStorage());
    await engine.cachePolicies(expected.revision, expected.policies, expected.decisions, expected.serverTimestamp);
    const cached = await engine.snapshot();
    expect(cached.policies).toEqual(expected.policies);
    expect(cached.counters[String(expected.decisions[0]!.policyId)]?.seconds).toBe(expected.decisions[0]!.usedSeconds);
  });

  test("default endpoint and enums are owned by the shared contract", async () => {
    const contract = await fixture<any>("contract.json");
    expect(DEFAULT_ENDPOINT).toBe(contract.defaultEndpoint);
    expect(contract.alarmPeriodMinutes).toEqual({ firefoxMv2: 0.5, chromiumMv3: 0.5 });
    expect(contract.mediaStates).toContain("seeking");
    expect(contract.actions).toEqual(["allow", "observe", "warn", "soft-block", "hard-block", "continue", "snooze", "override"]);
  });
});
