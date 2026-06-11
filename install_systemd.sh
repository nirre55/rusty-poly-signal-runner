#!/usr/bin/env bash
set -euo pipefail

APP_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERVICE_NAME="${1:-}"
RUST_LOG_VALUE="${RUST_LOG_VALUE:-error}"
START_SERVICE=1
ENABLE_SERVICE=1
USER_NAME="${USER_NAME:-${SUDO_USER:-${USER:-mehdi}}}"

usage() {
  cat <<'EOF'
Usage:
  ./install_systemd.sh SERVICE_NAME [options]

Examples:
  ./install_systemd.sh rusty-poly-signal-runner-single
  ./install_systemd.sh rusty-poly-signal-runner-account2 --rust-log info
  ./install_systemd.sh rusty-poly-signal-runner-test --no-start

Options:
  --rust-log VALUE   RUST_LOG value for the service. Default: error
  --no-start         Install/enable the service but do not start it now
  --no-enable        Install the service but do not enable it at boot
  -h, --help         Show this help

Environment:
  USER_NAME=mehdi    Linux user that should run the bot. Default: sudo user or current shell user
EOF
}

log() {
  echo "[$(date -Is)] $*"
}

run_as_root() {
  if [[ "${EUID:-$(id -u)}" -eq 0 ]]; then
    "$@"
  else
    sudo "$@"
  fi
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rust-log)
      RUST_LOG_VALUE="${2:-}"
      if [[ -z "$RUST_LOG_VALUE" ]]; then
        echo "--rust-log requires a value" >&2
        exit 2
      fi
      shift 2
      ;;
    --no-start)
      START_SERVICE=0
      shift
      ;;
    --no-enable)
      ENABLE_SERVICE=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      if [[ -z "$SERVICE_NAME" ]]; then
        SERVICE_NAME="$1"
        shift
      else
        echo "Unknown argument: $1" >&2
        usage >&2
        exit 2
      fi
      ;;
  esac
done

if [[ -z "$SERVICE_NAME" ]]; then
  usage >&2
  exit 2
fi

if [[ "$SERVICE_NAME" != *".service" ]]; then
  SERVICE_UNIT="$SERVICE_NAME.service"
else
  SERVICE_UNIT="$SERVICE_NAME"
  SERVICE_NAME="${SERVICE_NAME%.service}"
fi

if [[ "$SERVICE_NAME" =~ [^a-zA-Z0-9_.@-] ]]; then
  echo "Invalid service name: $SERVICE_NAME" >&2
  echo "Use only letters, numbers, dot, underscore, @, and dash." >&2
  exit 2
fi

if [[ ! -f "$APP_DIR/start_all.sh" ]]; then
  echo "start_all.sh not found in $APP_DIR" >&2
  exit 1
fi

if [[ ! -f "$APP_DIR/.env" ]]; then
  echo "Warning: .env not found in $APP_DIR" >&2
  echo "Create/configure .env before starting live bots." >&2
fi

log "Validation start_all.sh"
bash -n "$APP_DIR/start_all.sh"
chmod +x "$APP_DIR/start_all.sh"
chmod +x "$APP_DIR/install_systemd.sh"

SERVICE_PATH="/etc/systemd/system/$SERVICE_UNIT"
TMP_SERVICE="$(mktemp)"
cat >"$TMP_SERVICE" <<EOF
[Unit]
Description=Rusty Poly Signal Runner bots ($SERVICE_NAME)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
RemainAfterExit=yes
User=$USER_NAME
WorkingDirectory=$APP_DIR
Environment=PATH=/home/$USER_NAME/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
Environment=RUST_LOG=$RUST_LOG_VALUE
ExecStart=/bin/bash $APP_DIR/start_all.sh start
ExecStop=/bin/bash $APP_DIR/start_all.sh stop
TimeoutStartSec=600
TimeoutStopSec=120

[Install]
WantedBy=multi-user.target
EOF

log "Installing $SERVICE_PATH"
run_as_root cp "$TMP_SERVICE" "$SERVICE_PATH"
rm -f "$TMP_SERVICE"

log "Reload systemd"
run_as_root systemctl daemon-reload
run_as_root systemctl reset-failed "$SERVICE_NAME" || true

if [[ "$ENABLE_SERVICE" == "1" ]]; then
  log "Enable $SERVICE_NAME at boot"
  run_as_root systemctl enable "$SERVICE_NAME"
fi

if [[ "$START_SERVICE" == "1" ]]; then
  log "Start $SERVICE_NAME"
  run_as_root systemctl start "$SERVICE_NAME"

  log "Systemd status"
  run_as_root systemctl status "$SERVICE_NAME" --no-pager -l

  log "Bot status"
  bash "$APP_DIR/start_all.sh" status
else
  log "Installed without starting. Start later with: sudo systemctl start $SERVICE_NAME"
fi

log "Done"
