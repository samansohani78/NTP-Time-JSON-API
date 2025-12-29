#!/usr/bin/env python3
"""Test WebSocket endpoint"""
import asyncio
import json
import sys

try:
    import websockets
except ImportError:
    print("ERROR: websockets module not installed")
    print("Install with: pip install websockets")
    sys.exit(1)

async def test_websocket():
    """Test WebSocket /ws endpoint"""
    uri = "ws://localhost:8080/ws"

    print("Testing WebSocket endpoint...")
    print(f"Connecting to {uri}")

    try:
        async with websockets.connect(uri) as websocket:
            print("✓ Connected successfully")

            # Receive messages for 3 seconds
            messages_received = 0
            timestamps = []

            try:
                for i in range(5):
                    message = await asyncio.wait_for(
                        websocket.recv(),
                        timeout=2.0
                    )
                    messages_received += 1

                    # Parse JSON
                    try:
                        data = json.loads(message)
                        if 'epoch_ms' in data:
                            timestamps.append(data['epoch_ms'])
                            print(f"  Message {i+1}: epoch_ms={data['epoch_ms']}, "
                                  f"iso8601={data.get('iso8601', 'N/A')}")
                        else:
                            print(f"  Message {i+1}: {message}")
                    except json.JSONDecodeError:
                        print(f"  Message {i+1} (not JSON): {message}")

            except asyncio.TimeoutError:
                print("  (Timeout waiting for more messages)")

            print(f"\n✓ Received {messages_received} messages")

            # Check timestamp progression
            if len(timestamps) >= 2:
                all_increasing = all(
                    timestamps[i] < timestamps[i+1]
                    for i in range(len(timestamps)-1)
                )
                if all_increasing:
                    print("✓ Timestamps are increasing")
                    avg_diff = sum(
                        timestamps[i+1] - timestamps[i]
                        for i in range(len(timestamps)-1)
                    ) / (len(timestamps) - 1)
                    print(f"  Average interval: {avg_diff:.0f}ms")
                else:
                    print("✗ FAIL: Timestamps not monotonically increasing")
                    return False

            print("\n✓ WebSocket test PASSED")
            return True

    except Exception as e:
        print(f"✗ FAIL: {type(e).__name__}: {e}")
        return False

if __name__ == "__main__":
    result = asyncio.run(test_websocket())
    sys.exit(0 if result else 1)
