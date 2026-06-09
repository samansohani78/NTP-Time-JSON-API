#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use ntp_time_json_api::{
    config::Config,
    http::{create_router, create_router_for_test, state::AppState},
    metrics::{Metrics, ReplicaLabel},
    ntp::{
        NtpServer, NtpSyncer, SyncOutcome, SyncQuality,
        protocol::{
            LI_NO_WARNING, MODE_CLIENT, MODE_SERVER, NTP_VERSION, NtpPacket, STRATUM_PRIMARY,
            parse_packet, parse_server_response, serialize_packet, unix_ms_to_ntp,
        },
    },
    performance::{LockFreeMetrics, TimeCache},
    timebase::TimeBase,
};

// ── TestServer ────────────────────────────────────────────────────────────────

/// A running HTTP server bound to a random port, kept alive as long as this
/// value is in scope.  Drop to trigger graceful shutdown.
pub struct TestServer {
    pub base_url: String,
    pub http_addr: SocketAddr,
    pub state: Arc<AppState>,
    // Dropping the sender signals the server to shut down.
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

// ── MockNtpUpstream ───────────────────────────────────────────────────────────

/// A fake stratum-1 NTP server.  Replies to every Mode 3 query with a
/// well-formed response at the given fixed epoch.
pub struct MockNtpUpstream {
    pub addr: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockNtpUpstream {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub async fn start_mock_ntp_upstream(epoch_ms: i64) -> MockNtpUpstream {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = socket.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            let Ok((n, peer)) = socket.recv_from(&mut buf).await else {
                break;
            };
            let Ok(req) = parse_packet(&buf[..n]) else {
                continue;
            };
            let ts = unix_ms_to_ntp(epoch_ms);
            let reply = NtpPacket {
                li: LI_NO_WARNING,
                vn: NTP_VERSION,
                mode: MODE_SERVER,
                stratum: STRATUM_PRIMARY,
                poll: req.poll,
                precision: -20,
                root_delay: 0x0000_0100, // 256/65536 s ≈ 3.9 ms in NTP-short
                root_dispersion: 0x0000_0100, // same
                reference_id: u32::from_be_bytes(*b"GPS "),
                ref_timestamp: ts,
                origin_timestamp: req.transmit_timestamp,
                receive_timestamp: ts,
                transmit_timestamp: ts,
            };
            let wire = serialize_packet(&reply);
            let _ = socket.send_to(&wire, peer).await;
        }
    });

    MockNtpUpstream { addr, handle }
}

// ── Build helpers ─────────────────────────────────────────────────────────────

pub fn build_state(config: Arc<Config>) -> Arc<AppState> {
    let time_cache = Arc::new(TimeCache::new(
        config.messages.ok.clone(),
        config.messages.ok_cache.clone(),
    ));
    let perf_metrics = Arc::new(LockFreeMetrics::new());
    let timebase = TimeBase::new(config.ntp.monotonic_output).with_cache(time_cache.clone());
    let metrics = Arc::new(Metrics::new());
    Arc::new(AppState::new(
        config,
        timebase,
        metrics,
        time_cache,
        perf_metrics,
    ))
}

