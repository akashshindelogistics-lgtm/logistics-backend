# Local Kubernetes (kind)

Runs the API, the dashboard and a MySQL of their own on a dedicated kind
cluster named `logistics`. Nothing here connects to the host's MySQL.

```
deploy/k8s/kind.sh up      # build images, create or update the cluster
deploy/k8s/kind.sh down    # delete the cluster and all its data
```

`up` needs a logistics-frontend checkout. It looks for a sibling folder named
`logistics-frontend`; otherwise set `FRONTEND_DIR`. It builds whatever is on
disk there, including uncommitted changes.

| In the browser | URL |
| --- | --- |
| Dashboard | http://localhost:8088 |
| API (direct) | http://localhost:8089/api/health |
| Swagger UI | http://localhost:8089/swagger-ui/ |

The dashboard reaches the API through `/api` on its own origin (nginx proxies
it). Both ports are bound to 127.0.0.1 only.

Use the cluster's context explicitly, because `up` leaves your current
kubectl context as it was:

```
kubectl --context kind-logistics -n logistics get pods
```

## Secrets

`up` generates the MySQL passwords and `JWT_SECRET` into the
`logistics-secrets` Secret once, and reuses them afterwards. To turn on the AI
features, set `ANTHROPIC_API_KEY` when running `up` for a new cluster. Twilio
and Resend aren't wired in, so notifications stay queued.

## Host-specific fixes

Two settings exist because of how this runs on a Linux desktop:

- `kind-cluster.yaml` stops privileged pods copying host devices. Otherwise,
  unplugging a USB device (such as an Android phone) stops kube-proxy starting.
- `mysql.yaml` has a `preStop` hook that shuts MySQL down from inside. If the
  host has MySQL installed, its AppArmor profile also applies to the mysqld in
  the pod and blocks the stop signal, leaving the pod stuck in `Terminating`.
