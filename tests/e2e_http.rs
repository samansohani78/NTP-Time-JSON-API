mod common;

use std::time::Duration;

// ── Helpers ───────────────────────────────────────────────────────────────────

async fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap()
}

// ── /time: pre-sync ───────────────────────────────────────────────────────────

/// Before any NTP sync, /time must return 503 (REQUIRE_SYNC=true default).
#[tokio::test]
async fn time_pre_sync_returns_503() {
    let server = common::spawn_server_unsynced().await;
    let resp = client()
        .await
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], 503);
}

// ── /time: post-sync ──────────────────────────────────────────────────────────

/// After sync, /time returns 200 with the expected backward-compatible body.
#[tokio::test]
async fn time_post_sync_returns_200_with_correct_body() {
    let fixed_epoch: i64 = 1_704_067_200_000; // 2024-01-01T00:00:00Z
    let upstream = common::start_mock_ntp_upstream(fixed_epoch).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    // Backward-compat shape: {message, status, data}
    assert_eq!(body["status"], 200);
    assert!(body["message"].is_string(), "missing 'message' field");
    let epoch_ms = body["data"].as_i64().expect("data must be i64");
    assert!(epoch_ms > 0);
    assert!(
        (epoch_ms - fixed_epoch).abs() < 5_000,
        "epoch {epoch_ms} too far from expected {fixed_epoch}"
    );
}

/// /time response must NOT include quality fields in the body (backward-compat).
#[tokio::test]
async fn time_body_has_no_quality_fields() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["source"].is_null(), "source must NOT be in /time body");
    assert!(
        body["uncertainty_ms"].is_null(),
        "uncertainty_ms must NOT be in /time body"
    );
}

// ── /time: quality headers ────────────────────────────────────────────────────

/// After sync, /time must carry all X-Time-* quality headers.
#[tokio::test]
async fn time_quality_headers_present_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let headers = resp.headers();

    let source = headers
        .get("x-time-source")
        .expect("x-time-source missing")
        .to_str()
        .unwrap();
    assert!(
        ["ntp", "degraded"].contains(&source),
        "unexpected source: {source}"
    );

    let serve_state = headers
        .get("x-time-serve-state")
        .expect("x-time-serve-state missing")
        .to_str()
        .unwrap();
    assert!(
        ["ok", "degraded"].contains(&serve_state),
        "unexpected serve_state: {serve_state}"
    );

    assert!(
        headers.contains_key("x-time-uncertainty-ms"),
        "x-time-uncertainty-ms missing"
    );
    assert!(
        headers.contains_key("x-time-stratum"),
        "x-time-stratum missing"
    );
    assert!(
        headers.contains_key("x-time-staleness-ms"),
        "x-time-staleness-ms missing"
    );
    assert!(
        headers.contains_key("x-time-selected-server"),
        "x-time-selected-server missing"
    );
}

// ── GET / (alias) ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn root_alias_returns_same_as_time() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

// ── /time/full ────────────────────────────────────────────────────────────────

/// /time/full returns an enriched body with quality fields.
#[tokio::test]
async fn time_full_returns_enriched_body() {
    let fixed_epoch: i64 = 1_704_067_200_000;
    let upstream = common::start_mock_ntp_upstream(fixed_epoch).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time/full", server.base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(body["status"], 200);
    assert!(body["data"].as_i64().unwrap_or(0) > 0);
    // Quality fields must be present in /time/full body
    assert!(body["source"].is_string(), "source missing from /time/full");
    assert!(
        body["serve_state"].is_string(),
        "serve_state missing from /time/full"
    );
    assert!(
        body["uncertainty_ms"].is_number(),
        "uncertainty_ms missing from /time/full"
    );
    assert!(
        body["staleness_ms"].is_number(),
        "staleness_ms missing from /time/full"
    );
    assert!(
        body["stratum"].is_number(),
        "stratum missing from /time/full"
    );
}

// ── /status ───────────────────────────────────────────────────────────────────

/// /status always returns 200 regardless of serve state.
#[tokio::test]
async fn status_always_returns_200() {
    // Unsynced server
    let server = common::spawn_server_unsynced().await;
    let resp = client()
        .await
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "/status must be 200 even before sync"
    );
}

/// /status after sync carries the full quality envelope.
#[tokio::test]
async fn status_returns_quality_envelope_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["source"].is_string());
    assert!(body["serve_state"].is_string());
    assert!(body["uncertainty_ms"].is_number());
    assert!(body["staleness_ms"].is_number());
    assert!(body["stratum"].is_number());
    assert!(
        body["ntp_synced"].as_bool().unwrap_or(false),
        "ntp_synced must be true"
    );
}

