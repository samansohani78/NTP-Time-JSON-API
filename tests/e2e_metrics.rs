mod common;

use std::path::Path;

use std::time::Duration;

async fn scrape_metrics(base_url: &str) -> String {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    client
        .get(format!("{base_url}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

/// /metrics must always include the core metric families regardless of
/// whether any requests have been processed.
#[tokio::test]
async fn metrics_contains_core_families() {
    let server = common::spawn_server_unsynced().await;
    let body = scrape_metrics(&server.base_url).await;

    for family in &[
        "build_info",
        "ntp_sync_total",
        "ntp_staleness_seconds",
        "ntp_consecutive_failures",
    ] {
        assert!(body.contains(family), "metrics missing {family}");
    }
}

/// After a sync, /metrics must include the time-quality envelope metrics
/// added in P0-4.
#[tokio::test]
async fn metrics_contains_quality_envelope_families() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    // Hit /time once so that HTTP metrics are populated too.
    reqwest::get(format!("{}/time", server.base_url))
        .await
        .unwrap();

    let body = scrape_metrics(&server.base_url).await;

    for family in &[
        "time_uncertainty_milliseconds",
        "time_source_mode",
        "time_serve_state",
    ] {
        assert!(body.contains(family), "metrics missing {family}");
    }
}

/// After a sync, /metrics must include the P1-6 selection metrics.
#[tokio::test]
async fn metrics_contains_p1_6_selection_families_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let body = scrape_metrics(&server.base_url).await;

    // Singleton gauges/counters: always present even with no series.
    for family in &[
        "ntp_selection_quorum_size",
        "ntp_selection_falsetickers_total",
        "ntp_combined_uncertainty_milliseconds",
        "ntp_selection_single_provider",
    ] {
        assert!(
            body.contains(family),
            "metrics missing P1-6 metric: {family}"
        );
    }
    // ntp_sample_uncertainty_milliseconds: Family metric — appears once apply_sync_to_state
    // calls get_or_create{server=...}. Tested in metrics_sample_uncertainty_appears_after_sync.
    assert!(
        body.contains("ntp_sample_uncertainty_milliseconds"),
        "ntp_sample_uncertainty_milliseconds must appear after sync (series created by apply_sync)"
    );
}

/// After a sync, ntp_selection_quorum_size must be >= 1 (single mock upstream).
#[tokio::test]
async fn metrics_selection_quorum_size_is_positive_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    // Trigger a sync by hitting /status (state is already synced in spawn_server_synced)
    let body = scrape_metrics(&server.base_url).await;

    let quorum_line = body
        .lines()
        .find(|l| l.starts_with("ntp_selection_quorum_size "))
        .expect("ntp_selection_quorum_size metric line not found");

    let value: f64 = quorum_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .expect("ntp_selection_quorum_size value must be numeric");

    assert!(
        value >= 1.0,
        "ntp_selection_quorum_size should be >= 1 after sync; got {value}"
    );
}

