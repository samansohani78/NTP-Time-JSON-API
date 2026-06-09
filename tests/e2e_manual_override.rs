mod common;
use common::{
    spawn_server_with_admin, spawn_server_with_admin_and_ntp_server,
    spawn_server_with_admin_force_allowed, start_mock_ntp_upstream,
};
use futures_util::StreamExt;
use std::time::Duration;
use tokio_tungstenite::connect_async;

const TOKEN: &str = "test-secret-token-abc123";
const FIXED_EPOCH_MS: i64 = 1_704_067_200_000; // 2024-01-01T00:00:00Z

// ── Helper ────────────────────────────────────────────────────────────────────

async fn post_override(base_url: &str, token: &str, body: &serde_json::Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base_url}/admin/time/override"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .expect("POST /admin/time/override failed")
}

async fn get_override_status(base_url: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(format!("{base_url}/admin/time/override"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("GET /admin/time/override failed")
}

async fn delete_override(base_url: &str, token: &str) -> reqwest::Response {
    reqwest::Client::new()
        .delete(format!("{base_url}/admin/time/override"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("DELETE /admin/time/override failed")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn admin_disabled_returns_404_on_get() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/admin/time/override", server.base_url))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn admin_disabled_returns_404_on_post() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/admin/time/override", server.base_url))
        .json(&serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "test",
            "ttl_seconds": 60
        }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn admin_disabled_returns_404_on_delete() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = common::spawn_server_synced(&upstream).await;

    let resp = reqwest::Client::new()
        .delete(format!("{}/admin/time/override", server.base_url))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn get_override_returns_inactive_when_none_set() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    let resp = get_override_status(&server.base_url, TOKEN).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["active"], false);
}

#[tokio::test]
async fn post_override_missing_token_returns_401() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    let resp = reqwest::Client::new()
        .post(format!("{}/admin/time/override", server.base_url))
        .json(&serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "test",
            "ttl_seconds": 60
        }))
        .send()
        .await
        .expect("request failed");
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], 401);
    assert_eq!(body["error"], "Unauthorized");
}

#[tokio::test]
async fn post_override_wrong_token_returns_401() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    let resp = post_override(
        &server.base_url,
        "wrong-token",
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "test",
            "ttl_seconds": 60
        }),
    )
    .await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn missing_and_wrong_token_return_identical_401_body() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    // Missing token
    let missing = reqwest::Client::new()
        .get(format!("{}/admin/time/override", server.base_url))
        .send()
        .await
        .expect("request failed");
    assert_eq!(missing.status(), 401);
    let missing_body = missing.text().await.unwrap();

    // Wrong token
    let wrong = reqwest::Client::new()
        .get(format!("{}/admin/time/override", server.base_url))
        .header("Authorization", "Bearer completely-wrong")
        .send()
        .await
        .expect("request failed");
    assert_eq!(wrong.status(), 401);
    let wrong_body = wrong.text().await.unwrap();

    assert_eq!(
        missing_body, wrong_body,
        "missing and wrong token must return identical 401 bodies"
    );
}

#[tokio::test]
async fn post_override_empty_reason_returns_400() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    let resp = post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "",
            "ttl_seconds": 60
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "ValidationError");
}

#[tokio::test]
async fn post_override_zero_ttl_returns_400() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    let resp = post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "test",
            "ttl_seconds": 0
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "ValidationError");
}

#[tokio::test]
async fn post_override_ttl_exceeds_max_returns_400() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    // Default max_ttl_secs=300; send 301
    let resp = post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "test",
            "ttl_seconds": 301
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "ValidationError");
}

#[tokio::test]
async fn force_true_rejected_when_allow_force_false() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    // spawn_server_with_admin has allow_force=false (default)
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    let resp = post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "test",
            "ttl_seconds": 60,
            "force": true
        }),
    )
    .await;
    assert_eq!(resp.status(), 400);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "ForceNotAllowed");
}

#[tokio::test]
async fn post_override_jump_too_large_returns_422() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    // max_jump_ms = 500 (very tight)
    let server = spawn_server_with_admin(&upstream, TOKEN, 500).await;

    // Jump by 10 minutes (600_000 ms) — well over 500 ms limit
    let far_future = FIXED_EPOCH_MS + 600_000;
    let resp = post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": far_future,
            "reason": "jump test",
            "ttl_seconds": 60
        }),
    )
    .await;
    assert_eq!(resp.status(), 422);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "JumpTooLarge");
}

#[tokio::test]
async fn post_override_success_returns_override_info() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    let resp = post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "maintenance test",
            "ttl_seconds": 60,
            "operator": "alice"
        }),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], 200);
    let ov = &body["override"];
    assert_eq!(ov["epoch_ms"], FIXED_EPOCH_MS);
    assert_eq!(ov["reason"], "maintenance test");
    assert_eq!(ov["operator"], "alice");
    assert_eq!(ov["ttl_remaining_secs"], 60);
    assert!(ov["set_at_ms"].as_i64().unwrap_or(0) > 0);
    assert!(ov["expires_at_ms"].as_i64().unwrap_or(0) > ov["set_at_ms"].as_i64().unwrap_or(0));
}

