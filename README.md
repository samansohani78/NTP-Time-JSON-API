# NTP Time JSON API

A **production-oriented, general-purpose** HTTP service that returns NTP-derived time as JSON, built
with Rust 1.92. It is **not yet financial/time-critical production-ready** — that requires completing
the P0 tasks in [`PRODUCTION_ACCURACY_PLAN.md`](PRODUCTION_ACCURACY_PLAN.md).

> **Current limitations (see [`PRODUCTION_ACCURACY_PLAN.md`](PRODUCTION_ACCURACY_PLAN.md)):**
> - ~~Upstream T2/T3 reconstructed from offset/delay~~ — **fixed (P0-1/P0-2)**: T2/T3 and root
>   fields are now read directly from NTP packet bytes via the in-house `PacketNtpClient`.
> - ~~UDP NTP server advertises `root_dispersion = 0`~~ — **fixed (P0-3)**: UDP replies now carry
>   honest `root_delay` (upstream + local RTT) and `root_dispersion` (RFC 5905 §11.2 formula:
>   upstream dispersion + precision + PHI×age + RTT/2, clamped to `MAX_ROOT_DISPERSION_MS`).
> - ~~There is no time-quality / uncertainty envelope and no serve/stop SLA~~ — **fixed (P0-4)**:
>   `/status`, `/time/full`, quality response headers (`X-Time-Source`, `X-Time-Serve-State`,
>   `X-Time-Uncertainty-Ms`, `X-Time-Stratum`, `X-Time-Staleness-Ms`, `X-Time-Selected-Server`),
>   and the serve/stop policy (503 when uncertainty exceeds
>   `SERVE_OK_MAX_UNCERTAINTY_MS`/`SERVE_DEGRADED_MAX_UNCERTAINTY_MS`) are all live.
>   `/readyz` gates on `READINESS_MAX_UNCERTAINTY_MS` (250 ms default) after first sync.
> - ~~There is no secure manual time-override path~~ — **done (P1-7)**: `/admin/time/override`
>   (POST/GET/DELETE) is live; requires `ADMIN_API_ENABLED=true` and `ADMIN_API_TOKEN`;
>   bearer-token auth with constant-time comparison; force, TTL, and jump-limit guards.
> - ~~There is no full end-to-end CI~~ — **done (P0-5)**: real E2E harness (`tests/e2e_*.rs`)
>   exercises HTTP, UDP NTP, WebSocket, and metrics endpoints against an in-process server using a
>   mock NTP upstream; CI `e2e` job runs on every push. Use `make e2e` locally.
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

> **Done (P1-6 + P1F-12):** The weighted-median selector (`WeightedMedianSelector`) with per-sample
> root-distance λ scoring, quorum gate, and provider-group cap is live. Marzullo
> interval-intersection pre-filter (P1F-12) runs before the weighted-median step: falsetickers are
> discarded and competing clusters fail closed. See `src/ntp/selection.rs`.

### Probe Behavior

Critical for Kubernetes: probes are designed so NTP failures don't kill pods after initial sync.

- **`/healthz`**: Always returns 200 if process is alive
- **`/readyz`**: Returns 503 before first sync (if `REQUIRE_SYNC=true`); after first sync, returns 503 when `uncertainty_ms > READINESS_MAX_UNCERTAINTY_MS` (default 250 ms), otherwise 200
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

**Quality response headers (P0-4):**
- `X-Time-Source: ntp` | `degraded` | `unsynced`
- `X-Time-Serve-State: ok` | `degraded` | `stopped` | `unsynced`
- `X-Time-Uncertainty-Ms: 4.872` (omitted when unsynced)
- `X-Time-Stratum: 2` (omitted when unsynced)
- `X-Time-Staleness-Ms: 1200` (omitted when unsynced)
- `X-Time-Selected-Server: time.google.com:123` (omitted when unsynced)

**Before First Sync (REQUIRE_SYNC=true):**
```json
{
  "message": "error",
  "status": 503,
  "data": 0,
  "error": "Service not yet synchronized with NTP"
}
```

**Serve/stop policy (P0-4):** after first sync, if computed uncertainty exceeds
`SERVE_OK_MAX_UNCERTAINTY_MS` (default 50 ms) and `ALLOW_DEGRADED=false` (default), `/time` returns
503 with `serve_state: "stopped"` to prevent serving low-quality time.

### `GET /time/full`

Enriched time response. Same policy as `/time` but body includes quality fields. Runs on the slow
router (full middleware, not zero-copy cache). Not backward-compatible — use `/time` + headers if
you need stability.

```json
{
  "message": "done",
  "status": 200,
  "data": 1704067200000,
  "source": "ntp",
  "serve_state": "ok",
  "uncertainty_ms": 4.87,
  "staleness_ms": 1200,
  "stratum": 2,
  "selected_server": "time.google.com:123",
  "leap": 0
}
```

### `GET /status`

Operational quality envelope. Always returns 200 regardless of serve state — read `serve_state` to
determine whether `/time` would return 200 or 503.

