// Integration tests for the NTP Time JSON API.
//
// NOTE: This is a binary crate (main.rs, no lib.rs), so integration tests in
// tests/ cannot import internal types. The full NTP sync → HTTP pipeline tests
// live as inline #[cfg(test)] modules in src/http/mod.rs where all types are
// accessible. See src/http/mod.rs::tests for:
//
//   - test_time_before_sync_returns_503
//   - test_readyz_before_sync_returns_503
//   - test_startupz_before_sync_returns_503
//   - test_time_after_sync_returns_correct_epoch  (uses real UDP mock NTP server)
//   - test_probes_return_200_after_sync
//   - test_time_is_monotonic
//   - test_metrics_endpoint_contains_required_families
//   - test_performance_endpoint_structure
//
// This file is kept as a placeholder. To add external integration tests that
// test the running binary over HTTP, either:
//   a) Add a src/lib.rs to expose internal types to this crate, or
//   b) Use std::process::Command to spawn the built binary and test via reqwest.
