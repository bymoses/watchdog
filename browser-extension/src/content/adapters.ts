import { domainMatches, normalizeLocation } from "../privacy";
import type { MediaState, PolicyDecision } from "../types";

export interface SiteAdapter {
  id: string;
  matches(origin: string): boolean;
  readMediaState(document: Document): MediaState | undefined;
  applyIntervention(decision: PolicyDecision, document: Document): void;
}

function readElements(document: Document, selectors: string[]): MediaState | undefined {
  const seen = new Set<HTMLMediaElement>();
  const elements: HTMLMediaElement[] = [];
  for (const selector of selectors) {
    for (const element of document.querySelectorAll<HTMLMediaElement>(selector)) {
      if (!seen.has(element)) {
        seen.add(element);
        elements.push(element);
      }
    }
  }
  if (!elements.length) return undefined;
  const media = elements.find((item) => !item.paused && !item.ended) ?? elements[0]!;
  let state: MediaState["state"] = "paused";
  if (media.ended) state = "ended";
  else if (media.seeking) state = "seeking";
  else if (!media.paused) state = "playing";
  return {
    state,
    audible: !media.muted && media.volume > 0 && state === "playing",
    pictureInPicture: document.pictureInPictureElement === media,
    ...(Number.isFinite(media.currentTime) ? { position: media.currentTime } : {}),
    ...(Number.isFinite(media.duration) ? { duration: media.duration } : {}),
    ...(Number.isFinite(media.playbackRate) ? { playbackRate: media.playbackRate } : {}),
  };
}

function pauseForDecision(decision: PolicyDecision, document: Document): void {
  if (decision.action !== "soft-block" && decision.action !== "hard-block") return;
  for (const media of document.querySelectorAll<HTMLMediaElement>("video,audio")) {
    media.pause();
  }
}

function originDomain(origin: string): string {
  return normalizeLocation(origin)?.domain ?? "";
}

export const genericHtml5Adapter: SiteAdapter = {
  id: "generic-html5",
  matches: (origin) => Boolean(normalizeLocation(origin)),
  readMediaState: (document) => readElements(document, ["video", "audio"]),
  applyIntervention: pauseForDecision,
};

export const youtubeAdapter: SiteAdapter = {
  id: "youtube",
  matches: (origin) => domainMatches(originDomain(origin), "youtube.com") || domainMatches(originDomain(origin), "youtu.be"),
  readMediaState: (document) => readElements(document, ["#movie_player video", "ytd-player video", "video.html5-main-video", "video"]),
  applyIntervention: pauseForDecision,
};

export const twitchAdapter: SiteAdapter = {
  id: "twitch",
  matches: (origin) => domainMatches(originDomain(origin), "twitch.tv"),
  readMediaState: (document) => readElements(document, ["[data-a-target='video-player'] video", ".video-player video", "video"]),
  applyIntervention: pauseForDecision,
};

export const kickAdapter: SiteAdapter = {
  id: "kick",
  matches: (origin) => domainMatches(originDomain(origin), "kick.com"),
  readMediaState: (document) => readElements(document, ["#video-player video", "[data-testid='video-player'] video", "video"]),
  applyIntervention: pauseForDecision,
};

export const siteAdapters: SiteAdapter[] = [youtubeAdapter, twitchAdapter, kickAdapter, genericHtml5Adapter];

export function adapterFor(origin: string): SiteAdapter {
  return siteAdapters.find((adapter) => adapter.id !== "generic-html5" && adapter.matches(origin)) ?? genericHtml5Adapter;
}
