import { reduceIntervention, type InterventionState } from "../intervention";
import type { PolicyDecision } from "../types";

const HOST_ID = "bms-watchdog-intervention";

export class InterventionOverlay {
  private host: HTMLElement | undefined;
  private state: InterventionState | undefined;

  constructor(private readonly send: (message: unknown) => void) {}

  apply(decision: PolicyDecision): InterventionState {
    const next = reduceIntervention(decision);
    this.state = next;
    if (!next.visible) {
      this.remove();
      return next;
    }
    this.render(next, decision);
    return next;
  }

  remove(): void {
    this.host?.remove();
    this.host = undefined;
  }

  private render(state: InterventionState, decision: PolicyDecision): void {
    this.remove();
    const host = document.createElement("div");
    host.id = HOST_ID;
    host.style.cssText = state.enforcement === "warn"
      ? "all:initial;position:fixed;z-index:2147483647;left:16px;right:16px;top:16px"
      : "all:initial;position:fixed;z-index:2147483647;inset:0";
    const shadow = host.attachShadow({ mode: "closed" });
    const panel = document.createElement("section");
    panel.className = `panel ${state.enforcement}`;
    panel.innerHTML = `
      <style>
        :host{color-scheme:dark;font-family:system-ui,-apple-system,sans-serif}
        .panel{box-sizing:border-box;background:#17181c;color:#f5f6f8;border:1px solid #3a3d46;border-radius:14px;padding:20px;box-shadow:0 16px 60px #0009;max-width:620px;margin:auto}
        .soft-block,.hard-block{position:fixed;inset:0;max-width:none;border:0;border-radius:0;display:grid;place-content:center;text-align:center;background:#101115f2;padding:10vw}
        h1{font-size:20px;margin:0 0 8px}p{font-size:15px;line-height:1.5;margin:0 0 16px;color:#d4d7df}.remaining{font-variant-numeric:tabular-nums}
        .actions{display:flex;gap:10px;justify-content:center}button{border:0;border-radius:9px;padding:10px 16px;font:600 14px system-ui;cursor:pointer;background:#6d7cff;color:white}button.secondary{background:#30333b}button:focus-visible{outline:3px solid #aeb7ff}
      </style>
      <div><h1>${escapeHtml(titleFor(state))}</h1><p>${escapeHtml(state.message)}</p>
      ${decision.remainingSeconds !== undefined ? `<p class="remaining">${Math.ceil(decision.remainingSeconds / 60)} minutes remaining</p>` : ""}
      <div class="actions"></div></div>`;
    const actions = panel.querySelector(".actions")!;
    if (state.canContinue) actions.append(this.button("Continue", "continue", decision));
    if (state.policyId) actions.append(this.button("Snooze", "snooze", decision, "secondary"));
    shadow.append(panel);
    (document.documentElement ?? document.body).append(host);
    this.host = host;
  }

  private button(label: string, action: string, decision: PolicyDecision, className = ""): HTMLButtonElement {
    const button = document.createElement("button");
    button.textContent = label;
    button.className = className;
    button.addEventListener("click", () => {
      this.send({ kind: "intervention-action", action, policyId: decision.policyId, revision: decision.revision });
      if (action === "continue") this.remove();
    });
    return button;
  }
}

function titleFor(state: InterventionState): string {
  if (state.enforcement === "warn") return "Attention budget reminder";
  if (state.enforcement === "soft-block") return "Take a deliberate pause";
  return "This site is blocked by your policy";
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"]/g, (char) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[char]!);
}