#[tokio::test]
async fn get_override_after_set_shows_active() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "test active",
            "ttl_seconds": 60
        }),
    )
    .await;

    let resp = get_override_status(&server.base_url, TOKEN).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["active"], true);
    let ov = &body["override"];
    assert_eq!(ov["epoch_ms"], FIXED_EPOCH_MS);
    assert_eq!(ov["reason"], "test active");
}

#[tokio::test]
async fn time_endpoint_returns_manual_epoch_when_override_active() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    // Set override to 60 seconds ahead of NTP time (within max_jump_ms=100_000).
    // Far enough from the raw NTP value to distinguish, but within the jump limit.
    let override_epoch: i64 = FIXED_EPOCH_MS + 60_000;
    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": override_epoch,
            "reason": "epoch test",
            "ttl_seconds": 60
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .expect("GET /time failed");
    assert_eq!(resp.status(), 200);
    // Save headers before consuming the body.
    let source_header = resp
        .headers()
        .get("x-time-source")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_owned());
    let body: serde_json::Value = resp.json().await.unwrap();
    let returned = body["data"].as_i64().unwrap_or(0);
    assert!(
        (returned - override_epoch).abs() < 1000,
        "expected epoch near {override_epoch}, got {returned}"
    );
    assert_eq!(source_header.as_deref(), Some("manual"));
}

#[tokio::test]
async fn status_endpoint_shows_manual_source_when_override_active() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "status test",
            "ttl_seconds": 60
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get(format!("{}/status", server.base_url))
        .send()
        .await
        .expect("GET /status failed");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["source"], "manual");
    assert_eq!(body["serve_state"], "ok");
    assert!(
        !body["override_info"].is_null(),
        "override_info should be present"
    );
    assert_eq!(body["override_info"]["reason"], "status test");
}

#[tokio::test]
async fn delete_override_clears_active_override() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "to delete",
            "ttl_seconds": 60
        }),
    )
    .await;

    let del = delete_override(&server.base_url, TOKEN).await;
    assert_eq!(del.status(), 200);
    let body: serde_json::Value = del.json().await.unwrap();
    assert_eq!(body["message"], "override cleared");

    // GET should now report inactive
    let get_resp = get_override_status(&server.base_url, TOKEN).await;
    let get_body: serde_json::Value = get_resp.json().await.unwrap();
    assert_eq!(get_body["active"], false);
}

#[tokio::test]
async fn delete_override_is_idempotent_when_no_override() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    let resp = delete_override(&server.base_url, TOKEN).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["message"], "no active override");
}

#[tokio::test]
async fn time_after_delete_returns_ntp_source() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "transient",
            "ttl_seconds": 60
        }),
    )
    .await;

    delete_override(&server.base_url, TOKEN).await;

    let resp = reqwest::Client::new()
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .expect("GET /time failed");
    assert_eq!(resp.status(), 200);
    let source = resp
        .headers()
        .get("x-time-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_ne!(
        source, "manual",
        "source should not be 'manual' after DELETE"
    );
}

#[tokio::test]
async fn override_auto_expires() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    // Set 1-second TTL
    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "expires soon",
            "ttl_seconds": 1
        }),
    )
    .await;

    // Verify active immediately
    let before = get_override_status(&server.base_url, TOKEN).await;
    let before_body: serde_json::Value = before.json().await.unwrap();
    assert_eq!(before_body["active"], true);

    // Wait for expiry (1s TTL + 500ms buffer)
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

    // Should be inactive now
    let after = get_override_status(&server.base_url, TOKEN).await;
    let after_body: serde_json::Value = after.json().await.unwrap();
    assert_eq!(
        after_body["active"], false,
        "override should be inactive after TTL expiry"
    );
}

#[tokio::test]
async fn post_override_replaces_existing_override() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    // Set first override
    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "first override",
            "ttl_seconds": 60
        }),
    )
    .await;

    // Set second override (replaces first)
    let second_epoch = FIXED_EPOCH_MS + 1000;
    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": second_epoch,
            "reason": "second override",
            "ttl_seconds": 60
        }),
    )
    .await;

    // GET should show the second override
    let resp = get_override_status(&server.base_url, TOKEN).await;
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["active"], true);
    assert_eq!(body["override"]["epoch_ms"], second_epoch);
    assert_eq!(body["override"]["reason"], "second override");
}

#[tokio::test]
async fn metrics_show_manual_override_active_gauge() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "metrics test",
            "ttl_seconds": 60
        }),
    )
    .await;

    let resp = reqwest::Client::new()
        .get(format!("{}/metrics", server.base_url))
        .send()
        .await
        .expect("GET /metrics failed");
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("manual_override_active"),
        "metrics should contain manual_override_active"
    );
    assert!(
        text.contains("manual_override_total"),
        "metrics should contain manual_override_total"
    );
}