/// Apply one sync outcome to AppState — same bookkeeping sync_loop does in main.rs.
pub fn apply_sync_to_state(state: &AppState, outcome: &SyncOutcome) {
    use ntp_time_json_api::http::state::NtpTimingSummary;
    use ntp_time_json_api::ntp::selection::TimingSource;

    let result = &outcome.result;
    let diag = &outcome.diagnostics;
    state.timebase.update(result);
    state.record_sync_success();
    *state.last_selection_diagnostics.write() = Some(diag.clone());

    // Mirror what sync_loop does in main.rs: update P1-6 Prometheus metrics.
    state
        .metrics
        .ntp_selection_quorum_size
        .set(diag.quorum_size as i64);
    state
        .metrics
        .ntp_selection_single_provider
        .set(if diag.single_provider { 1 } else { 0 });
    if let Some(u) = diag.combined_uncertainty_ms {
        state.metrics.ntp_combined_uncertainty_milliseconds.set(u);
    }
    for (server, lambda_ms) in &diag.candidate_lambdas {
        state
            .metrics
            .ntp_sample_uncertainty_milliseconds
            .get_or_create(&ntp_time_json_api::metrics::ServerLabel {
                server: server.clone(),
            })
            .set(*lambda_ms);
    }

    // P1F-12: intersection metrics
    {
        let ix = &diag.intersection;
        state
            .metrics
            .ntp_intersection_truechimers
            .set(ix.truechimer_count as i64);
        state
            .metrics
            .ntp_intersection_ambiguous_clusters
            .set(ix.competing_cluster_count as i64);
        if let Some(w) = ix.intersection_width_ms {
            state.metrics.ntp_intersection_width_milliseconds.set(w);
        }
        if ix.falseticker_count > 0 {
            state
                .metrics
                .ntp_intersection_falsetickers_total
                .inc_by(ix.falseticker_count as u64);
        }
    }

    // P1-8: replica drift metrics — compute quality (reflects any active override too)
    let quality = state.compute_quality();
    let replica_label = ReplicaLabel {
        replica_id: state.config.replica.replica_id.clone(),
    };
    state
        .metrics
        .time_replica_offset_milliseconds
        .get_or_create(&replica_label)
        .set(result.offset_ms as f64);
    state
        .metrics
        .time_replica_uncertainty_milliseconds
        .get_or_create(&replica_label)
        .set(quality.uncertainty_ms.unwrap_or(0.0));
    state
        .metrics
        .time_replica_serve_state
        .get_or_create(&replica_label)
        .set(match quality.serve_state {
            "ok" => 0,
            "degraded" => 1,
            "stopped" => 2,
            "unsynced" => 3,
            _ => 4, // "holdover"
        });
    state
        .metrics
        .time_replica_source_mode
        .get_or_create(&replica_label)
        .set(match quality.source {
            "ntp" => 0,
            "degraded" => 1,
            "unsynced" => 2,
            "manual" => 3,
            _ => 4, // "holdover"
        });

    let rtt_ms = result.rtt.as_millis() as u64;
    state.last_rtt_ms.store(rtt_ms, Ordering::Release);

    *state.last_ntp_timing.write() = Some(NtpTimingSummary {
        server: result.server.clone(),
        t1_client_send_ms: result.t1_client_send_ms,
        t2_server_recv_ms: result.t2_server_recv_ms,
        t3_server_send_ms: result.t3_server_send_ms,
        t4_client_recv_ms: result.t4_client_recv_ms,
        offset_ms: result.offset_ms,
        rtt_ms,
        root_delay_ms: result.root_delay_ms,
        root_dispersion_ms: result.root_dispersion_ms,
        stratum: result.stratum,
        leap: result.leap,
        precision_log2: result.precision_log2,
        reference_id: result.reference_id,
        timing_source: TimingSource::Measured,
    });

    *state.last_sync_quality.write() = Some(SyncQuality {
        upstream_root_delay_ms: result.root_delay_ms,
        upstream_root_dispersion_ms: result.root_dispersion_ms,
        precision_log2: result.precision_log2,
        stratum: result.stratum,
        leap: result.leap,
        measured_rtt_ms: rtt_ms,
        jitter_ms: outcome.jitter_ms,
        offset_ms: result.offset_ms,
        last_sync_instant: Instant::now(),
        selected_server: result.server.clone(),
    });
}

async fn start_http_server(state: Arc<AppState>) -> TestServer {
    let app = create_router_for_test(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .ok();
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    TestServer {
        base_url: format!("http://{addr}"),
        http_addr: addr,
        state,
        _shutdown: shutdown_tx,
    }
}

/// Like `start_http_server` but with rate limiting enabled and ConnectInfo injected,
/// matching the production `main.rs` serve path exactly.
async fn start_http_server_rate_limited(state: Arc<AppState>) -> TestServer {
    let app = create_router(state.clone()); // rate limiting enabled (disable_rate_limiting=false)
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = shutdown_rx.await;
        })
        .await
        .ok();
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

    TestServer {
        base_url: format!("http://{addr}"),
        http_addr: addr,
        state,
        _shutdown: shutdown_tx,
    }
}

