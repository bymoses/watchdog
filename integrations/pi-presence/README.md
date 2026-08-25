# bms-watchdog Pi presence

Optional Pi extension that publishes the currently active native Pi session as a short-lived, same-user runtime lease. It reports only lifecycle presence; it does not read or write conversation entries.

## Data contract

Each active Pi process owns one atomic JSON file in:

```text
$XDG_RUNTIME_DIR/bms-watchdog/agent-presence/pi-<pid>-<instance>.json
```

The directory is `0700` and lease files are `0600`. A lease contains only:

- schema/event/process instance identifiers;
- harness (`pi`) and source (`local`);
- PID and Linux process start ticks when available;
- Pi's exact current `sessionManager.getSessionId()`;
- working directory;
- `idle` or `busy` state;
- observed and expiry timestamps.

It never contains prompts, messages, assistant/thinking content, session entries, model payloads, commands, tool names, tool arguments, or tool results. Normal failures are non-fatal and diagnostics are sanitized and rate-limited.

## Lifecycle

- `session_start`: starts the heartbeat and writes `idle`.
- `agent_start`: writes `busy`.
- `agent_settled`: writes `idle` after retries, compaction, and queued continuation settle.
- `session_shutdown`: stops the heartbeat and removes the lease idempotently.

Pi reloads the extension runtime for `/new`, `/resume`, `/fork`, and `/clone`, so the replacement session publishes its own exact native session ID. A crash is handled by the 45-second TTL.

## Install and manage

Requires Pi with package support and Bun for package tests.

```bash
cd integrations/pi-presence
bun test
bun run pi:install
bun run pi:status
bun run pi:uninstall
```

The install script runs `pi install` with this package's absolute local path. It does not copy source or modify Pi session files. Restart Pi after installation, or use `/reload` in an active Pi session.

For a one-off test without installation:

```bash
pi -e ./integrations/pi-presence/index.ts
```
