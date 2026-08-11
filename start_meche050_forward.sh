#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

export MECHE050_CONFIG="configs/meche050_forward.env"
export MECHE050_ENABLED_CONFIG="configs/meche050_forward_enabled.env"
export MECHE050_RUNTIME_LOGS="logs/meche050-forward"

exec "$ROOT/start_meche050.sh" "$@"
