#!/usr/bin/env bash
# Keep the local kind cluster on `master` of both repos.
# Run every minute by logi-kind-deploy.timer; see "Auto-deploy" in
# deploy/k8s/README.md.
#
# A new commit on either repo builds both images tagged by commit, rolls them
# out with kind.sh and checks the dashboard and API answer. If anything fails,
# the cluster goes back to the last good images, which are still loaded in it,
# and that pair of commits is not retried until one of the repos moves on.
# When nothing changed, a run costs two `git fetch`es.
set -Eeuo pipefail

ROOT=${LOGI_KIND_ROOT:-$HOME/logistics-kind/autodeploy}
GITHUB=${LOGI_GITHUB:-https://github.com/akashshindelogistics-lgtm}
CLUSTER=logistics
KEEP_IMAGES=3
DASHBOARD=http://127.0.0.1:8088
API=http://127.0.0.1:8089

BACKEND=$ROOT/repos/logistics-backend
FRONTEND=$ROOT/repos/logistics-frontend
DEPLOYED=$ROOT/state/deployed # "<backend sha> <frontend sha>" now live
FAILED=$ROOT/state/failed     # the pair that last failed

log() { echo "[kind-deploy] $*"; }

wait_for() { # URL
    local i
    for ((i = 0; i < 60; i++)); do
        curl -fsS -m 3 -o /dev/null "$1" 2>/dev/null && return 0
        sleep 1
    done
    return 1
}

healthy() {
    wait_for "$API/api/health" && wait_for "$DASHBOARD/" && wait_for "$DASHBOARD/api/health"
}

master_sha() { # repo dir, clone url
    [ -d "$1/.git" ] || git clone --quiet "$2" "$1"
    git -C "$1" fetch --quiet origin master
    git -C "$1" rev-parse FETCH_HEAD
}

# The units ship with the repo, so changing one in a PR rolls out like code.
sync_units() {
    local dest=$HOME/.config/systemd/user file changed=""
    mkdir -p "$dest"
    for file in "$BACKEND"/deploy/k8s/systemd/logi-kind-*; do
        if ! cmp -s "$file" "$dest/$(basename "$file")"; then
            cp "$file" "$dest/"
            changed=1
        fi
    done
    if [ -n "$changed" ]; then
        log "updated systemd units"
        systemctl --user daemon-reload
    fi
}

kind_sh() { # api tag, frontend tag, kind.sh command
    API_TAG=$1 FRONTEND_TAG=$2 FRONTEND_DIR=$FRONTEND "$BACKEND/deploy/k8s/kind.sh" "$3"
}

# Keep the newest few commit-tagged images on the host and drop the rest; the
# kind node forgets images nothing runs any more. Best effort.
prune() {
    local repo
    for repo in logistics-api logistics-frontend; do
        docker image ls "$repo" --format '{{.CreatedAt}}\t{{.Repository}}:{{.Tag}}' |
            grep -v ':local$' | sort -r | tail -n +$((KEEP_IMAGES + 1)) | cut -f2 |
            xargs -r docker image rm >/dev/null 2>&1 || true
    done
    docker exec "$CLUSTER-control-plane" crictl rmi --prune >/dev/null 2>&1 || true
}

mkdir -p "$ROOT/repos" "$ROOT/state"
backend_sha=$(master_sha "$BACKEND" "$GITHUB/logistics-backend.git")
frontend_sha=$(master_sha "$FRONTEND" "$GITHUB/logistics-frontend.git")
target="$backend_sha $frontend_sha"
previous=$(cat "$DEPLOYED" 2>/dev/null || true)

if [ "$target" = "$previous" ]; then exit 0; fi
if [ "$target" = "$(cat "$FAILED" 2>/dev/null)" ]; then exit 0; fi

api_tag=${backend_sha:0:12}
web_tag=${frontend_sha:0:12}

on_failure() {
    trap - ERR
    echo "$target" >"$FAILED"
    log "FAILED at backend ${api_tag:0:7}, frontend ${web_tag:0:7}"
    if [ -n "$previous" ]; then
        read -r prev_backend prev_frontend <<<"$previous"
        log "rolling back to backend ${prev_backend:0:7}, frontend ${prev_frontend:0:7}"
        if kind_sh "${prev_backend:0:12}" "${prev_frontend:0:12}" apply && healthy; then
            log "rolled back"
        else
            log "the previous release is unhealthy too"
        fi
    fi
    exit 1
}
# Any failed command from here on parks this pair, so a broken master is not
# rebuilt every minute. A shutdown mid-build is not an ERR and retries.
trap on_failure ERR

log "deploying backend ${api_tag:0:7} ($(git -C "$BACKEND" log -1 --format=%s FETCH_HEAD))," \
    "frontend ${web_tag:0:7} ($(git -C "$FRONTEND" log -1 --format=%s FETCH_HEAD))"
git -C "$BACKEND" checkout --quiet --force --detach "$backend_sha"
git -C "$FRONTEND" checkout --quiet --force --detach "$frontend_sha"
sync_units

kind_sh "$api_tag" "$web_tag" up
if ! healthy; then
    log "unhealthy after rollout"
    false
fi

trap - ERR
echo "$target" >"$DEPLOYED"
rm -f "$FAILED"
prune
log "live: backend ${api_tag:0:7}, frontend ${web_tag:0:7}"
