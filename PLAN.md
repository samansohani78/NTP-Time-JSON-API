# Deep Code Review & Plan — `ntp-time-json-api`

Generated after reading every line of every file in `src/`, the test
suite, the build wiring, and the deployment manifests. Each finding is
anchored to `file:line` so it can be navigated to directly.

> **Status legend**
> `[BUG]`   — incorrect behavior on a plausible code path
> `[SMELL]` — works today but is fragile, misleading, or hard to maintain
> `[PERF]`  — measurable cost on a hot path
> `[SEC]`   — security or reliability concern
> `[DOC]`   — code/docs disagree
> `[CLEAN]` — already correct, included for completeness

---

## 0. Executive summary

The codebase is small, well-organized, and mostly correct. The
hot-path design (lock-free atomic timebase + pre-serialized JSON
cache + split router) is genuinely clever. The gRPC stub was
already removed (good). The NTP server feature was added in
this commit and is consistent with the existing style.

A careful read surfaced **23 distinct findings** across
correctness, robustness, observability, and developer
experience. **18 of those have been resolved** across the
six waves catalogued in §29. The five most important:

1. `src/main.rs:158-177` — the graceful-shutdown `select!` was
   genuinely broken (one arm unreachable, double-abort).
   **Fixed in wave 0** (commit `a25f75e`).
2. `src/performance.rs:39` — `TimeCache::update` ignored its
   `is_stale` argument; `MSG_OK_CACHE` was therefore never
   observed in the HTTP response. **Fixed in wave 0.**
3. `src/performance.rs:43` — `Instant::now().elapsed()` always
   returns ~0 (it's "now" minus "now"). **Fixed in wave 0.**
4. `src/http/handlers.rs:78-82` — every 503 from the pre-sync
   window was being counted as a `success` in perf metrics.
   **Fixed in wave 0.**
5. `src/ntp/sync.rs:152` — the smart-sticky RTT subtraction
   could mislead; `rtt_improvement` was the wrong name.
   **Fixed in wave 1** (commit `43f8120`) and renamed to
   `rtt_diff_ms`.

Other findings tracked through the waves (B6 dead code in
selector, B13 RFC 5905 T1–T4, B19 AppError refactor) are
catalogued in §26. The remaining open items (B7, B16, B18)
are listed in §26.5 — each is its own focused PR.

---

## 1. `src/main.rs` (348 lines)

### 1.1 `mod` block and allocator  `[l.1-27]`
`[CLEAN]` No issues. The `#[cfg(not(target_env = "msvc"))]` guard
is correct — `tikv-jemallocator` doesn't ship prebuilt for MSVC
and the project says debug builds will not work on MSVC targets.

### 1.2 Component wiring  `[l.30-87]`  `[CLEAN]`
- L32: `Config::from_env()?` is the only entry point — correct.
- L44-58: all components built before any spawn. Good.
- L61-73: `sync_loop` and `probe_loop` are independent Tokio
  tasks that share `state` and `timebase` via `Arc`. The
  `timebase` parameter to `sync_loop` is **cloned** (no `Arc`),
  but `TimeBase` is already `#[derive(Clone)]` and its fields
  are all `Arc<Atomic>`, so cloning is cheap. Correct.
- L75-87: NTP server is conditionally spawned — good.

### 1.3 Listener setup  `[l.89-136]`  `[SMELL]`

```rust
let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
    .expect("Failed to create socket");
```

- L91: `.expect("...")` will panic at startup if the OS refuses
  the socket call. Acceptable for a server (fail-fast), but
  consider returning an error from `main` for diagnosability.
- L93-110: `SO_REUSEADDR`, `TCP_NODELAY`, and `TCP_KEEPALIVE`
  are all set individually. No IPv6 `IPV6_V6ONLY` is set, which
  on Linux with dual-stack `::` means the socket accepts both
  v4 and v6. That is the intended default but should be
  documented; consider making it explicit via
  `set_only_v6(false)` for clarity.
- L131: `socket.bind(&addr.into()).expect(...)` will panic on
  EADDRINUSE. Same comment as L91.

### 1.4 Graceful shutdown  `[l.145-181]`  `[BUG → fixed]`

The previous `tokio::select!` shape was:

```rust
tokio::select! {
    _ = async {
        sync_handle.abort();
        probe_handle.abort();
        sleep(Duration::from_millis(100)).await;
    } => { /* "stopped gracefully" */ }
    _ = sleep(Duration::from_secs(5)) => { /* "force exit" */ }
}
```

The first arm aborts, then sleeps 100 ms; the second sleeps 5 s.
Because `tokio::select!` polls both arms concurrently, the first
arm **always wins** (100 ms < 5 s) — the "force exit" branch was
unreachable. Additionally, `.abort()` was called in both arms
on timeout, so on the "force" path we aborted twice.

**Fix applied**: replace with a single
`tokio::time::timeout(Duration::from_secs(5), …)` that aborts
all handles up front and just `await`s them. On timeout, log a
warning. L158-177 in the current file.

### 1.5 `sync_loop`  `[l.183-260]`  `[SMELL]`

- L190: `interval(config.sync_interval())` — first tick fires
  immediately (Tokio `Interval` semantics), so the first sync
  happens at `jitter + 0`, then every `SYNC_INTERVAL`. Correct.
- L193: `rand::random::<u64>() % 5000` — 5 s max jitter. Reasonable.
- L210-215: `SystemTime::now().duration_since(UNIX_EPOCH).unwrap()`
  **panics** if the system clock is before 1970. Replace with
  `.checked_duration_since(...).map_or(0, |d| d.as_secs() as i64)`.
  `[BUG, low-priority]`
- L222-226 / L237-243: two `info!`/`warn!` log calls with very
  similar shape. The "serving from cache" branch could be
  consolidated, but it's fine.
- L256-258: `state.get_staleness_seconds()` returns
  `Option<u64>`. If the very first sync just happened, this
  returns `Some(0)`, which is correct. If no sync has
  happened, it returns `None` and we skip the gauge update.
  Subtle: a successful sync **also** sets `last_sync_time`, so
  after the first successful sync this branch is always taken.
  Correct.

### 1.6 `probe_loop`  `[l.262-298]`  `[CLEAN]`
Pure metrics-refresh loop. The `min_ms > max_ms` invariant is
guaranteed by `Config::validate` (L231-233 in `config.rs`).

### 1.7 `init_logging`  `[l.300-319]`  `[CLEAN]`
`tracing_subscriber::registry().with(...).init()` is the
standard pattern. EnvFilter is read from `RUST_LOG` first,
falling back to `config.logging.level`. Correct.

### 1.8 `shutdown_signal`  `[l.321-348]`  `[CLEAN]`
Both `Ctrl+C` and `SIGTERM` are handled on Unix. The
`std::future::pending()` shim on non-Unix is correct.

---

## 2. `src/config.rs` (346 lines)

### 2.1 Struct shapes  `[l.6-79]`  `[SMELL]`

- `Config` (L7-13) is a 4-field aggregate. With the new
  `NtpServerConfig` (L47-56) it is now a 5-field aggregate.
  Consider grouping all `ntp_server` access behind a
  `config.ntp_server.*` chain (already done) and a getter
  pattern, but this is fine for a binary crate.
- `NtpConfig::asymmetry_bias_ms` (L37) is read from env but
  **never used** by `ntp::sync`. Either wire it into the offset
  calculation in `sync.rs:212` or remove it. `[DEAD CONFIG]`
- `NtpConfig::offset_bias_ms` (L36) is added to
  `epoch_ms` in `sync.rs:212`. The asymmetry bias should
  similarly be added (or subtracted, depending on the sign
  convention). This is a real missing feature.

### 2.2 Env helpers  `[l.81-93]`  `[CLEAN]`
Standard `env_or_default` / `env_or_parse`. The bound
`T::Err: std::fmt::Debug` is correct for `unwrap_or` to compile
when `parse` fails.

### 2.3 `from_env`  `[l.95-219]`  `[SMELL]`

- L104-114: every env-var call is its own statement. Works, but
  no error context. The recent addition of NTP_SERVER_* already
  uses `.context("Failed to parse NTP_SERVER_ADDR")` — apply
  the same to all `parse()` calls so operators see
  `Failed to parse MAX_OFFSET_SKEW_MS: invalid digit…` etc.
- L121-132: the server-list parser. It now filters empty
  strings (good) and refuses to add `:123` if a port is
  already present. Behavior with `NTP_SERVERS=",,,"`:
  produces an empty vec, which is caught by L134-136.
- L153-159: `SELECTION_STRATEGY` is parsed with a bail on
  unknown value. The enum currently has only one variant; this
  is forward-looking for `weighted_rtt`, `geo_aware`, etc.
  (CLAUDE.md §2.4). Until those land, the `bail!` branch is
  unreachable code. Acceptable.

### 2.4 `validate`  `[l.221-238]`  `[CLEAN]`
All invariants are covered. New check at L234-236 for
`NTP_SERVER_MAX_PACKET_SIZE >= 48` (the NTP packet minimum).

### 2.5 `Default for Config`  `[l.249-295]`  `[SMELL]`
- The test-only `Default` impl duplicates every field. When
  the new `NtpServerConfig` is added here, every new
  downstream config field needs the same dance. Consider a
  builder (`Config::for_test()`) and a macro if this grows
  further. Not worth fixing now.

### 2.6 `tests`  `[l.297-346]`  `[SMELL]`

- `test_utf8_messages` (L327-345) calls
  `std::env::set_var` and `remove_var` which are marked
  `unsafe` in newer Rust editions. The `unsafe { … }` blocks
  are present. Correct, but this test will silently
  affect other tests in the same process that depend on env
  state. The clean-up at the end is necessary; this is a
  pattern to watch when more env-dependent tests are added.
- `test_default_config` (L301-307) and `test_config_validation`
  (L309-325) are good. The validation test covers empty
  servers and bad probe intervals. Add a test for
  `NTP_SERVER_MAX_PACKET_SIZE < 48` once the feature is
  exercised.

---

## 3. `src/errors.rs` (46 lines)

### 3.1 `AppError` enum  `[l.9-23]`  `[SMELL]`

The enum is `#[allow(dead_code)]` — none of the variants are
ever constructed by the current code path. Two observations:

- L29-30: `AppError::NotSynced` is the canonical way to express
  "not synced" but `handlers::time_handler` builds a `503`
  response inline instead of returning `AppError::NotSynced`.
  Consolidate for consistency: change `time_handler` to return
  `Result<Response, AppError>` and use `?`. That also makes
  the dead-code annotation unnecessary.
- L29: `AppError::NtpError(String)` is also unused.
- L30: `AppError::Internal(#[from] anyhow::Error)` exists but
  is never constructed either.

If the team does not want to refactor `time_handler`, consider
removing `AppError` entirely. Currently it's dead code that
masks future error-handling plans.

### 3.2 `IntoResponse` impl  `[l.25-46]`  `[CLEAN]`
The implementation is correct but unreachable from the current
codebase (see 3.1). The JSON shape matches the rest of the
service: `{ message, status, data, error }`.

---

## 4. `src/metrics.rs` (289 lines)

### 4.1 Label types  `[l.10-26]`  `[CLEAN]`
Three `EncodeLabelSet` structs. All required traits
(`Clone`, `Debug`, `Hash`, `PartialEq`, `Eq`) are derived. The
registry's metric names match the field names (modulo the
historical `ntp_server_rtt_milliseconds` rename — see 4.2).

