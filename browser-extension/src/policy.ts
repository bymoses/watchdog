import { domainMatches } from "./privacy";
import { LEASE_MS } from "./types";
import type { BrowserEvent, Policy, PolicyDecision, PolicyMetric } from "./types";
import type { StorageArea } from "./webext";

interface ActiveLease { active: boolean; lastEventTimestamp: number }
export interface PolicyCounter {
  policyId: number;
  revision: number;
  windowKey: string;
  seconds: number;
  lastTimestamp?: number;
  activeTabs: Record<string, ActiveLease>;
  rolling: Array<[number, number]>;
  snoozedUntil?: number;
  continuedUntil?: number;
  dailySnoozes: number;
}
interface PolicyRecord {
  revision: number;
  policies: Policy[];
  counters: Record<string, PolicyCounter>;
  fetchedAt: number;
}

export class PolicyEngine {
  static readonly KEY = "policyCacheV1";
  constructor(private readonly storage: StorageArea) {}

  async cachePolicies(revision: number, policies: Policy[], decisions: PolicyDecision[] = [], now = Date.now()): Promise<void> {
    const current = await this.load();
    const validIds = new Set(policies.map((item) => String(item.id)));
    current.counters = Object.fromEntries(Object.entries(current.counters).filter(([id]) => validIds.has(id)));
    current.revision = revision;
    current.policies = policies;
    current.fetchedAt = now;
    for (const decision of decisions) {
      const policy = policies.find((item) => item.id === decision.policyId && item.revision === decision.revision);
      if (!policy) continue;
      const counter = this.counterFor(current, policy, now);
      counter.seconds = Math.max(0, decision.usedSeconds);
      counter.rolling = policy.windowKind === "rolling" ? [[now, counter.seconds]] : [];
    }
    await this.save(current);
  }

  async record(event: BrowserEvent, now = event.eventTimestamp): Promise<PolicyDecision[]> {
    const record = await this.load();
    const decisions: PolicyDecision[] = [];
    for (const policy of record.policies) {
      if (!policy.enabled) continue;
      const counter = this.counterFor(record, policy, now);
      this.rollWindow(counter, policy, now);
      accrue(counter, policy, now);
      pruneStale(counter, now);
      const tabKey = `${event.windowId}:${event.tabId}`;
      const domainInScope = event.domain !== undefined && policyMatchesDomain(policy, event.domain);
      if (event.eventType === "tab_close") delete counter.activeTabs[tabKey];
      else counter.activeTabs[tabKey] = {
        active: domainInScope && metricActive(policy.metric, event),
        lastEventTimestamp: now,
      };
      counter.lastTimestamp = now;
      if (domainInScope) decisions.push(decide(policy, counter, now));
    }
    await this.save(record);
    return decisions;
  }

  async evaluate(domain: string, now = Date.now(), degraded = false, failClosed = false): Promise<PolicyDecision[]> {
    const record = await this.load();
    return record.policies.filter((item) => item.enabled && policyMatchesDomain(item, domain)).map((item) => {
      const counter = this.counterFor(record, item, now);
      this.rollWindow(counter, item, now);
      accrue(counter, item, now);
      pruneStale(counter, now);
      counter.lastTimestamp = now;
      const decision = decide(item, counter, now);
      if (degraded) {
        decision.degraded = true;
        if (!failClosed && (decision.action === "soft-block" || decision.action === "hard-block")) {
          decision.action = "warn";
          decision.enforcement = "warn";
          decision.reason = "Policy server unavailable; failing open";
        }
      }
      return decision;
    });
  }

  async snooze(policyId: number, now = Date.now()): Promise<PolicyDecision> {
    const { record, policy, counter } = await this.actionState(policyId, now);
    if (counter.dailySnoozes >= policy.grace.maxDailySnoozes) throw new Error("No snoozes remaining");
    counter.dailySnoozes += 1;
    counter.snoozedUntil = now + policy.grace.snoozeSeconds * 1_000;
    await this.save(record);
    return decide(policy, counter, now);
  }

  async continue(policyId: number, now = Date.now()): Promise<PolicyDecision> {
    const { record, policy, counter } = await this.actionState(policyId, now);
    counter.continuedUntil = now + Math.max(1, policy.grace.graceSeconds) * 1_000;
    await this.save(record);
    return decide(policy, counter, now);
  }

  async clear(): Promise<void> { await this.storage.remove(PolicyEngine.KEY); }
  async snapshot(): Promise<PolicyRecord> { return this.load(); }

  private async actionState(policyId: number, now: number): Promise<{ record: PolicyRecord; policy: Policy; counter: PolicyCounter }> {
    const record = await this.load();
    const policy = record.policies.find((item) => item.id === policyId);
    if (!policy) throw new Error("Unknown policy");
    const counter = this.counterFor(record, policy, now);
    this.rollWindow(counter, policy, now);
    return { record, policy, counter };
  }

  private counterFor(record: PolicyRecord, policy: Policy, now: number): PolicyCounter {
    const key = String(policy.id);
    let counter = record.counters[key];
    if (!counter || counter.revision !== policy.revision) {
      counter = { policyId: policy.id, revision: policy.revision, windowKey: windowKey(policy, now), seconds: 0, activeTabs: {}, rolling: [], dailySnoozes: 0 };
      record.counters[key] = counter;
    }
    return counter;
  }

  private rollWindow(counter: PolicyCounter, policy: Policy, now: number): void {
    const key = windowKey(policy, now);
    if (policy.windowKind === "calendar-day" && counter.windowKey !== key) resetCounter(counter, key);
    else if (policy.windowKind === "session" && counter.lastTimestamp !== undefined && !hasActive(counter) && now - counter.lastTimestamp > policy.grace.sessionGapSeconds * 1_000) resetCounter(counter, key);
    else if (policy.windowKind === "rolling") {
      const cutoff = now - policy.grace.rollingSeconds * 1_000;
      counter.rolling = counter.rolling.filter(([timestamp]) => timestamp >= cutoff);
      counter.seconds = counter.rolling.reduce((sum, [, seconds]) => sum + seconds, 0);
    }
  }

