export {};
const api = (globalThis as any).browser ?? (globalThis as any).chrome;
document.querySelector("#options")?.addEventListener("click", () => api.runtime.openOptionsPage());
