# PROJECT_PLAN.md

Current improvement and maintenance plan. Update this file after every architectural or plan-relevant change.

---

## Summary

Two distinct readiness states — do **not** conflate them:

- **General-purpose maintenance: COMPLETE** except one external blocker (Docker registry push, 5.1,
  needs registry credentials). All correctness bugs, test-coverage gaps, and previously-blocked
  maintenance items tracked in *this* file are resolved. The service is sound as a general-purpose
  time API.
- **Financial / time-critical production hardening: NOT COMPLETE.** The P0/P1 roadmap below (and in
  `PRODUCTION_ACCURACY_PLAN.md`) is active and unstarted. **The service is not production-ready for
  financial/time-critical use today.**

> **Why not financial-ready yet.** P0-1 through P0-5 are complete: rsntp is removed; T2/T3 and
> root fields are measured from packet bytes; the UDP server advertises honest `root_delay` and
> `root_dispersion`; time-quality envelope, `/status`, `/time/full`, serve/stop SLA, and quality
> response headers are all live; a real E2E harness (`tests/e2e_*.rs`) and CI `e2e` job are in
> place. P1-7 secure manual-override API is also complete (see below).
> See **`PRODUCTION_ACCURACY_PLAN.md`** (P0/P1/P2, phased) for the plan.

---

## Production-Hardening Tasks (active roadmap)

Full implementation-ready detail (structs, functions, tests, acceptance criteria, config) lives in
**`PRODUCTION_ACCURACY_PLAN.md`**. This table is the tracker. Status legend at bottom of file.

