# NTP Time JSON API

A **production-oriented, general-purpose** HTTP service that returns NTP-derived time as JSON, built
with Rust 1.92. It is **not yet financial/time-critical production-ready** — that requires completing
the P0 tasks in [`PRODUCTION_ACCURACY_PLAN.md`](PRODUCTION_ACCURACY_PLAN.md).

> **Current limitations (see [`PRODUCTION_ACCURACY_PLAN.md`](PRODUCTION_ACCURACY_PLAN.md)):**
> - Upstream T2/T3 are **reconstructed (estimated)** from offset/delay, not measured server
>   timestamps (the `rsntp` client discards the raw timestamps).
> - The UDP NTP server currently advertises **`root_dispersion = 0`** (false certainty).
> - There is **no time-quality / uncertainty envelope** and no serve/stop SLA yet.
> - There is **no secure manual time-override** path yet.
> - There is **no full end-to-end CI** yet (unit + inline integration tests only).
>
> For financial / latency-sensitive deployments, follow the P0/P1 roadmap in
> [`PRODUCTION_ACCURACY_PLAN.md`](PRODUCTION_ACCURACY_PLAN.md) before relying on this service as an
> authoritative time source.

## Features

- **NTP-Authoritative Time**: Directly queries NTP servers (UDP) without relying on OS wall clock
- **High Performance**: Lightweight hot-path with cached NTP time, sub-millisecond response times
- **Monotonic Time Model**: Guarantees time never goes backwards using `Instant::now()` + NTP base
- **Multi-Server Support**: Queries all configured NTP servers each sync with accuracy-first selection and automatic failover
- **Outlier Filtering**: Uses median offset calculation to reject divergent server responses
- **Resilient**: Continues serving from cache if NTP sync fails after initial successful sync
- **Kubernetes-Ready**: Includes liveness, readiness, and startup probes
- **Prometheus Metrics**: Full observability with HTTP and NTP metrics
- **Configurable Messages**: Supports UTF-8 messages including Persian/Farsi text
- **Graceful Shutdown**: Proper SIGTERM handling with connection draining

## Architecture

### Time Model

The service uses a monotonic time progression model to avoid OS wall clock authority:

```rust
on successful NTP sync:
    base_ntp_epoch_ms = NTP epoch time in milliseconds
    base_instant = Instant::now() (monotonic clock)

on each /time request:
    now_ms = base_ntp_epoch_ms + (Instant::now() - base_instant).as_millis()
```

This ensures:
- No dependence on `SystemTime` for correctness
- Time never goes backwards due to OS clock adjustments
- Extremely fast request hot-path (no NTP queries per request)

### NTP Strategy

- **Selection**: accuracy-first / median-consensus based — picks the server whose offset is closest
  to the consensus (median) offset; **RTT is only a tiebreaker**. (The `SELECTION_STRATEGY=rtt_min`
  env value is a backwards-compatible alias for this algorithm, not "lowest RTT".)
- **Sampling**: Queries **all** configured servers each sync (in parallel), then selects.
- **Outlier Filtering**: Rejects servers beyond `MAX_OFFSET_SKEW_MS` from median
- **Failover**: Automatically tries backup servers if primary fails
- **Sync Interval**: Background sync every 30 seconds (configurable)
- **Probe Loop**: Separate jittered loop for keeping server stats fresh

> Planned: P1-6 in [`PRODUCTION_ACCURACY_PLAN.md`](PRODUCTION_ACCURACY_PLAN.md) replaces this with an
> uncertainty-scored selection model (per-sample root distance + quorum), and P1F-12 adds
> interval-intersection robustness against an independently-wrong majority.

### Probe Behavior

Critical for Kubernetes: probes are designed so NTP failures don't kill pods after initial sync.

- **`/healthz`**: Always returns 200 if process is alive
- **`/readyz`**: Returns 503 before first sync (if `REQUIRE_SYNC=true`), then always 200
- **`/startupz`**: Returns 503 until first successful sync, then always 200
- **`/time`**: Returns 503 before first sync (if `REQUIRE_SYNC=true`), then always 200 (serves from cache)

