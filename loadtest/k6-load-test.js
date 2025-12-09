/**
 * Spooled Cloud Load Test Suite
 * 
 * Uses k6 for load testing the API endpoints.
 * 
 * Installation:
 *   brew install k6  (macOS)
 *   or: https://k6.io/docs/getting-started/installation/
 * 
 * Usage:
 *   # Smoke test (quick validation)
 *   k6 run --env BASE_URL=http://localhost:8080 --env API_KEY=sk_test_xxx loadtest/k6-load-test.js
 * 
 *   # Load test (sustained load)
 *   k6 run --env BASE_URL=http://localhost:8080 --env API_KEY=sk_test_xxx -e SCENARIO=load loadtest/k6-load-test.js
 * 
 *   # Stress test (find breaking point)
 *   k6 run --env BASE_URL=http://localhost:8080 --env API_KEY=sk_test_xxx -e SCENARIO=stress loadtest/k6-load-test.js
 * 
 *   # Spike test (sudden traffic spike)
 *   k6 run --env BASE_URL=http://localhost:8080 --env API_KEY=sk_test_xxx -e SCENARIO=spike loadtest/k6-load-test.js
 */

import http from 'k6/http';
import { check, sleep, group } from 'k6';
import { Counter, Rate, Trend } from 'k6/metrics';
import { randomString, randomIntBetween } from 'https://jslib.k6.io/k6-utils/1.2.0/index.js';

// Configuration
const BASE_URL = __ENV.BASE_URL || 'http://localhost:8080';
const API_KEY = __ENV.API_KEY || 'sk_test_default';
const SCENARIO = __ENV.SCENARIO || 'smoke';

// Custom metrics
const jobEnqueueDuration = new Trend('job_enqueue_duration', true);
const jobDequeueDuration = new Trend('job_dequeue_duration', true);
const healthCheckDuration = new Trend('health_check_duration', true);
const failedRequests = new Counter('failed_requests');
const successRate = new Rate('success_rate');

// Test scenarios
const scenarios = {
  smoke: {
    executor: 'constant-vus',
    vus: 1,
    duration: '30s',
  },
  load: {
    executor: 'ramping-vus',
    startVUs: 0,
    stages: [
      { duration: '2m', target: 50 },   // Ramp up to 50 users
      { duration: '5m', target: 50 },   // Stay at 50 users
      { duration: '2m', target: 100 },  // Ramp up to 100 users
      { duration: '5m', target: 100 },  // Stay at 100 users
      { duration: '2m', target: 0 },    // Ramp down
    ],
  },
  stress: {
    executor: 'ramping-vus',
    startVUs: 0,
    stages: [
      { duration: '2m', target: 100 },
      { duration: '5m', target: 100 },
      { duration: '2m', target: 200 },
      { duration: '5m', target: 200 },
      { duration: '2m', target: 300 },
      { duration: '5m', target: 300 },
      { duration: '2m', target: 400 },
      { duration: '5m', target: 400 },
      { duration: '5m', target: 0 },
    ],
  },
  spike: {
    executor: 'ramping-vus',
    startVUs: 0,
    stages: [
      { duration: '10s', target: 10 },   // Normal load
      { duration: '1m', target: 10 },
      { duration: '10s', target: 500 },  // Spike!
      { duration: '3m', target: 500 },   // Stay at spike
      { duration: '10s', target: 10 },   // Back to normal
      { duration: '3m', target: 10 },
      { duration: '10s', target: 0 },
    ],
  },
  soak: {
    executor: 'constant-vus',
    vus: 50,
    duration: '30m',
  },
  // Performance baseline: Target 10,000 jobs/sec throughput
  // Uses constant arrival rate to guarantee request rate
  baseline: {
    executor: 'constant-arrival-rate',
    rate: 1000,              // 1000 iterations per timeUnit
    timeUnit: '1s',          // = 1000 req/sec
    duration: '5m',
    preAllocatedVUs: 100,    // Pre-allocate VUs
    maxVUs: 500,             // Max VUs if needed
  },
  // High throughput test: Push toward 10k jobs/sec
  throughput: {
    executor: 'ramping-arrival-rate',
    startRate: 100,
    timeUnit: '1s',
    preAllocatedVUs: 200,
    maxVUs: 1000,
    stages: [
      { duration: '1m', target: 1000 },   // Ramp to 1k/sec
      { duration: '2m', target: 1000 },   // Hold 1k/sec
      { duration: '1m', target: 3000 },   // Ramp to 3k/sec
      { duration: '2m', target: 3000 },   // Hold 3k/sec
      { duration: '1m', target: 5000 },   // Ramp to 5k/sec
      { duration: '2m', target: 5000 },   // Hold 5k/sec
      { duration: '1m', target: 10000 },  // Ramp to 10k/sec (target!)
      { duration: '3m', target: 10000 },  // Hold 10k/sec
      { duration: '1m', target: 0 },      // Ramp down
    ],
  },
  // Enqueue-only test for pure job throughput measurement
  enqueue_only: {
    executor: 'constant-arrival-rate',
    rate: 5000,              // 5000 enqueues per second
    timeUnit: '1s',
    duration: '3m',
    preAllocatedVUs: 200,
    maxVUs: 500,
  },
};

