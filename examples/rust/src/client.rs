/// Rust client example for NTP Time JSON API
/// Demonstrates async HTTP requests using reqwest
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::Instant;

const BASE_URL: &str = "http://localhost:8080";

#[derive(Debug, Deserialize, Serialize)]
struct TimeResponse {
    message: String,
    status: u16,
    data: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PerformanceMetrics {
    total_requests: u64,
    success_requests: u64,
    error_requests: u64,
    cache_hits: u64,
    cache_hit_rate: f64,
    avg_latency_us: f64,
    min_latency_us: u64,
    max_latency_us: u64,
}

struct NTPTimeClient {
    client: reqwest::Client,
    base_url: String,
}

impl NTPTimeClient {
    fn new(base_url: Option<String>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("NTP-Time-Rust-Client/1.0")
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            base_url: base_url.unwrap_or_else(|| BASE_URL.to_string()),
        }
    }

    async fn get_time(&self) -> Result<TimeResponse> {
        let url = format!("{}/time", self.base_url);
        let response = self.client.get(&url).send().await?;
        let time_data: TimeResponse = response.json().await?;
        Ok(time_data)
    }

    async fn get_time_ms(&self) -> Result<i64> {
        let response = self.get_time().await?;
        if response.status == 200 {
            Ok(response.data)
        } else {
            Err(anyhow::anyhow!("Non-200 status: {}", response.status))
        }
    }

    async fn get_time_datetime(&self) -> Result<DateTime<Utc>> {
        let epoch_ms = self.get_time_ms().await?;
        let secs = epoch_ms / 1000;
        let nsecs = ((epoch_ms % 1000) * 1_000_000) as u32;

        DateTime::from_timestamp(secs, nsecs)
            .ok_or_else(|| anyhow::anyhow!("Invalid timestamp"))
    }

    async fn healthz(&self) -> bool {
        let url = format!("{}/healthz", self.base_url);
        self.client
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    async fn readyz(&self) -> bool {
        let url = format!("{}/readyz", self.base_url);
        self.client
            .get(&url)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    async fn get_performance(&self) -> Result<PerformanceMetrics> {
        let url = format!("{}/performance", self.base_url);
        let response = self.client.get(&url).send().await?;
        let metrics: PerformanceMetrics = response.json().await?;
        Ok(metrics)
    }

    async fn get_metrics(&self) -> Result<String> {
        let url = format!("{}/metrics", self.base_url);
        let response = self.client.get(&url).send().await?;
        let text = response.text().await?;
        Ok(text)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("{}", "=".repeat(50));
    println!("NTP Time JSON API - Rust Client Example");
    println!("{}", "=".repeat(50));

    let client = NTPTimeClient::new(None);

    // 1. Health check
    println!("\n1. Health check:");
    if client.healthz().await {
        println!("   ✓ Service is alive");
    } else {
        println!("   ✗ Service is not responding");
        return Ok(());
    }

    // 2. Readiness check
    println!("\n2. Readiness check:");
    if client.readyz().await {
        println!("   ✓ Service is ready");
    } else {
        println!("   ✗ Service is not ready");
    }

    // 3. Get time (full response)
    println!("\n3. Get time (full response):");
    match client.get_time().await {
        Ok(time_data) => {
            println!("   Status: {}", time_data.status);
            println!("   Message: {}", time_data.message);
            println!("   Epoch MS: {}", time_data.data);
        }
        Err(e) => println!("   Error: {}", e),
    }

    // 4. Get time as milliseconds
    println!("\n4. Get time (epoch milliseconds):");
    match client.get_time_ms().await {
        Ok(epoch_ms) => println!("   {}", epoch_ms),
        Err(e) => println!("   Error: {}", e),
    }

    // 5. Get time as DateTime
    println!("\n5. Get time (DateTime<Utc>):");
    match client.get_time_datetime().await {
        Ok(dt) => {
            println!("   {}", dt.to_rfc3339());
            println!("   UTC: {}", dt.format("%Y-%m-%d %H:%M:%S%.3f"));
        }
        Err(e) => println!("   Error: {}", e),
    }

    // 6. Performance metrics
    println!("\n6. Performance metrics:");
    match client.get_performance().await {
        Ok(perf) => {
            println!("   Total requests: {}", perf.total_requests);
            println!("   Success rate: {}", perf.success_requests);
            println!("   Cache hit rate: {:.2}%", perf.cache_hit_rate * 100.0);
            println!("   Avg latency: {:.2}μs", perf.avg_latency_us);
        }
        Err(e) => println!("   Error: {}", e),
    }

    // 7. Benchmark
    println!("\n7. Benchmark (100 requests):");
    let start = Instant::now();
    let mut successes = 0;

    for _ in 0..100 {
        if client.get_time_ms().await.is_ok() {
            successes += 1;
        }
    }

    let duration = start.elapsed();
    let rps = 100.0 / duration.as_secs_f64();

    println!("   Duration: {:.3}s", duration.as_secs_f64());
    println!("   Requests/sec: {:.2}", rps);
    println!("   Success rate: {}/100", successes);

    println!("\n{}", "=".repeat(50));

    Ok(())
}