| Task | Pri | Status | Affected files | Acceptance criteria | Validation command |
|------|-----|--------|----------------|---------------------|--------------------|
| **P0-1** Packet-level async NTP client (real T1-T4) | P0 | done | `src/ntp/client.rs`*(new)*, `src/ntp/protocol.rs`, `src/ntp/mod.rs` | Client returns **measured** T2/T3 byte-identical to server reply; origin-mismatch / KoD / leap-alarm / zero-transmit / negative-delay / timeout all rejected; no `rsntp` in client | `cargo test ntp::client` |
| **P0-2** Carry real fields end-to-end | P0 | done | `src/ntp/sync.rs`, `src/ntp/selection.rs`, `src/http/state.rs`, `src/main.rs`, `src/timebase.rs` | `/performance` shows `timing_source:"measured"` + real `root_delay_ms`/`root_dispersion_ms`/`stratum`/`leap`; `last_sync_quality` populated each sync | `cargo test ntp:: && cargo run` + `curl /performance` |
| **P0-3** Honest UDP `root_delay`/`root_dispersion` | P0 | done | `src/ntp/server.rs`, `src/ntp/sync.rs`, `src/main.rs`, `src/metrics.rs`, `src/config.rs` | Synced UDP replies carry `root_dispersion>0` growing with sync age, bounded by `MAX_ROOT_DISPERSION_MS`; `root_delay>=upstream_root_delay` | `cargo test ntp::server` |
| **P0-4** Time-quality envelope + `/status` + serve/stop SLA | P0 | done | `src/http/handlers.rs`, `src/http/mod.rs`, `src/http/state.rs`, `src/http/websocket.rs`, `src/config.rs`, `src/metrics.rs`, `src/errors.rs` | `/status` full envelope; `/time` byte-compatible + quality headers; 503 at hard limits; `source` never silently wrong | `cargo test http:: ` + `curl /status` |
| **P0-5** Real integration / E2E harness + CI | P0 | done | `src/lib.rs`*(new)*, `src/main.rs`, `tests/common/mod.rs`*(new)*, `tests/e2e_*.rs`, `.github/workflows/ci.yml`, `Makefile` | placeholder `integration_api.rs` gone; HTTP+UDP+WS+metrics exercised vs running server; CI `e2e` job green | `make e2e` |
| **P1-6** Uncertainty-scored selection (weighted-median + quorum) | P1 | **done** | `src/ntp/selection.rs`, `src/ntp/sync.rs`, `src/ntp/stats.rs`, `src/config.rs`, `src/metrics.rs` | 15 adversarial unit tests green; min-RTT fallback removed; `/status` + `/time/full` expose `selection` diagnostics; 5 new E2E tests; 6 Prometheus metrics | `cargo test ntp::selection` + `make e2e` |
| **P1-7** Secure manual time-override API **(core requirement — P0/P1 boundary, "P1-high")** | P1-high | **done** *(merge requires security review)* | `src/timebase.rs`, `src/http/handlers_admin.rs`*(new)*, `src/http/mod.rs`, `src/http/middleware.rs`, `src/http/state.rs`, `src/ntp/server.rs`, `src/config.rs`, `src/metrics.rs`, `Cargo.toml` (+`subtle`) | 27 E2E tests green; disabled→404; bearer token with `subtle::ConstantTimeEq`; `MANUAL_OVERRIDE_MAX_TTL_SECS=300`, `MANUAL_OVERRIDE_MAX_JUMP_MS=5000`, `MANUAL_OVERRIDE_ALLOW_FORCE=false`; `source:"manual"` in HTTP+WS; UDP `MANU` refid; monotonic preserved across every transition; audit log at `warn` (no token); force bypass gated by `MANUAL_OVERRIDE_ALLOW_FORCE` | `cargo test --test e2e_manual_override` |
| **P1-8** Replica-drift visibility (no consensus) | P1 | **done** | `src/http/handlers.rs`, `src/config.rs`, `src/metrics.rs`, `k8s/prometheus-rules.yaml`*(new)*, `k8s/deployment.yaml`, `PROJECT_ARCHITECTURE.md` | `REPLICA_ID` config; `/status` + `/time/full` include `replica_id`, `selected_offset_ms`, `combined_uncertainty_ms`, `selected_provider`, `selection_state`; 4 replica Family metrics; 4 Prometheus alert rules; downward API in deployment; 8 new tests | `cargo test replica` + `make e2e` |
| **P1F-12** Marzullo / interval-intersection robustness follow-up to P1-6 | P1-followup | **done** | `src/ntp/selection.rs`, `src/config.rs`, `src/metrics.rs`, `src/main.rs`, `src/http/handlers.rs`, `tests/common/mod.rs` | Marzullo sweep pre-filter before weighted-median: truechimers/falsetickers, fail-closed on NoIntersection/AmbiguousCluster; `NTP_INTERVAL_SELECTION_ENABLED` config (default true); `IntersectionDiagnostics` in `/status` + `/time/full`; 5 new Prometheus metrics; 10 unit tests; 5 E2E tests | `cargo test ntp::selection && make e2e` |
| **P2-9** Cleanup: drop `rsntp`, retire tautological test, document/expose bias knobs, rename `SelectionStrategy` | P2 | todo | `Cargo.toml`, `src/ntp/selection.rs`, `src/config.rs`, `src/http/handlers.rs` | `rsntp` removed; `rfc5905_four_tuple_relations_hold` replaced by real wire-parse test; bias knobs surfaced in `/status` | `cargo build && cargo test` |
| **DOC-1** Reconcile public docs (`README.md`, `CLAUDE.md`) with reality + this roadmap — **do before OPS-1 commit** | P1 | todo | `README.md`, `CLAUDE.md` | No public doc claims unqualified "production-ready", "RTT-min", or rsntp-as-fine; see checklist in `PRODUCTION_ACCURACY_PLAN.md` §7.1 | `grep -nE "production-ready\|RTT-min\|SAMPLE_SERVERS_PER_SYNC\|UID 1000" README.md CLAUDE.md` → only intended hits |
| **OPS-1** Track plan/docs in `.gitignore` | P2 | todo | `.gitignore` | `git check-ignore PROJECT_PLAN.md` empty; 3 plan docs + `CLAUDE.md` addable | `git check-ignore -v PROJECT_PLAN.md` |
| **OPS-2** Fix misleading `fcd8895` commit message | P2 | todo *(needs approval — pushed)* | git history | history attributes the change correctly; **no force-push without approval** (commit is pushed) | `git log --oneline -3` |

