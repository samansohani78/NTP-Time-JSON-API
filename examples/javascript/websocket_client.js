#!/usr/bin/env node
/**
 * WebSocket streaming client example for NTP Time JSON API
 * Demonstrates real-time time streaming
 */

const WebSocket = require('ws');

const WS_URL = process.env.NTP_WS_URL || 'ws://localhost:8080/stream';

class WebSocketTimeClient {
    constructor(wsUrl = WS_URL) {
        this.wsUrl = wsUrl;
        this.ws = null;
        this.messageCount = 0;
        this.connected = false;
    }

    /**
     * Connect to WebSocket endpoint
     * @returns {Promise<boolean>}
     */
    connect() {
        return new Promise((resolve, reject) => {
            try {
                this.ws = new WebSocket(this.wsUrl);

                this.ws.on('open', () => {
                    this.connected = true;
                    console.log(`✓ Connected to ${this.wsUrl}`);
                    resolve(true);
                });

                this.ws.on('error', (error) => {
                    console.error(`✗ WebSocket error: ${error.message}`);
                    reject(error);
                });

                this.ws.on('close', () => {
                    this.connected = false;
                    console.log('✓ Disconnected');
                });

            } catch (error) {
                console.error(`✗ Connection failed: ${error.message}`);
                reject(error);
            }
        });
    }

    /**
     * Disconnect from WebSocket
     */
    disconnect() {
        if (this.ws) {
            this.ws.close();
            this.connected = false;
        }
    }

    /**
     * Receive messages for specified duration
     * @param {number} duration - Duration in seconds (null = infinite)
     * @param {Function} onMessage - Message handler callback
     * @returns {Promise<void>}
     */
    async receiveMessages(duration = null, onMessage = null) {
        return new Promise((resolve) => {
            const startTime = Date.now();

            this.ws.on('message', (data) => {
                try {
                    const message = JSON.parse(data.toString());
                    this.handleMessage(message, onMessage);
                } catch (error) {
                    console.error(`✗ Invalid JSON: ${data}`);
                }
            });

            // Handle timeout
            if (duration) {
                setTimeout(() => {
                    resolve();
                }, duration * 1000);
            }

            // Handle close
            this.ws.on('close', () => {
                resolve();
            });
        });
    }

    /**
     * Handle incoming message
     * @param {Object} data - Parsed message data
     * @param {Function} callback - Optional callback
     */
    handleMessage(data, callback) {
        const msgType = data.type || 'unknown';

        if (msgType === 'welcome') {
            console.log(`\n📡 ${data.message}`);
            console.log(`   Update interval: ${data.update_interval_ms}ms`);
            console.log(`   Max duration: ${data.max_duration_secs}s\n`);

        } else if (msgType === 'tick') {
            this.messageCount++;

            const epochMs = data.epoch_ms;
            const iso8601 = data.iso8601;
            const isStale = data.is_stale || false;
            const staleness = data.staleness_secs || 0;
            const sequence = data.sequence || 0;

            // Convert to Date
            const date = new Date(epochMs);
            const timeStr = date.toISOString().replace('T', ' ').substring(0, 23);

            const staleIndicator = isStale ? '⚠ STALE' : '✓';
            console.log(`[${sequence.toString().padStart(4, '0')}] ${staleIndicator} ${timeStr} UTC (age: ${staleness}s)`);

        } else if (msgType === 'error') {
            console.log(`✗ Error: ${data.message}`);

        } else {
            console.log(`? Unknown message type: ${msgType}`);
        }

        // Call custom callback if provided
        if (callback) {
            callback(data);
        }
    }

    /**
     * Get client statistics
     * @returns {Object}
     */
    getStats() {
        return {
            messages_received: this.messageCount,
            connected: this.connected
        };
    }
}

// Example usage
async function main() {
    console.log('='.repeat(60));
    console.log('NTP Time JSON API - WebSocket Streaming Client (Node.js)');
    console.log('='.repeat(60));
    console.log('\nConnecting to WebSocket stream...');
    console.log('Press Ctrl+C to stop\n');

    const client = new WebSocketTimeClient();

    try {
        await client.connect();

        // Receive messages for 30 seconds
        await client.receiveMessages(30);

        // Print stats
        const stats = client.getStats();
        console.log(`\n${'='.repeat(60)}`);
        console.log('Session Statistics:');
        console.log(`  Messages received: ${stats.messages_received}`);
        console.log('='.repeat(60) + '\n');

    } catch (error) {
        console.error(`Error: ${error.message}`);
    } finally {
        client.disconnect();
    }
}

// Handle Ctrl+C
process.on('SIGINT', () => {
    console.log('\n⚠ Interrupted by user');
    process.exit(0);
});

// Run if executed directly
if (require.main === module) {
    main().catch(console.error);
}

module.exports = { WebSocketTimeClient };
