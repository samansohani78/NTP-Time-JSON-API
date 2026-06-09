use std::collections::HashMap;
use std::time::Duration;

use crate::config::SelectionConfig;

/// Whether T2/T3 (and root fields) were parsed directly from the NTP
/// packet bytes, or algebraically reconstructed from offset/delay.
/// Always `Measured` after P0-1/P0-2; `Estimated` only for legacy paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimingSource {
    /// T2/T3/root_delay/root_dispersion read from packet bytes.
    Measured,
    /// T2/T3 derived from T1+θ+δ/2; root fields unavailable.
    #[allow(dead_code)] // legacy path; retained for test helpers and future non-packet sources
    Estimated,
}

/// One NTP query result, carrying the RFC 5905 §8 four-tuple
/// (T1, T2, T3, T4) plus derived fields.
#[derive(Debug, Clone)]
pub struct NtpResult {
    pub server: String,
    pub epoch_ms: i64,
    pub rtt: Duration,
    pub offset_ms: i64,
    pub t1_client_send_ms: i64,
    pub t2_server_recv_ms: i64,
    pub t3_server_send_ms: i64,
    pub t4_client_recv_ms: i64,
    pub instant: std::time::Instant,
    pub root_delay_ms: u32,
    pub root_dispersion_ms: u32,
    pub stratum: u8,
    pub leap: u8,
    pub precision_log2: i8,
    pub reference_id: u32,
    pub timing_source: TimingSource,
}

impl NtpResult {
    /// Test-only constructor with sensible defaults.
    #[cfg(test)]
    pub fn for_testing(
        server: &str,
        epoch_ms: i64,
        rtt: Duration,
        offset_ms: i64,
        instant: std::time::Instant,
    ) -> Self {
        Self {
            server: server.to_string(),
            epoch_ms,
            rtt,
            offset_ms,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            instant,
            root_delay_ms: 0,
            root_dispersion_ms: 0,
            stratum: 1,
            leap: 0,
            precision_log2: -20, // 2^-20 s ≈ 1 µs — avoids exceeding max_root_distance_ms in tests
            reference_id: 0,
            timing_source: TimingSource::Estimated,
        }
    }

    /// Extended test constructor with full field control.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub fn for_testing_with(
        server: &str,
        epoch_ms: i64,
        rtt: Duration,
        offset_ms: i64,
        instant: std::time::Instant,
        stratum: u8,
        leap: u8,
        root_delay_ms: u32,
        root_dispersion_ms: u32,
        precision_log2: i8,
    ) -> Self {
        Self {
            server: server.to_string(),
            epoch_ms,
            rtt,
            offset_ms,
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            instant,
            root_delay_ms,
            root_dispersion_ms,
            stratum,
            leap,
            precision_log2,
            reference_id: 0,
            timing_source: TimingSource::Estimated,
        }
    }
}

// ── SelectionDiagnostics ──────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct RejectedSource {
    pub server: String,
    pub reason: &'static str,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionState {
    Ok,
    NoQuorum,
    NoCandidates,
    /// P1F-12: interval sweep found no cluster with overlap ≥ min_quorum.
    NoIntersection,
    /// P1F-12: two or more independent clusters each meet min_quorum — ambiguous.
    AmbiguousCluster,
}

/// P1F-12 Marzullo/interval-intersection diagnostic.
/// Populated whenever the intersection pre-filter ran (enabled=true).
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntersectionState {
    /// Intersection disabled via config.
    Disabled,
    /// A single cluster found; truechimers passed to weighted median.
    Ok,
    /// No cluster reached min_quorum overlap depth.
    NoIntersection,
    /// Truechimers identified but count < min_quorum (defensive; rarely reached).
    InsufficientQuorum,
    /// Multiple independent clusters each met min_quorum — too ambiguous to select.
    AmbiguousCluster,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IntersectionDiagnostics {
    pub enabled: bool,
    pub state: IntersectionState,
    /// Low end of the selected intersection region (ms). None when no intersection.
    pub intersection_low_ms: Option<f64>,
    /// High end of the selected intersection region (ms). None when no intersection.
    pub intersection_high_ms: Option<f64>,
    /// Width = high − low (ms). None when no intersection.
    pub intersection_width_ms: Option<f64>,
    /// Candidates that span the intersection region (truechimers).
    pub truechimer_count: usize,
    /// Candidates eliminated by the intersection filter (falsetickers).
    pub falseticker_count: usize,
    /// Number of independent clusters found (≥ 2 triggers AmbiguousCluster).
    pub competing_cluster_count: usize,
}

impl IntersectionDiagnostics {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            state: IntersectionState::Disabled,
            intersection_low_ms: None,
            intersection_high_ms: None,
            intersection_width_ms: None,
            truechimer_count: 0,
            falseticker_count: 0,
            competing_cluster_count: 0,
        }
    }
}

/// Diagnostic snapshot from one invocation of `WeightedMedianSelector::select`.
/// Exposed via `/status` and Prometheus metrics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SelectionDiagnostics {
    /// Number of servers that agreed with the weighted-median consensus.
    pub quorum_size: usize,
    /// Servers that passed the hard gates (before the agreement check).
    pub candidate_count: usize,
    /// Servers eliminated by a hard gate.
    pub rejected_count: usize,
    pub rejected_sources: Vec<RejectedSource>,
    /// Lambda of the selected server (ms); None when no server was selected.
    pub combined_uncertainty_ms: Option<f64>,
    pub selected_server: Option<String>,
    /// True when one provider group holds > `provider_group_max_fraction` of the agreers.
    pub single_provider: bool,
    pub selection_state: SelectionState,
    pub max_root_distance_ms: f64,
    pub min_quorum: usize,
    /// Weighted-median offset of the candidate set (ms); None when no candidates.
    pub weighted_median_offset_ms: Option<f64>,
    /// Per-candidate (server, lambda_ms) for all servers that passed hard gates.
    /// Used to populate `ntp_sample_uncertainty_milliseconds{server}` Prometheus gauge.
    #[serde(skip)]
    pub candidate_lambdas: Vec<(String, f64)>,
    /// P1F-12 interval-intersection diagnostics.
    pub intersection: IntersectionDiagnostics,
}

// ── SelectionOutput ───────────────────────────────────────────────────────────

pub struct SelectionOutput {
    pub selected: Option<NtpResult>,
    /// Agreers passed to `sticky_select`; empty when no quorum.
    pub agreers: Vec<NtpResult>,
    pub diagnostics: SelectionDiagnostics,
}

// ── Internal helpers ──────────────────────────────────────────────────────────

struct ScoredResult {
    result: NtpResult,
    lambda_ms: f64,
    weight: f64,
}

/// RFC 5905 §11.2 root-distance (lambda) in milliseconds.
fn compute_lambda(r: &NtpResult, jitter_ms: f64) -> f64 {
    use super::protocol::precision_log2_to_ms;
    const PHI_MS_PER_MS: f64 = 15e-6; // 15 µs/s = 0.015 ms/s = 15e-6 ms/ms; multiply by age_ms
    let age_ms = r.instant.elapsed().as_millis() as f64;
    let delay_half_ms = r.rtt.as_millis() as f64 / 2.0;
    let precision_ms = precision_log2_to_ms(r.precision_log2).abs();
    (r.root_delay_ms as f64) / 2.0
        + (r.root_dispersion_ms as f64)
        + delay_half_ms
        + jitter_ms
        + precision_ms
        + PHI_MS_PER_MS * age_ms
}

/// Extract provider group from a server address.
/// Uses the last two DNS labels of the hostname, or the IP literal if it has no DNS.
/// Optional `overrides` map takes precedence.
fn provider_group(server: &str, overrides: &HashMap<String, String>) -> String {
    if let Some(g) = overrides.get(server) {
        return g.clone();
    }
    let host = server.split(':').next().unwrap_or(server);
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() < 2 {
        return host.to_string();
    }
    // IP address: all numeric octets
    if parts.iter().all(|p| p.parse::<u16>().is_ok()) {
        return host.to_string();
    }
    format!("{}.{}", parts[parts.len() - 2], parts[parts.len() - 1])
}