> **Dependency order:** P0-1 → P0-2 → {P0-3 → P0-4, P1-6 → P1F-12}; P0-5 in parallel; P1-7 after P0-4 (+security review); P1-8 after P0-2. DOC-1 before OPS-1.
> **On P1-7 priority (manual override).** It is a **core product requirement**, not optional future
> work. It is sequenced *after* P0-4 **only because it technically depends on the time-quality
> envelope** (so manual mode can surface `source:"manual"` and a conservative uncertainty) and
> **must pass a dedicated security review** before merge — not because it is low importance. Treat it
> as the highest-priority P1 (the "P0/P1 boundary"); start it the moment P0-4 lands.
> **Human sign-off required** (see `PRODUCTION_ACCURACY_PLAN.md` §9): P1-7 **merge requires security review** (coding may start; contract is locked in §4 of the accuracy plan). D1/D2/D6/D8 decisions are now locked in the security contract. OPS-2 history rewrite still needs approval if force-push is desired.

---

## Prioritized Action Plan (completed maintenance)

### Priority 1 — Correctness / Silent Bugs

| # | Item | Status | File / Location |
|---|---|---|---|
| 1.1 | `ntp_offset_seconds` metric always reports 0 | **done** | `src/metrics.rs`, `src/main.rs:sync_loop` |
| 1.2 | `cache_hits` in `/performance` always reports 0 | **done** | `src/http/handlers.rs:time_handler` |
| 1.3 | `TimeCache::update` swap logic bug — `is_stale=true` returned MSG_OK instead of MSG_OK_CACHE | **done** | `src/performance.rs` |

---

### Priority 2 — Test Coverage

| # | Item | Status | File / Location |
|---|---|---|---|
| 2.1 | All integration tests are stubs (`assert!(true)`) | **done** | `tests/integration_api.rs`, `src/http/mod.rs` |
| 2.2 | No end-to-end test for `/time` with real NTP sync flow | **done** | `src/http/mod.rs::test_time_after_sync_returns_correct_epoch` |
| 2.3 | No test for WebSocket stream behavior | **done** | extracted `compute_max_updates` + 4 unit tests in `src/http/websocket.rs` |
| 2.4 | No test for `REQUIRE_SYNC=false` fallback path | **done** | `src/http/handlers.rs::test_time_require_sync_false_*` |
| 2.5 | No test for sticky server selection switch logic | **done** | extracted `sticky_select` pure function + 6 unit tests in `src/ntp/sync.rs` |

#### Details

**2.5** — The sticky switching logic (`keep current unless 50ms+ RTT improvement`) was embedded in the async `NtpSyncer::sync()` method, making it untestable without network access. Fixed by extracting into a pure `sticky_select(results, best, current_server, threshold_ms)` function that returns `(NtpResult, Option<String>)`. Six unit tests cover all decision paths: no current server, current failed, current is still best, new server not significantly better, new server significantly better, and exactly-at-threshold.

---

### Priority 3 — Dead Code / Unused Features

| # | Item | Status | File / Location |
|---|---|---|---|
| 3.1 | `AppError::NtpError` and `AppError::Timeout` variants unused | **done** | Removed unused variants from `src/errors.rs`; only `NotSynced` and `Internal` remain |
| 3.2 | `NtpResult` T1–T4 fields collected but not exposed | **done** | `NtpTimingSummary` in `AppState`; stored by sync loop; exposed in `/performance` JSON |
| 3.3 | `ServerStats::is_available()` unused in production | **done** | moved to `#[cfg(test)]` impl in `src/ntp/stats.rs` |
| 3.4 | `TimeCache::get_epoch()` and `is_initialized()` unused in production | **done** | moved to `#[cfg(test)]` impl in `src/performance.rs` |
| 3.5 | `LockFreeMetrics` methods unused: `requests_per_second`, `avg_latency_us`, `error_rate`, `cache_hit_rate` | **done** | `requests_per_second` removed; others moved to `#[cfg(test)]` impl |

#### Details

**3.1** — Removed `AppError::NtpError` and `AppError::Timeout` variants and the `#[allow(dead_code)]` attribute. `AppError` now has exactly two variants: `NotSynced` (→ 503) and `Internal` (→ 500).

