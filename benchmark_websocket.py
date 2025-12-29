#!/usr/bin/env python3
"""
WebSocket Benchmark for NTP Time JSON API
Tests WebSocket streaming performance and latency
"""

import asyncio
import json
import time
import statistics
from collections import defaultdict
import argparse
import sys

try:
    import websockets
except ImportError:
    print("Error: websockets library not installed")
    print("Install with: pip install websockets")
    sys.exit(1)


class WebSocketBenchmark:
    def __init__(self, url, duration_secs=10, num_connections=1):
        self.url = url
        self.duration_secs = duration_secs
        self.num_connections = num_connections
        self.results = defaultdict(list)

    async def benchmark_connection(self, connection_id):
        """Benchmark a single WebSocket connection"""
        messages_received = 0
        latencies = []
        start_time = time.time()

        try:
            async with websockets.connect(self.url) as websocket:
                connection_time = time.time() - start_time
                self.results['connection_times'].append(connection_time)

                # Receive messages for specified duration
                deadline = time.time() + self.duration_secs

                while time.time() < deadline:
                    try:
                        message_start = time.time()
                        message = await asyncio.wait_for(
                            websocket.recv(),
                            timeout=max(0.1, deadline - time.time())
                        )
                        message_latency = time.time() - message_start

                        data = json.loads(message)
                        messages_received += 1
                        latencies.append(message_latency)

                        # Track message types
                        msg_type = data.get('type', 'unknown')
                        self.results[f'msg_type_{msg_type}'].append(1)

                    except asyncio.TimeoutError:
                        break
                    except json.JSONDecodeError:
                        self.results['json_errors'].append(1)

                self.results['messages_per_connection'].append(messages_received)
                self.results['latencies'].extend(latencies)

        except Exception as e:
            self.results['connection_errors'].append(str(e))
            print(f"Connection {connection_id} error: {e}")

    async def run(self):
        """Run benchmark with multiple concurrent connections"""
        print("=" * 50)
        print("WebSocket Benchmark - NTP Time JSON API")
        print("=" * 50)
        print(f"URL:              {self.url}")
        print(f"Duration:         {self.duration_secs}s")
        print(f"Connections:      {self.num_connections}")
        print("=" * 50)
        print("Running benchmark...\n")

        # Start all connections concurrently
        tasks = [
            self.benchmark_connection(i)
            for i in range(self.num_connections)
        ]
        await asyncio.gather(*tasks)

        self.print_results()

    def print_results(self):
        """Print benchmark results"""
        print("=" * 50)
        print("Results")
        print("=" * 50)

        # Connection statistics
        conn_times = self.results['connection_times']
        if conn_times:
            print(f"\nConnection Times:")
            print(f"  Successful:      {len(conn_times)}")
            print(f"  Min:             {min(conn_times):.6f}s")
            print(f"  Avg:             {statistics.mean(conn_times):.6f}s")
            print(f"  Max:             {max(conn_times):.6f}s")

        # Connection errors
        conn_errors = self.results['connection_errors']
        if conn_errors:
            print(f"  Errors:          {len(conn_errors)}")

        # Message statistics
        messages = self.results['messages_per_connection']
        if messages:
            total_messages = sum(messages)
            print(f"\nMessages Received:")
            print(f"  Total:           {total_messages}")
            print(f"  Per connection:  {statistics.mean(messages):.1f}")
            print(f"  Rate:            {total_messages / self.duration_secs:.1f} msg/s")

        # Message types
        tick_count = sum(self.results.get('msg_type_tick', []))
        welcome_count = sum(self.results.get('msg_type_welcome', []))
        error_count = sum(self.results.get('msg_type_error', []))

        print(f"\nMessage Types:")
        print(f"  Welcome:         {welcome_count}")
        print(f"  Tick:            {tick_count}")
        print(f"  Error:           {error_count}")

        # Latency statistics
        latencies = self.results['latencies']
        if latencies:
            latencies_ms = [lat * 1000 for lat in latencies]
            latencies_sorted = sorted(latencies_ms)

            print(f"\nMessage Latency (ms):")
            print(f"  Min:             {min(latencies_ms):.3f}")
            print(f"  Avg:             {statistics.mean(latencies_ms):.3f}")
            print(f"  Median:          {statistics.median(latencies_ms):.3f}")
            print(f"  P95:             {latencies_sorted[int(len(latencies_sorted) * 0.95)]:.3f}")
            print(f"  P99:             {latencies_sorted[int(len(latencies_sorted) * 0.99)]:.3f}")
            print(f"  Max:             {max(latencies_ms):.3f}")

        # JSON errors
        json_errors = self.results.get('json_errors', [])
        if json_errors:
            print(f"\nJSON Parse Errors: {len(json_errors)}")

        print("=" * 50)
        print()


def main():
    parser = argparse.ArgumentParser(
        description='Benchmark WebSocket endpoint of NTP Time JSON API'
    )
    parser.add_argument(
        '--url',
        default='ws://localhost:8080/stream',
        help='WebSocket URL to benchmark (default: ws://localhost:8080/stream)'
    )
    parser.add_argument(
        '--duration',
        type=int,
        default=10,
        help='Duration of benchmark in seconds (default: 10)'
    )
    parser.add_argument(
        '--connections',
        type=int,
        default=1,
        help='Number of concurrent connections (default: 1)'
    )

    args = parser.parse_args()

    benchmark = WebSocketBenchmark(
        url=args.url,
        duration_secs=args.duration,
        num_connections=args.connections
    )

    asyncio.run(benchmark.run())


if __name__ == '__main__':
    main()