## API Endpoints

### `GET /time` (or `GET /`)

Returns current NTP-derived epoch time in milliseconds.

**Success Response:**
```json
{
  "message": "done",
  "status": 200,
  "data": 1704067200000
}
```

**Before First Sync (REQUIRE_SYNC=true):**
```json
{
  "message": "error",
  "status": 503,
  "data": 0,
  "error": "Service not yet synchronized with NTP"
}
```

### `GET /healthz`

Liveness probe - always returns 200 if process is alive.

### `GET /readyz`

Readiness probe - returns 503 before first sync, then always 200.

### `GET /startupz`

Startup probe - returns 503 until first successful sync.

### `GET /metrics`

Prometheus metrics in text exposition format.

### `WS /stream` (WebSocket)

Real-time time streaming endpoint. Connects via WebSocket and receives periodic time updates.

**Configuration:**
- `WS_UPDATE_INTERVAL_MS` - Update interval in milliseconds (default: 1000)
- `WS_MAX_DURATION_SECS` - Maximum connection duration in seconds (default: 3600)

**Welcome Message:**
```json
{
  "type": "welcome",
  "message": "Connected to NTP Time JSON API WebSocket",
  "update_interval_ms": 1000,
  "max_duration_secs": 3600
}
```

**Time Update Messages:**
```json
{
  "type": "tick",
  "epoch_ms": 1735446000000,
  "iso8601": "2024-12-29T00:00:00+00:00",
  "is_stale": false,
  "staleness_secs": 12,
  "message": "done",
  "sequence": 42
}
```

**Usage Examples:**
```javascript
// Browser
const ws = new WebSocket('ws://localhost:8080/stream');
ws.onmessage = (event) => {
    const data = JSON.parse(event.data);
    console.log('Time:', data.epoch_ms, data.iso8601);
};
```

```bash
# CLI (using websocat)
websocat ws://localhost:8080/stream
```

See `test_websocket.html` for an interactive test client.

## Configuration

All configuration via environment variables:

### HTTP Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `ADDR` | `0.0.0.0:8080` | HTTP server bind address |
| `REQUEST_TIMEOUT` | `5` | Request timeout in seconds |
| `BODY_LIMIT_BYTES` | `1024` | Max request body size |

### NTP Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `NTP_SERVERS` | `time.google.com:123,time.cloudflare.com:123,pool.ntp.org:123` | Comma-separated NTP servers |
| `NTP_TIMEOUT` | `2` | NTP query timeout in seconds |
| `SYNC_INTERVAL` | `30` | Background sync interval in seconds |
| `PROBE_MIN_INTERVAL` | `10` | Min probe interval in seconds |
| `PROBE_MAX_INTERVAL` | `20` | Max probe interval in seconds |
| `MAX_STALENESS` | `120` | Max staleness before warning (seconds) |
| `REQUIRE_SYNC` | `true` | Require successful NTP sync before serving |
| `SELECTION_STRATEGY` | `rtt_min` | Selection algorithm. `rtt_min` is a **backwards-compatible alias** for the accuracy-first / median-consensus algorithm (RTT is only a tiebreaker); `accuracy_first` is also accepted |
| `MAX_OFFSET_SKEW_MS` | `1000` | Outlier threshold in milliseconds |
| `MONOTONIC_OUTPUT` | `true` | Enable monotonic time clamping |
| `OFFSET_BIAS_MS` | `0` | Manual time offset bias |
| `ASYMMETRY_BIAS_MS` | `0` | Manual asymmetry bias |

### Logging Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `LOG_LEVEL` | `info` | Log level (trace, debug, info, warn, error) |
| `LOG_FORMAT` | `json` | Log format (json, pretty) |

### Message Configuration (UTF-8 / Persian Support)

| Variable | Default | Description |
|----------|---------|-------------|
| `MSG_OK` | `done` | Success message |
| `MSG_OK_CACHE` | `done` | Success message when serving from cache |
| `MSG_ERROR` | `error` | Generic error message |
| `ERROR_TEXT_NO_SYNC` | `Service not yet synchronized with NTP` | Not-synced error text |
| `ERROR_TEXT_INTERNAL` | `Internal server error` | Internal error text |
| `ERROR_TEXT_TIMEOUT` | `Request timeout` | Timeout error text |

