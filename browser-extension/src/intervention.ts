import type { Enforcement, PolicyDecision } from "./types";

export interface InterventionState {
  visible: boolean;
  enforcement: Enforcement;
  canContinue: boolean;
  pauseMedia: boolean;
  message: string;
  policyId?: number;
}

export function reduceIntervention(decision: PolicyDecision): InterventionState {
  if (decision.action === "allow" || decision.action === "observe" || decision.enforcement === "observe") {
    return { visible: false, enforcement: decision.enforcement, canContinue: true, pauseMedia: false, message: "" };
  }
  if (decision.action === "warn") {
    return { visible: true, enforcement: "warn", canContinue: true, pauseMedia: false, message: decision.reason ?? "Your configured attention budget is nearly exhausted.", policyId: decision.policyId };
  }
  const hard = decision.action === "hard-block";
  return { visible: true, enforcement: hard ? "hard-block" : "soft-block", canContinue: !hard, pauseMedia: true, message: decision.reason ?? "Your configured attention budget is exhausted.", policyId: decision.policyId };
}
