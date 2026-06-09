use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub http: HttpConfig,
    pub ntp: NtpConfig,
    pub ntp_server: NtpServerConfig,
    pub quality: QualityConfig,
    pub persist: PersistConfig,
    pub ws: WsConfig,
    pub logging: LoggingConfig,
    pub messages: MessageConfig,
    pub admin: AdminConfig,
    pub replica: ReplicaConfig,
}

/// P1-8 replica identity configuration.
///
/// `replica_id` is the unique label for this process instance.
/// It is stamped on replica-specific Prometheus metrics so operators
/// can detect inter-replica drift via alerting rules.
///
/// Resolution order:
/// 1. `REPLICA_ID` env var (explicit)
/// 2. `HOSTNAME` env var (set automatically inside a Kubernetes pod)
/// 3. `replica-<pid>` (process-local fallback)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplicaConfig {
    /// Non-empty, max 128 characters.
    pub replica_id: String,
}

/// Configuration for the optional admin API (P1-7 secure manual time override).
///
/// All admin endpoints are only registered when `enabled = true`.
/// Enabling without setting `token` is a startup error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminConfig {
    /// Whether the admin API is enabled. Default: false.
    pub enabled: bool,
    /// Bearer token required for all admin endpoints.
    /// Must be non-empty when `enabled = true`. Never logged.
    pub token: String,
    /// Maximum TTL (seconds) for a manual time override. Default: 300.
    pub max_ttl_secs: u32,
    /// Maximum epoch_ms jump (ms) allowed from current NTP time. Default: 5000.
    pub max_jump_ms: u64,
    /// Whether `force=true` in POST /admin/time/override is allowed.
    /// Set via `MANUAL_OVERRIDE_ALLOW_FORCE=true`. Default: false.
    /// When false, any request with `force=true` is rejected 400.
    /// When true, `force=true` bypasses the jump check (monotonic clamp still applies).
    pub allow_force: bool,
    /// Base root_dispersion (ms) advertised by the UDP NTP server in MANU mode. Default: 1000.
    pub dispersion_ms: u64,
}

/// Serve/stop SLA thresholds for the time-quality envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityConfig {
    /// When `false` (default), the service is holdover-first: after any seed
    /// (NTP, manual, or persisted), `/time` always returns HTTP 200 and reports
    /// quality via headers and `serve_state`. Only returns 503 when completely
    /// uninitialized (no seed) + `REQUIRE_SYNC=true`.
    ///
    /// When `true` (strict / opt-in for financial deployments), high uncertainty
    /// returns 503 exactly as in the pre-v1.1 behaviour:
    ///   uncertainty > `serve_ok_max_uncertainty_ms` + `ALLOW_DEGRADED=false` → 503
    ///   uncertainty > `serve_degraded_max_uncertainty_ms` → 503 always
    pub strict_sla_mode: bool,
    /// In strict mode only: allow uncertainty in the degraded band to return 200.
    /// Ignored when `strict_sla_mode=false`.
    pub allow_degraded: bool,
    /// Max uncertainty (ms) to report `serve_state="ok"`. Default 50 ms.
    pub serve_ok_max_uncertainty_ms: f64,
    /// Max uncertainty (ms) for the degraded band (strict mode). Default 250 ms.
    pub serve_degraded_max_uncertainty_ms: f64,
    /// Max uncertainty (ms) for `/readyz` to return 200 after first sync.
    pub readiness_max_uncertainty_ms: f64,
}