#[tokio::test]
async fn force_true_allowed_when_allow_force_true() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    // max_jump_ms=100: normally rejects epochs more than 100 ms away from NTP.
    let server = spawn_server_with_admin_force_allowed(&upstream, TOKEN, 100).await;

    // This epoch is 1 year away — would fail jump check without force.
    let far_epoch = FIXED_EPOCH_MS + 365 * 24 * 3600 * 1000i64;
    let resp = post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": far_epoch,
            "reason": "emergency override",
            "ttl_seconds": 60,
            "force": true
        }),
    )
    .await;
    assert_eq!(
        resp.status(),
        200,
        "force=true with MANUAL_OVERRIDE_ALLOW_FORCE=true must succeed regardless of jump"
    );
}

#[tokio::test]
async fn websocket_tick_reports_manual_source() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "ws source test",
            "ttl_seconds": 60
        }),
    )
    .await;

    let ws_url = format!("ws://{}/stream", server.http_addr);
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .expect("WebSocket connection failed");
    let (_, mut read) = ws_stream.split();

    // Skip welcome message.
    tokio::time::timeout(Duration::from_secs(2), read.next())
        .await
        .expect("timed out waiting for welcome")
        .unwrap()
        .unwrap();

    // First tick must carry source="manual".
    let msg = tokio::time::timeout(Duration::from_secs(2), read.next())
        .await
        .expect("timed out waiting for tick")
        .expect("stream ended")
        .expect("WS error");
    let tick: serde_json::Value =
        serde_json::from_str(msg.to_text().unwrap()).expect("tick must be JSON");

    assert_eq!(tick["type"], "tick");
    assert_eq!(
        tick["source"], "manual",
        "WebSocket tick must report source='manual' while override is active"
    );
}

#[tokio::test]
async fn udp_ntp_server_advertises_manu_when_override_active() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let (server, ntp_addr) =
        spawn_server_with_admin_and_ntp_server(&upstream, TOKEN, 100_000).await;

    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": FIXED_EPOCH_MS,
            "reason": "udp ntp test",
            "ttl_seconds": 60
        }),
    )
    .await;

    let packet = common::query_ntp_udp(ntp_addr).await;

    assert_eq!(packet.li, 0, "LI must be 0 (no warning) in MANU mode");
    assert_eq!(packet.stratum, 2, "Stratum must be 2 in MANU mode");
    assert_eq!(
        packet.reference_id,
        u32::from_be_bytes(*b"MANU"),
        "reference_id must be MANU when manual override is active"
    );
    assert!(
        packet.root_dispersion > 0,
        "root_dispersion must be > 0 (base dispersion_ms=1000)"
    );
    assert_eq!(packet.root_delay, 0, "root_delay must be 0 in MANU mode");
}

#[tokio::test]
async fn monotonic_preserved_when_manual_epoch_behind() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    let server = spawn_server_with_admin(&upstream, TOKEN, 100_000).await;

    // Get first time value (NTP-based) to warm last_served_ms.
    let first: serde_json::Value = reqwest::Client::new()
        .get(format!("{}/time", server.base_url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let first_epoch = first["data"].as_i64().unwrap();

    // Set override 2 seconds BEHIND current NTP — within max_jump_ms=100_000 but below last served.
    let behind_epoch = FIXED_EPOCH_MS - 2000;
    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({
            "epoch_ms": behind_epoch,
            "reason": "monotonic test",
            "ttl_seconds": 60
        }),
    )
    .await;

    // Three consecutive /time calls must be strictly non-decreasing.
    let mut prev = first_epoch;
    for _ in 0..3 {
        let resp: serde_json::Value = reqwest::Client::new()
            .get(format!("{}/time", server.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let epoch = resp["data"].as_i64().unwrap();
        assert!(
            epoch >= prev,
            "time went backwards: {epoch} < {prev} (monotonic clamp must hold across override transition)"
        );
        prev = epoch;
    }
}

#[tokio::test]
async fn metrics_rejected_total_increments_for_multiple_reasons() {
    let upstream = start_mock_ntp_upstream(FIXED_EPOCH_MS).await;
    // tight max_jump_ms=100 so jump_too_large is easy to trigger
    let server = spawn_server_with_admin(&upstream, TOKEN, 100).await;

    // Trigger force_not_allowed (allow_force=false by default)
    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({ "epoch_ms": FIXED_EPOCH_MS, "reason": "r", "ttl_seconds": 60, "force": true }),
    )
    .await;

    // Trigger empty_reason validation error
    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({ "epoch_ms": FIXED_EPOCH_MS, "reason": "", "ttl_seconds": 60 }),
    )
    .await;

    // Trigger jump_too_large (1 minute away, over 100 ms limit)
    post_override(
        &server.base_url,
        TOKEN,
        &serde_json::json!({ "epoch_ms": FIXED_EPOCH_MS + 60_000, "reason": "r", "ttl_seconds": 60 }),
    )
    .await;

    let text = reqwest::Client::new()
        .get(format!("{}/metrics", server.base_url))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(
        text.contains(r#"reason="force_not_allowed""#),
        "force_not_allowed counter must appear in metrics"
    );
    assert!(
        text.contains(r#"reason="empty_reason""#),
        "empty_reason counter must appear in metrics"
    );
    assert!(
        text.contains(r#"reason="jump_too_large""#),
        "jump_too_large counter must appear in metrics"
    );
}
