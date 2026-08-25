export {};
const api = (globalThis as any).browser ?? (globalThis as any).chrome;
const byId = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;
let current: any;

async function send(message: unknown): Promise<any> {
  const response = (globalThis as any).browser
    ? await api.runtime.sendMessage(message)
    : await new Promise((resolve, reject) => api.runtime.sendMessage(message, (value: unknown) => {
      const error = api.runtime.lastError;
      if (error) reject(new Error(error.message)); else resolve(value);
    }));
  if (response?.error) throw new Error(String(response.error));
  return response;
}

async function refresh(): Promise<void> {
  current = await send({ kind: "get-status" });
  const settings = current.settings;
  byId<HTMLInputElement>("endpoint").value = settings.endpoint;
  byId<HTMLInputElement>("displayName").value = settings.displayName;
  byId<HTMLSelectElement>("privacyMode").value = settings.privacyMode;
  byId<HTMLInputElement>("failClosed").checked = settings.failClosed;
  byId("connection").textContent = current.paired ? `Paired client ${current.clientId}` : "Not paired";
  byId<HTMLButtonElement>("revoke").disabled = !current.paired;
  renderOrigins(settings.selectedMediaOrigins);
  byId("preview").textContent = JSON.stringify(current.preview ?? { observation: "none" }, null, 2);
  const values = {
    "Queue depth": `${current.queue.depth} events (${current.queue.bytes} bytes)`,
    "Last successful sync": current.queue.lastSuccessfulSync ? new Date(current.queue.lastSuccessfulSync).toLocaleString() : "Never",
    "Dropped/compacted": String(current.queue.dropped),
    "Policy revision": String(current.policy.revision),
    "Cached policies": String(current.policy.count),
  };
  byId("health").innerHTML = Object.entries(values).map(([key, value]) => `<dt>${escapeHtml(key)}</dt><dd>${escapeHtml(value)}</dd>`).join("");
}

function settingsFromForm(): any {
  return {
    ...current.settings,
    endpoint: byId<HTMLInputElement>("endpoint").value,
    displayName: byId<HTMLInputElement>("displayName").value,
    privacyMode: byId<HTMLSelectElement>("privacyMode").value,
    failClosed: byId<HTMLInputElement>("failClosed").checked,
  };
}

function renderOrigins(origins: string[]): void {
  const list = byId("origins");
  list.replaceChildren(...origins.map((origin) => {
    const item = document.createElement("li");
    const text = document.createElement("span");
    text.textContent = origin;
    const remove = document.createElement("button");
    remove.textContent = "Remove";
    remove.className = "danger";
    remove.addEventListener("click", () => run(async () => { await send({ kind: "remove-media-origin", origin }); await refresh(); }));
    item.append(text, remove);
    return item;
  }));
}

byId("save").addEventListener("click", () => run(async () => { await send({ kind: "save-settings", settings: settingsFromForm() }); await refresh(); }));
byId("pair").addEventListener("click", () => run(async () => { await send({ kind: "save-settings", settings: settingsFromForm() }); await send({ kind: "pair", code: byId<HTMLInputElement>("pairCode").value.trim() }); byId<HTMLInputElement>("pairCode").value = ""; await refresh(); }));
byId("revoke").addEventListener("click", () => run(async () => { if (confirm("Revoke this browser client and delete its local token and queue?")) { await send({ kind: "revoke" }); await refresh(); } }));
byId("sync").addEventListener("click", () => run(async () => { await send({ kind: "sync" }); await refresh(); }));
byId("addOrigin").addEventListener("click", () => run(async () => { const origin = byId<HTMLInputElement>("mediaOrigin").value; const result = await send({ kind: "request-media-permission", origin }); if (!result.granted) throw new Error("Host permission was not granted"); byId<HTMLInputElement>("mediaOrigin").value = ""; await refresh(); }));

async function run(work: () => Promise<void>): Promise<void> {
  byId("error").textContent = "";
  try { await work(); } catch (error) { byId("error").textContent = error instanceof Error ? error.message : "Operation failed"; }
}
function escapeHtml(value: string): string { return value.replace(/[&<>"]/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[char]!); }
void run(refresh);
