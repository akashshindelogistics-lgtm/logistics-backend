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
| Grafana | http://localhost:8088/grafana/ |

The dashboard reaches the API through `/api` on its own origin (nginx proxies
it). Both ports are bound to 127.0.0.1 only.

Use the cluster's context explicitly, because `up` leaves your current
kubectl context as it was:

```
kubectl --context kind-logistics -n logistics get pods
```

## Monitoring

Prometheus and Grafana run in the same namespace; files are in `monitoring/`.
Grafana opens without a login and has two dashboards in the Logistics folder:

- **Logistics — Operations** reads the app's MySQL tables directly: active and
  in-transit dispatches, deliveries and time to deliver, status changes, low
  stock, unpaid and overdue invoices, expiring vehicle documents, overdue
  maintenance, open hires, failed notifications and vehicles that stopped
  reporting location. Filter by organisation at the top.
- **Logistics — API & infrastructure** uses Prometheus: request rate, errors
  and latency per route from the API's `GET /metrics`, delay-alert scan
  failures, MySQL health from `mysqld-exporter`, and CPU and memory per pod
  from the kubelet.

Prometheus keeps 15 days on its own volume. It has no outside port; to query
it directly:

```
kubectl --context kind-logistics -n logistics port-forward svc/prometheus 9090
```

Dashboards are provisioned from the JSON files and can't be saved from the UI.
To change one, edit it in Grafana, export the JSON over the file, and run `up`.

The notification panels show "table doesn't exist" until the app sends its
first notification; that is when it creates the `Notifications` table.

### Alerts

`monitoring/alerts.yaml` provisions alert rules into the Logistics folder
(Alerting > Alert rules in Grafana). They can't be edited from the UI.

| Group | Checked | Rules |
| --- | --- | --- |
| infrastructure | every minute | scrape target down, MySQL down, API 5xx ratio above 5%, API p95 latency above 2s, delay-alert scan failing, pod memory above 90% of its limit, MySQL slow queries |
| operations | every 5 minutes, one alert per organisation | dispatches in transit over 48h, hired vehicle not assigned for 6h, vehicles on in-transit dispatches not reporting location, stock at or below reorder threshold, vehicle documents expired, maintenance overdue, invoices overdue, notifications failing |

No contact point is set up, so alerts only show in Grafana. To be told, add
one (email needs SMTP settings; Slack or a webhook needs only a URL) under
Alerting > Contact points and point the default notification policy at it.

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
