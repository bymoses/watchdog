import { normalizeLocation } from "../privacy";
import type { ContentMediaMessage, MediaState, PolicyDecision } from "../types";
import { adapterFor } from "./adapters";
import { InterventionOverlay } from "./overlay";

declare global {
  interface Window { __bmsWatchdogLoaded?: boolean }
}

const api = (globalThis as any).browser ?? (globalThis as any).chrome;
const locationFact = normalizeLocation(location.href);

if (!window.__bmsWatchdogLoaded && locationFact && api?.runtime) {
  window.__bmsWatchdogLoaded = true;
  start(locationFact.origin);
}

function start(origin: string): void {
  let adapter = adapterFor(origin);
  let restartSuppressed = false;
  let lastSerialized = "";
  let heartbeatTimer = 0;
  const send = (message: unknown): void => {
    try {
      const result = api.runtime.sendMessage(message);
      if (result?.catch) result.catch(() => undefined);
    } catch { /* Extension context may have been invalidated. */ }
  };
  const overlay = new InterventionOverlay(send);

  const report = (reason: ContentMediaMessage["reason"], force = false): void => {
    const current = normalizeLocation(location.href);
    if (!current) return;
    if (current.origin !== origin) {
      origin = current.origin;
      adapter = adapterFor(origin);
    }
    const media = adapter.readMediaState(document) ?? emptyMedia();
    const serialized = JSON.stringify([origin, document.visibilityState, media]);
    if (!force && reason !== "heartbeat" && serialized === lastSerialized) return;
    lastSerialized = serialized;
    const message: ContentMediaMessage = {
      kind: "content-media",
      origin,
      visible: document.visibilityState === "visible",
      media,
      reason,
    };
    send(message);
    scheduleHeartbeat(media.state === "playing" ? 15_000 : 45_000);
  };

  const scheduleHeartbeat = (delay: number): void => {
    clearTimeout(heartbeatTimer);
    heartbeatTimer = window.setTimeout(() => report("heartbeat", true), delay);
  };

  document.addEventListener("visibilitychange", () => report("visibility", true));
  for (const event of ["play", "playing", "pause", "ended", "seeking", "seeked", "volumechange", "enterpictureinpicture", "leavepictureinpicture"]) {
    document.addEventListener(event, () => {
      if (restartSuppressed && (event === "play" || event === "playing")) pauseAll();
      report(event === "ended" ? "ended" : event === "pause" ? "pause" : "play", true);
    }, true);
  }

  let mutationTimer = 0;
  const observer = new MutationObserver(() => {
    clearTimeout(mutationTimer);
    mutationTimer = window.setTimeout(() => report("snapshot"), 250);
  });
  observer.observe(document.documentElement, { childList: true, subtree: true });

  const navigation = (): void => { window.setTimeout(() => report("navigation", true), 0); };
  for (const method of ["pushState", "replaceState"] as const) {
    const original = history[method].bind(history) as (...args: unknown[]) => unknown;
    (history as any)[method] = (...args: unknown[]) => {
      const result = original(...args);
      navigation();
      return result;
    };
  }
  addEventListener("popstate", navigation);
  addEventListener("hashchange", navigation);

  api.runtime.onMessage.addListener((message: unknown) => {
    if (!isPolicyDecisionMessage(message)) return undefined;
    const state = overlay.apply(message.decision);
    // Soft and hard blocks both suppress immediate media restart until an allow/continue/snooze decision arrives.
    restartSuppressed = state.pauseMedia;
    adapter.applyIntervention(message.decision, document);
    if (state.pauseMedia) pauseAll();
    return Promise.resolve({ applied: true });
  });

  report("snapshot", true);

  function pauseAll(): void {
    for (const media of document.querySelectorAll<HTMLMediaElement>("video,audio")) media.pause();
  }
}

function emptyMedia(): MediaState {
  return { state: "none", audible: false, pictureInPicture: false };
}

function isPolicyDecisionMessage(value: unknown): value is { kind: "policy-decision"; decision: PolicyDecision } {
  return Boolean(value && typeof value === "object" && (value as any).kind === "policy-decision" && (value as any).decision);
}