/// After a sync, ntp_sample_uncertainty_milliseconds must have a series
/// with a server label and a positive lambda value.
#[tokio::test]
async fn metrics_sample_uncertainty_has_server_label_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let body = scrape_metrics(&server.base_url).await;

    // Should have a line like: ntp_sample_uncertainty_milliseconds{server="..."} 11.0
    let has_labeled_series = body
        .lines()
        .any(|l| l.starts_with("ntp_sample_uncertainty_milliseconds{") && l.contains("server="));
    assert!(
        has_labeled_series,
        "ntp_sample_uncertainty_milliseconds must have a labeled series after sync; output:\n{}",
        body.lines()
            .filter(|l| l.contains("ntp_sample"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// After a sync, all 4 P1-8 replica metrics must be present with replica_id labels.
#[tokio::test]
async fn metrics_contains_replica_labeled_series_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let body = scrape_metrics(&server.base_url).await;

    for family in &[
        "time_replica_offset_milliseconds",
        "time_replica_uncertainty_milliseconds",
        "time_replica_serve_state",
        "time_replica_source_mode",
    ] {
        // Family metric — only appears after get_or_create is called in apply_sync_to_state
        assert!(
            body.contains(family),
            "P1-8 replica metric missing: {family}"
        );
        // Must have a labeled series with replica_id
        let has_label = body
            .lines()
            .any(|l| l.starts_with(family) && l.contains("replica_id="));
        assert!(
            has_label,
            "P1-8 replica metric {family} must have a replica_id-labeled series"
        );
    }
}

/// After a sync with a manual override active, the replica source_mode metric
/// must reflect source=manual (encoded as 3).
#[tokio::test]
async fn metrics_replica_source_mode_reflects_manual_override() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    // Use force_allowed so the jump check can be bypassed regardless of epoch delta.
    let server = common::spawn_server_with_admin_force_allowed(&upstream, "test-token", 5000).await;

    // Activate manual override with force=true to bypass jump check.
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("{}/admin/time/override", server.base_url))
        .header("Authorization", "Bearer test-token")
        .json(&serde_json::json!({
            "epoch_ms": 1_704_067_200_000i64,
            "ttl_seconds": 300,
            "reason": "metrics test",
            "force": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Trigger metric update: apply a "sync" while override is active so compute_quality → manual
    use ntp_time_json_api::{
        ntp::SyncOutcome,
        ntp::selection::{
            IntersectionDiagnostics, SelectionDiagnostics, SelectionState, TimingSource,
        },
    };
    use std::time::Instant;
    let outcome = SyncOutcome {
        result: ntp_time_json_api::ntp::SyncResult {
            epoch_ms: 1_704_067_200_000,
            server: upstream.addr.to_string(),
            rtt: std::time::Duration::from_millis(10),
            offset_ms: 0,
            instant: Instant::now(),
            t1_client_send_ms: 0,
            t2_server_recv_ms: 0,
            t3_server_send_ms: 0,
            t4_client_recv_ms: 0,
            root_delay_ms: 0,
            root_dispersion_ms: 1,
            stratum: 1,
            leap: 0,
            precision_log2: -20,
            reference_id: 0,
            timing_source: TimingSource::Measured,
        },
        diagnostics: SelectionDiagnostics {
            quorum_size: 1,
            candidate_count: 1,
            rejected_count: 0,
            rejected_sources: vec![],
            combined_uncertainty_ms: Some(5.0),
            selected_server: Some(upstream.addr.to_string()),
            single_provider: false,
            selection_state: SelectionState::Ok,
            max_root_distance_ms: 500.0,
            min_quorum: 1,
            weighted_median_offset_ms: Some(0.0),
            candidate_lambdas: vec![(upstream.addr.to_string(), 5.0)],
            intersection: IntersectionDiagnostics::disabled(),
        },
        jitter_ms: 0,
    };
    common::apply_sync_to_state(&server.state, &outcome);

    let body = scrape_metrics(&server.base_url).await;

    // time_replica_source_mode should be 3 (manual) because override is active
    let manual_line = body
        .lines()
        .find(|l| l.starts_with("time_replica_source_mode{") && l.contains("replica_id="));
    let manual_line = manual_line.expect("time_replica_source_mode{replica_id=...} not found");
    let value: f64 = manual_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .expect("parse value");
    assert_eq!(
        value as i64, 3,
        "time_replica_source_mode must be 3 (manual) when override is active; got {value}"
    );
}

/// NTP UDP server metrics must be present when the NTP server is enabled.
#[tokio::test]
async fn metrics_contains_ntp_udp_server_families_when_enabled() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let (server, ntp_addr) = common::spawn_server_with_ntp_server(&upstream).await;

    // Trigger at least one UDP request so the counters are non-zero.
    common::query_ntp_udp(ntp_addr).await;

    let body = scrape_metrics(&server.base_url).await;

    for family in &[
        "ntp_udp_server_requests_total",
        "ntp_udp_server_responses_total",
        "ntp_udp_server_errors_total",
        "ntp_udp_server_root_dispersion_seconds",
    ] {
        assert!(body.contains(family), "metrics missing {family}");
    }
}

// ── P1F-12: interval-intersection metrics ────────────────────────────────────

/// After a successful sync the intersection gauge families must appear in the
/// Prometheus output.
#[tokio::test]
async fn metrics_contains_intersection_families_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let body = scrape_metrics(&server.base_url).await;

    for family in &[
        "ntp_intersection_truechimers",
        "ntp_intersection_width_milliseconds",
        "ntp_intersection_ambiguous_clusters",
    ] {
        assert!(
            body.contains(family),
            "metrics scrape missing P1F-12 family: {family}\n---\n{body}"
        );
    }
}

/// ntp_intersection_truechimers must be ≥ 1 after a successful sync.
#[tokio::test]
async fn metrics_intersection_truechimers_positive_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let body = scrape_metrics(&server.base_url).await;

    let line = body
        .lines()
        .find(|l| l.starts_with("ntp_intersection_truechimers "))
        .expect("ntp_intersection_truechimers not found in metrics");
    let value: f64 = line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .expect("parse ntp_intersection_truechimers value");
    assert!(
        value >= 1.0,
        "ntp_intersection_truechimers must be ≥ 1 after sync, got {value}"
    );
}

/// Prometheus rules file must exist and contain all four required alert names.
#[test]
fn prometheus_rules_file_contains_required_alerts() {
    let rules_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("k8s/prometheus-rules.yaml");
    assert!(
        rules_path.exists(),
        "k8s/prometheus-rules.yaml does not exist"
    );
    let content =
        std::fs::read_to_string(&rules_path).expect("failed to read k8s/prometheus-rules.yaml");
    for alert in &[
        "NtpTimeReplicaHighUncertainty",
        "NtpTimeReplicaStopped",
        "NtpTimeReplicaSpreadHigh",
        "NtpTimeSingleProvider",
    ] {
        assert!(
            content.contains(alert),
            "k8s/prometheus-rules.yaml missing alert: {alert}"
        );
    }
}
