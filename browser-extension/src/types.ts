export const SCHEMA_VERSION = 1 as const;
export const PAYLOAD_VERSION = 1 as const;
export const DEFAULT_ENDPOINT = "http://127.0.0.1:24923";
export const LEASE_MS = 45_000;
export const HEARTBEAT_MS = 20_000;
export const ALARM_PERIOD_MINUTES = 0.5;

export type PrivacyMode =
  | "off"
  | "domains-only"
  | "domains-and-titles"
  | "selected-sites-media"
  | "full-debug";

export type MediaStateName = "none" | "paused" | "playing" | "ended" | "seeking";
export type EventType =
  | "snapshot"
  | "window_focus"
  | "active_tab"
  | "navigation"
  | "visibility"
  | "media_play"
  | "media_pause"
  | "media_end"
  | "heartbeat"
  | "tab_close";

export interface MediaState {
  state: MediaStateName;
  audible: boolean;
  pictureInPicture: boolean;
  position?: number;
  duration?: number;
  playbackRate?: number;
}

export interface BrowserObservation {
  windowId: number;
  tabId: number;
  url?: string;
  title?: string;
  private: boolean;
  browserFocused: boolean;
  tabActive: boolean;
  documentVisible: boolean;
  audible: boolean;
  media: MediaState;
}

export interface BrowserEvent {
  payloadVersion: typeof PAYLOAD_VERSION;
  eventKey: string;
  clientSequence: number;
  eventTimestamp: number;
  clientId: string;
  eventType: EventType;
  windowId: number;
  tabId: number;
  domain?: string;
  origin?: string;
  title?: string;
  active: boolean;
  browserFocused: boolean;
  documentVisible: boolean;
  audible: boolean;
  mediaState: MediaStateName;
  pictureInPicture: boolean;
  mediaPosition?: number;
  mediaDuration?: number;
  playbackRate?: number;
  private: false;
}

export interface QueueHealth {
  depth: number;
  bytes: number;
  dropped: number;
  permanentRejected: number;
  recoverySnapshots: number;
  oldestTimestamp?: number;
  lastSuccessfulSync?: number;
  lastDataLoss?: number;
}
export interface EventBatch { schemaVersion: typeof SCHEMA_VERSION; queueHealth: QueueHealth; events: BrowserEvent[] }

export interface ExtensionSettings {
  endpoint: string;
  privacyMode: PrivacyMode;
  includePrivate: boolean;
  selectedMediaOrigins: string[];
  displayName: string;
  failClosed: boolean;
}

export interface ClientIdentity { clientId: string; nextSequence: number; token?: string }
export type Enforcement = "observe" | "warn" | "soft-block" | "hard-block";
export type PolicyAction = "allow" | "observe" | "warn" | "soft-block" | "hard-block" | "continue" | "snooze" | "override";
export type PolicyMetric =
  | "tab_active_seconds"
  | "browser_focused_seconds"
  | "document_visible_seconds"
  | "media_playback_seconds"
  | "audible_playback_seconds"
  | "focused_playback_seconds"
  | "verified_focused_playback_seconds"
  | "background_playback_seconds";

export interface PolicyScope { domains: string[]; excludedDomains: string[] }
export interface PolicySchedule { weekdays: number[]; startMinute: number; endMinute: number }
export interface PolicyGrace {
  graceSeconds: number;
  snoozeSeconds: number;
  maxDailySnoozes: number;
  sessionGapSeconds: number;
  rollingSeconds: number;
}
export interface Policy {
  id: number;
  name: string;
  revision: number;
  enabled: boolean;
  scope: PolicyScope;
  metric: PolicyMetric;
  windowKind: "calendar-day" | "rolling" | "session";
  budgetSeconds: number;
  warningSeconds?: number;
  enforcement: Enforcement;
  schedule: PolicySchedule;
  grace: PolicyGrace;
}
export interface PolicyDecision {
  policyId: number;
  revision: number;
  enforcement: Enforcement;
  action: PolicyAction;
  usedSeconds: number;
  budgetSeconds: number;
  remainingSeconds: number;
  reason?: string;
  degraded: boolean;
  snoozedUntil?: number;
}

export interface ContentMediaMessage {
  kind: "content-media";
  origin: string;
  visible: boolean;
  media: MediaState;
  reason: "snapshot" | "visibility" | "play" | "pause" | "ended" | "navigation" | "heartbeat";
}

export const DEFAULT_SETTINGS: ExtensionSettings = {
  endpoint: DEFAULT_ENDPOINT,
  privacyMode: "domains-only",
  includePrivate: false,
  selectedMediaOrigins: [],
  displayName: "My browser",
  failClosed: false,
};
