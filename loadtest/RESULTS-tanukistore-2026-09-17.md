# tanukistore feed load test

**Date:** 2026-09-17 · **Target:** tanukistore-server `0.2.0-rc.1` / `rc.2`, one pod
capped at **1 CPU / 256Mi**, on k3s (`friday`) · **Generator:** k6 0.58.0,
in-cluster, open model

This test measures tanukistore itself. The earlier reports
([`RESULTS-2026-09-16.md`](./RESULTS-2026-09-16.md),
[`OPTIMIZATION-2026-09-17.md`](./OPTIMIZATION-2026-09-17.md)) measured MinIO.

## What was measured, and what was not

- **Real feed.** Two releases of a 200MB demo app (`1.0.0`, `1.1.0`, darwin/arm64
  and win32/x64) were published into a private bucket by `tanukistore-publish`.
  So every manifest the server returns was written by the real publisher.
- **No asset bytes in the path.** Downloads are 302s to presigned MinIO URLs, and
  k6 never follows redirects. The 200MB asset size therefore does not affect
  these numbers at all; the MinIO reports cover the byte path.
- **Traffic mix** (weighted toward what an update server mostly sees: clients
  that are already current):

  | Share | Request | Expected |
  |---|---|---|
  | 70% | darwin update check, client current | 204 |
  | 10% | darwin update check, client outdated | 200 + `latest.json` |
  | 15% | win32 `RELEASES` | 200 |
  | 4% | `/download/demoapp/latest` | 302 |
  | 1% | win32 nupkg by filename | 302 |

- **Open model** (`constant-arrival-rate`). Requests arrive on schedule whether or
  not the server keeps up, so saturation shows up as latency and dropped
  arrivals instead of being hidden.
- **Probes** (`probe-tanukistore.sh`) run 40s at a fixed rate and sample 20s of
  steady state from the pods' cgroups: server and k6 CPU, CPU throttling, the
  kernel's share of server CPU, server memory and restarts.

## End-to-end correctness

The smoke test and every probe below 12k/s returned **zero wrong statuses**
across several million requests. The smoke test also verified:

- 204 vs 404 follow spec 11 (a real channel with nothing published answers 204;
  an unknown app or channel answers 404).
- The `url` inside `latest.json` resolves through the server to a presigned
  URL, and that URL returns the object (206 on a ranged GET).
- A presigned signature is bound to its key (403 on another key).
- `/metrics` is served only on the admin port (404 on the public port).

## Capacity: ~12,700 req/s per core

| Offered | Achieved | p95 | p99 | Server CPU | Req/s per core |
|---|---|---|---|---|---|
| 2,000 | 1,989 | 0.85 ms | 1.16 ms | – | – |
| 4,000 | 4,000 | 0.68 ms | 1.17 ms | 0.48 | 8,410 |
| 8,000 | 7,995 | 0.63 ms | 1.29 ms | 0.73 (stepped run) | ~11,000 |
| 12,000 | 11,983 | 2.01 ms | 3.98 ms | 0.94 | 12,730 |
| 16,000 | 12,701 | 943 ms | 966 ms | 0.99 | 12,795 (uncapped, saturated) |

- **Tokio sized itself to the quota.** Rust's `available_parallelism` reads the
  cgroup, so the server runs one worker thread under a 1-core limit and was
  never CPU-throttled, even when saturated.
- **About 52% of server CPU is kernel time** (syscalls and the network stack)
  at every load level. tanukistore's own per-request work is already a
  minority of the cost.

## Bug found: OOM under overload (fixed in rc.2)

In rc.1, at 16k offered, the pod was **OOM-killed**. For the seconds it took to
restart, every client got `connection refused`.

The cause was unbounded acceptance, not a leak:

- Identical 160k-request runs left memory flat (24 → 24 MiB).
- Overload grew memory from **29 to 199 MiB in 40s**, at about 28KB per waiting
  connection, because the server accepted every connection k6 opened (up to
  6,000).

rc.2 added three things (`crates/server/src/overload.rs`):

1. A process-wide cap on requests in flight, with load shedding: excess requests
   get an immediate `503` with `Retry-After: 1`.
2. A connection cap, enforced at accept. Clients beyond it wait in the kernel's
   accept backlog instead of in server memory.
3. The Prometheus exporter's upkeep task. `install_recorder()` does not start it,
   so histogram samples would otherwise accumulate until someone scrapes
   `/metrics`. This was confirmed from the exporter's source. It was too slow to
   cause this OOM, but it is unbounded on any pod that is never scraped.

Result: **no restarts at up to 32k/s offered (2.5× capacity), and memory peaked
at 79 MiB.**

## Sizing the caps