export const options = {
  scenarios: {
    default: scenarios[SCENARIO],
  },
  thresholds: {
    // Overall HTTP metrics
    http_req_duration: ['p(50)<100', 'p(95)<500', 'p(99)<1000'],  // p50 < 100ms, p95 < 500ms, p99 < 1s
    http_req_failed: ['rate<0.01'],                               // < 1% errors
    success_rate: ['rate>0.99'],                                  // > 99% success
    
    // Job-specific latency targets
    job_enqueue_duration: ['p(50)<50', 'p(95)<200', 'p(99)<500'], // p50 < 50ms, p95 < 200ms, p99 < 500ms
    job_dequeue_duration: ['p(50)<50', 'p(95)<200', 'p(99)<500'], // Same targets for dequeue
    health_check_duration: ['p(95)<50'],                          // Health checks should be fast
  },
  // Summary output configuration
  summaryTrendStats: ['avg', 'min', 'med', 'max', 'p(50)', 'p(90)', 'p(95)', 'p(99)'],
};

// Headers for authenticated requests
const headers = {
  'Content-Type': 'application/json',
  'Authorization': `Bearer ${API_KEY}`,
};

// Test data generators
function generateJobPayload() {
  return JSON.stringify({
    queue_name: `test-queue-${randomIntBetween(1, 5)}`,
    payload: {
      task: 'process_data',
      data: randomString(100),
      timestamp: Date.now(),
    },
    priority: randomIntBetween(-10, 10),
    max_retries: 3,
    timeout_seconds: 300,
  });
}

function generateBulkJobPayload(count) {
  const jobs = [];
  for (let i = 0; i < count; i++) {
    jobs.push({
      queue_name: `test-queue-${randomIntBetween(1, 5)}`,
      payload: {
        task: 'bulk_task',
        index: i,
        data: randomString(50),
      },
      priority: 0,
    });
  }
  return JSON.stringify({ jobs });
}