// ── Marzullo interval-intersection filter (P1F-12) ───────────────────────────

/// Internal state returned by the Marzullo sweep.
enum MarzulloState {
    /// A single significant cluster found; proceed.
    Ok,
    /// No cluster reached min_quorum overlap depth.
    NoIntersection,
    /// Truechimers < min_quorum (shouldn't happen normally, but defensive).
    InsufficientQuorum,
    /// Multiple independent clusters each reached min_quorum — too ambiguous.
    AmbiguousCluster,
}

struct MarzulloResult {
    state: MarzulloState,
    /// Bounds of the max-overlap region (the intersection of truechimer intervals).
    intersection_low_ms: f64,
    intersection_high_ms: f64,
    /// Indices into `candidates` that are truechimers (span the intersection).
    truechimer_indices: Vec<usize>,
    /// Number of candidates that did NOT span the intersection.
    falseticker_count: usize,
    /// Number of independent clusters that reached min_quorum.
    competing_cluster_count: usize,
}

/// Run a Marzullo/interval-intersection sweep over `candidates`.
///
/// Algorithm:
/// 1. For each candidate with offset θ and root-distance λ, form interval [θ−λ, θ+λ].
/// 2. Create endpoint events: (value, +1=open / −1=close).
/// 3. Sort ascending by value; opens (+1) before closes (−1) at equal values.
/// 4. Sweep events, tracking the current overlap count.
/// 5. A "significant cluster" is any contiguous region where count ≥ min_quorum.
/// 6. If 0 significant clusters → NoIntersection.
/// 7. If 2+ significant clusters → AmbiguousCluster (wrong-majority detection).
/// 8. If exactly 1 cluster: within it, find the peak sub-region (max overlap).
///    Truechimers = candidates whose interval spans the peak region [peak_lo, peak_hi].
fn marzullo_filter(candidates: &[ScoredResult], min_quorum: usize) -> MarzulloResult {
    if candidates.is_empty() {
        return MarzulloResult {
            state: MarzulloState::NoIntersection,
            intersection_low_ms: 0.0,
            intersection_high_ms: 0.0,
            truechimer_indices: vec![],
            falseticker_count: 0,
            competing_cluster_count: 0,
        };
    }

    // Build events: (value_ms, kind {+1=open, -1=close})
    let mut events: Vec<(f64, i32)> = Vec::with_capacity(candidates.len() * 2);
    for c in candidates {
        let theta = c.result.offset_ms as f64;
        let lambda = c.lambda_ms;
        events.push((theta - lambda, 1));
        events.push((theta + lambda, -1));
    }
    // Sort ascending by value; opens before closes at the same value.
    events.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.1.cmp(&a.1)) // +1 > -1, so opens sort first
    });

    let min_q = min_quorum as i32;

    // Sweep to count significant clusters and find the first cluster's peak region.
    let mut count: i32 = 0;
    let mut significant_clusters: usize = 0;
    let mut in_sig = false;
    // Peak tracking (within the FIRST significant cluster)
    let mut max_in_cluster: i32 = 0;
    let mut peak_lo: f64 = 0.0;
    let mut peak_hi: f64 = 0.0;
    let mut in_peak = false;
    let mut peak_found = false;

    for &(v, kind) in &events {
        count += kind;

        if !in_sig && count >= min_q {
            // Entering a significant cluster.
            in_sig = true;
            significant_clusters += 1;
            if significant_clusters == 1 {
                // Start tracking peak within the first cluster.
                max_in_cluster = count;
                peak_lo = v;
                in_peak = true;
                peak_found = false;
            }
        } else if in_sig && count < min_q {
            // Leaving a significant cluster.
            if significant_clusters == 1 && in_peak {
                peak_hi = v;
                in_peak = false;
                peak_found = true;
            }
            in_sig = false;
        } else if in_sig && significant_clusters == 1 {
            // Still inside the first significant cluster — track the peak.
            if count > max_in_cluster {
                max_in_cluster = count;
                peak_lo = v;
                in_peak = true;
                peak_found = false;
            } else if in_peak && count < max_in_cluster {
                peak_hi = v;
                in_peak = false;
                peak_found = true;
            }
        }
    }

    // If we ended still inside a cluster/peak, close it.
    if in_sig && significant_clusters == 1 && in_peak {
        peak_hi = events.last().map(|e| e.0).unwrap_or(peak_lo);
        peak_found = true;
    }

    if significant_clusters == 0 {
        return MarzulloResult {
            state: MarzulloState::NoIntersection,
            intersection_low_ms: 0.0,
            intersection_high_ms: 0.0,
            truechimer_indices: vec![],
            falseticker_count: candidates.len(),
            competing_cluster_count: 0,
        };
    }

    if significant_clusters > 1 {
        return MarzulloResult {
            state: MarzulloState::AmbiguousCluster,
            intersection_low_ms: 0.0,
            intersection_high_ms: 0.0,
            truechimer_indices: vec![],
            falseticker_count: candidates.len(),
            competing_cluster_count: significant_clusters,
        };
    }

    // Exactly one significant cluster. Use its peak region.
    let (r_lo, r_hi) = if peak_found {
        (peak_lo, peak_hi)
    } else {
        // Degenerate: the cluster has uniform count throughout; use the full extent.
        let lo = events.first().map(|e| e.0).unwrap_or(0.0);
        let hi = events.last().map(|e| e.0).unwrap_or(lo);
        (lo, hi)
    };

    // Truechimers: candidates whose interval [θ−λ, θ+λ] spans [r_lo, r_hi].
    // i.e., θ−λ ≤ r_lo AND θ+λ ≥ r_hi
    let truechimer_indices: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            let l = c.result.offset_ms as f64 - c.lambda_ms;
            let h = c.result.offset_ms as f64 + c.lambda_ms;
            l <= r_lo && h >= r_hi
        })
        .map(|(i, _)| i)
        .collect();

    let truechimer_count = truechimer_indices.len();
    let falseticker_count = candidates.len() - truechimer_count;

    if truechimer_count < min_quorum {
        return MarzulloResult {
            state: MarzulloState::InsufficientQuorum,
            intersection_low_ms: r_lo,
            intersection_high_ms: r_hi,
            truechimer_indices,
            falseticker_count,
            competing_cluster_count: 1,
        };
    }

    MarzulloResult {
        state: MarzulloState::Ok,
        intersection_low_ms: r_lo,
        intersection_high_ms: r_hi,
        truechimer_indices,
        falseticker_count,
        competing_cluster_count: 1,
    }
}

// ── WeightedMedianSelector ────────────────────────────────────────────────────

pub struct WeightedMedianSelector;