**Persian Example:**
```bash
export MSG_OK="انجام شد"
export MSG_ERROR="خطا"
export ERROR_TEXT_NO_SYNC="سرویس هنوز با NTP همگام نشده است"
```

## Building

### Development Build

```bash
cargo build
```

### Release Build

```bash
cargo build --release
```

### Run Tests

```bash
cargo test
```

### Run Locally

```bash
cargo run
```

Or with custom configuration:

```bash
export NTP_SERVERS="time.google.com:123"
export LOG_LEVEL=debug
export LOG_FORMAT=pretty
cargo run
```

## Docker

### Build Image

```bash
docker build -t ntp-time-api:latest .
```

### Run Container

```bash
docker run -p 8080:8080 \
  -e NTP_SERVERS="time.google.com:123,time.cloudflare.com:123" \
  -e LOG_LEVEL=info \
  ntp-time-api:latest
```

### Test Endpoints

```bash
curl http://localhost:8080/time
curl http://localhost:8080/healthz
curl http://localhost:8080/metrics
```

## Kubernetes Deployment

### Apply Manifests

```bash
kubectl apply -f k8s/configmap.yaml
kubectl apply -f k8s/deployment.yaml
kubectl apply -f k8s/service.yaml
```

### Optional: ServiceMonitor for Prometheus Operator

```bash
kubectl apply -f k8s/servicemonitor.yaml
```

### Verify Deployment

```bash
kubectl get pods -l app=ntp-time-api
kubectl logs -l app=ntp-time-api -f
```

### Test Service

```bash
kubectl port-forward svc/ntp-time-api 8080:80
curl http://localhost:8080/time
```

## Metrics

The service exposes Prometheus metrics at `/metrics`:

### HTTP Metrics

- `http_requests_total{method,path,status}` - Total HTTP requests
- `http_request_duration_seconds_bucket{method,path}` - Request duration histogram
- `http_inflight_requests` - Current in-flight requests

### NTP Metrics

- `ntp_sync_total` - Total NTP sync attempts
- `ntp_sync_errors_total` - Total failed sync attempts
- `ntp_last_sync_timestamp_seconds` - Unix timestamp of last successful sync
- `ntp_staleness_seconds` - Seconds since last successful sync
- `ntp_offset_seconds` - Current NTP time offset
- `ntp_rtt_seconds` - NTP round-trip time histogram
- `ntp_server_up{server}` - Upstream NTP source health status (1=up, 0=down)
- `ntp_server_rtt_milliseconds{server}` - Per-upstream-source RTT
- `ntp_consecutive_failures` - Consecutive sync failure count

### UDP NTP Server Metrics (when `NTP_SERVER_ENABLED=true`)

These describe the local UDP NTP server (inbound), and are prefixed `ntp_udp_server_*` to
distinguish them from the upstream-source metrics above (renamed from the former `ntp_server_*`):

- `ntp_udp_server_requests_total` - UDP NTP requests received
- `ntp_udp_server_responses_total` - UDP NTP responses sent
- `ntp_udp_server_errors_total` - UDP NTP errors (malformed packets, send failures, rate-limited drops)
- `ntp_udp_server_unsynced_responses_total` - Responses sent while unsynced (LI=3, Stratum=16)

### Build Info

- `build_info{version,git_sha}` - Build information

## Performance

- **Response Time**: < 1ms for `/time` endpoint (hot path)
- **Memory**: ~10-20 MB RSS (typical)
- **CPU**: ~1-2% under moderate load (1000 req/s)
- **Throughput**: > 10,000 req/s on modern hardware

## Security

- Runs as non-root user (distroless `nonroot`, UID 65532; no shell in image)
- Read-only root filesystem
- All capabilities dropped
- Request timeouts enforced
- Body size limits enforced
- Graceful shutdown on SIGTERM

## Development

### Project Structure

