#!/usr/bin/env python3
"""
Async Python client example for NTP Time JSON API
Demonstrates asynchronous HTTP requests using aiohttp
"""

import asyncio
import aiohttp
from datetime import datetime
from typing import Optional, Dict, Any


class AsyncNTPTimeClient:
    """Asynchronous client for NTP Time JSON API"""

    def __init__(self, base_url: str = "http://localhost:8080"):
        """
        Initialize the async client

        Args:
            base_url: Base URL of the NTP Time API
        """
        self.base_url = base_url.rstrip('/')
        self.session: Optional[aiohttp.ClientSession] = None

    async def __aenter__(self):
        self.session = aiohttp.ClientSession(
            headers={'User-Agent': 'NTP-Time-Python-Async-Client/1.0'}
        )
        return self

    async def __aexit__(self, exc_type, exc_val, exc_tb):
        if self.session:
            await self.session.close()

    async def get_time(self) -> Optional[Dict[str, Any]]:
        """Get current NTP time"""
        try:
            async with self.session.get(f"{self.base_url}/time", timeout=aiohttp.ClientTimeout(total=5)) as response:
                response.raise_for_status()
                return await response.json()
        except aiohttp.ClientError as e:
            print(f"Error fetching time: {e}")
            return None

    async def get_time_ms(self) -> Optional[int]:
        """Get current time as epoch milliseconds"""
        result = await self.get_time()
        if result and result.get('status') == 200:
            return result.get('data')
        return None

    async def get_time_datetime(self) -> Optional[datetime]:
        """Get current time as Python datetime object"""
        epoch_ms = await self.get_time_ms()
        if epoch_ms:
            return datetime.fromtimestamp(epoch_ms / 1000.0)
        return None

    async def healthz(self) -> bool:
        """Check if service is alive"""
        try:
            async with self.session.get(f"{self.base_url}/healthz", timeout=aiohttp.ClientTimeout(total=2)) as response:
                return response.status == 200
        except:
            return False

    async def readyz(self) -> bool:
        """Check if service is ready"""
        try:
            async with self.session.get(f"{self.base_url}/readyz", timeout=aiohttp.ClientTimeout(total=2)) as response:
                return response.status == 200
        except:
            return False

    async def get_performance(self) -> Optional[Dict[str, Any]]:
        """Get performance metrics"""
        try:
            async with self.session.get(f"{self.base_url}/performance", timeout=aiohttp.ClientTimeout(total=5)) as response:
                response.raise_for_status()
                return await response.json()
        except:
            return None

    async def get_many(self, count: int) -> list:
        """Fetch time concurrently multiple times"""
        tasks = [self.get_time_ms() for _ in range(count)]
        return await asyncio.gather(*tasks)


async def main():
    """Example usage"""
    print("=" * 50)
    print("NTP Time JSON API - Async Python Client Example")
    print("=" * 50)

    async with AsyncNTPTimeClient() as client:
        # Check health
        print("\n1. Health check:")
        if await client.healthz():
            print("   ✓ Service is alive")
        else:
            print("   ✗ Service is not responding")
            return

        # Check readiness
        print("\n2. Readiness check:")
        if await client.readyz():
            print("   ✓ Service is ready")
        else:
            print("   ✗ Service is not ready")

        # Get time
        print("\n3. Get time:")
        time_data = await client.get_time()
        if time_data:
            print(f"   Status: {time_data.get('status')}")
            print(f"   Epoch MS: {time_data.get('data')}")

        # Get time as datetime
        print("\n4. Get time (datetime):")
        dt = await client.get_time_datetime()
        if dt:
            print(f"   {dt.isoformat()}")

        # Concurrent requests benchmark
        print("\n5. Concurrent benchmark (100 requests):")
        import time
        start = time.time()
        results = await client.get_many(100)
        duration = time.time() - start
        successes = sum(1 for r in results if r is not None)
        print(f"   Duration: {duration:.3f}s")
        print(f"   Requests/sec: {100/duration:.2f}")
        print(f"   Success rate: {successes}/100")

        # Performance metrics
        print("\n6. Performance metrics:")
        perf = await client.get_performance()
        if perf:
            print(f"   Total requests: {perf.get('total_requests', 0)}")
            print(f"   Cache hit rate: {perf.get('cache_hit_rate', 0):.2%}")
            print(f"   Avg latency: {perf.get('avg_latency_us', 0):.2f}μs")

    print("\n" + "=" * 50)


if __name__ == "__main__":
    asyncio.run(main())