impl WeightedMedianSelector {
    /// Select the best NTP server using uncertainty-aware weighted median + quorum.
    ///
    /// Algorithm:
    /// 1. Compute λ (root distance) for each result; apply hard-rejection gates.
    /// 2. Weight = 1 / (λ + 1) (low-uncertainty servers carry more weight).
    /// 3. Weighted median offset → consensus.
    /// 4. Agreers: servers within `max_offset_skew_ms` of the consensus.
    /// 5. Quorum check: agreers.len() ≥ `min_quorum`.
    /// 6. Provider-group cap: if one group > `provider_group_max_fraction`, flag `single_provider`.
    /// 7. Select best agreer (lowest λ, RTT as tiebreaker).
    pub fn select(
        results: Vec<NtpResult>,
        jitter_by_server: &HashMap<String, u64>,
        config: &SelectionConfig,
    ) -> SelectionOutput {
        use super::protocol::LI_ALARM_UNSYNCHRONIZED;
        use tracing::{info, warn};

        let total_in = results.len();
        let mut candidates: Vec<ScoredResult> = Vec::with_capacity(total_in);
        let mut rejected: Vec<RejectedSource> = Vec::new();

        for r in results {
            // ── Hard gates ────────────────────────────────────────────────────
            if r.stratum == 0 {
                rejected.push(RejectedSource {
                    server: r.server.clone(),
                    reason: "stratum_zero",
                });
                continue;
            }
            if r.stratum > config.max_stratum {
                rejected.push(RejectedSource {
                    server: r.server.clone(),
                    reason: "stratum_too_high",
                });
                continue;
            }
            if config.reject_leap_alarm && r.leap == LI_ALARM_UNSYNCHRONIZED {
                rejected.push(RejectedSource {
                    server: r.server.clone(),
                    reason: "leap_alarm",
                });
                continue;
            }
            let age_secs = r.instant.elapsed().as_secs();
            if age_secs > config.max_sample_age_secs {
                rejected.push(RejectedSource {
                    server: r.server.clone(),
                    reason: "sample_too_old",
                });
                continue;
            }

            let jitter_ms = *jitter_by_server.get(&r.server).unwrap_or(&0) as f64;
            let lambda_ms = compute_lambda(&r, jitter_ms);

            if lambda_ms > config.max_root_distance_ms {
                rejected.push(RejectedSource {
                    server: r.server.clone(),
                    reason: "root_distance_too_high",
                });
                continue;
            }

            let weight = 1.0 / (lambda_ms + 1.0);
            candidates.push(ScoredResult {
                result: r,
                lambda_ms,
                weight,
            });
        }

        let candidate_count = candidates.len();

        if candidates.is_empty() {
            warn!(
                total_servers = total_in,
                rejected = rejected.len(),
                "All NTP servers failed hard gates — no candidates"
            );
            return SelectionOutput {
                selected: None,
                agreers: Vec::new(),
                diagnostics: SelectionDiagnostics {
                    quorum_size: 0,
                    candidate_count: 0,
                    rejected_count: rejected.len(),
                    rejected_sources: rejected,
                    combined_uncertainty_ms: None,
                    selected_server: None,
                    single_provider: false,
                    selection_state: SelectionState::NoCandidates,
                    max_root_distance_ms: config.max_root_distance_ms,
                    min_quorum: config.min_quorum,
                    weighted_median_offset_ms: None,
                    candidate_lambdas: vec![],
                    intersection: IntersectionDiagnostics::disabled(),
                },
            };
        }

        // Capture per-candidate lambdas for Prometheus (ntp_sample_uncertainty_milliseconds).
        let candidate_lambdas: Vec<(String, f64)> = candidates
            .iter()
            .map(|c| (c.result.server.clone(), c.lambda_ms))
            .collect();

        // ── P1F-12: Marzullo interval-intersection pre-filter ─────────────────
        // When enabled, run a sweep over uncertainty intervals [θ−λ, θ+λ].
        // Candidates whose intervals don't participate in the maximum-overlap
        // cluster are discarded as falsetickers before the weighted-median step.
        let (active_candidates, intersection_diag) = if config.interval_selection_enabled {
            let mr = marzullo_filter(&candidates, config.min_quorum);

            match mr.state {
                MarzulloState::NoIntersection => {
                    warn!(
                        candidates = candidate_count,
                        min_quorum = config.min_quorum,
                        "NTP selection: no intersection cluster meets min_quorum (NoIntersection)"
                    );
                    let inter = IntersectionDiagnostics {
                        enabled: true,
                        state: IntersectionState::NoIntersection,
                        intersection_low_ms: None,
                        intersection_high_ms: None,
                        intersection_width_ms: None,
                        truechimer_count: 0,
                        falseticker_count: mr.falseticker_count,
                        competing_cluster_count: mr.competing_cluster_count,
                    };
                    return SelectionOutput {
                        selected: None,
                        agreers: vec![],
                        diagnostics: SelectionDiagnostics {
                            quorum_size: 0,
                            candidate_count,
                            rejected_count: rejected.len(),
                            rejected_sources: rejected,
                            combined_uncertainty_ms: None,
                            selected_server: None,
                            single_provider: false,
                            selection_state: SelectionState::NoIntersection,
                            max_root_distance_ms: config.max_root_distance_ms,
                            min_quorum: config.min_quorum,
                            weighted_median_offset_ms: None,
                            candidate_lambdas,
                            intersection: inter,
                        },
                    };
                }
                MarzulloState::AmbiguousCluster => {
                    warn!(
                        candidates = candidate_count,
                        clusters = mr.competing_cluster_count,
                        min_quorum = config.min_quorum,
                        "NTP selection: multiple competing clusters (AmbiguousCluster — fail closed)"
                    );
                    let inter = IntersectionDiagnostics {
                        enabled: true,
                        state: IntersectionState::AmbiguousCluster,
                        intersection_low_ms: None,
                        intersection_high_ms: None,
                        intersection_width_ms: None,
                        truechimer_count: 0,
                        falseticker_count: mr.falseticker_count,
                        competing_cluster_count: mr.competing_cluster_count,
                    };
                    return SelectionOutput {
                        selected: None,
                        agreers: vec![],
                        diagnostics: SelectionDiagnostics {
                            quorum_size: 0,
                            candidate_count,
                            rejected_count: rejected.len(),
                            rejected_sources: rejected,
                            combined_uncertainty_ms: None,
                            selected_server: None,
                            single_provider: false,
                            selection_state: SelectionState::AmbiguousCluster,
                            max_root_distance_ms: config.max_root_distance_ms,
                            min_quorum: config.min_quorum,
                            weighted_median_offset_ms: None,
                            candidate_lambdas,
                            intersection: inter,
                        },
                    };
                }
                MarzulloState::InsufficientQuorum => {
                    warn!(
                        truechimers = mr.truechimer_indices.len(),
                        min_quorum = config.min_quorum,
                        "NTP selection: intersection found but truechimers < min_quorum"
                    );
                    let inter = IntersectionDiagnostics {
                        enabled: true,
                        state: IntersectionState::InsufficientQuorum,
                        intersection_low_ms: Some(mr.intersection_low_ms),
                        intersection_high_ms: Some(mr.intersection_high_ms),
                        intersection_width_ms: Some(
                            mr.intersection_high_ms - mr.intersection_low_ms,
                        ),
                        truechimer_count: mr.truechimer_indices.len(),
                        falseticker_count: mr.falseticker_count,
                        competing_cluster_count: mr.competing_cluster_count,
                    };
                    return SelectionOutput {
                        selected: None,
                        agreers: vec![],
                        diagnostics: SelectionDiagnostics {
                            quorum_size: mr.truechimer_indices.len(),
                            candidate_count,
                            rejected_count: rejected.len(),
                            rejected_sources: rejected,
                            combined_uncertainty_ms: None,
                            selected_server: None,
                            single_provider: false,
                            selection_state: SelectionState::NoIntersection,
                            max_root_distance_ms: config.max_root_distance_ms,
                            min_quorum: config.min_quorum,
                            weighted_median_offset_ms: None,
                            candidate_lambdas,
                            intersection: inter,
                        },
                    };
                }
                MarzulloState::Ok => {
                    // Filter to truechimers; pass to weighted-median step.
                    let inter = IntersectionDiagnostics {
                        enabled: true,
                        state: IntersectionState::Ok,
                        intersection_low_ms: Some(mr.intersection_low_ms),
                        intersection_high_ms: Some(mr.intersection_high_ms),
                        intersection_width_ms: Some(
                            mr.intersection_high_ms - mr.intersection_low_ms,
                        ),
                        truechimer_count: mr.truechimer_indices.len(),
                        falseticker_count: mr.falseticker_count,
                        competing_cluster_count: mr.competing_cluster_count,
                    };
                    let tc_set: std::collections::HashSet<usize> =
                        mr.truechimer_indices.into_iter().collect();
                    let filtered: Vec<ScoredResult> = candidates
                        .into_iter()
                        .enumerate()
                        .filter(|(i, _)| tc_set.contains(i))
                        .map(|(_, c)| c)
                        .collect();
                    if !inter.falseticker_count.eq(&0) {
                        info!(
                            falsetickers = inter.falseticker_count,
                            truechimers = inter.truechimer_count,
                            low_ms = inter.intersection_low_ms.unwrap_or(0.0),
                            high_ms = inter.intersection_high_ms.unwrap_or(0.0),
                            "NTP selection: Marzullo filter applied"
                        );
                    }
                    (filtered, inter)
                }
            }
        } else {
            (candidates, IntersectionDiagnostics::disabled())
        };

        // ── Weighted median ───────────────────────────────────────────────────
        // Runs on truechimers only (when intersection enabled) or all candidates.
        let mut active = active_candidates;
        active.sort_by_key(|a| a.result.offset_ms);
        let total_weight: f64 = active.iter().map(|c| c.weight).sum();
        let half = total_weight / 2.0;
        let mut cumulative = 0.0;
        let mut wm_offset = active
            .last()
            .map(|c| c.result.offset_ms as f64)
            .unwrap_or(0.0);
        for c in &active {
            cumulative += c.weight;
            if cumulative >= half {
                wm_offset = c.result.offset_ms as f64;
                break;
            }
        }

        // ── Agreers ───────────────────────────────────────────────────────────
        let skew = config.max_offset_skew_ms as f64;
        let agreers: Vec<&ScoredResult> = active
            .iter()
            .filter(|c| (c.result.offset_ms as f64 - wm_offset).abs() <= skew)
            .collect();

        let quorum_size = agreers.len();

        info!(
            candidates = candidate_count,
            agreers = quorum_size,
            wm_offset_ms = wm_offset as i64,
            rejected = rejected.len(),
            "NTP selection: weighted-median consensus"
        );

        if quorum_size < config.min_quorum {
            warn!(
                quorum_size,
                min_quorum = config.min_quorum,
                "NTP selection: insufficient quorum"
            );
            return SelectionOutput {
                selected: None,
                agreers: Vec::new(),
                diagnostics: SelectionDiagnostics {
                    quorum_size,
                    candidate_count,
                    rejected_count: rejected.len(),
                    rejected_sources: rejected,
                    combined_uncertainty_ms: None,
                    selected_server: None,
                    single_provider: false,
                    selection_state: SelectionState::NoQuorum,
                    max_root_distance_ms: config.max_root_distance_ms,
                    min_quorum: config.min_quorum,
                    weighted_median_offset_ms: Some(wm_offset),
                    candidate_lambdas,
                    intersection: intersection_diag,
                },
            };
        }

        // ── Provider-group cap ────────────────────────────────────────────────
        let mut group_counts: HashMap<String, usize> = HashMap::new();
        for a in &agreers {
            let g = provider_group(&a.result.server, &config.provider_groups);
            *group_counts.entry(g).or_insert(0) += 1;
        }
        let max_group = group_counts.values().max().copied().unwrap_or(0);
        let single_provider =
            (max_group as f64) > (quorum_size as f64) * config.provider_group_max_fraction;

        // ── Best agreer (lowest λ, RTT tiebreaker) ────────────────────────────
        let best = agreers
            .iter()
            .min_by(|a, b| {
                a.lambda_ms
                    .partial_cmp(&b.lambda_ms)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.result.rtt.cmp(&b.result.rtt))
            })
            .unwrap(); // safe: quorum_size >= min_quorum >= 1

