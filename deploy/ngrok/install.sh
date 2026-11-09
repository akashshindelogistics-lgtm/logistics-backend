#!/usr/bin/env bash
# One-time setup of the auto-deploying ngrok host. Safe to re-run.
# Prerequisites are listed in deploy/ngrok/README.md.
set -euo pipefail

ENV_FILE=$HOME/.config/logistics-ngrok/api.env
ROOT=$HOME/logistics-ngrok/autodeploy
HERE=$(cd "$(dirname "$0")" && pwd)

[ -f "$ENV_FILE" ] || { echo "Missing $ENV_FILE (see README.md)" >&2; exit 1; }

# The timer runs the copy of deploy.sh in this clone, so it tracks master.
mkdir -p "$ROOT/repos"
[ -d "$ROOT/repos/logistics-backend/.git" ] ||
    git clone --quiet https://github.com/akashshindelogistics-lgtm/logistics-backend.git "$ROOT/repos/logistics-backend"

mkdir -p "$HOME/.config/systemd/user"
cp "$HERE"/systemd/logi-* "$HOME/.config/systemd/user/"
systemctl --user daemon-reload

# Without linger, user services die at logout and don't start at boot.
loginctl enable-linger "$USER"
systemctl --user enable logi-api.service logi-web.service logi-ngrok.service
systemctl --user enable --now logi-deploy.timer
systemctl --user start --no-block logi-deploy.service
echo "First deploy started: journalctl --user -fu logi-deploy"
