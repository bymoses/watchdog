import { describe, expect, test } from "bun:test";
import { DurableEventQueue } from "../src/queue";
import { browserEvent, MemoryStorage } from "./helpers";

describe("durable bounded event queue", () => {
  test("survives reconstruction, deduplicates, and preserves sequence", async () => {
    const storage = new MemoryStorage();
    const queue = new DurableEventQueue(storage);
    const second = browserEvent({ eventKey: "b", clientSequence: 2 });
    const first = browserEvent({ eventKey: "a", clientSequence: 1 });
    await queue.enqueue([second, first, first], 1_000);
    const restored = new DurableEventQueue(storage);
    expect((await restored.peekBatch(1_000)).map((item) => item.event.eventKey)).toEqual(["a", "b"]);
  });
  test("acknowledges accepted keys and records sync", async () => {
    const queue = new DurableEventQueue(new MemoryStorage());
    await queue.enqueue([browserEvent({ eventKey: "a" }), browserEvent({ eventKey: "b", clientSequence: 2 })], 1_000);
    const status = await queue.acknowledge(["a"], 2_000);
    expect(status).toMatchObject({ depth: 1, lastSuccessfulSync: 2_000 });
  });
  test("uses exponential backoff with bounded jitter", async () => {
    const queue = new DurableEventQueue(new MemoryStorage(), { random: () => 0.5 });
    await queue.enqueue(browserEvent({ eventKey: "a" }), 1_000);
    expect(await queue.fail(["a"], 1_000)).toBe(2_000);
    expect(await queue.peekBatch(1_999)).toHaveLength(0);
    expect(await queue.peekBatch(2_000)).toHaveLength(1);
    expect(await queue.fail(["a"], 2_000)).toBe(4_000);
  });
  test("does not reorder around a delayed first item", async () => {
    const queue = new DurableEventQueue(new MemoryStorage(), { random: () => 0.5 });
    await queue.enqueue([browserEvent({ eventKey: "a", clientSequence: 1 }), browserEvent({ eventKey: "b", clientSequence: 2 })], 1_000);
    await queue.fail(["a"], 1_000);
    expect(await queue.peekBatch(1_500)).toEqual([]);
  });
  test("compacts redundant heartbeats before transitions", async () => {
    const queue = new DurableEventQueue(new MemoryStorage(), { maxItems: 3 });
    await queue.enqueue([
      browserEvent({ eventKey: "t", clientSequence: 1, eventType: "navigation" }),
      browserEvent({ eventKey: "h1", clientSequence: 2, eventType: "heartbeat" }),
      browserEvent({ eventKey: "h2", clientSequence: 3, eventType: "heartbeat" }),
      browserEvent({ eventKey: "m", clientSequence: 4, eventType: "media_play" }),
    ], 1_000);
    const keys = (await queue.peekBatch(1_000)).map((item) => item.event.eventKey);
    expect(keys).toEqual(["t", "h2", "m"]);
  });
  test("dropping a transition promotes the latest same-tab state to a recovery snapshot", async () => {
    const queue = new DurableEventQueue(new MemoryStorage(), { maxItems: 2 });
    await queue.enqueue([
      browserEvent({ eventKey: "old", clientSequence: 1, eventType: "navigation" }),
      browserEvent({ eventKey: "latest", clientSequence: 2, eventType: "media_play", mediaState: "playing" }),
      browserEvent({ eventKey: "other", clientSequence: 3, windowId: 9, tabId: 9 }),
    ], 1_000);
    const batch = await queue.peekBatch(1_000);
    expect(batch[0]?.event).toMatchObject({ eventKey: "latest", eventType: "snapshot" });
    expect((await queue.status()).recoverySnapshots).toBe(1);
  });

  test("expires events after the offline window and exposes data-loss health", async () => {
    const queue = new DurableEventQueue(new MemoryStorage(), { maxAgeMs: 100 });
    await queue.enqueue(browserEvent({ eventTimestamp: 1_000 }), 1_000);
    expect(await queue.status(1_101)).toMatchObject({ depth: 0, dropped: 1, lastDataLoss: 1_101 });
  });
  test("respects event and byte batch limits", async () => {
    const queue = new DurableEventQueue(new MemoryStorage());
    await queue.enqueue(Array.from({ length: 5 }, (_, i) => browserEvent({ eventKey: String(i), clientSequence: i + 1 })), 1_000);
    expect(await queue.peekBatch(1_000, 2)).toHaveLength(2);
  });
});
