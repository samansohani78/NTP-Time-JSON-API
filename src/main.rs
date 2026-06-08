mod config;
mod errors;
mod http;
mod metrics;
mod ntp;
mod performance;
mod timebase;

// PERFORMANCE: Use jemalloc for 10-20% throughput improvement
#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

use config::Config;
use http::state::AppState;
use metrics::Metrics;
use ntp::{NtpServer, NtpSyncer};
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
    let time_cache = Arc::new(performance::TimeCache::new(
        config.messages.ok.clone(),
        config.messages.ok_cache.clone(),
    ));
    let perf_metrics = Arc::new(performance::LockFreeMetrics::new());
    let timebase = TimeBase::new(config.ntp.monotonic_output).with_cache(time_cache.clone());
    let metrics = Arc::new(Metrics::new());
    let ntp_syncer = Arc::new(NtpSyncer::new(Arc::new(config.ntp.clone())));
    let state = Arc::new(AppState::new(
        config.clone(),
        timebase.clone(),
        metrics.clone(),
        time_cache.clone(),
        perf_metrics.clone(),
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

    // Start NTP server (responds to NTP clients on UDP) if enabled
    let ntp_server_handle = if config.ntp_server.enabled {
        let ntp_server = NtpServer::new(config.ntp_server.addr, timebase.clone(), metrics.clone())
            .with_max_packet_size(config.ntp_server.max_packet_size);
        Some(tokio::spawn(async move {
            if let Err(e) = ntp_server.run().await {
                error!(error = %e, "NTP server terminated");
            }
        }))
    } else {
        info!("NTP server disabled (NTP_SERVER_ENABLED=false)");
        None
    };

    // Create HTTP router
    let app = http::create_router(state.clone());

    // Start HTTP server with TCP optimizations
    let listener = {
        use socket2::{Domain, Protocol, Socket, Type};
        use std::net::SocketAddr as StdSocketAddr;

        let addr: StdSocketAddr = config.http.addr;
        let domain = if addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };

        let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))
            .expect("Failed to create socket");

        // Enable SO_REUSEADDR for faster restarts
        socket
            .set_reuse_address(true)
            .expect("Failed to set SO_REUSEADDR");

        // Enable TCP_NODELAY for lower latency (disable Nagle's algorithm)
        if config.http.tcp_nodelay {
            socket
                .set_tcp_nodelay(true)
                .expect("Failed to set TCP_NODELAY");
        }

        // Enable TCP keepalive if configured
        if let Some(keepalive_secs) = config.http.tcp_keepalive_secs {
            let keepalive = socket2::TcpKeepalive::new()
                .with_time(std::time::Duration::from_secs(keepalive_secs));
            socket
                .set_tcp_keepalive(&keepalive)
                .expect("Failed to set TCP keepalive");
        }

        socket
            .set_nonblocking(true)
            .expect("Failed to set non-blocking");
        socket.bind(&addr.into()).expect("Failed to bind");
        socket.listen(1024).expect("Failed to listen");

        tokio::net::TcpListener::from_std(socket.into())
            .expect("Failed to convert to tokio listener")
    };

    info!(
        addr = %config.http.addr,
        tcp_nodelay = config.http.tcp_nodelay,
        tcp_keepalive = ?config.http.tcp_keepalive_secs,
        "HTTP server listening"
    );

    let http_server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());

    // Run HTTP server and wait for shutdown
    if let Err(e) = http_server.await {
        error!(error = %e, "HTTP server error");
    }

    info!("Shutting down...");

    // Give background tasks up to 5 seconds to finish on their own, then
    // forcibly abort them. Abort is idempotent; the previous shape of
    // this block had a buggy `tokio::select!` whose first arm always
    // won (100 ms < 5 s), so the "force exit" arm was dead code.
    if let Some(h) = ntp_server_handle.as_ref() {
        h.abort();
    }
    sync_handle.abort();
    probe_handle.abort();

    if tokio::time::timeout(Duration::from_secs(5), async {
        if let Some(h) = ntp_server_handle {
            let _ = h.await;
        }
        let _ = sync_handle.await;
        let _ = probe_handle.await;
    })
    .await
    .is_err()
    {
        warn!("Shutdown timeout exceeded, forcing exit");
    } else {
        info!("Background tasks stopped gracefully");
    }

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
                    .ntp_server_rtt_ms
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