```json
{
  "source": "ntp",
  "serve_state": "ok",
  "uncertainty_ms": 4.87,
  "staleness_ms": 1200,
  "stratum": 2,
  "selected_server": "time.google.com:123",
  "leap": 0,
  "ntp_synced": true
}
```

### Admin API (P1-7, requires `ADMIN_API_ENABLED=true`)

All admin routes return 404 when disabled. Auth: `Authorization: Bearer <ADMIN_API_TOKEN>`. Missing or wrong token returns 401 with identical bodies (no oracle).

**`POST /admin/time/override`** — Set a manual time override.
```json
{ "epoch_ms": 1704067200000, "ttl_seconds": 60, "reason": "operator note", "force": false }
```
Returns 200 on success. `force: true` bypasses the jump limit (`MANUAL_OVERRIDE_MAX_JUMP_MS`) and requires `MANUAL_OVERRIDE_ALLOW_FORCE=true`.

**`GET /admin/time/override`** — Get the current override status (active or not).

**`DELETE /admin/time/override`** — Cancel the active override and revert to NTP time.

### `GET /healthz`

Liveness probe - always returns 200 if process is alive.

### `GET /readyz`

Readiness probe. Returns 503 before first sync (if `REQUIRE_SYNC=true`). After first sync, returns 503 when `uncertainty_ms > READINESS_MAX_UNCERTAINTY_MS` (default 250 ms), otherwise 200.

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

### Quality / SLA Configuration (P0-4)

| Variable | Default | Description |
|----------|---------|-------------|
| `ALLOW_DEGRADED` | `false` | When false, uncertainty > `SERVE_OK_MAX_UNCERTAINTY_MS` triggers 503. When true, uncertainty up to `SERVE_DEGRADED_MAX_UNCERTAINTY_MS` returns 200 with `source="degraded"`. |
| `SERVE_OK_MAX_UNCERTAINTY_MS` | `50` | Max uncertainty (ms) for `serve_state="ok"` |
| `SERVE_DEGRADED_MAX_UNCERTAINTY_MS` | `250` | Max uncertainty (ms) to serve at all (when `ALLOW_DEGRADED=true`). Must be > `SERVE_OK_MAX_UNCERTAINTY_MS`. |
| `READINESS_MAX_UNCERTAINTY_MS` | `250` | Max uncertainty (ms) for `/readyz` to return 200 after first sync |

### Replica Identity Configuration (P1-8)

| Variable | Default | Description |
|----------|---------|-------------|
| `REPLICA_ID` | `$HOSTNAME` or `replica-<pid>` | Unique identifier for this replica, attached to all replica-labeled Prometheus metrics. In Kubernetes, set via downward API (`metadata.name`) to use the pod name. Max 128 characters. |

### Interval-Intersection Selection Configuration (P1F-12)

| Variable | Default | Description |
|----------|---------|-------------|
| `NTP_INTERVAL_SELECTION_ENABLED` | `true` | Enable Marzullo/interval-intersection pre-filter before the weighted-median step. When true, candidates whose uncertainty intervals don't overlap the consensus region (falsetickers) are discarded; ambiguous competing clusters cause fail-closed. Set to `false` to disable (not recommended in production). |

### Advanced Selection Configuration (P1-6)

| Variable | Default | Description |
|----------|---------|-------------|
| `MIN_QUORUM` | `2` | Minimum agreeing servers required for a valid sync |
| `MAX_STRATUM` | `4` | Hard-reject servers at or above this stratum |
| `MAX_ROOT_DISTANCE_MS` | `500` | Hard-reject servers whose λ (root distance) exceeds this value (ms) |
| `MAX_SAMPLE_AGE_SECS` | `60` | Hard-reject samples older than this (seconds) |
| `REJECT_LEAP_ALARM` | `true` | Hard-reject servers with leap indicator = 3 (clock unsynchronized) |
| `NTP_PROVIDER_GROUPS` | `` | Override provider-group assignment; format: `server1=group1,server2=group2` |
| `PROVIDER_GROUP_MAX_FRACTION` | `0.5` | Fraction threshold above which a single provider group triggers uncertainty doubling |
| `MAX_CONSECUTIVE_FAILURES` | `10` | Number of consecutive sync failures before `/readyz` reports unhealthy |

### Admin / Manual Override Configuration (P1-7)

| Variable | Default | Description |
|----------|---------|-------------|
| `ADMIN_API_ENABLED` | `false` | Enable the admin API (`/admin/*`). When false, admin routes are not registered (Axum returns 404). |
| `ADMIN_API_TOKEN` | *(required if enabled)* | Bearer token for admin endpoint authentication. Startup fails if enabled but token is empty. |
| `MANUAL_OVERRIDE_MAX_TTL_SECS` | `300` | Maximum TTL for a manual time override (seconds) |
| `MANUAL_OVERRIDE_MAX_JUMP_MS` | `5000` | Maximum allowed clock jump for override without `force=true` (ms) |
| `MANUAL_OVERRIDE_ALLOW_FORCE` | `false` | Allow `force=true` in override requests (bypasses jump limit) |
| `MANUAL_OVERRIDE_DISPERSION_MS` | `1000` | Uncertainty advertised while a manual override is active (ms) |

