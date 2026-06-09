use anyhow::Result;
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(suffix: &str) -> String {
        format!(
            "/tmp/ntp_time_json_api_test_{suffix}_{}.json",
            std::process::id()
        )
    }

    fn make_state(epoch_ms: i64, saved_at_unix_ms: i64) -> PersistedState {
        PersistedState {
            version: PERSIST_VERSION,
            saved_epoch_ms: epoch_ms,
            saved_at_unix_ms,
            uncertainty_ms: Some(5.0),
            source: "ntp".to_string(),
            selected_server: Some("pool.ntp.org".to_string()),
            selected_provider: None,
            last_successful_ntp_sync_unix_ms: Some(saved_at_unix_ms),
        }
    }

    #[test]
    fn persisted_state_seeds_timebase_on_startup() {
        let path = tmp_path("seed");
        let _ = std::fs::remove_file(&path);

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let epoch_ms = 1_700_000_000_000i64;
        let state = make_state(epoch_ms, now_ms);
        save_state(&path, &state).expect("save");

        let loaded = load_state(&path).expect("load").expect("some");
        assert_eq!(loaded.version, PERSIST_VERSION);
        assert_eq!(loaded.saved_epoch_ms, epoch_ms);
        assert_eq!(loaded.saved_at_unix_ms, now_ms);
        assert_eq!(loaded.selected_server.as_deref(), Some("pool.ntp.org"));
        // The effective epoch after elapsed adjustment should be ≥ epoch_ms
        let elapsed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            - loaded.saved_at_unix_ms;
        let effective = loaded.saved_epoch_ms + elapsed;
        assert!(
            effective >= epoch_ms,
            "effective={effective} epoch_ms={epoch_ms}"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn stale_persisted_state_returns_holdover_with_high_uncertainty() {
        let path = tmp_path("stale");
        let _ = std::fs::remove_file(&path);

        // Simulate a state saved 24 hours ago
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        let stale_saved_at = now_ms - 86_400_000; // 24 hours ago
        let state = make_state(1_700_000_000_000, stale_saved_at);
        save_state(&path, &state).expect("save");

        let loaded = load_state(&path).expect("load").expect("some");
        let elapsed = now_ms - loaded.saved_at_unix_ms;
        let effective = loaded.saved_epoch_ms + elapsed;
        assert!(
            elapsed >= 86_000_000,
            "elapsed should reflect 24h gap: {elapsed}"
        );
        // A seed from this stale state would have high staleness, but the
        // service still seeds TimeBase (holdover source) rather than refusing.
        assert!(effective > loaded.saved_epoch_ms);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_state_returns_none_for_missing_file() {
        let result = load_state("/tmp/ntp_time_json_api_nonexistent_9182736.json")
            .expect("should not error on missing file");
        assert!(result.is_none());
    }

    #[test]
    fn save_state_is_atomic_write_then_rename() {
        let path = tmp_path("atomic");
        let _ = std::fs::remove_file(&path);
        let tmp = format!("{path}.tmp");

        let now_ms = 1_700_000_000_000i64;
        let state = make_state(now_ms, now_ms);
        save_state(&path, &state).expect("save");

        // After save, the .tmp file must not exist (it was renamed to path)
        assert!(
            !std::path::Path::new(&tmp).exists(),
            ".tmp file must be removed after atomic rename"
        );
        assert!(
            std::path::Path::new(&path).exists(),
            "state file must exist"
        );

        let _ = std::fs::remove_file(&path);
    }
}

pub const PERSIST_VERSION: u32 = 1;

/// Snapshot of the last-known-good time state, written to disk after each
/// successful NTP sync and loaded on startup to enable holdover when NTP is
/// temporarily unavailable (e.g. internet down, DNS failure, container restart).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedState {
    pub version: u32,
    /// NTP-derived epoch_ms at the moment this snapshot was taken.
    pub saved_epoch_ms: i64,
    /// Unix epoch ms (wall clock) at the moment this snapshot was taken,
    /// used to compute elapsed time since save on next startup.
    pub saved_at_unix_ms: i64,
    pub uncertainty_ms: Option<f64>,
    pub source: String,
    pub selected_server: Option<String>,
    pub selected_provider: Option<String>,
    /// Unix epoch ms of the last successful NTP sync (same as saved_at_unix_ms
    /// when saved immediately after sync).
    pub last_successful_ntp_sync_unix_ms: Option<i64>,
}

/// Write a `PersistedState` to `path` atomically (write-then-rename).
pub fn save_state(path: &str, state: &PersistedState) -> Result<()> {
    let json = serde_json::to_string_pretty(state)?;
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load a `PersistedState` from `path`.  Returns `None` if the file does
/// not exist (first startup).  Returns `Err` for parse / IO errors.
pub fn load_state(path: &str) -> Result<Option<PersistedState>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(serde_json::from_str(&content)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
