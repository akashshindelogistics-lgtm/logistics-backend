#!/usr/bin/env bash
# One-time setup of auto-deploy for the local kind cluster. Safe to re-run.
# See "Auto-deploy" in deploy/k8s/README.md.
set -euo pipefail

ROOT=$HOME/logistics-kind/autodeploy
HERE=$(cd "$(dirname "$0")" && pwd)

for tool in git docker kind kubectl curl; do
    command -v "$tool" >/dev/null || { echo "Missing $tool" >&2; exit 1; }
done
docker info >/dev/null 2>&1 || {
    echo "Can't talk to Docker as $USER; add yourself to the docker group and log in again" >&2
    exit 1
}

# The timer runs the copy of autodeploy.sh in this clone, so it tracks master.
mkdir -p "$ROOT/repos"
[ -d "$ROOT/repos/logistics-backend/.git" ] ||
    git clone --quiet https://github.com/akashshindelogistics-lgtm/logistics-backend.git "$ROOT/repos/logistics-backend"

mkdir -p "$HOME/.config/systemd/user"
cp "$HERE"/systemd/logi-kind-* "$HOME/.config/systemd/user/"
systemctl --user daemon-reload

# Without linger, user services die at logout and don't start at boot.
loginctl enable-linger "$USER"
systemctl --user enable --now logi-kind-deploy.timer
systemctl --user start --no-block logi-kind-deploy.service
echo "First deploy started: journalctl --user -fu logi-kind-deploy"
