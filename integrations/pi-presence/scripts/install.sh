#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
if ! command -v pi >/dev/null 2>&1; then
  printf 'pi is not installed or not on PATH\n' >&2
  exit 1
fi

pi install "$ROOT"
printf 'Installed bms-watchdog Pi presence from %s\n' "$ROOT"