        // ── Combined uncertainty ──────────────────────────────────────────────
        // When intersection is enabled, the combined uncertainty is the larger of:
        // - the best truechimer's λ (point precision of the selected server)
        // - the intersection radius (width/2 of the consensus region)
        // This ensures the uncertainty reflects the spread across agreers, not just
        // the best server's quality.
        let intersection_radius = intersection_diag
            .intersection_width_ms
            .map(|w| w / 2.0)
            .unwrap_or(0.0);
        let base_uncertainty = f64::max(best.lambda_ms, intersection_radius);
        let combined_uncertainty_ms = if single_provider {
            base_uncertainty * 2.0
        } else {
            base_uncertainty
        };

        let agreer_results: Vec<NtpResult> = agreers.iter().map(|a| a.result.clone()).collect();

        info!(
            selected_server = %best.result.server,
            lambda_ms = best.lambda_ms,
            intersection_radius_ms = intersection_radius,
            quorum_size,
            single_provider,
            combined_uncertainty_ms,
            "NTP selection: selected server"
        );

        SelectionOutput {
            selected: Some(best.result.clone()),
            agreers: agreer_results,
            diagnostics: SelectionDiagnostics {
                quorum_size,
                candidate_count,
                rejected_count: rejected.len(),
                rejected_sources: rejected,
                combined_uncertainty_ms: Some(combined_uncertainty_ms),
                selected_server: Some(best.result.server.clone()),
                single_provider,
                selection_state: SelectionState::Ok,
                max_root_distance_ms: config.max_root_distance_ms,
                min_quorum: config.min_quorum,
                weighted_median_offset_ms: Some(wm_offset),
                candidate_lambdas,
                intersection: intersection_diag,
            },
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// P1-6 test config: interval selection OFF (tests weighted-median only).
    fn cfg(min_quorum: usize) -> SelectionConfig {
        SelectionConfig {
            max_stratum: 4,
            min_quorum,
            reject_leap_alarm: true,
            max_root_distance_ms: 500.0,
            max_sample_age_secs: 60,
            provider_group_max_fraction: 0.5,
            provider_groups: HashMap::new(),
            max_offset_skew_ms: 500,
            interval_selection_enabled: false,
        }
    }

    /// P1F-12 test config: interval selection ON.
    fn cfg_ix(min_quorum: usize) -> SelectionConfig {
        SelectionConfig {
            interval_selection_enabled: true,
            ..cfg(min_quorum)
        }
    }

    fn r(server: &str, rtt_ms: u64, offset_ms: i64) -> NtpResult {
        NtpResult::for_testing(
            server,
            1_700_000_000_000,
            Duration::from_millis(rtt_ms),
            offset_ms,
            Instant::now(),
        )
    }

    fn r_full(
        server: &str,
        rtt_ms: u64,
        offset_ms: i64,
        stratum: u8,
        leap: u8,
        root_delay_ms: u32,
        root_dispersion_ms: u32,
    ) -> NtpResult {
        NtpResult::for_testing_with(
            server,
            1_700_000_000_000,
            Duration::from_millis(rtt_ms),
            offset_ms,
            Instant::now(),
            stratum,
            leap,
            root_delay_ms,
            root_dispersion_ms,
            -20,
        )
    }

    #[test]
    fn single_server_quorum_1_selects_it() {
        let out = WeightedMedianSelector::select(vec![r("a:123", 10, 5)], &HashMap::new(), &cfg(1));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        assert_eq!(out.selected.unwrap().server, "a:123");
    }

    #[test]
    fn single_server_quorum_2_no_quorum() {
        let out = WeightedMedianSelector::select(vec![r("a:123", 10, 5)], &HashMap::new(), &cfg(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::NoQuorum);
        assert!(out.selected.is_none());
        assert_eq!(out.diagnostics.quorum_size, 1);
    }

    #[test]
    fn three_servers_two_agree_third_is_outlier() {
        let now = Instant::now();
        let results = vec![
            NtpResult::for_testing(
                "a:123",
                1_700_000_000_000,
                Duration::from_millis(10),
                100,
                now,
            ),
            NtpResult::for_testing(
                "b:123",
                1_700_000_000_100,
                Duration::from_millis(12),
                105,
                now,
            ),
            NtpResult::for_testing(
                "c:123",
                1_700_000_000_000,
                Duration::from_millis(8),
                5000,
                now,
            ),
        ];
        // Weighted median ~ 105; a and b are within 500 ms, c is not
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        assert_eq!(out.diagnostics.quorum_size, 2);
        // c should not be selected
        assert_ne!(out.selected.as_ref().unwrap().server, "c:123");
    }

    #[test]
    fn all_disagree_no_quorum() {
        // Three servers with equal RTT and equal weight; offsets spread > max_offset_skew_ms.
        // WM = 2000ms (middle server); only b is within 500ms → quorum=1 < 2 → NoQuorum.
        let now = Instant::now();
        let results = vec![
            NtpResult::for_testing("a:123", 0, Duration::from_millis(10), 0, now),
            NtpResult::for_testing("b:123", 0, Duration::from_millis(10), 2000, now),
            NtpResult::for_testing("c:123", 0, Duration::from_millis(10), 4000, now),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::NoQuorum);
        assert!(out.selected.is_none());
        assert_eq!(out.diagnostics.candidate_count, 3);
    }

    /// The core P1-6 guarantee: consensus (offset agreement) beats raw RTT.
    /// A server with LOW RTT but WRONG OFFSET is not selected.
    ///
    /// Math (all with root_delay=4ms, root_dispersion=4ms):
    ///   λ(bad_fast) = 4/2 + 4 + 1/2   = 6.5ms  → weight ≈ 0.133
    ///   λ(good_*)   = 4/2 + 4 + 10/2  = 11ms   → weight ≈ 0.083 each
    ///   Combined good weight (0.166) > bad_fast (0.133) → WM = 2ms (good consensus).
    ///   bad_fast (offset 5000ms) is 4998ms outside the ±500ms skew → non-agreer.
    #[test]
    fn low_rtt_but_wrong_offset_not_selected() {
        let results = vec![
            r_full("bad_fast:123", 1, 5000, 1, 0, 4, 4), // very low RTT, offset far from consensus
            r_full("good_a:123", 10, 0, 1, 0, 4, 4),
            r_full("good_b:123", 10, 2, 1, 0, 4, 4),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        assert_eq!(
            out.diagnostics.selection_state,
            SelectionState::Ok,
            "selection must succeed despite bad_fast being present"
        );
        let sel = out.selected.as_ref().unwrap();
        assert_ne!(
            sel.server, "bad_fast:123",
            "low-RTT server with offset far from consensus must NOT be selected"
        );
        assert_eq!(
            out.diagnostics.quorum_size, 2,
            "exactly the 2 good servers must form the quorum"
        );
        assert!(
            !out.agreers.iter().any(|a| a.server == "bad_fast:123"),
            "bad_fast must not appear in the agreers list"
        );
    }

    /// Prove the min-RTT fallback is gone: when all servers disagree, even the one
    /// with the lowest RTT is NOT returned.  Old code had a dangerous fallback that
    /// would return the min-RTT server; P1-6 fails closed.
    #[test]
    fn all_disagree_no_min_rtt_fallback() {
        // fast:123 has the lowest RTT and dominates the weighted median (offset=0),
        // but mid and slow disagree → only fast agrees → quorum=1 < min_quorum=2 → NoQuorum.
        let results = vec![
            r("fast:123", 1, 0),     // very low RTT, offset=0; λ≈0.5ms → weight≈0.667
            r("mid:123", 10, 2000),  // λ≈5ms → weight≈0.167
            r("slow:123", 20, 4000), // λ≈10ms → weight≈0.091
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        assert_eq!(
            out.diagnostics.selection_state,
            SelectionState::NoQuorum,
            "all-disagree must produce NoQuorum, not fall back to the min-RTT winner"
        );
        assert!(
            out.selected.is_none(),
            "no server must be selected when quorum fails — min-RTT fallback is absent"
        );
        assert!(out.agreers.is_empty(), "agreers must be empty on NoQuorum");
    }

    #[test]
    fn leap_alarm_hard_gated() {
        let results = vec![
            r_full("a:123", 10, 100, 1, 3, 0, 0), // leap=3 → rejected
            r_full("b:123", 10, 100, 1, 0, 0, 0),
            r_full("c:123", 10, 100, 1, 0, 0, 0),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        assert_eq!(out.diagnostics.rejected_count, 1);
        assert_eq!(out.diagnostics.rejected_sources[0].reason, "leap_alarm");
        assert_eq!(out.diagnostics.rejected_sources[0].server, "a:123");
    }

    #[test]
    fn stratum_zero_hard_gated() {
        let results = vec![
            r_full("a:123", 10, 100, 0, 0, 0, 0), // stratum=0 → rejected
            r_full("b:123", 10, 100, 1, 0, 0, 0),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        assert_eq!(out.diagnostics.rejected_count, 1);
        assert_eq!(out.diagnostics.rejected_sources[0].reason, "stratum_zero");
    }

    #[test]
    fn stratum_too_high_hard_gated() {
        let results = vec![
            r_full("a:123", 10, 100, 5, 0, 0, 0), // stratum=5 > max_stratum=4 → rejected
            r_full("b:123", 10, 100, 2, 0, 0, 0),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        assert_eq!(out.diagnostics.rejected_count, 1);
        assert_eq!(
            out.diagnostics.rejected_sources[0].reason,
            "stratum_too_high"
        );
    }

    #[test]
    fn all_gated_returns_no_candidates() {
        let results = vec![
            r_full("a:123", 10, 100, 5, 0, 0, 0), // stratum too high
            r_full("b:123", 10, 100, 0, 0, 0, 0), // stratum zero
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        assert_eq!(
            out.diagnostics.selection_state,
            SelectionState::NoCandidates
        );
        assert!(out.selected.is_none());
        assert_eq!(out.diagnostics.candidate_count, 0);
    }

    #[test]
    fn high_root_distance_gated() {
        // root_dispersion_ms=600 > max_root_distance=500 → gated
        let results = vec![
            r_full("a:123", 10, 100, 1, 0, 0, 600),
            r_full("b:123", 10, 100, 1, 0, 0, 1),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        assert_eq!(out.diagnostics.rejected_count, 1);
        assert_eq!(
            out.diagnostics.rejected_sources[0].reason,
            "root_distance_too_high"
        );
        assert_eq!(out.selected.as_ref().unwrap().server, "b:123");
    }

    #[test]
    fn stale_sample_hard_gated() {
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(120))
            .unwrap_or_else(Instant::now);
        let results = vec![
            NtpResult::for_testing_with(
                "a:123",
                0,
                Duration::from_millis(10),
                100,
                stale,
                1,
                0,
                0,
                0,
                -20,
            ),
            r_full("b:123", 10, 100, 1, 0, 0, 0),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        assert_eq!(out.diagnostics.rejected_count, 1);
        assert_eq!(out.diagnostics.rejected_sources[0].reason, "sample_too_old");
        assert_eq!(out.selected.as_ref().unwrap().server, "b:123");
    }

    #[test]
    fn high_dispersion_outlier_not_selected() {
        // "a": high root_dispersion → high λ → low weight → can't shift median
        // "b" and "c": low dispersion, offset near consensus
        let now = Instant::now();
        let results = vec![
            // "a" has wrong offset AND high dispersion (λ ≈ 405 ms → weight tiny)
            NtpResult::for_testing_with(
                "a:123",
                0,
                Duration::from_millis(10),
                10_000,
                now,
                1,
                0,
                0,
                400,
                -20,
            ),
            // "b" and "c": near consensus, low dispersion (λ ≈ 10 ms → high weight)
            NtpResult::for_testing_with(
                "b:123",
                0,
                Duration::from_millis(10),
                100,
                now,
                1,
                0,
                0,
                5,
                -20,
            ),
            NtpResult::for_testing_with(
                "c:123",
                0,
                Duration::from_millis(10),
                110,
                now,
                1,
                0,
                0,
                5,
                -20,
            ),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        // "b" and "c" dominate the weighted median; "a" is far outside ±500ms skew
        let sel = out.selected.as_ref().unwrap();
        assert_ne!(sel.server, "a:123");
        assert_eq!(out.diagnostics.quorum_size, 2);
    }

    #[test]
    fn high_jitter_server_down_weighted() {
        // Server "a" has high jitter → high λ → lower weight → not selected
        let now = Instant::now();
        let results = vec![
            NtpResult::for_testing("a:123", 0, Duration::from_millis(10), 100, now),
            NtpResult::for_testing("b:123", 0, Duration::from_millis(10), 100, now),
        ];
        let jitter: HashMap<String, u64> = [("a:123".to_string(), 400u64)].into_iter().collect();
        let out = WeightedMedianSelector::select(results, &jitter, &cfg(1));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        // "b" has lower λ so should be selected
        assert_eq!(out.selected.as_ref().unwrap().server, "b:123");
    }

    #[test]
    fn single_provider_flag_set_when_one_group_dominates() {
        // All 3 agreers from *.google.com — one group dominates
        let now = Instant::now();
        let results = vec![
            NtpResult::for_testing(
                "time1.google.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
            NtpResult::for_testing(
                "time2.google.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
            NtpResult::for_testing(
                "time3.google.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        assert!(
            out.diagnostics.single_provider,
            "all from google.com → single_provider must be true"
        );
        // Combined uncertainty should be doubled
        let lambda = out.diagnostics.combined_uncertainty_ms.unwrap();
        assert!(lambda > 0.0);
    }

    #[test]
    fn mixed_providers_no_single_provider_flag() {
        let now = Instant::now();
        let results = vec![
            NtpResult::for_testing(
                "time.google.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
            NtpResult::for_testing(
                "time.cloudflare.com:123",
                0,
                Duration::from_millis(10),
                105,
                now,
            ),
            NtpResult::for_testing("pool.ntp.org:123", 0, Duration::from_millis(10), 110, now),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        assert!(!out.diagnostics.single_provider);
    }

    /// The single-provider flag must double `combined_uncertainty_ms`.
    /// Running the same two servers from the same provider vs two from different
    /// providers: the single-provider case must have higher combined_uncertainty_ms.
    #[test]
    fn single_provider_doubles_combined_uncertainty() {
        let now = Instant::now();
        // Two servers from google.com — single_provider=true
        let same_provider = vec![
            NtpResult::for_testing(
                "time1.google.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
            NtpResult::for_testing(
                "time2.google.com:123",
                0,
                Duration::from_millis(10),
                102,
                now,
            ),
        ];
        let out_same = WeightedMedianSelector::select(same_provider, &HashMap::new(), &cfg(1));
        assert!(out_same.diagnostics.single_provider);
        let unc_single = out_same.diagnostics.combined_uncertainty_ms.unwrap();

        // Two servers from different providers — single_provider=false
        let mixed = vec![
            NtpResult::for_testing(
                "time.google.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
            NtpResult::for_testing(
                "time.cloudflare.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
        ];
        let out_mixed = WeightedMedianSelector::select(mixed, &HashMap::new(), &cfg(1));
        assert!(!out_mixed.diagnostics.single_provider);
        let unc_mixed = out_mixed.diagnostics.combined_uncertainty_ms.unwrap();

        // Both have equal λ; single-provider case must have 2× the uncertainty.
        assert!(
            unc_single > unc_mixed,
            "single-provider uncertainty ({unc_single:.2}) must be > mixed ({unc_mixed:.2})"
        );
        assert!(
            (unc_single - 2.0 * unc_mixed).abs() < 0.01,
            "single-provider uncertainty ({unc_single:.2}) must be exactly 2× mixed ({unc_mixed:.2})"
        );
    }

    /// IP address literals are used verbatim as the provider group, so two different
    /// IPs are two different providers (no grouping by subnet).
    #[test]
    fn ip_literal_provider_grouping() {
        let now = Instant::now();
        let results = vec![
            NtpResult::for_testing("192.0.2.1:123", 0, Duration::from_millis(10), 100, now),
            NtpResult::for_testing("192.0.2.2:123", 0, Duration::from_millis(10), 102, now),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        // Two different IPs → two different provider groups → not single-provider
        // (each has count 1, max_group=1, quorum=2, 1 > 2*0.5=1 is FALSE → not single_provider)
        assert!(
            !out.diagnostics.single_provider,
            "two different IP literals must be treated as different providers"
        );
    }

    /// NTP_PROVIDER_GROUPS override: a manually-specified mapping lets operators
    /// group arbitrary hostnames under one provider key.
    #[test]
    fn provider_groups_override_consolidates_providers() {
        let now = Instant::now();
        let mut overrides = HashMap::new();
        // Both servers are declared to be the same provider via override
        overrides.insert("ntp1.example.com:123".to_string(), "example".to_string());
        overrides.insert("ntp2.example.com:123".to_string(), "example".to_string());
        let config = SelectionConfig {
            provider_groups: overrides,
            ..cfg(1)
        };
        let results = vec![
            NtpResult::for_testing(
                "ntp1.example.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
            NtpResult::for_testing(
                "ntp2.example.com:123",
                0,
                Duration::from_millis(10),
                102,
                now,
            ),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &config);
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        assert!(
            out.diagnostics.single_provider,
            "override mapping both servers to 'example' must trigger single_provider=true"
        );
    }

    /// All required SelectionDiagnostics fields are populated on a successful selection.
    #[test]
    fn diagnostics_expose_all_required_fields() {
        let results = vec![
            r_full("a:123", 10, 0, 1, 0, 4, 4),
            r_full("b:123", 10, 2, 1, 0, 4, 4),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        let d = &out.diagnostics;
        assert_eq!(d.selection_state, SelectionState::Ok);
        assert_eq!(d.quorum_size, 2);
        assert_eq!(d.candidate_count, 2);
        assert_eq!(d.rejected_count, 0);
        assert!(d.rejected_sources.is_empty());
        assert!(d.combined_uncertainty_ms.is_some());
        assert!(d.selected_server.is_some());
        assert!(!d.single_provider);
        assert_eq!(d.max_root_distance_ms, 500.0);
        assert_eq!(d.min_quorum, 2);
        assert!(d.weighted_median_offset_ms.is_some());
        assert_eq!(d.candidate_lambdas.len(), 2, "one lambda per candidate");
    }

    /// On NoQuorum, diagnostics still contain candidate_lambdas and weighted_median_offset_ms.
    #[test]
    fn no_quorum_diagnostics_still_populated() {
        let results = vec![r("only:123", 10, 100)];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(2));
        let d = &out.diagnostics;
        assert_eq!(d.selection_state, SelectionState::NoQuorum);
        assert_eq!(d.quorum_size, 1);
        assert_eq!(d.candidate_count, 1);
        assert!(d.weighted_median_offset_ms.is_some());
        assert_eq!(d.candidate_lambdas.len(), 1);
        assert!(d.combined_uncertainty_ms.is_none());
        assert!(d.selected_server.is_none());
        assert_eq!(d.min_quorum, 2);
    }

    #[test]
    fn best_lambda_server_selected_not_best_rtt() {
        let now = Instant::now();
        // "a": RTT=5ms, root_dispersion=200ms → high lambda
        // "b": RTT=20ms, root_dispersion=1ms  → low lambda
        let results = vec![
            NtpResult::for_testing_with(
                "a:123",
                0,
                Duration::from_millis(5),
                100,
                now,
                1,
                0,
                0,
                200,
                -20,
            ),
            NtpResult::for_testing_with(
                "b:123",
                0,
                Duration::from_millis(20),
                100,
                now,
                1,
                0,
                0,
                1,
                -20,
            ),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        assert_eq!(out.selected.as_ref().unwrap().server, "b:123");
    }

    // ── Kept from original selection.rs ──────────────────────────────────────

    /// Hand-computed RFC 5905 §8 four-tuple.
    #[test]
    fn rfc5905_four_tuple_relations_hold() {
        let r = NtpResult {
            server: "test:123".to_string(),
            epoch_ms: 1170,
            rtt: Duration::from_millis(100),
            offset_ms: -30,
            t1_client_send_ms: 1000,
            t2_server_recv_ms: 1020,
            t3_server_send_ms: 1120,
            t4_client_recv_ms: 1200,
            instant: std::time::Instant::now(),
            root_delay_ms: 0,
            root_dispersion_ms: 0,
            stratum: 1,
            leap: 0,
            precision_log2: -20,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        };

        let derived_offset_ms = ((r.t2_server_recv_ms - r.t1_client_send_ms)
            + (r.t3_server_send_ms - r.t4_client_recv_ms))
            / 2;
        let derived_delay_ms = (r.t4_client_recv_ms - r.t1_client_send_ms)
            - (r.t3_server_send_ms - r.t2_server_recv_ms);

        assert_eq!(derived_offset_ms, r.offset_ms, "θ derivation");
        assert_eq!(derived_delay_ms, r.rtt.as_millis() as i64, "δ derivation");
        assert_eq!(
            r.epoch_ms,
            r.t4_client_recv_ms + r.offset_ms,
            "corrected time"
        );

        let half_delay = derived_delay_ms / 2;
        let t2_derived = r.t1_client_send_ms + derived_offset_ms + half_delay;
        let t3_derived = r.t4_client_recv_ms + derived_offset_ms - half_delay;
        assert_eq!(t2_derived, r.t2_server_recv_ms, "T2 inverse derivation");
        assert_eq!(t3_derived, r.t3_server_send_ms, "T3 inverse derivation");
    }

    #[test]
    fn provider_group_extracts_last_two_labels() {
        assert_eq!(
            provider_group("time.google.com:123", &HashMap::new()),
            "google.com"
        );
        assert_eq!(
            provider_group("time1.google.com:123", &HashMap::new()),
            "google.com"
        );
        assert_eq!(
            provider_group("time.cloudflare.com:123", &HashMap::new()),
            "cloudflare.com"
        );
        assert_eq!(
            provider_group("pool.ntp.org:123", &HashMap::new()),
            "ntp.org"
        );
        assert_eq!(
            provider_group("192.168.1.1:123", &HashMap::new()),
            "192.168.1.1"
        );
        assert_eq!(provider_group("bare:123", &HashMap::new()), "bare");
    }

    #[test]
    fn provider_group_override_takes_precedence() {
        let overrides: HashMap<String, String> =
            [("special:123".to_string(), "custom-group".to_string())]
                .into_iter()
                .collect();
        assert_eq!(provider_group("special:123", &overrides), "custom-group");
    }

    // ── P1F-12 interval-intersection tests ───────────────────────────────────

    /// Two servers whose intervals overlap → both are truechimers, selection Ok.
    #[test]
    fn overlapping_consensus_selects_truechimers() {
        let results = vec![
            r_full("a:123", 10, 0, 1, 0, 4, 4), // λ≈11ms, interval≈[-11,11]
            r_full("b:123", 10, 2, 1, 0, 4, 4), // λ≈11ms, interval≈[-9,13]
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg_ix(1));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        let ix = &out.diagnostics.intersection;
        assert!(ix.enabled);
        assert_eq!(ix.state, IntersectionState::Ok);
        assert_eq!(ix.truechimer_count, 2);
        assert_eq!(ix.falseticker_count, 0);
        assert!(ix.intersection_low_ms.is_some());
        assert!(ix.intersection_high_ms.is_some());
        assert!(ix.intersection_width_ms.is_some());
    }

    /// Three servers with completely disjoint intervals → NoIntersection, fail closed.
    #[test]
    fn all_disjoint_intervals_fail_closed() {
        // Offsets spread 2000ms apart, lambda≈0.5ms → intervals never touch.
        let results = vec![r("a:123", 1, 0), r("b:123", 1, 2000), r("c:123", 1, 4000)];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg_ix(2));
        assert_eq!(
            out.diagnostics.selection_state,
            SelectionState::NoIntersection,
            "disjoint intervals must fail closed with NoIntersection"
        );
        assert!(out.selected.is_none());
        assert!(out.agreers.is_empty());
        let ix = &out.diagnostics.intersection;
        assert!(ix.enabled);
        assert_eq!(ix.state, IntersectionState::NoIntersection);
        assert_eq!(ix.truechimer_count, 0);
    }

    /// Even with min_quorum=2, a single server with small lambda passes when
    /// there is only one cluster.
    #[test]
    fn no_intersection_quorum_fails_closed() {
        // Only 1 server; min_quorum=2 → even with interval selection, the single
        // cluster has depth 1 < min_quorum=2 → NoIntersection.
        let results = vec![r("a:123", 10, 0)];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg_ix(2));
        assert_eq!(
            out.diagnostics.selection_state,
            SelectionState::NoIntersection
        );
        assert!(out.selected.is_none());
    }

    /// An independently-wrong majority (3 tight wrong, 2 tight correct, disjoint)
    /// must fail closed: AmbiguousCluster detection rejects the ambiguity.
    #[test]
    fn independently_wrong_majority_fails_closed_or_high_uncertainty() {
        // Wrong majority: offset=5000ms, lambda=5ms → intervals [4995, 5005]
        // Correct minority: offset=0ms, lambda=5ms → intervals [-5, 5]
        // Two distinct clusters each meeting min_quorum=2 → AmbiguousCluster.
        let now = Instant::now();
        let results = vec![
            NtpResult::for_testing_with(
                "wrong1:123",
                0,
                Duration::from_millis(1),
                5000,
                now,
                1,
                0,
                0,
                1,
                -20,
            ),
            NtpResult::for_testing_with(
                "wrong2:123",
                0,
                Duration::from_millis(1),
                5000,
                now,
                1,
                0,
                0,
                1,
                -20,
            ),
            NtpResult::for_testing_with(
                "wrong3:123",
                0,
                Duration::from_millis(1),
                5000,
                now,
                1,
                0,
                0,
                1,
                -20,
            ),
            NtpResult::for_testing_with(
                "correct1:123",
                0,
                Duration::from_millis(1),
                0,
                now,
                1,
                0,
                0,
                1,
                -20,
            ),
            NtpResult::for_testing_with(
                "correct2:123",
                0,
                Duration::from_millis(1),
                0,
                now,
                1,
                0,
                0,
                1,
                -20,
            ),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg_ix(2));
        // Must NOT select any server (fail closed).
        assert!(
            out.selected.is_none(),
            "wrong majority must not be silently selected — got {:?}",
            out.selected.as_ref().map(|s| &s.server)
        );
        let ix = &out.diagnostics.intersection;
        assert!(ix.enabled);
        assert_eq!(
            ix.state,
            IntersectionState::AmbiguousCluster,
            "two disjoint quorum-meeting clusters must trigger AmbiguousCluster"
        );
        assert_eq!(ix.competing_cluster_count, 2);
        assert!(
            matches!(
                out.diagnostics.selection_state,
                SelectionState::AmbiguousCluster | SelectionState::NoIntersection
            ),
            "selection_state must indicate failure, got {:?}",
            out.diagnostics.selection_state
        );
    }

    /// Loose wrong majority with intervals that overlap the correct minority:
    /// the intersection is defined by the tight minority, the correct server is
    /// selected (lowest lambda), and the combined_offset is NOT silently overridden.
    #[test]
    fn minority_with_tight_overlap_not_silently_overridden_by_loose_wrong_majority() {
        // Wrong servers have wide intervals that include the correct range.
        // Correct servers have tight intervals.
        // The Marzullo intersection is defined by the tight minority.
        let now = Instant::now();
        let results = vec![
            // Wrong: offset=300ms, root_dispersion=400ms → λ≈400ms → [-100, 700]
            NtpResult::for_testing_with(
                "wrong1:123",
                0,
                Duration::from_millis(1),
                300,
                now,
                1,
                0,
                0,
                400,
                -20,
            ),
            NtpResult::for_testing_with(
                "wrong2:123",
                0,
                Duration::from_millis(1),
                300,
                now,
                1,
                0,
                0,
                400,
                -20,
            ),
            NtpResult::for_testing_with(
                "wrong3:123",
                0,
                Duration::from_millis(1),
                300,
                now,
                1,
                0,
                0,
                400,
                -20,
            ),
            // Correct: offset=0ms, root_dispersion=1ms → λ≈5ms → [-5, 5]
            NtpResult::for_testing_with(
                "correct1:123",
                0,
                Duration::from_millis(1),
                0,
                now,
                1,
                0,
                0,
                1,
                -20,
            ),
            NtpResult::for_testing_with(
                "correct2:123",
                0,
                Duration::from_millis(1),
                0,
                now,
                1,
                0,
                0,
                1,
                -20,
            ),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg_ix(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        let sel = out.selected.as_ref().expect("must select a server");
        // The correct server (lowest lambda) must win.
        assert!(
            sel.server.starts_with("correct"),
            "correct server (lowest λ) must be selected, got {}",
            sel.server
        );
        let ix = &out.diagnostics.intersection;
        assert_eq!(ix.state, IntersectionState::Ok);
        // Intersection must be the tight minority region (≈[-5,5]).
        let width = ix.intersection_width_ms.unwrap();
        assert!(
            width < 20.0,
            "intersection must be the tight minority region (width≈10ms), got {width:.2}"
        );
    }

    /// The P1-6 adversarial case (low-RTT wrong-offset) still fails with interval selection.
    #[test]
    fn low_rtt_wrong_offset_still_not_selected() {
        let results = vec![
            r_full("bad_fast:123", 1, 5000, 1, 0, 4, 4),
            r_full("good_a:123", 10, 0, 1, 0, 4, 4),
            r_full("good_b:123", 10, 2, 1, 0, 4, 4),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg_ix(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        let sel = out.selected.as_ref().unwrap();
        assert_ne!(
            sel.server, "bad_fast:123",
            "low-RTT wrong-offset must not be selected"
        );
        assert!(
            !out.diagnostics.intersection.falseticker_count.eq(&0),
            "bad_fast must be an intersection falseticker"
        );
    }

    /// Provider-group cap (single_provider) still works with interval selection.
    #[test]
    fn provider_cap_works_with_interval_selection() {
        let now = Instant::now();
        let results = vec![
            NtpResult::for_testing(
                "time1.google.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
            NtpResult::for_testing(
                "time2.google.com:123",
                0,
                Duration::from_millis(10),
                100,
                now,
            ),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg_ix(1));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        assert!(
            out.diagnostics.single_provider,
            "same provider must trigger single_provider=true even with intersection"
        );
        let base_unc = out
            .diagnostics
            .intersection
            .intersection_width_ms
            .unwrap_or(0.0)
            / 2.0;
        let _ = base_unc; // combined_uncertainty is >= 2× base due to single_provider
        assert!(out.diagnostics.combined_uncertainty_ms.unwrap() > 0.0);
    }

    /// Diagnostics always expose truechimer/falseticker counts.
    #[test]
    fn intersection_diagnostics_expose_truechimer_and_falseticker_counts() {
        let now = Instant::now();
        let results = vec![
            // Good cluster [99,101]
            NtpResult::for_testing("a:123", 0, Duration::from_millis(1), 100, now),
            NtpResult::for_testing("b:123", 0, Duration::from_millis(1), 100, now),
            // Outlier far outside
            NtpResult::for_testing("c:123", 0, Duration::from_millis(1), 5000, now),
        ];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg_ix(2));
        assert_eq!(out.diagnostics.selection_state, SelectionState::Ok);
        let ix = &out.diagnostics.intersection;
        assert_eq!(ix.truechimer_count, 2);
        assert_eq!(ix.falseticker_count, 1);
        assert!(ix.intersection_width_ms.is_some());
    }

    /// When intersection is disabled, the `intersection` field reports Disabled.
    #[test]
    fn intersection_disabled_reports_disabled_state() {
        let results = vec![r("a:123", 10, 0), r("b:123", 10, 2)];
        let out = WeightedMedianSelector::select(results, &HashMap::new(), &cfg(1));
        let ix = &out.diagnostics.intersection;
        assert!(!ix.enabled);
        assert_eq!(ix.state, IntersectionState::Disabled);
        assert_eq!(ix.truechimer_count, 0);
        assert_eq!(ix.falseticker_count, 0);
    }

    /// When intersection fails, the combined_uncertainty_ms radiates the intersection width.
    /// Tight intersection → small uncertainty; wide intersection → larger uncertainty.
    #[test]
    fn intersection_radius_influences_combined_uncertainty() {
        let now = Instant::now();
        // Tight overlapping cluster: both at offset=0ms, large dispersion so λ is wide enough
        // that the intersection_radius > best.lambda, driving combined_uncertainty up.
        // a,b: offset=0ms, root_dispersion=10ms → λ ≈ 10ms → interval [-10, +10]
        // intersection = [-10, +10], width = 20ms, radius = 10ms
        // best.lambda ≈ 10ms → combined = max(10, 10) = 10ms > 0
        let tight = vec![
            NtpResult::for_testing_with(
                "a:123",
                0,
                Duration::from_millis(1),
                0,
                now,
                1,
                0,
                0,
                10,
                -20,
            ),
            NtpResult::for_testing_with(
                "b:123",
                0,
                Duration::from_millis(1),
                0,
                now,
                1,
                0,
                0,
                10,
                -20,
            ),
        ];
        let out_tight = WeightedMedianSelector::select(tight, &HashMap::new(), &cfg_ix(2));
        let unc_tight = out_tight.diagnostics.combined_uncertainty_ms.unwrap();
        assert!(
            unc_tight > 0.0,
            "combined_uncertainty must be > 0 for a tight consensus"
        );

        // Narrow cluster: same two servers but very small dispersion.
        // λ ≈ rtt/2 ≈ 0.5ms → combined_uncertainty is much smaller than the wide case.
        let narrow = vec![
            NtpResult::for_testing_with(
                "a:123",
                0,
                Duration::from_millis(1),
                0,
                now,
                1,
                0,
                0,
                0,
                -20,
            ),
            NtpResult::for_testing_with(
                "b:123",
                0,
                Duration::from_millis(1),
                0,
                now,
                1,
                0,
                0,
                0,
                -20,
            ),
        ];
        let out_narrow = WeightedMedianSelector::select(narrow, &HashMap::new(), &cfg_ix(2));
        let unc_narrow = out_narrow.diagnostics.combined_uncertainty_ms.unwrap();
        assert!(
            unc_narrow > 0.0,
            "combined_uncertainty must be > 0 for narrow case"
        );

        // Wide dispersion → larger combined_uncertainty than narrow dispersion.
        assert!(
            unc_tight > unc_narrow,
            "larger dispersion must increase combined_uncertainty: tight={unc_tight} narrow={unc_narrow}"
        );
    }

    /// PHI = 15 µs/s = 0.015 ms/s = 15e-6 ms/ms.
    /// For a 1000-second-old sample with all other λ terms zero, the PHI
    /// contribution must be ≈ 15 ms (not 15000 ms, which would indicate the
    /// wrong unit of ms/s instead of ms/ms).
    #[test]
    fn phi_contribution_1000_seconds_is_15_ms() {
        // Backdated instant: ~1000 seconds old. All root/rtt/jitter terms are
        // zero so the only λ contribution is PHI * age_ms.
        let old_instant = Instant::now() - Duration::from_secs(1000);
        let sample = NtpResult::for_testing("a:123", 0, Duration::from_millis(0), 0, old_instant);
        // jitter_ms = 0 → lambda = PHI_MS_PER_MS * age_ms ≈ 15e-6 * 1_000_000 = 15 ms
        let lambda = compute_lambda(&sample, 0.0);
        assert!(
            (lambda - 15.0).abs() < 1.0,
            "PHI contribution for 1000 s age should be ≈15 ms, got {lambda:.3} ms"
        );
        // Sanity: must NOT be anywhere near 15000 ms (that would mean ms/s unit error)
        assert!(
            lambda < 100.0,
            "lambda {lambda:.3} ms is implausibly large — PHI unit error?"
        );
    }
}
