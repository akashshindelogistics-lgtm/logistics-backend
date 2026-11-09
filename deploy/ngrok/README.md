# ngrok deployment

Serves `master` of [logistics-backend] and [logistics-frontend] from one machine
through an ngrok tunnel, and redeploys within about a minute of a merge plus
build time. This is separate from the Oracle VM setup in
[`../README.md`](../README.md).

```
Browser ─▶ ngrok ─▶ logi-web :4173 (vite preview) ─ /api ─▶ logi-api :8090 ─▶ local MySQL
                    logi-deploy.timer: every 60s, deploy.sh checks both repos
```

## How a deploy works

`deploy.sh` fetches `master` of each repo and, for a new commit, builds it into
`~/logistics-ngrok/autodeploy/releases/<component>/<sha>/`, repoints
`current-<component>`, restarts the service and checks its health. If the
health check fails it switches back to the previous release.

A commit that fails to build or to start is recorded in `autodeploy/state/` and
not retried; the next commit on `master` is tried as normal. The site keeps
running the last good release in the meantime.

The API changes the database schema itself at startup, and a rollback does not
undo that.

**Why polling and not a GitHub Action:** GitHub's hosted runners can't reach
this machine, so an Action would need a self-hosted runner here. Both repos are
public, and a pull request from a fork could then run code on the machine that
holds the live database and API keys. Polling needs no credentials and only
ever runs what has been merged to `master`.

## Setup

Prerequisites: Rust, Node 22, MySQL, an authenticated `ngrok`, and
`~/.config/logistics-ngrok/api.env` (mode 600) with `HOST=127.0.0.1`,
`PORT=8090`, `DATABASE_URL`, `JWT_SECRET` and an absolute `UPLOAD_DIR`.

Background sending (notifications and delay alerts) stays off here until
`EVENT_WORKERS_ENABLED=true` is added to that env file; see
`docs/async-events.md`. Finish `docs/database-cleanup.md` first.

```sh
deploy/ngrok/install.sh
```

## Operating it

```sh
systemctl --user list-units 'logi-*'        # what is running
journalctl --user -u logi-deploy -f         # follow deploys
readlink ~/logistics-ngrok/autodeploy/current-backend    # deployed commit
systemctl --user start logi-deploy          # check now instead of waiting

# Retry a commit that failed for a passing reason, e.g. no network
rm ~/logistics-ngrok/autodeploy/state/backend.failed

systemctl --user stop logi-deploy.timer     # pause auto-deploy
```

[logistics-backend]: https://github.com/akashshindelogistics-lgtm/logistics-backend
[logistics-frontend]: https://github.com/akashshindelogistics-lgtm/logistics-frontend