// ── Public spawn functions ────────────────────────────────────────────────────

/// Spawn an HTTP server that has NOT yet performed any NTP sync.
/// `/time` will return 503 (REQUIRE_SYNC=true).
pub async fn spawn_server_unsynced() -> TestServer {
    let mut config = Config::default();
    config.ntp.servers = vec!["127.0.0.1:1".to_string()]; // unreachable; won't be contacted
    config.ntp.require_sync = true;
    config.ws.update_interval_ms = 100;
    start_http_server(build_state(Arc::new(config))).await
}

/// Spawn an HTTP server that has completed one NTP sync against `upstream`.
pub async fn spawn_server_synced(upstream: &MockNtpUpstream) -> TestServer {
    let mut config = Config::default();
    config.ntp.servers = vec![upstream.addr.to_string()];
    config.ntp.timeout_secs = 5;
    config.ntp.require_sync = true;
    config.ntp.selection.min_quorum = 1; // single upstream in tests
    config.ws.update_interval_ms = 100;
    let config = Arc::new(config);

    let state = build_state(config.clone());
    let syncer = NtpSyncer::new(Arc::new(config.ntp.clone()));
    let outcome = syncer
        .sync()
        .await
        .expect("initial sync against mock NTP upstream should succeed");
    apply_sync_to_state(&state, &outcome);

    start_http_server(state).await
}

/// Spawn an HTTP server with rate limiting enabled (production code path).
/// Uses `into_make_service_with_connect_info` so `PeerIpKeyExtractor` can read
/// the client IP — the same path as `main.rs`.
pub async fn spawn_server_synced_rate_limited(upstream: &MockNtpUpstream) -> TestServer {
    let mut config = Config::default();
    config.ntp.servers = vec![upstream.addr.to_string()];
    config.ntp.timeout_secs = 5;
    config.ntp.require_sync = true;
    config.ntp.selection.min_quorum = 1;
    config.ws.update_interval_ms = 100;
    // disable_rate_limiting defaults to false — rate limiting IS active
    let config = Arc::new(config);

    let state = build_state(config.clone());
    let syncer = NtpSyncer::new(Arc::new(config.ntp.clone()));
    let outcome = syncer
        .sync()
        .await
        .expect("initial sync against mock NTP upstream should succeed");
    apply_sync_to_state(&state, &outcome);

    start_http_server_rate_limited(state).await
}

/// Spawn an HTTP server and a UDP NTP server component, both synced.
/// Returns `(TestServer, ntp_udp_addr)`.
pub async fn spawn_server_with_ntp_server(upstream: &MockNtpUpstream) -> (TestServer, SocketAddr) {
    let mut config = Config::default();
    config.ntp.servers = vec![upstream.addr.to_string()];
    config.ntp.timeout_secs = 5;
    config.ntp.require_sync = true;
    config.ntp.selection.min_quorum = 1;
    config.ws.update_interval_ms = 100;
    let config = Arc::new(config);

    let state = build_state(config.clone());
    let syncer = NtpSyncer::new(Arc::new(config.ntp.clone()));
    let outcome = syncer.sync().await.expect("initial sync should succeed");
    apply_sync_to_state(&state, &outcome);

    let ntp_addr = start_ntp_server_component(&state, &config).await;
    let server = start_http_server(state).await;
    (server, ntp_addr)
}

