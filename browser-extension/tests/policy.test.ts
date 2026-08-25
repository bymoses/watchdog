import { describe, expect, test } from "bun:test";
import { clipScheduledMilliseconds, metricActive, PolicyEngine, scheduleActive } from "../src/policy";
import type { Policy } from "../src/types";
import { browserEvent, MemoryStorage } from "./helpers";

const base: Policy = {
  id: 1, name: "video", revision: 1, enabled: true,
  scope: { domains: ["youtube.com"], excludedDomains: [] },
  metric: "focused_playback_seconds", windowKind: "calendar-day", budgetSeconds: 60,
  warningSeconds: 10, enforcement: "soft-block",
  schedule: { weekdays: [1, 2, 3, 4, 5, 6, 7], startMinute: 0, endMinute: 1440 },
  grace: { graceSeconds: 0, snoozeSeconds: 30, maxDailySnoozes: 1, sessionGapSeconds: 60, rollingSeconds: 3600 },
};

describe("policy counters and decisions", () => {
  test("focused playback excludes paused and background media", () => {
    expect(metricActive("focused_playback_seconds", browserEvent({ mediaState: "paused" }))).toBeFalse();
    expect(metricActive("focused_playback_seconds", browserEvent({ mediaState: "playing", browserFocused: false }))).toBeFalse();
    expect(metricActive("focused_playback_seconds", browserEvent({ mediaState: "seeking" }))).toBeTrue();
  });

  test("charges union elapsed time and caps offline gaps", async () => {
    const engine = new PolicyEngine(new MemoryStorage());
    await engine.cachePolicies(1, [base], [], 0);
    await engine.record(browserEvent({ eventTimestamp: 1_000, mediaState: "playing" }));
    expect((await engine.record(browserEvent({ eventTimestamp: 21_000, mediaState: "playing" })))[0]?.remainingSeconds).toBe(40);
    const decision = (await engine.record(browserEvent({ eventTimestamp: 121_000, mediaState: "playing" })))[0]!;
    expect(decision.usedSeconds).toBe(65);
    expect(decision.action).toBe("soft-block");
  });

  test("unrelated-domain event stops only that tab and concurrent tabs count once", async () => {
    const engine = new PolicyEngine(new MemoryStorage());
    await engine.cachePolicies(1, [base]);
    await engine.record(browserEvent({ windowId: 1, tabId: 1, eventTimestamp: 1_000, mediaState: "playing" }));
    await engine.record(browserEvent({ windowId: 2, tabId: 2, eventTimestamp: 6_000, mediaState: "playing" }));
    await engine.record(browserEvent({ windowId: 1, tabId: 1, eventTimestamp: 11_000, domain: "example.com", origin: "https://example.com", mediaState: "none" }));
    await engine.record(browserEvent({ windowId: 2, tabId: 2, eventTimestamp: 16_000, mediaState: "playing" }));
    expect((await engine.snapshot()).counters["1"]?.seconds).toBe(15);
  });

  test("tab close and switching away stop accumulation", async () => {
    const engine = new PolicyEngine(new MemoryStorage());
    await engine.cachePolicies(1, [base]);
    await engine.record(browserEvent({ eventTimestamp: 1_000, mediaState: "playing" }));
    await engine.record(browserEvent({ eventTimestamp: 11_000, eventType: "tab_close", active: false, browserFocused: false, documentVisible: false, mediaState: "none" }));
    await engine.record(browserEvent({ eventTimestamp: 21_000, domain: "example.com", origin: "https://example.com" }));
    expect((await engine.snapshot()).counters["1"]?.seconds).toBe(10);
  });

  test("clock corrections never double charge", async () => {
    const engine = new PolicyEngine(new MemoryStorage());
    await engine.cachePolicies(1, [base]);
    await engine.record(browserEvent({ eventTimestamp: 10_000, mediaState: "playing" }));
    await engine.record(browserEvent({ eventTimestamp: 5_000, mediaState: "playing" }));
    expect((await engine.snapshot()).counters["1"]?.seconds).toBe(0);
  });

  test("snooze is bounded, continue suppresses restart, and server reconciliation wins", async () => {
    const engine = new PolicyEngine(new MemoryStorage());
    await engine.cachePolicies(1, [{ ...base, budgetSeconds: 0 }]);
    expect((await engine.snooze(1, 1_000)).action).toBe("allow");
    await expect(engine.snooze(1, 1_001)).rejects.toThrow("No snoozes");
    await engine.cachePolicies(1, [{ ...base, budgetSeconds: 0 }], [{ policyId: 1, revision: 1, enforcement: "soft-block", action: "soft-block", usedSeconds: 30, budgetSeconds: 0, remainingSeconds: 0, degraded: false }], 2_000);
    expect((await engine.continue(1, 2_000)).action).toBe("allow");
    expect((await engine.snapshot()).counters["1"]?.seconds).toBe(30);
  });

  test("schedule clipping handles daytime, overnight, and absolute-minute DST segments", () => {
    const day = new Date(2026, 0, 15, 0, 0, 0, 0).getTime();
    const activeFor = (startMinute: number, endMinute: number) => (timestamp: number) => {
      const date = new Date(timestamp); const minute = date.getHours() * 60 + date.getMinutes();
      return startMinute < endMinute ? minute >= startMinute && minute < endMinute : minute >= startMinute || minute < endMinute;
    };
    expect(clipScheduledMilliseconds(day, day + 3 * 3_600_000, activeFor(60, 120))).toBe(3_600_000);
    expect(clipScheduledMilliseconds(day, day + 24 * 3_600_000, activeFor(23 * 60, 60))).toBe(2 * 3_600_000);
    // The production splitter evaluates each absolute minute independently, so skipped/repeated local minutes do not get bridged.
    expect(clipScheduledMilliseconds(0, 180_000, (timestamp) => timestamp < 60_000 || timestamp >= 120_000)).toBe(120_000);
  });

  test("restricted overnight schedules attribute post-midnight time to the previous weekday", () => {
    const overnight = (weekdays: number[]): Policy => ({ ...base, schedule: { weekdays, startMinute: 23 * 60, endMinute: 60 } });
    const monday2330 = new Date(2026, 0, 5, 23, 30).getTime();
    const tuesday0030 = new Date(2026, 0, 6, 0, 30).getTime();
    expect(scheduleActive(overnight([1]), monday2330)).toBe(true);
    expect(scheduleActive(overnight([1]), tuesday0030)).toBe(true);
    expect(scheduleActive(overnight([2]), tuesday0030)).toBe(false);
    expect(scheduleActive(overnight([1]), new Date(2026, 0, 6, 1, 0).getTime())).toBe(false);
    const sundaySchedule = overnight([7]);
    expect(scheduleActive(sundaySchedule, new Date(2026, 0, 5, 0, 30).getTime())).toBe(true);
    expect(scheduleActive(overnight([1]), new Date(2026, 0, 5, 0, 30).getTime())).toBe(false);
  });

  test("local counter excludes the off-schedule part of a crossing lease", async () => {
    const engine = new PolicyEngine(new MemoryStorage());
    const crossing = new Date(2026, 0, 15, 0, 59, 50, 0).getTime();
    await engine.cachePolicies(1, [{ ...base, schedule: { weekdays: [1,2,3,4,5,6,7], startMinute: 60, endMinute: 120 } }]);
    await engine.record(browserEvent({ eventTimestamp: crossing, mediaState: "playing" }));
    await engine.record(browserEvent({ eventTimestamp: crossing + 40_000, mediaState: "playing" }));
    expect((await engine.snapshot()).counters["1"]?.seconds).toBe(30);
  });

  test("schedule, exclusion, observe, and degraded modes are explicit", async () => {
    const engine = new PolicyEngine(new MemoryStorage());
    await engine.cachePolicies(1, [{ ...base, budgetSeconds: 0, enforcement: "observe", scope: { domains: ["youtube.com"], excludedDomains: ["music.youtube.com"] } }]);
    expect((await engine.evaluate("youtube.com"))[0]?.action).toBe("observe");
    expect(await engine.evaluate("music.youtube.com")).toHaveLength(0);
    await engine.cachePolicies(2, [{ ...base, revision: 2, budgetSeconds: 0, enforcement: "hard-block" }]);
    expect((await engine.evaluate("youtube.com", 1_000, true, false))[0]).toMatchObject({ action: "warn", degraded: true });
    expect((await engine.evaluate("youtube.com", 1_000, true, true))[0]).toMatchObject({ action: "hard-block", degraded: true });
  });
});
