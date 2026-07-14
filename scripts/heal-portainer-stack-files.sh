#!/usr/bin/env bash
# Heal Portainer Git leftovers for Spooled backend on the shared prod host.
#
# WHY: Portainer Git "Pull and redeploy" clones into /data/compose/71/<sha>/ then
# often deletes that directory after compose up. Containers keep running, but
# labels still point at the missing path → UI "Unable to retrieve stack file".
# This script only repairs stack id 71 / durable /opt/spooled/backend. It never
# touches other Compose projects and never removes volumes.
#
# WHEN: After Pull and redeploy, if Portainer cannot open docker-compose.prod.yml
# or `docker inspect spooled_backend` shows a working_dir that does not exist.
#
# Image policy: production follows ghcr.io/spooled-cloud/spooled-backend:latest
# (see docs/guides/production-host-portainer.md).
set -euo pipefail

DURABLE="${SPOOL_BACKEND_DURABLE:-/opt/spooled/backend}"
STACK_ROOT="${SPOOL_BACKEND_STACK_ROOT:-/data/compose/71}"

if [[ ! -d "$DURABLE" ]]; then
  echo "Durable path missing: $DURABLE" >&2
  exit 1
fi

# Ensure path from running backend label exists (Portainer often deletes the sha dir).
if command -v docker >/dev/null 2>&1 && docker inspect spooled_backend >/dev/null 2>&1; then
  wd=$(docker inspect spooled_backend --format '{{index .Config.Labels "com.docker.compose.project.working_dir"}}' 2>/dev/null || true)
  if [[ -n "${wd:-}" && ! -e "$wd" ]]; then
    echo "Missing labeled WD $wd — linking to durable"
    mkdir -p "$(dirname "$wd")"
    ln -sfn "$DURABLE" "$wd"
  fi
fi

# Heal empty sha leftovers under this stack only.
if [[ -d "$STACK_ROOT" ]]; then
  for d in "$STACK_ROOT"/*/; do
    base=$(basename "$d")
    case "$base" in
      spooled-backend*) continue ;;
    esac
    if [[ ! -e "$d/docker-compose.prod.yml" ]]; then
      ln -sfn "$DURABLE/docker-compose.prod.yml" "$d/docker-compose.prod.yml"
    fi
    if [[ ! -e "$d/.env" && -f "$DURABLE/.env" ]]; then
      ln -sfn "$DURABLE/.env" "$d/.env"
    fi
  done
  ln -sfn "$DURABLE" "$STACK_ROOT/spooled-backend"
fi

echo "Healed. durable=$DURABLE"
if command -v docker >/dev/null 2>&1 && docker inspect spooled_backend >/dev/null 2>&1; then
  docker inspect spooled_backend --format 'labeled_wd={{index .Config.Labels "com.docker.compose.project.working_dir"}} health={{.State.Health.Status}}'
fi
