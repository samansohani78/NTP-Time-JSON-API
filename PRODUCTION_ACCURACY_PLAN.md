# Production Accuracy & Reliability Plan (Implementation-Ready)

> Status: **design / planning only — no code in this pass.** This revision turns the roadmap into
> concrete, implementable engineering tasks: decided defaults, exact files, structs, functions,
> tests, acceptance criteria, and validation commands.
>
> Companion to `PROJECT_PLAN.md` (which now tracks these as real plan items). **Both files are
> git-ignored** today; Task OPS-1 fixes that.

---

## 0. Executive Summary

The systems engineering is solid (lock-free `TimeBase`, zero-alloc hot path, correct tested NTP
codec in `src/ntp/protocol.rs`, clean modules). The production gap is **honesty about time
quality**, rooted in one fact verified against the crate source: **`rsntp` discards every field
that matters** — it returns only offset/delay/stratum/leap/refid and throws away the raw
T2/T3/T4 timestamps, `root_delay`, `root_dispersion`, and `precision`. Consequently the T2/T3 in
`/performance` are algebraic reconstructions, the UDP server advertises `root_dispersion = 0`
(a false "perfect clock" claim), and nothing exposes uncertainty to clients.

The fix unlocks data we already parse. This plan:
- Replaces `rsntp` with an in-house packet client on the existing codec (**P0-1/P0-2**).
- Makes UDP `root_delay`/`root_dispersion` honest (**P0-3**).
- Adds a time-quality envelope + serve/stop SLA (**P0-4**).
- Builds a real E2E test harness (**P0-5**).
- Adds an uncertainty-scored selection model (**P1-6**), a secure manual-override API (**P1-7**),
  and replica-drift visibility (**P1-8**).
- Cleans up and fixes doc tracking / commit hygiene (**P2** + **OPS**).

All former open questions are now **decided** (§1). Items still needing a *human sign-off* (secrets,
SLA numbers, product behavior) are listed in §9 — they do not block starting P0-1/P0-2/P0-3/P0-5.

---

## 1. Decisions (former open questions → recommended defaults)

| # | Question | **Decision (default)** | Why it's the safe default | Choose otherwise if… | Human approval before impl? |
|---|----------|------------------------|---------------------------|----------------------|------------------------------|
| D1 | LI/stratum/refid during manual override | **Stratum 2, Reference ID `MANU`, `LI=0`, large `root_dispersion`**; `source:"manual"` everywhere in HTTP | `LI=3` (unsynced) makes NTP clients reject us entirely, defeating the point of manual mode (keep serving). `MANU` + big dispersion + HTTP flag is honest without breaking clients | Clients must hard-reject manual time at the protocol level → then use `LI=3`/Stratum 16 | **Yes** (product/security) |
| D2 | Hard-stop vs degraded-serving when stale/uncertain | **Hard-stop: `ALLOW_DEGRADED=false` → 503** past limits | Serving silently-degraded time to a financial client is worse than serving none; 503 lets clients fail over | Clients explicitly prefer stale time to no time (set `ALLOW_DEGRADED=true`) | **Yes** (product/SLA) |
| D3 | Does `/time` carry quality? | **No — keep `/time` body minimal & backward-compatible.** Add quality via (a) response headers `X-Time-Source`, `X-Time-Uncertainty-Ms` on `/time`, (b) new `GET /status` full envelope, (c) opt-in `GET /time/full` enriched JSON | Preserves the zero-alloc cached hot path and existing contract; headers are cache-friendly; clients that want quality opt in | A consumer needs quality in the `/time` body itself → enable `/time/full` (already provided) | No (non-breaking) |
| D4 | Selection algorithm depth | **Now: weighted-median + per-sample uncertainty + agreement/quorum gate. Later: Marzullo intersection + cluster/combine** | Weighted-median+uncertainty fixes the dangerous min-RTT fallback and handles most adversarial cases with modest complexity; full Marzullo is a later refinement | Biased-majority robustness becomes a hard requirement sooner → pull Marzullo into P1-6 | No (algorithm choice documented) |
| D5 | Keep `OFFSET_BIAS_MS` / `ASYMMETRY_BIAS_MS`? | **Keep, applied post-selection, surfaced in `/status`** | Some known-asymmetric links genuinely need manual calibration; making them visible removes the "invisible foot-gun" risk | Operators confirm they're unused after P1-6 ships → remove in a later cleanup | No |
| D6 | Admin auth tier | **Bearer token (constant-time compare) + optional IP allowlist + slow-router rate limit.** HMAC-signed & mTLS documented as hardening | Simplest defensible auth; constant-time avoids timing leaks; allowlist + rate limit bound blast radius | Hostile/shared network → require HMAC(body+timestamp) or mTLS/reverse-proxy-only | **Yes** (secret management/deploy) |
| D7 | Provider-group definition | **Last two DNS labels of the hostname** (e.g. `time.google.com`→`google.com`); override via `NTP_PROVIDER_GROUPS`. Cap any one group to `< quorum majority` | Cheap, no config required, catches the common "all Google" case; explicit override for edge cases | Need precise eTLD+1 (public-suffix list) → add `publicsuffix` crate later | No |
| D8 | SLA millisecond targets | **`SERVE_OK_MAX_MS=50`, `SERVE_DEGRADED_MAX_MS=250`, `READINESS_MAX_UNCERTAINTY_MS=250`** (defaults; tighten per deployment) | Conservative, round, easy to tune; 50 ms is comfortably achievable over WAN NTP | The financial SLA dictates specific numbers | **Yes** (SLA owner sets final values) |
| D9 | `rsntp` removal vs feature-flag | **Remove `rsntp`; put query behind `trait NtpClient` with a mock impl** for tests | A trait is enough for swap/testability; keeping rsntp behind a flag doubles maintenance for no benefit once tests pass | Want A/B offset comparison during rollout → keep rsntp behind `--features legacy-client` for one release | No |
| D10 | Commit-message fix for `fcd8895` | **Do NOT `git commit --amend` — it is already pushed.** Use a clarifying follow-up commit (OPS-2) | Amending published history forces a force-push and breaks anyone who pulled | Sole owner, no other clones, you accept a force-push → amend + `--force-with-lease` | **Yes** (history rewrite) |

