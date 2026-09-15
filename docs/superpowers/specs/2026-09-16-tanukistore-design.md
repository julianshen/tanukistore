# tanukistore — v1 Implementation Spec

**Date:** 2026-09-16
**Source design:** [`electron-update-server-design_2.md`](../../../electron-update-server-design_2.md) — cited below as *source §N*; a bare *§N* means a section of this spec
**Status:** approved for planning

## 1. Goal

A self-hosted update server for an Electron application whose installed fleet uses
Electron's **native `autoUpdater`** module — Squirrel.Mac on macOS, Squirrel.Windows on
Windows. Metadata and binaries live in self-hosted **MinIO**; there is no database.
Per-user version-check history is durable in **NATS JetStream**.

The driving constraint is that a real installed fleet will point `setFeedURL` at this
service. Wire-protocol correctness is therefore the bar, and its failure mode is silent:
a plausible-but-wrong manifest means clients never update and nobody finds out for months.

## 2. In scope for v1

- source §3 data model as MinIO objects, including `{arch}` (Electron's arm64/x64 split is real).
- `ObjectStore` abstraction over MinIO; strictly generic S3 API (source §4).
- L1 in-memory manifest cache with single-flight, freshness window, and stale-on-error.
- Both Squirrel feeds: macOS `latest.json` resolution and Windows `RELEASES`.
- Presigned download redirects; binaries are never proxied through the service.
- Publisher CLI: upload assets, conditional `index.json` write, derive manifests.
- Staged rollouts (`rollout_pct`) with deterministic, fixed-seed bucketing.
- OpenTelemetry tracing, metrics, and structured logs (source §8).
- `VersionCheckSink` with both `NoopSink` and a real `JetStreamSink`, config-selected.
- Cache invalidation driven by MinIO bucket notifications over NATS.
- Bulkhead + circuit breaker protecting MinIO.

## 3. Explicitly deferred

| Deferred | Why |
|---|---|
| All of source §9 — thread-per-core, `SO_REUSEPORT`, io_uring probe, jemalloc, cgroup-derived sizing | Optimization against an unmeasured baseline; source §13 concedes the numbers are placeholders. Revisit only if load testing justifies it. |
| L2 local disk cache (source §5) | source §5 concedes it is not on the serving path. With presigned redirects nothing needs re-hashing at read time. Pure speculation until delta updates exist. |
| Delta / blockmap updates | source §13: belongs to `electron-updater`'s protocol, not native Squirrel. |
| Build flavors (source §1, source §6) | No current requirement. |
| Admin UI | Publishing is out-of-band by design (source §11). |
| Publish-time code-signing verification | source §13 open question; not required for v1. |

v1 runs on plain Tokio with axum, a shared work-stealing runtime, and the default allocator.

## 4. Decisions resolving ambiguities in the source design

The source design is internally contradictory or silent in six places. Each is settled here.

### 4.1 Rollout bucketing is separate from user identity

**Conflict:** source §6 states that omitting `uid` "changes nothing about the response, only what
gets logged," but source §3 buckets rollouts on "a deterministic hash of `(app, channel,
client_identifier)`." If `uid` is that identifier, omitting it changes the response.

**Decision:** split the concerns.

- `cid` — an opaque, app-generated, **persistent install id** — is the rollout bucketing key.
- `uid` — an opaque user/account id — is used only for attribution, tracing, and logging.
- Bucketing precedence: `cid`, else `uid`, else the client is **ineligible for any release
  with `rollout_pct < 100`**.

This keeps source §8's privacy boundary intact (bucketing never requires a user identity), makes
source §6's claim about `uid` true again, and fails safe. Documented consequence: a fleet that
sends neither id gets all-or-nothing rollouts.

Both are accepted as query parameters, and also as `X-User-Id` / `X-Client-Id` headers.

### 4.2 The bucketing hash is a frozen wire contract

`std`'s `RandomState` seeds SipHash per process, so it would bucket the same `cid`
differently on every replica and re-bucket on every restart — clients flapping between old
and new, the exact failure source §3 says rollouts prevent.

**Decision:** bucketing uses an explicitly fixed-seed hash — SHA-256 over
`{app}:{channel}:{id}`, first 8 bytes as a big-endian `u64`, modulo 100, eligible when
`< rollout_pct`. This function is a frozen contract: changing it re-buckets the entire
fleet at once, so it is covered by a property test and must never be altered casually.

### 4.3 Cache invalidation transport is NATS, not SNS/SQS

**Conflict:** source §5 proposes "SNS→SQS long-poll" to evict L1 on every replica, but an SQS
queue delivers each message to exactly one consumer. With N replicas, one would evict and
N−1 would serve stale manifests until TTL.

**Decision:** MinIO publishes bucket notifications natively to NATS. Every server replica
subscribes to a core NATS subject and evicts the matching L1 key. Fan-out is inherent to
NATS pub/sub, this is operator configuration rather than code, and it catches out-of-band
bucket writes the publisher would not know about. Eviction is idempotent, so duplicate or
replayed notifications are harmless.

### 4.4 Conditional `PutObject` support is probed, never assumed

source §4's optimistic-concurrency guard depends on `If-Match` on PUT, whose support is
version-dependent on MinIO. Silently degrading to last-write-wins would let concurrent
publishes lose release history.

**Decision:** the publisher probes `If-Match` support at startup and **hard-fails** if it is
absent. Proceeding without it requires an explicit `--allow-unsafe-overwrite` flag.

### 4.5 Windows filename convention is validated, not parsed

Squirrel.Windows parses `{AppId}-{version}-full.nupkg` to derive version and full/delta, so
filename is load-bearing on Windows regardless of the source doc's inherited
"resolve by metadata, not filename" principle (which only ever held for macOS).

**Decision:** `index.json` remains the source of truth for resolution. The **publisher
validates** asset filenames against the Squirrel convention at publish time and refuses
non-conforming names. Nothing parses filenames on the read path.

### 4.6 Routes must carry `{app}` and `{arch}`; Windows does not need `{version}`

**Conflict:** source §3's key layout requires `{app}` and `{arch}`; source §6's routes carry neither, so
`/update/darwin/:version` cannot resolve a key. The platforms also differ in what they send:

- **Squirrel.Mac** hits the feed URL directly and does not append the running version — the
  app interpolates its own version into `setFeedURL`. The server does the comparison.
- **Squirrel.Windows** appends `/RELEASES` to the feed base and performs its own version
  comparison against that manifest. `:version` is vestigial on Windows.

**Decision:** the app controls `setFeedURL` entirely, so the full coordinate is encoded in
the URL it sets. See §6.

## 5. Data model

Unchanged from source §3, restated for precision. All objects live in one MinIO bucket.

```
{app}/config.json                                      { channels: [..], defaultChannel }
{app}/{channel}/{platform}/{arch}/index.json           append-only system of record
{app}/{channel}/{platform}/{arch}/latest.json          derived — Squirrel.Mac manifest
{app}/{channel}/{platform}/{arch}/RELEASES             derived — Squirrel.Windows manifest
{app}/{channel}/{platform}/{arch}/{version}/{filename} raw asset bytes
```

`index.json` entries: `{ version, notes, pub_date, rollout_pct, assets: [{ kind, filename,
sha1, sha512, size_bytes }] }`. `sha1` is not redundant with `sha512` — it exists solely to
satisfy the `RELEASES` format and the publisher must compute both. `latest.json` and
`RELEASES` are pure derivations, recomputed whenever `index.json` changes.

Versions are `semver::Version`. Resolution picks the highest eligible version, not the last
appended entry.

## 6. API surface

| Route | Client | Behavior |
|---|---|---|
| `GET /update/:app/darwin/:arch/:version[/:channel]` | Squirrel.Mac | Resolve highest eligible release; `204` if the client is current, else the cached `latest.json` body with a presigned `url` |
| `GET /update/:app/win32/:arch[/:channel]/RELEASES` | Squirrel.Windows | Cached `RELEASES` manifest, one `{sha1} {presigned-url} {size}` line per asset |
| `GET /download/:app/latest?platform=&arch=&channel=` | Humans / CI | `302` to a fresh presigned URL |
| `GET /download/:app/:version?platform=&arch=&filename=` | Humans / CI | `302` to a presigned URL for a pinned version |
| `GET /notes/:app/:version?channel=` | Any | Release notes from the matching `index.json` entry |
| `GET /healthz`, `GET /readyz` | Orchestrator | Liveness / readiness |

Channel defaults to the app's `defaultChannel`. There is deliberately **no write route**;
the bucket's own access policy is the only write-side security boundary (source §11).

**Optional segments.** axum has no optional path parameters, so each `[/:channel]` variant on
the two update routes is registered as two explicit patterns. The download and notes filters
are query parameters rather than optional path segments, which avoids a combinatorial
explosion of route registrations.

**Presign TTL constraint.** Presigned URLs embedded in `RELEASES` must outlive the delay
between a client fetching the manifest and actually starting a multi-hundred-megabyte
download. Presign TTL is therefore **1 hour**, well above the manifest freshness window.
The invariant to preserve: *presign TTL must exceed worst-case download start delay.*

## 7. Crate topology

Two binaries with **separate MinIO service accounts** is a security boundary, not tidiness:
source §11 gives the server read-only access and the publisher write access. One binary would mean
one credential, and "there is deliberately no write route" would be enforced by nothing but
code review.

```
crates/core/          no I/O beyond the ObjectStore trait
  model.rs            Platform, Arch, AssetKind, Asset, Release, Index
  keys.rs             §5 key layout, one function per object kind
  store.rs            ObjectStore trait, S3CompatibleStore, InMemoryStore (test fake),
                      CircuitBreakerStore<S> decorator
  cache.rs            ManifestCache (moka + try_get_with single-flight, freshness, stale-on-error)
  derive.rs           pure: &Index -> (latest.json bytes, RELEASES bytes)
  rollout.rs          fixed-seed bucketing (§4.2)
  events.rs           VersionCheckEvent, VersionCheckSink, NoopSink, JetStreamSink

crates/server/        READ-ONLY MinIO credentials
  routes.rs handlers.rs config.rs telemetry.rs invalidate.rs (NATS subscriber)

crates/publish/       WRITE MinIO credentials
  main.rs (clap) upload.rs index_write.rs (If-Match + bounded retry) validate.rs
```

`derive()` is a **pure function** — no clock, no I/O — called only by the publisher. The
server never derives anything; it serves precomputed bytes. Purity is what makes the golden
fixtures in §12 a meaningful protocol oracle.

## 8. Data flows

**Read (hot path).** Parse route → build key → `ManifestCache::get_or_fetch` (L1 hit is one
refcount bump on `Bytes`; miss is single-flighted, parsed once, stored as `CachedObject {
bytes, parsed, fetched_at }`) → evaluate rollout against `cid`/`uid` → compare versions
(darwin only) → presign asset URLs → respond. Span attributes and the structured log line
are synchronous and local. The JetStream publish is a non-blocking `try_send` onto a bounded
MPSC and is never awaited on the response path (source §9).

**Publish (out-of-band).** Validate filename convention → compute SHA-1 and SHA-512 →
`PutObject` assets to versioned keys → `GetObject index.json`, capturing its ETag → append
the new entry → conditional `PutObject` with `If-Match` → on `412`, re-read and retry with
exponential backoff and jitter, bounded at **5 attempts**, then fail loudly → derive and
upload `latest.json` and `RELEASES`.

**Invalidation.** MinIO bucket notification → NATS subject → every replica's subscriber
evicts the matching L1 key.

## 9. Resilience

### 9.1 Freshness window and stale-on-error

Plain TTL expiry fails every update check in the fleet when MinIO hiccups, because a cache
miss has nowhere to go. Instead:

- `CachedObject` carries `fetched_at`.
- Cache TTL becomes a long **max-staleness bound** (1 hour).
- The **freshness window** is 30–60s for `index.json` / `latest.json` / `RELEASES`, and
  several minutes for `config.json`, which changes only when a channel is added.
- A read past the freshness window attempts a single-flighted refresh. If the refresh fails,
  serve the stale bytes, increment `manifest_stale_served_total`, and mark the span degraded.
- NATS invalidation evicts outright, bypassing the freshness window.

A MinIO outage therefore degrades to slightly-stale manifests rather than a fleet-wide `503`.

### 9.2 Bulkhead and circuit breaker

Back-pressure prevention and failure handling are distinct mechanisms and v1 has both. A
bulkhead bounds the load that can ever be applied to MinIO, acting *before* anything fails;
a breaker stops applying load once failure is sustained. A breaker alone still lets a
thundering herd hit a merely-slow MinIO, because slow is not yet failed.

Read path order: cache → single-flight → **breaker check** → **semaphore** → **timeout** → MinIO.

- **Bulkhead:** a semaphore caps concurrent origin fetches at **32** per process.
- **Timeout:** **2s** per manifest operation. This is a prerequisite, not a companion — a
  hung connection never returns an error, so without a timeout the breaker never trips and
  merely accumulates stuck tasks.
- **Breaker:** `CircuitBreakerStore<S: ObjectStore>`, a decorator in `core` so it is
  testable against `InMemoryStore` with an injected failure mode and the cache and handlers
  stay unaware of it.
  - *Closed* → *Open* on ≥50% failures across a ≥20-request rolling 10s window. Ratio, not
    consecutive count, so partial degradation trips it.
  - *Open* fails fast with no MinIO call, for a cooldown period.
  - *HalfOpen* admits ~3 trial requests; success closes the breaker, failure reopens it with
    an escalating cooldown.
- **`NotFound` never counts as a failure.** A `404` is a successful MinIO response. If it
  counted, a fleet polling a channel that has not published yet would trip the breaker and
  take every other app's feed down with it. Only timeouts, connection errors, and 5xx count.
- Only the **server** breaks. The publisher retries instead. `presign_get` is pure local
  crypto with no network call and is never gated.

Breaker and stale cache compose unusually well: an open breaker serves slightly stale
manifests rather than errors, which makes an aggressive breaker cheap and drops load on a
struggling MinIO to zero precisely when that helps most.

## 10. Observability

Every `/update/...` request opens one `update.check` span with attributes `app`, `channel`,
`platform`, `arch`, `requested_version`, `resolved_version`, `update_available`, and
`cache_tier` (`l1` / `origin` / `stale`), exported via an OTel Collector.

- `uid` present → attached as `user.id`, plus a structured log line
  (`event=version_check`, with `trace_id` / `span_id`), always, synchronously, locally.
- `uid` present **and** JetStream configured → the same event is additionally published on
  `updates.versioncheck.{app}.{uid}.{channel}.{platform}`. The subject hierarchy *is* the
  per-user index; a durable consumer filtered on `updates.versioncheck.*.{uid}.>` replays
  one user's history with no separate database.
- `uid` absent → span and the aggregate `update_checks_total{app, channel, platform,
  update_available}` metric only. Nothing user-attributable is emitted. The system never
  invents an identity for an anonymous client.
- Tail-based sampling always keeps spans carrying `user.id`, up to a volume cap.

Metrics beyond the source design: `manifest_stale_served_total`,
`version_check_events_dropped_total`, `circuit_breaker_state{target="minio"}`,
`circuit_breaker_rejected_total`, `origin_fetch_duration_seconds`.

## 11. Error handling and failure modes

`204` versus `404` is a product decision, not a detail. Squirrel.Mac treats `204` as "you
are current" and may surface a `404` to the user, so the two "missing manifest" cases must
diverge. Collapsing both to `404` shows an update error to every pre-first-release client;
collapsing both to `204` makes a typo'd feed URL fail silently forever.

Fail-safe has a direction: on any error evaluating a rollout, default to **ineligible**. An
erroneous "no update" costs one polling interval; an erroneous "yes, update" ships a build to
clients it was deliberately withheld from, and that cannot be un-shipped.

| Failure | Behavior |
|---|---|
| Unknown app (`config.json` absent) | `404` — loud, it is misconfiguration |
| Known coordinate, nothing published | `204` (darwin); `404` on `RELEASES` (Squirrel handles it) |
| MinIO unreachable, stale copy held | Serve stale, `manifest_stale_served_total`, span degraded |
| MinIO unreachable, no copy held | `503` + `Retry-After` |
| Breaker open, stale copy held | Serve stale, `circuit_breaker_rejected_total` |
| Breaker open, no copy held | Immediate `503` + `Retry-After`, no MinIO call |
| Presign fails | `503` |
| Rollout evaluation error | Default ineligible → `204` |
| JetStream unreachable, or MPSC full | Drop event, `version_check_events_dropped_total`, response unaffected |
| NATS invalidation subscriber down | Log + metric. **Not** a readiness failure — the freshness window carries correctness and the service still serves correctly |
| Publisher hits `412` | Re-read ETag, retry with backoff + jitter, bounded at 5 attempts, then fail loudly |
| Publisher: `If-Match` unsupported | Hard fail at startup unless `--allow-unsafe-overwrite` |

## 12. Testing strategy

Written in this order, because the first item is what decides whether the fleet updates at all.

1. **Golden fixtures — the protocol oracle.** A day-one throwaway spike points a real
   Electron app at hardcoded manifests and confirms both Squirrel clients accept the bytes.
   Those verified bytes freeze into `tests/fixtures/`, and `derive()` is asserted
   byte-for-byte against them. Golden fixtures alone would only encode our *belief* about the
   wire format; validating against a real client first is what makes them an oracle.
2. **Pure unit tests.** Rollout determinism as a property test: identical `cid` yields
   identical verdicts across processes and restarts, and the distribution over many ids lands
   near `rollout_pct`. Semver edge cases: prereleases, build metadata, and a client reporting
   a version newer than the channel's latest.
3. **Route-layer integration against `InMemoryStore`.** No MinIO, no NATS. Covers the
   `204`/`404` split, stale-on-error, breaker state transitions with an injected failure mode,
   `NotFound` not tripping the breaker, and single-flight (N concurrent misses on one key
   produce exactly one `get_object` call, asserted on a counting fake).
4. **Real-infra integration via testcontainers.** MinIO for genuine `If-Match` / `412` race
   semantics (two publishers racing one `index.json`) and presigned-URL round-trips; NATS for
   JetStream publish and invalidation fan-out across two server instances.
5. **Baseline load measurement** with `oha` — not to hit a target, but so the source §9 deferral is
   revisited against numbers rather than assumption.

## 13. Milestones

1. Protocol spike + golden fixtures; workspace scaffold.
2. `core`: model, keys, `derive()`, rollout. Pure, fully unit-tested.
3. `core`: `ObjectStore`, `InMemoryStore`, `S3CompatibleStore`, breaker, bulkhead.
4. `core`: `ManifestCache` with single-flight, freshness, stale-on-error.
5. `server`: both feeds + download/notes routes, against `InMemoryStore`.
6. `publish`: upload, validation, conditional write with retry, derivation upload.
7. MinIO integration tests via testcontainers; end-to-end against a real MinIO.
8. Telemetry: tracing, metrics, structured logs, `NoopSink`.
9. `JetStreamSink` + bounded MPSC + background publisher task.
10. NATS-driven invalidation; multi-replica fan-out test.
11. Baseline load measurement; revisit the source §9 deferral.

## 14. Open questions

- Rollout advancement is manual (edit `rollout_pct`, republish). Automated time-based ramping
  is out of scope for v1.
- JetStream retention (age versus byte limits) and `uid`-scoped deletion for erasure requests
  are unresolved compliance decisions, carried over from source §13.
- Publish-time code-signing verification remains open (source §13).
- Whether `/readyz` should report degraded when the breaker is open. Current decision is no —
  the service still serves correctly from cache, and failing readiness would remove pods that
  are working.
