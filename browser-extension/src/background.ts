import { normalizeLocation, sanitizeTitle } from "./privacy";
import { DurableEventQueue } from "./queue";
import { reduceTabState, heartbeatDue, type ReducedTabState, type StateTrigger } from "./state";
import { loadSettings, saveSettings } from "./settings";
import { BrowserTransport, PairingStore } from "./transport";
import { ALARM_PERIOD_MINUTES, SCHEMA_VERSION } from "./types";
import type { BrowserEvent, BrowserObservation, ContentMediaMessage, EventType, MediaState, Policy, PolicyDecision } from "./types";
import { makeBrowserEvent } from "./privacy";
import { PolicyEngine } from "./policy";
import { addListener, getWebExtensionApi } from "./webext";

const api = getWebExtensionApi();
const storage = api.storage.local;
const queue = new DurableEventQueue(storage);
const pairing = new PairingStore(storage);
const policy = new PolicyEngine(storage);
const transport = new BrowserTransport(() => loadSettings(storage), pairing, queue);
const states = new Map<number, ReducedTabState>();
const contentMedia = new Map<number, { origin: string; visible: boolean; media: MediaState }>();
let focusedWindowId = -1;
let initialized: Promise<void> | undefined;
const appliedInterventions = new Set<string>();

function ensureInitialized(): Promise<void> {
  initialized ??= initialize().catch((error) => {
    initialized = undefined;
    console.warn("BMS Watchdog initialization failed", safeError(error));
  });
  return initialized;
}

async function initialize(): Promise<void> {
  const focused = await api.windows.getLastFocused({ populate: true });
  focusedWindowId = focused?.focused ? focused.id : -1;
  const tabs = Array.isArray(focused?.tabs) ? focused.tabs : await api.tabs.query({ active: true });
  for (const tab of tabs) await observeTab(tab, "snapshot");
  await refreshPolicy().catch(() => undefined);
  await flushAndApply().catch(() => undefined);
  // Chromium MV3 clamps periodic alarms to 30 seconds; Firefox supports this cadence too.
  api.alarms.create("bms-heartbeat", { periodInMinutes: ALARM_PERIOD_MINUTES });
  api.alarms.create("bms-policy", { periodInMinutes: 5 });
}

addListener(api.windows.onFocusChanged, async (windowId: number) => {
  await ensureInitialized();
  focusedWindowId = windowId;
  for (const state of [...states.values()]) {
    const observation = { ...state.observation, browserFocused: state.observation.windowId === windowId };
    await processObservation(observation, "window-focus");
  }
  if (windowId >= 0) {
    for (const tab of await api.tabs.query({ windowId, active: true })) await observeTab(tab, "window-focus");
  }
});

addListener(api.tabs.onActivated, async (info: { tabId: number; windowId: number }) => {
  await ensureInitialized();
  for (const state of [...states.values()]) {
    if (state.observation.windowId === info.windowId && state.observation.tabId !== info.tabId && state.observation.tabActive) {
      await processObservation({ ...state.observation, tabActive: false }, "tab-active");
    }
  }
  await refreshTab(info.tabId, "tab-active");
});

addListener(api.tabs.onUpdated, async (tabId: number, changeInfo: Record<string, unknown>, tab: any) => {
  await ensureInitialized();
  const trigger: StateTrigger = typeof changeInfo.url === "string" ? "navigation" : "audible";
  await observeTab(tab ?? await api.tabs.get(tabId), trigger);
});

addListener(api.webNavigation.onCommitted, async (details: { tabId: number; frameId: number }) => {
  if (details.frameId !== 0) return;
  await ensureInitialized();
  contentMedia.delete(details.tabId);
  await refreshTab(details.tabId, "navigation");
});

addListener(api.tabs.onRemoved, async (tabId: number) => {
  await ensureInitialized();
  const previous = states.get(tabId);
  if (previous) {
    const closed: BrowserObservation = {
      ...previous.observation,
      browserFocused: false,
      tabActive: false,
      documentVisible: false,
      audible: false,
      media: emptyMedia(),
    };
    await emit(closed, "tab_close");
  }
  states.delete(tabId);
  contentMedia.delete(tabId);
});

