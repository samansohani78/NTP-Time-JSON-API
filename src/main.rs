mod config;
mod errors;
mod http;
mod metrics;
mod ntp;
mod timebase;

use config::Config;
use http::state::AppState;
use metrics::Metrics;
use ntp::NtpSyncer;
use std::sync::Arc;
use std::time::Duration;
use timebase::TimeBase;
use tokio::signal;
use tokio::time::{interval, sleep};
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load configuration
    let config = Arc::new(Config::from_env()?);

    // Initialize logging
    init_logging(&config);

    info!(
        version = env!("CARGO_PKG_VERSION"),
        addr = %config.http.addr,
        "Starting NTP Time JSON API"
    );

    // Initialize components
    let timebase = TimeBase::new(config.ntp.monotonic_output);
    let metrics = Arc::new(Metrics::new());
    let ntp_syncer = Arc::new(NtpSyncer::new(Arc::new(config.ntp.clone())));
    let state = Arc::new(AppState::new(
        config.clone(),
        timebase.clone(),
        metrics.clone(),
    ));

    // Start background sync loop
    let sync_handle = tokio::spawn(sync_loop(
        ntp_syncer.clone(),
        timebase.clone(),
        state.clone(),
        config.clone(),
    ));

    // Start probe loop (for keeping server stats fresh)
    let probe_handle = tokio::spawn(probe_loop(
        ntp_syncer.clone(),
        state.clone(),
        config.clone(),
    ));

    // Create HTTP router
    let app = http::create_router(state.clone());

    // Start HTTP server
    let listener = tokio::net::TcpListener::bind(&config.http.addr)
        .await
        .expect("Failed to bind to address");

    info!(addr = %config.http.addr, "HTTP server listening");

    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());

    // Run server and wait for shutdown
    if let Err(e) = server.await {
        error!(error = %e, "Server error");
    }

    info!("Shutting down...");

    // Cancel background tasks
    sync_handle.abort();
    probe_handle.abort();

    info!("Shutdown complete");
    Ok(())
}

/// Background sync loop - syncs with NTP servers periodically
async fn sync_loop(
    syncer: Arc<NtpSyncer>,
    timebase: TimeBase,
    state: Arc<AppState>,
    config: Arc<Config>,
) {
    let mut sync_interval = interval(config.sync_interval());

    // Add initial jitter to avoid thundering herd
    let jitter = rand::random::<u64>() % 5000;
    sleep(Duration::from_millis(jitter)).await;

    loop {
        sync_interval.tick().await;

        state.metrics.ntp_sync_total.inc();

        match syncer.sync().await {
            Ok(result) => {
                // Update timebase
                timebase.update(&result);

                // Update state
                state.record_sync_success();

                // Update metrics
                state.metrics.ntp_last_sync_timestamp_seconds.set(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64,
                );
                state
                    .metrics
                    .ntp_rtt_seconds
                    .observe(result.rtt.as_secs_f64());
                state.metrics.ntp_consecutive_failures.set(0);

                info!(
                    server = %result.server,
                    rtt_ms = result.rtt.as_millis(),
                    "NTP sync successful"
                );
            }
            Err(e) => {
                state.record_sync_failure();
                state.metrics.ntp_sync_errors_total.inc();
                state
                    .metrics
                    .ntp_consecutive_failures
                    .set(state.get_consecutive_failures() as i64);

                if timebase.has_synced() {
                    // We've synced before, so we can continue serving from cache
                    warn!(
                        error = %e,
                        consecutive_failures = state.get_consecutive_failures(),
                        serving_from_cache = true,
                        "NTP sync failed; serving from cache"
                    );
                } else {
                    // Never synced, this is more critical
                    error!(
                        error = %e,
                        consecutive_failures = state.get_consecutive_failures(),
                        "NTP sync failed; service not yet synchronized"
                    );
                }
            }
        }

        // Update staleness metric
        if let Some(staleness) = state.get_staleness_seconds() {
            state.metrics.ntp_staleness_seconds.set(staleness as i64);
        }
    }
}

/// Probe loop - periodically updates server health stats
async fn probe_loop(syncer: Arc<NtpSyncer>, state: Arc<AppState>, config: Arc<Config>) {
    // Calculate random interval between min and max
    let min_ms = config.ntp.probe_min_interval_secs * 1000;
    let max_ms = config.ntp.probe_max_interval_secs * 1000;

    loop {
        let jitter = if max_ms > min_ms {
            rand::random::<u64>() % (max_ms - min_ms)
        } else {
            0
        };
        let delay = Duration::from_millis(min_ms + jitter);
        sleep(delay).await;

        // Update per-server metrics
        let stats = syncer.get_stats().await;
        for (server, stat) in stats {
            let is_up = if stat.is_healthy() { 1 } else { 0 };
            state
                .metrics
                .ntp_server_up
                .get_or_create(&metrics::ServerLabel {
                    server: server.clone(),
                })
                .set(is_up);

            if let Some(rtt) = stat.last_rtt {
                state
                    .metrics
                    .ntp_server_rtt_seconds
                    .get_or_create(&metrics::ServerLabel { server })
                    .set(rtt.as_millis() as i64);
            }
        }
    }
}

/// Initialize logging based on configuration
fn init_logging(config: &Config) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

    match config.logging.format {
        config::LogFormat::Json => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().json())
                .init();
        }
        config::LogFormat::Pretty => {
            tracing_subscriber::registry()
                .with(env_filter)
                .with(tracing_subscriber::fmt::layer().pretty())
                .init();
        }
    }
}

/// Graceful shutdown signal handler
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            info!("Received Ctrl+C signal");
        },
        _ = terminate => {
            info!("Received SIGTERM signal");
        },
    }
}
