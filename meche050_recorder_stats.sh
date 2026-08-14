#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$ROOT/target/release/meche050_recorder_stats"
CONFIG="${MECHE050_STATS_CONFIG:-configs/meche050_forward.env}"

if [[ "$CONFIG" != /* ]]; then
  CONFIG="$ROOT/$CONFIG"
fi
if [[ -f "$CONFIG" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$CONFIG"
  set +a
fi

if [[ ! -x "$BIN" ]]; then
  (cd "$ROOT" && "$HOME/.cargo/bin/cargo" build --locked --release --bin meche050_recorder_stats)
fi

cd "$ROOT"
exec "$BIN" --logs-dir "${MECHE050_STATS_LOGS_DIR:-logs/meche050-forward}" "$@"
