#!/usr/bin/env bash
# Run the whole system on a local kind cluster of its own.
#   deploy/k8s/kind.sh up      build images, create or update the cluster
#   deploy/k8s/kind.sh apply   roll out images that are already in the cluster
#   deploy/k8s/kind.sh down    delete the cluster and all its data
# FRONTEND_DIR: logistics-frontend checkout (default: sibling of this repo).
# ANTHROPIC_API_KEY: stored in the cluster on first `up` if set; enables AI.
# API_TAG / FRONTEND_TAG: image tags (default `local`). autodeploy.sh tags by
#   commit so it can roll back to images still in the cluster with `apply`.
set -euo pipefail

CLUSTER=logistics
CONTEXT="kind-$CLUSTER"
NAMESPACE=logistics
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$HERE/../.." && pwd)"
FRONTEND_DIR="${FRONTEND_DIR:-$REPO_ROOT/../logistics-frontend}"
API_TAG="${API_TAG:-local}"
FRONTEND_TAG="${FRONTEND_TAG:-local}"
API_IMAGE="logistics-api:$API_TAG"
FRONTEND_IMAGE="logistics-frontend:$FRONTEND_TAG"
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

# Apply the manifests with this run's image tags and wait for the rollout.
apply() {
  ensure_secrets
  kc kustomize "$HERE" |
    sed -e "s#image: logistics-api:local\$#image: $API_IMAGE#" \
        -e "s#image: logistics-frontend:local\$#image: $FRONTEND_IMAGE#" |
    kc apply -f -
  if [ "$API_TAG" = local ] || [ "$FRONTEND_TAG" = local ]; then
    # A reused tag doesn't change the spec, so a rollout has to be asked for.
    kc -n "$NAMESPACE" rollout restart deployment/logistics-api deployment/logistics-frontend
  fi
  kc -n "$NAMESPACE" rollout status statefulset/mysql --timeout=300s
  kc -n "$NAMESPACE" rollout status deployment/logistics-api --timeout=300s
  kc -n "$NAMESPACE" rollout status deployment/logistics-frontend --timeout=120s
  kc -n "$NAMESPACE" rollout status deployment/prometheus deployment/grafana --timeout=180s
}

up() {
  if [ ! -f "$FRONTEND_DIR/package.json" ]; then
    echo "No frontend at $FRONTEND_DIR; set FRONTEND_DIR." >&2
    exit 1
  fi
  docker build -t "$API_IMAGE" "$REPO_ROOT"
  docker build -t "$FRONTEND_IMAGE" -f "$HERE/frontend.Dockerfile" "$FRONTEND_DIR"
  ensure_cluster
  kind load docker-image --name "$CLUSTER" "$API_IMAGE" "$FRONTEND_IMAGE"
  apply
  cat <<URLS

Dashboard:  http://localhost:8088
API:        http://localhost:8089/api/health
Swagger UI: http://localhost:8089/swagger-ui/
Grafana:    http://localhost:8088/grafana/
URLS
}

case "${1:-}" in
  up) up ;;
  apply) apply ;;
  down) kind delete cluster --name "$CLUSTER" ;;
  *) echo "usage: $0 up|apply|down" >&2; exit 2 ;;
esac