/// /status before sync has source=unsynced and ntp_synced=false.
#[tokio::test]
async fn status_unsynced_reports_unsynced() {
    let server = common::spawn_server_unsynced().await;
    let resp = client()
        .await
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["source"], "unsynced");
    assert_eq!(body["serve_state"], "unsynced");
    assert!(body["uncertainty_ms"].is_null());
    let ntp_synced = body["ntp_synced"].as_bool().unwrap_or(true);
    assert!(!ntp_synced, "ntp_synced should be false before sync");
}

// ── Health probes ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn healthz_always_returns_200() {
    let server = common::spawn_server_unsynced().await;
    let resp = client()
        .await
        .get(format!("{}/healthz", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn readyz_503_before_sync() {
    let server = common::spawn_server_unsynced().await;
    let resp = client()
        .await
        .get(format!("{}/readyz", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
}

#[tokio::test]
async fn readyz_200_after_sync_with_good_uncertainty() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/readyz", server.base_url))
        .send()
        .await
        .unwrap();
    // Our mock NTP returns root_delay=128,root_dispersion=64 (sub-ms values),
    // giving uncertainty << 250 ms threshold → readyz should be 200.
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn startupz_200_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/startupz", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

// ── /performance ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn performance_endpoint_returns_200() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/performance", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["metrics"].is_object(), "missing 'metrics' key");
    assert!(body["metrics"]["requests"].is_object());
    assert!(body["metrics"]["cache"].is_object());
}

// ── /status: P1-6 selection diagnostics ──────────────────────────────────────

/// After a sync, /status must include a `selection` object with ALL required
/// P1-6 diagnostic fields populated by the WeightedMedianSelector.
#[tokio::test]
async fn status_contains_selection_diagnostics_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let sel = &body["selection"];
    assert!(
        sel.is_object(),
        "/status 'selection' field must be an object"
    );

    // Required numeric fields
    assert!(sel["quorum_size"].is_number(), "selection.quorum_size");
    assert!(
        sel["candidate_count"].is_number(),
        "selection.candidate_count"
    );
    assert!(
        sel["rejected_count"].is_number(),
        "selection.rejected_count"
    );
    assert!(
        sel["max_root_distance_ms"].is_number(),
        "selection.max_root_distance_ms"
    );
    assert!(sel["min_quorum"].is_number(), "selection.min_quorum");

    // selection_state must be one of the known values
    let state = sel["selection_state"]
        .as_str()
        .expect("selection_state must be a string");
    assert!(
        ["ok", "no_quorum", "no_candidates"].contains(&state),
        "unexpected selection_state: {state}"
    );

    // Boolean
    assert!(
        sel["single_provider"].is_boolean(),
        "selection.single_provider"
    );

    // Arrays
    assert!(
        sel["rejected_sources"].is_array(),
        "selection.rejected_sources"
    );

    // On a successful sync these must be populated
    if state == "ok" {
        assert!(
            sel["combined_uncertainty_ms"].is_number(),
            "selection.combined_uncertainty_ms must be set on Ok"
        );
        assert!(
            sel["selected_server"].is_string(),
            "selection.selected_server must be set on Ok"
        );
        assert!(
            sel["weighted_median_offset_ms"].is_number(),
            "selection.weighted_median_offset_ms must be set on Ok"
        );
    }
}

/// After a sync, `selection.quorum_size` must equal at least 1 (single
/// upstream in test environment with min_quorum=1).
#[tokio::test]
async fn status_selection_quorum_size_is_positive_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let quorum = body["selection"]["quorum_size"]
        .as_u64()
        .expect("quorum_size must be a non-negative integer");
    assert!(
        quorum >= 1,
        "quorum_size should be >= 1 after a successful sync"
    );
}

/// /time/full must also embed selection diagnostics in its body.
#[tokio::test]
async fn time_full_contains_selection_diagnostics_after_sync() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time/full", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let sel = &body["selection"];
    assert!(
        sel.is_object(),
        "/time/full 'selection' field must be an object; got: {sel}"
    );
    assert!(sel["selection_state"].is_string());
}

// ── P1-8: replica identity fields ────────────────────────────────────────────

/// /status must include replica_id, selected_offset_ms, combined_uncertainty_ms,
/// selected_provider, and selection_state as P1-8 drift-visibility fields.
#[tokio::test]
async fn status_contains_replica_id_and_drift_fields() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();

    // replica_id must be a non-empty string
    let replica_id = body["replica_id"]
        .as_str()
        .expect("replica_id must be a string");
    assert!(!replica_id.is_empty(), "replica_id must not be empty");

    // selected_offset_ms: null before sync, number after sync
    assert!(
        body["selected_offset_ms"].is_number(),
        "selected_offset_ms must be a number after sync"
    );

    // combined_uncertainty_ms
    assert!(
        body["combined_uncertainty_ms"].is_number(),
        "combined_uncertainty_ms must be a number after sync"
    );

    // selected_provider: derived from selected_server
    assert!(
        body["selected_provider"].is_string(),
        "selected_provider must be a string after sync"
    );

    // selection_state: top-level convenience field
    let sel_state = body["selection_state"]
        .as_str()
        .expect("selection_state must be a string");
    assert!(
        ["ok", "no_quorum", "no_candidates"].contains(&sel_state),
        "unexpected selection_state: {sel_state}"
    );
}

