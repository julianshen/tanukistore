#!/usr/bin/env bash
# Constant-concurrency probe: runs k6 at a fixed VU count against one URL and
# reports throughput alongside the CPU it cost, both per pod and node-wide.
#
# Throughput alone cannot rank two backends. What matters for capacity is
# bytes moved per core, so every probe reports MiB/s AND the cores spent, and
# the comparison is made on their ratio.
#
#   probe.sh <label> <url> <vus> <server-pod-label-selector>
set -uo pipefail

LABEL="$1" URL="$2" VUS="$3" SELECTOR="$4"
NS="${NS:-tanukistore}"
DURATION="${DURATION:-50s}"
WARMUP="${WARMUP:-15}"   # seconds to let the plateau settle before sampling
WINDOW="${WINDOW:-20}"   # seconds of steady state to measure over
JOB="k6-probe-${LABEL}"
cd "$(dirname "$0")/.."

node_cpu() { grep '^cpu ' /proc/stat; }
pod_usage() { # summed cpu usage_usec across pods matching the selector
  local total=0 uid path u
  for uid in $(kubectl -n "$NS" get pods -l "$SELECTOR" \
                 -o jsonpath='{range .items[*]}{.metadata.uid}{" "}{end}'); do
    path=$(find /sys/fs/cgroup/kubepods.slice -maxdepth 3 -type d \
             -name "*${uid//-/_}*" 2>/dev/null | head -1)
    [ -z "$path" ] && continue
    u=$(awk '/^usage_usec/{print $2}' "$path/cpu.stat" 2>/dev/null)
    total=$((total + ${u:-0}))
  done
  echo "$total"
}
throttled() {
  local total=0 uid path t
  for uid in $(kubectl -n "$NS" get pods -l "$SELECTOR" \
                 -o jsonpath='{range .items[*]}{.metadata.uid}{" "}{end}'); do
    path=$(find /sys/fs/cgroup/kubepods.slice -maxdepth 3 -type d \
             -name "*${uid//-/_}*" 2>/dev/null | head -1)
    [ -z "$path" ] && continue
    t=$(awk '/^throttled_usec/{print $2}' "$path/cpu.stat" 2>/dev/null)
    total=$((total + ${t:-0}))
  done
  echo "$total"
}

kubectl -n "$NS" delete job "$JOB" --ignore-not-found >/dev/null 2>&1
sed -e "s/name: k6-minio-200mb/name: ${JOB}/" \
    -e "s#args: \[\"run\", \"/scripts/minio-200mb.js\"\]#args: [\"run\", \"--vus\", \"${VUS}\", \"--duration\", \"${DURATION}\", \"/scripts/minio-200mb.js\"]#" \
    -e "s#value: http://minio.tanukistore.svc.cluster.local:9000/tanukistore-assets/asset-200mb.bin#value: ${URL}#" \
    deploy/minio/k6-job.yaml | kubectl apply -f - >/dev/null

sleep "$WARMUP"
n1=$(node_cpu); p1=$(pod_usage); t1=$(throttled); s1=$(date +%s.%N)
sleep "$WINDOW"
n2=$(node_cpu); p2=$(pod_usage); t2=$(throttled); s2=$(date +%s.%N)

kubectl -n "$NS" wait --for=condition=complete "job/${JOB}" --timeout=180s >/dev/null 2>&1
agg=$(kubectl -n "$NS" logs "job/${JOB}" 2>/dev/null \
        | awk -F': ' '/aggregate throughput/{print $2}' | awk '{print $1}')
failed=$(kubectl -n "$NS" logs "job/${JOB}" 2>/dev/null \
        | awk -F': ' '/^failed/{print $2}')

python3 - "$LABEL" "$VUS" "$agg" "$failed" "$n1" "$n2" "$p1" "$p2" "$t1" "$t2" "$s1" "$s2" <<'PY'
import sys
label, vus, agg, failed, n1, n2, p1, p2, t1, t2, s1, s2 = sys.argv[1:]
a = list(map(int, n1.split()[1:])); b = list(map(int, n2.split()[1:]))
d = [b[i] - a[i] for i in range(8)]; tot = sum(d)
ncpu = 16
node_cores = ncpu * (tot - d[3] - d[4]) / tot
sys_pct = 100 * d[2] / tot
wall = float(s2) - float(s1)
server_cores = (int(p2) - int(p1)) / 1e6 / wall
thr = (int(t2) - int(t1)) / 1e6
agg = float(agg or 0)
BASELINE = 1.26  # idle node cores, measured with no test running
test_cores = node_cores - BASELINE
print(f"{label:>10} vus={vus:>2} | {agg:8.1f} MiB/s | server {server_cores:5.2f} cores "
      f"({agg/server_cores if server_cores else 0:6.1f} MiB/s/core, throttled {thr:4.1f}s) | "
      f"node {node_cores:5.2f} cores, test {test_cores:5.2f} "
      f"({agg/test_cores if test_cores>0 else 0:6.1f} MiB/s/core) sys={sys_pct:4.1f}% | failed {failed}")
PY
