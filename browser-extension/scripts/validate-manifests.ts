import { exists } from "node:fs/promises";
import { join } from "node:path";

const root = join(import.meta.dir, "..");
const errors: string[] = [];

for (const target of ["firefox", "chromium"] as const) {
  const sourcePath = join(root, "manifests", `${target}.json`);
  const manifest = await Bun.file(sourcePath).json() as Record<string, any>;
  const prefix = `${target}:`;
  if (manifest.name !== "BMS Watchdog") errors.push(`${prefix} unexpected name`);
  if (!/^\d+\.\d+\.\d+$/.test(manifest.version)) errors.push(`${prefix} invalid version`);
  if (target === "firefox" && manifest.manifest_version !== 2) errors.push(`${prefix} must use MV2 for Zen/Firefox compatibility`);
  if (target === "chromium" && manifest.manifest_version !== 3) errors.push(`${prefix} must use MV3`);

  const requiredOrigins = [...(manifest.permissions ?? []), ...(manifest.host_permissions ?? [])].filter((item: string) => item.includes("://"));
  if (requiredOrigins.some((item: string) => item !== "http://127.0.0.1/*")) errors.push(`${prefix} non-loopback required host permission`);
  const optional = [...(manifest.optional_permissions ?? []), ...(manifest.optional_host_permissions ?? [])];
  if (!optional.includes("<all_urls>")) errors.push(`${prefix} selected-site access must be optional`);
  if (manifest.content_scripts) errors.push(`${prefix} content scripts must be dynamically injected only after optional permission`);
  if (JSON.stringify(manifest).includes("http://*/*") || JSON.stringify(manifest).includes("https://*/*")) errors.push(`${prefix} broad required host access`);

  const distDir = join(root, "dist", target);
  if (await exists(distDir)) {
    for (const file of ["manifest.json", "background.js", "content.js", "options/index.html", "options/options.js", "blocked/index.html", "blocked/blocked.js"]) {
      if (!(await exists(join(distDir, file)))) errors.push(`${prefix} generated file missing: ${file}`);
    }
    const generated = await Bun.file(join(distDir, "manifest.json")).json();
    if (JSON.stringify(generated) !== JSON.stringify(manifest)) errors.push(`${prefix} generated manifest differs from source`);
  }
}

if (errors.length) {
  for (const error of errors) console.error(error);
  process.exit(1);
}
console.log("Firefox and Chromium manifests are valid");