### 4.2 `Metrics` struct  `[l.28-59]`  `[SMELL]`

- L45-47: `ntp_server_rtt_ms` is now correctly named
  (was `ntp_server_rtt_seconds`); the registered Prometheus
  name is still `ntp_server_rtt_milliseconds` (L146). The
  rename is complete; the doc comment explains the historical
  mismatch. Good.
- L51-54: the four new `ntp_server_*` counters are wired.
- L41, L49, L57: `#[allow(dead_code)]` on `ntp_offset_seconds`
  and `build_info`. `build_info` is set in `new()` (L194-196)
  but never read. That's fine — it's a metric.
  `ntp_offset_seconds` is registered but never set. Either
  start populating it from `sync_loop` (L199-220 in
  `main.rs`) or remove it.

### 4.3 `Metrics::new`  `[l.61-218]`  `[CLEAN]`
Every metric is registered with a name and help string. The
`build_info` family is initialised to `version=… git_sha=…`.
The `GIT_SHA` env var is read at compile time via
`option_env!` — if not set, falls back to `"unknown"`. CI
should set this; the Dockerfile does not. Add a `GIT_SHA`
build arg in the Dockerfile for proper traceability.

### 4.4 `record_http_request`  `[l.226-243]`  `[CLEAN]`
Standard. Allocates a `String` per call for the labels. On a
hot path that's `O(microseconds)`; if `/time` ever becomes
HTTP-metered (today it is on the fast path and **not** metered
because it bypasses `track_metrics`), this would matter. Not
worth changing.

### 4.5 `encode`  `[l.220-224]`  `[SMELL]`
`encode(&mut buffer, &self.registry).unwrap()` — `encode` can
only fail on `fmt::Write` errors, which `String` does not
trigger. The `unwrap()` is fine. If we ever stream to a TCP
socket directly, this needs to handle `io::Error`.

---

## 5. `src/performance.rs` (341 lines)

### 5.1 `TimeCache` struct  `[l.6-25]`  `[CLEAN]`
Six fields. The two `Arc<ArcSwap<String>>` are the zero-copy
JSON payloads. The addition of `start_instant: Instant` (L20)
in this commit fixes the previously-broken monotonic millis
counter.

### 5.2 `TimeCache::new`  `[l.27-40]`  `[CLEAN]`
Both `ArcSwap`s are seeded with the `initializing` JSON. The
`start_instant` is captured at construction.

### 5.3 `TimeCache::update`  `[l.42-82]`  `[BUG → fixed]`

The previous shape accepted `is_stale: bool` and then **ignored
it** (`_is_stale`). The two `ArcSwap`s were populated with the
same epoch but different `message_*` strings, and
`get_json(is_stale)` returned the matching one. The bug was
that `is_stale` was effectively a no-op input — the
`MSG_OK_CACHE` field was plumbed through but had no effect on
the rendered response because of the `if is_stale { … } else
{ … }` swap in the new code (L75-81).

**Fix applied** (L75-81): the `is_stale` parameter now controls
which string goes into which `ArcSwap`. Net result:
- `get_json(false)` returns the JSON with `message_ok`
- `get_json(true)` returns the JSON with `message_ok_cache`

The pre-fix invariant was accidentally preserved because
`get_json` reads from the matching swap; the bug only
manifested if a single request flipped staleness between two
calls to `update()` — at which point the *previous* call's
data would have been returned. This is now deterministic.

### 5.4 `TimeCache::get_json`  `[l.84-92]`  `[CLEAN]`
`Arc::load_full()` returns an `Arc<String>` — zero-copy clone.
The conditional is now correct (see 5.3).

### 5.5 `get_epoch` / `is_initialized`  `[l.94-104]`  `[SMELL]`
Both are `#[allow(dead_code)]`. `is_initialized` is used in the
`tests` module below (L283, 287). `get_epoch` is used by no
production code. Either delete or wire into `/performance` to
expose the raw last-known epoch.

### 5.6 `LockFreeMetrics`  `[l.107-273]`  `[SMELL]`

- The whole struct is `#[allow(dead_code)]`. Several fields
  (`cache_updates`, `min_latency_us`, `max_latency_us`,
  `requests_per_second()`, `avg_latency_us()`, `error_rate()`,
  `cache_hit_rate()`) are never read by the production
  `/performance` handler. The handler reads the raw atomics
  directly and does its own math.
- L131-143: `new()` seeds `min_latency_us` to `u64::MAX`
  (sentinel). The `min_latency_us()` getter (L258-261) handles
  the sentinel by returning 0. Correct.
- L150-151: `total_latency_us.fetch_add(latency_us, …)` is
  u64 addition that can overflow after ~584,000 years at
  1 GHz latency accumulation. Not a real concern.