  private async load(): Promise<PolicyRecord> {
    const raw = (await this.storage.get(PolicyEngine.KEY))[PolicyEngine.KEY] as PolicyRecord | undefined;
    return raw ?? { revision: 0, policies: [], counters: {}, fetchedAt: 0 };
  }
  private save(record: PolicyRecord): Promise<void> { return this.storage.set({ [PolicyEngine.KEY]: record }); }
}

function accrue(counter: PolicyCounter, policy: Policy, now: number): void {
  if (counter.lastTimestamp === undefined || now <= counter.lastTimestamp) return;
  const end = Math.min(now, counter.lastTimestamp + LEASE_MS);
  const elapsed = hasActive(counter) ? scheduledMilliseconds(policy, counter.lastTimestamp, end) / 1_000 : 0;
  if (elapsed > 0) {
    counter.seconds += elapsed;
    if (policy.windowKind === "rolling") counter.rolling.push([end, elapsed]);
  }
}
function hasActive(counter: PolicyCounter): boolean { return Object.values(counter.activeTabs).some((lease) => lease.active); }
function pruneStale(counter: PolicyCounter, now: number): void {
  for (const [tab, lease] of Object.entries(counter.activeTabs)) {
    if (now - lease.lastEventTimestamp > LEASE_MS) delete counter.activeTabs[tab];
  }
}
function policyMatchesDomain(policy: Policy, domain: string): boolean {
  return policy.scope.domains.some((scope) => domainMatches(domain, scope)) &&
    !policy.scope.excludedDomains.some((scope) => domainMatches(domain, scope));
}
export function clipScheduledMilliseconds(start: number, end: number, activeAt: (timestamp: number) => boolean): number {
  let total = 0;
  for (let cursor = start; cursor < end;) {
    const next = Math.min(end, (Math.floor(cursor / 60_000) + 1) * 60_000);
    if (activeAt(cursor)) total += next - cursor;
    cursor = next;
  }
  return total;
}
function scheduledMilliseconds(policy: Policy, start: number, end: number): number {
  return clipScheduledMilliseconds(start, end, (timestamp) => scheduleActive(policy, timestamp));
}
export function scheduleActive(policy: Policy, now: number): boolean {
  const date = new Date(now);
  const weekday = date.getDay() === 0 ? 7 : date.getDay();
  const selected = (day: number) => !policy.schedule.weekdays.length || policy.schedule.weekdays.includes(day);
  const minute = date.getHours() * 60 + date.getMinutes();
  const { startMinute: start, endMinute: end } = policy.schedule;
  if (start === end || end === 1440 && start === 0) return selected(weekday);
  if (start < end) return selected(weekday) && minute >= start && minute < end;
  if (minute >= start) return selected(weekday);
  const previousWeekday = weekday === 1 ? 7 : weekday - 1;
  return minute < end && selected(previousWeekday);
}

export function metricActive(metric: PolicyMetric, event: BrowserEvent): boolean {
  const media = event.mediaState === "playing" || event.mediaState === "seeking";
  switch (metric) {
    case "tab_active_seconds": return event.active;
    case "browser_focused_seconds": return event.browserFocused;
    case "document_visible_seconds": return event.documentVisible;
    case "media_playback_seconds": return media;
    case "audible_playback_seconds": return media && event.audible;
    case "focused_playback_seconds":
    case "verified_focused_playback_seconds": return media && event.active && event.browserFocused && event.documentVisible;
    case "background_playback_seconds": return media && (!event.browserFocused || !event.active || !event.documentVisible);
  }
}

function decide(policy: Policy, counter: PolicyCounter, now: number): PolicyDecision {
  const effectiveBudget = policy.budgetSeconds + policy.grace.graceSeconds;
  const remainingSeconds = Math.max(0, effectiveBudget - counter.seconds);
  const base = { policyId: policy.id, revision: policy.revision, enforcement: policy.enforcement, usedSeconds: counter.seconds, budgetSeconds: policy.budgetSeconds, remainingSeconds, degraded: false };
  if (counter.snoozedUntil && counter.snoozedUntil > now) return { ...base, action: "allow", reason: "Snoozed", snoozedUntil: counter.snoozedUntil };
  if (counter.continuedUntil && counter.continuedUntil > now) return { ...base, action: "allow", reason: "Continued" };
  if (policy.enforcement === "observe") return { ...base, enforcement: "observe", action: "observe" };
  const warningAt = policy.warningSeconds ?? 0;
  if (remainingSeconds > warningAt) return { ...base, action: "allow" };
  if (remainingSeconds > 0 || policy.enforcement === "warn") return { ...base, enforcement: "warn", action: "warn", reason: "Attention budget nearly exhausted" };
  return { ...base, action: policy.enforcement, reason: "Attention budget exhausted" };
}
function windowKey(policy: Policy, now: number): string {
  if (policy.windowKind !== "calendar-day") return policy.windowKind;
  const date = new Date(now);
  return `${date.getFullYear()}-${date.getMonth() + 1}-${date.getDate()}`;
}
function resetCounter(counter: PolicyCounter, key: string): void {
  counter.windowKey = key;
  counter.seconds = 0;
  delete counter.lastTimestamp;
  counter.activeTabs = {};
  counter.rolling = [];
  delete counter.snoozedUntil;
  delete counter.continuedUntil;
  counter.dailySnoozes = 0;
}
