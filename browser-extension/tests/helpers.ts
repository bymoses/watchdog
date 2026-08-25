import type { BrowserEvent, BrowserObservation, ExtensionSettings } from "../src/types";
import { DEFAULT_SETTINGS, PAYLOAD_VERSION } from "../src/types";
import type { StorageArea } from "../src/webext";

export class MemoryStorage implements StorageArea {
  data: Record<string, any> = {};
  async get(keys?: string | string[] | Record<string, unknown> | null): Promise<Record<string, any>> {
    if (typeof keys === "string") return { [keys]: structuredClone(this.data[keys]) };
    if (Array.isArray(keys)) return Object.fromEntries(keys.map((key) => [key, structuredClone(this.data[key])]));
    return structuredClone(this.data);
  }
  async set(values: Record<string, unknown>): Promise<void> { Object.assign(this.data, structuredClone(values)); }
  async remove(keys: string | string[]): Promise<void> { for (const key of Array.isArray(keys) ? keys : [keys]) delete this.data[key]; }
}

export function observation(overrides: Partial<BrowserObservation> = {}): BrowserObservation {
  return {
    windowId: 1,
    tabId: 2,
    url: "https://www.youtube.com/watch?v=secret#fragment",
    title: "A private title",
    private: false,
    browserFocused: true,
    tabActive: true,
    documentVisible: true,
    audible: false,
    media: { state: "none", audible: false, pictureInPicture: false },
    ...overrides,
  };
}

export function settings(overrides: Partial<ExtensionSettings> = {}): ExtensionSettings {
  return { ...DEFAULT_SETTINGS, ...overrides };
}

export function browserEvent(overrides: Partial<BrowserEvent> = {}): BrowserEvent {
  return {
    payloadVersion: PAYLOAD_VERSION,
    eventKey: crypto.randomUUID(),
    clientSequence: 1,
    eventTimestamp: 1_000,
    clientId: "client",
    eventType: "snapshot",
    windowId: 1,
    tabId: 2,
    domain: "youtube.com",
    origin: "https://youtube.com",
    active: true,
    browserFocused: true,
    documentVisible: true,
    audible: false,
    mediaState: "none",
    pictureInPicture: false,
    private: false,
    ...overrides,
  };
}
