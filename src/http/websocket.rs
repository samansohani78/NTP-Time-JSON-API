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

    // Configuration
    let update_interval_ms = std::env::var("WS_UPDATE_INTERVAL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000); // Default: 1 second

    let max_duration_secs = std::env::var("WS_MAX_DURATION_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600); // Default: 1 hour

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
        let max_updates = (max_duration_secs * 1000) / update_interval_ms;

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
                    // Determine if stale
                    let is_stale = state_clone
                        .get_staleness_seconds()
                        .map(|s| s > state_clone.config.ntp.max_staleness_secs)
                        .unwrap_or(false);

                    let staleness_secs = state_clone.get_staleness_seconds().unwrap_or(0);

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
                    })
                }
                None => {
                    json!({
                        "type": "error",
                        "message": &state_clone.config.messages.error_no_sync,
                        "sequence": count,
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
}