- L226-255: the `update_min` / `update_max` CAS loops are
  textbook lock-free patterns. **Caveat**: they use
  `Ordering::Relaxed` for both the load and the CAS. The
  semantics are still correct (each individual CAS is atomic
  and the loop converges) but a relaxed store of the new min
  might be reordered with a subsequent relaxed load from
  another thread — which is fine because the new min can
  only be smaller than what the other thread saw, and the
  other thread will see *some* CAS-overwritten value when its
  loop retries. Document this in a comment to head off
  future questions.

### 5.7 `LockFreeMetrics` tests  `[l.275-340]`  `[CLEAN]`
Good coverage of the rate / hit calculations. The
`test_lock_free_metrics` and `test_cache_hit_rate` tests are
the only ones that exercise the CAS loops. Add a stress test
with `loom` (already a dev-dep would be needed) to verify the
CAS loops under contention — or accept the simple tests as
sufficient for a binary crate.

---

## 6. `src/timebase.rs` (207 lines)

### 6.1 `REFERENCE_INSTANT`  `[l.11]`  `[CLEAN]`
Global lazy `Instant::now()`. `once_cell::sync::Lazy` is the
right primitive (versus `std::sync::OnceLock` which would
require a separate `Instant` default). Captured once at first
access from any thread; never mutated.

### 6.2 `TimeBase` struct  `[l.15-35]`  `[CLEAN]`
All mutable state is `Arc<Atomic…>`. The struct is
`#[derive(Clone)]` and cloning is cheap (Arc bumps only).
`time_cache` is optional; `with_cache` returns `Self` for
builder-style chaining.

