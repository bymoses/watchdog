import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { PresenceLeaseWriter } from "./presence.ts";

export default function piPresenceExtension(pi: ExtensionAPI): void {
  const writer = new PresenceLeaseWriter();

  pi.on("session_start", async (_event, ctx) => {
    await writer.start(ctx);
  });

  pi.on("agent_start", async (_event, ctx) => {
    await writer.setState("busy", ctx);
  });

  pi.on("agent_settled", async (_event, ctx) => {
    await writer.setState("idle", ctx);
  });

  // The display name is deliberately not written. This hook only refreshes the
  // exact native ID in case a future Pi lifecycle changes metadata in place.
  pi.on("session_info_changed", async (_event, ctx) => {
    await writer.refresh(ctx);
  });

  pi.on("session_shutdown", async (_event, _ctx: ExtensionContext) => {
    await writer.stop();
  });
}
