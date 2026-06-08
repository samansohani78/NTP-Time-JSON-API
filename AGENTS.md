# Repository Guidelines

## Project

Rust 2024 / `cargo` (single crate, no workspace). Production HTTP service that serves NTP-derived time as JSON, with WebSocket streaming and Prometheus metrics. Linux builds use `tikv-jemallocator` as the global allocator (see `src/main.rs:9-15`) — debug builds will not work on MSVC targets.

## Layout

- `src/main.rs` — bootstrap, router wiring, background NTP sync + probe loops, graceful shutdown.
- `src/http/` — `mod.rs` (router; see Hot-path note), `handlers.rs` (probe + `/time` + `/metrics` + `/performance`), `websocket.rs`, `state.rs`, `middleware.rs`.
- `src/ntp/` — `sync.rs` (multi-server sync + smart-sticky selection), `selection.rs` (outlier filter + RTT-min), `stats.rs` (per-server stats, auto-disable on `MAX_CONSECUTIVE_FAILURES`).
- `src/config.rs` — env-driven `Config::from_env()` (the only place most env vars are read).
- `src/timebase.rs`, `src/performance.rs`, `src/metrics.rs`, `src/errors.rs` — monotonic time model, lock-free metrics, Prometheus encoder.
- `tests/integration_api.rs` — **placeholder tests** (every test is `assert!(true, ...)`). Do not rely on them for behavior coverage.
- `k8s/`, `observability/`, `examples/` — deployment manifests and client examples.

## Build, test, verify

- `make ci` — full local check (`fmt-check` + `lint` + `test`). Run before opening a PR.
- `make dev-check` — quicker (`fmt` + `check` + `test`, skips clippy).
- `make run` / `make build` — `cargo run` / `cargo build --release`.
- `make fmt`, `make lint`, `make fmt-check` — formatting / clippy.
- `make docker-build` / `docker-up` / `docker-down` / `docker-logs`.
- Single-test filter: `cargo test --lib timebase` (or any test name).
- `lint` is strict: `cargo clippy --all-targets --all-features -- -D warnings`. Fix warnings, don't `#[allow(...)]` them unless intentional.

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy ... -D warnings`, `cargo test --all-features --verbose`, `cargo build --release`, `cargo audit`, and a Docker build scanned with Trivy. Note: CI's `cargo test` does **not** pass `--all-targets` like the Makefile does, so integration tests in `tests/` are silently skipped in CI anyway.

## Hot-path: router split (`src/http/mod.rs`)

Two routers merged together:
- **Fast path** (`/time`, `/`) — no middleware, no tracing, no body limit, no timeout, no metrics layer. Keep `/time` here. Moving it to the slow router will hurt latency.
- **Slow path** (`/healthz`, `/readyz`, `/startupz`, `/metrics`, `/performance`, `/stream`) — full middleware stack.

Rate limiting (`tower_governor`, 1000 rps / burst 100, per IP) is applied only in production builds, not when `create_router_for_test()` is used.

## Probe semantics (Kubernetes, do not change casually)

- `/healthz` — always 200 while the process is alive.
- `/readyz` — 503 until first successful NTP sync (when `REQUIRE_SYNC=true`), then always 200, even if NTP later fails. Designed so transient NTP outages don't kill pods.
- `/startupz` — same as `/readyz` (sync gate).
- `/time` — 503 before first sync if `REQUIRE_SYNC=true`; otherwise 200.

## Configuration

100% environment-driven. Add new HTTP/NTP/logging/message vars to `Config::from_env()` in `src/config.rs` and to the `Default for Config` impl under `#[cfg(test)]`. The full list with defaults is documented in `README.md`; do not duplicate it here.

WebSocket-specific vars (`WS_UPDATE_INTERVAL_MS`, `WS_MAX_DURATION_SECS`) are read once in `Config::from_env()` (`src/config.rs:WsConfig`) and exposed on `state.config.ws.{update_interval_ms, max_duration_secs}`. The per-connection handler reads from state, not from `std::env`.

NTP selection: `NtpSyncer` always queries **all** configured servers, applies outlier filtering (`MAX_OFFSET_SKEW_MS`), then uses RTT-min. The "smart sticky" policy in `src/ntp/sync.rs` only switches the current server when a new candidate is 50ms+ faster, to avoid flapping.

A server is auto-disabled after `MAX_CONSECUTIVE_FAILURES` (default 10) consecutive failures and re-enabled on the next success.

## Known doc/code drift (do not "fix" without owner input)

- `CLAUDE.md` at the repo root is gitignored (`.gitignore` line 34) and not part of the committed contract. Use `README.md` for the canonical public docs.

## End-to-end scripts (require a running service)

- `test_api.sh` — exercises endpoints; **requires `jq`** to be installed. Not a substitute for `cargo test`.
- `test_ntp_servers.sh` — uses `sntp` or `ntpdate` from the host; produces no PIDs/services.
- `test_ntp_failure.sh`, `benchmark.sh`, `benchmark_websocket.py`, `test_websocket.py` — manual/benchmarking helpers.

The distroless image has no shell, so `docker-compose.yml` sets `healthcheck: test: ["NONE"]` and relies on the Kubernetes probes. Don't try to wget/curl from inside the container.

## Code style

Rust 2024 with `rustfmt` defaults, 4-space indent, `snake_case` for files/modules/functions, `PascalCase` for types, `SCREAMING_SNAKE_CASE` for env-var names. `Cargo.lock` is in `.gitignore` but is also tracked — leave it that way (binary crate).

## Commits & PRs

Repo style is short, imperative, lower-case subjects (see `git log --oneline`: `fix issue 2 secend behind system`, `add test`, `compare time`). Keep commits single-purpose. For PRs: describe the behavior change, link issues, list verification commands (at minimum `make ci`), and include sample `curl`/logs when touching API or probe behavior.

## Security

Never commit secrets, `.env*` files, or env-specific credentials. `tikv-jemallocator` and `socket2` already bring native deps — when adding a new crate, watch for build-time system libraries.
