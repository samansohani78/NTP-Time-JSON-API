/// WebSocket streaming client example for NTP Time JSON API
/// Demonstrates real-time time streaming using tokio-tungstenite
use anyhow::Result;
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message};

const WS_URL: &str = "ws://localhost:8080/stream";

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
enum WebSocketMessage {
    #[serde(rename = "welcome")]
    Welcome {
        message: String,
        update_interval_ms: u64,
        max_duration_secs: u64,
    },
    #[serde(rename = "tick")]
    Tick {
        epoch_ms: i64,
        iso8601: String,
        is_stale: bool,
        staleness_secs: u64,
        message: String,
        sequence: u64,
    },
    #[serde(rename = "error")]
    Error { message: String, sequence: u64 },
}

struct WebSocketTimeClient {
    ws_url: String,
    message_count: u64,
}

impl WebSocketTimeClient {
    fn new(ws_url: Option<String>) -> Self {
        Self {
            ws_url: ws_url.unwrap_or_else(|| WS_URL.to_string()),
            message_count: 0,
        }
    }

    async fn connect_and_stream(&mut self, duration_secs: Option<u64>) -> Result<()> {
        println!("Connecting to {}...", self.ws_url);

        let (ws_stream, _) = connect_async(&self.ws_url).await?;
        println!("✓ Connected\n");

        let (mut _write, mut read) = ws_stream.split();

        let start = tokio::time::Instant::now();

        while let Some(message) = read.next().await {
            match message {
                Ok(Message::Text(text)) => {
                    self.handle_message(&text)?;

                    // Check duration
                    if let Some(dur) = duration_secs {
                        if start.elapsed().as_secs() >= dur {
                            break;
                        }
                    }
                }
                Ok(Message::Close(_)) => {
                    println!("✓ Connection closed by server");
                    break;
                }
                Err(e) => {
                    eprintln!("✗ WebSocket error: {}", e);
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn handle_message(&mut self, text: &str) -> Result<()> {
        let msg: WebSocketMessage = serde_json::from_str(text)?;

        match msg {
            WebSocketMessage::Welcome {
                message,
                update_interval_ms,
                max_duration_secs,
            } => {
                println!("📡 {}", message);
                println!("   Update interval: {}ms", update_interval_ms);
                println!("   Max duration: {}s\n", max_duration_secs);
            }

            WebSocketMessage::Tick {
                epoch_ms,
                iso8601: _,
                is_stale,
                staleness_secs,
                message: _,
                sequence,
            } => {
                self.message_count += 1;

                // Convert to DateTime
                let secs = epoch_ms / 1000;
                let nsecs = ((epoch_ms % 1000) * 1_000_000) as u32;

                if let Some(dt) = DateTime::from_timestamp(secs, nsecs) {
                    let dt_utc: DateTime<Utc> = dt.into();
                    let time_str = dt_utc.format("%Y-%m-%d %H:%M:%S%.3f");

                    let stale_indicator = if is_stale { "⚠ STALE" } else { "✓" };

                    println!(
                        "[{:04}] {} {} UTC (age: {}s)",
                        sequence, stale_indicator, time_str, staleness_secs
                    );
                }
            }

            WebSocketMessage::Error { message, sequence } => {
                println!("[{:04}] ✗ Error: {}", sequence, message);
            }
        }

        Ok(())
    }

    fn get_stats(&self) -> (u64, bool) {
        (self.message_count, false)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("{}", "=".repeat(60));
    println!("NTP Time JSON API - WebSocket Streaming Client (Rust)");
    println!("{}", "=".repeat(60));
    println!("\nPress Ctrl+C to stop\n");

    let mut client = WebSocketTimeClient::new(None);

    // Stream for 30 seconds
    if let Err(e) = client.connect_and_stream(Some(30)).await {
        eprintln!("Error: {}", e);
    }

    // Print stats
    let (messages_received, _connected) = client.get_stats();
    println!("\n{}", "=".repeat(60));
    println!("Session Statistics:");
    println!("  Messages received: {}", messages_received);
    println!("{}", "=".repeat(60));

    Ok(())
}
