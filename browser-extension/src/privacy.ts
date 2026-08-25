import type { BrowserEvent, BrowserObservation, EventType, ExtensionSettings } from "./types";
import { PAYLOAD_VERSION } from "./types";

export interface NormalizedLocation {
  origin: string;
  domain: string;
}

export function normalizeLocation(raw: string | undefined): NormalizedLocation | undefined {
  if (!raw) return undefined;
  try {
    const parsed = new URL(raw);
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") return undefined;
    const hostname = parsed.hostname.toLowerCase().replace(/\.$/, "");
    if (!hostname || hostname.length > 253) return undefined;
    const port = parsed.port && !isDefaultPort(parsed.protocol, parsed.port) ? `:${parsed.port}` : "";
    return { domain: hostname, origin: `${parsed.protocol}//${hostname}${port}` };
  } catch {
    return undefined;
  }
}

function isDefaultPort(protocol: string, port: string): boolean {
  return (protocol === "http:" && port === "80") || (protocol === "https:" && port === "443");
}

export function domainMatches(domain: string, suffix: string): boolean {
  const normalizedDomain = domain.toLowerCase().replace(/\.$/, "");
  const normalizedSuffix = suffix.toLowerCase().replace(/^\./, "").replace(/\.$/, "");
  return normalizedDomain === normalizedSuffix || normalizedDomain.endsWith(`.${normalizedSuffix}`);
}

export function mediaAllowed(origin: string | undefined, selected: string[]): boolean {
  if (!origin) return false;
  return selected.some((item) => normalizeLocation(item)?.origin === origin);
}

export function sanitizeTitle(title: string | undefined): string | undefined {
  if (!title) return undefined;
  const cleaned = title.replace(/[\u0000-\u001f\u007f]/g, " ").trim();
  return cleaned ? cleaned.slice(0, 512) : undefined;
}

export function makeBrowserEvent(
  observation: BrowserObservation,
  eventType: EventType,
  settings: ExtensionSettings,
  identity: { clientId: string; sequence: number },
  now = Date.now(),
  eventKey: string = crypto.randomUUID(),
): BrowserEvent | undefined {
  if (settings.privacyMode === "off") return undefined;
  if (observation.private && !settings.includePrivate) return undefined;
  // Private collection is intentionally unavailable in v1 even if a future UI toggles it.
  if (observation.private) return undefined;
  const location = normalizeLocation(observation.url);
  const selectedMedia = mediaAllowed(location?.origin, settings.selectedMediaOrigins);
  const includeMedia = settings.privacyMode === "selected-sites-media" || settings.privacyMode === "full-debug";
  const includeTitle = settings.privacyMode === "domains-and-titles" || settings.privacyMode === "full-debug";
  const event: BrowserEvent = {
    payloadVersion: PAYLOAD_VERSION,
    eventKey,
    clientSequence: identity.sequence,
    eventTimestamp: now,
    clientId: identity.clientId,
    eventType,
    windowId: observation.windowId,
    tabId: observation.tabId,
    active: observation.tabActive,
    browserFocused: observation.browserFocused,
    documentVisible: observation.documentVisible,
    audible: selectedMedia && includeMedia ? observation.audible : false,
    mediaState: selectedMedia && includeMedia ? observation.media.state : "none",
    pictureInPicture: selectedMedia && includeMedia ? observation.media.pictureInPicture : false,
    private: false,
  };
  if (location) {
    event.domain = location.domain;
    event.origin = location.origin;
  }
  const title = includeTitle ? sanitizeTitle(observation.title) : undefined;
  if (title) event.title = title;
  if (selectedMedia && includeMedia) {
    const position = finite(observation.media.position);
    const duration = finite(observation.media.duration);
    const playbackRate = finite(observation.media.playbackRate);
    if (position !== undefined) event.mediaPosition = position;
    if (duration !== undefined) event.mediaDuration = duration;
    if (playbackRate !== undefined) event.playbackRate = playbackRate;
  }
  return event;
}

function finite(value: number | undefined): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

export function assertNoFullUrls(value: unknown): void {
  const serialized = JSON.stringify(value);
  if (/https?:\/\/[^"\s/?#]+[/?#][^"\s]*/i.test(serialized)) {
    throw new Error("privacy invariant: payload contains a full URL");
  }
}
