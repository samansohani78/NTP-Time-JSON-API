mod common;

use ntp_time_json_api::ntp::protocol::{LI_ALARM_UNSYNCHRONIZED, LI_NO_WARNING, MODE_SERVER};

// ── Synced path ───────────────────────────────────────────────────────────────

/// After a sync, our UDP NTP server must respond with a well-formed Mode 4
/// reply: LI=0, VN=4, Mode=4, Stratum=2, positive dispersion, origin echo.
#[tokio::test]
async fn ntp_server_synced_response_is_well_formed() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let (server, ntp_addr) = common::spawn_server_with_ntp_server(&upstream).await;
    let _ = &server; // keep alive

    let reply = common::query_ntp_udp(ntp_addr).await;

    // RFC 5905 mode
    assert_eq!(reply.mode, MODE_SERVER, "mode must be 4 (server)");
    assert_eq!(reply.vn, 4, "version must be 4");

    // When synced: LI must NOT be alarm
    assert_ne!(
        reply.li, LI_ALARM_UNSYNCHRONIZED,
        "LI must not be alarm when synced"
    );
    assert_eq!(reply.li, LI_NO_WARNING, "LI must be 0 when synced");

    // We're a Stratum-2 server (upstream mock is stratum 1)
    assert_eq!(
        reply.stratum, 2,
        "stratum must be 2 after syncing to stratum-1 upstream"
    );

    // Dispersion must be positive after an honest sync
    assert!(
        reply.root_dispersion > 0,
        "root_dispersion must be > 0 after sync (P0-3)"
    );
    assert!(
        reply.root_delay > 0,
        "root_delay must be > 0 after sync (P0-3)"
    );

    // Transmit timestamp must be non-zero
    assert_ne!(
        reply.transmit_timestamp, 0,
        "transmit_timestamp must not be zero"
    );

    // Reference ID for a synced Stratum-2 server with LOCL: 0x4C4F434C
    let ref_id = u32::from_be_bytes(*b"LOCL");
    assert_eq!(reply.reference_id, ref_id, "reference_id must be 'LOCL'");
}

/// The origin_timestamp in the reply must echo the client's transmit_timestamp.
#[tokio::test]
async fn ntp_server_echoes_origin_timestamp() {
    let upstream = common::start_mock_ntp_upstream(1_704_067_200_000).await;
    let (server, ntp_addr) = common::spawn_server_with_ntp_server(&upstream).await;
    let _ = &server;

    // query_ntp_udp sends a packet with transmit_timestamp = unix_ms_to_ntp(now_ms).
    // We need to verify the server echoes it back.
    use ntp_time_json_api::ntp::protocol::{
        LI_NO_WARNING, MODE_CLIENT, NTP_VERSION, NtpPacket, parse_server_response,
        serialize_packet, unix_ms_to_ntp,
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let client_ts = unix_ms_to_ntp(now_ms);

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
        transmit_timestamp: client_ts,
    };
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(&serialize_packet(&req), ntp_addr)
        .await
        .unwrap();

    let mut buf = [0u8; 512];
    let (n, _) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.recv_from(&mut buf),
    )
    .await
    .expect("timed out")
    .unwrap();

    let reply = parse_server_response(&buf[..n]).unwrap();
    assert_eq!(
        reply.origin_timestamp, client_ts,
        "origin_timestamp must echo client transmit_timestamp"
    );
}

// ── Unsynced path ─────────────────────────────────────────────────────────────

/// Before any sync, the NTP server must reply with LI=3 and Stratum=16
/// to signal "I am not authoritative".
#[tokio::test]
async fn ntp_server_unsynced_response() {
    use ntp_time_json_api::config::Config;
    use std::sync::Arc;

    // Build state without any NTP sync.
    let mut config = Config::default();
    config.ntp.servers = vec!["127.0.0.1:1".into()]; // unreachable
    let config = Arc::new(config);

    use ntp_time_json_api::{
        http::state::AppState,
        metrics::Metrics,
        performance::{LockFreeMetrics, TimeCache},
        timebase::TimeBase,
    };
    let time_cache = Arc::new(TimeCache::new(
        config.messages.ok.clone(),
        config.messages.ok_cache.clone(),
    ));
    let timebase = TimeBase::new(config.ntp.monotonic_output).with_cache(time_cache.clone());
    let metrics = Arc::new(Metrics::new());
    let state = Arc::new(AppState::new(
        config.clone(),
        timebase,
        metrics,
        time_cache,
        Arc::new(LockFreeMetrics::new()),
    ));

    let ntp_addr = common::start_ntp_server_component(&state, &config).await;
    let reply = common::query_ntp_udp(ntp_addr).await;

    assert_eq!(
        reply.li, LI_ALARM_UNSYNCHRONIZED,
        "LI must be 3 (alarm) when unsynced"
    );
    assert_eq!(reply.stratum, 16, "stratum must be 16 when unsynced");
    assert_eq!(reply.root_delay, 0, "root_delay must be 0 when unsynced");
    assert_eq!(
        reply.root_dispersion, 0,
        "root_dispersion must be 0 when unsynced"
    );
}