---

## 2. P0 Tasks — Correctness Foundation (implementation-ready)

### Task P0-1: Implement a packet-level async NTP client
**Status:** todo **Priority:** P0 **Risk:** medium

**Affected files**
- `src/ntp/client.rs` *(new)* — the client + `NtpSample` + `trait NtpClient`.
- `src/ntp/protocol.rs` — add inverse helpers `ntp_short_to_ms`, `precision_log2_to_ms`.
- `src/ntp/mod.rs` — export `client`.
- `Cargo.toml` — (later, P2-9) drop `rsntp`.

**Structs / functions to add**
```rust
// src/ntp/client.rs
pub struct NtpSample {
    pub server: String,
    pub t1_unix_ms: i64, pub t2_unix_ms: i64, pub t3_unix_ms: i64, pub t4_unix_ms: i64, // T2/T3 MEASURED
    pub t1_instant: Instant, pub t4_instant: Instant, // RTT via Instant (step-immune)
    pub offset_ms: i64, pub delay_ms: i64,
    pub root_delay_ms: u32, pub root_dispersion_ms: u32, // parsed from reply (NTP short)
    pub precision_log2: i8, pub stratum: u8, pub leap: u8, pub reference_id: u32, pub poll: i8,
}

#[async_trait::async_trait]
pub trait NtpClient: Send + Sync {
    async fn query(&self, server: &str, timeout: Duration) -> anyhow::Result<NtpSample>;
}

pub struct PacketNtpClient;            // production impl (UDP + protocol.rs)
#[cfg(test)] pub struct MockNtpClient; // returns scripted NtpSample for unit tests
```
```rust
// src/ntp/protocol.rs  (add; inverse of existing ms_to_ntp_short)
pub fn ntp_short_to_ms(raw: u32) -> u64 { (raw as u64 * 1000) >> 16 }
pub fn precision_log2_to_ms(p: i8) -> f64 { 2f64.powi(p as i32) * 1000.0 }
```

**Implementation steps**
1. Resolve `server` (`host:port`) via `tokio::net::lookup_host` (rsntp used to resolve for us).
2. Bind ephemeral UDP socket (`UdpSocket::bind("0.0.0.0:0")`), `connect()` to the target.
3. Capture **T1** as a pair: `let t1_instant = Instant::now(); let t1_sys = SystemTime::now();`
   *immediately* before send. Write `unix_ms_to_ntp(t1_unix_ms)` into the request's
   `transmit_timestamp`. Build with `NtpPacket::new(LI_NO_WARNING, NTP_VERSION, MODE_CLIENT)` +
   `serialize_packet`.
4. `send` then `tokio::time::timeout(timeout, recv)`; on return capture **T4** pair *immediately*.
5. `parse_packet(&resp)` → **T2** = `ntp_to_unix_ms(receive_timestamp)`, **T3** =
   `ntp_to_unix_ms(transmit_timestamp)`; `root_delay_ms = ntp_short_to_ms(root_delay)`;
   `root_dispersion_ms = ntp_short_to_ms(root_dispersion)`; copy `precision`, `stratum`, `li`,
   `reference_id`, `poll`.
6. **Validate (safety-critical — rsntp did this for us):**
   - reply `origin_timestamp` MUST equal our request `transmit_timestamp` → else
     `bail!("origin mismatch (stale/spoofed reply)")`.
   - reject `li == LI_ALARM_UNSYNCHRONIZED`, `stratum == 0` (KoD), `stratum >= STRATUM_UNSYNCHRONIZED`.
   - reject `transmit_timestamp == 0`.
7. Compute `offset_ms = ((T2−T1)+(T3−T4))/2`, `delay_ms = (T4−T1)−(T3−T2)`; reject `delay_ms < 0`.
8. RTT for downstream use = `t4_instant − t1_instant` (Instant, not wall clock).

**Tests** (`src/ntp/client.rs` `#[cfg(test)]` + extend mock UDP server)
- `reads_real_t2_t3_byte_for_byte`: mock server replies with chosen `receive_timestamp`/
  `transmit_timestamp`; assert `t2_unix_ms`/`t3_unix_ms` equal those exact values (NOT reconstructed).
- `parses_root_delay_dispersion`: mock sets `root_delay=0x00040000` (=4 s? choose realistic),
  assert `root_delay_ms`/`root_dispersion_ms` decode correctly via `ntp_short_to_ms`.
- `rejects_origin_mismatch`, `rejects_kiss_of_death` (stratum 0), `rejects_leap_alarm`,
  `rejects_zero_transmit`, `rejects_negative_delay`, `times_out_on_silence`.