/// Persisted last-good state for restart recovery.
///
/// When `enabled=true`, the service writes a JSON snapshot to `file_path`
/// after every successful NTP sync.  On the next startup, if NTP is
/// unreachable, the snapshot is read and used to seed the `TimeBase` so
/// the service can serve time in holdover mode until NTP recovers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistConfig {
    /// Set `TIME_STATE_PERSIST_ENABLED=true` to enable. Default: false.
    pub enabled: bool,
    /// Path to the JSON state file. Default: `/var/lib/ntp-time-json-api/state.json`.
    pub file_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    pub addr: SocketAddr,
    pub request_timeout_secs: u64,
    pub body_limit_bytes: usize,
    pub tcp_nodelay: bool,
    pub tcp_keepalive_secs: Option<u64>,
    /// When `true`, skip `GovernorLayer` rate limiting. Set via
    /// `DISABLE_RATE_LIMITING=true`. Useful for local dev/smoke-testing
    /// where no real peer IP is available to `PeerIpKeyExtractor`.
    pub disable_rate_limiting: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpConfig {
    pub servers: Vec<String>,
    pub timeout_secs: u64,
    pub sync_interval_secs: u64,
    pub probe_min_interval_secs: u64,
    pub probe_max_interval_secs: u64,
    pub max_staleness_secs: u64,
    pub require_sync: bool,
    /// Deprecated: accepted for backwards compat but has no effect since P1-6.
    pub selection_strategy: SelectionStrategy,
    pub monotonic_output: bool,
    pub offset_bias_ms: i64,
    pub asymmetry_bias_ms: i64,
    pub max_consecutive_failures: u32,
    /// P1-6 uncertainty-aware weighted-median selection configuration.
    pub selection: SelectionConfig,
}

/// NTP server selection strategy.
///
/// **Deprecated** — kept for backwards-compatible env-var parsing only.
/// P1-6 replaced the algorithm with uncertainty-aware weighted median + quorum;
/// the `SELECTION_STRATEGY` env var is accepted but has no effect.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStrategy {
    /// Historical alias for the old accuracy-first algorithm.
    /// Accepted for backwards compat; ignored by the P1-6 selection.
    AccuracyFirst,
}

/// Configuration for the P1-6 uncertainty-aware weighted-median NTP selection
/// algorithm.  All fields are read from environment variables at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionConfig {
    /// Maximum upstream stratum accepted (hard gate). Default: 4.
    pub max_stratum: u8,
    /// Minimum number of agreeing servers required for a valid selection.
    /// A production deployment should use ≥ 2 NTP sources. Default: 2.
    pub min_quorum: usize,
    /// Hard-gate samples with `leap = 3` (LI_ALARM / unsynchronised). Default: true.
    pub reject_leap_alarm: bool,
    /// Hard-gate samples whose root-distance λ exceeds this (ms). Default: 500.0.
    pub max_root_distance_ms: f64,
    /// Hard-gate samples older than this (seconds). Default: 60 (= 2 × default sync interval).
    pub max_sample_age_secs: u64,
    /// Provider-group cap: if one provider group holds more than this fraction
    /// of the agreers, `single_provider=true` and uncertainty is doubled. Default: 0.5.
    pub provider_group_max_fraction: f64,
    /// Optional per-server provider-group overrides.
    /// Format: `"server1=group1,server2=group2"` via `NTP_PROVIDER_GROUPS`.
    pub provider_groups: HashMap<String, String>,
    /// Maximum offset deviation from the weighted median for a server to be
    /// considered an agreer.  Moved from `NtpConfig` for P1-6.  Default: 1000 ms.
    pub max_offset_skew_ms: i64,
    /// P1F-12: enable Marzullo/interval-intersection pre-filter before the weighted
    /// median step.  When true, candidates whose uncertainty intervals do not
    /// participate in the maximum-overlap cluster are rejected as falsetickers.
    /// Default: true.  Set `NTP_INTERVAL_SELECTION_ENABLED=false` to disable.
    pub interval_selection_enabled: bool,
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            max_stratum: 4,
            min_quorum: 2,
            reject_leap_alarm: true,
            max_root_distance_ms: 500.0,
            max_sample_age_secs: 60,
            provider_group_max_fraction: 0.5,
            provider_groups: HashMap::new(),
            max_offset_skew_ms: 1000,
            interval_selection_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NtpServerConfig {
    /// Whether to listen for NTP client requests on UDP.
    pub enabled: bool,
    /// UDP bind address. Default `0.0.0.0:123`. Binding to ports < 1024
    /// requires `CAP_NET_BIND_SERVICE` (or root).
    pub addr: SocketAddr,
    /// Maximum packet size we will accept from a client.
    pub max_packet_size: usize,
    /// Hard ceiling on the `root_dispersion` we will ever advertise to
    /// downstream NTP clients (milliseconds). RFC 5905 §7.1 caps
    /// MAX_DISPERSION at 16 seconds; 16000 is the conservative default.
    /// Set `NTP_SERVER_MAX_ROOT_DISPERSION_MS` to override.
    pub max_root_dispersion_ms: u64,
}

