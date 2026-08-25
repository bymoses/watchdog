# BMS Watchdog WebExtension

Zen/Firefox-first, Chromium-portable browser telemetry and attention-policy client.

## Privacy

The default mode is **domains only**. The queue and transport never store or send full URLs, query strings, fragments, page text, forms, cookies, request contents, or media IDs. Titles require an explicit privacy mode and are off by default. Private-window observations are excluded in v1. Media access requires an optional host permission and an explicitly selected origin.

## Commands

```sh
bun install
bun test
bun run check
bun run build
bun run validate:manifests
```

Unpacked builds are written to `dist/firefox` and `dist/chromium`.

## Pairing

Open the extension options, confirm the loopback endpoint, and enter the short-lived code shown by the watchdog dashboard. The returned bearer token remains in extension-local storage. Revocation deletes it locally and asks the server to revoke the client.

Policy enforcement starts fail-open and observe-only unless the user and server explicitly configure a stronger mode.