**Acceptance criteria**
- Client returns **measured** T2/T3 identical to bytes the mock server sent.
- All four reject/validate paths covered and green.
- No dependency on `rsntp` in `client.rs`.

---

### Task P0-2: Wire the client into sync; carry real fields end-to-end
**Status:** todo **Priority:** P0 **Risk:** medium

**Affected files**
- `src/ntp/sync.rs` — `query_ntp_server` calls `NtpClient::query`; `NtpSyncer` holds
  `Arc<dyn NtpClient>` (defaults to `PacketNtpClient`, injectable for tests).
- `src/ntp/selection.rs` — extend `NtpResult`.
- `src/http/state.rs` — extend `NtpTimingSummary`; add `SyncQuality` (below).
- `src/main.rs` — `sync_loop` stores new fields + `last_sync_instant`.
- `src/timebase.rs` — `SyncResult` gains the carry-through fields (no read-path change).

**Structs to change**
```rust
// NtpResult / SyncResult / NtpTimingSummary all gain:
root_delay_ms: u32, root_dispersion_ms: u32, stratum: u8, leap: u8,
precision_log2: i8, reference_id: u32,
timing_source: TimingSource,   // enum { Measured, Estimated }  -> always Measured after P0
```
```rust
// src/http/state.rs — single source for UDP server + /status
pub struct SyncQuality {
    pub upstream_root_delay_ms: u32, pub upstream_root_dispersion_ms: u32,
    pub precision_log2: i8, pub stratum: u8, pub leap: u8,
    pub measured_rtt_ms: u64, pub jitter_ms: u64, pub offset_ms: i64,
    pub last_sync_instant: Instant, pub selected_server: String,
}
pub last_sync_quality: Arc<parking_lot::RwLock<Option<SyncQuality>>>, // in AppState
```

**Implementation steps**
1. Inject `Arc<dyn NtpClient>` into `NtpSyncer::new` (default `PacketNtpClient`).
2. Replace the `rsntp` block in `query_ntp_server` with `client.query(...)`; map `NtpSample` →
   `NtpResult` (real fields).
3. In `sync_loop`, after `timebase.update`, populate `last_ntp_timing` (now `timing_source:
   Measured`) **and** `last_sync_quality` (incl. `last_sync_instant = Instant::now()`).
4. `jitter_ms`: stddev of the last N per-server offsets — add a small ring buffer to `ServerStats`
   (P1-6 refines; a 1-sample stub returning 0 is acceptable for P0).

**Tests**
- `sync_populates_real_timing`: inject `MockNtpClient`; assert `NtpTimingSummary.timing_source ==
  Measured` and root fields propagate.
- Existing sync tests updated for new fields.

**Acceptance criteria**
- `/performance` `ntp_timing` shows `timing_source:"measured"` and non-trivial
  `root_delay_ms`/`root_dispersion_ms`/`stratum`/`leap`.
- `last_sync_quality` populated on every successful sync.

---

### Task P0-3: Honest `root_delay` / `root_dispersion` on the UDP NTP server
**Status:** todo **Priority:** P0 **Risk:** low (depends on P0-2)

**Affected files**
- `src/ntp/server.rs` — `build_response` takes `Option<&SyncQuality>`; new dispersion math.
- `src/main.rs` — pass `state.last_sync_quality` into `NtpServer::new`.
- `src/metrics.rs` — `ntp_udp_server_root_dispersion_seconds` gauge.

**Math (RFC 5905 §11.2)**
```
PHI = 15e-6  // 15 ppm max local clock drift
age_s = last_sync_quality.last_sync_instant.elapsed().as_secs_f64()
root_dispersion_ms = upstream_root_dispersion_ms
                   + precision_log2_to_ms(precision_log2).abs()
                   + jitter_ms
                   + (PHI * age_s * 1000.0)
                   + (delay_ms / 2)              // offset estimation error
root_delay_ms      = upstream_root_delay_ms + measured_rtt_ms   // we are a stratum-2 relay
// clamp root_dispersion_ms to MAX_ROOT_DISPERSION_MS; advertise via ms_to_ntp_short(...)
```