/// WebSocket configuration. Read once at startup; the per-connection
/// handler reads from `AppState` rather than re-hitting `std::env`.
///
/// * `update_interval_ms` — milliseconds between time updates sent to
///   each connected client. `0` is treated as "unset" and falls back
///   to the default (1000 ms). Must be > 0 at runtime.
/// * `max_duration_secs` — maximum connection length before the
///   server auto-closes. `0` is "unlimited" (no cap). The
///   `validate()` method enforces sane bounds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WsConfig {
    pub update_interval_ms: u64,
    pub max_duration_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Pretty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageConfig {
    pub ok: String,
    pub ok_cache: String,
    pub error: String,
    pub error_no_sync: String,
    pub error_internal: String,
    pub error_timeout: String,
}

/// Resolve the replica ID using the priority chain:
/// `REPLICA_ID` → `HOSTNAME` → `replica-<pid>`.
pub(crate) fn resolve_replica_id() -> String {
    std::env::var("REPLICA_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| format!("replica-{}", std::process::id()))
}

fn env_or_default(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_or_parse<T: std::str::FromStr>(key: &str, default: T) -> T
where
    T::Err: std::fmt::Debug,
{
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl Config {
    pub fn from_env() -> Result<Self> {
        // HTTP config
        let addr = env_or_default("ADDR", "0.0.0.0:8080")
            .parse()
            .context("Failed to parse ADDR")?;
        let request_timeout_secs = env_or_parse("REQUEST_TIMEOUT", 5);
        let body_limit_bytes = env_or_parse("BODY_LIMIT_BYTES", 1024);
        let tcp_nodelay = env_or_parse("TCP_NODELAY", true);
        let tcp_keepalive_secs = match env_or_parse("TCP_KEEPALIVE_SECS", 0) {
            0 => None,
            n => Some(n),
        };
        let disable_rate_limiting = env_or_parse("DISABLE_RATE_LIMITING", false);

        // Logging config
        let level = env_or_default("LOG_LEVEL", "info");
        let format = match env_or_default("LOG_FORMAT", "json").to_lowercase().as_str() {
            "pretty" => LogFormat::Pretty,
            _ => LogFormat::Json,
        };

        // NTP config
        let servers_str = env_or_default(
            "NTP_SERVERS",
            "time.google.com:123,time.cloudflare.com:123,pool.ntp.org:123",
        );
        let servers: Vec<String> = servers_str
            .split(',')
            .map(|s| {
                let s = s.trim().to_string();
                if s.is_empty() || s.contains(':') {
                    s
                } else {
                    format!("{}:123", s)
                }
            })
            .filter(|s| !s.is_empty())
            .collect();

        if servers.is_empty() {
            anyhow::bail!("NTP_SERVERS cannot be empty");
        }

        // NTP server (responds to NTP clients on UDP) config
        let ntp_server_enabled = env_or_parse("NTP_SERVER_ENABLED", false);
        let ntp_server_addr = env_or_default("NTP_SERVER_ADDR", "0.0.0.0:123")
            .parse()
            .context("Failed to parse NTP_SERVER_ADDR")?;
        let ntp_server_max_packet_size =
            env_or_parse("NTP_SERVER_MAX_PACKET_SIZE", 1024usize).max(48);
        let ntp_server_max_root_dispersion_ms =
            env_or_parse("NTP_SERVER_MAX_ROOT_DISPERSION_MS", 16_000u64);

        // WebSocket config. 0 / unparseable falls back to the default.
        // We apply the .filter(|&ms| ms > 0) guard here so the
        // per-connection handler doesn't have to re-do the validation
        // and divide-by-zero in the max_updates calculation.
        let ws_update_interval_ms = env_or_parse("WS_UPDATE_INTERVAL_MS", 1000u64).max(1);
        let ws_max_duration_secs = env_or_parse("WS_MAX_DURATION_SECS", 3600u64);

        let timeout_secs = env_or_parse("NTP_TIMEOUT", 2);
        let sync_interval_secs = env_or_parse("SYNC_INTERVAL", 30);
        let probe_min_interval_secs = env_or_parse("PROBE_MIN_INTERVAL", 10);
        let probe_max_interval_secs = env_or_parse("PROBE_MAX_INTERVAL", 20);
        let max_staleness_secs = env_or_parse("MAX_STALENESS", 120);
        let require_sync = env_or_parse("REQUIRE_SYNC", true);

        let selection_strategy = match env_or_default("SELECTION_STRATEGY", "rtt_min")
            .to_lowercase()
            .as_str()
        {
            "rtt_min" | "accuracy_first" => SelectionStrategy::AccuracyFirst,
            other => anyhow::bail!("Invalid SELECTION_STRATEGY: {}", other),
        };

        // P1-6 selection config
        let sel_max_stratum = env_or_parse("MAX_STRATUM", 4u8);
        let sel_min_quorum = env_or_parse("MIN_QUORUM", 2usize);
        let sel_reject_leap_alarm = env_or_parse("REJECT_LEAP_ALARM", true);
        let sel_max_root_distance_ms = env_or_parse("MAX_ROOT_DISTANCE_MS", 500.0f64);
        let sel_max_sample_age_secs = env_or_parse("MAX_SAMPLE_AGE_SECS", 60u64);
        let sel_provider_group_max_fraction = env_or_parse("PROVIDER_GROUP_MAX_FRACTION", 0.5f64);
        let sel_provider_groups: HashMap<String, String> = {
            let raw = env_or_default("NTP_PROVIDER_GROUPS", "");
            raw.split(',')
                .filter(|s| s.contains('='))
                .filter_map(|s| {
                    let mut parts = s.splitn(2, '=');
                    let k = parts.next()?.trim().to_string();
                    let v = parts.next()?.trim().to_string();
                    if k.is_empty() || v.is_empty() {
                        None
                    } else {
                        Some((k, v))
                    }
                })
                .collect()
        };
        let sel_max_offset_skew_ms = env_or_parse("MAX_OFFSET_SKEW_MS", 1000i64);
        let sel_interval_selection_enabled = env_or_parse("NTP_INTERVAL_SELECTION_ENABLED", true);

        let monotonic_output = env_or_parse("MONOTONIC_OUTPUT", true);
        let offset_bias_ms = env_or_parse("OFFSET_BIAS_MS", 0);
        let asymmetry_bias_ms = env_or_parse("ASYMMETRY_BIAS_MS", 0);
        let max_consecutive_failures = env_or_parse("MAX_CONSECUTIVE_FAILURES", 10);

        // Message config
        let ok = env_or_default("MSG_OK", "done");
        let ok_cache = env_or_default("MSG_OK_CACHE", "done");
        let error = env_or_default("MSG_ERROR", "error");
        let error_no_sync = env_or_default(
            "ERROR_TEXT_NO_SYNC",
            "Service not yet synchronized with NTP",
        );
        let error_internal = env_or_default("ERROR_TEXT_INTERNAL", "Internal server error");
        let error_timeout = env_or_default("ERROR_TEXT_TIMEOUT", "Request timeout");

        // Quality / SLA config
        let strict_sla_mode = env_or_parse("STRICT_SLA_MODE", false);
        let allow_degraded = env_or_parse("ALLOW_DEGRADED", false);
        let serve_ok_max_uncertainty_ms = env_or_parse("SERVE_OK_MAX_UNCERTAINTY_MS", 50.0f64);
        let serve_degraded_max_uncertainty_ms =
            env_or_parse("SERVE_DEGRADED_MAX_UNCERTAINTY_MS", 250.0f64);
        let readiness_max_uncertainty_ms = env_or_parse("READINESS_MAX_UNCERTAINTY_MS", 250.0f64);

        // Persistence config
        let persist_enabled = env_or_parse("TIME_STATE_PERSIST_ENABLED", false);
        let persist_file =
            env_or_default("TIME_STATE_FILE", "/var/lib/ntp-time-json-api/state.json");

        // P1-8: replica identity
        let replica_id = resolve_replica_id();

        // Admin API config (P1-7)
        let admin_enabled = env_or_parse("ADMIN_API_ENABLED", false);
        let admin_token = env_or_default("ADMIN_API_TOKEN", "");
        let admin_max_ttl_secs = env_or_parse("MANUAL_OVERRIDE_MAX_TTL_SECS", 300u32);
        let admin_max_jump_ms = env_or_parse("MANUAL_OVERRIDE_MAX_JUMP_MS", 5000u64);
        let admin_dispersion_ms = env_or_parse("MANUAL_OVERRIDE_DISPERSION_MS", 1000u64);
        let admin_allow_force = env_or_parse("MANUAL_OVERRIDE_ALLOW_FORCE", false);

        let config = Config {
            http: HttpConfig {
                addr,
                request_timeout_secs,
                body_limit_bytes,
                tcp_nodelay,
                tcp_keepalive_secs,
                disable_rate_limiting,
            },
            ntp: NtpConfig {
                servers,
                timeout_secs,
                sync_interval_secs,
                probe_min_interval_secs,
                probe_max_interval_secs,
                max_staleness_secs,
                require_sync,
                selection_strategy,
                monotonic_output,
                offset_bias_ms,
                asymmetry_bias_ms,
                max_consecutive_failures,
                selection: SelectionConfig {
                    max_stratum: sel_max_stratum,
                    min_quorum: sel_min_quorum,
                    reject_leap_alarm: sel_reject_leap_alarm,
                    max_root_distance_ms: sel_max_root_distance_ms,
                    max_sample_age_secs: sel_max_sample_age_secs,
                    provider_group_max_fraction: sel_provider_group_max_fraction,
                    provider_groups: sel_provider_groups,
                    max_offset_skew_ms: sel_max_offset_skew_ms,
                    interval_selection_enabled: sel_interval_selection_enabled,
                },
            },
            ntp_server: NtpServerConfig {
                enabled: ntp_server_enabled,
                addr: ntp_server_addr,
                max_packet_size: ntp_server_max_packet_size,
                max_root_dispersion_ms: ntp_server_max_root_dispersion_ms,
            },
            quality: QualityConfig {
                strict_sla_mode,
                allow_degraded,
                serve_ok_max_uncertainty_ms,
                serve_degraded_max_uncertainty_ms,
                readiness_max_uncertainty_ms,
            },
            persist: PersistConfig {
                enabled: persist_enabled,
                file_path: persist_file,
            },
            ws: WsConfig {
                update_interval_ms: ws_update_interval_ms,
                max_duration_secs: ws_max_duration_secs,
            },
            logging: LoggingConfig { level, format },
            messages: MessageConfig {
                ok,
                ok_cache,
                error,
                error_no_sync,
                error_internal,
                error_timeout,
            },
            admin: AdminConfig {
                enabled: admin_enabled,
                token: admin_token,
                max_ttl_secs: admin_max_ttl_secs,
                max_jump_ms: admin_max_jump_ms,
                allow_force: admin_allow_force,
                dispersion_ms: admin_dispersion_ms,
            },
            replica: ReplicaConfig { replica_id },
        };

        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.ntp.servers.is_empty() {
            anyhow::bail!("At least one NTP server must be configured");
        }
        if self.ntp.sync_interval_secs < 1 {
            anyhow::bail!("SYNC_INTERVAL must be at least 1 second");
        }
        if self.ntp.timeout_secs < 1 {
            anyhow::bail!("NTP_TIMEOUT must be at least 1 second");
        }
        if self.ntp.probe_min_interval_secs > self.ntp.probe_max_interval_secs {
            anyhow::bail!("PROBE_MIN_INTERVAL cannot be greater than PROBE_MAX_INTERVAL");
        }
        if self.ntp_server.max_packet_size < 48 {
            anyhow::bail!("NTP_SERVER_MAX_PACKET_SIZE must be at least 48");
        }
        if self.ntp_server.max_root_dispersion_ms == 0 {
            anyhow::bail!("NTP_SERVER_MAX_ROOT_DISPERSION_MS must be > 0");
        }
        if self.ws.update_interval_ms == 0 {
            anyhow::bail!("WS_UPDATE_INTERVAL_MS must be at least 1 ms");
        }
        if self.quality.serve_ok_max_uncertainty_ms <= 0.0 {
            anyhow::bail!("SERVE_OK_MAX_UNCERTAINTY_MS must be > 0");
        }
        if self.quality.serve_ok_max_uncertainty_ms
            >= self.quality.serve_degraded_max_uncertainty_ms
        {
            anyhow::bail!(
                "SERVE_OK_MAX_UNCERTAINTY_MS must be less than SERVE_DEGRADED_MAX_UNCERTAINTY_MS"
            );
        }
        if self.admin.enabled && self.admin.token.is_empty() {
            anyhow::bail!("ADMIN_API_TOKEN must be set when ADMIN_API_ENABLED=true");
        }
        if self.admin.enabled && self.admin.max_ttl_secs == 0 {
            anyhow::bail!("MANUAL_OVERRIDE_MAX_TTL_SECS must be > 0");
        }
        if self.admin.enabled && self.admin.dispersion_ms == 0 {
            anyhow::bail!("MANUAL_OVERRIDE_DISPERSION_MS must be > 0");
        }
        if self.replica.replica_id.is_empty() {
            anyhow::bail!("REPLICA_ID must not be empty");
        }
        if self.replica.replica_id.len() > 128 {
            anyhow::bail!("REPLICA_ID must be 128 characters or fewer");
        }
        let sel = &self.ntp.selection;
        if sel.max_stratum == 0 {
            anyhow::bail!("MAX_STRATUM must be >= 1");
        }
        if sel.min_quorum == 0 {
            anyhow::bail!("MIN_QUORUM must be >= 1");
        }
        if sel.max_root_distance_ms <= 0.0 {
            anyhow::bail!("MAX_ROOT_DISTANCE_MS must be > 0");
        }
        if sel.max_sample_age_secs == 0 {
            anyhow::bail!("MAX_SAMPLE_AGE_SECS must be > 0");
        }
        if sel.provider_group_max_fraction <= 0.0 || sel.provider_group_max_fraction > 1.0 {
            anyhow::bail!("PROVIDER_GROUP_MAX_FRACTION must be in (0, 1]");
        }
        Ok(())
    }

    pub fn sync_interval(&self) -> Duration {
        Duration::from_secs(self.ntp.sync_interval_secs)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.http.request_timeout_secs)
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            http: HttpConfig {
                addr: "0.0.0.0:8080".parse().unwrap(),
                request_timeout_secs: 5,
                body_limit_bytes: 1024,
                tcp_nodelay: true,
                tcp_keepalive_secs: Some(60),
                disable_rate_limiting: false,
            },
            ntp: NtpConfig {
                servers: vec!["time.google.com:123".to_string()],
                timeout_secs: 2,
                sync_interval_secs: 30,
                probe_min_interval_secs: 10,
                probe_max_interval_secs: 20,
                max_staleness_secs: 120,
                require_sync: true,
                selection_strategy: SelectionStrategy::AccuracyFirst,
                monotonic_output: true,
                offset_bias_ms: 0,
                asymmetry_bias_ms: 0,
                max_consecutive_failures: 10,
                selection: SelectionConfig::default(),
            },
            ntp_server: NtpServerConfig {
                enabled: false,
                addr: "0.0.0.0:123".parse().unwrap(),
                max_packet_size: 1024,
                max_root_dispersion_ms: 16_000,
            },
            quality: QualityConfig {
                strict_sla_mode: false,
                allow_degraded: false,
                serve_ok_max_uncertainty_ms: 50.0,
                serve_degraded_max_uncertainty_ms: 250.0,
                readiness_max_uncertainty_ms: 250.0,
            },
            persist: PersistConfig {
                enabled: false,
                file_path: "/var/lib/ntp-time-json-api/state.json".to_string(),
            },
            ws: WsConfig {
                update_interval_ms: 1000,
                max_duration_secs: 3600,
            },
            logging: LoggingConfig {
                level: "info".to_string(),
                format: LogFormat::Json,
            },
            messages: MessageConfig {
                ok: "done".to_string(),
                ok_cache: "done".to_string(),
                error: "error".to_string(),
                error_no_sync: "Service not yet synchronized with NTP".to_string(),
                error_internal: "Internal server error".to_string(),
                error_timeout: "Request timeout".to_string(),
            },
            admin: AdminConfig {
                enabled: false,
                token: String::new(),
                max_ttl_secs: 300,
                max_jump_ms: 5000,
                allow_force: false,
                dispersion_ms: 1000,
            },
            replica: ReplicaConfig {
                replica_id: format!("replica-{}", std::process::id()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert!(!config.ntp.servers.is_empty());
        assert_eq!(
            config.ntp.selection_strategy,
            SelectionStrategy::AccuracyFirst
        );
        assert!(config.ntp.monotonic_output);
    }

    #[test]
    fn test_config_validation() {
        let mut config = Config::default();

        // Empty servers should fail
        config.ntp.servers.clear();
        assert!(config.validate().is_err());

        // Restore servers
        config.ntp.servers = vec!["time.google.com:123".to_string()];
        assert!(config.validate().is_ok());

        // Invalid probe intervals
        config.ntp.probe_min_interval_secs = 100;
        config.ntp.probe_max_interval_secs = 10;
        assert!(config.validate().is_err());

        // WS update interval of 0 should fail (would cause
        // divide-by-zero in the per-connection max_updates calc).
        config.ntp.probe_min_interval_secs = 10;
        config.ntp.probe_max_interval_secs = 20;
        config.ws.update_interval_ms = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_replica_id_from_explicit_env() {
        let saved = std::env::var("REPLICA_ID").ok();
        unsafe {
            std::env::set_var("REPLICA_ID", "my-explicit-replica");
        }
        let id = resolve_replica_id();
        unsafe {
            match saved {
                Some(v) => std::env::set_var("REPLICA_ID", v),
                None => std::env::remove_var("REPLICA_ID"),
            }
        }
        assert_eq!(id, "my-explicit-replica");
    }

    #[test]
    fn test_replica_id_defaults_from_hostname() {
        // Save current state so we can restore after the test.
        let saved_rid = std::env::var("REPLICA_ID").ok();
        let saved_host = std::env::var("HOSTNAME").ok();
        unsafe {
            std::env::remove_var("REPLICA_ID");
            std::env::set_var("HOSTNAME", "ntp-pod-abc123");
        }
        let id = resolve_replica_id();
        // Restore before asserting (cleanup even on failure via unwind).
        unsafe {
            match saved_rid {
                Some(v) => std::env::set_var("REPLICA_ID", v),
                None => std::env::remove_var("REPLICA_ID"),
            }
            match saved_host {
                Some(v) => std::env::set_var("HOSTNAME", v),
                None => std::env::remove_var("HOSTNAME"),
            }
        }
        assert_eq!(id, "ntp-pod-abc123");
    }

    #[test]
    fn test_replica_id_too_long_fails_validation() {
        let mut config = Config::default();
        config.replica.replica_id = "x".repeat(129);
        assert!(config.validate().is_err());
        config.replica.replica_id = "x".repeat(128);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_utf8_messages() {
        // Test that UTF-8 Persian strings work
        unsafe {
            std::env::set_var("MSG_OK", "انجام شد");
            std::env::set_var("MSG_ERROR", "خطا");
        }

        let config = Config::from_env().unwrap();

        assert_eq!(config.messages.ok, "انجام شد");
        assert_eq!(config.messages.error, "خطا");

        // Cleanup
        unsafe {
            std::env::remove_var("MSG_OK");
            std::env::remove_var("MSG_ERROR");
        }
    }
}
