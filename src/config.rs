use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub http: HttpConfig,
    pub ntp: NtpConfig,
    pub ntp_server: NtpServerConfig,
    pub ws: WsConfig,
    pub logging: LoggingConfig,
    pub messages: MessageConfig,
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
    pub selection_strategy: SelectionStrategy,
    pub max_offset_skew_ms: i64,
    pub monotonic_output: bool,
    pub offset_bias_ms: i64,
    pub asymmetry_bias_ms: i64,
    pub max_consecutive_failures: u32,
}

/// NTP server selection strategy.
///
/// Only one strategy is currently implemented. The env-var value
/// `"rtt_min"` is kept for backwards compatibility even though the
/// actual algorithm is accuracy-first (closest to median offset),
/// using RTT only as a tiebreaker.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelectionStrategy {
    /// Accuracy-first: select the server closest to the consensus
    /// (median) offset. RTT is used only as a tiebreaker. Accepted
    /// env-var value: `"rtt_min"` (historical; preserved for compat).
    AccuracyFirst,
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

        let max_offset_skew_ms = env_or_parse("MAX_OFFSET_SKEW_MS", 1000);
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
                max_offset_skew_ms,
                monotonic_output,
                offset_bias_ms,
                asymmetry_bias_ms,
                max_consecutive_failures,
            },
            ntp_server: NtpServerConfig {
                enabled: ntp_server_enabled,
                addr: ntp_server_addr,
                max_packet_size: ntp_server_max_packet_size,
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
        if self.ws.update_interval_ms == 0 {
            anyhow::bail!("WS_UPDATE_INTERVAL_MS must be at least 1 ms");
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

// For tests only
#[cfg(test)]
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
                max_offset_skew_ms: 1000,
                monotonic_output: true,
                offset_bias_ms: 0,
                asymmetry_bias_ms: 0,
                max_consecutive_failures: 10,
            },
            ntp_server: NtpServerConfig {
                enabled: false,
                addr: "0.0.0.0:123".parse().unwrap(),
                max_packet_size: 1024,
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
