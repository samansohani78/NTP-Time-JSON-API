mod common;

use futures_util::StreamExt;
use std::time::Duration;
use tokio_tungstenite::connect_async;

/// Connect to /stream, receive the welcome message and at least one tick.
/// Verify the tick carries all expected quality fields.
#[tokio::test]
async fn websocket_delivers_welcome_and_tick() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let ws_url = format!("ws://{}/stream", server.http_addr);
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .expect("WebSocket connection failed");

    let (_, mut read) = ws_stream.split();

    // First message must be a "welcome"
    let msg = tokio::time::timeout(Duration::from_secs(2), read.next())
        .await
        .expect("timed out waiting for welcome")
        .expect("stream ended")
        .expect("WS error");

    let welcome: serde_json::Value =
        serde_json::from_str(msg.to_text().expect("welcome must be text"))
            .expect("welcome must be JSON");
    assert_eq!(
        welcome["type"], "welcome",
        "first message must be type=welcome"
    );
    assert!(welcome["update_interval_ms"].is_number());

    // Second message must be a "tick" (interval is 100 ms in the test config)
    let msg = tokio::time::timeout(Duration::from_secs(2), read.next())
        .await
        .expect("timed out waiting for tick")
        .expect("stream ended")
        .expect("WS error");

    let tick: serde_json::Value =
        serde_json::from_str(msg.to_text().expect("tick must be text")).expect("tick must be JSON");

    assert_eq!(tick["type"], "tick", "second message must be type=tick");
    assert!(
        tick["epoch_ms"].as_i64().unwrap_or(0) > 0,
        "epoch_ms must be positive"
    );
    assert!(tick["iso8601"].is_string(), "iso8601 missing from tick");
    assert!(tick["is_stale"].is_boolean(), "is_stale missing from tick");

    // Quality fields (P0-4)
    assert!(tick["source"].is_string(), "source missing from tick");
    assert!(
        tick["serve_state"].is_string(),
        "serve_state missing from tick"
    );
    // uncertainty_ms is a number when synced
    assert!(
        tick["uncertainty_ms"].is_number() || tick["uncertainty_ms"].is_null(),
        "uncertainty_ms should be a number or null"
    );
}

/// Consecutive ticks must have non-decreasing epoch_ms (monotonic).
#[tokio::test]
async fn websocket_ticks_are_monotonic() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let server = common::spawn_server_synced(&upstream).await;

    let ws_url = format!("ws://{}/stream", server.http_addr);
    let (ws_stream, _) = connect_async(&ws_url)
        .await
        .expect("WebSocket connection failed");
    let (_, mut read) = ws_stream.split();

    // Skip welcome
    tokio::time::timeout(Duration::from_secs(2), read.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    let mut prev_epoch: i64 = 0;
    for _ in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(2), read.next())
            .await
            .expect("timed out")
            .expect("stream ended")
            .expect("WS error");

        let tick: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        let epoch = tick["epoch_ms"].as_i64().unwrap_or(0);
        assert!(
            epoch >= prev_epoch,
            "time went backwards: {epoch} < {prev_epoch}"
        );
        prev_epoch = epoch;
    }
}