**Implementation steps**
1. Thread `Option<&SyncQuality>` into `build_response`; when `None`/unsynced → keep LI=3/Stratum16,
   dispersion `0` as today (we genuinely don't know).
2. When synced → compute per the formula; set `root_delay`/`root_dispersion` via `ms_to_ntp_short`.
3. Emit `ntp_udp_server_root_dispersion_seconds`.

**Tests** (`src/ntp/server.rs` + `tests/e2e_ntp_udp.rs`)
- `root_dispersion_nonzero_when_synced`.
- `root_dispersion_grows_with_age`: inject `SyncQuality` with an older `last_sync_instant`; assert
  larger dispersion.
- `root_delay_includes_upstream`: assert advertised `root_delay >= upstream_root_delay`.

**Acceptance criteria**
- Synced UDP replies carry `root_dispersion > 0` that increases with sync age and is bounded by
  `MAX_ROOT_DISPERSION_MS`.

---

### Task P0-4: Time-quality envelope + `/status` + serve/stop policy (SLA)
**Status:** todo **Priority:** P0 **Risk:** medium (touches hot path + cache; depends on P0-2)

**Affected files**
- `src/http/handlers.rs` — `time_handler` (headers + policy), new `status_handler`, `time_full_handler`.
- `src/http/mod.rs` — routes `/status`, `/time/full`.
- `src/performance.rs` — `TimeCache` extended to also hold the current `source` flag (cheap).
- `src/http/websocket.rs` — tick gains quality fields.
- `src/config.rs` — SLA thresholds.
- `src/metrics.rs` — `time_uncertainty_milliseconds`, `time_source_mode`, `time_serve_state`.

**Quality model (computed from `SyncQuality` + staleness)**
```
source: "ntp" | "manual" | "degraded" | "unsynced"
uncertainty_ms = root_dispersion_ms (server's own, from P0-3 math) ; the single trust number
staleness_ms, stratum, selected_server, selected_provider, quorum_size, leap, last_sync_status
```
**Serve/stop policy (D2 default `ALLOW_DEGRADED=false`)**
```
unsynced & REQUIRE_SYNC          -> 503                      (unchanged)
synced & uncertainty < OK_MAX    -> 200 source=ntp
synced & staleness < MAX_STALENESS & uncertainty < DEGRADED_MAX -> 200 source=degraded (FLAGGED)
else                              -> 503 (hard stop)         unless ALLOW_DEGRADED=true
```

**Decisions applied:** D3 (`/time` body unchanged; headers + `/status` + `/time/full`), D2 (hard-stop).

**Tests**
- Policy table test per state transition (ok / degraded / stop / unsynced).
- `/status` JSON shape; degraded flag never absent when stale.
- `/time` headers present; `/time` body byte-identical to today when `source=ntp`.
- WS tick carries quality fields.

**Acceptance criteria**
- `/status` returns full envelope; `/time` stays backward-compatible; 503 enforced at hard limits;
  `source` never silently wrong.

---

### Task P0-5: Real integration / E2E test harness + CI  (see §6 for file-level detail)
**Status:** todo **Priority:** P0 **Risk:** low (additive)
Summarized here; fully specified in **§6**. Adds `src/lib.rs`, `tests/common/mod.rs`, and
`tests/e2e_*.rs`, plus a CI job. Acceptance: release pipeline runs unit → integration → live HTTP →
live UDP → WS → metrics → `cargo build --release`, all green.

---

## 3. Accuracy Model (concrete — Task P1-6)

### Task P1-6: Uncertainty-scored selection (weighted-median + quorum)
**Status:** todo **Priority:** P1 **Risk:** medium-high (depends on P0-1/P0-2)

**Affected files:** `src/ntp/selection.rs` (algorithm + `Uncertainty`), `src/ntp/sync.rs`
(gates + provider grouping), `src/ntp/stats.rs` (per-server offset ring buffer for jitter),
`src/config.rs` (knobs), `src/metrics.rs`.

**Per-sample uncertainty (root distance λ)**
```
PHI = 15e-6
lambda_ms = root_delay_ms/2 + root_dispersion_ms + delay_ms/2
          + jitter_ms + precision_log2_to_ms(precision).abs() + PHI*age_ms
weight = 1.0 / (lambda_ms + 1.0)   // +1 avoids div-by-zero; higher = more trusted
```
Fields feeding the score: `root_delay_ms`, `root_dispersion_ms`, `delay_ms`, `jitter_ms`
(stddev of recent per-server offsets), `precision`, sample age. (All now real, from P0-1.)

**Hard rejection gates (before scoring)**
- `leap == alarm` (if `REJECT_LEAP_ALARM`) · `stratum == 0 || stratum > MAX_STRATUM`
- `lambda_ms > MAX_ROOT_DISTANCE_MS` · `age > MAX_SAMPLE_AGE` · client-level validation failed.

**Selection algorithm**
1. Apply gates → `candidates`. If empty → **degraded** (no new selection; keep prior time; alert).
2. `weighted_median = weighted_median_by(candidates, |c| c.offset_ms, |c| c.weight)`.
3. `agreers = candidates where |offset − weighted_median| <= MAX_OFFSET_SKEW_MS`.
4. `quorum = agreers.len()`. If `quorum < MIN_QUORUM` → **degraded**.
5. **Provider-group cap (D7):** group agreers by `provider_group(server)`; if one group >
   `PROVIDER_GROUP_MAX_FRACTION` of agreers, set `single_provider=true`, inflate combined
   uncertainty ×2 (and optionally fail readiness via P1-8).
6. `combined_offset = weighted_mean(agreers)`; `combined_uncertainty = sqrt(Σ wᵢ·λᵢ² / Σ wᵢ) +
   spread(agreer offsets)`.
7. `selected = agreer.min_by(λ).then(min rtt)`; sticky logic (`sticky_select`) unchanged on top.

**Failure behavior when sources disagree:** **never** fall back to min-RTT (delete that path).
Empty candidates or `quorum < MIN_QUORUM` → mark `degraded`, keep serving prior good time with
inflated uncertainty, `ntp_selection_falsetickers_total++`; if never synced → stay 503.

**Config + safe defaults**
```
MAX_STRATUM=4  MIN_QUORUM=2  REJECT_LEAP_ALARM=true  MAX_ROOT_DISTANCE_MS=500
MAX_SAMPLE_AGE_SECS=<2× sync interval>  PROVIDER_GROUP_MAX_FRACTION=0.5
MAX_OFFSET_SKEW_MS=1000 (kept as coarse backstop)  NTP_PROVIDER_GROUPS="" (optional override)
```

**Metrics:** `ntp_selection_quorum_size`, `ntp_selection_falsetickers_total`,
`ntp_sample_uncertainty_milliseconds{server}`, `ntp_combined_uncertainty_milliseconds`.

**Adversarial test cases (all required; pure unit tests via extended `for_testing`)**
| Case | Setup | Expected |
|------|-------|----------|
| low RTT but wrong | one 5 ms-RTT sample offset far from consensus | rejected (non-agreer); not selected |
| high RTT but accurate | 300 ms-RTT sample agreeing w/ consensus | eligible; selected if others gated out |
| majority wrong (independent) | 3 wrong / 2 right, different providers | weighted-median follows majority → **documented limit**; `quorum`+spread metrics flag it |
| majority wrong (same provider) | 3 wrong all `x.example.com` / 2 right | provider cap → `single_provider`, uncertainty inflated, alertable |
| all disagree | no shared agreement | **degraded**, no new selection, falsetickers++ |
| one provider dominates | all agreers in one group | `single_provider=true`, uncertainty ×2 |
| leap alarm | one sample `leap=3` | hard-gated out |
| stratum too high | `stratum=8` (>MAX_STRATUM) | hard-gated out |
| stale sample | `age > MAX_SAMPLE_AGE` | hard-gated out |
| root dispersion high | `λ > MAX_ROOT_DISTANCE_MS` | hard-gated out |
| jitter high | large offset stddev | high λ → down-weighted, not selected |

**Acceptance criteria:** every row above is a green test; min-RTT fallback removed; `/status`
exposes `quorum_size`, `combined_uncertainty_ms`, `rejected_sources`, `single_provider`.

**Later (tracked as P1F-12, not in P1-6):** Marzullo interval intersection + cluster/combine; per-peer
8-sample clock filter — see next task.

### Task P1F-12: Interval-intersection / clock-combine robustness (follow-up to P1-6)
**Status:** todo **Priority:** P1-followup **Risk:** medium-high (depends on P1-6)

**Problem.** The P1-6 weighted-median can still **follow an independently-wrong majority** (multiple
distinct providers that happen to agree on a wrong offset). Weighted median has no concept of "the
truth might be outside the majority," so it can confidently select a wrong consensus.

**Recommended solution.** Replace/augment the weighted-median step with **Marzullo/Intersection
(RFC 5905 §11.2.1)** over the per-sample uncertainty intervals `[θ−λ, θ+λ]`: keep the largest set of
mutually-overlapping intervals (truechimers), discard the rest (falsetickers), then **cluster +
combine** the survivors. Optionally add **source trust tiers** (`NTP_SOURCE_TIERS`) so a known-good
stratum-1 source can outweigh a larger group of lower-tier peers.

**Affected files.** `src/ntp/selection.rs`, `src/ntp/sync.rs`, `src/config.rs`, `src/metrics.rs`.

**Config (additive).** `SELECTION_ALGO=weighted_median|intersection` (default flips to
`intersection` once validated), `NTP_SOURCE_TIERS` (optional map server→tier).

**Acceptance criteria.** The **independently-wrong-majority** adversarial test (3 distinct providers
agree on a wrong offset, 1–2 correct) must **fail closed** — i.e. it MUST do at least one of:
(a) reject the wrong consensus via interval intersection so it is not selected; **or**
(b) apply trust tiers so the higher-trust correct source(s) win; **or**
(c) expose `combined_uncertainty_ms` high enough that the **P1-8 readiness gate rejects the replica**
(`uncertainty > READINESS_MAX_UNCERTAINTY_MS` → `/readyz` 503).
A **silent confident wrong selection is not acceptable.** Metric `ntp_selection_falsetickers_total`
must increment when falsetickers are discarded.

---

## 4. Manual Override (full design — Task P1-7)

### Task P1-7: Secure manual time-override admin API
**Status:** todo **Priority:** P1-high (P0/P1 boundary) **Risk:** HIGH (security + hot-path `TimeBase`) — isolated phase, dedicated security review.

> **Priority note.** Manual override is a **core product requirement**, not optional future fluff. It
> is sequenced after P0-4 **only** because it technically depends on the time-quality envelope (manual
> mode must surface `source:"manual"` + a conservative uncertainty rather than impersonate NTP) and
> because it needs a dedicated security review before merge. It is the **highest-priority P1** — begin
> it as soon as P0-4 lands and a security reviewer is available.

**Decisions (defaults, per prompt — all accepted):** disabled by default; `ADMIN_API_ENABLED=true`
required; bearer `ADMIN_API_TOKEN` (constant-time); TTL required; reason required; max-jump enforced
unless `MANUAL_OVERRIDE_ALLOW_FORCE=true`; responses expose `source:"manual"`; UDP Reference ID
`MANU`; manual mode never impersonates normal NTP sync.

**Affected files:** `src/timebase.rs` (manual layer + precedence), `src/http/handlers_admin.rs`
*(new)*, `src/http/mod.rs` (guarded `/admin` router), `src/http/middleware.rs` (auth + allowlist),
`src/http/state.rs` (override state), `src/ntp/server.rs` (`MANU` + dispersion when manual),
`src/config.rs`, `src/metrics.rs`.

**TimeBase changes (read-path precedence: manual → NTP → unsynced)**
```rust
manual_active: Arc<AtomicBool>,
manual_base_epoch_ms: Arc<AtomicI64>,
manual_base_instant_nanos: Arc<AtomicU64>,
manual_expires_at_nanos: Arc<AtomicU64>,   // absolute, since REFERENCE_INSTANT
// now_ms(): if manual_active && now < expires -> manual base+elapsed; else if has_synced -> NTP; else None.
// Monotonic clamp (last_served_ms) applies to the FINAL value regardless of source -> no backward jump on enter/exit/expiry.
```

**Endpoints (slow router only, behind admin guard + rate limit)**
- `POST /admin/time/override` — body `{ epoch_ms, reason, ttl_seconds, force }`
- `DELETE /admin/time/override` — revert to NTP
- `GET /admin/time/override` — current state
- `POST /admin/sync/force` — trigger immediate upstream sync

**Auth model (D6):** `ADMIN_API_ENABLED` gate → else 404. `Authorization: Bearer <token>` compared
with constant-time eq (`subtle::ConstantTimeEq`); optional CIDR allowlist `ADMIN_API_ALLOWLIST`;
slow-router rate limit. Token never logged. HMAC/mTLS documented as hardening.

**Env vars**
```
ADMIN_API_ENABLED=false  ADMIN_API_TOKEN=<secret>  ADMIN_API_ALLOWLIST=""(CIDRs)
MANUAL_OVERRIDE_MAX_TTL_SECS=3600  MANUAL_OVERRIDE_MAX_JUMP_MS=60000
MANUAL_OVERRIDE_ALLOW_FORCE=false  MANUAL_OVERRIDE_DISPERSION_MS=1000
```

**Request / Response JSON**
```jsonc
// POST /admin/time/override
{ "epoch_ms": 1735459200000, "reason": "NTP providers unreachable", "ttl_seconds": 300, "force": false }
// 200
{ "status": 200, "source": "manual", "epoch_ms": 1735459200000,
  "expires_at_ms": 1735459500000, "reason": "NTP providers unreachable", "operator": "<token-id|ip>" }
```

**Validation:** `1 <= ttl_seconds <= MANUAL_OVERRIDE_MAX_TTL_SECS`; `reason` non-empty;
`|epoch_ms − now_ms| <= MANUAL_OVERRIDE_MAX_JUMP_MS` unless `force && MANUAL_OVERRIDE_ALLOW_FORCE`
(else 400). TTL expiry auto-reverts to NTP without backward jump.

**Behavior surfaces**
- `/time`: served value = manual; header `X-Time-Source: manual`. Body adds `source` only via
  `/time/full` (D3).
- `/performance` & `/status`: `source:"manual"`, `expires_at_ms`, `reason`, `operator`.
- UDP NTP: Stratum 2, `LI=0`, Reference ID `MANU`, `root_dispersion = MANUAL_OVERRIDE_DISPERSION_MS`
  (conservative), `root_delay = 0` (no upstream). (D1)

**Audit logging:** structured event per action — `action`, `operator`, `source_ip`, `old_epoch`,
`new_epoch`, `delta_ms`, `reason`, `ttl`, `force`, `result`. Emit at `warn` so it's never filtered.

**Metrics:** `time_source_mode{mode="ntp|manual|degraded|unsynced"}`, `manual_override_active` (0/1),
`manual_override_expiry_timestamp_seconds`, `manual_override_total` (counter).

**Security risks & mitigations**
- token leak → never log; constant-time compare; allowlist; rate limit.
- replay → offer HMAC(body+timestamp) variant; short TTL.
- accidental enable → disabled by default + explicit env + 404 when disabled.
- malicious huge jump → max-jump bound; force gated by separate env.
- DoS on admin path → slow-router rate limit.

**Tests (`tests/e2e_manual_override.rs` + `src/timebase.rs` units)**
disabled-by-default→404; missing/wrong token→401/403; allowlist enforced; set→`/time`+`/status`+UDP
reflect manual & `MANU`; clear→back to NTP; TTL expiry auto-reverts (no backward jump); jump>max→400;
force allowed only when configured; rate-limited; audit log emitted; monotonic preserved across
enter/exit/expiry.

**Acceptance criteria:** all tests green; manual mode unmistakable in HTTP + UDP; never advertises
as normal NTP; bounded, TTL'd, audited; default-off verified.

---

## 5. HA / Replica Drift (actionable — Task P1-8)

### Task P1-8: Replica-drift visibility (no cross-replica consensus)
**Status:** todo **Priority:** P1 **Risk:** low

**Decision:** do **not** build consensus/leader election (adds a worse SPOF). Make shared-nothing
replicas’ disagreement **observable and gated**.

**Affected files:** `src/http/handlers.rs` (`/status` + readiness gate), `src/config.rs`
(`REPLICA_ID`, `READINESS_MAX_UNCERTAINTY_MS`), `src/metrics.rs`, `k8s/prometheus-rules.yaml`
*(new)*, `PROJECT_ARCHITECTURE.md` (document the decision).

**Tasks**
1. `REPLICA_ID` env (default = hostname). Expose in `/status` and as metric label.
2. Metrics: `time_replica_info{replica_id,selected_server,source}=1`,
   `time_offset_milliseconds`, `time_uncertainty_milliseconds`.
3. `/status` exposes: `replica_id`, `source`, `selected_server`, `selected_provider`,
   `uncertainty_ms`, `staleness_ms`, `quorum_size`.
4. Readiness gate: `/readyz` returns 503 when `uncertainty_ms > READINESS_MAX_UNCERTAINTY_MS`
   (D8 default 250) → LB drops the diverged replica gracefully.
5. `k8s/prometheus-rules.yaml`: alert on inter-replica spread
   `max(time_offset_milliseconds) − min(time_offset_milliseconds) > THRESHOLD`.
6. Deployment guidance (docs): identical `NTP_SERVERS` across replicas; any LB algorithm is fine
   because drift is bounded + observable; no quorum/odd-count requirement.

**Tests**
- `/status` includes `replica_id` + uncertainty.
- readiness flips to 503 when injected uncertainty exceeds threshold; back to 200 below.
- metric emitted with labels.

**Acceptance criteria:** operators can detect two replicas disagreeing via a single Prometheus
expression; a too-uncertain replica is removed from rotation automatically.

---

## 6. Integration / E2E Test Plan (file-level — Task P0-5)

**Decision:** add `src/lib.rs` (expose modules + `pub async fn run()`); `main.rs` becomes a thin
shim. Most E2E spawns the server **in-process** on port `:0` (deterministic, no port races); one
binary smoke uses `assert_cmd` to validate the packaged artifact.

**File list**
- `src/lib.rs` *(new)* — `pub mod {config,errors,http,metrics,ntp,performance,timebase};` +
  `pub async fn run() -> anyhow::Result<()>` (move body from `main.rs`).
- `src/main.rs` — `fn main() { … lib::run() … }`.
- `tests/common/mod.rs` *(new)* — helpers: `spawn_test_server(config) -> TestServer{base_url,
  shutdown}`; `mock_udp_ntp_server(epoch) -> SocketAddr` (lift from `src/http/mod.rs`);
  `build_ntp_request(t1)`; `parse_ntp_reply(buf)`.
- `tests/integration_api.rs` → **replace** placeholder by `git mv` to `tests/e2e_http.rs`.
- `tests/e2e_http.rs` — `/time` (503 pre-sync, 200 post-sync via mock NTP, monotonic, headers),
  `/healthz`, `/readyz`, `/startupz`, `/performance` (incl. `timing_source:"measured"`), `/status`,
  `/time/full`.
- `tests/e2e_ntp_udp.rs` — raw Mode 3 packet → parse reply: Mode 4, Stratum 2, RefID `LOCL`,
  origin echo, non-zero recv/transmit, `root_dispersion > 0` and grows with age (P0-3).
- `tests/e2e_websocket.rs` — `tokio-tungstenite` connect; tick cadence, `is_stale`, monotonic
  `epoch_ms`, `sequence` increments, quality fields.
- `tests/e2e_metrics.rs` — scrape `/metrics`; assert `build_info`, `ntp_sync_total`,
  `ntp_udp_server_*`, `time_uncertainty_milliseconds`, `time_source_mode`.
- `tests/e2e_manual_override.rs` — full P1-7 suite.

**dev-dependencies (`Cargo.toml`):** `reqwest` (json, rustls), `tokio-tungstenite`, `assert_cmd`,
`predicates`.

**CI (`.github/workflows/ci.yml`)**
- existing `test` job: `cargo test --all` (unit + integration in-process).
- new `e2e` job (after `build`): `cargo test --test e2e_http --test e2e_ntp_udp --test
  e2e_websocket --test e2e_metrics`, then a binary smoke (`assert_cmd` spawn + curl `/time` + raw
  UDP). `e2e_manual_override` runs with `ADMIN_API_ENABLED=true ADMIN_API_TOKEN=test-only`.
- `Makefile`: `make e2e` runs the e2e tests; `make ci` stays fmt+clippy+test.

**What stays manual/smoke:** `test_api.sh`, `benchmark.sh`, `benchmark_websocket.py` remain manual
load/smoke tools (not in CI).

**Acceptance criteria:** `make e2e` green locally and in CI; placeholder `integration_api.rs` gone;
release pipeline exercises HTTP + UDP + WS + metrics against a running server.

---

## 7. Documentation Consistency & Tracking

### 7.1 Task DOC-1: Reconcile public docs with reality (do BEFORE the OPS-1 commit)
**Status:** todo **Priority:** P1 **Risk:** none (content only)

`README.md` (the only currently-tracked public doc) and `CLAUDE.md` contain claims that contradict
both the current code and `PRODUCTION_ACCURACY_PLAN.md`. They must be corrected **before** OPS-1
commits the plan docs, so the repo does not ship self-contradicting documentation.

**`README.md` — exact fixes:**
1. Line ~3: "A production-ready HTTP service" → qualify: general-purpose / production-oriented;
   **not** financial/time-critical ready until P0 lands; point to `PRODUCTION_ACCURACY_PLAN.md`.
2. Lines ~10 & ~40: "RTT-based selection" / "RTT-min strategy (chooses server with lowest
   round-trip time)" → **wrong**. Actual algorithm is **accuracy-first** (closest to consensus/median
   offset, RTT only as tiebreaker). Reword; note P1-6 will replace it with uncertainty-scored selection.
