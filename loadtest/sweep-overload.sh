#!/usr/bin/env bash
# Sweeps the overload bounds via env only (no rebuild). Each configuration
# starts from a fresh pod, so its first probe also exercises the cold cache.
set -uo pipefail
cd "$(dirname "$0")/.."
for conf in "256 1024" "512 1024"; do
  set -- $conf
  echo "### max_connections=$1 max_in_flight=$2"
  kubectl -n tanukistore set env deploy/tanukistore TANUKI_MAX_CONNECTIONS="$1" TANUKI_MAX_IN_FLIGHT="$2" >/dev/null
  kubectl -n tanukistore rollout status deploy/tanukistore --timeout=300s >/dev/null
  # Wait for the old pod to be gone entirely, or the first probe splits
  # traffic with a terminating pod.
  until [ "$(kubectl -n tanukistore get pods -l app=tanukistore --no-headers | wc -l)" = 1 ]; do sleep 2; done
  for r in 4000 12000 16000 24000; do ./loadtest/probe-tanukistore.sh "$r" 40s; done
done
