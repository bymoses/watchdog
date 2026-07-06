# bms-watchdog

Rust/Tokio local battery and process monitor. The runtime is a single host binary; Docker is only used to run Cargo/Rust tooling during development.

![Dashboard screenshot](docs/assets/dashboard.png)

## Build/check

```bash
./scripts/cargo-docker.sh check
./scripts/cargo-docker.sh test
./scripts/cargo-docker.sh build --release
```

## Run

```bash
./target/release/bms-watchdog start
# UI: http://127.0.0.1:24923
```

Useful options:

```bash
./target/release/bms-watchdog start --no-serve
./target/release/bms-watchdog start --once --force-collect
./target/release/bms-watchdog start --port 24923 --data-dir ~/.local/share/bms-watchdog
./target/release/bms-watchdog start --redact-browser-titles
```

Private/incognito browser focused-window titles are redacted by default. USB-C/typec power-role and USB power-supply context is recorded when the kernel exposes it.

Retention defaults are tuned for a continuous trace: battery/environment/app aggregate history is kept for 365 days, while detailed per-process snapshots are kept for 2 days.

## Install user service

```bash
./target/release/bms-watchdog install systemctl-unit --port 24923
```

For tests or previewing the unit without touching systemd:

```bash
./target/release/bms-watchdog install systemctl-unit --dry-run --unit-dir /tmp/systemd-user
```

## Migrations

Default SQLite migrations live in `migrations/` and are embedded into the binary. They are applied automatically on startup and recorded in `schema_migrations`.

To migrate an old database into a new data dir:

```bash
./target/release/bms-watchdog migrate old-db \
  --source /home/user/workspace/bms/devices/battery-monitor/data/battery-monitor.sqlite \
  --data-dir ~/.local/share/bms-watchdog
```

Use `--replace` only when you want to delete the destination DB first.

## Tests

```bash
./scripts/cargo-docker.sh test
bun run test:e2e
```
