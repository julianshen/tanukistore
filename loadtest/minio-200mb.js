// Capacity baseline for tanukistore's asset path: repeated 200MB object reads
// from MinIO, which is the payload profile spec 4.7's /download route exists to
// serve (an Electron app bundle).
//
// Runs INSIDE the cluster as a Job, so it dials the MinIO Service directly and
// measures the same network path tanukistore-server will take. Nothing is
// published off the node.
//
// Override the target without editing this file:
//   OBJECT_URL=... OBJECT_BYTES=... k6 run minio-200mb.js

import http from 'k6/http';
import { check } from 'k6';
import { Rate, Trend, Counter } from 'k6/metrics';

const OBJECT_URL =
  __ENV.OBJECT_URL ||
  'http://minio.tanukistore.svc.cluster.local:9000/tanukistore-assets/asset-200mb.bin';
const OBJECT_BYTES = Number(__ENV.OBJECT_BYTES || 209715200); // 200 MiB

const errors = new Rate('errors');
// Per-stream throughput. The built-in data_received covers the AGGREGATE, but
// the interesting question at this payload size is what a single client sees
// while N others compete, which only a per-request figure answers.
const streamMBps = new Trend('stream_mbps');
const shortReads = new Counter('short_reads');

export const options = {
  // Load-bearing at this payload size. Without it every VU buffers 200MB of
  // response in memory, so 16 VUs is 3.2GB of load-generator heap and k6 dies
  // before MinIO is under any interesting pressure. k6 still reads the full
  // body off the socket, so the timings stay honest.
  discardResponseBodies: true,

  // A genuine STEP profile. `stages` interpolates linearly from the current VU
  // count to each target, so a bare list of rising targets is one long ramp
  // with no steady state anywhere - every sample is taken mid-transition and
  // the concurrency that produced it is already gone. Pairing each short ramp
  // with a hold is what creates a plateau long enough to read a stable number
  // off, and stepping between plateaus is what exposes the concurrency at
  // which aggregate throughput stops scaling.
  stages: [
    { duration: '10s', target: 1 },  // single-stream baseline
    { duration: '40s', target: 1 },
    { duration: '10s', target: 4 },
    { duration: '40s', target: 4 },
    { duration: '10s', target: 8 },
    { duration: '40s', target: 8 },
    { duration: '10s', target: 16 },
    { duration: '40s', target: 16 },
    { duration: '10s', target: 0 },
  ],

  thresholds: {
    http_req_failed: ['rate<0.01'],
    errors: ['rate<0.01'],
    // Deliberately NOT a p95 on http_req_duration. At 200MB per request,
    // duration is just throughput wearing a disguise, and a latency SLO copied
    // from a JSON-API test would be meaningless here.
    stream_mbps: ['p(50)>20'],
    short_reads: ['count==0'],
  },

  summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

export default function () {
  const res = http.get(OBJECT_URL, {
    // k6 defaults to a 60s request timeout. A 200MB transfer under heavy
    // contention can legitimately exceed that, and a timeout would be recorded
    // as an error - reporting saturation as a failure.
    timeout: '120s',
    tags: { name: 'asset-200mb' },
  });

  const ok = check(res, {
    'status is 200': (r) => r.status === 200,
    // With discarded bodies r.body is null, so the only way to prove the whole
    // object arrived is the Content-Length header. A truncated response that
    // still returns 200 would otherwise read as a very fast success.
    'full object served': (r) =>
      Number(r.headers['Content-Length']) === OBJECT_BYTES,
  });

  if (!ok) {
    if (res.status === 200 && Number(res.headers['Content-Length']) !== OBJECT_BYTES) {
      shortReads.add(1);
    }
    errors.add(1);
    return;
  }

  errors.add(0);
  // Wall-clock for the request, not time-to-first-byte: the transfer IS the
  // work here, so dividing by duration alone would flatter the result.
  const seconds = res.timings.duration / 1000;
  if (seconds > 0) {
    streamMBps.add(OBJECT_BYTES / 1048576 / seconds);
  }
  // No sleep(): this is a saturation test, so VUs pull continuously. Adding
  // think-time would measure politeness rather than capacity.
}

export function handleSummary(data) {
  return {
    '/results/summary.json': JSON.stringify(data, null, 2),
    stdout: textSummary(data),
  };
}

// Minimal renderer so the Job logs carry the numbers even if the results volume
// is never collected.
function textSummary(data) {
  const m = data.metrics;
  const get = (name, stat) => (m[name] && m[name].values[stat] != null ? m[name].values[stat] : NaN);
  const fmt = (n, d = 2) => (Number.isFinite(n) ? n.toFixed(d) : 'n/a');
  const gib = (b) => fmt(b / 1073741824);

  const reqs = get('http_reqs', 'count');
  const durSec = data.state && data.state.testRunDurationMs ? data.state.testRunDurationMs / 1000 : NaN;
  const received = get('data_received', 'count');

  return [
    '',
    '=== 200MB object read - capacity baseline ===',
    `requests            : ${fmt(reqs, 0)}`,
    `failed              : ${fmt(get('http_req_failed', 'rate') * 100)}%`,
    `short reads         : ${fmt(get('short_reads', 'count'), 0)}`,
    `data received       : ${gib(received)} GiB`,
    `aggregate throughput: ${fmt(received / 1048576 / durSec)} MiB/s over ${fmt(durSec, 1)}s`,
    '',
    '--- per-stream throughput (MiB/s) ---',
    `med ${fmt(get('stream_mbps', 'med'))}  avg ${fmt(get('stream_mbps', 'avg'))}  ` +
      `p90 ${fmt(get('stream_mbps', 'p(90)'))}  p95 ${fmt(get('stream_mbps', 'p(95)'))}  ` +
      `min ${fmt(get('stream_mbps', 'min'))}  max ${fmt(get('stream_mbps', 'max'))}`,
    '',
    '--- request duration (ms) ---',
    `med ${fmt(get('http_req_duration', 'med'))}  p90 ${fmt(get('http_req_duration', 'p(90)'))}  ` +
      `p95 ${fmt(get('http_req_duration', 'p(95)'))}  max ${fmt(get('http_req_duration', 'max'))}`,
    '',
  ].join('\n');
}
