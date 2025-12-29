#!/usr/bin/env python3
"""
Python client example for NTP Time JSON API
Demonstrates synchronous HTTP requests
"""

import requests
import time
from datetime import datetime
from typing import Optional, Dict, Any


class NTPTimeClient:
    """Synchronous client for NTP Time JSON API"""

    def __init__(self, base_url: str = "http://localhost:8080"):
        """
        Initialize the client

        Args:
            base_url: Base URL of the NTP Time API
        """
        self.base_url = base_url.rstrip('/')
        self.session = requests.Session()
        self.session.headers.update({
            'User-Agent': 'NTP-Time-Python-Client/1.0'
        })

    def get_time(self) -> Optional[Dict[str, Any]]:
        """
        Get current NTP time

        Returns:
            Response dict with 'status', 'message', and 'data' (epoch_ms)
            None if request fails
        """
        try:
            response = self.session.get(f"{self.base_url}/time", timeout=5)
            response.raise_for_status()
            return response.json()
        except requests.exceptions.RequestException as e:
            print(f"Error fetching time: {e}")
            return None

    def get_time_ms(self) -> Optional[int]:
        """
        Get current time as epoch milliseconds

        Returns:
            Epoch milliseconds or None if request fails
        """
        result = self.get_time()
        if result and result.get('status') == 200:
            return result.get('data')
        return None

    def get_time_datetime(self) -> Optional[datetime]:
        """
        Get current time as Python datetime object

        Returns:
            datetime object or None if request fails
        """
        epoch_ms = self.get_time_ms()
        if epoch_ms:
            return datetime.fromtimestamp(epoch_ms / 1000.0)
        return None

    def healthz(self) -> bool:
        """Check if service is alive"""
        try:
            response = self.session.get(f"{self.base_url}/healthz", timeout=2)
            return response.status_code == 200
        except:
            return False

    def readyz(self) -> bool:
        """Check if service is ready"""
        try:
            response = self.session.get(f"{self.base_url}/readyz", timeout=2)
            return response.status_code == 200
        except:
            return False

    def get_metrics(self) -> Optional[str]:
        """Get Prometheus metrics"""
        try:
            response = self.session.get(f"{self.base_url}/metrics", timeout=5)
            response.raise_for_status()
            return response.text
        except:
            return None

    def get_performance(self) -> Optional[Dict[str, Any]]:
        """Get performance metrics"""
        try:
            response = self.session.get(f"{self.base_url}/performance", timeout=5)
            response.raise_for_status()
            return response.json()
        except:
            return None

    def close(self):
        """Close the session"""
        self.session.close()

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.close()


def main():
    """Example usage"""
    print("=" * 50)
    print("NTP Time JSON API - Python Client Example")
    print("=" * 50)

    with NTPTimeClient() as client:
        # Check health
        print("\n1. Health check:")
        if client.healthz():
            print("   ✓ Service is alive")
        else:
            print("   ✗ Service is not responding")
            return

        # Check readiness
        print("\n2. Readiness check:")
        if client.readyz():
            print("   ✓ Service is ready")
        else:
            print("   ✗ Service is not ready (may not be synced yet)")

        # Get time (full response)
        print("\n3. Get time (full response):")
        time_data = client.get_time()
        if time_data:
            print(f"   Status: {time_data.get('status')}")
            print(f"   Message: {time_data.get('message')}")
            print(f"   Epoch MS: {time_data.get('data')}")

        # Get time as milliseconds
        print("\n4. Get time (epoch milliseconds):")
        epoch_ms = client.get_time_ms()
        if epoch_ms:
            print(f"   {epoch_ms}")

        # Get time as datetime
        print("\n5. Get time (datetime object):")
        dt = client.get_time_datetime()
        if dt:
            print(f"   {dt.isoformat()}")
            print(f"   UTC: {dt.strftime('%Y-%m-%d %H:%M:%S.%f')[:-3]}")

        # Get performance metrics
        print("\n6. Performance metrics:")
        perf = client.get_performance()
        if perf:
            print(f"   Total requests: {perf.get('total_requests', 0)}")
            print(f"   Success rate: {perf.get('success_requests', 0)}")
            print(f"   Cache hit rate: {perf.get('cache_hit_rate', 0):.2%}")
            print(f"   Avg latency: {perf.get('avg_latency_us', 0):.2f}μs")

        # Benchmark
        print("\n7. Benchmark (100 requests):")
        start = time.time()
        successes = 0
        for _ in range(100):
            if client.get_time_ms():
                successes += 1
        duration = time.time() - start
        print(f"   Duration: {duration:.3f}s")
        print(f"   Requests/sec: {100/duration:.2f}")
        print(f"   Success rate: {successes}/100")

    print("\n" + "=" * 50)


if __name__ == "__main__":
    main()
