#!/usr/bin/env bash
# Samples pod CPU/memory and cgroup CPU-throttling while a load test runs.
#
# Throttling is the point. `kubectl top` alone cannot distinguish "using 4 cores
# because that is all the work there is" from "pinned at 4 cores because the
# limit says so", and those two lead to opposite conclusions about where the
# bottleneck is. cpu.stat's nr_throttled counts the periods the kernel actually
# stopped the cgroup, which answers it directly.
set -uo pipefail

NS="${NS:-tanukistore}"
INTERVAL="${INTERVAL:-5}"
SAMPLES="${SAMPLES:-48}"

cgroup_stat() { # $1=pod name -> "nr_throttled throttled_usec usage_usec"
  local uid path
  uid=$(kubectl -n "$NS" get pod "$1" -o jsonpath='{.metadata.uid}' 2>/dev/null) || return
  [ -z "$uid" ] && return
  # k3s uses the systemd cgroup driver, which rewrites dashes in the UID.
  path=$(find /sys/fs/cgroup/kubepods.slice -maxdepth 3 -type d \
           -name "*${uid//-/_}*" 2>/dev/null | head -1)
  [ -z "$path" ] && path=$(find /sys/fs/cgroup/kubepods.slice -maxdepth 3 -type d \
           -name "*${uid}*" 2>/dev/null | head -1)
  [ -z "$path" ] && return
  awk '/^nr_throttled/{t=$2} /^throttled_usec/{u=$2} /^usage_usec/{g=$2} END{print t, u, g}' \
      "$path/cpu.stat" 2>/dev/null
}

echo "ts,pod,cpu,mem,nr_throttled,throttled_usec,usage_usec"
for ((i = 1; i <= SAMPLES; i++)); do
  ts=$((i * INTERVAL))
  while read -r pod cpu mem; do
    [ -z "$pod" ] && continue
    echo "${ts},${pod},${cpu},${mem},$(cgroup_stat "$pod" | tr ' ' ',')"
  done < <(kubectl top pods -n "$NS" --no-headers 2>/dev/null)
  sleep "$INTERVAL"
done
