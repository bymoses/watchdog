import { describe, expect, test } from "bun:test";
import { DurableEventQueue } from "../src/queue";
import { loadSettings, saveSettings, validateSettings } from "../src/settings";
import { BrowserTransport, PairingStore } from "../src/transport";
import { browserEvent, MemoryStorage, settings } from "./helpers";

describe("settings", () => {
  test("defaults to domains only and private exclusion", async () => {
    expect(await loadSettings(new MemoryStorage())).toMatchObject({ privacyMode: "domains-only", includePrivate: false });
  });
  test("allows only loopback HTTP endpoints", () => {
    expect(() => validateSettings(settings({ endpoint: "https://example.com" }))).toThrow();
    expect(() => validateSettings(settings({ endpoint: "http://localhost:9898" }))).toThrow();
  });
  test("normalizes selected origins and cannot enable private events", async () => {
    const storage = new MemoryStorage();
    const saved = await saveSettings(storage, settings({ includePrivate: true, selectedMediaOrigins: ["https://YouTube.com/watch?v=x", "https://youtube.com/other"] }));
    expect(saved).toMatchObject({ includePrivate: false, selectedMediaOrigins: ["https://youtube.com"] });
  });
});

describe("pairing transport", () => {
  test("reserves unique sequences under concurrent browser events", async () => {
    const pairing = new PairingStore(new MemoryStorage());
    const reservations = await Promise.all(Array.from({ length: 20 }, () => pairing.reserveSequence()));
    expect(reservations.map((item) => item.sequence).sort((a, b) => a - b)).toEqual(Array.from({ length: 20 }, (_, index) => index + 1));
  });
  test("stores returned token and never sends it in pairing body", async () => {
    const storage = new MemoryStorage();
    const pairing = new PairingStore(storage);
    const queue = new DurableEventQueue(storage);
    let request: RequestInit | undefined;
    const fetcher = async (_url: string, init?: RequestInit) => {
      request = init;
      return Response.json({ clientId: "server-client", token: "secret-token" });
    };
    const transport = new BrowserTransport(async () => settings(), pairing, queue, fetcher);
    await transport.pair("ABCD-1234", "firefox", "0.1.0");
    expect((await pairing.get())?.token).toBe("secret-token");
    expect(request?.headers).not.toHaveProperty("Authorization");
    expect(request?.body).not.toContain("secret-token");
  });
  test("sends ordered batches with bearer auth and acknowledges", async () => {
    const storage = new MemoryStorage();
    const pairing = new PairingStore(storage);
    await pairing.setToken({ clientId: "c", token: "token" });
    const queue = new DurableEventQueue(storage);
    await queue.enqueue([browserEvent({ eventKey: "a", clientSequence: 1 }), browserEvent({ eventKey: "b", clientSequence: 2 })], 1_000);
    let body: any;
    let authorization = "";
    const fetcher = (async (_url: string | URL | Request, init?: RequestInit) => {
      body = JSON.parse(String(init?.body));
      authorization = (init?.headers as Record<string, string>).Authorization ?? "";
      return Response.json({ accepted: 2, duplicates: 0, serverTimestamp: 2_000 });
    });
    const transport = new BrowserTransport(async () => settings(), pairing, queue, fetcher);
    await transport.flush(1_000);
    expect(body.schemaVersion).toBe(1);
    expect(body.events.map((event: any) => event.eventKey)).toEqual(["a", "b"]);
    expect(authorization).toBe("Bearer token");
    expect((await queue.status()).depth).toBe(0);
  });
  test("retries rejected indexes but removes accepted duplicates", async () => {
    const storage = new MemoryStorage();
    const pairing = new PairingStore(storage);
    await pairing.setToken({ clientId: "c", token: "token" });
    const queue = new DurableEventQueue(storage, { random: () => 0.5 });
    await queue.enqueue([browserEvent({ eventKey: "a", clientSequence: 1 }), browserEvent({ eventKey: "b", clientSequence: 2 })], 1_000);
    const fetcher = async () => Response.json({ accepted: 1, duplicates: 0, rejected: [{ index: 1, reason: "temporary" }], serverTimestamp: 2_000 });
    await new BrowserTransport(async () => settings(), pairing, queue, fetcher).flush(1_000);
    expect((await queue.status(1_000)).depth).toBe(1);
    expect(await queue.peekBatch(1_999)).toHaveLength(0);
    expect((await queue.peekBatch(2_000))[0]?.event.eventKey).toBe("b");
  });
  test("permanent event rejection advances the queue", async () => {
    const storage = new MemoryStorage();
    const pairing = new PairingStore(storage);
    await pairing.setToken({ clientId: "c", token: "token" });
    const queue = new DurableEventQueue(storage);
    await queue.enqueue([browserEvent({ eventKey: "poison", clientSequence: 1 }), browserEvent({ eventKey: "next", clientSequence: 2 })], 1_000);
    const fetcher = async () => Response.json({ schemaVersion: 1, accepted: 1, duplicates: 0, rejected: [{ index: 0, code: "invalid_event", reason: "invalid", permanent: true }], serverTimestamp: 2_000, policyRevision: 0 });
    await new BrowserTransport(async () => settings(), pairing, queue, fetcher).flush(1_000);
    expect((await queue.status()).depth).toBe(0);
    expect((await queue.status()).permanentRejected).toBe(1);
  });

  test("revocation clears token and queue even after server rejection", async () => {
    const storage = new MemoryStorage();
    const pairing = new PairingStore(storage);
    await pairing.setToken({ clientId: "c", token: "token" });
    const queue = new DurableEventQueue(storage);
    await queue.enqueue(browserEvent(), 1_000);
    const fetcher = async () => new Response("no", { status: 500 });
    await expect(new BrowserTransport(async () => settings(), pairing, queue, fetcher).revoke()).rejects.toThrow();
    expect((await pairing.get())?.token).toBeUndefined();
    expect((await queue.status()).depth).toBe(0);
  });
});
