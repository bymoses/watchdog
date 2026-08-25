import { HEARTBEAT_MS } from "./types";
import type { BrowserObservation, EventType, MediaStateName } from "./types";
import { normalizeLocation } from "./privacy";

export interface ReducedTabState {
  observation: BrowserObservation;
  fingerprint: string;
  lastEmittedAt: number;
}

export interface Reduction {
  next: ReducedTabState;
  eventType?: EventType;
  material: boolean;
}

export type StateTrigger =
  | "snapshot"
  | "window-focus"
  | "tab-active"
  | "navigation"
  | "visibility"
  | "media"
  | "audible"
  | "heartbeat";

function fingerprint(observation: BrowserObservation): string {
  const location = normalizeLocation(observation.url);
  return JSON.stringify([
    location?.origin ?? "",
    observation.browserFocused,
    observation.tabActive,
    observation.documentVisible,
    observation.audible,
    observation.media.state,
    observation.media.pictureInPicture,
  ]);
}

function mediaEvent(state: MediaStateName): EventType {
  if (state === "playing" || state === "seeking") return "media_play";
  if (state === "ended") return "media_end";
  return "media_pause";
}

export function reduceTabState(
  previous: ReducedTabState | undefined,
  observation: BrowserObservation,
  trigger: StateTrigger,
  now = Date.now(),
): Reduction {
  const nextFingerprint = fingerprint(observation);
  const material = !previous || previous.fingerprint !== nextFingerprint;
  let eventType: EventType | undefined;
  if (!previous) eventType = "snapshot";
  else if (material) {
    const old = previous.observation;
    const oldOrigin = normalizeLocation(old.url)?.origin;
    const nextOrigin = normalizeLocation(observation.url)?.origin;
    if (trigger === "navigation" || oldOrigin !== nextOrigin) eventType = "navigation";
    else if (old.media.state !== observation.media.state || trigger === "media") eventType = mediaEvent(observation.media.state);
    else if (old.documentVisible !== observation.documentVisible || trigger === "visibility") eventType = "visibility";
    else if (old.tabActive !== observation.tabActive || trigger === "tab-active") eventType = "active_tab";
    else if (old.browserFocused !== observation.browserFocused || trigger === "window-focus") eventType = "window_focus";
    else eventType = "snapshot";
  } else if (trigger === "heartbeat") {
    eventType = "heartbeat";
  }
  return {
    next: {
      observation,
      fingerprint: nextFingerprint,
      lastEmittedAt: eventType ? now : (previous?.lastEmittedAt ?? now),
    },
    ...(eventType ? { eventType } : {}),
    material,
  };
}

export function heartbeatIntervalMs(_state: ReducedTabState): number {
  // Always comfortably inside the 45-second lease; MV2/MV3 alarms run every 30 seconds.
  return HEARTBEAT_MS;
}

export function heartbeatDue(state: ReducedTabState, now = Date.now()): boolean {
  const eligible = state.observation.tabActive || state.observation.media.state === "playing";
  return eligible && now - state.lastEmittedAt >= heartbeatIntervalMs(state);
}
