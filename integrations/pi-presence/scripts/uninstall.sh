#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
if ! command -v pi >/dev/null 2>&1; then
  printf 'pi is not installed or not on PATH\n' >&2
  exit 1
fi

pi remove "$ROOT"
printf 'Removed bms-watchdog Pi presence package reference for %s\n' "$ROOT"
