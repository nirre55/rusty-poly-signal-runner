#!/usr/bin/env bash
set -euo pipefail

APP_DIR="${APP_DIR:-/home/mehdi/rusty-poly-signal-runner-fiveyear}"
SERVICE_NAME="${SERVICE_NAME:-rusty-poly-signal-runner-fiveyear}"
SERVICE_FILE="${SERVICE_FILE:-systemd/rusty-poly-signal-runner-fiveyear.service}"
BRANCH="${BRANCH:-master}"
REMOTE="${REMOTE:-origin}"
REPO_URL="${REPO_URL:-https://github.com/nirre55/rusty-poly-signal-runner.git}"
STASH_LOCAL_CHANGES="${STASH_LOCAL_CHANGES:-1}"
AUTO_INSTALL_SYSTEMD="${AUTO_INSTALL_SYSTEMD:-1}"

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

service_exists() {
  systemctl list-unit-files "$SERVICE_NAME.service" --no-legend 2>/dev/null | grep -q "$SERVICE_NAME.service"
}

install_systemd_service() {
  if [[ "$AUTO_INSTALL_SYSTEMD" != "1" ]]; then
    return 0
  fi

  if [[ ! -f "$SERVICE_FILE" ]]; then
    log "Service file introuvable: $SERVICE_FILE"
    return 0
  fi

  log "Installation/maj systemd $SERVICE_NAME"
  run_as_root cp "$SERVICE_FILE" "/etc/systemd/system/$SERVICE_NAME.service"
  run_as_root systemctl daemon-reload
  run_as_root systemctl enable "$SERVICE_NAME"
}

stop_bots() {
  if service_exists; then
    log "Arret du service $SERVICE_NAME"
    run_as_root systemctl stop "$SERVICE_NAME" || true
  elif [[ -x "./start_all.sh" || -f "./start_all.sh" ]]; then
    log "Service absent, arret via start_all.sh"
    bash ./start_all.sh stop || true
  else
    log "Aucun service/start_all.sh disponible pour arreter"
  fi
}

start_bots() {
  if service_exists; then
    log "Demarrage du service $SERVICE_NAME"
    run_as_root systemctl reset-failed "$SERVICE_NAME" || true
    run_as_root systemctl start "$SERVICE_NAME"

    log "Status systemd"
    run_as_root systemctl status "$SERVICE_NAME" --no-pager -l
  else
    log "Service absent, demarrage direct via start_all.sh"
    bash ./start_all.sh start
  fi
}

if [[ ! -d "$APP_DIR/.git" ]]; then
  echo "Repo introuvable: $APP_DIR" >&2
  echo "Astuce: APP_DIR=/chemin/du/repo ./server_update_fiveyear.sh" >&2
  exit 1
fi

cd "$APP_DIR"

stop_bots

stash_created=0
if [[ -n "$(git status --porcelain)" ]]; then
  if [[ "$STASH_LOCAL_CHANGES" == "1" ]]; then
    log "Changements locaux detectes, stash automatique"
    git stash push --include-untracked -m "fiveyear auto-stash before update $(date -Is)"
    stash_created=1
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

log "Merge fast-forward vers $REMOTE/$BRANCH"
git checkout "$BRANCH"
git merge --ff-only "$REMOTE/$BRANCH"

if [[ "$stash_created" == "1" ]]; then
  log "Reapplication des changements locaux"
  if ! git stash pop; then
    echo "Conflit pendant git stash pop. Corrige les fichiers, puis relance les validations manuellement." >&2
    git status --short
    exit 1
  fi
fi

log "Permissions scripts"
chmod +x start_all.sh server_update_fiveyear.sh
[[ -f server_update.sh ]] && chmod +x server_update.sh

log "Validation rapide start_all.sh"
bash -n start_all.sh

install_systemd_service

log "Demarrage des bots"
start_bots

log "Status bots"
bash start_all.sh status

log "Update fiveyear terminee"
