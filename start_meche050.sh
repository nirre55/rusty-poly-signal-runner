#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

resolve_root_path() {
  if [[ "$1" == /* ]]; then
    printf '%s\n' "$1"
  else
    printf '%s/%s\n' "$ROOT" "$1"
  fi
}

config_value() {
  local key="$1"
  local path="$2"
  [[ -f "$path" ]] || return 0
  awk -v key="$key" '
    index($0, key "=") == 1 {
      value = substr($0, length(key) + 2)
      sub(/\r$/, "", value)
      print value
      exit
    }
  ' "$path"
}

CONFIG_PATH="$(resolve_root_path "${MECHE050_CONFIG:-configs/meche050_portfolio.env}")"
CONFIG_ENABLED="$(config_value PORTFOLIO_ENABLED_CONFIG "$CONFIG_PATH")"
CONFIG_LOGS="$(config_value LOGS_DIR "$CONFIG_PATH")"
ENABLED_PATH="$(resolve_root_path "${MECHE050_ENABLED_CONFIG:-${CONFIG_ENABLED:-configs/meche050_enabled.env}}")"
RUNTIME_LOGS="${MECHE050_RUNTIME_LOGS:-${CONFIG_LOGS:-logs/meche050}}"
SUPERVISOR_DIR="$(resolve_root_path "$RUNTIME_LOGS")/supervisor"
PID_FILE="$SUPERVISOR_DIR/portfolio_runner.pid"
LOG_FILE="$SUPERVISOR_DIR/portfolio_runner.console.log"
CARGO_PROFILE="${CARGO_PROFILE:-release}"
RUST_LOG="${RUST_LOG:-info}"
RESTART_DELAY_SECONDS="${RESTART_DELAY_SECONDS:-15}"

STRATEGIES=(boll_fade streak_rsi trio_vote2 reversal_pro)
MARKETS=(btc_5m eth_5m btc_15m eth_15m)

usage() {
  cat <<'EOF'
Usage:
  ./start_meche050.sh start
  ./start_meche050.sh stop
  ./start_meche050.sh status
  ./start_meche050.sh restart
  ./start_meche050.sh strategy status
  ./start_meche050.sh strategy all
  ./start_meche050.sh strategy enable <strategy> [btc_5m|eth_5m|btc_15m|eth_15m]
  ./start_meche050.sh strategy disable <strategy> [btc_5m|eth_5m|btc_15m|eth_15m]
  ./start_meche050.sh strategy only <strategy> [btc_5m|eth_5m|btc_15m|eth_15m]

Strategies: boll_fade, streak_rsi, trio_vote2, reversal_pro

The strategy commands atomically rewrite the selected activation grid, then restart
the selected shared runner. Existing state remains in its configured LOGS_DIR.
EOF
}

binary_path() {
  if [[ "$CARGO_PROFILE" == "debug" ]]; then
    printf '%s\n' "$ROOT/target/debug/portfolio_runner"
  else
    printf '%s\n' "$ROOT/target/release/portfolio_runner"
  fi
}

is_running() {
  [[ -n "${1:-}" ]] && kill -0 "$1" 2>/dev/null
}

build_once() {
  if [[ "$CARGO_PROFILE" == "debug" ]]; then
    cargo build --locked --bin portfolio_runner
  else
    cargo build --locked --release --bin portfolio_runner
  fi
}

supervise() {
  set +e
  local config_path="$1"
  local binary="$2"
  export STRATEGY_CONFIG="$config_path"
  export RUST_LOG
  cd "$ROOT"

  while true; do
    "$binary"
    local exit_code=$?
    printf '[%s] portfolio_runner exited code=%s\n' "$(date -Is)" "$exit_code"
    if [[ "${NO_RESTART:-0}" == "1" ]]; then
      break
    fi
    sleep "$RESTART_DELAY_SECONDS"
  done
}

start_runner() {
  mkdir -p "$SUPERVISOR_DIR"
  [[ -f "$CONFIG_PATH" ]] || { echo "Config introuvable: $CONFIG_PATH" >&2; return 1; }
  [[ -f "$ENABLED_PATH" ]] || { echo "Grille introuvable: $ENABLED_PATH" >&2; return 1; }
  if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
    build_once
  fi

  local binary
  binary="$(binary_path)"
  [[ -x "$binary" ]] || { echo "Binaire introuvable: $binary" >&2; return 1; }

  if [[ -f "$PID_FILE" ]] && is_running "$(<"$PID_FILE")"; then
    echo "portfolio_runner déjà actif (pid $(<"$PID_FILE"))"
    return 0
  fi

  nohup bash -c 'supervise "$@"' _ "$CONFIG_PATH" "$binary" >>"$LOG_FILE" 2>&1 &
  echo "$!" >"$PID_FILE"
  echo "portfolio_runner démarré (pid $!, log $LOG_FILE)"
}

stop_runner() {
  if [[ ! -f "$PID_FILE" ]]; then
    echo "portfolio_runner arrêté"
    return 0
  fi

  local pid
  pid="$(<"$PID_FILE")"
  if is_running "$pid"; then
    pkill -TERM -P "$pid" 2>/dev/null || true
    kill -TERM "$pid" 2>/dev/null || true
    sleep 2
    if is_running "$pid"; then
      pkill -KILL -P "$pid" 2>/dev/null || true
      kill -KILL "$pid" 2>/dev/null || true
    fi
    echo "portfolio_runner arrêté (pid $pid)"
  else
    echo "PID périmé supprimé (pid $pid)"
  fi
  rm -f "$PID_FILE"
}

status_runner() {
  if [[ -f "$PID_FILE" ]] && is_running "$(<"$PID_FILE")"; then
    echo "portfolio_runner RUNNING (pid $(<"$PID_FILE"), log $LOG_FILE)"
  else
    echo "portfolio_runner STOPPED (log $LOG_FILE)"
  fi
}

restart_runner() {
  stop_runner
  local binary
  binary="$(binary_path)"
  if [[ ! -x "$binary" ]]; then
    build_once
  fi
  SKIP_BUILD=1 start_runner
}

valid_strategy() {
  local wanted="$1"
  local strategy
  for strategy in "${STRATEGIES[@]}"; do
    [[ "$strategy" == "$wanted" ]] && return 0
  done
  return 1
}

valid_market() {
  local wanted="$1"
  local market
  for market in "${MARKETS[@]}"; do
    [[ "$market" == "$wanted" ]] && return 0
  done
  return 1
}

enabled_key() {
  local strategy="${1^^}"
  local market="${2^^}"
  printf 'MECHE050_ENABLED_%s_%s\n' "$strategy" "$market"
}

rewrite_matrix() {
  [[ -f "$ENABLED_PATH" ]] || { echo "Grille introuvable: $ENABLED_PATH" >&2; return 1; }
  local tmp next update key value
  tmp="$(mktemp "$ENABLED_PATH.tmp.XXXXXX")"
  cp "$ENABLED_PATH" "$tmp"

  for update in "$@"; do
    key="${update%%=*}"
    value="${update#*=}"
    next="$(mktemp "$ENABLED_PATH.tmp.XXXXXX")"
    if ! awk -v key="$key" -v value="$value" '
      BEGIN { found = 0 }
      $0 ~ "^" key "=" { print key "=" value; found = 1; next }
      { print }
      END { if (!found) exit 2 }
    ' "$tmp" >"$next"; then
      rm -f "$tmp" "$next"
      echo "Clé d’activation absente: $key" >&2
      return 1
    fi
    mv "$next" "$tmp"
  done

  mv "$tmp" "$ENABLED_PATH"
}

append_scope_updates() {
  local -n out="$1"
  local strategy="$2"
  local value="$3"
  local market="${4:-}"
  if [[ -n "$market" ]]; then
    out+=("$(enabled_key "$strategy" "$market")=$value")
    return
  fi
  for market in "${MARKETS[@]}"; do
    out+=("$(enabled_key "$strategy" "$market")=$value")
  done
}

matrix_status() {
  [[ -f "$ENABLED_PATH" ]] || { echo "Grille introuvable: $ENABLED_PATH" >&2; return 1; }
  printf '%-16s %-9s %s\n' "strategy" "market" "enabled"
  local strategy market key value
  for strategy in "${STRATEGIES[@]}"; do
    for market in "${MARKETS[@]}"; do
      key="$(enabled_key "$strategy" "$market")"
      value="$(awk -F= -v key="$key" '$1 == key { print $2 }' "$ENABLED_PATH")"
      printf '%-16s %-9s %s\n' "$strategy" "$market" "${value:-MISSING}"
    done
  done
}

strategy_command() {
  local action="${1:-}"
  case "$action" in
    status)
      matrix_status
      return
      ;;
    all)
      local updates=()
      local strategy market
      for strategy in "${STRATEGIES[@]}"; do
        for market in "${MARKETS[@]}"; do
          updates+=("$(enabled_key "$strategy" "$market")=true")
        done
      done
      rewrite_matrix "${updates[@]}"
      ;;
    enable|disable|only)
      local strategy="${2:-}"
      local market="${3:-}"
      valid_strategy "$strategy" || { echo "Stratégie invalide: $strategy" >&2; return 2; }
      [[ -z "$market" ]] || valid_market "$market" || { echo "Marché invalide: $market" >&2; return 2; }
      local updates=()
      if [[ "$action" == "only" ]]; then
        local other_strategy other_market
        for other_strategy in "${STRATEGIES[@]}"; do
          for other_market in "${MARKETS[@]}"; do
            updates+=("$(enabled_key "$other_strategy" "$other_market")=false")
          done
        done
        append_scope_updates updates "$strategy" true "$market"
      elif [[ "$action" == "enable" ]]; then
        append_scope_updates updates "$strategy" true "$market"
      else
        append_scope_updates updates "$strategy" false "$market"
      fi
      rewrite_matrix "${updates[@]}"
      ;;
    *)
      usage >&2
      return 2
      ;;
  esac

  restart_runner
  matrix_status
}

export -f supervise
export ROOT RUST_LOG RESTART_DELAY_SECONDS

case "${1:-start}" in
  start) start_runner ;;
  stop) stop_runner ;;
  status) status_runner ;;
  restart) restart_runner ;;
  strategy) shift; strategy_command "$@" ;;
  help|-h|--help) usage ;;
  *) usage >&2; exit 2 ;;
esac
