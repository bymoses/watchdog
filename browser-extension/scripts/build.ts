import { cp, mkdir, rm } from "node:fs/promises";
import { join } from "node:path";

const root = join(import.meta.dir, "..");
const dist = join(root, "dist");
await rm(dist, { recursive: true, force: true });

const entries = [
  ["src/background.ts", "background.js"],
  ["src/content/main.ts", "content.js"],
  ["src/options/options.ts", "options/options.js"],
  ["src/blocked/blocked.ts", "blocked/blocked.js"],
] as const;

for (const target of ["firefox", "chromium"] as const) {
  const outdir = join(dist, target);
  await mkdir(join(outdir, "options"), { recursive: true });
  await mkdir(join(outdir, "blocked"), { recursive: true });
  for (const [entry, output] of entries) {
    const result = await Bun.build({
      entrypoints: [join(root, entry)],
      outdir: join(outdir, output.includes("/") ? output.split("/")[0]! : "."),
      naming: output.includes("/") ? output.split("/")[1]! : output,
      target: "browser",
      format: "iife",
      minify: false,
      sourcemap: "none",
    });
    if (!result.success) {
      for (const log of result.logs) console.error(log);
      process.exitCode = 1;
      throw new Error(`Failed to build ${entry} for ${target}`);
    }
  }
  await Bun.write(join(outdir, "manifest.json"), `${JSON.stringify(await Bun.file(join(root, "manifests", `${target}.json`)).json(), null, 2)}\n`);
  await cp(join(root, "src/options/index.html"), join(outdir, "options/index.html"));
  await cp(join(root, "src/options/styles.css"), join(outdir, "options/styles.css"));
  await cp(join(root, "src/blocked/index.html"), join(outdir, "blocked/index.html"));
  await cp(join(root, "src/blocked/styles.css"), join(outdir, "blocked/styles.css"));
}

console.log("Built Firefox and Chromium WebExtensions in dist/");
