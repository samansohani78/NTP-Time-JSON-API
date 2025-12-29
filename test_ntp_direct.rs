// Test direct NTP query to understand the offset issue
use rsntp::SntpClient;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let servers = vec![
        "pool.ntp.org:123",
        "time.google.com:123",
        "time.cloudflare.com:123",
        "ntp.iranet.ir:123",
    ];

    println!("=== Direct NTP Query Test ===\n");

    let system_time = SystemTime::now();
    let system_unix = system_time.duration_since(UNIX_EPOCH).unwrap();
    let system_epoch_ms = system_unix.as_millis() as i64;

    println!("System time: {} ms", system_epoch_ms);
    println!("System time: {} UTC\n", chrono::DateTime::from_timestamp_millis(system_epoch_ms).unwrap());

    for server in servers {
        println!("Testing {}...", server);

        let client = SntpClient::new();
        match client.synchronize(server) {
            Ok(result) => {
                // Method 1: Using clock_offset (current implementation)
                let offset = result.clock_offset().as_secs_f64();
                let ntp_time_method1 = system_unix.as_secs_f64() + offset;
                let epoch_ms_method1 = (ntp_time_method1 * 1000.0) as i64;

                println!("  clock_offset: {:.3}s", offset);
                println!("  Method 1 (system + offset): {} ms", epoch_ms_method1);
                println!("  Method 1 UTC: {}", chrono::DateTime::from_timestamp_millis(epoch_ms_method1).unwrap());

                // Try to get transmit timestamp if available
                // Note: rsntp may not expose this directly

                let diff_ms = epoch_ms_method1 - system_epoch_ms;
                println!("  Difference from system: {} ms", diff_ms);
                println!();
            }
            Err(e) => {
                println!("  Error: {}\n", e);
            }
        }
    }
}
