#!/bin/bash

echo "=== Time Comparison: API vs System ==="
echo ""
echo "Running 9 comparisons..."
echo ""

for i in 1 2 3 4 5 6 7 8 9; do
    api=$(curl -s http://localhost:8080/time | jq -r '.data')
    sys=$(date +%s%3N)
    diff=$((api - sys))

    printf "Test %d: Diff = %6d ms\n" $i $diff
    sleep 1
done

echo ""
echo "Checking NTP metrics..."
curl -s http://localhost:8080/metrics | grep "ntp_staleness_seconds " | head -1
curl -s http://localhost:8080/metrics | grep "ntp_offset_seconds " | head -1

echo ""
echo "Checking if your system clock is synced with NTP..."
if command -v timedatectl &> /dev/null; then
    timedatectl status | grep -E "(NTP|synchronized)"
fi
