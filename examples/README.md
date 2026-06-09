# NTP Time JSON API - Client Examples

This directory contains example client implementations for the NTP Time JSON API in multiple programming languages.

---

## Available Examples

### 1. Python 🐍

**Location**: `python/`

**Files**:
- `client.py` - Synchronous HTTP client using requests
- `client_async.py` - Asynchronous HTTP client using aiohttp
- `websocket_client.py` - WebSocket streaming client using websockets
- `requirements.txt` - Python dependencies

**Setup**:
```bash
cd python
pip install -r requirements.txt
```

**Usage**:
```bash
# Synchronous client
python client.py

# Asynchronous client
python client_async.py

# WebSocket streaming
python websocket_client.py
```

---

### 2. JavaScript/Node.js 📦

**Location**: `javascript/`

**Files**:
- `client.js` - HTTP client using native fetch API
- `websocket_client.js` - WebSocket streaming client using ws library
- `package.json` - Node.js dependencies

**Setup**:
```bash
cd javascript
npm install
```

**Usage**:
```bash
# HTTP client
npm run client
# or: node client.js

# WebSocket client
npm run websocket
# or: node websocket_client.js
```

---

### 3. Rust 🦀

**Location**: `rust/`

**Files**:
- `src/client.rs` - HTTP client using reqwest
- `src/websocket_client.rs` - WebSocket streaming client using tokio-tungstenite
- `Cargo.toml` - Rust dependencies

**Setup**:
```bash
cd rust
```

**Usage**:
```bash
# HTTP client
cargo run --bin client

# WebSocket client
cargo run --bin websocket_client
```

---

## Quick Start

### Prerequisites

1. **Start the NTP Time JSON API service**:
   ```bash
   docker compose up -d
   ```

2. **Verify service is running**:
   ```bash
   curl http://localhost:8080/time
   ```

### Run Examples

**Python**:
```bash
cd examples/python
pip install -r requirements.txt
python client.py
```

**JavaScript**:
```bash
cd examples/javascript
npm install
node client.js
```

**Rust**:
```bash
cd examples/rust
cargo run --bin client
```

---

## Example Features

All examples demonstrate:

### HTTP/REST Endpoints
- ✅ `GET /time` - Get current NTP time
- ✅ `GET /healthz` - Health check
- ✅ `GET /readyz` - Readiness check
- ✅ `GET /performance` - Performance metrics
- ✅ `GET /metrics` - Prometheus metrics

### WebSocket Streaming
- ✅ Real-time time updates
- ✅ Connection management
- ✅ Message parsing (welcome, tick, error)
- ✅ Graceful shutdown

### Advanced Features
- ✅ Error handling
- ✅ Timeouts and retries
- ✅ Benchmarking capabilities
- ✅ Statistics tracking

---

## API Response Format

### `/time` Response
```json
{
  "message": "done",
  "status": 200,
  "data": 1735446000000
}
```

### WebSocket Messages

**Welcome Message**:
```json
{
  "type": "welcome",
  "message": "Connected to NTP Time JSON API WebSocket",
  "update_interval_ms": 1000,
  "max_duration_secs": 3600
}
```

**Time Update (Tick)**:
```json
{
  "type": "tick",
  "epoch_ms": 1735446000000,
  "iso8601": "2024-12-29T00:00:00+00:00",
  "is_stale": false,
  "staleness_secs": 12,
  "message": "done",
  "sequence": 42
}
```

---

## Configuration

All examples support environment variables:

**HTTP Clients**:
- `NTP_API_URL` - Base URL (default: `http://localhost:8080`)

**WebSocket Clients**:
- `NTP_WS_URL` - WebSocket URL (default: `ws://localhost:8080/stream`)

**Example**:
```bash
export NTP_API_URL=http://api.example.com:8080
python client.py
```

---

## Benchmarking

All HTTP client examples include built-in benchmarking:

**Python**:
```python
# Benchmark 100 requests
with NTPTimeClient() as client:
    start = time.time()
    for _ in range(100):
        client.get_time_ms()
    duration = time.time() - start
    print(f"Requests/sec: {100/duration:.2f}")
```

**JavaScript**:
```javascript
// Concurrent benchmark
const promises = Array(100).fill(null).map(() => client.getTimeMs());
const results = await Promise.all(promises);
```

**Rust**:
```rust
// Sequential benchmark
for _ in 0..100 {
    client.get_time_ms().await?;
}
```

---

## Error Handling

All examples implement proper error handling:

**Network Errors**:
- Connection failures
- Timeouts
- HTTP errors

**API Errors**:
- 503 before first sync
- Invalid JSON
- Unexpected responses

**WebSocket Errors**:
- Connection closed
- Invalid messages
- Reconnection logic (in some examples)

---

## Best Practices

### 1. Connection Reuse
✅ **Good** - Reuse HTTP client/session:
```python
with NTPTimeClient() as client:
    for _ in range(100):
        client.get_time()  # Reuses connection
```

❌ **Bad** - Create new client each time:
```python
for _ in range(100):
    client = NTPTimeClient()
    client.get_time()  # New connection overhead
```

### 2. Timeout Configuration
Always set appropriate timeouts:
```python
session = requests.Session()
response = session.get(url, timeout=5)  # 5 second timeout
```

### 3. Error Handling
Handle both network and API errors:
```python
try:
    response = client.get_time()
    if response['status'] == 503:
        print("Service not yet synced")
except requests.exceptions.RequestException as e:
    print(f"Network error: {e}")
```

### 4. WebSocket Reconnection
Implement reconnection logic for production:
```python
while True:
    try:
        await client.connect()
        await client.receive_messages()
    except Exception as e:
        print(f"Error: {e}, reconnecting in 5s...")
        await asyncio.sleep(5)
```

---

## Performance Tips

### HTTP Clients
1. **Reuse connections** - Use session/client pooling
2. **Set timeouts** - Prevent hanging requests
3. **Concurrent requests** - Use async/await or threads
4. **Connection pooling** - Configure max connections

### WebSocket Clients
1. **Buffering** - Handle message bursts
2. **Heartbeat** - Implement ping/pong
3. **Reconnection** - Auto-reconnect on disconnect
4. **Message queue** - Process messages asynchronously

---

## Troubleshooting

### "Connection refused"
- Ensure service is running: `docker compose ps`
- Check port mapping: `docker compose ps | grep 8080`
- Verify firewall: `telnet localhost 8080`

### "Service not yet synchronized"
- Wait for first NTP sync (typically 2-10 seconds)
- Check readiness: `curl http://localhost:8080/readyz`
- Check logs: `docker compose logs -f`

### WebSocket disconnects immediately
- Check service logs for errors
- Verify WebSocket endpoint: `WS /stream`
- Test with websocat: `websocat ws://localhost:8080/stream`

---

## Additional Resources

- **API Documentation**: See `../README.md`
- **Protocol Comparison**: See `../PROTOCOL_COMPARISON.md`
- **Deployment Guide**: See `../k8s/`

---

## Contributing

To add a new language example:

1. Create directory: `examples/<language>/`
2. Implement HTTP client
3. Implement WebSocket client (optional)
4. Add README with setup instructions
5. Add dependency file (requirements.txt, package.json, etc.)
6. Update this README

---

## License

MIT OR Apache-2.0 (same as main project)

---

*Generated: December 29, 2025*
