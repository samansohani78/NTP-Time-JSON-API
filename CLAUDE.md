# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> **Readiness & roadmap.** This is a production-oriented **general-purpose** time API; it is **not yet
> financial/time-critical production-ready**. P0/P1/P1F tasks are complete (P0-1 through P0-5, P1-6,
> P1-7, P1-8, P1F-12). For financial / latency-sensitive / authoritative time-source work, additional
> external items are required: NTS (authenticated NTP), host-clock discipline (chrony/ntpd), and a
> formal SLA/security sign-off. See `PRODUCTION_ACCURACY_PLAN.md` and `PROJECT_PLAN.md` for history
> and status. Next: P2-9 cleanup (if planned).

## Commands

```bash
# Build
cargo build           # dev build
cargo build --release # release build

# Run
cargo run             # start locally (default: 0.0.0.0:8080)

# Test
cargo test            # run all tests (unit + inline integration + E2E; 213 tests total)
cargo test <name>     # run a specific test by name
make e2e              # E2E tests only (HTTP + UDP NTP + WebSocket + metrics; 39 tests; manual override suite via cargo test --test e2e_manual_override)

# Code quality
cargo fmt --all                                         # format
cargo fmt --all -- --check                             # check formatting
cargo clippy --all-targets --all-features -- -D warnings  # lint

# CI equivalent
make ci               # fmt-check + lint + test (all)
make dev-check        # fmt + check + test

# Docker
make docker-build     # build image
make docker-up        # start with docker-compose
make docker-down      # stop
```

## Architecture

**Stack**: Rust + Axum (HTTP) + Tokio (async) + `PacketNtpClient` (in-house async UDP NTP client in `src/ntp/client.rs` — reads measured T2/T3 + root_delay/root_dispersion directly from packet bytes; `rsntp` removed as of P0-1/P0-2) + prometheus-client + jemalloc

### Time Model

The core design avoids OS wall clock authority entirely. On each NTP sync, `timebase.rs` records `base_ntp_epoch_ms` (NTP-derived epoch) and `base_instant_nanos` (monotonic `Instant` offset from a global `REFERENCE_INSTANT`). Every `/time` request computes:

```
now_ms = base_epoch_ms + (Instant::now() - REFERENCE_INSTANT - base_instant_nanos)
```

All time fields in `TimeBase` are atomics (`AtomicI64`, `AtomicU64`) — the hot read path is entirely lock-free.

### Module Overview

- **`src/main.rs`** — Entry point; spawns three background tasks: `sync_loop` (NTP sync every 30s), `probe_loop` (jittered server health polling), and optionally an NTP server. Handles graceful shutdown on SIGTERM/Ctrl+C.
- **`src/config.rs`** — All config read from env vars at startup via `Config::from_env()`. Validates constraints (e.g., `PROBE_MIN_INTERVAL ≤ PROBE_MAX_INTERVAL`).
- **`src/timebase.rs`** — Monotonic time model with optional `TimeCache` (zero-copy pre-serialized JSON).
- **`src/performance.rs`** — `TimeCache` (pre-built JSON bytes updated on each tick) and `LockFreeMetrics`.
- **`src/http/`** — Axum router (`mod.rs`), request handlers (`handlers.rs`), middleware (`middleware.rs`), shared `AppState` (`state.rs`), WebSocket streaming (`websocket.rs`).
- **`src/ntp/`** — NTP client logic: `client.rs` (`NtpClient` trait + `PacketNtpClient` + `MockNtpClient`; reads measured T2/T3/root fields from packet bytes), `sync.rs` (query + filtering; `NtpSyncer` holds `Arc<dyn NtpClient>`, injectable for tests; `sync()` returns `SyncOutcome` with diagnostics), `selection.rs` (`WeightedMedianSelector`: Marzullo interval-intersection pre-filter (P1F-12) → truechimers only → λ-weighted median + quorum gate + provider-group cap; P1-6 + P1F-12 complete; `SELECTION_STRATEGY=rtt_min` env is a backwards-compat alias retained but no longer drives the algorithm), `stats.rs` (per-server health + jitter ring-buffer), `protocol.rs` (raw NTP packet encode/decode), `server.rs` (optional UDP NTP server mode).
- **`src/metrics.rs`** — Prometheus metrics definitions.
- **`src/errors.rs`** — Error types.

### Key Design Decisions

- **Probe vs Sync loops**: `sync_loop` updates the timebase; `probe_loop` keeps per-server RTT stats fresh with random jitter to avoid thundering herd.
- **NTP server mode** (`NTP_SERVER_ENABLED`): Disabled by default. When enabled, listens on UDP (default `0.0.0.0:123`) and requires `CAP_NET_BIND_SERVICE` in Kubernetes.
- **E2E tests** (`tests/e2e_*.rs`, P0-5): Real harness — spawns an in-process server on `:0` with a mock upstream NTP server. Covers HTTP, UDP NTP, WebSocket, and metrics. Run with `make e2e`. `tests/integration_api.rs` is now a redirect comment pointing to these files.
- **Serve/stop policy (P0-4)**: `/time` now enforces `SERVE_OK_MAX_UNCERTAINTY_MS` (default 50 ms). After first sync, if computed uncertainty exceeds the threshold and `ALLOW_DEGRADED=false`, `/time` returns 503 (`serve_state="stopped"`). `/status` always returns 200 and reports the full quality envelope. `/time/full` adds quality fields to the body. All 200 `/time` responses carry `X-Time-Source`/`X-Time-Serve-State`/`X-Time-Uncertainty-Ms`/`X-Time-Stratum`/`X-Time-Staleness-Ms` headers (plus optional `X-Time-Selected-Server`).
- **Probe behavior for Kubernetes**: After first sync, `/startupz` always returns 200. `/readyz` returns 200 unless uncertainty exceeds `READINESS_MAX_UNCERTAINTY_MS` (default 250 ms). NTP sync failures after first sync do not kill pods.
- **jemalloc**: Enabled globally (`[global_allocator]`) for ~10–20% throughput improvement.

### Configuration

All configuration is environment variables — see `src/config.rs` `Config::from_env()` or the README for the full list. Key vars: `ADDR`, `NTP_SERVERS`, `SYNC_INTERVAL`, `REQUIRE_SYNC`, `LOG_FORMAT` (json/pretty), `NTP_SERVER_ENABLED`, `ALLOW_DEGRADED`, `SERVE_OK_MAX_UNCERTAINTY_MS`, `SERVE_DEGRADED_MAX_UNCERTAINTY_MS`, `READINESS_MAX_UNCERTAINTY_MS`, `REPLICA_ID` (default: `$HOSTNAME` or `replica-<pid>`), `NTP_INTERVAL_SELECTION_ENABLED` (default: `true` — Marzullo pre-filter).
