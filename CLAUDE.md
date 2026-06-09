# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> **Readiness & roadmap.** This is a production-oriented **general-purpose** time API; it is **not yet
> financial/time-critical production-ready**. For any financial / latency-sensitive / authoritative
> time-source work, you **must** follow the production-hardening roadmap in
> `PRODUCTION_ACCURACY_PLAN.md` (P0/P1 tasks) and the status in `PROJECT_PLAN.md`. Key open gaps:
> upstream T2/T3 are reconstructed (not measured), the UDP server advertises `root_dispersion = 0`,
> there is no time-quality envelope, no secure manual override, and no full E2E CI. The first
> implementation task is **P0-1** (a packet-level NTP client replacing `rsntp`).

## Commands

```bash
# Build
cargo build           # dev build
cargo build --release # release build

# Run
cargo run             # start locally (default: 0.0.0.0:8080)

# Test
cargo test            # run all tests
cargo test <name>     # run a specific test by name

# Code quality
cargo fmt --all                                         # format
cargo fmt --all -- --check                             # check formatting
cargo clippy --all-targets --all-features -- -D warnings  # lint

# CI equivalent
make ci               # fmt-check + lint + test
make dev-check        # fmt + check + test

# Docker
make docker-build     # build image
make docker-up        # start with docker-compose
make docker-down      # stop
```

## Architecture

**Stack**: Rust + Axum (HTTP) + Tokio (async) + rsntp (NTP client — *current state*; planned to be
replaced by an in-house packet-level NTP client in **P0-1**, see `PRODUCTION_ACCURACY_PLAN.md`, so we
can read measured T2/T3 + upstream root_delay/root_dispersion) + prometheus-client + jemalloc

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
- **`src/ntp/`** — NTP client logic: `sync.rs` (query + outlier filtering), `selection.rs` (accuracy-first / median-consensus selection; RTT is only a tiebreaker — the `SELECTION_STRATEGY=rtt_min` env value is a backwards-compatible alias, not the algorithm), `stats.rs` (per-server health tracking), `protocol.rs` (raw NTP packet encode/decode), `server.rs` (optional UDP NTP server mode).
- **`src/metrics.rs`** — Prometheus metrics definitions.
- **`src/errors.rs`** — Error types.

### Key Design Decisions

- **Probe vs Sync loops**: `sync_loop` updates the timebase; `probe_loop` keeps per-server RTT stats fresh with random jitter to avoid thundering herd.
- **NTP server mode** (`NTP_SERVER_ENABLED`): Disabled by default. When enabled, listens on UDP (default `0.0.0.0:123`) and requires `CAP_NET_BIND_SERVICE` in Kubernetes.
- **Integration tests** (`tests/integration_api.rs`): Currently placeholder stubs. Real tests require a mock UDP NTP server.
- **Probe behavior for Kubernetes**: After first sync, `/readyz`, `/startupz`, and `/time` all return 200 permanently — NTP failures don't kill pods post-sync.
- **jemalloc**: Enabled globally (`[global_allocator]`) for ~10–20% throughput improvement.

### Configuration

All configuration is environment variables — see `src/config.rs` `Config::from_env()` or the README for the full list. Key vars: `ADDR`, `NTP_SERVERS`, `SYNC_INTERVAL`, `REQUIRE_SYNC`, `LOG_FORMAT` (json/pretty), `NTP_SERVER_ENABLED`.
