# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

> **Readiness & roadmap.** This is a production-oriented **general-purpose** time API; it is **not yet
> financial/time-critical production-ready**. P0/P1/P1F/v1.1.0 tasks are complete (P0-1 through P0-5,
> P1-6, P1-7, P1-8, P1F-12, v1.1.0 holdover-first). For financial / latency-sensitive / authoritative
> time-source work, additional external items are required: NTS (authenticated NTP), host-clock discipline
> (chrony/ntpd), and a formal SLA/security sign-off. See `PRODUCTION_ACCURACY_PLAN.md` and
> `PROJECT_PLAN.md` for history and status. Next: P2-9 cleanup (if planned).

## Commands

```bash
# Build
cargo build           # dev build
cargo build --release # release build

# Run
cargo run             # start locally (default: 0.0.0.0:8080)

# Test
cargo test            # run all tests (unit + inline integration + E2E; 239 tests total)
cargo test <name>     # run a specific test by name
make e2e              # E2E tests only (HTTP + UDP NTP + WebSocket + metrics; 48 tests; manual override suite via cargo test --test e2e_manual_override)

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

- **`src/main.rs`** — Entry point; spawns three background tasks: `sync_loop` (NTP sync every 30s), `probe_loop` (jittered server health polling), and optionally an NTP server. On startup, loads persisted state if `TIME_STATE_PERSIST_ENABLED=true`. Handles graceful shutdown on SIGTERM/Ctrl+C.
- **`src/config.rs`** — All config read from env vars at startup via `Config::from_env()`. Validates constraints. Includes `QualityConfig.strict_sla_mode` and `PersistConfig`.
- **`src/timebase.rs`** — Monotonic time model with optional `TimeCache` (zero-copy pre-serialized JSON).
- **`src/performance.rs`** — `TimeCache` (pre-built JSON bytes updated on each tick) and `LockFreeMetrics`.
- **`src/persist.rs`** — Atomic JSON state persistence: `PersistedState`, `save_state()` (write-then-rename), `load_state()`. Used for holdover across restarts.
- **`src/http/`** — Axum router (`mod.rs`), request handlers (`handlers.rs`), middleware (`middleware.rs`), shared `AppState` (`state.rs`), WebSocket streaming (`websocket.rs`).
- **`src/ntp/`** — NTP client logic: `client.rs` (`NtpClient` trait + `PacketNtpClient` + `MockNtpClient`; reads measured T2/T3/root fields from packet bytes), `sync.rs` (query + filtering; `NtpSyncer` holds `Arc<dyn NtpClient>`, injectable for tests; `sync()` returns `SyncOutcome` with diagnostics), `selection.rs` (`WeightedMedianSelector`: Marzullo interval-intersection pre-filter (P1F-12) → truechimers only → λ-weighted median + quorum gate + provider-group cap; P1-6 + P1F-12 complete; `SELECTION_STRATEGY=rtt_min` env is a backwards-compat alias retained but no longer drives the algorithm), `stats.rs` (per-server health + jitter ring-buffer), `protocol.rs` (raw NTP packet encode/decode), `server.rs` (optional UDP NTP server mode).
- **`src/metrics.rs`** — Prometheus metrics definitions.
- **`src/errors.rs`** — Error types.

### Key Design Decisions

- **Holdover-first design (v1.1.0)**: After any seed (NTP, manual override, or persisted state load), `/time` always returns HTTP 200. Quality is communicated via `X-Time-*` headers and `/time/full` body fields, not via the HTTP status code. HTTP 503 is only returned when: (a) completely uninitialized (no seed) + `REQUIRE_SYNC=true`, or (b) `STRICT_SLA_MODE=true` and uncertainty exceeds the configured stop threshold.
- **State machine** (`compute_quality()`): MANUAL (override active) → SYNCED (fresh NTP, low uncertainty) → DEGRADED (NTP seeded, uncertainty in band) → HOLDOVER (NTP seeded, stale or high uncertainty) → UNSYNCED (no seed). `source` and `serve_state` JSON fields reflect this machine.
- **Strict SLA mode** (`STRICT_SLA_MODE=false` default): Opt-in for financial/critical deployments. When true, restores old hard-stop 503 behavior for high-uncertainty states.
- **Persistence** (`TIME_STATE_PERSIST_ENABLED=false` default): When enabled, saves last-good NTP state to `TIME_STATE_FILE` after each sync (atomic write-then-rename). On startup, loads this file to seed TimeBase before the first NTP sync completes — enables holdover across container restarts.
- **Manual seed**: When `POST /admin/time/override` is called and NTP has never synced, the override permanently seeds TimeBase (in addition to setting the TTL-limited override). After the override expires or is deleted, the service continues serving from holdover.
- **Probe vs Sync loops**: `sync_loop` updates the timebase; `probe_loop` keeps per-server RTT stats fresh with random jitter to avoid thundering herd.
- **NTP server mode** (`NTP_SERVER_ENABLED`): Disabled by default. When enabled, listens on UDP (default `0.0.0.0:123`) and requires `CAP_NET_BIND_SERVICE` in Kubernetes.
- **E2E tests** (`tests/e2e_*.rs`, P0-5): Real harness — spawns an in-process server on `:0` with a mock upstream NTP server. Covers HTTP, UDP NTP, WebSocket, and metrics. Run with `make e2e`. `tests/integration_api.rs` is now a redirect comment pointing to these files.
- **Quality headers**: All 200 `/time` responses carry `X-Time-Source`/`X-Time-Serve-State`/`X-Time-Uncertainty-Ms`/`X-Time-Stratum`/`X-Time-Staleness-Ms` headers (plus optional `X-Time-Selected-Server`). In holdover state, uncertainty/stratum/staleness headers are omitted when unknown.
- **Probe behavior for Kubernetes**: After first seed (NTP or persisted), `/startupz` always returns 200. `/readyz` returns 200 unless uncertainty exceeds `READINESS_MAX_UNCERTAINTY_MS` (default 250 ms). NTP sync failures after first sync do not kill pods.
- **jemalloc**: Enabled globally (`[global_allocator]`) for ~10–20% throughput improvement.

### Configuration

All configuration is environment variables — see `src/config.rs` `Config::from_env()` or the README for the full list. Key vars: `ADDR`, `NTP_SERVERS`, `SYNC_INTERVAL`, `REQUIRE_SYNC`, `LOG_FORMAT` (json/pretty), `NTP_SERVER_ENABLED`, `STRICT_SLA_MODE` (default: `false`), `ALLOW_DEGRADED`, `SERVE_OK_MAX_UNCERTAINTY_MS`, `SERVE_DEGRADED_MAX_UNCERTAINTY_MS`, `READINESS_MAX_UNCERTAINTY_MS`, `REPLICA_ID` (default: `$HOSTNAME` or `replica-<pid>`), `NTP_INTERVAL_SELECTION_ENABLED` (default: `true` — Marzullo pre-filter), `TIME_STATE_PERSIST_ENABLED` (default: `false`), `TIME_STATE_FILE` (default: `/var/lib/ntp-time-json-api/state.json`).
