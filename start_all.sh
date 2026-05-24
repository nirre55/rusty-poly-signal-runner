#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

CONFIGS=(
  "configs/btc_combined.env"
  "configs/eth_ensemble.env"
  "configs/btc_15m_ensemble.env"
  "configs/eth_15m_ensemble.env"
  "configs/btc_1h_ensemble.env"
  "configs/eth_1h_ensemble.env"
)

SUPERVISOR_DIR="$ROOT/logs/supervisor"
RESTART_DELAY_SECONDS="${RESTART_DELAY_SECONDS:-15}"
CARGO_PROFILE="${CARGO_PROFILE:-release}"
RUST_LOG="${RUST_LOG:-error}"
SUPERVISOR_VERBOSE="${SUPERVISOR_VERBOSE:-0}"

mkdir -p "$SUPERVISOR_DIR"

usage() {
  cat <<'EOF'
Usage:
  ./start_all.sh start      Start all strategies in supervised background mode
  ./start_all.sh stop       Stop all supervised strategies
  ./start_all.sh status     Show supervisor status
  ./start_all.sh restart    Stop then start all strategies

Environment:
  CARGO_PROFILE=release|debug      Default: release
  RUST_LOG=error|warn|info         Default: error
  SUPERVISOR_VERBOSE=1             Log supervisor lifecycle messages
  RESTART_DELAY_SECONDS=15         Delay before auto-restart
  NO_RESTART=1                     Disable auto-restart
EOF
}

strategy_name() {
  local cfg="$1"
  local name="${cfg#configs/}"
  echo "${name%.env}"
}

cargo_args() {
  if [[ "$CARGO_PROFILE" == "debug" ]]; then
    echo "build"
  else
    echo "build --release"
  fi
}

binary_path() {
  if [[ "$CARGO_PROFILE" == "debug" ]]; then
    echo "$ROOT/target/debug/rusty-poly-streak-rsi"
  else
    echo "$ROOT/target/release/rusty-poly-streak-rsi"
  fi
}

is_running() {
  local pid="$1"
  [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null
}

supervise() {
  set +e
  local cfg="$1"
  local name="$2"
  local binary="$3"

  export STRATEGY_CONFIG="$cfg"
  export RUST_LOG

  log_supervisor() {
    if [[ "${SUPERVISOR_VERBOSE:-0}" == "1" ]]; then
      echo "[$(date -Is)] [$name] $*"
    fi
  }

  log_supervisor "supervisor started | config=$cfg | binary=$binary | rust_log=$RUST_LOG | restart=${NO_RESTART:-0}"

  while true; do
    log_supervisor "process starting"
    "$binary"
    exit_code=$?
    log_supervisor "process exited code=$exit_code"

    if [[ "${NO_RESTART:-0}" == "1" ]]; then
      break
    fi

    log_supervisor "restart in ${RESTART_DELAY_SECONDS}s"
    sleep "$RESTART_DELAY_SECONDS"
  done
}

start_one() {
  local cfg="$1"
  local name
  name="$(strategy_name "$cfg")"
  local pid_file="$SUPERVISOR_DIR/$name.pid"
  local log_file="$SUPERVISOR_DIR/$name.console.log"
  local binary
  binary="$(binary_path)"

  if [[ ! -f "$cfg" ]]; then
    echo "[$name] config not found: $cfg" >&2
    return 1
  fi

  if [[ -f "$pid_file" ]]; then
    local old_pid
    old_pid="$(cat "$pid_file" || true)"
    if is_running "$old_pid"; then
      echo "[$name] already running supervisor pid=$old_pid"
      return 0
    fi
  fi

  nohup bash -c 'supervise "$@"' _ "$cfg" "$name" "$binary" >>"$log_file" 2>&1 &
  local pid=$!
  echo "$pid" >"$pid_file"
  echo "[$name] started supervisor pid=$pid log=$log_file"
}

stop_one() {
  local cfg="$1"
  local name
  name="$(strategy_name "$cfg")"
  local pid_file="$SUPERVISOR_DIR/$name.pid"

  if [[ ! -f "$pid_file" ]]; then
    echo "[$name] not running (no pid file)"
    return 0
  fi

  local pid
  pid="$(cat "$pid_file" || true)"
  if is_running "$pid"; then
    pkill -TERM -P "$pid" 2>/dev/null || true
    kill -TERM "$pid" 2>/dev/null || true
    sleep 2
    if is_running "$pid"; then
      pkill -KILL -P "$pid" 2>/dev/null || true
      kill -KILL "$pid" 2>/dev/null || true
    fi
    echo "[$name] stopped supervisor pid=$pid"
  else
    echo "[$name] stale pid file removed"
  fi

  rm -f "$pid_file"
}

status_one() {
  local cfg="$1"
  local name
  name="$(strategy_name "$cfg")"
  local pid_file="$SUPERVISOR_DIR/$name.pid"
  local log_file="$SUPERVISOR_DIR/$name.console.log"

  if [[ -f "$pid_file" ]]; then
    local pid
    pid="$(cat "$pid_file" || true)"
    if is_running "$pid"; then
      echo "[$name] RUNNING supervisor pid=$pid log=$log_file"
    else
      echo "[$name] STOPPED stale pid=$pid log=$log_file"
    fi
  else
    echo "[$name] STOPPED log=$log_file"
  fi
}

build_once() {
  local args
  args="$(cargo_args)"
  echo "Building once: cargo $args"
  # shellcheck disable=SC2086
  cargo $args

  local binary
  binary="$(binary_path)"
  if [[ ! -x "$binary" ]]; then
    echo "Binary not found or not executable: $binary" >&2
    exit 1
  fi
}

export -f supervise cargo_args binary_path
export ROOT STRATEGY_CONFIG RESTART_DELAY_SECONDS CARGO_PROFILE RUST_LOG SUPERVISOR_VERBOSE NO_RESTART

command="${1:-start}"
case "$command" in
  start)
    if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
      build_once
    fi
    for cfg in "${CONFIGS[@]}"; do
      start_one "$cfg"
      sleep 0.5
    done
    ;;
  stop)
    for cfg in "${CONFIGS[@]}"; do
      stop_one "$cfg"
    done
    ;;
  status)
    for cfg in "${CONFIGS[@]}"; do
      status_one "$cfg"
    done
    ;;
  restart)
    "$0" stop
    "$0" start
    ;;
  help|-h|--help)
    usage
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
