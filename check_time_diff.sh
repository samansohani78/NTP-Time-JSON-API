#!/bin/bash

echo "=== Checking Time Difference ==="
echo ""

# Get API time
api_time=$(curl -s http://localhost:8080/time | jq -r '.data')
api_human=$(date -d @$((api_time/1000)) '+%Y-%m-%d %H:%M:%S')

echo "API Time:"
echo "  Epoch (ms): $api_time"
echo "  Human:      $api_human"
echo ""

# Get system time
sys_time=$(date +%s%3N)
sys_human=$(date '+%Y-%m-%d %H:%M:%S')

echo "System Time:"
echo "  Epoch (ms): $sys_time"
echo "  Human:      $sys_human"
echo ""

# Calculate difference
diff=$((api_time - sys_time))
diff_abs=${diff#-}
diff_sec=$((diff/1000))
diff_ms=$((diff%1000))

echo "Difference:"
echo "  ${diff}ms (API - System)"
echo "  ${diff_sec}s ${diff_ms}ms"
echo ""

# Check staleness
echo "Checking staleness..."
staleness=$(curl -s http://localhost:8080/metrics | grep "ntp_staleness_seconds" | awk '{print $2}')
echo "  NTP Staleness: ${staleness}s"
echo ""

# Check last sync time
last_sync=$(curl -s http://localhost:8080/metrics | grep "ntp_last_sync_timestamp_seconds" | awk '{print $2}')
if [ ! -z "$last_sync" ]; then
    current_unix=$(date +%s)
    time_since_sync=$((current_unix - last_sync))
    echo "  Last NTP Sync: ${time_since_sync}s ago"
fi
echo ""

# Check offset bias configuration
echo "Checking configuration..."
echo "  Looking for OFFSET_BIAS_MS in docker-compose.yml..."
offset_bias=$(grep "OFFSET_BIAS_MS" docker-compose.yml | grep -v "#" | awk -F'=' '{print $2}')
echo "  OFFSET_BIAS_MS: ${offset_bias:-0}"