3. Line ~169: documents `SAMPLE_SERVERS_PER_SYNC` — **no such env var exists** (`config.rs` queries
   **all** configured servers each sync). Remove it.
4. Line ~168: `SELECTION_STRATEGY=rtt_min` — keep the value (back-compat) but document it as a
   historical alias for accuracy-first, not "lowest RTT".
5. Line ~325: "Runs as non-root user (UID 1000)" → distroless `nonroot` is **UID 65532** (recent
   Dockerfile change). Correct it.
6. Add a short "Current limitations / not-yet-financial-ready" note (estimated T2/T3,
   `root_dispersion=0`, no quality envelope, no manual override, no full E2E CI) linking the plan.
7. Project-structure tree (~336-358) omits `protocol.rs`, `server.rs`, `performance.rs`,
   `websocket.rs`, `tests/` reality — update or mark as illustrative.
8. Metrics list omits the `ntp_udp_server_*` family — add it (and note it's renamed from
   `ntp_server_*`).
9. Verify `PROTOCOL_COMPARISON.md` (referenced ~457) exists; if not, drop the reference.

**`CLAUDE.md` — exact fixes:**
1. Line ~55: "`selection.rs` (RTT-min strategy)" → "accuracy-first (closest to consensus offset;
   RTT tiebreaker)".