Each configuration was started on a fresh pod, so its first probe also
exercises a cold cache.

| max_connections / max_in_flight | 4k/s (cold) | 12k/s | Overload p95 | Overload throughput | Memory under overload |
|---|---|---|---|---|---|
| unbounded (rc.1 behavior) | ok | ok | **943–1376 ms** | ~12.8k | 199 MiB → **OOM** |
| 2048 / 256 | **0.97% shed** | ok | 313 ms | ~8.3k | ≤ 79 MiB |
| **512 / 1024 (chosen)** | ok, 0% failed | ok, p95 2.0 ms | **60–87 ms** | ~7.4–8.1k | ≤ 22 MiB |
| 256 / 1024 | **only 2,596/s achieved** | only 7,895/s | 22 ms | ~8.5k | 14 MiB |

What the sweep showed:

- **The connection cap is what bounds latency.** An HTTP/1.1 connection carries
  one request at a time, so past capacity each client waits roughly
  (open connections × service time). 2,048 connections gave a 313 ms p95;
  256 gave 22 ms.
- **The cap must leave room for idle keep-alive connections,** which hold a
  permit while doing nothing. At 256, k6's pool of 200+ VUs held nearly every
  permit, which starved an ordinary 4k/s load.
- **The in-flight cap must sit above the connection cap.** With HTTP/1.1,
  in-flight requests can never exceed open connections, so a lower in-flight cap
  only fires during bursts. It did fire during the cold-cache burst after a
  restart, shedding 1% of a 4k/s load. It stays in place as the bound for
  HTTP/2.
- **Capping costs about 37% of throughput under overload.**

## Open question: why capping costs throughput

Capped at 512, the server spends a full core delivering ~7.4k/s. At 12k/s,
before overload, it spends 0.94 cores on ~12k/s. The kernel share is the same
~52% in both cases, so the extra cost is not accept or backlog churn in the
kernel.

The remaining suspect is an interaction between k6's VU pool and the capped
accept queue: arrivals are handed to VUs that cannot connect. Settling it needs a
CPU profile (`perf`, which needs root on the node) or a load generator with a
fixed connection pool. The trade still favors the cap: 60 ms and bounded memory
under overload beat 1 s and an OOM.

## Mistakes in the harness along the way

These are recorded because each one produced numbers that looked plausible and
were wrong.

- **k6 throttled by its own quota.** Go sized its scheduler from the node's 16
  cores, not from k6's 8-core limit, so CFS repeatedly froze it. The first
  stepped run showed a 0.46 ms median beside a 273 ms p95 while the server sat
  unthrottled at 0.86 cores. The latency was k6's own. Fixed by setting
  `GOMAXPROCS` equal to k6's CPU limit.
- **Probe script lost a run.** The k6 log was embedded in Python source, and a
  trailing quote broke it. The log is now passed as a file.
- **HEAD against a presigned URL returns 403.** SigV4 signs the HTTP method.
  Verify presigned URLs with a ranged GET.

## Optimization opportunities, ranked

1. **Scale out, not up.** At ~12.7k req/s per core with a flat per-core curve,
   capacity is a replica count. Set an HPA on CPU at ~70%. The in-memory cache
   is per replica, which the 30s freshness window already tolerates.
2. **Let an edge cache absorb update checks.** Every feed response is a pure
   function of its URL for up to the freshness window. Today the server sends
   `no-cache` on 200s and nothing on 204s. Sending `Cache-Control: public,
   max-age=30` would let an ingress or CDN answer most of the traffic before it
   reaches a core. This is a product decision: a publish would take up to 30s
   longer to reach clients, on top of the freshness window that already exists.
3. **Remove the per-request kernel cost.** With 52% of CPU in the kernel,
   tanukistore's own code is not where the remaining time goes. Options include
   keeping client connections alive longer (fewer accepts and handshakes;
   Squirrel's own behavior decides how much this helps) and terminating TLS at
   the ingress, not in-process, when TLS arrives.
4. **Finish the overload investigation** with a profile on the node, then
   decide whether capped goodput can be recovered.

## Reproducing

```sh
# One-time: bucket, identities, server, demo releases
kubectl apply -f deploy/tanukistore/access-job.yaml   # needs the two -s3 secrets, see file header
kubectl apply -f deploy/tanukistore/server.yaml
kubectl apply -f deploy/tanukistore/publish-job.yaml

kubectl -n tanukistore create configmap k6-tanukistore \
  --from-file=loadtest/tanukistore.js --dry-run=client -o yaml | kubectl apply -f -

# On the node (reads cgroup stats directly):
./loadtest/probe-tanukistore.sh 12000 40s
./loadtest/sweep-overload.sh
```
