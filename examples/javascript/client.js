#!/usr/bin/env node
/**
 * Node.js client example for NTP Time JSON API
 * Demonstrates HTTP requests using native fetch API
 */

const BASE_URL = process.env.NTP_API_URL || 'http://localhost:8080';

class NTPTimeClient {
    constructor(baseUrl = BASE_URL) {
        this.baseUrl = baseUrl.replace(/\/$/, '');
    }

    /**
     * Get current NTP time
     * @returns {Promise<Object>} Response with status, message, and data
     */
    async getTime() {
        try {
            const response = await fetch(`${this.baseUrl}/time`, {
                headers: {
                    'User-Agent': 'NTP-Time-NodeJS-Client/1.0'
                }
            });

            if (!response.ok) {
                throw new Error(`HTTP ${response.status}: ${response.statusText}`);
            }

            return await response.json();
        } catch (error) {
            console.error(`Error fetching time: ${error.message}`);
            return null;
        }
    }

    /**
     * Get time as epoch milliseconds
     * @returns {Promise<number|null>}
     */
    async getTimeMs() {
        const result = await this.getTime();
        if (result && result.status === 200) {
            return result.data;
        }
        return null;
    }

    /**
     * Get time as Date object
     * @returns {Promise<Date|null>}
     */
    async getTimeDate() {
        const epochMs = await this.getTimeMs();
        return epochMs ? new Date(epochMs) : null;
    }

    /**
     * Check if service is alive
     * @returns {Promise<boolean>}
     */
    async healthz() {
        try {
            const response = await fetch(`${this.baseUrl}/healthz`);
            return response.ok;
        } catch {
            return false;
        }
    }

    /**
     * Check if service is ready
     * @returns {Promise<boolean>}
     */
    async readyz() {
        try {
            const response = await fetch(`${this.baseUrl}/readyz`);
            return response.ok;
        } catch {
            return false;
        }
    }

    /**
     * Get performance metrics
     * @returns {Promise<Object|null>}
     */
    async getPerformance() {
        try {
            const response = await fetch(`${this.baseUrl}/performance`);
            if (!response.ok) return null;
            return await response.json();
        } catch {
            return null;
        }
    }

    /**
     * Get Prometheus metrics
     * @returns {Promise<string|null>}
     */
    async getMetrics() {
        try {
            const response = await fetch(`${this.baseUrl}/metrics`);
            if (!response.ok) return null;
            return await response.text();
        } catch {
            return null;
        }
    }
}

// Example usage
async function main() {
    console.log('='.repeat(50));
    console.log('NTP Time JSON API - Node.js Client Example');
    console.log('='.repeat(50));

    const client = new NTPTimeClient();

    // Health check
    console.log('\n1. Health check:');
    const healthy = await client.healthz();
    console.log(healthy ? '   ✓ Service is alive' : '   ✗ Service is not responding');

    if (!healthy) return;

    // Readiness check
    console.log('\n2. Readiness check:');
    const ready = await client.readyz();
    console.log(ready ? '   ✓ Service is ready' : '   ✗ Service is not ready');

    // Get time (full response)
    console.log('\n3. Get time (full response):');
    const timeData = await client.getTime();
    if (timeData) {
        console.log(`   Status: ${timeData.status}`);
        console.log(`   Message: ${timeData.message}`);
        console.log(`   Epoch MS: ${timeData.data}`);
    }

    // Get time as milliseconds
    console.log('\n4. Get time (epoch milliseconds):');
    const epochMs = await client.getTimeMs();
    if (epochMs) {
        console.log(`   ${epochMs}`);
    }

    // Get time as Date
    console.log('\n5. Get time (Date object):');
    const date = await client.getTimeDate();
    if (date) {
        console.log(`   ${date.toISOString()}`);
        console.log(`   UTC: ${date.toUTCString()}`);
    }

    // Performance metrics
    console.log('\n6. Performance metrics:');
    const perf = await client.getPerformance();
    if (perf) {
        console.log(`   Total requests: ${perf.total_requests || 0}`);
        console.log(`   Success rate: ${perf.success_requests || 0}`);
        console.log(`   Cache hit rate: ${((perf.cache_hit_rate || 0) * 100).toFixed(2)}%`);
        console.log(`   Avg latency: ${(perf.avg_latency_us || 0).toFixed(2)}μs`);
    }

    // Benchmark
    console.log('\n7. Benchmark (100 requests):');
    const startTime = Date.now();
    let successes = 0;

    const promises = Array(100).fill(null).map(() => client.getTimeMs());
    const results = await Promise.all(promises);
    successes = results.filter(r => r !== null).length;

    const duration = (Date.now() - startTime) / 1000;
    console.log(`   Duration: ${duration.toFixed(3)}s`);
    console.log(`   Requests/sec: ${(100 / duration).toFixed(2)}`);
    console.log(`   Success rate: ${successes}/100`);

    console.log('\n' + '='.repeat(50));
}

// Run if executed directly
if (require.main === module) {
    main().catch(console.error);
}

module.exports = { NTPTimeClient };