2. Line ~36: "rsntp (NTP client)" → keep as current state but add "(P0-1 replaces this with an
   in-house packet client for measured T1–T4; see `PRODUCTION_ACCURACY_PLAN.md`)".
3. No "production-ready" claim present — leave as is.

**Validation:**
```bash
grep -nE "production-ready|RTT-min|RTT-based|SAMPLE_SERVERS_PER_SYNC|UID 1000" README.md CLAUDE.md
```
should return **only** intended/qualified hits (e.g. a sentence that explicitly says "not financial
production-ready").

**Acceptance criteria:** no public doc asserts unqualified "production-ready", "RTT-min/RTT-based
selection", a non-existent config var, or the wrong container UID; README links the accuracy plan;
public docs no longer contradict `PRODUCTION_ACCURACY_PLAN.md`.

### 7.2 Task OPS-1: Track the plan/docs in git
**Status:** todo **Priority:** P2 **Risk:** none
The `*.md` glob (`.gitignore:38`) ignores everything except already-tracked `README.md`. Add
explicit un-ignores:
```gitignore
# keep planning + guidance docs tracked despite the blanket *.md ignore
!PROJECT_ARCHITECTURE.md
!PROJECT_PLAN.md
!PRODUCTION_ACCURACY_PLAN.md
!CLAUDE.md
```
**`CLAUDE.md`: recommend tracking it** — it is repo-level guidance for Claude Code and belongs in
version control so all contributors share it. (`README.md` is already tracked — confirmed via
`git ls-files '*.md'`.)
**Validation:** `git check-ignore -v PROJECT_PLAN.md` returns nothing; `git status` shows the four
docs as untracked/added.
**Acceptance:** the three plan docs + `CLAUDE.md` are addable and committable.

---

## 8. Commit-Message Fix (Task OPS-2)

### Task OPS-2: Correct the misleading commit message on `fcd8895`
**Status:** todo **Priority:** P2 **Risk:** medium (history)
`fcd8895` ("Change Dockerfile to use distroless image") actually contains the T1-T4 / UDP-metrics /
rate-limiter / docs work. **Verified it is already pushed** (`origin/main == fcd8895`).

**Therefore do NOT amend** (the prompt's `git commit --amend` is only safe for unpushed commits).
Options, in order of safety:
- **(default, safe)** Add a clarifying follow-up commit and move on:
  ```bash
  git commit --allow-empty -m "Note: fcd8895 contains NTP timing, UDP metrics, rate limiting, and production docs (prior message was inaccurate)"
  ```
- **(only if sole owner, no other clones)** Rewrite + force-push:
  ```bash
  git commit --amend -m "Improve NTP timing, UDP metrics, rate limiting, and production docs"
  git push --force-with-lease
  ```
**Acceptance:** history clearly attributes the change; no force-push unless explicitly approved (D10).

---

## 9. Human Approval Points (do NOT proceed on these without sign-off)
- **D1** manual-mode LI/stratum behavior (financial clients may require hard-reject).
- **D2** hard-stop vs degraded-serving default.
- **D6** admin auth tier + secret management for production.
- **D8** final SLA millisecond numbers.
- **P1-7** manual override — full security review of the diff before merge.
- **OPS-2** any history rewrite / force-push.

Everything else (P0-1, P0-2, P0-3, P0-5, P1-6, P1F-12, P1-8, DOC-1, OPS-1) can start without further
product input, using the decided defaults above.

---

## 10. Implementation Order (dependency-aware)
```
P0-1 ─┬─> P0-2 ─┬─> P0-3 ──> P0-4 ──> P1-7 (highest P1; needs P0-4 + security review)
      │         ├─> P1-6 ──> P1F-12 (intersection/clock-combine robustness follow-up)
      │         └─> P1-8 (replica drift; needs P0-2 quality data)
      └─> (mock client enables) P0-5 harness  (can start in parallel)
DOC-1 (fix README/CLAUDE) ──> OPS-1 (un-ignore + commit docs)   // do DOC-1 first
OPS-2 commit-message note (anytime; no force-push without approval)
```
**First task to implement: P0-1 (packet-level NTP client)** — it unblocks P0-2/P0-3/P0-4/P1-6.

## No-Code Rule
This pass is **planning only**. No source, config, or test code was written or modified. Implementation
begins after the §9 sign-offs, starting with P0-1.
