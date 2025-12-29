#!/usr/bin/env python3
"""
WebSocket streaming client example for NTP Time JSON API
Demonstrates real-time time streaming
"""

import asyncio
import websockets
import json
import signal
from datetime import datetime
from typing import Optional


class WebSocketTimeClient:
    """WebSocket client for real-time time streaming"""

    def __init__(self, ws_url: str = "ws://localhost:8080/stream"):
        """
        Initialize WebSocket client

        Args:
            ws_url: WebSocket URL of the streaming endpoint
        """
        self.ws_url = ws_url
        self.websocket: Optional[websockets.WebSocketClientProtocol] = None
        self.running = False
        self.message_count = 0

    async def connect(self):
        """Connect to WebSocket endpoint"""
        try:
            self.websocket = await websockets.connect(self.ws_url)
            self.running = True
            print(f"✓ Connected to {self.ws_url}")
            return True
        except Exception as e:
            print(f"✗ Connection failed: {e}")
            return False

    async def disconnect(self):
        """Disconnect from WebSocket"""
        self.running = False
        if self.websocket:
            await self.websocket.close()
            print("✓ Disconnected")

    async def receive_messages(self, duration: Optional[int] = None):
        """
        Receive and process messages

        Args:
            duration: Optional duration in seconds (None = infinite)
        """
        start_time = asyncio.get_event_loop().time()

        try:
            while self.running:
                if duration and (asyncio.get_event_loop().time() - start_time) >= duration:
                    break

                try:
                    message = await asyncio.wait_for(self.websocket.recv(), timeout=1.0)
                    await self.handle_message(message)
                except asyncio.TimeoutError:
                    continue
                except websockets.exceptions.ConnectionClosed:
                    print("✗ Connection closed by server")
                    break

        except KeyboardInterrupt:
            print("\n⚠ Interrupted by user")

    async def handle_message(self, message: str):
        """Process received message"""
        try:
            data = json.loads(message)
            msg_type = data.get('type', 'unknown')

            if msg_type == 'welcome':
                print(f"\n📡 {data.get('message')}")
                print(f"   Update interval: {data.get('update_interval_ms')}ms")
                print(f"   Max duration: {data.get('max_duration_secs')}s")
                print()

            elif msg_type == 'tick':
                self.message_count += 1
                epoch_ms = data.get('epoch_ms')
                iso8601 = data.get('iso8601')
                is_stale = data.get('is_stale', False)
                staleness = data.get('staleness_secs', 0)
                sequence = data.get('sequence', 0)

                # Convert epoch to datetime
                if epoch_ms:
                    dt = datetime.fromtimestamp(epoch_ms / 1000.0)

                    stale_indicator = "⚠ STALE" if is_stale else "✓"
                    print(f"[{sequence:04d}] {stale_indicator} {dt.strftime('%Y-%m-%d %H:%M:%S.%f')[:-3]} UTC (age: {staleness}s)")

            elif msg_type == 'error':
                print(f"✗ Error: {data.get('message')}")

            else:
                print(f"? Unknown message type: {msg_type}")

        except json.JSONDecodeError:
            print(f"✗ Invalid JSON: {message}")

    def get_stats(self) -> dict:
        """Get client statistics"""
        return {
            'messages_received': self.message_count,
            'connected': self.running
        }


async def main():
    """Example usage"""
    print("=" * 60)
    print("NTP Time JSON API - WebSocket Streaming Client")
    print("=" * 60)
    print("\nConnecting to WebSocket stream...")
    print("Press Ctrl+C to stop\n")

    client = WebSocketTimeClient()

    if await client.connect():
        try:
            # Receive messages for 30 seconds (or until Ctrl+C)
            await client.receive_messages(duration=30)
        finally:
            stats = client.get_stats()
            print(f"\n{'=' * 60}")
            print(f"Session Statistics:")
            print(f"  Messages received: {stats['messages_received']}")
            print(f"{'=' * 60}\n")

            await client.disconnect()


async def continuous_monitoring():
    """Example: Continuous monitoring with reconnection"""
    print("=" * 60)
    print("Continuous WebSocket Monitor (with auto-reconnect)")
    print("=" * 60)
    print("Press Ctrl+C to stop\n")

    client = WebSocketTimeClient()
    reconnect_delay = 5

    while True:
        try:
            if await client.connect():
                await client.receive_messages()
        except KeyboardInterrupt:
            print("\n⚠ Stopping monitor...")
            break
        except Exception as e:
            print(f"✗ Error: {e}")

        if not client.running:
            print(f"⟳ Reconnecting in {reconnect_delay}s...")
            await asyncio.sleep(reconnect_delay)

    await client.disconnect()


if __name__ == "__main__":
    # Run basic example (30 seconds)
    asyncio.run(main())

    # Or run continuous monitoring:
    # asyncio.run(continuous_monitoring())
