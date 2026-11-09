#!/usr/bin/env bash
# Bring the ngrok deployment up to date with `master` of both repos.
# Run every minute by logi-deploy.timer; see deploy/ngrok/README.md.
#
#   deploy.sh              deploy the backend, then the frontend
#   deploy.sh backend      deploy one component
#
# A component that is already current, or whose newest commit already failed,
# costs one `git fetch` and nothing else.
set -Eeuo pipefail

ROOT=${LOGI_DEPLOY_ROOT:-$HOME/logistics-ngrok/autodeploy}
GITHUB=https://github.com/akashshindelogistics-lgtm
API_PORT=8090
WEB_PORT=4173
KEEP_RELEASES=3

log() { echo "[deploy${name:+ $name}] $*"; }

wait_for() { # URL
    local i
    for ((i = 0; i < 60; i++)); do
        curl -fsS -m 3 -o /dev/null "$1" 2>/dev/null && return 0
        sleep 1
    done
    return 1
}

healthy() {
    case $name in
    backend) wait_for "http://127.0.0.1:$API_PORT/api/health" ;;
    frontend) wait_for "http://127.0.0.1:$WEB_PORT/" && wait_for "http://127.0.0.1:$WEB_PORT/api/health" ;;
    esac
}

build() { # release dir to fill
    case $name in
    backend)
        cargo build --release --locked --bin logistics-system
        cp target/release/logistics-system "$1/"
        ;;
    frontend)
        npm ci --no-audit --no-fund
        npm run build
        cp -a dist/. "$1/"
        ;;
    esac
}

# The units ship with the repo, so changing one in a PR rolls out like code.
sync_units() {
    local dest=$HOME/.config/systemd/user file changed=""
    for file in deploy/ngrok/systemd/logi-*; do
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

prune() {
    # Newest first; the current release is always among the newest.
    find "$ROOT/releases/$name" -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' | sort -rn |
        tail -n +$((KEEP_RELEASES + 1)) | cut -d' ' -f2- | xargs -r rm -rf
}

deploy_one() {
    name=$1
    local repo=$ROOT/repos/logistics-$name
    local current=$ROOT/current-$name
    local failed_file=$ROOT/state/$name.failed
    local unit=logi-api previous=""
    if [ "$name" = frontend ]; then unit=logi-web; fi
    mkdir -p "$ROOT/repos" "$ROOT/releases/$name" "$ROOT/state"

    [ -d "$repo/.git" ] || git clone --quiet "$GITHUB/logistics-$name.git" "$repo"
    cd "$repo"
    git fetch --quiet origin master
    sha=$(git rev-parse FETCH_HEAD)

    if [ -L "$current" ]; then previous=$(readlink "$current"); fi
    if [ "$(basename "$previous")" = "$sha" ]; then return 0; fi
    if [ "$(cat "$failed_file" 2>/dev/null)" = "$sha" ]; then return 0; fi

    # Any failed command from here on parks this commit, so a broken master is
    # not rebuilt every minute. A shutdown mid-build is not an ERR and retries.
    trap 'echo "$sha" >"$failed_file"; log "FAILED at ${sha:0:7}"' ERR

    log "deploying ${sha:0:7}: $(git log -1 --format=%s FETCH_HEAD)"
    git checkout --quiet --force --detach "$sha"
    if [ "$name" = backend ]; then sync_units; fi

    local release=$ROOT/releases/$name/$sha
    rm -rf "$release"
    mkdir -p "$release"
    build "$release"

    ln -sfn "$release" "$current"
    systemctl --user restart "$unit"
    if ! healthy; then
        log "unhealthy after restart"
        if [ -n "$previous" ]; then
            log "rolling back to $(basename "$previous" | cut -c1-7)"
            ln -sfn "$previous" "$current"
            systemctl --user restart "$unit"
            healthy || log "the previous release is unhealthy too"
        fi
        false
    fi

    trap - ERR
    rm -f "$failed_file"
    prune
    log "live at ${sha:0:7}"
}

if [ $# -eq 1 ]; then
    deploy_one "$1"
    exit 0
fi

# Separate processes so a failing backend still lets the frontend deploy.
# Backend first: a dashboard change may need the API change that preceded it.
rc=0
"$0" backend || rc=1
"$0" frontend || rc=1
systemctl --user start logi-ngrok
exit $rc
