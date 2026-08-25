#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
if ! command -v pi >/dev/null 2>&1; then
  printf 'not installed: pi is not on PATH\n'
  exit 1
fi

LIST="$(pi list)"
if grep -F -- "$ROOT" <<<"$LIST" >/dev/null; then
  printf 'installed: %s\n' "$ROOT"
  exit 0
fi

printf 'not installed: %s\n' "$ROOT"
exit 1