/// Spawn an HTTP server with admin API enabled.
/// The server has completed one NTP sync against `upstream`.
pub async fn spawn_server_with_admin(
    upstream: &MockNtpUpstream,
    admin_token: &str,
    max_jump_ms: u64,
) -> TestServer {
    let mut config = Config::default();
    config.ntp.servers = vec![upstream.addr.to_string()];
    config.ntp.timeout_secs = 5;
    config.ntp.require_sync = true;
    config.ntp.selection.min_quorum = 1;
    config.ws.update_interval_ms = 100;
    config.admin.enabled = true;
    config.admin.token = admin_token.to_string();
    config.admin.max_ttl_secs = 300;
    config.admin.max_jump_ms = max_jump_ms;
    config.admin.dispersion_ms = 1000;
    let config = Arc::new(config);

    let state = build_state(config.clone());
    let syncer = NtpSyncer::new(Arc::new(config.ntp.clone()));
    let outcome = syncer
        .sync()
        .await
        .expect("initial sync against mock NTP upstream should succeed");
    apply_sync_to_state(&state, &outcome);

    start_http_server(state).await
}

/// Spawn an HTTP server with admin API enabled and `allow_force=true`.
/// The server has completed one NTP sync against `upstream`.
pub async fn spawn_server_with_admin_force_allowed(
    upstream: &MockNtpUpstream,
    admin_token: &str,
    max_jump_ms: u64,
) -> TestServer {
    let mut config = Config::default();
    config.ntp.servers = vec![upstream.addr.to_string()];
    config.ntp.timeout_secs = 5;
    config.ntp.require_sync = true;
    config.ntp.selection.min_quorum = 1;
    config.ws.update_interval_ms = 100;
    config.admin.enabled = true;
    config.admin.token = admin_token.to_string();
    config.admin.max_ttl_secs = 300;
    config.admin.max_jump_ms = max_jump_ms;
    config.admin.allow_force = true;
    config.admin.dispersion_ms = 1000;
    let config = Arc::new(config);

    let state = build_state(config.clone());
    let syncer = NtpSyncer::new(Arc::new(config.ntp.clone()));
    let outcome = syncer
        .sync()
        .await
        .expect("initial sync against mock NTP upstream should succeed");
    apply_sync_to_state(&state, &outcome);

    start_http_server(state).await
}

/// Spawn an HTTP server with admin API enabled **and** a UDP NTP server component.
/// Returns `(TestServer, udp_addr)`.  The server has completed one NTP sync.
pub async fn spawn_server_with_admin_and_ntp_server(
    upstream: &MockNtpUpstream,
    admin_token: &str,
    max_jump_ms: u64,
) -> (TestServer, std::net::SocketAddr) {
    let mut config = Config::default();
    config.ntp.servers = vec![upstream.addr.to_string()];
    config.ntp.timeout_secs = 5;
    config.ntp.require_sync = true;
    config.ntp.selection.min_quorum = 1;
    config.ws.update_interval_ms = 100;
    config.admin.enabled = true;
    config.admin.token = admin_token.to_string();
    config.admin.max_ttl_secs = 300;
    config.admin.max_jump_ms = max_jump_ms;
    config.admin.dispersion_ms = 1000;
    let config = Arc::new(config);

    let state = build_state(config.clone());
    let syncer = NtpSyncer::new(Arc::new(config.ntp.clone()));
    let outcome = syncer
        .sync()
        .await
        .expect("initial sync against mock NTP upstream should succeed");
    apply_sync_to_state(&state, &outcome);

    let ntp_addr = start_ntp_server_component(&state, &config).await;
    let server = start_http_server(state).await;
    (server, ntp_addr)
}

/// Spawn an HTTP server that has never synced — but is seeded from a simulated
/// persisted state (i.e. TimeBase has been updated with a synthetic SyncResult).
/// `/time` should return 200 with `source="holdover"`.
pub async fn spawn_server_holdover_seeded(epoch_ms: i64) -> TestServer {
    use ntp_time_json_api::ntp::{SyncResult, selection::TimingSource};
    let mut config = Config::default();
    config.ntp.servers = vec!["127.0.0.1:1".to_string()]; // unreachable
    config.ntp.require_sync = true;
    config.ws.update_interval_ms = 100;
    let config = Arc::new(config);
    let state = build_state(config);
    // Seed TimeBase as if loaded from persisted state, but do NOT populate last_sync_quality
    let seed = SyncResult {
        epoch_ms,
        server: "persisted".to_string(),
        rtt: std::time::Duration::ZERO,
        instant: std::time::Instant::now(),
        offset_ms: 0,
        t1_client_send_ms: epoch_ms,
        t2_server_recv_ms: epoch_ms,
        t3_server_send_ms: epoch_ms,
        t4_client_recv_ms: epoch_ms,
        root_delay_ms: 0,
        root_dispersion_ms: 1000,
        stratum: 2,
        leap: 0,
        precision_log2: 0,
        reference_id: u32::from_be_bytes(*b"LOAD"),
        timing_source: TimingSource::Estimated,
    };
    state.timebase.update(&seed);
    start_http_server(state).await
}

