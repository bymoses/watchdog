import { describe, expect, test } from "bun:test";
import { heartbeatDue, heartbeatIntervalMs, reduceTabState } from "../src/state";
import { observation } from "./helpers";

describe("tab/window state reducer with browser API-shaped observations", () => {
  test("first observation is a recovery snapshot", () => {
    const result = reduceTabState(undefined, observation(), "snapshot", 100);
    expect(result.eventType).toBe("snapshot");
    expect(result.material).toBeTrue();
  });
  test("unchanged updates do not emit transitions", () => {
    const first = reduceTabState(undefined, observation(), "snapshot", 100);
    const second = reduceTabState(first.next, observation(), "audible", 101);
    expect(second.eventType).toBeUndefined();
    expect(second.next.lastEmittedAt).toBe(100);
  });
  test("classifies focus, activation, visibility, and navigation", () => {
    const first = reduceTabState(undefined, observation(), "snapshot", 1).next;
    expect(reduceTabState(first, observation({ browserFocused: false }), "window-focus").eventType).toBe("window_focus");
    expect(reduceTabState(first, observation({ tabActive: false }), "tab-active").eventType).toBe("active_tab");
    expect(reduceTabState(first, observation({ documentVisible: false }), "visibility").eventType).toBe("visibility");
    expect(reduceTabState(first, observation({ url: "https://twitch.tv/something" }), "navigation").eventType).toBe("navigation");
  });
  test("classifies media lifecycle", () => {
    const first = reduceTabState(undefined, observation(), "snapshot").next;
    const playing = observation({ media: { state: "playing", audible: true, pictureInPicture: false }, audible: true });
    const playingState = reduceTabState(first, playing, "media").next;
    expect(reduceTabState(first, playing, "media").eventType).toBe("media_play");
    expect(reduceTabState(playingState, observation({ media: { state: "paused", audible: false, pictureInPicture: false } }), "media").eventType).toBe("media_pause");
    expect(reduceTabState(playingState, observation({ media: { state: "ended", audible: false, pictureInPicture: false } }), "media").eventType).toBe("media_end");
  });
  test("heartbeats emit without becoming material", () => {
    const first = reduceTabState(undefined, observation(), "snapshot", 1).next;
    const result = reduceTabState(first, observation(), "heartbeat", 50);
    expect(result).toMatchObject({ eventType: "heartbeat", material: false });
  });
  test("all eligible states heartbeat comfortably inside the lease", () => {
    const active = reduceTabState(undefined, observation({ media: { state: "playing", audible: false, pictureInPicture: false } }), "snapshot", 1_000).next;
    const simple = reduceTabState(undefined, observation(), "snapshot", 1_000).next;
    expect(heartbeatIntervalMs(active)).toBe(20_000);
    expect(heartbeatIntervalMs(simple)).toBe(20_000);
    expect(heartbeatDue(active, 21_000)).toBeTrue();
    expect(heartbeatDue(simple, 20_999)).toBeFalse();
  });
});