**3.2** — `NtpTimingSummary` struct added to `http/state.rs` (fields: `server`, `t1_client_send_ms`, `t2_server_recv_ms`, `t3_server_send_ms`, `t4_client_recv_ms`, `offset_ms`, `rtt_ms`). `AppState` holds `last_ntp_timing: Arc<parking_lot::RwLock<Option<NtpTimingSummary>>>`. `sync_loop` writes the timing on every successful sync. `/performance` handler includes `"ntp_timing"` in the response (null before first sync, populated after). Two unit tests added: one verifying null before sync, one verifying all T1-T4 fields after sync injection.

---

### Priority 4 — Naming and Documentation Inconsistencies

| # | Item | Status | File / Location |
|---|---|---|---|
| 4.1 | `SelectionStrategy::RttMin` name is misleading — actual algorithm is accuracy-first | **done** | renamed to `AccuracyFirst`; `"rtt_min"` env-var string kept for backwards compat |
| 4.2 | Prometheus metric field `ntp_server_rtt_ms` vs registered name `ntp_server_rtt_milliseconds` | **done** | renamed to `ntp_server_rtt_milliseconds` in `src/metrics.rs` |
| 4.3 | `NtpServer` UDP server vs NTP client "servers" naming collision in metrics | **done** | UDP server metrics renamed to `ntp_udp_server_*` in `src/metrics.rs` and `src/ntp/server.rs` |

#### Details

**4.3** — Renamed the four local UDP NTP server metrics from `ntp_server_*` to `ntp_udp_server_*`:
- `ntp_udp_server_requests_total` — UDP requests received
- `ntp_udp_server_responses_total` — UDP responses sent
- `ntp_udp_server_errors_total` — UDP errors (malformed packets, send failures)
- `ntp_udp_server_unsynced_responses_total` — responses sent while unsynced

The `ntp_server_up` and `ntp_server_rtt_milliseconds` metrics retain the `ntp_server_` prefix as they refer to upstream NTP client servers (labelled by server address).

---

### Priority 0 — Developer Experience

| # | Item | Status | File / Location |
|---|---|---|---|
| 0.1 | `GovernorLayer` blocks all requests in local sandbox (no peer IP available) | **done** | Added `DISABLE_RATE_LIMITING=true` env var; `src/config.rs:HttpConfig`, `src/http/mod.rs` |

**0.1** — Added `disable_rate_limiting: bool` to `HttpConfig`, read from `DISABLE_RATE_LIMITING` env var (default `false`). When `true`, `create_router()` calls `create_router_internal(state, false)`, skipping the `GovernorLayer`. This unblocks local smoke-testing:
```
DISABLE_RATE_LIMITING=true LOG_FORMAT=pretty cargo run
```

---

### Priority 5 — Infrastructure and Deployment

| # | Item | Status | File / Location |
|---|---|---|---|
| 5.1 | Docker image not pushed to any registry in CI | **blocked** | Requires registry credentials/target; operator decision needed |
| 5.2 | `docker-compose.yml` healthcheck is `NONE` (distroless has no shell) | **known-limitation** | `docker-compose.yml:67-70` |
| 5.3 | UDP NTP server reports `root_delay=0` and `root_dispersion=0` | **done** | `last_rtt_ms: Arc<AtomicU64>` in `AppState`; propagated to `NtpServer`; used in `build_response` |
| 5.4 | Kubernetes `configmap.yaml` has only 3 servers vs 24 in docker-compose | **done** | expanded to 12 servers in `k8s/configmap.yaml` |

#### Details

**5.1** — Docker push is commented out in CI. Un-comment and configure `DOCKER_USERNAME`/`DOCKER_PASSWORD` secrets when a registry target is chosen. The Trivy scan already gates the push correctly.

**5.3** — `AppState` now holds `last_rtt_ms: Arc<AtomicU64>` (milliseconds). The sync loop stores the measured RTT on every successful sync. `NtpServer` receives this `Arc` and passes the value to `build_response`, which converts ms → NTP short format (`ms * 65536 / 1000`) and sets `root_delay`. `root_dispersion` remains 0 (upstream dispersion is not tracked).

---

### Priority 6 — Performance and Scalability

