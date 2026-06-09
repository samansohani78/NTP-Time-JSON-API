use super::state::AppState;
use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, info, warn};

/// WebSocket upgrade handler
pub async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| websocket_connection(socket, state))
}

/// Handle WebSocket connection - streams time updates
async fn websocket_connection(socket: WebSocket, state: Arc<AppState>) {
    let (mut sender, mut receiver) = socket.split();

    // Client info
    info!("WebSocket client connected");

    // Read the WS config once (it was populated at startup from
    // the WS_UPDATE_INTERVAL_MS / WS_MAX_DURATION_SECS env vars
    // and validated in Config::from_env). Re-reading std::env on
    // every connection would defeat rolling deploys and waste
    // a few microseconds per handshake.
    let update_interval_ms = state.config.ws.update_interval_ms;
    let max_duration_secs = state.config.ws.max_duration_secs;

    // Send welcome message
    let welcome = json!({
        "type": "welcome",
        "message": "Connected to NTP Time JSON API WebSocket",
        "update_interval_ms": update_interval_ms,
        "max_duration_secs": max_duration_secs,
    });

    if sender
        .send(Message::Text(
            serde_json::to_string(&welcome).unwrap().into(),
        ))
        .await
        .is_err()
    {
        warn!("Failed to send welcome message, client disconnected");
        return;
    }

    // Spawn a task to send time updates
    let state_clone = state.clone();
    let send_task = tokio::spawn(async move {
        let mut tick = interval(Duration::from_millis(update_interval_ms));
        let mut count = 0u64;
        let max_updates = compute_max_updates(max_duration_secs, update_interval_ms);

        loop {
            tick.tick().await;

            if count >= max_updates {
                info!(
                    updates_sent = count,
                    max_duration_secs = max_duration_secs,
                    "WebSocket max duration reached, closing connection"
                );
                break;
            }

            let message = match state_clone.timebase.now_ms() {
                Some(epoch_ms) => {
                    let quality = state_clone.compute_quality();
                    let is_stale = quality.serve_state != "ok";
                    let staleness_secs = quality.staleness_ms.unwrap_or(0) / 1000;

                    json!({
                        "type": "tick",
                        "epoch_ms": epoch_ms,
                        "iso8601": format_epoch_ms_to_iso8601(epoch_ms),
                        "is_stale": is_stale,
                        "staleness_secs": staleness_secs,
                        "message": if is_stale {
                            &state_clone.config.messages.ok_cache
                        } else {
                            &state_clone.config.messages.ok
                        },
                        "sequence": count,
                        // P0-4 quality fields
                        "source": quality.source,
                        "serve_state": quality.serve_state,
                        "uncertainty_ms": quality.uncertainty_ms,
                        "staleness_ms": quality.staleness_ms,
                    })
                }
                None => {
                    json!({
                        "type": "error",
                        "message": &state_clone.config.messages.error_no_sync,
                        "sequence": count,
                        "source": "unsynced",
                        "serve_state": "unsynced",
                    })
                }
            };

            let text = serde_json::to_string(&message).unwrap();

            if sender.send(Message::Text(text.into())).await.is_err() {
                debug!(updates_sent = count, "WebSocket client disconnected");
                break;
            }

            count += 1;
        }

        // Send close message
        let _ = sender
            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1000, // Normal closure
                reason: "Max duration reached or client closed".into(),
            })))
            .await;
    });

    // Spawn a task to receive messages (ping/pong, close)
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(text) => {
                    debug!(message = %text, "Received text message from client");
                }
                Message::Close(_) => {
                    debug!("Client sent close message");
                    break;
                }
                Message::Ping(data) => {
                    debug!("Received ping, ignoring (axum handles pong)");
                    // Axum automatically sends pong
                    let _ = data; // Suppress unused warning
                }
                Message::Pong(_) => {
                    debug!("Received pong");
                }
                _ => {}
            }
        }
    });

    // Wait for either task to complete
    tokio::select! {
        _ = send_task => {
            info!("WebSocket send task completed");
        }
        _ = recv_task => {
            info!("WebSocket receive task completed");
        }
    }

    info!("WebSocket connection closed");
}

/// Compute the maximum number of tick messages to send for a connection.
///
/// Returns `u64::MAX` when `max_duration_secs` is 0 (unlimited).
/// Uses saturating multiplication to defend against absurdly large values.
/// `update_interval_ms` is guaranteed > 0 by `Config::validate`.
fn compute_max_updates(max_duration_secs: u64, update_interval_ms: u64) -> u64 {
    if max_duration_secs == 0 {
        u64::MAX
    } else {
        max_duration_secs.saturating_mul(1000) / update_interval_ms
    }
}

/// Format epoch milliseconds to ISO 8601 string
fn format_epoch_ms_to_iso8601(epoch_ms: i64) -> String {
    use chrono::DateTime;

    let secs = epoch_ms / 1000;
    let nsecs = ((epoch_ms % 1000) * 1_000_000) as u32;

    match DateTime::from_timestamp(secs, nsecs) {
        Some(dt) => dt.to_rfc3339(),
        None => "invalid".to_string(),
    }
}

use futures_util::SinkExt;
use futures_util::stream::StreamExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iso8601_formatting() {
        let epoch_ms = 1735459200000; // Recent timestamp
        let iso = format_epoch_ms_to_iso8601(epoch_ms);
        // Verify it's not invalid and contains a date and time
        assert_ne!(iso, "invalid");
        assert!(iso.contains("T")); // ISO8601 has T separator
        assert!(iso.len() > 10); // Should be full date-time
    }

    #[test]
    fn test_compute_max_updates_unlimited() {
        // max_duration_secs=0 means unlimited — should return u64::MAX
        assert_eq!(compute_max_updates(0, 1000), u64::MAX);
        assert_eq!(compute_max_updates(0, 500), u64::MAX);
    }

    #[test]
    fn test_compute_max_updates_normal() {
        // 60 seconds at 1000ms interval = 60 updates
        assert_eq!(compute_max_updates(60, 1000), 60);
        // 60 seconds at 500ms interval = 120 updates
        assert_eq!(compute_max_updates(60, 500), 120);
        // 3600 seconds at 1000ms = 3600 updates
        assert_eq!(compute_max_updates(3600, 1000), 3600);
    }

    #[test]
    fn test_compute_max_updates_truncates() {
        // 1 second at 300ms interval = 3 (not 3.33), integer truncation
        assert_eq!(compute_max_updates(1, 300), 3);
    }

    #[test]
    fn test_compute_max_updates_saturating() {
        // u64::MAX * 1000 would overflow without saturating_mul; verify it doesn't panic
        let result = compute_max_updates(u64::MAX, 1);
        assert_eq!(result, u64::MAX); // saturates at u64::MAX, then / 1 = u64::MAX
    }
}