```
├── src/
│   ├── main.rs              # Entry point, background loops
│   ├── config.rs            # Configuration management
│   ├── errors.rs            # Error types
│   ├── timebase.rs          # Lock-free monotonic time model
│   ├── performance.rs       # TimeCache (zero-copy JSON) + LockFreeMetrics
│   ├── metrics.rs           # Prometheus metrics
│   ├── http/
│   │   ├── mod.rs           # HTTP router (fast/slow split, CORS, rate limit)
│   │   ├── handlers.rs      # Endpoint handlers
│   │   ├── middleware.rs    # HTTP middleware (metrics tracking)
│   │   ├── websocket.rs     # WebSocket streaming (/stream)
│   │   └── state.rs         # Application state
│   └── ntp/
│       ├── mod.rs           # NTP module re-exports
│       ├── sync.rs          # NTP sync logic (parallel query + sticky selection)
│       ├── selection.rs     # Server selection (accuracy-first)
│       ├── stats.rs         # Per-server statistics
│       ├── protocol.rs      # RFC 5905 NTP packet codec (encode/decode)
│       └── server.rs        # Optional UDP NTP server (Stratum 2)
├── tests/
│   └── integration_api.rs   # Placeholder (real tests are inline in src/http/mod.rs;
│                            #  full E2E harness is planned as P0-5)
├── k8s/                     # Kubernetes manifests
├── Dockerfile               # Multi-stage build → distroless nonroot
└── Cargo.toml               # Dependencies
```

### Code Quality

```bash
# Format code
cargo fmt

# Lint
cargo clippy -- -D warnings

# Run tests with coverage
cargo test --all-features
```

## CI/CD

A GitHub Actions workflow is provided in `.github/workflows/ci.yml`:

- Runs on every push and PR
- Checks formatting (`cargo fmt --check`)
- Runs linter (`cargo clippy`)
- Runs tests (`cargo test`)
- Builds release binary
- (Optional) Builds Docker image

## Troubleshooting

### Service returns 503 on /time

- Check if `REQUIRE_SYNC=true` and service hasn't synced yet
- Check NTP server connectivity: `curl http://localhost:8080/metrics | grep ntp_sync_errors`
- Check logs: `kubectl logs -l app=ntp-time-api`

### Time is incorrect

- Verify NTP servers are reachable
- Check `ntp_staleness_seconds` metric
- Ensure `OFFSET_BIAS_MS` is set correctly (default: 0)

### Pod keeps restarting

- Check if startup probe is failing due to NTP timeout
- Increase `startupProbe.failureThreshold` in deployment
- Verify NTP servers in ConfigMap are valid

### Metrics not scraped

- Verify Prometheus annotations on pod
- Or apply ServiceMonitor if using Prometheus Operator
- Check `/metrics` endpoint manually

## Benchmarking

Performance testing tools are provided for both HTTP and WebSocket endpoints:

### HTTP/REST Benchmark

```bash
# Run default benchmark (1000 requests, 10 concurrent)
./benchmark.sh

# Custom parameters
./benchmark.sh http://localhost:8080/time 5000 50
```

**Example output:**
```
Requests/sec:      1,848.96
P50 Latency:       0.51ms
P95 Latency:       0.80ms
P99 Latency:       1.47ms
```

### WebSocket Benchmark

```bash
# Install Python websockets library
pip install websockets

# Run default benchmark (10 seconds, 1 connection)
./benchmark_websocket.py

# Custom parameters
./benchmark_websocket.py --duration 30 --connections 10
```

**Example output:**
```
Messages Received:
  Total:           300
  Rate:            30.0 msg/s

Message Latency (ms):
  Avg:             1.234
  P95:             2.100
  P99:             3.500
```

**See also**: `PROTOCOL_COMPARISON.md` for detailed performance analysis of all protocols.

## License

MIT OR Apache-2.0

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make your changes
4. Run tests: `cargo test`
5. Run linter: `cargo clippy -- -D warnings`
6. Format code: `cargo fmt`
7. Submit a pull request

## Support

For issues and questions:
- Open an issue on GitHub
- Check existing issues for similar problems
