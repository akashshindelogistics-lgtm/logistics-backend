#!/usr/bin/env bash
# Run the whole system on a local kind cluster of its own.
#   deploy/k8s/kind.sh up     create or update it
#   deploy/k8s/kind.sh down   delete the cluster and all its data
# FRONTEND_DIR: logistics-frontend checkout (default: sibling of this repo).
# ANTHROPIC_API_KEY: stored in the cluster on first `up` if set; enables AI.
set -euo pipefail

CLUSTER=logistics
CONTEXT="kind-$CLUSTER"
NAMESPACE=logistics
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
FRONTEND_DIR="${FRONTEND_DIR:-$REPO_ROOT/../logistics-frontend}"
kc() { kubectl --context "$CONTEXT" "$@"; }

ensure_cluster() {
  if kind get clusters | grep -qx "$CLUSTER"; then return; fi
  # `kind create` switches the current kubectl context; put it back so
  # creating this cluster doesn't redirect commands meant for another one.
  local previous
  previous="$(kubectl config current-context 2>/dev/null || true)"
  kind create cluster --name "$CLUSTER" --config "$HERE/kind-cluster.yaml"
  if [ -n "$previous" ]; then kubectl config use-context "$previous" >/dev/null; fi
}

ensure_secrets() {
  kc apply -f "$HERE/namespace.yaml"
  # Never regenerated: MySQL keeps the password it first initialised with.
  if kc -n "$NAMESPACE" get secret logistics-secrets >/dev/null 2>&1; then return; fi
  local args=(
    --from-literal=mysql-password="$(openssl rand -hex 24)"
    --from-literal=mysql-root-password="$(openssl rand -hex 24)"
    --from-literal=jwt-secret="$(openssl rand -hex 32)"
  )
  if [ -n "${ANTHROPIC_API_KEY:-}" ]; then
    args+=(--from-literal=anthropic-api-key="$ANTHROPIC_API_KEY")
  fi
  kc -n "$NAMESPACE" create secret generic logistics-secrets "${args[@]}"
}

up() {
  if [ ! -f "$FRONTEND_DIR/package.json" ]; then
    echo "No frontend at $FRONTEND_DIR; set FRONTEND_DIR." >&2
    exit 1
  fi
  docker build -t logistics-api:local "$REPO_ROOT"
  docker build -t logistics-frontend:local -f "$HERE/frontend.Dockerfile" "$FRONTEND_DIR"
  ensure_cluster
  kind load docker-image --name "$CLUSTER" logistics-api:local logistics-frontend:local
  ensure_secrets
  kc apply -k "$HERE"
  # The tags don't change between builds, so a rollout has to be asked for.
  kc -n "$NAMESPACE" rollout restart deployment/logistics-api deployment/logistics-frontend
  kc -n "$NAMESPACE" rollout status statefulset/mysql --timeout=300s
  kc -n "$NAMESPACE" rollout status deployment/logistics-api --timeout=300s
  kc -n "$NAMESPACE" rollout status deployment/logistics-frontend --timeout=120s
  kc -n "$NAMESPACE" rollout status deployment/prometheus deployment/grafana --timeout=180s
  cat <<URLS

Dashboard:  http://localhost:8088
API:        http://localhost:8089/api/health
Swagger UI: http://localhost:8089/swagger-ui/
Grafana:    http://localhost:8088/grafana/
URLS
}

case "${1:-}" in
  up) up ;;
  down) kind delete cluster --name "$CLUSTER" ;;
  *) echo "usage: $0 up|down" >&2; exit 2 ;;
esac