addListener(api.alarms.onAlarm, async (alarm: { name: string }) => {
  await ensureInitialized();
  if (alarm.name === "bms-heartbeat") {
    const now = Date.now();
    for (const state of [...states.values()]) {
      if (heartbeatDue(state, now)) await processObservation(state.observation, "heartbeat", now);
    }
    await flushAndApply(now).catch(() => undefined);
  } else if (alarm.name === "bms-policy") {
    await refreshPolicy().catch(() => undefined);
  }
});

if ((globalThis as any).browser) {
  addListener(api.runtime.onMessage, (message: unknown, sender: any) => handleMessage(message, sender));
} else {
  addListener(api.runtime.onMessage, (message: unknown, sender: any, sendResponse: (value: unknown) => void) => {
    void handleMessage(message, sender).then(sendResponse, (error) => sendResponse({ error: safeError(error) }));
    return true;
  });
}
addListener(api.runtime.onInstalled, () => ensureInitialized());
addListener(api.runtime.onStartup, () => ensureInitialized());
const action = (api as any).action ?? (api as any).browserAction;
addListener(action?.onClicked, () => api.runtime.openOptionsPage());
void ensureInitialized();

async function handleMessage(message: unknown, sender: any): Promise<unknown> {
  await ensureInitialized();
  if (!message || typeof message !== "object") return undefined;
  const input = message as Record<string, any>;
  if (input.kind === "content-media") return handleContentMedia(input as unknown as ContentMediaMessage, sender);
  if (input.kind === "get-status") return status();
  if (input.kind === "save-settings") {
    const settings = await saveSettings(storage, input.settings);
    await syncConfigIfPaired();
    await injectEligibleTabs();
    return { settings };
  }
  if (input.kind === "pair") {
    const manifest = api.runtime.getManifest();
    await transport.pair(String(input.code ?? ""), browserFamily(), manifest.version);
    return status();
  }
  if (input.kind === "revoke") {
    await transport.revoke();
    await policy.clear();
    return status();
  }
  if (input.kind === "sync") {
    await flushAndApply();
    await refreshPolicy().catch(() => undefined);
    return status();
  }
  if (input.kind === "request-media-permission") {
    const origin = normalizeLocation(String(input.origin ?? ""))?.origin;
    if (!origin) throw new Error("Invalid origin");
    const granted = await requestOrigin(origin);
    if (granted) {
      const settings = await loadSettings(storage);
      await saveSettings(storage, { ...settings, selectedMediaOrigins: [...settings.selectedMediaOrigins, origin] });
      await syncConfigIfPaired();
      await injectEligibleTabs();
    }
    return { granted };
  }
  if (input.kind === "remove-media-origin") {
    const origin = normalizeLocation(String(input.origin ?? ""))?.origin;
    const settings = await loadSettings(storage);
    await saveSettings(storage, { ...settings, selectedMediaOrigins: settings.selectedMediaOrigins.filter((item) => item !== origin) });
    await syncConfigIfPaired();
    return status();
  }
  if (input.kind === "intervention-action") {
    const policyId = Number(input.policyId);
    if (!Number.isSafeInteger(policyId) || policyId <= 0) throw new Error("Invalid policy id");
    const action = input.action === "snooze" ? "snooze" : "continue";
    const decision = action === "snooze" ? await policy.snooze(policyId) : await policy.continue(policyId);
    if (sender?.tab?.id !== undefined) await sendDecision(sender.tab.id, decision, false);
    await transport.reportIntervention({
      schemaVersion: SCHEMA_VERSION,
      policyId,
      action,
      decisionRevision: Number(input.revision ?? 0),
      eventTimestamp: Date.now(),
      remainingSeconds: decision.remainingSeconds,
    }).catch(() => undefined);
    return { ok: true };
  }
  return undefined;
}

async function handleContentMedia(message: ContentMediaMessage, sender: any): Promise<void> {
  const tab = sender?.tab;
  if (!tab || typeof tab.id !== "number") return;
  const senderLocation = normalizeLocation(tab.url);
  const messageLocation = normalizeLocation(message.origin);
  if (!senderLocation || senderLocation.origin !== messageLocation?.origin) return;
  const settings = await loadSettings(storage);
  if (!settings.selectedMediaOrigins.includes(senderLocation.origin)) return;
  contentMedia.set(tab.id, { origin: senderLocation.origin, visible: Boolean(message.visible), media: sanitizeMedia(message.media) });
  const trigger: StateTrigger = message.reason === "visibility" ? "visibility" : message.reason === "heartbeat" ? "heartbeat" : message.reason === "navigation" ? "navigation" : "media";
  await observeTab(tab, trigger);
}

