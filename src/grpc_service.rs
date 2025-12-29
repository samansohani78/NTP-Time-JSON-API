use crate::http::state::AppState;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, info};

// Include the generated proto code
pub mod timeservice {
    tonic::include_proto!("timeservice");
}

use timeservice::time_service_server::{TimeService, TimeServiceServer};
use timeservice::{StreamRequest, TimeRequest, TimeResponse};

pub struct TimeServiceImpl {
    state: Arc<AppState>,
}

impl TimeServiceImpl {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    pub fn into_server(self) -> TimeServiceServer<Self> {
        TimeServiceServer::new(self)
    }

    fn create_time_response(
        &self,
        include_iso8601: bool,
        sequence: u64,
    ) -> Result<TimeResponse, Status> {
        match self.state.timebase.now_ms() {
            Some(epoch_ms) => {
                let is_stale = self
                    .state
                    .get_staleness_seconds()
                    .map(|s| s > self.state.config.ntp.max_staleness_secs)
                    .unwrap_or(false);

                let staleness_secs = self.state.get_staleness_seconds().unwrap_or(0);

                let message = if is_stale {
                    &self.state.config.messages.ok_cache
                } else {
                    &self.state.config.messages.ok
                };

                let iso8601 = if include_iso8601 {
                    format_epoch_ms_to_iso8601(epoch_ms)
                } else {
                    String::new()
                };

                Ok(TimeResponse {
                    epoch_ms,
                    iso8601,
                    message: message.clone(),
                    is_stale,
                    staleness_secs,
                    sequence,
                })
            }
            None => Err(Status::unavailable(
                self.state.config.messages.error_no_sync.clone(),
            )),
        }
    }
}

#[tonic::async_trait]
impl TimeService for TimeServiceImpl {
    async fn get_time(
        &self,
        request: Request<TimeRequest>,
    ) -> Result<Response<TimeResponse>, Status> {
        let req = request.into_inner();
        debug!(include_iso8601 = req.include_iso8601, "gRPC GetTime request");

        let response = self.create_time_response(req.include_iso8601, 0)?;
        Ok(Response::new(response))
    }

    type StreamTimeStream = ReceiverStream<Result<TimeResponse, Status>>;

    async fn stream_time(
        &self,
        request: Request<StreamRequest>,
    ) -> Result<Response<Self::StreamTimeStream>, Status> {
        let req = request.into_inner();

        let interval_ms = if req.interval_ms > 0 {
            req.interval_ms
        } else {
            1000 // Default 1 second
        };

        let max_updates = if req.max_updates > 0 {
            req.max_updates as u64
        } else {
            3600 // Default 1 hour at 1 second intervals
        };

        info!(
            interval_ms = interval_ms,
            max_updates = max_updates,
            include_iso8601 = req.include_iso8601,
            "gRPC StreamTime request"
        );

        let (tx, rx) = tokio::sync::mpsc::channel(128);
        let state = self.state.clone();

        tokio::spawn(async move {
            let mut tick = interval(Duration::from_millis(interval_ms as u64));
            let mut sequence = 0u64;

            while sequence < max_updates {
                tick.tick().await;

                let response = match state.timebase.now_ms() {
                    Some(epoch_ms) => {
                        let is_stale = state
                            .get_staleness_seconds()
                            .map(|s| s > state.config.ntp.max_staleness_secs)
                            .unwrap_or(false);

                        let staleness_secs = state.get_staleness_seconds().unwrap_or(0);

                        let message = if is_stale {
                            &state.config.messages.ok_cache
                        } else {
                            &state.config.messages.ok
                        };

                        let iso8601 = if req.include_iso8601 {
                            format_epoch_ms_to_iso8601(epoch_ms)
                        } else {
                            String::new()
                        };

                        TimeResponse {
                            epoch_ms,
                            iso8601,
                            message: message.clone(),
                            is_stale,
                            staleness_secs,
                            sequence,
                        }
                    }
                    None => {
                        // Service not yet synced
                        TimeResponse {
                            epoch_ms: 0,
                            iso8601: String::new(),
                            message: state.config.messages.error_no_sync.clone(),
                            is_stale: false,
                            staleness_secs: 0,
                            sequence,
                        }
                    }
                };

                if tx.send(Ok(response)).await.is_err() {
                    debug!(
                        sequence = sequence,
                        "gRPC StreamTime client disconnected"
                    );
                    break;
                }

                sequence += 1;
            }

            info!(
                updates_sent = sequence,
                max_updates = max_updates,
                "gRPC StreamTime completed"
            );
        });

        Ok(Response::new(ReceiverStream::new(rx)))
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