// Main test function
export default function () {
  // Health check (lightweight, always run)
  group('Health Checks', function () {
    const healthRes = http.get(`${BASE_URL}/health`);
    healthCheckDuration.add(healthRes.timings.duration);
    
    const healthOk = check(healthRes, {
      'health check status is 200': (r) => r.status === 200,
      'health check returns healthy': (r) => {
        try {
          return JSON.parse(r.body).status === 'healthy';
        } catch {
          return false;
        }
      },
    });
    
    successRate.add(healthOk);
    if (!healthOk) failedRequests.add(1);
  });

  sleep(0.1);

  // Job operations (main load)
  group('Job Operations', function () {
    // Enqueue a job
    const enqueueRes = http.post(
      `${BASE_URL}/api/v1/jobs`,
      generateJobPayload(),
      { headers }
    );
    jobEnqueueDuration.add(enqueueRes.timings.duration);
    
    const enqueueOk = check(enqueueRes, {
      'enqueue status is 201': (r) => r.status === 201,
      'enqueue returns job id': (r) => {
        try {
          return JSON.parse(r.body).id !== undefined;
        } catch {
          return false;
        }
      },
    });
    
    successRate.add(enqueueOk);
    if (!enqueueOk) failedRequests.add(1);

    // List jobs
    const listRes = http.get(`${BASE_URL}/api/v1/jobs?limit=10`, { headers });
    check(listRes, {
      'list jobs status is 200': (r) => r.status === 200,
    });

    // Get job stats
    const statsRes = http.get(`${BASE_URL}/api/v1/jobs/stats`, { headers });
    check(statsRes, {
      'job stats status is 200': (r) => r.status === 200,
    });
  });

  sleep(0.2);

  // Queue operations
  group('Queue Operations', function () {
    const queueRes = http.get(`${BASE_URL}/api/v1/queues`, { headers });
    check(queueRes, {
      'list queues status is 200': (r) => r.status === 200,
    });

    const queueStatsRes = http.get(`${BASE_URL}/api/v1/queues/test-queue-1/stats`, { headers });
    // May return 404 if queue doesn't exist, that's OK
    check(queueStatsRes, {
      'queue stats returns valid response': (r) => r.status === 200 || r.status === 404,
    });
  });

  sleep(0.1);

  // Worker simulation (occasionally)
  if (Math.random() < 0.1) {
    group('Worker Operations', function () {
      const workerId = `worker-${randomString(8)}`;
      
      // Register worker
      const registerRes = http.post(
        `${BASE_URL}/api/v1/workers/register`,
        JSON.stringify({
          id: workerId,
          queue_names: ['test-queue-1', 'test-queue-2'],
          max_concurrent_jobs: 5,
        }),
        { headers }
      );
      
      check(registerRes, {
        'worker register status is 200 or 201': (r) => r.status === 200 || r.status === 201,
      });

      // Heartbeat
      const heartbeatRes = http.post(
        `${BASE_URL}/api/v1/workers/${workerId}/heartbeat`,
        JSON.stringify({ status: 'healthy' }),
        { headers }
      );
      
      check(heartbeatRes, {
        'heartbeat returns valid response': (r) => r.status === 200 || r.status === 404,
      });
    });
  }

  // Bulk operations (occasionally)
  if (Math.random() < 0.05) {
    group('Bulk Operations', function () {
      const bulkRes = http.post(
        `${BASE_URL}/api/v1/jobs/bulk`,
        generateBulkJobPayload(10),
        { headers }
      );
      
      check(bulkRes, {
        'bulk enqueue status is 200 or 201': (r) => r.status === 200 || r.status === 201,
      });
    });
  }

  sleep(randomIntBetween(1, 3) / 10);
}

// Setup function (runs once before all VUs)
export function setup() {
  console.log(`Starting ${SCENARIO} test against ${BASE_URL}`);
  
  // Verify API is accessible
  const res = http.get(`${BASE_URL}/health`);
  if (res.status !== 200) {
    throw new Error(`API not accessible: ${res.status}`);
  }
  
  console.log('API is healthy, starting load test...');
  return { startTime: Date.now() };
}

// Teardown function (runs once after all VUs)
export function teardown(data) {
  const duration = (Date.now() - data.startTime) / 1000;
  console.log(`Test completed in ${duration.toFixed(2)} seconds`);
}

// Alternative test function for enqueue-only scenario
// Used when SCENARIO=enqueue_only for maximum throughput measurement
export function enqueueOnly() {
  const res = http.post(
    `${BASE_URL}/api/v1/jobs`,
    generateJobPayload(),
    { headers }
  );
  
  jobEnqueueDuration.add(res.timings.duration);
  
  const ok = check(res, {
    'enqueue status is 201': (r) => r.status === 201,
  });
  
  successRate.add(ok);
  if (!ok) failedRequests.add(1);
}

