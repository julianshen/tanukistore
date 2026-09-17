#!/usr/bin/env bash
# Fixed-rate probe of tanukistore: offered rate in; achieved rate, latency,
# CPU (server AND load generator, with throttling), server memory and restarts
# out.
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

cgroup_of() { # <pod uid> -> cgroup dir
  find /sys/fs/cgroup/kubepods.slice -maxdepth 3 -type d -name "*${1//-/_}*" 2>/dev/null | head -1
}

usage() { # <label selector> -> "usage_usec throttled_usec system_usec"
  local u=0 t=0 y=0 uid path
  for uid in $(kubectl -n "$NS" get pods -l "$1" --field-selector=status.phase=Running \
                 -o jsonpath='{range .items[*]}{.metadata.uid}{" "}{end}'); do
    path=$(cgroup_of "$uid")
    [ -z "$path" ] && continue
    u=$((u + $(awk '/^usage_usec/{print $2}' "$path/cpu.stat")))
    t=$((t + $(awk '/^throttled_usec/{print $2}' "$path/cpu.stat")))
    y=$((y + $(awk '/^system_usec/{print $2}' "$path/cpu.stat")))
  done
  echo "$u $t $y"
}

server_mem_mib() {
  local uid
  uid=$(kubectl -n "$NS" get pods -l app=tanukistore --field-selector=status.phase=Running \
          -o jsonpath='{.items[0].metadata.uid}')
  echo $(( $(cat "$(cgroup_of "$uid")/memory.current") / 1048576 ))
}

server_restarts() {
  kubectl -n "$NS" get pods -l app=tanukistore \
    -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}'
}

m0=$(server_mem_mib) r0=$(server_restarts)
kubectl -n "$NS" delete job "$JOB" --ignore-not-found >/dev/null 2>&1
sed -e "s/name: k6-tanukistore$/name: ${JOB}/" \
    -e "s/app: k6-tanukistore/app: k6-tanukistore, probe: \"${RATE}\"/" \
    -e "s/value: \"stepped\"/value: \"constant\"/" \
    -e "s/{ name: RATE, value: \"1000\" }/{ name: RATE, value: \"${RATE}\" }/" \
    -e "s/{ name: DURATION, value: \"40s\" }/{ name: DURATION, value: \"${DURATION}\" }/" \
    deploy/tanukistore/k6-job.yaml | kubectl apply -f - >/dev/null

sleep "$WARMUP"
read -r s1 st1 sy1 < <(usage app=tanukistore)
read -r k1 kt1 _ < <(usage "probe=${RATE}")
t1=$(date +%s.%N)
sleep "$WINDOW"
read -r s2 st2 sy2 < <(usage app=tanukistore)
read -r k2 kt2 _ < <(usage "probe=${RATE}")
t2=$(date +%s.%N)

# k6 exits non-zero when a threshold is crossed, which marks the Job Failed;
# either terminal state means the run is over.
kubectl -n "$NS" wait --for=condition=complete "job/${JOB}" --timeout=180s >/dev/null 2>&1 \
  || kubectl -n "$NS" wait --for=condition=failed "job/${JOB}" --timeout=5s >/dev/null 2>&1
m1=$(server_mem_mib) r1=$(server_restarts)

# The log is handed over as a FILE. Pasting it into the Python source broke an
# earlier version of this script: k6 lines end in quotes that terminate an
# embedded string literal, and a whole probe run was lost.
logfile=$(mktemp)
kubectl -n "$NS" logs "job/${JOB}" > "$logfile" 2>/dev/null

python3 - "$RATE" "$s1" "$s2" "$st1" "$st2" "$k1" "$k2" "$kt1" "$kt2" "$t1" "$t2" \
          "$logfile" "$m0" "$m1" "$r0" "$r1" "$sy1" "$sy2" <<'EOF'
import re
import sys

(rate, s1, s2, st1, st2, k1, k2, kt1, kt2, t1, t2,
 logfile, m0, m1, r0, r1, sy1, sy2) = sys.argv[1:]
wall = float(t2) - float(t1)
srv = (int(s2) - int(s1)) / 1e6 / wall
srv_thr = (int(st2) - int(st1)) / 1e6
# Share of the server's CPU spent in the kernel: syscalls and the network
# stack, as opposed to tanukistore's own code.
srv_sys = (int(sy2) - int(sy1)) / 1e6 / wall
sys_share = 100 * srv_sys / srv if srv > 0 else 0.0
gen = (int(k2) - int(k1)) / 1e6 / wall
gen_thr = (int(kt2) - int(kt1)) / 1e6
log = open(logfile, errors="replace").read()


def grab(pattern):
    m = re.search(pattern, log)
    return m.group(1) if m else "?"


achieved = grab(r"\((\d+) req/s mean")
p95 = grab(r"latency ms\s+: med [\d.]+\s+p95 ([\d.]+)")
p99 = grab(r"latency ms\s+: .*p99 ([\d.]+)")
dropped = grab(r"dropped iters\s+: (\d+)")
failed = grab(r"failed\s+: ([\d.]+)%")
per_core = float(achieved) / srv if achieved != "?" and srv > 0 else 0.0
restarted = f" RESTARTED x{int(r1) - int(r0)}" if r0 != r1 else ""
print(
    f"offered {int(rate):>6}/s | achieved {achieved:>6}/s | p95 {p95:>7}ms p99 {p99:>7}ms | "
    f"dropped {dropped:>7} failed {failed}% | "
    f"server {srv:4.2f} cores ({sys_share:3.0f}% sys) thr {srv_thr:4.1f}s ~{per_core:,.0f} req/s/core | "
    f"k6 {gen:4.2f} cores thr {gen_thr:4.1f}s | server mem {m0}->{m1} MiB{restarted}"
)
EOF
rm -f "$logfile"