### UDP NTP Server Configuration

| Variable | Default | Description |
|----------|---------|-------------|
| `NTP_SERVER_ENABLED` | `false` | Enable the optional Stratum-2 UDP NTP server. Requires `CAP_NET_BIND_SERVICE` in Kubernetes when binding to port 123. |
| `NTP_SERVER_ADDR` | `0.0.0.0:123` | UDP NTP server bind address |
| `NTP_SERVER_MAX_ROOT_DISPERSION_MS` | `16000` | Maximum root_dispersion the UDP server will advertise (ms) |
| `NTP_SERVER_MAX_PACKET_SIZE` | `1024` | Maximum inbound UDP packet size accepted (bytes; minimum 48) |

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
cargo test        # all tests: unit + E2E (213 tests)
make e2e          # E2E tests only: HTTP, UDP NTP, WebSocket, metrics (39 tests)
make ci           # fmt-check + clippy + all tests (same as CI)
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

### Selection / Uncertainty Metrics (P1-6)

- `ntp_selection_quorum_size` — count of servers agreeing with the weighted-median consensus
- `ntp_selection_falsetickers_total` — cumulative count of candidates hard-rejected by gates
- `ntp_selection_rejected_total{reason}` — per-reason rejection counter (stratum, leap, age, distance, jitter)
- `ntp_sample_uncertainty_milliseconds{server}` — per-upstream λ (root distance) at last sync
- `ntp_combined_uncertainty_milliseconds` — selected server's combined uncertainty after provider-cap inflation
- `ntp_selection_single_provider` — 1 when one provider group holds > 50% of agreers (uncertainty doubled)

### Manual Override Metrics (P1-7)

- `manual_override_active` — 1 when a manual override is active, 0 otherwise
- `manual_override_total` — cumulative count of overrides set since process start
- `manual_override_expiry_timestamp_seconds` — Unix timestamp when the current override expires; 0 if none
- `manual_override_rejected_total{reason}` — rejected override requests by reason (e.g., `jump_too_large`, `force_not_allowed`)

### Time-Quality Envelope Metrics (P0-4)

- `time_uncertainty_milliseconds` - Computed time uncertainty (ms) from most recent NTP sync (RFC 5905 §11.2)
- `time_source_mode` - Time source mode: 0=ntp, 1=degraded, 2=unsynced
- `time_serve_state` - Serve state: 0=ok, 1=degraded, 2=stopped, 3=unsynced

### Replica Drift Metrics (P1-8)

All four metrics below are labeled `{replica_id="..."}` so Prometheus can aggregate across replicas to detect drift. Each replica reports only its own state — no gossip or coordination.

- `time_replica_offset_milliseconds{replica_id}` — NTP offset from most recent sync (ms)
- `time_replica_uncertainty_milliseconds{replica_id}` — Combined uncertainty, provider-cap inflated (ms)
- `time_replica_serve_state{replica_id}` — Serve state: 0=ok, 1=degraded, 2=stopped, 3=unsynced
- `time_replica_source_mode{replica_id}` — Source mode: 0=ntp, 1=degraded, 2=unsynced, 3=manual

**Prometheus alerts** (`k8s/prometheus-rules.yaml`):

| Alert | Condition | Severity |
|-------|-----------|----------|
| `NtpTimeReplicaHighUncertainty` | `time_replica_uncertainty_milliseconds > 250` for 5 min | warning |
| `NtpTimeReplicaStopped` | `time_replica_serve_state > 1` for 2 min | critical |
| `NtpTimeReplicaSpreadHigh` | `max - min` offset across replicas > 100 ms for 5 min | warning |
| `NtpTimeSingleProvider` | `ntp_selection_single_provider == 1` for 10 min | warning |

### Interval-Intersection Metrics (P1F-12)

After each NTP sync, the following metrics reflect the Marzullo sweep result:

- `ntp_intersection_truechimers` — gauge: count of truechimers (servers whose intervals span the consensus region)
- `ntp_intersection_falsetickers_total` — counter: cumulative count of falsetickers discarded across all syncs
- `ntp_intersection_width_milliseconds` — gauge: width of the intersection region (ms); wider = more uncertainty
- `ntp_intersection_failures_total{reason}` — counter: intersection failures by reason (`no_intersection`, `ambiguous_cluster`)
- `ntp_intersection_ambiguous_clusters` — gauge: number of competing clusters found (≥ 2 means AmbiguousCluster was detected)

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
│   ├── integration_api.rs   # Redirect comment → real E2E harness in e2e_*.rs (P0-5 done)
│   ├── e2e_http.rs          # HTTP endpoint E2E tests
│   ├── e2e_metrics.rs       # Prometheus metrics E2E tests
│   ├── e2e_ntp_udp.rs       # UDP NTP server E2E tests
│   ├── e2e_websocket.rs     # WebSocket E2E tests
│   ├── e2e_manual_override.rs # Admin manual-override E2E tests (P1-7)
│   └── common/mod.rs        # Shared E2E helpers (mock NTP upstream, spawn helpers)
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
