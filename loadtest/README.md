# Spooled Cloud Load Testing

This directory contains load testing tools for validating the performance and reliability of the Spooled Cloud API.

## Tools

### 1. k6 Load Test (Recommended)

Professional load testing with [k6](https://k6.io/).

#### Installation

```bash
# macOS
brew install k6

# Linux
sudo gpg -k
sudo gpg --no-default-keyring --keyring /usr/share/keyrings/k6-archive-keyring.gpg --keyserver hkp://keyserver.ubuntu.com:80 --recv-keys C5AD17C747E3415A3642D57D77C6C491D6AC1D69
echo "deb [signed-by=/usr/share/keyrings/k6-archive-keyring.gpg] https://dl.k6.io/deb stable main" | sudo tee /etc/apt/sources.list.d/k6.list
sudo apt-get update
sudo apt-get install k6

# Docker
docker pull grafana/k6
```

#### Test Scenarios

| Scenario | Description | Duration | Target Rate |
|----------|-------------|----------|-------------|
| `smoke` | Quick validation | 30s | 1 VU |
| `load` | Normal load | 16m | 50→100 VUs |
| `stress` | Find breaking point | 35m | 100→400 VUs |
| `spike` | Sudden traffic spike | 8m | 10→500→10 VUs |
| `soak` | Long-running stability | 30m | 50 VUs |
| `baseline` | Performance baseline | 5m | 1,000 req/sec |
| `throughput` | Max throughput test | 15m | 100→10,000 req/sec |
| `enqueue_only` | Pure enqueue throughput | 3m | 5,000 req/sec |

#### Usage

```bash
# Smoke test (quick validation)
k6 run \
  --env BASE_URL=http://localhost:8080 \
  --env API_KEY=sk_test_your_key \
  loadtest/k6-load-test.js

# Load test
k6 run \
  --env BASE_URL=http://localhost:8080 \
  --env API_KEY=sk_test_your_key \
  --env SCENARIO=load \
  loadtest/k6-load-test.js

# Stress test
k6 run \
  --env BASE_URL=http://localhost:8080 \
  --env API_KEY=sk_test_your_key \
  --env SCENARIO=stress \
  loadtest/k6-load-test.js

# Spike test
k6 run \
  --env BASE_URL=http://localhost:8080 \
  --env API_KEY=sk_test_your_key \
  --env SCENARIO=spike \
  loadtest/k6-load-test.js

# Performance baseline (1000 req/sec)
k6 run \
  --env BASE_URL=http://localhost:8080 \
  --env API_KEY=sk_test_your_key \
  --env SCENARIO=baseline \
  loadtest/k6-load-test.js

# High throughput test (target: 10k req/sec)
k6 run \
  --env BASE_URL=http://localhost:8080 \
  --env API_KEY=sk_test_your_key \
  --env SCENARIO=throughput \
  loadtest/k6-load-test.js

# With JSON output for analysis
k6 run \
  --env BASE_URL=http://localhost:8080 \
  --env API_KEY=sk_test_your_key \
  --out json=results.json \
  loadtest/k6-load-test.js
```

#### Performance Baseline Script

For a comprehensive performance baseline with reporting:

```bash
# Run the full baseline suite
./loadtest/run-perf-baseline.sh

# Or with environment variables
BASE_URL=http://api.example.com API_KEY=sk_live_xxx ./loadtest/run-perf-baseline.sh
```

#### Docker Usage

```bash
docker run --rm -i \
  -v $(pwd)/loadtest:/scripts \
  -e BASE_URL=http://host.docker.internal:8080 \
  -e API_KEY=sk_test_your_key \
  grafana/k6 run /scripts/k6-load-test.js
```

### 2. Simple Shell Test

Basic load testing with just curl and bash (no dependencies).

#### Usage

```bash
# Make executable
chmod +x loadtest/simple-load-test.sh

# Run with defaults (10 concurrent, 30 seconds)
./loadtest/simple-load-test.sh http://localhost:8080 sk_test_your_key

# Custom settings
./loadtest/simple-load-test.sh http://localhost:8080 sk_test_your_key 50 60
#                              ^BASE_URL             ^API_KEY         ^CONCURRENT ^DURATION
```

## Performance Thresholds

The k6 test includes these pass/fail thresholds:

| Metric | p50 | p95 | p99 | Description |
|--------|-----|-----|-----|-------------|
| `http_req_duration` | < 100ms | < 500ms | < 1000ms | Overall HTTP latency |
| `job_enqueue_duration` | < 50ms | < 200ms | < 500ms | Job enqueue latency |
| `job_dequeue_duration` | < 50ms | < 200ms | < 500ms | Job dequeue latency |
| `health_check_duration` | - | < 50ms | - | Health endpoint |

| Metric | Threshold | Description |
|--------|-----------|-------------|
| `http_req_failed` | < 1% | Less than 1% error rate |
| `success_rate` | > 99% | Over 99% successful responses |

### Throughput Target

The target throughput for the Spooled backend is **10,000 jobs/second**. Use the `throughput` scenario to validate your deployment can handle this load.

## Custom Metrics

The tests track these custom metrics:

- `job_enqueue_duration` - Time to enqueue a job
- `job_dequeue_duration` - Time to dequeue a job
- `health_check_duration` - Health endpoint latency
- `failed_requests` - Counter of failed requests
- `success_rate` - Rate of successful requests

## Interpreting Results

### Good Results

```
✓ http_req_duration..............: avg=45ms    min=12ms   med=38ms   max=210ms   p(95)=98ms
✓ http_req_failed................: 0.05%  ✓ 12     ✗ 23988
✓ success_rate...................: 99.95% ✓ 23988  ✗ 12
```

### Concerning Results

```
✗ http_req_duration..............: avg=850ms   min=100ms  med=650ms  max=5.2s    p(95)=2.1s
✗ http_req_failed................: 5.23%  ✓ 1234   ✗ 22766
✗ success_rate...................: 94.77% ✓ 22766  ✗ 1234
```

## Troubleshooting

### Connection Refused

```
Error: API not accessible at http://localhost:8080
```

- Ensure the server is running
- Check the BASE_URL is correct
- Verify firewall/network settings

### High Error Rate

- Check server logs for errors
- Verify database connection pool size
- Check Redis availability
- Review rate limit settings

### Slow Response Times

- Check database query performance
- Review connection pool settings
- Check for resource constraints (CPU/memory)
- Consider scaling horizontally

## CI/CD Integration

### GitHub Actions

```yaml
- name: Run Load Tests
  run: |
    k6 run \
      --env BASE_URL=${{ secrets.STAGING_URL }} \
      --env API_KEY=${{ secrets.STAGING_API_KEY }} \
      --env SCENARIO=smoke \
      loadtest/k6-load-test.js
```

### GitLab CI

```yaml
load_test:
  image: grafana/k6
  script:
    - k6 run --env BASE_URL=$STAGING_URL --env API_KEY=$API_KEY loadtest/k6-load-test.js
```

## Further Reading

- [k6 Documentation](https://k6.io/docs/)
- [Performance Testing Guide](https://k6.io/docs/testing-guides/api-load-testing/)
- [Spooled Architecture](../docs/guides/architecture.md)

