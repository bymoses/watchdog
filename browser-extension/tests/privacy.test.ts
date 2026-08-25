import { describe, expect, test } from "bun:test";
import { assertNoFullUrls, domainMatches, makeBrowserEvent, mediaAllowed, normalizeLocation } from "../src/privacy";
import { observation, settings } from "./helpers";

describe("privacy normalization", () => {
  test("retains only normalized hostname and origin", () => {
    expect(normalizeLocation("HTTPS://WWW.YouTube.com:443/watch?v=secret#x")).toEqual({ domain: "www.youtube.com", origin: "https://www.youtube.com" });
  });
  test("normalizes IDNs through URL parsing", () => {
    expect(normalizeLocation("https://bücher.example/path")?.domain).toBe("xn--bcher-kva.example");
  });
  test("rejects internal and malformed schemes", () => {
    expect(normalizeLocation("about:privatebrowsing")).toBeUndefined();
    expect(normalizeLocation("file:///etc/passwd")).toBeUndefined();
    expect(normalizeLocation("not a url")).toBeUndefined();
  });
  test("uses label-boundary domain matching", () => {
    expect(domainMatches("music.youtube.com", "youtube.com")).toBeTrue();
    expect(domainMatches("notyoutube.com", "youtube.com")).toBeFalse();
  });
  test("domain-only events omit title, full URL, and media", () => {
    const event = makeBrowserEvent(observation({ audible: true, media: { state: "playing", audible: true, pictureInPicture: false, position: 42 } }), "snapshot", settings(), { clientId: "c", sequence: 1 }, 5, "e");
    expect(event).toMatchObject({ domain: "www.youtube.com", origin: "https://www.youtube.com", mediaState: "none", audible: false });
    expect(event).not.toHaveProperty("title");
    expect(event).not.toHaveProperty("mediaPosition");
    expect(JSON.stringify(event)).not.toContain("watch?v");
    expect(() => assertNoFullUrls(event)).not.toThrow();
  });
  test("titles are bounded and explicit", () => {
    const event = makeBrowserEvent(observation({ title: `\u0000 ${"x".repeat(700)}` }), "snapshot", settings({ privacyMode: "domains-and-titles" }), { clientId: "c", sequence: 1 });
    expect(event?.title?.length).toBe(512);
    expect(event?.title).not.toContain("\u0000");
  });
  test("media details require both mode and selected origin", () => {
    const media = { state: "playing" as const, audible: true, pictureInPicture: false, position: 10, duration: 20, playbackRate: 1 };
    const allowed = makeBrowserEvent(observation({ media, audible: true }), "media_play", settings({ privacyMode: "selected-sites-media", selectedMediaOrigins: ["https://www.youtube.com/foo"] }), { clientId: "c", sequence: 1 });
    const denied = makeBrowserEvent(observation({ media, audible: true }), "media_play", settings({ privacyMode: "selected-sites-media", selectedMediaOrigins: ["https://twitch.tv"] }), { clientId: "c", sequence: 1 });
    expect(allowed).toMatchObject({ mediaState: "playing", mediaPosition: 10, audible: true });
    expect(denied).toMatchObject({ mediaState: "none", audible: false });
    expect(mediaAllowed("https://www.youtube.com", ["https://www.youtube.com/path"])).toBeTrue();
  });
  test("off and private observations emit nothing", () => {
    expect(makeBrowserEvent(observation(), "snapshot", settings({ privacyMode: "off" }), { clientId: "c", sequence: 1 })).toBeUndefined();
    expect(makeBrowserEvent(observation({ private: true }), "snapshot", settings({ includePrivate: true }), { clientId: "c", sequence: 1 })).toBeUndefined();
  });
  test("payload invariant catches accidental paths", () => {
    expect(() => assertNoFullUrls({ url: "https://example.com/private?q=1" })).toThrow();
  });
});
