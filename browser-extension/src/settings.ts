import { normalizeLocation } from "./privacy";
import { DEFAULT_SETTINGS, type ExtensionSettings, type PrivacyMode } from "./types";
import type { StorageArea } from "./webext";

export const SETTINGS_KEY = "settingsV1";
const MODES = new Set<PrivacyMode>(["off", "domains-only", "domains-and-titles", "selected-sites-media", "full-debug"]);

export async function loadSettings(storage: StorageArea): Promise<ExtensionSettings> {
  const raw = (await storage.get(SETTINGS_KEY))[SETTINGS_KEY] as Partial<ExtensionSettings> | undefined;
  return validateSettings({ ...DEFAULT_SETTINGS, ...raw });
}

export async function saveSettings(storage: StorageArea, value: unknown): Promise<ExtensionSettings> {
  const settings = validateSettings(value);
  await storage.set({ [SETTINGS_KEY]: settings });
  return settings;
}

export function validateSettings(value: unknown): ExtensionSettings {
  if (!value || typeof value !== "object") throw new Error("Settings must be an object");
  const input = value as Partial<ExtensionSettings>;
  if (!input.privacyMode || !MODES.has(input.privacyMode)) throw new Error("Unknown privacy mode");
  const endpoint = new URL(input.endpoint ?? DEFAULT_SETTINGS.endpoint);
  if (endpoint.protocol !== "http:" || endpoint.hostname !== "127.0.0.1" || endpoint.username || endpoint.password || endpoint.pathname !== "/") {
    throw new Error("Server endpoint must be an origin on http://127.0.0.1");
  }
  const selected = [...new Set((input.selectedMediaOrigins ?? []).map((item) => {
    const normalized = normalizeLocation(item);
    if (!normalized) throw new Error(`Invalid selected media origin: ${item}`);
    return normalized.origin;
  }))].slice(0, 100);
  return {
    endpoint: endpoint.origin,
    privacyMode: input.privacyMode,
    // Private reporting is reserved until the server has a separate explicit opt-in.
    includePrivate: false,
    selectedMediaOrigins: selected,
    displayName: String(input.displayName ?? DEFAULT_SETTINGS.displayName).trim().slice(0, 128) || DEFAULT_SETTINGS.displayName,
    failClosed: Boolean(input.failClosed),
  };
}