async function refreshTab(tabId: number, trigger: StateTrigger): Promise<void> {
  try { await observeTab(await api.tabs.get(tabId), trigger); } catch { /* Closed tab. */ }
}

async function observeTab(tab: any, trigger: StateTrigger): Promise<void> {
  if (!tab || typeof tab.id !== "number" || typeof tab.windowId !== "number") return;
  const media = contentMedia.get(tab.id);
  const observation: BrowserObservation = {
    windowId: tab.windowId,
    tabId: tab.id,
    ...(typeof tab.url === "string" ? { url: tab.url } : {}),
    ...(typeof tab.title === "string" ? { title: tab.title } : {}),
    private: Boolean(tab.incognito),
    browserFocused: tab.windowId === focusedWindowId,
    tabActive: Boolean(tab.active),
    documentVisible: media?.visible ?? Boolean(tab.active),
    audible: media?.media.audible ?? Boolean(tab.audible),
    media: media?.media ?? emptyMedia(),
  };
  await processObservation(observation, trigger);
  await maybeInject(tab);
}

async function processObservation(observation: BrowserObservation, trigger: StateTrigger, now = Date.now()): Promise<void> {
  const previous = states.get(observation.tabId);
  const reduction = reduceTabState(previous, observation, trigger, now);
  states.set(observation.tabId, reduction.next);
  if (reduction.eventType) await emit(observation, reduction.eventType, now);
}

async function emit(observation: BrowserObservation, eventType: EventType, now = Date.now()): Promise<void> {
  const settings = await loadSettings(storage);
  const sequence = await pairing.reserveSequence();
  const event = makeBrowserEvent(observation, eventType, settings, sequence, now);
  if (!event) return;
  const response = await transport.enqueue(event).catch(() => undefined);
  const decisions = await policy.record(event, now);
  const policyState = await policy.snapshot();
  const decision = strongest(decisions);
  if (decision) {
    const stale = now - policyState.fetchedAt > 10 * 60_000;
    const blocking = decision.action === "soft-block" || decision.action === "hard-block";
    const effective = stale && !settings.failClosed && blocking
      ? { ...decision, action: "warn" as const, enforcement: "warn" as const, degraded: true, reason: "Policy server unavailable; failing open" }
      : decision;
    await sendDecision(observation.tabId, effective);
  }
  if (response?.policyDecision) await sendDecision(observation.tabId, response.policyDecision);
}

async function sendDecision(tabId: number, decision: PolicyDecision, audit = true): Promise<void> {
  try {
    await api.tabs.sendMessage(tabId, { kind: "policy-decision", decision });
    if (audit && ["warn", "soft-block", "hard-block"].includes(decision.action)) {
      const key = `${decision.policyId}:${decision.revision}:${decision.action}`;
      if (!appliedInterventions.has(key)) {
        appliedInterventions.add(key);
        await transport.reportIntervention({
          schemaVersion: SCHEMA_VERSION,
          policyId: decision.policyId,
          action: decision.action,
          decisionRevision: decision.revision,
          eventTimestamp: Date.now(),
          remainingSeconds: decision.remainingSeconds,
        }).catch(() => undefined);
      }
    }
  } catch { /* No selected-site script in this tab. */ }
}

async function refreshPolicy(): Promise<void> {
  const active = [...states.values()].find((state) => state.observation.tabActive && state.observation.browserFocused);
  const domain = normalizeLocation(active?.observation.url)?.domain;
  const response = await transport.getPolicyState<{ schemaVersion: number; revision: number; policies: Policy[]; decisions: PolicyDecision[] }>(domain);
  if (response.schemaVersion === SCHEMA_VERSION && Number.isInteger(response.revision) && Array.isArray(response.policies)) {
    await policy.cachePolicies(response.revision, response.policies, response.decisions ?? []);
  }
}

