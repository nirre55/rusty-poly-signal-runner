#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$ROOT/target/release/meche050_recorder_stats"

if [[ ! -x "$BIN" ]]; then
  "$HOME/.cargo/bin/cargo" build --locked --release --bin meche050_recorder_stats
fi

cd "$ROOT"
exec "$BIN" --logs-dir "${MECHE050_STATS_LOGS_DIR:-logs/meche050-forward}" "$@"
