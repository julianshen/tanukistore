# 200MB asset path — where the CPU goes, and what to change

**Date:** 2026-09-17 · follows [`RESULTS-2026-09-16.md`](./RESULTS-2026-09-16.md)

The baseline found a ~2.6 GiB/s ceiling that stops scaling between 4 and 8
concurrent streams and gets *worse* at 16. This pass finds out why, using
cgroup CPU accounting (`monitor.sh`, `probe.sh`) and an A/B against nginx
serving the same bytes with `sendfile` (`nginx-sendfile.yaml`).

## Findings

### 1. MinIO spends ~6–11× more CPU per byte than zero-copy serving

Same node, same 200MB random object, same k6, same 8-core limit. Samples are
20s of steady state at a fixed concurrency.

| VUs | Backend | MiB/s | Server cores | Server MiB/s per core | Node cores (minus idle) | Kernel `sys` |
|---|---|---|---|---|---|---|
| 4 | MinIO | 2583 | 4.60 | 562 | 7.53 | 27.7% |
| 4 | nginx | 3218 | 0.92 | 3500 | 4.68 | 18.1% |
| 8 | MinIO | 2609 | **7.08** | 369 | **11.57** | 40.2% |
| 8 | nginx | 2538 | **0.63** | 4012 | **4.40** | 15.7% |
| 16 | MinIO | 2315 | 7.99 (throttled) | 290 | 12.57 | 43.8% |
| 16 | nginx | 2258 | 0.45 | 4990 | 4.24 | 14.0% |

At 8 streams both backends move about 2.5 GiB/s. MinIO spends **7.08
cores** doing it and nginx spends **0.63**. Node-wide, the MinIO run
occupies 11.6 cores against nginx's 4.4, and most of nginx's 4.4 is the
load generator itself.

MinIO reads each object into its own process memory and then writes it to
the socket. `sendfile` hands page-cache pages straight to the socket. The
difference shows up as kernel time: 40% `sys` for MinIO against 16% for
nginx.

### 2. The 16-stream regression is MinIO's CPU limit

At 16 VUs MinIO sits at 7.99 of its 8 cores and is throttled; across the
full baseline run it lost 52.6s to throttling. That is why throughput falls
rather than flattens past 8 streams. nginx never comes near the limit.

### 3. The 8-stream plateau is node-wide CPU, not the limit

At 8 VUs MinIO is at 79% of its limit and barely throttled (0.1 events/s),
yet throughput is already flat. The node is ~80% busy on **every** core, so
the plateau comes from the whole box running out of cycles. Network
interrupt work (`softirq`) is spread evenly (hottest core 1.1× the
average), which rules out a single-core network bottleneck.

### 4. The test harness caps the nginx numbers

Against nginx, the k6 pod is pinned at its 4-core limit and throttled 3.9s
(4 VUs) and 8.1s (16 VUs) per 20s window. So nginx's MiB/s figures are k6's
ceiling rather than nginx's, and nginx's real advantage is **larger** than
the table shows. Against MinIO, k6 used 3.0–3.3 cores and was never
throttled, so the MinIO numbers, including the baseline report, measure
MinIO.

## Opportunities, ranked

1. **Keep MinIO off the hot byte path for large assets.** Put a
   zero-copy tier in front of it for release bundles: nginx `proxy_cache`
   on local disk with `sendfile`, or a CDN. Release assets are immutable
   per version (spec 5 keys them by `{version}/{filename}`), so they cache
   perfectly with no invalidation. Measured upside: roughly 6–11× less
   server CPU per byte.
   *Caveat:* presigned URLs carry their signature in the query string. A
   cache keyed on the full URL never hits, so key on the object path and
   authorize before serving. Don't make the cache an unauthenticated
   bypass.

2. **Keep `/download` a redirect; never proxy through tanukistore-server.**
   Spec 6 already 302s to a presigned URL. These numbers show why that
   matters: a server that streamed bytes itself would add another
   userspace copy on top of MinIO's, on the most expensive path in the
   system. Hold this line when the server is implemented.

3. **Bound concurrent large transfers at ~4–8 per node.** Past that,
   aggregate throughput doesn't rise and per-stream throughput collapses
   (658 → 147 MiB/s from 1 to 16 streams). A concurrency limit turns
   "everyone slow" into "a few fast, the rest briefly queued", which suits
   updater clients that retry.

4. **Raise MinIO's CPU limit, but only together with #3.** Lifting the
   8-core cap fixes the 16-stream regression. On its own, though, it lets
   MinIO take the whole node: it already reached 12.6 of 16 cores, and the
   `nats-chat` stack shares this box. Do it as `limits.cpu: 12` plus the
   concurrency bound, not by deleting the limit.

5. **Fix the harness before measuring the cached tier.** Raise k6's CPU
   limit or run several k6 pods in parallel (k6 `execution-segment`).
   Otherwise any backend faster than MinIO reads as "the same", which is
   exactly what happened with nginx here.

## Scope and caveats

- Everything was served from page cache, because one 200MB object on a 62GB
  box stays in RAM. This measures serving cost, not disk.
- In-cluster only. A real client's link (1GbE ≈ 119 MiB/s) binds long
  before any of this, so these limits matter for per-node density, i.e.
  how many clients one node can feed, not for single-client speed.
- MinIO ran standalone on a single drive, and this pass didn't try any MinIO
  tuning. Whether a MinIO setting narrows the gap is an open question.
- The idle baseline was 1.26 cores, subtracted as "node cores (minus idle)".

## Reproducing

```sh
kubectl apply -f loadtest/nginx-sendfile.yaml
# probe.sh <label> <url> <vus> <label-selector of pods to account CPU to>
./loadtest/probe.sh minio-v8 \
  http://minio.tanukistore.svc.cluster.local:9000/tanukistore-assets/asset-200mb.bin 8 app=minio
./loadtest/probe.sh nginx-v8 \
  http://nginx-sendfile.tanukistore.svc.cluster.local:8080/asset-200mb.bin 8 app=nginx-sendfile
```

`probe.sh` and `monitor.sh` read cgroup v2 `cpu.stat` directly, so run them
on the node itself.