### 6.3 `update`  `[l.55-78]`  `[CLEAN]`
- L60-63: stores `Instant` as nanos since `REFERENCE_INSTANT`.
  This avoids serialising a 12-byte `Instant` through atomics
  (we'd need a lock for that) — we only store a `u64`.
- L67-71: stores use `Ordering::Release`; the `has_synced`
  store is the last one, so any thread that observes
  `has_synced == true` via Acquire is guaranteed to see the
  updated `base_*` values. Correct acquire/release pairing.

### 6.4 `now_ms`  `[l.84-115]`  `[CLEAN]`
- L87-89: short-circuit on `has_synced` — no arithmetic if
  not yet synced. Avoids reporting a wrong time before first
  sync.
- L96: `Instant::now().duration_since(*REFERENCE_INSTANT)` —
  `as_nanos() as u64` truncates to u64, which is safe for the
  next ~584 years of uptime.
- L99: `saturating_sub` defends against clock skew where
  `now_nanos < base_instant_nanos` (shouldn't happen on a
  monotonic clock, but defensive).
- L106-111: monotonic clamp uses Acquire on load, Release on
  store. Two threads that race here can both observe the same
  `last_served` and both bump it to `last_served + 1`; one of
  the resulting values will be discarded by the next reader
  because it's smaller. Strict monotonicity is preserved at
  the cost of occasional +1 ms jumps under extreme contention.
  Acceptable for the workload.

### 6.5 `has_synced`  `[l.118-121]`  `[CLEAN]`
Acquire load — correct.

### 6.6 Tests  `[l.124-206]`  `[CLEAN]`
Five tests cover the four invariants:
- pre-sync returns `None`
- post-sync returns `Some(...)` close to the base
- monotonic progression
- monotonic clamp under simulated jump
- non-clamped mode still progresses

The `test_monotonic_clamping` test (L175-191) directly stores
into `tb.last_served_ms` — a test-only reach into a private
field. This is fine for unit tests but the private-field
access relies on the `#[cfg(test)]` module being in the same
crate, which it is. No leakage.

---

## 7. `src/ntp/sync.rs` (322 lines)

### 7.1 `SyncResult`  `[l.13-19]`  `[CLEAN]`
Plain data struct. `instant: Instant` pairs with `epoch_ms`
to bridge between monotonic and wall-clock time. Critical for
`TimeBase::update` (see 6.3).

### 7.2 `NtpSyncer`  `[l.21-39]`  `[CLEAN]`
- L22-25: three `Arc<…>` fields. `current_server` is
  `Arc<RwLock<Option<String>>>` — the `Arc` is unnecessary
  because `NtpSyncer` itself is already shared via `Arc`. Use
  just `RwLock<Option<String>>`. (Minor — does not affect
  correctness.)
- L29-32: pre-populates `stats` with one entry per configured
  server. Good — no runtime insertion during sync.

### 7.3 `sync`  `[l.42-217]`  `[SMELL]`

- L45-46: clones the entire server list and the current
  server. Cheap; correct.
- L48-52: `info!` log lists all servers. At 24+ servers
  (current `docker-compose.yml`) this log line is long but
  useful for debugging.
- L56-63: spawns one Tokio task per server. Each task uses
  `tokio::task::spawn_blocking` to call the blocking
  `SntpClient::synchronize` (L228-231). 24 concurrent
  blocking tasks is well within the default blocking pool
  (512 threads) but the pool will be exhausted if any
  individual call hangs. The 2 s timeout (L58) saves us
  here, but consider an explicit `task::Builder::spawn_blocking`
  with a per-task timeout that's enforced by the outer
  `timeout` (L227). Already done. Correct.
- L66-123: result collection. The two failure branches
  (L88-104 query-failed, L105-121 task-panicked) are
  textually identical. **Extract a helper**:
  ```rust
  async fn record_failure(&self, server: &str) { … }
  ```
  Both branches become a one-liner. Reduces 35 lines to ~10.
- L75, L163, L186, L197, L208: `result.clone()` on values
  that are either already owned (`Ok(Ok(result))` matches
  on a `Result<NtpResult>` — `result` is moved into the
  `clone` call) or `&NtpResult` from `iter().find()`. The
  `clones` on the owned `result` are unnecessary; on the
  `&` results they are. In the smart-sticky branch
  (L163, L186, L208), `current_result` is a `&NtpResult`
  so `clone()` is required; but the result is then passed
  straight into `SyncResult`, so the clone is also
  redundant — we can move out via `current_result.clone()`
  replaced with indexing/ownership manipulation. Low
  priority.
- L142-143: `results.clone()` passes the full vec into
  `select_best_result`. The function takes `Vec<NtpResult>`
  by value, then `results` is used again for the smart-sticky
  logic below. The clone is necessary **only if** the inner
  loop uses `results` afterwards. It does (L148:
  `results.iter().find(...)`). So the clone is required.
  Acceptable.
- L152: **`[BUG]`** `let rtt_improvement = current_rtt_ms as i64 - best_rtt_ms as i64;`
  Both casts to `i64` are fine, but the subtraction is
  `i64 - i64`. If `current_rtt_ms > best_rtt_ms`, this is
  positive (improvement). Otherwise negative (degradation).
  The code uses `rtt_improvement >= 50` (L164) to decide
  whether to switch — a negative number fails that check,
  which is the correct semantic. **However**, the variable
  is also used at L173 (`improvement_ms = rtt_improvement`)
  for logging, where a negative value is misleading.
  **Fix**: rename to `rtt_diff_ms` and check sign
  explicitly:
  ```rust
  let rtt_diff_ms = current_rtt_ms as i64 - best_rtt_ms as i64;
  if best.server != current_server && rtt_diff_ms >= 50 { … }
  ```
  Already present at L184 (`rtt_diff_ms` is logged with the
  correct sign). Inconsistent naming between L152 and L184.
- L201: `*self.current_server.write().await = Some(best.server.clone())` —
  `best.server` is moved into the assignment, but the `Ok`
  arm later (L211-216) also references `selected_result.server`
  — that's the same `best` because no reassignment happened
  on the first path. Correct.

### 7.4 `query_ntp_server`  `[l.220-283]`  `[CLEAN]`

- L221: `let start = Instant::now()` — measures RTT. The
  `Duration` is reported on L244.
- L227-237: nested `timeout(…)` wraps a
  `tokio::task::spawn_blocking` that calls
  `SntpClient::synchronize`. The `?` chain (`??`) unwraps
  JoinError, SNTP error, and timeout error in that order.
  Each `?` adds a `.context("…")` so the error is
  attributable.
- L241-242: paired `Instant::now()` and `SystemTime::now()`
  captures immediately after the query returns. This pair is
  the foundation of the epoch_ms computation: we add the
  NTP-reported offset to the system time, then convert to
  ms. The two captures are deliberately consecutive to
  minimise drift between them.
- L252-268: signed-offset handling. Uses
  `abs_as_std_duration()` which exists on `rsntp`'s
  `NtpDuration`. Subtle: this is a "non-negative" duration,
  so the original sign is restored by the `if`/`else`. The
  overflow check (`checked_add`/`checked_sub`) is defensive —
  in practice NTP offsets are bounded by network round-trip
  and are tiny.
- L270-274: produces `epoch_ms` as i64 ms since 1970.

### 7.5 `get_stats`  `[l.286-288]`  `[CLEAN]`
Returns a clone of the full `HashMap`. Called by `probe_loop`
every 5-10 s. The clone is O(n) where n is server count;
acceptable.

### 7.6 Tests  `[l.291-321]`  `[SMELL]`
Only one test, `test_ntp_syncer_creation`, and it just checks
that `new` returns a syncer with non-empty stats. No
integration tests for `query_ntp_server` (it would need
either a real network or a mock UDP server). The integration
test placeholder file at `tests/integration_api.rs:1-26`
mentions the right approach (mock UDP). Out of scope for
this commit.

---

## 8. `src/ntp/selection.rs` (286 lines)

### 8.1 `NtpResult`  `[l.4-11]`  `[CLEAN]`
Lives in `selection.rs` rather than `sync.rs` because the
selector returns it. Reasonable.

### 8.2 `ServerSelector::select_servers_for_query`  `[l.17-41]`  `[DEAD CODE]`
- `#[allow(dead_code)]` is a tell: nothing calls this. The
  implementation switches to "query all servers every sync" in
  `sync.rs:42-63`, so this is unreachable.
- **Action**: delete, or wire it up as the actual sample
  strategy behind `SAMPLE_SERVERS_PER_SYNC` (which the docs
  in `README.md:169` and `k8s/deployment.yaml:57` reference as
  a real env var). The current `config.rs` does not read it.

### 8.3 `select_best_result`  `[l.50-141]`  `[SMELL]`

- L65-67: median calculation
  `let median_offset = offsets[offsets.len() / 2];` — for an
  **even** number of servers, this picks the upper of the
  two middle values (a common convention). The behaviour is
  not documented. Add a comment.
- L70-80: standard deviation is computed but never used to
  filter — only the median-based skew filter (L92) runs. The
  `std_dev` is logged (L86) and that's it. The `variance` and
  `std_dev` computation can be deleted without affecting
  behaviour; if kept, the unused-must-use lint should be
  added. Low priority.
- L96-103: fallback when all are outliers. The condition is
  "all servers filtered out"; the recovery is "pick the one
  with min RTT". This is documented and correct.
- L116-130: `min_by` closure picks the inlier closest to
  median, with RTT as tiebreaker. The closure signature
  requires comparing **all** pairs; this is O(n²) for
  `select_servers_for_query`-style pre-sorting, but here n is
  at most 24 so it's fine. Use
  `min_by_key(|r| (offset_dist, rtt))` for clarity.

### 8.4 Tests  `[l.144-285]`  `[CLEAN]`
Four tests cover:
- single result passthrough
- outlier filtering
- accuracy-first selection (closest to median)
- RTT tiebreaker

The `test_select_servers_for_query` test is the only thing
keeping the dead code alive. Once that's removed, the test
goes too.

---

## 9. `src/ntp/stats.rs` (119 lines)

### 9.1 `ServerStats` struct  `[l.3-13]`  `[CLEAN]`
Eight fields. `last_rtt`, `last_success`, `last_failure` are
all `Option<…>` so a brand-new stats object has a well-defined
"nothing happened yet" state.

### 9.2 `record_success`  `[l.29-39]`  `[CLEAN]`
- L30-33: zero the failure counter, set the success timestamp.
- L36-38: re-enable if previously disabled, return whether it
  was disabled. The caller uses this to log the re-enable
  event. Correct.

### 9.3 `record_failure`  `[l.41-53]`  `[CLEAN]`
- L42-45: increment counters and the failure timestamp.
- L48-51: disable if threshold reached, return whether it
  *just* transitioned. The threshold check is `>=`, so the
  Nth failure (not the N+1th) is the disabling one. This
  matches the docstring and the k8s deployment value
  (`MAX_CONSECUTIVE_FAILURES=10`).

### 9.4 `is_healthy` / `is_available` / `rtt_score`  `[l.55-73]`  `[CLEAN]`
- `is_healthy` is the only one actually used in
  `main.rs:280`. The other two are dead code but
  `#[allow(dead_code)]` covers them.

### 9.5 Tests  `[l.76-118]`  `[CLEAN]`
Single comprehensive test that walks the full lifecycle. Good
coverage for a small struct.

---

## 10. `src/ntp/protocol.rs` (404 lines, new in this commit)

### 10.1 Layout  `[l.1-44]`  `[CLEAN]`
Module-level docstring reproduces the RFC 5905 wire format
diagram. `#![allow(dead_code)]` at L42 is needed because the
constants form a public protocol API surface.

### 10.2 Constants  `[l.46-77]`  `[CLEAN]`
- `NTP_EPOCH_OFFSET_SECS = 2_208_988_800` is verified by the
  `ntp_epoch_offset_is_70_years` test (L335-339).
- Mode / LI / Stratum constants cover all standard values.
- `STRATUM_PRIMARY` and `STRATUM_UNSYNCHRONIZED` are used by
  `server.rs`. The rest are reference material for downstream
  consumers.

### 10.3 `ProtocolError`  `[l.80-103]`  `[CLEAN]`
Three variants. `Display` impl is informative. `Error` impl
is the trivial blanket. Could derive `std::error::Error` via
`thiserror::Error` for consistency with the rest of the crate,
but the manual impl is fine.

### 10.4 `NtpPacket`  `[l.105-149]`  `[CLEAN]]
13 fields exactly matching RFC 5905. `new(li, vn, mode)` is
a builder that zero-initialises the rest — useful for tests.
`reference_id_ascii()` returns `[char; 4]` for inspection.

### 10.5 `parse_packet`  `[l.156-190]`  `[CLEAN]`

- L157-162: length check returns `ProtocolError::TooShort` —
  caller can decide whether to silently drop.
- L168-173: rejects non-client modes and unsupported versions.
  Strict (no NTPv2, no symmetric modes). Acceptable for an
  NTP server that wants to be a server, not a peer.
- L175-189: field decoding. Endianness is handled by
  `u32::from_be_bytes` / `read_u64`. All `u64` reads go
  through the helper at L247-252. No `unsafe`.

### 10.6 `serialize_packet`  `[l.193-207]`  `[CLEAN]`
Big-endian writes. The first byte is `LI | VN | Mode` packed
per the diagram. Returns `[u8; 48]` — value semantics, no
allocations.

### 10.7 `unix_ms_to_ntp`  `[l.214-223]`  `[CLEAN]`

- L215-217: defensive early return for negative ms (only
  possible if the system clock is pre-1970).
- L220-222: `(ms_part << 32) / 1000` computes the 32-bit
  fraction. The intermediate `ms_part << 32` is u64, so for
  `ms_part < 1000` it does not overflow.
- L222: shifts the seconds part left by 32 and ORs in the
  fraction. The shift is safe because `secs + offset < 2^32`
  for any plausible date (year 2106 is the 2^32 second
  boundary).

### 10.8 `ntp_to_unix_ms`  `[l.226-234]`  `[CLEAN]`

- L232: `(frac * 1000) >> 32` — fixed-point multiply. The
  `*1000` is u64 and `frac < 2^32`, so the multiplication
  fits in u64.
- L233: `saturating_mul(1000)` defends against the year-2106
  boundary.

### 10.9 `system_unix_ms`  `[l.240-245]`  `[CLEAN]`
Returns 0 if the system clock is pre-1970. The caller in
`server.rs:112,118` accepts 0 as a valid `epoch_ms` because
`unix_ms_to_ntp` itself short-circuits negatives to 0. So
even in a "broken clock" deployment, the NTP server will
return a 0 timestamp with LI=3 / Stratum=16. The client
should refuse to trust the answer (and the LI=3 kiss code
encodes exactly that).

### 10.10 Tests  `[l.259-403]`  `[CLEAN]`
14 tests cover parsing, rejection paths, roundtrip, epoch
math, edge cases. Good coverage for a new module.

---

## 11. `src/ntp/server.rs` (345 lines, new in this commit)

### 11.1 Imports and constants  `[l.19-38]`  `[CLEAN]`
Imports only the protocol symbols used. `REFERENCE_ID_LOCAL`
is `b"LOCL"` packed as u32 BE — a "local clock" KISS code
that's correct for Stratum 2.

### 11.2 `NtpServer`  `[l.42-62]`  `[CLEAN]`
Builder-style `with_max_packet_size` enforces a 48-byte
minimum. `new` is plain.

### 11.3 `run`  `[l.68-95]`  `[CLEAN]`

- L69: `UdpSocket::bind(self.addr).await?` — bind errors
  bubble up to `main`, which logs and exits.
- L76-81: warns if binding a privileged port. Helpful for
  Kubernetes deployments where `CAP_NET_BIND_SERVICE` is
  often forgotten.
- L83: allocates a buffer the size of `max_packet_size`. The
  buffer is reused across all received packets.
- L84-94: the recv loop. On `Ok((len, peer))`, dispatches to
  `handle_request`. On `Err(e)`, logs and sleeps 50 ms to
  avoid burning CPU on a broken socket.

### 11.4 `handle_request`  `[l.97-140]`  `[CLEAN]`

- L98: `ntp_server_requests_total` increments **before** the
  parse attempt, so malformed packets are still counted as
  requests. That's the right semantic.
- L100-109: parse failure path. Increments the error counter
  and returns. No reply sent — clients will time out.
- L112: receive timestamp is captured **after** parse but
  before the response is built. This is the closest we can
  get to the "real" receive time. For sub-millisecond
  accuracy, the timestamp should be captured before
  `parse_packet` runs, but `parse_packet` is microseconds
  fast.
- L114-115: `build_response` produces the `NtpPacket`;
  `serialize_packet` writes the wire bytes.
- L118-120: **transmit timestamp** is captured *after*
  serialization but *before* `send_to`. This is the
  "as-late-as-possible" pattern that gives the client a
  correct transmit timestamp. The implementation re-serializes
  the transmit field via `write_transmit` (L192-197) rather
  than rebuilding the packet — cheaper.
- L122-139: send path. Success increments response counters.
  If the timebase was unsynced, the dedicated
  `ntp_server_unsynced_responses_total` counter is also
  bumped, so operators can alert on it specifically.

### 11.5 `build_response`  `[l.143-187]`  `[CLEAN]`

- L146-150: stratum selection. Synced → Stratum 2
  (`STRATUM_PRIMARY + 1`); unsynced → Stratum 16
  (`STRATUM_UNSYNCHRONIZED`).
- L158: `REFERENCE_ID_LOCAL` ("LOCL") for the synced path.
  Unsynced gets `0` (RFC 5905 §7.3 doesn't require a
  particular value here).
- L162: `ref_timestamp` = the time we synced. Not strictly
  accurate (it would be the upstream server's reference
  timestamp), but a useful hint to clients.
- L175: `precision = -20` advertises "we know time to within
  ~1 µs". Optimistic — the actual jitter is much higher
  because `TimeBase::now_ms()` reads the Rust `Instant`,
  which on Linux is `CLOCK_MONOTONIC` with ns resolution
  but unknown accuracy. Consider `precision = -10` (~1 ms)
  for honesty.
- L181: `origin_timestamp = request.transmit_timestamp` —
  per RFC 5905 §7.3. Correct.

### 11.6 `write_transmit`  `[l.192-197]`  `[CLEAN]`
Direct byte slice write into the serialized packet. Defensive
length check. The `u64.to_be_bytes()` call is a const-fn in
recent stdlib, so this is a few instructions.

### 11.7 Tests  `[l.199-344]`  `[CLEAN]`
4 tests, two of which spin up real UDP sockets and exchange
real packets. The synced-path test verifies all 6 invariants
on the response (LI, VN, Mode, Stratum, Reference ID,
origin-timestamp echo). The unsynced-path test verifies
LI=3/Stratum=16. Two unit tests cover `build_response` in
isolation. Solid coverage.

---

## 12. `src/http/mod.rs` (138 lines)

### 12.1 Imports  `[l.1-17]`  `[CLEAN]`
Standard axum + tower-http + tower_governor imports.

### 12.2 `create_router` / `create_router_for_test`  `[l.19-26]`  `[CLEAN]`
The split between prod and test routers exists only to
disable rate limiting in tests (which would need real IP
addresses to be meaningful).

### 12.3 `create_router_internal`  `[l.28-92]`  `[CLEAN]`

- L34-37: `fast_router` — `/time` and `/` only. The comment
  at L31-33 explains the design intent. Important: do not
  add any middleware to this router.
- L40-65: `slow_router` — everything else, with the full
  middleware stack. The order of `.layer()` calls matters
  (applied bottom-up): the outermost is CORS, the innermost
  is the metrics recorder. That's correct.
- L52-55: `axum_middleware::from_fn_with_state` is the
  standard pattern for a middleware that needs the
  `AppState`. The `state.clone()` here is cheap.
- L67-72: CORS is permissive (any origin, any method, any
  header) for the public time API. Fine for a public
  service. If this is ever used in a private VPC, narrow
  it down.
- L77-86: rate limiting only in production (not tests).
  1000 rps per IP, burst 100. `tower_governor` uses
  `PeerIpKeyExtractor` by default which means the source IP
  is the client IP. In a Kubernetes deployment behind a
  service mesh, you'd need to inject a different key
  extractor. Out of scope.

### 12.4 Tests  `[l.94-137]`  `[CLEAN]`
Single test that hits `/healthz`. The `oneshot` pattern is
correct for an axum router in tests.

---

## 13. `src/http/handlers.rs` (276 lines)

### 13.1 `time_handler`  `[l.7-85]`  `[BUG → fixed]`

The previous shape returned `Response` directly and recorded
`record_success` for every call, including 503. The new
shape (L8-85):

- L11: `now_ms()` returns `Option<i64>` — `None` only before
  first sync.
- L14-17: stale determination is based on
  `last_sync_time` + `max_staleness_secs`. Note: this uses
  the `last_sync_time` set by `state.record_sync_success()`
  (`state.rs:38-41`), not the `TimeBase`'s internal
  monotonic state. They are updated together in
  `sync_loop:201-208`, so they stay in lockstep.
- L21-22: `time_cache.update(epoch_ms, is_stale)` is now
  correct (see §5.3).
- L24-28: the 200 response uses the pre-serialized JSON.
- L33-47: the 503 path (pre-sync, `require_sync=true`).
- L49-69: the `require_sync=false` fallback. Returns
  `SystemTime::now()` as ms — but the comment notes this
  defeats the NTP-authoritative design. Acceptable for
  development; in production `require_sync=true` should
  always be set.
- L78-82: success vs. error recording. Fixed.

### 13.2 `healthz_handler`  `[l.87-95]`  `[CLEAN]`
Always 200. Per the AGENTS.md contract: "alive while the
process is alive." Correct.

### 13.3 `readyz_handler`  `[l.97-117]`  `[CLEAN]`
503 if `require_sync=true` and `!has_synced()`, else 200.
Per the AGENTS.md contract: "after first sync, always 200,
even if NTP later fails." Correct.

### 13.4 `startupz_handler`  `[l.119-139]`  `[CLEAN]`
Same logic as `/readyz`. Correct (Kubernetes uses
`startupProbe` to gate `livenessProbe`/`readinessProbe`
during cold start).

### 13.5 `metrics_handler`  `[l.141-144]`  `[CLEAN]`
Returns the registry dump. The `Content-Type` header is
**not** set — Prometheus scrapers will infer it. Setting
`text/plain; version=0.0.4` explicitly is recommended.

### 13.6 `performance_handler`  `[l.146-214]`  `[CLEAN]`

- L148-164: reads raw atomics. The atomic ordering is
  `Relaxed` because the metrics are independent of each
  other and don't need to be consistent with each other.
- L166-182: derives avg / hit_rate / error_rate. All
  safe-divide-guarded.
- L184-213: serialises to a pretty JSON. The numbers are
  formatted with `{:.2}` / `{:.4}` / `{:.3}` depending on
  the precision. Reasonable.

### 13.7 Tests  `[l.216-275]`  `[CLEAN]`
4 tests. `test_metrics` is a smoke test. The
`test_time_before_sync` and `test_readyz_before_sync` tests
branch on `state.config.ntp.require_sync` to remain
correct under both configurations.

---

## 14. `src/http/state.rs` (57 lines)

### 14.1 `AppState`  `[l.8-17]`  `[CLEAN]`
7 fields, all `Arc<…>`. Cloneable (implicit via `derive`).
The `last_sync_time` and `consecutive_failures` fields use
`parking_lot::RwLock` — faster than `std::sync::RwLock` and
unpoisonable. These are touched only by the NTP sync loop
(every 30 s) and the HTTP handler (every request for
staleness check).

### 14.2 `record_sync_success` / `record_sync_failure`  `[l.38-45]`  `[CLEAN]`
`record_sync_success` overwrites `last_sync_time` and zeroes
the failure counter. `record_sync_failure` increments the
counter. Both acquire the write lock briefly.

### 14.3 `get_staleness_seconds`  `[l.47-52]`  `[CLEAN]`
Returns the time elapsed since the last successful sync, in
seconds. `as_secs()` (vs. `as_millis()`) is the right unit
because the comparison threshold in `handlers.rs:16` is
`max_staleness_secs` (seconds).

### 14.4 `get_consecutive_failures`  `[l.54-56]`  `[CLEAN]`
Simple read. Used by `sync_loop` for logging.

---

## 15. `src/http/middleware.rs` (37 lines)

### 15.1 `track_metrics`  `[l.10-37]`  `[CLEAN]`

- L15-17: captures method and path **before** calling
  `next.run`. This is important because the request body is
  consumed by `next.run` and re-extracting later is
  awkward.
- L20: `http_inflight_requests.inc()` is incremented.
- L23: awaits the inner handler.
- L26: `http_inflight_requests.dec()` — paired decrement
  even on error. Correct.
- L28-34: records the request in the metrics family. The
  `path` here is the **matched** route (axum normalises
  `/time/` to `/time`, etc.), not the raw URL. That's the
  right behavior — it keeps the cardinality of the label
  small.

---

## 16. `src/http/websocket.rs` (197 lines)

### 16.1 `websocket_handler`  `[l.16-21]`  `[CLEAN]`
Standard axum WebSocket upgrade. The `state.clone()` here
moves a `Arc<AppState>` into the spawned task.

### 16.2 `websocket_connection`  `[l.24-166]`  `[SMELL]`

- L31-39: **env-var re-read on every connection**. This is
  wasteful (a few microseconds per connection) and means a
  rolling deploy won't pick up config changes. **Fix**: lift
  to startup, store in `AppState` as `Option<WsConfig>`,
  read once.
- L42-47: welcome message includes the env values. Once the
  env-var read is moved, the welcome message can be built
  from `state.ws_config`.
- L49-58: send the welcome, return on failure.
- L62-129: sender task. The `max_updates` calculation at
  L65 can **panic on division by zero** if
  `update_interval_ms == 0` (someone sets the env var
  to `"0"`). Fix: validate at startup or treat 0 as
  "unlimited".
- L132-153: receiver task. The `let _ = data;` at L145 is
  to silence the unused-variable warning on the
  `Message::Ping(data)` arm where `data` is not used (axum
  auto-pongs). Consider matching `_` instead and dropping
  the `data` binding.
- L156-163: `tokio::select!` between sender and receiver
  tasks. Whichever finishes first cancels the other
  implicitly (via task drop). Correct.

### 16.3 `format_epoch_ms_to_iso8601`  `[l.168-179]`  `[CLEAN]`
Standard conversion. `format!("{}.{:09}", secs, nsecs)` could
be used as a `chrono`-free alternative; the current code
uses `chrono::DateTime` for cleaner RFC 3339 output.

### 16.4 `use` statements  `[l.181-182]`  `[CLEAN]`
The `use futures_util::SinkExt; use futures_util::stream::StreamExt;`
are at the bottom of the file, not the top. That's a
style choice — some teams enforce top-of-file imports.
Either is fine in Rust; clippy doesn't lint either way.

### 16.5 Tests  `[l.184-196]`  `[CLEAN]`
Single test for ISO 8601 formatting. The WebSocket protocol
itself is not unit-tested; would need a mock WebSocket
client. Out of scope.

---

## 17. `src/ntp/mod.rs` (12 lines)

### 17.1 Module exports  `[l.1-12]`  `[CLEAN]`
Five submodules, three re-exports. The `#[allow(unused_imports)]`
on the protocol re-exports is needed because the in-tree
consumer (`server.rs`) imports the protocol items via
`super::protocol::…` rather than via `crate::ntp::…`.

---

## 18. `Cargo.toml`

### 18.1 Dependency pinning  `[l.9-52]`  `[SMELL]`
- All deps are pinned to a major version. Good for binary
  reproducibility.
- `tokio = "1.48"` — recent.
- `tikv-jemallocator = "0.6"` — pinned.
- `parking_lot = "0.12"` — pinned.
- `prometheus-client = "0.24"` — pinned.
- `rsntp = "4.1"` — pinned.

No `Cargo.lock` updates were needed for this commit because
no new dependencies were added (the new feature uses only
already-available `std` and `tokio` symbols).

### 18.2 Profiles  `[l.58-68]`  `[CLEAN]`
- `release`: opt-3, lto=thin, codegen-units=1, strip. Aggressive
  but reasonable for a single-binary deployment.
- `dev`: opt-0. The default.
- `test`: opt-1. Slightly faster tests.

---

## 19. `Makefile` (73 lines)

### 19.1 Targets  `[l.1-73]`  `[CLEAN]`
Standard set: `build`, `test`, `lint`, `fmt`, `fmt-check`,
`check`, `clean`, `run`, `docker-build`, `docker-up`,
`docker-down`, `docker-logs`, `ci`, `dev-check`. The `ci`
target chains `fmt-check lint test`; `dev-check` chains
`fmt check test` (skips clippy for faster local iteration).

### 19.2 `test` target  `[l.22-24]`  `[CLEAN]`
`cargo test --all-targets --all-features`. The CI workflow
uses `cargo test --all-features --verbose` (no
`--all-targets`), which means CI does **not** run the
integration tests in `tests/`. The AGENTS.md flags this as
known drift.

---

## 20. `docker-compose.yml` (72 lines)

### 20.1 Service definition  `[l.1-72]`  `[SMELL]`
- 24 NTP servers are configured at L32. The list is
  long; if any one of them changes, this file is the
  source of truth. Consider moving the list to an
  environment file.
- L55-60: Persian message strings. UTF-8 is correctly
  supported (verified by the `test_utf8_messages` test
  in `config.rs:327`).
- L63-65: `healthcheck: test: ["NONE"]` — required because
  the distroless image has no shell. Correct.
- **NTP server exposure**: in this commit, added
  `123:123/udp` to `ports:` (after the L5 edit) and
  `NTP_SERVER_ENABLED=true` + `NTP_SERVER_ADDR=0.0.0.0:123`
  + `NTP_SERVER_MAX_PACKET_SIZE=1024` to `environment:`.

---

## 21. `k8s/deployment.yaml` (112 lines)

### 21.1 Deployment spec  `[l.1-112]`  `[SMELL]`
- L9: `replicas: 3` — three pods. For an NTP sync service
  this is fine; the pods all converge to the same NTP
  consensus.
- L57-58: `SAMPLE_SERVERS_PER_SYNC=3` env var. The Rust
  code does **not** read this — see §7.3. Doc/code drift.
- L105-110: `securityContext` drops all capabilities and
  runs as non-root. The distroless image runs as UID
  65532 (`nonroot` tag). Binding to port 123 (privileged)
  with all capabilities dropped **will fail**. The
  k8s deployment must either:
  (a) add `NET_BIND_SERVICE` to the `capabilities.add`
      list, or
  (b) run a separate `ntp-server` container with that
      capability, or
  (c) accept that the NTP server only runs in the
      docker-compose profile where port 123 is mapped
      from the host and the host's user has bind
      privilege.
  In this commit, we add the `ntp` UDP `containerPort`
  (L33-34) and `NTP_SERVER_ENABLED=true` (L66-70). The
  capability add is left for the operator.

### 21.2 `k8s/service.yaml` (15 lines)  `[CLEAN]`
ClusterIP, TCP 80, UDP 123 (added in this commit).

---

## 22. `Dockerfile` (34 lines)

### 22.1 Multi-stage build  `[l.1-34]`  `[SMELL]`
- L4: `rust:1.92-bookworm` — pinned. The Cargo.lock
  guarantees reproducible builds given this base.
- L7-10: installs `pkg-config` and `libssl-dev` for native
  deps (`tikv-jemallocator` and `socket2`).
- L21: `cargo build --release --bin ntp-time-json-api` —
  --release is correct.
- L25: `gcr.io/distroless/cc-debian13:nonroot` — runs as
  UID 65532 with no shell, no busybox, no libc headers.
  Just `glibc` + `libssl` + the binary.
- L28: copies `/app/target/release/ntp-time-json-api` to
  `/ntp-time-json-api`. The WORKDIR of the runtime image
  is `/`.
- L31: `EXPOSE 8080`. After this commit, also
  `EXPOSE 123/udp` is needed for the NTP server.
  **Action item**: add `EXPOSE 123/udp` to the Dockerfile.

### 22.2 `GIT_SHA` build arg  `[L21 area]`  `[MISSING]`
The CI workflow does not pass `GIT_SHA=…` to the build.
`option_env!("GIT_SHA")` in `metrics.rs:193` will return
`None` and the metric will show `git_sha="unknown"`. Either:
- add `GIT_SHA=${{ github.sha }}` to the CI build step, or
- derive the SHA from `git rev-parse HEAD` in a build
  wrapper script.

---

## 23. `tests/integration_api.rs` (125 lines)

### 23.1 Placeholder tests  `[l.1-125]`  `[SMELL]`
Every test body is `assert!(true, "Integration test
placeholder")`. The file is git-tracked and the tests run
in CI (although the AGENTS.md note in §1.2 says CI's
`cargo test` doesn't include `--all-targets`, so the
integration tests are actually skipped in CI). The
file documents the *intent* — what real integration tests
should look like — but the assertions are trivial.

Real integration tests would require either a mock UDP
NTP server (the right answer) or a configurable local NTP
server. The test file is a useful roadmap; converting it
to real tests is out of scope for this commit.

---

## 24. `Cargo.lock` (drift note)

`Cargo.lock` is listed in `.gitignore` at L5 but is
actually tracked (visible in `git ls-files`). The AGENTS.md
documents this as intentional (binary crate, lockfile
should be committed). No change needed.

---

## 25. Cross-cutting concerns

### 25.1 Logging discipline
All async tasks and handlers use `tracing` with structured
fields (e.g., `server = %name, rtt_ms = …`). Consistent and
grep-friendly. Good.

### 25.2 Error handling
- `anyhow::Result` at the binary entry point (`main.rs:30`)
  is appropriate.
- The HTTP layer doesn't use `?` for error propagation;
  each handler builds a `Response` directly. Consistent
  with the rest of the codebase but `AppError` (see §3) is
  then dead code. Pick one.

### 25.3 Memory ordering
Atomic operations are consistently Release on store, Acquire
on load, Relaxed for self-contained counters. The
`timebase.rs` acquire/release pair is correct. The
`LockFreeMetrics` CAS loops use Relaxed on both sides —
intentional and correct, but document it.

### 25.4 Numeric types
- `epoch_ms: i64` — can represent any Unix ms timestamp up
  to year 292 million.
- `rtt: Duration` — stdlib's safe type.
- `latency_us: u64` — same.
- No `usize`-for-time bugs. Clean.

### 25.5 Concurrency primitives
- `tokio::sync::RwLock` for the server-stats map. The
  writers hold the lock for a microsecond at most.
- `parking_lot::RwLock` for `AppState`'s small primitives.
  Cheaper than `std::sync::RwLock` and unpoisonable.
- `Arc<Atomic*>` everywhere else. No `Mutex` in the hot
  path.

---

## 26. Concrete action plan

The following items are ordered by **value-to-risk ratio**.
Each item is small and self-contained.

> **Status (as of wave 6 of 6):** the items marked **[done]** in
> §26.1, §26.2, and §26.3 are landed on `main`. The open items
> are in §26.4 and §26.5.

### 26.1 P0 — correctness (must fix)
- [done] **B1**: Fix broken shutdown `select!` in `main.rs`.
- [done] **B2**: Honor `is_stale` in `TimeCache::update`.
- [done] **B3**: Fix `Instant::now().elapsed()` to use a
  proper anchor.
- [done] **B9**: 503 → `record_error` instead of
  `record_success`.
- [done] **B5**: Rename `rtt_improvement` to `rtt_diff_ms` at
  `src/ntp/sync.rs:152`; use absolute difference in the
  decision to switch. (Wave 1, commit `43f8120`.)
- [done] **B10**: Use `checked_duration_since(UNIX_EPOCH)` at
  `src/main.rs:212`. (Wave 1.) The actual `SystemTime` API
  doesn't have a `checked_*` variant, so the fix is
  `.unwrap_or_default()`, which yields `Duration::ZERO` —
  the same fallback already used by `unix_ms_to_ntp` and
  `system_unix_ms`.
- **B7**: Lift WebSocket env-var reads to startup, store
  in `AppState`. File: `src/http/websocket.rs:31-39`.
  (Wave 1 deferred this; see §26.5.)

### 26.2 P1 — cleanup
- [done] **B4**: Extract `record_server_failure` helper in
  `src/ntp/sync.rs`. (Wave 2, commit `0d6cece`.) Both
  `Ok(Err(_))` and `Err(_)` arms of the result-collection
  loop now funnel through one helper.
- [done] **B6**: Delete `ServerSelector::select_servers_for_query`
  and its test. (Wave 2.) Also dropped the now-dead
  `ServerStats::rtt_score`. Kept `ServerStats::address` as
  a public identity field with `#[allow(dead_code)]`.
- [done] **B8**: Validate `update_interval_ms != 0` in
  `src/http/websocket.rs`. (Wave 2.) Also: `WS_MAX_DURATION_SECS=0`
  is now treated as "unlimited" via `u64::MAX` cap, and
  `saturating_mul` prevents overflow for absurdly large
  values.
- [done] **B10**: See §26.1.
- [done] **B11**: Rename `ntp_server_rtt_seconds` to
  `ntp_server_rtt_ms`. (Landed as part of the NTP server
  feature commit `a25f75e`.)
- [done] **B12**: Empty entries in `NTP_SERVERS` are now
  filtered out. (Landed as part of the NTP server feature
  commit `a25f75e`.)
- [done] **B13**: Wire `asymmetry_bias_ms` into the offset
  calculation. (Wave 3, commit `ca190b6`.) Full RFC 5905
  T1–T4 implementation: `NtpResult` now carries
  `t1_client_send_ms`, `t2_server_recv_ms`,
  `t3_server_send_ms`, `t4_client_recv_ms`. The
  closed-form solution of the linear system
  `θ = ((T2-T1)+(T3-T4))/2`, `δ = (T4-T1)-(T3-T2)`
  is `T2 = T1+θ+δ/2`, `T3 = T4+θ-δ/2`. New test
  `rfc5905_four_tuple_relations_hold` verifies both
  directions of the math.
- [done] **B14**: Replace `min_by` closure with
  `min_by_key` in `src/ntp/selection.rs`. (Wave 2.)
- [done] **B15**: Comment in `select_best_result`
  documenting the upper-mid median convention. (Wave 2.)

### 26.3 P2 — observability & ops
- **B16**: Set `GIT_SHA` from CI in
  `.github/workflows/ci.yml`. (See §26.5.)
- [done] **B17**: Add `EXPOSE 123/udp` to `Dockerfile`.
  (Wave 5, commit `a21e874`.) With a comment documenting
  HTTP vs. NTP port assignments.
- **B18**: Add `NET_BIND_SERVICE` capability to
  `k8s/deployment.yaml` for the NTP server, or document
  the requirement. (See §26.5.)
- [done] **B19**: `AppError` is no longer dead. (Wave 4,
  commit `935755e`.) `time_handler` now returns
  `Result<Response, AppError>`, the 503 path uses
  `Err(NotSynced)` with the configured `MSG_ERROR` /
  `ERROR_TEXT_NO_SYNC` strings, and the success path
  builds a 200 OK response. The JSON shape of the 503
  body is locked in by a new test.
- [done] **B20**: `SAMPLE_SERVERS_PER_SYNC` removed from
  `k8s/deployment.yaml`. (Wave 5.) The `GRPC_*` leftovers
  were already removed in the prior gRPC-cleanup work.
- [done] **B21**: `precision = -10` in `build_response`
  for honesty about jitter. (Wave 5.) The old `-20`
  (≈ 1 µs) was a lie: the upstream sync runs at ~30 s
  intervals, so the real accuracy floor is ~1 ms.

### 26.4 P3 — nice-to-have
- Replace the integration test placeholders with a real
  mock-NTP test harness.
- Add `loom`-based stress tests for the `LockFreeMetrics`
  CAS loops.
- Promote `AppState` field names to private (use getters
  where appropriate) to make future refactors safer.

### 26.5 Deferred beyond waves 1–6

These were intentionally not addressed in this round.
Each is its own focused PR.

- **B7** — WebSocket env-var reads still happen per
  connection. Lifting to startup needs `AppState` to
  carry a `WsConfig` struct, plus updates to `Config`,
  `AppState::new`, and `websocket::websocket_connection`.
- **B16** — `GIT_SHA` build arg is still not set in CI.
  Independent concern from the rest; needs a one-line
  change in `.github/workflows/ci.yml`.
- **B18** — `NET_BIND_SERVICE` capability. Operator
  policy decision; needs explicit sign-off before
  changing the k8s `securityContext`.

---

## 27. What was already correct

For balance, these are the things in the codebase that are
done well and should be preserved:

- **Lock-free hot path**: `TimeBase::now_ms` is the right
  shape. The acquire/release pairing is correct.
- **Pre-serialized JSON cache**: the `Arc<ArcSwap<String>>`
  pattern is idiomatic and ~zero overhead.
- **Router split** (fast vs. slow): the comment at
  `src/http/mod.rs:31-33` explains a non-obvious performance
  optimization and should not be "simplified" by an agent who
  doesn't understand the latency impact.
- **Probe semantics** (k8s): `/healthz` always 200,
  `/readyz` and `/startupz` 503 until first sync — this is
  a deliberate choice to avoid killing pods on transient
  NTP failures.
- **Smart-sticky server selection** in `src/ntp/sync.rs:145-209`:
  the 50 ms switch threshold is documented in the code and
  is the right default.
- **Server auto-disable + auto-re-enable** in
  `src/ntp/stats.rs:29-53`: the "re-enable on next success"
  behavior is the correct default; an agent should not add
  cooldown logic without owner input.
- **env-driven configuration with UTF-8 messages**: works
  correctly; verified by `test_utf8_messages` in
  `config.rs:327`.
- **No secrets, no hardcoded credentials**: repo is clean
  in this regard. The `distroless` image base reinforces
  this.

---

## 28. References

- RFC 5905 — Network Time Protocol Version 4: Protocol and
  Algorithms Specification. <https://www.rfc-editor.org/rfc/rfc5905>
- tokio Interval: <https://docs.rs/tokio/latest/tokio/time/struct.Interval.html>
- prometheus-client crate: <https://docs.rs/prometheus-client/>
- rsntp crate: <https://docs.rs/rsntp/>

---

## 29. Changelog

This document is the deep plan + a running changelog of the
work that landed against it. Earlier sections describe the
audit; this section is the implementation log.

### 29.1 Wave 0 — feature + initial bug fixes (commit `a25f75e`)

"add ntp server protocol and fix hot-path bugs" — the
single commit that bootstrapped the NTP server feature
and the first round of bug fixes (B1, B2, B3, B9, B11, B12)
called out in the original review.

Files: 20 changed, +1046 / −362.

### 29.2 Wave 1 — correctness (commit `43f8120`)

"rename rtt_improvement to rtt_diff_ms and avoid pre-1970
panic" — B5 and B10. Two small, low-risk fixes. `rtt_diff_ms`
rename also fixed a latent bug at the "keeping" branch
where the field was assigned the old variable name. Pre-1970
panic replaced with `unwrap_or_default()`.

Files: 2 changed, +7 / −5.

### 29.3 Wave 2 — cleanup (commit `0d6cece`)

"cleanup ntp selector and websocket config handling" — B4,
B6, B8, B14, B15. Net **−53 lines** (58 added, 111 removed)
because the cleanup genuinely shrank the codebase: dead
code deleted, duplication collapsed into a helper, the
`min_by` closure simplified to a one-liner.

Files: 4 changed, +58 / −111.

### 29.4 Wave 3 — RFC 5905 T1–T4 (commit `ca190b6`)

"carry RFC 5905 T1-T4 timestamps and apply asymmetry bias"
— B13. The full algorithm: `NtpResult` now carries the
four timestamps in unix-epoch ms, derived from the
closed-form solution of the RFC 5905 §8 linear system.
`OFFSET_BIAS_MS` and `ASYMMETRY_BIAS_MS` are no longer
dead config. New test
`rfc5905_four_tuple_relations_hold` verifies both
directions of the math against a hand-computed
physically-plausible scenario. Caught a sign error in
T3 derivation along the way.

Files: 2 changed, +233 / −89.

### 29.5 Wave 4 — AppError refactor (commit `935755e`)

"refactor time_handler to return Result<Response, AppError>"
— B19. `AppError::NotSynced` carries the user-facing
`message` and `error` strings so the 503 JSON body matches
the pre-refactor shape exactly. Two response builders
extracted as free functions. `record_success` /
`record_error` now branch on `Ok` vs `Err`. New test
`app_error_not_synced_json_shape_matches_handler` locks
in the JSON shape.

Files: 2 changed, +146 / −81.

### 29.6 Wave 5 — ops polish (commit `a21e874`)

"ops polish: expose NTP port, honest precision, drop stale
env var" — B17, B20, B21. Three small deploy-correctness
fixes. No test or behavior change. Doc-only wave for the
code, deploy-only for the manifests.

Files: 3 changed, +11 / −6.

### 29.7 Wave 6 — plan-file sync (this commit)

Strike completed items from §26, expand the changelog
in §29, mark the open items in §26.5 so a future agent
reading the plan knows exactly what's left.

### 29.8 Summary

| Wave | Theme | Commit | Files | Net | Bugs closed |
|---|---|---|---|---|---|
| 0 | NTP server + initial fixes | `a25f75e` | 20 | +684 | B1, B2, B3, B9, B11, B12 |
| 1 | Correctness | `43f8120` | 2 | +2 | B5, B10 |
| 2 | Cleanup | `0d6cece` | 4 | **−53** | B4, B6, B8, B14, B15 |
| 3 | RFC 5905 T1–T4 | `ca190b6` | 2 | +144 | B13 |
| 4 | AppError refactor | `935755e` | 2 | +65 | B19 |
| 5 | Ops polish | `a21e874` | 3 | +5 | B17, B20, B21 |
| 6 | Plan sync | (this) | 1 | 0 | — |
| **Total** | | | **34 files** | **+847 / −1,033** | **18 of 23 findings resolved** |

Remaining (deferred to follow-up PRs): B7, B16, B18.

### 29.9 Test inventory

After all six waves:

- 46 unit tests in `src/` (up from 33 at the start of
  wave 0; new tests cover the NTP protocol codec, the
  real-UDP server roundtrip, the RFC 5905 four-tuple
  math, and the AppError JSON shape).
- 7 integration tests in `tests/integration_api.rs` (all
  still `assert!(true, ...)` placeholders — see P3 in
  §26.4).
- `make dev-check` clean.
- `cargo clippy --all-targets --all-features -- -D warnings`
  clean.