async function flushAndApply(now = Date.now()): Promise<void> {
  const response = await transport.flush(now);
  if (!response?.policyDecision) return;
  const active = [...states.values()].find((state) => state.observation.tabActive && state.observation.browserFocused);
  if (active) await sendDecision(active.observation.tabId, response.policyDecision);
}

async function status(): Promise<Record<string, unknown>> {
  const settings = await loadSettings(storage);
  const identity = await pairing.get();
  const queueStatus = await queue.status();
  const policyState = await policy.snapshot();
  const active = [...states.values()].find((state) => state.observation.tabActive && state.observation.browserFocused);
  const location = normalizeLocation(active?.observation.url);
  return {
    settings,
    paired: Boolean(identity?.token),
    clientId: identity?.clientId,
    queue: queueStatus,
    policy: { revision: policyState.revision, count: policyState.policies.length, fetchedAt: policyState.fetchedAt },
    preview: active ? {
      domain: location?.domain,
      origin: location?.origin,
      titleIncluded: settings.privacyMode === "domains-and-titles" || settings.privacyMode === "full-debug",
      ...((settings.privacyMode === "domains-and-titles" || settings.privacyMode === "full-debug") && sanitizeTitle(active.observation.title)
        ? { title: sanitizeTitle(active.observation.title) }
        : {}),
      mediaIncluded: Boolean(location && settings.selectedMediaOrigins.includes(location.origin)),
      active: true,
      browserFocused: true,
      documentVisible: active.observation.documentVisible,
      audible: active.observation.audible,
      mediaState: active.observation.media.state,
      privateExcluded: active.observation.private,
    } : undefined,
  };
}

async function syncConfigIfPaired(): Promise<void> {
  if ((await pairing.get())?.token) await transport.updateConfig();
}

async function requestOrigin(origin: string): Promise<boolean> {
  if (!api.permissions) return false;
  return Boolean(await api.permissions.request({ origins: [`${origin}/*`] }));
}

async function maybeInject(tab: any): Promise<void> {
  if (typeof tab.id !== "number") return;
  const origin = normalizeLocation(tab.url)?.origin;
  if (!origin) return;
  const settings = await loadSettings(storage);
  if (!settings.selectedMediaOrigins.includes(origin)) return;
  if (api.permissions && !(await api.permissions.contains({ origins: [`${origin}/*`] }))) return;
  try {
    if (api.scripting) await api.scripting.executeScript({ target: { tabId: tab.id }, files: ["content.js"] });
    else await api.tabs.executeScript(tab.id, { file: "content.js", runAt: "document_idle" });
  } catch { /* Restricted page or already navigating. */ }
}

async function injectEligibleTabs(): Promise<void> {
  for (const tab of await api.tabs.query({})) await maybeInject(tab);
}

function strongest(decisions: PolicyDecision[]): PolicyDecision | undefined {
  const rank: Record<string, number> = { allow: 0, observe: 0, warn: 1, "soft-block": 2, "hard-block": 3 };
  return decisions.sort((a, b) => (rank[b.action] ?? 0) - (rank[a.action] ?? 0))[0];
}

function sanitizeMedia(media: MediaState): MediaState {
  const state = ["none", "paused", "playing", "ended", "seeking"].includes(media?.state) ? media.state : "none";
  return {
    state,
    audible: Boolean(media?.audible),
    pictureInPicture: Boolean(media?.pictureInPicture),
    ...(finiteBounded(media?.position, 0, 864_000) !== undefined ? { position: finiteBounded(media.position, 0, 864_000)! } : {}),
    ...(finiteBounded(media?.duration, 0, 864_000) !== undefined ? { duration: finiteBounded(media.duration, 0, 864_000)! } : {}),
    ...(finiteBounded(media?.playbackRate, 0.1, 16) !== undefined ? { playbackRate: finiteBounded(media.playbackRate, 0.1, 16)! } : {}),
  };
}

function finiteBounded(value: unknown, min: number, max: number): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? Math.min(max, Math.max(min, value)) : undefined;
}

function emptyMedia(): MediaState { return { state: "none", audible: false, pictureInPicture: false }; }
function browserFamily(): string { return api.runtime.getURL("").startsWith("moz-extension:") ? "firefox" : "chromium"; }
function safeError(error: unknown): string { return error instanceof Error ? error.name : "unknown"; }
