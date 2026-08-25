type Listener = (...args: any[]) => unknown;

export interface StorageArea {
  get(keys?: string | string[] | Record<string, unknown> | null): Promise<Record<string, any>>;
  set(values: Record<string, unknown>): Promise<void>;
  remove(keys: string | string[]): Promise<void>;
}

export interface WebExtensionApi {
  runtime: any;
  tabs: any;
  windows: any;
  storage: { local: StorageArea };
  alarms: any;
  webNavigation: any;
  permissions?: any;
  scripting?: any;
}

function promisifyChrome<T>(fn: (...args: any[]) => void, context: unknown, args: unknown[]): Promise<T> {
  return new Promise((resolve, reject) => {
    fn.call(context, ...args, (result: T) => {
      const error = (globalThis as any).chrome?.runtime?.lastError;
      if (error) reject(new Error(error.message));
      else resolve(result);
    });
  });
}

function wrapChrome(raw: any): WebExtensionApi {
  const storage = raw.storage.local;
  const storageGet = storage.get;
  const storageSet = storage.set;
  const storageRemove = storage.remove;
  storage.get = (...args: unknown[]) => promisifyChrome(storageGet, storage, args);
  storage.set = (...args: unknown[]) => promisifyChrome(storageSet, storage, args);
  storage.remove = (...args: unknown[]) => promisifyChrome(storageRemove, storage, args);
  for (const owner of [raw.tabs, raw.windows, raw.permissions, raw.scripting].filter(Boolean)) {
    for (const method of ["get", "getLastFocused", "query", "sendMessage", "create", "request", "contains", "executeScript"]) {
      if (typeof owner[method] === "function") {
        const original = owner[method];
        owner[method] = (...args: unknown[]) => promisifyChrome(original, owner, args);
      }
    }
  }
  const openOptionsPage = raw.runtime.openOptionsPage;
  raw.runtime.openOptionsPage = () => promisifyChrome(openOptionsPage, raw.runtime, []);
  return raw as WebExtensionApi;
}

export function getWebExtensionApi(): WebExtensionApi {
  const root = globalThis as any;
  if (root.browser) return root.browser as WebExtensionApi;
  if (root.chrome) return wrapChrome(root.chrome);
  throw new Error("WebExtension API unavailable");
}

export function addListener(event: { addListener: (listener: Listener, ...rest: unknown[]) => void } | undefined, listener: Listener, ...rest: unknown[]): void {
  event?.addListener(listener, ...rest);
}
