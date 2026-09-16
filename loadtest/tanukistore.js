// Load test for tanukistore ITSELF - update checks, feeds and download
// redirects - with no MinIO byte traffic in the measurement.
//
// Redirects are never followed. A download on this server is a 302 to a
// presigned MinIO URL, so following it would measure MinIO again (see
// RESULTS-2026-09-16.md) and a 200MB body would drown every latency figure.
// The asset size therefore does not affect this test at all: tanukistore
// never touches the bytes.
//
// OPEN model: requests arrive at a fixed rate whether or not the server keeps
// up. A closed model (N VUs looping) slows down with the server and hides
// saturation - coordinated omission. Here saturation shows up as latency and as
// k6's dropped_iterations.
//
//   MODE=constant RATE=4000 DURATION=40s   one fixed rate, for per-level CPU probes
//   (default)                              stepped ramp to find the ceiling

import http from 'k6/http';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

const BASE = __ENV.BASE_URL || 'http://tanukistore.tanukistore.svc.cluster.local:8080';
const APP = __ENV.APP || 'demoapp';
const CURRENT = __ENV.CURRENT_VERSION || '1.1.0';
const OLDER = __ENV.OLDER_VERSION || '1.0.0';

// Weighted mix. Most polls come from clients that are already current, so the
// 204 path dominates real update-server traffic.
const MIX = [
  { weight: 70, name: 'darwin_current', status: 204,
    path: `/update/${APP}/darwin/arm64/${CURRENT}` },
  { weight: 10, name: 'darwin_outdated', status: 200,
    path: `/update/${APP}/darwin/arm64/${OLDER}` },
  { weight: 15, name: 'win32_releases', status: 200,
    path: `/update/${APP}/win32/x64/RELEASES` },
  { weight: 4, name: 'download_latest', status: 302,
    path: `/download/${APP}/latest?platform=darwin&arch=arm64` },
  { weight: 1, name: 'win32_nupkg', status: 302,
    path: `/update/${APP}/win32/x64/DemoApp-${CURRENT}-full.nupkg` },
];
const TOTAL_WEIGHT = MIX.reduce((sum, m) => sum + m.weight, 0);

const wrongStatus = new Counter('wrong_status');

function stepped() {
  const levels = (__ENV.LEVELS || '500,1000,2000,4000,8000,16000').split(',').map(Number);
  const stages = [];
  for (const rate of levels) {
    stages.push({ duration: '10s', target: rate }); // ramp
    stages.push({ duration: '30s', target: rate }); // hold - read numbers here
  }
  return {
    executor: 'ramping-arrival-rate',
    startRate: levels[0],
    timeUnit: '1s',
    preAllocatedVUs: 200,
    maxVUs: 2000,
    stages,
  };
}

function constant() {
  return {
    executor: 'constant-arrival-rate',
    rate: Number(__ENV.RATE || 1000),
    timeUnit: '1s',
    duration: __ENV.DURATION || '40s',
    preAllocatedVUs: 200,
    maxVUs: 2000,
  };
}

const thresholds = {
  http_req_failed: ['rate<0.01'],
  checks: ['rate>0.99'],
  http_req_duration: ['p(95)<50'],
  wrong_status: ['count==0'],
};
// Referencing each tagged submetric makes k6 compute it, which is what puts
// per-endpoint latency into the summary. The bounds are deliberately loose.
for (const m of MIX) {
  thresholds[`http_req_duration{endpoint:${m.name}}`] = ['p(99)<1000'];
}

export const options = {
  discardResponseBodies: true,
  scenarios: {
    feed: __ENV.MODE === 'constant' ? constant() : stepped(),
  },
  thresholds,
  summaryTrendStats: ['med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
};

function pick() {
  let roll = Math.random() * TOTAL_WEIGHT;
  for (const m of MIX) {
    roll -= m.weight;
    if (roll < 0) return m;
  }
  return MIX[0];
}

export default function () {
  const m = pick();
  const res = http.get(BASE + m.path, {
    redirects: 0,
    tags: { endpoint: m.name },
  });
  const ok = check(res, { 'expected status': (r) => r.status === m.status }, { endpoint: m.name });
  if (!ok) wrongStatus.add(1, { endpoint: m.name, status: String(res.status) });
}

export function handleSummary(data) {
  const metric = (name) => data.metrics[name];
  const v = (name, stat) => {
    const m = metric(name);
    return m && m.values[stat] != null ? m.values[stat] : NaN;
  };
  const f = (n, d = 2) => (Number.isFinite(n) ? n.toFixed(d) : 'n/a');
  const secs = data.state.testRunDurationMs / 1000;

  const lines = [
    '',
    '=== tanukistore feed load test ===',
    `requests       : ${f(v('http_reqs', 'count'), 0)}  (${f(v('http_reqs', 'count') / secs, 0)} req/s mean over ${f(secs, 0)}s)`,
    `failed         : ${f(v('http_req_failed', 'rate') * 100, 3)}%`,
    `wrong status   : ${f(v('wrong_status', 'count') || 0, 0)}`,
    `dropped iters  : ${f(v('dropped_iterations', 'count') || 0, 0)}  (arrivals k6 could not start - saturation)`,
    `latency ms     : med ${f(v('http_req_duration', 'med'))}  p95 ${f(v('http_req_duration', 'p(95)'))}  ` +
      `p99 ${f(v('http_req_duration', 'p(99)'))}  max ${f(v('http_req_duration', 'max'))}`,
    '',
    'per endpoint (ms):',
  ];
  for (const m of MIX) {
    const key = `http_req_duration{endpoint:${m.name}}`;
    lines.push(
      `  ${m.name.padEnd(16)} med ${f(v(key, 'med'))}  p95 ${f(v(key, 'p(95)'))}  p99 ${f(v(key, 'p(99)'))}  max ${f(v(key, 'max'))}`,
    );
  }
  lines.push('');
  return { stdout: lines.join('\n') };
}
