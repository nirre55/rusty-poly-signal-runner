#!/usr/bin/env bash
set -euo pipefail

APP_DIR="${APP_DIR:-/home/mehdi/rusty-poly-signal-runner}"
SERVICE_NAME="${SERVICE_NAME:-rusty-poly-signal-runner}"
BRANCH="${BRANCH:-master}"
REMOTE="${REMOTE:-origin}"
REPO_URL="${REPO_URL:-https://github.com/nirre55/rusty-poly-signal-runner.git}"
STASH_LOCAL_CHANGES="${STASH_LOCAL_CHANGES:-1}"

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

if [[ ! -d "$APP_DIR/.git" ]]; then
  echo "Repo introuvable: $APP_DIR" >&2
  echo "Astuce: APP_DIR=/chemin/du/repo ./server_update.sh" >&2
  exit 1
fi

cd "$APP_DIR"

log "Arret du service $SERVICE_NAME"
run_as_root systemctl stop "$SERVICE_NAME" || true

if [[ -n "$(git status --porcelain)" ]]; then
  if [[ "$STASH_LOCAL_CHANGES" == "1" ]]; then
    log "Changements locaux detectes, stash automatique"
    git stash push --include-untracked -m "server auto-stash before update $(date -Is)"
  else
    echo "Changements locaux detectes. Commit/stash avant update, ou utilise STASH_LOCAL_CHANGES=1." >&2
    git status --short
    exit 1
  fi
fi

log "Mise a jour du remote origin"
git remote set-url origin "$REPO_URL"

log "Fetch $REMOTE/$BRANCH"
git fetch "$REMOTE" "$BRANCH"

log "Reset local vers $REMOTE/$BRANCH"
git checkout "$BRANCH"
git reset --hard "$REMOTE/$BRANCH"

log "Permissions scripts"
chmod +x start_all.sh server_update.sh

log "Validation rapide start_all.sh"
bash -n start_all.sh

log "Recharge systemd"
run_as_root systemctl daemon-reload
run_as_root systemctl reset-failed "$SERVICE_NAME" || true

log "Demarrage du service $SERVICE_NAME"
run_as_root systemctl start "$SERVICE_NAME"

log "Status systemd"
run_as_root systemctl status "$SERVICE_NAME" --no-pager -l

log "Status bots"
bash start_all.sh status

log "Update terminee"