| # | Item | Status | File / Location |
|---|---|---|---|
| 6.1 | Sync loop queries ALL servers every cycle (can be N×NTP_TIMEOUT=2s parallel wall time) | **known** | `src/ntp/sync.rs:54-102` |
| 6.2 | `hdrhistogram` crate is in `Cargo.toml` but not used anywhere | **done** | removed from `Cargo.toml` |
| 6.3 | `crossbeam` crate is in `Cargo.toml` but not used anywhere | **done** | removed from `Cargo.toml` |

#### Details

**6.1** — Every sync cycle queries ALL configured servers in parallel. This is intentional for accuracy. Wall time is bounded by the slowest server (not N × timeout). The risk is if ALL servers hit the timeout — then the sync fails entirely.

---

### Priority 7 — Security

| # | Item | Status | File / Location |
|---|---|---|---|
| 7.1 | UDP NTP server is a potential amplification vector (41:1 ratio) | **known** | `src/ntp/server.rs` |
| 7.2 | Rate limiting only on HTTP path; no rate limiting on UDP NTP server | **done** | Fixed-window per-IP rate limiter (`UdpRateLimiter`) in `src/ntp/server.rs`; default 100 req/s |
| 7.3 | `NTP_SERVER_ENABLED=false` by default — UDP disabled unless explicitly enabled | **mitigated** | `src/config.rs:155` |

#### Details

**7.1** — NTP amplification attacks are a real DDoS vector. When `NTP_SERVER_ENABLED=true`, spoofed source IPs can cause the server to send UDP responses to victims. Mitigations: (a) deploy behind a firewall; (b) monlist (MODE_7) is not implemented; (c) response size is bounded to 48 bytes; (d) per-IP rate limiting now enforced.

**7.2** — Added `UdpRateLimiter` struct in `src/ntp/server.rs`. Fixed-window (1-second) per-source-IP counter; default limit is `DEFAULT_UDP_RATE_LIMIT = 100` requests/second. Exceeded requests are silently dropped (no amplification reply). The map is lazily compacted when it exceeds 10,000 entries. `limit_per_second = 0` disables rate limiting. Unit-tested with 4 tests covering normal allow, blocking, independence across IPs, and the zero-means-unlimited case.

---

## Items Found as `#[allow(dead_code)]`

Remaining intentional suppressions (compiler would warn without the attribute):

| Location | Item | Status |
|---|---|---|
| `src/ntp/mod.rs:9` | `#[allow(unused_imports)]` on re-exports | acceptable — public API surface |
| `src/ntp/protocol.rs:42` | `#![allow(dead_code)]` on entire module | acceptable — full public protocol API |

Resolved (no longer `#[allow(dead_code)]` in production):
- `ServerStats::is_available()` — moved to `#[cfg(test)]` impl
- `TimeCache::get_epoch()`, `is_initialized()` — moved to `#[cfg(test)]` impl
- `LockFreeMetrics` helper methods — moved to `#[cfg(test)]` impl
- `ntp_offset_seconds` — now updated on every sync
- `ntp_server_rtt_ms` field — renamed and no longer mismatched
- `AppError::NtpError`, `AppError::Timeout` — removed entirely (3.1)
- `NtpResult::t1..t4` — now consumed by sync loop and exposed via `/performance` (3.2)

---

---

## Note: File Tracking

`PROJECT_PLAN.md` and `PROJECT_ARCHITECTURE.md` are currently matched by the `*.md` glob in `.gitignore` (line 38) and are **not tracked by git**. To commit them:

```bash
# Option A: add explicit overrides to .gitignore
echo '!PROJECT_PLAN.md' >> .gitignore
echo '!PROJECT_ARCHITECTURE.md' >> .gitignore
git add PROJECT_PLAN.md PROJECT_ARCHITECTURE.md

# Option B: force-add (one-time, does not persist for future commits)
git add -f PROJECT_PLAN.md PROJECT_ARCHITECTURE.md
```

---

## Status Legend

- `todo` — Not started, work needed; implementation plan documented above
- `in-progress` — Actively being worked on
- `blocked` — Waiting on external dependency or decision (only 5.1 remains)
- `done` — Completed
- `known-limitation` — Known issue, accepted as-is with rationale
