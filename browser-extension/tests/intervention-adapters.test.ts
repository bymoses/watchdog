import { describe, expect, test } from "bun:test";
import { adapterFor, genericHtml5Adapter, kickAdapter, twitchAdapter, youtubeAdapter } from "../src/content/adapters";
import { reduceIntervention } from "../src/intervention";
import type { PolicyDecision } from "../src/types";

const decision = (overrides: Partial<PolicyDecision>): PolicyDecision => ({
  policyId: 1, revision: 1, enforcement: "observe", action: "allow",
  usedSeconds: 0, budgetSeconds: 60, remainingSeconds: 60, degraded: false, ...overrides,
});

describe("selected-site adapters", () => {
  test("matches selected sites with domain boundaries", () => {
    expect(youtubeAdapter.matches("https://www.youtube.com")).toBeTrue();
    expect(youtubeAdapter.matches("https://notyoutube.com")).toBeFalse();
    expect(adapterFor("https://twitch.tv").id).toBe("twitch");
    expect(twitchAdapter.matches("https://player.twitch.tv")).toBeTrue();
    expect(kickAdapter.matches("https://kick.com")).toBeTrue();
    expect(adapterFor("https://example.com")).toBe(genericHtml5Adapter);
  });
  test("reads generic HTML5 state without content identity", () => {
    const media = { paused: false, ended: false, seeking: false, muted: false, volume: 1, currentTime: 12, duration: 100, playbackRate: 1, pause() {} };
    const document = { querySelectorAll: () => [media], pictureInPictureElement: media } as unknown as Document;
    expect(genericHtml5Adapter.readMediaState(document)).toEqual({ state: "playing", audible: true, pictureInPicture: true, position: 12, duration: 100, playbackRate: 1 });
  });
  test("soft and hard interventions pause every media element", () => {
    let paused = 0;
    const document = { querySelectorAll: () => [{ pause: () => paused++ }, { pause: () => paused++ }] } as unknown as Document;
    genericHtml5Adapter.applyIntervention(decision({ enforcement: "soft-block", action: "soft-block" }), document);
    expect(paused).toBe(2);
  });
});

describe("intervention state machine", () => {
  test("observe and allow remain invisible", () => expect(reduceIntervention(decision({ enforcement: "observe", action: "observe" })).visible).toBeFalse());
  test("warnings are non-blocking", () => expect(reduceIntervention(decision({ enforcement: "warn", action: "warn" }))).toMatchObject({ visible: true, pauseMedia: false, canContinue: true }));
  test("soft block can continue while hard block cannot", () => {
    expect(reduceIntervention(decision({ enforcement: "soft-block", action: "soft-block" }))).toMatchObject({ pauseMedia: true, canContinue: true });
    expect(reduceIntervention(decision({ enforcement: "hard-block", action: "hard-block" }))).toMatchObject({ pauseMedia: true, canContinue: false });
  });
});