/// Spawn an HTTP server synced with `strict_sla_mode=true`.
/// High uncertainty will cause `/time` to return 503.
pub async fn spawn_server_synced_strict(upstream: &MockNtpUpstream) -> TestServer {
    let mut config = Config::default();
    config.ntp.servers = vec![upstream.addr.to_string()];
    config.ntp.timeout_secs = 5;
    config.ntp.require_sync = true;
    config.ntp.selection.min_quorum = 1;
    config.ws.update_interval_ms = 100;
    config.quality.strict_sla_mode = true;
    config.quality.allow_degraded = false;
    let config = Arc::new(config);

    let state = build_state(config.clone());
    let syncer = NtpSyncer::new(Arc::new(config.ntp.clone()));
    let outcome = syncer
        .sync()
        .await
        .expect("initial sync against mock NTP upstream should succeed");
    apply_sync_to_state(&state, &outcome);

    start_http_server(state).await
}

/// Spawn an HTTP server with admin API enabled but unsynced (no initial NTP sync).
pub async fn spawn_server_admin_unsynced(admin_token: &str) -> TestServer {
    let mut config = Config::default();
    config.ntp.servers = vec!["127.0.0.1:1".to_string()]; // unreachable
    config.ntp.require_sync = true;
    config.ntp.selection.min_quorum = 1;
    config.ws.update_interval_ms = 100;
    config.admin.enabled = true;
    config.admin.token = admin_token.to_string();
    config.admin.max_ttl_secs = 300;
    config.admin.max_jump_ms = 10_000_000; // large so epoch_ms is accepted
    config.admin.allow_force = true;
    config.admin.dispersion_ms = 1000;
    start_http_server(build_state(Arc::new(config))).await
}

/// Start only the UDP NTP server component on an ephemeral port.
/// Returns the actual bound address.
pub async fn start_ntp_server_component(state: &Arc<AppState>, config: &Config) -> SocketAddr {
    let ntp_server = NtpServer::new(
        "127.0.0.1:0".parse().unwrap(),
        state.timebase.clone(),
        state.metrics.clone(),
        state.last_sync_quality.clone(),
        config.ntp_server.max_root_dispersion_ms,
    );

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        ntp_server.run_with_ready(ready_tx).await.ok();
    });

    ready_rx.await.expect("NTP server should bind and notify")
}

// ── NTP UDP helpers ───────────────────────────────────────────────────────────

/// Send a Mode 3 NTP request to `server_addr` and return the parsed response packet.
pub async fn query_ntp_udp(server_addr: SocketAddr) -> NtpPacket {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;

    let req = NtpPacket {
        li: LI_NO_WARNING,
        vn: NTP_VERSION,
        mode: MODE_CLIENT,
        stratum: 0,
        poll: 4,
        precision: 0,
        root_delay: 0,
        root_dispersion: 0,
        reference_id: 0,
        ref_timestamp: 0,
        origin_timestamp: 0,
        receive_timestamp: 0,
        transmit_timestamp: unix_ms_to_ntp(now_ms),
    };
    let wire = serialize_packet(&req);

    socket.send_to(&wire, server_addr).await.unwrap();

    let mut buf = [0u8; 512];
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.recv_from(&mut buf),
    )
    .await
    .expect("NTP response timed out")
    .expect("recv_from failed");

    parse_server_response(&buf[..n]).expect("failed to parse NTP response")
}