/// /time/full must include replica_id.
#[tokio::test]
async fn time_full_contains_replica_id() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time/full", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let replica_id = body["replica_id"]
        .as_str()
        .expect("replica_id missing from /time/full");
    assert!(!replica_id.is_empty(), "replica_id must not be empty");

    // /time body (GET /time) must NOT have replica_id (backward-compat)
}

/// /time (basic) must NOT include replica_id in the body (backward-compat).
#[tokio::test]
async fn time_body_has_no_replica_id() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("replica_id").is_none(),
        "/time body must not contain replica_id"
    );
}

// ── P1F-12: interval-intersection diagnostics ─────────────────────────────────

/// /status must include an `intersection` object with P1F-12 diagnostic fields.
#[tokio::test]
async fn status_contains_intersection_diagnostics() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let ix = body["intersection"]
        .as_object()
        .expect("intersection must be an object in /status");

    // Fields that must be present and have correct types.
    assert!(
        ix["enabled"].is_boolean(),
        "intersection.enabled must be bool"
    );
    assert!(ix["state"].is_string(), "intersection.state must be string");
    assert!(
        ix["truechimer_count"].is_number(),
        "intersection.truechimer_count must be a number"
    );
    assert!(
        ix["falseticker_count"].is_number(),
        "intersection.falseticker_count must be a number"
    );
    assert!(
        ix["competing_cluster_count"].is_number(),
        "intersection.competing_cluster_count must be a number"
    );

    // With a single upstream and default config, intersection must succeed.
    let state = ix["state"].as_str().unwrap();
    assert!(
        ["ok", "disabled"].contains(&state),
        "unexpected intersection.state after sync with single upstream: {state}"
    );

    // When intersection is ok, bounds are present.
    if state == "ok" {
        assert!(
            ix["intersection_low_ms"].is_number(),
            "intersection_low_ms must be present when state=ok"
        );
        assert!(
            ix["intersection_high_ms"].is_number(),
            "intersection_high_ms must be present when state=ok"
        );
        assert!(
            ix["intersection_width_ms"].is_number(),
            "intersection_width_ms must be present when state=ok"
        );
    }
}

/// /time/full must also include an `intersection` object.
#[tokio::test]
async fn time_full_contains_intersection_diagnostics() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time/full", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    let ix = body["intersection"]
        .as_object()
        .expect("intersection must be an object in /time/full");
    assert!(ix["enabled"].is_boolean());
    assert!(ix["state"].is_string());
    assert!(ix["truechimer_count"].is_number());
}

/// /time (basic) must NOT contain an `intersection` field (backward-compat).
#[tokio::test]
async fn time_body_has_no_intersection_field() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("intersection").is_none(),
        "/time body must not contain intersection field"
    );
}

// ── Rate-limiting regression (ConnectInfo) ────────────────────────────────────
// These tests use the production serve path (into_make_service_with_connect_info)
// to verify that PeerIpKeyExtractor can read the client IP and does NOT return
// the "Unable To Extract Key!" 500 error that occurs when ConnectInfo is absent.

/// /time must NOT 500 when rate limiting is enabled (ConnectInfo regression).
#[tokio::test]
async fn rate_limited_time_does_not_500() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced_rate_limited(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert_ne!(
        status, 500,
        "/time must not return 500 with rate limiting enabled"
    );
    assert!(
        status == 200 || status == 503,
        "/time must return 200 or 503, got {status}"
    );
}

/// /time/full must NOT 500 when rate limiting is enabled (ConnectInfo regression).
#[tokio::test]
async fn rate_limited_time_full_does_not_500() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced_rate_limited(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/time/full", server.base_url))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    assert_ne!(
        status, 500,
        "/time/full must not return 500 with rate limiting enabled"
    );
    assert!(
        status == 200 || status == 503,
        "/time/full must return 200 or 503, got {status}"
    );
}

/// /status must NOT 500 when rate limiting is enabled (ConnectInfo regression).
#[tokio::test]
async fn rate_limited_status_does_not_500() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced_rate_limited(&upstream).await;

    let resp = client()
        .await
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "/status must always return 200 with rate limiting enabled"
    );
}
