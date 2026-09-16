#!/usr/bin/env bash
# Fixed-rate probe of tanukistore: offered rate in, achieved rate + latency +
# CPU (server AND load generator, with throttling) out.
#
# Reporting the generator's throttling is not optional. If k6 is throttled the
# run measured k6, which is exactly what went wrong in the MinIO A/B.
#
#   probe-tanukistore.sh <rate> [duration]
set -uo pipefail
RATE="$1" DURATION="${2:-40s}"
NS=tanukistore JOB="k6-tk-${RATE}"
WARMUP="${WARMUP:-12}" WINDOW="${WINDOW:-20}"
cd "$(dirname "$0")/.."

usage() { # <label selector> -> "usage_usec throttled_usec"
  local u=0 t=0 uid path
  for uid in $(kubectl -n "$NS" get pods -l "$1" --field-selector=status.phase=Running \
                 -o jsonpath='{range .items[*]}{.metadata.uid}{" "}{end}'); do
    path=$(find /sys/fs/cgroup/kubepods.slice -maxdepth 3 -type d -name "*${uid//-/_}*" 2>/dev/null | head -1)
    [ -z "$path" ] && continue
    u=$((u + $(awk '/^usage_usec/{print $2}' "$path/cpu.stat")))
    t=$((t + $(awk '/^throttled_usec/{print $2}' "$path/cpu.stat")))
  done
  echo "$u $t"
}

kubectl -n "$NS" delete job "$JOB" --ignore-not-found >/dev/null 2>&1
sed -e "s/name: k6-tanukistore$/name: ${JOB}/" \
    -e "s/app: k6-tanukistore/app: k6-tanukistore, probe: \"${RATE}\"/" \
    -e "s/value: \"stepped\"/value: \"constant\"/" \
    -e "s/{ name: RATE, value: \"1000\" }/{ name: RATE, value: \"${RATE}\" }/" \
    -e "s/{ name: DURATION, value: \"40s\" }/{ name: DURATION, value: \"${DURATION}\" }/" \
    deploy/tanukistore/k6-job.yaml | kubectl apply -f - >/dev/null

sleep "$WARMUP"
read -r s1 st1 < <(usage app=tanukistore)
read -r k1 kt1 < <(usage "probe=${RATE}")
t1=$(date +%s.%N)
sleep "$WINDOW"
read -r s2 st2 < <(usage app=tanukistore)
read -r k2 kt2 < <(usage "probe=${RATE}")
t2=$(date +%s.%N)

kubectl -n "$NS" wait --for=condition=complete "job/${JOB}" --timeout=180s >/dev/null 2>&1 \
  || kubectl -n "$NS" wait --for=condition=failed "job/${JOB}" --timeout=5s >/dev/null 2>&1
log=$(kubectl -n "$NS" logs "job/${JOB}" 2>/dev/null)
python3 - "$RATE" "$s1" "$s2" "$st1" "$st2" "$k1" "$k2" "$kt1" "$kt2" "$t1" "$t2" <<PY
import re, sys
rate, s1, s2, st1, st2, k1, k2, kt1, kt2, t1, t2 = sys.argv[1:]
wall = float(t2) - float(t1)
srv = (int(s2) - int(s1)) / 1e6 / wall
srv_thr = (int(st2) - int(st1)) / 1e6
gen = (int(k2) - int(k1)) / 1e6 / wall
gen_thr = (int(kt2) - int(kt1)) / 1e6
log = """$log"""
def grab(pat):
    m = re.search(pat, log)
    return m.group(1) if m else "?"
achieved = grab(r"\((\d+) req/s mean")
p95 = grab(r"latency ms\s+: med [\d.]+\s+p95 ([\d.]+)")
p99 = grab(r"p99 ([\d.]+)\s+max")
dropped = grab(r"dropped iters\s+: (\d+)")
failed = grab(r"failed\s+: ([\d.]+)%")
per_core = float(achieved) / srv if achieved != "?" and srv > 0 else 0
print(f"offered {int(rate):>6}/s | achieved {achieved:>6}/s | p95 {p95:>7}ms p99 {p99:>7}ms | "
      f"dropped {dropped:>7} failed {failed}% | server {srv:4.2f} cores (throttled {srv_thr:5.1f}s) "
      f"~{per_core:,.0f} req/s/core | k6 {gen:4.2f} cores (throttled {gen_thr:4.1f}s)")
PY
