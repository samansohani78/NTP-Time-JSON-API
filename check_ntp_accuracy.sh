#!/bin/bash

echo "═══════════════════════════════════════════════"
echo "    NTP Server Accuracy Analysis"
echo "═══════════════════════════════════════════════"
echo ""

# Check system time accuracy
echo "1. SYSTEM TIME ACCURACY:"
echo "   $(timedatectl status | grep 'synchronized')"
echo "   $(timedatectl status | grep 'NTP service')"
if command -v chronyc &> /dev/null; then
    echo ""
    echo "   Chrony tracking:"
    chronyc tracking 2>/dev/null | grep -E "(Reference|Stratum|Last offset|RMS offset|Frequency|Residual freq)" || echo "   (chrony not available)"
fi
echo ""

# Check API logs for server selection
echo "2. RECENT NTP SERVER SELECTIONS:"
docker logs ntp-time-api 2>&1 | grep "Selected best NTP server" | tail -5 | while read line; do
    echo "   $line" | jq -r '. | "   [\(.timestamp)] Server: \(.fields.server), RTT: \(.fields.rtt_ms)ms, Epoch: \(.fields.epoch_ms)"'
done
echo ""

# Check all tested servers
echo "3. ALL SERVERS TESTED (last sync):"
docker logs ntp-time-api 2>&1 | grep "NTP query successful\|NTP query failed" | tail -20 | while read line; do
    echo "$line" | jq -r 'if .level == "INFO" then "   ✓ \(.fields.server): RTT=\(.fields.rtt_ms)ms" else "   ✗ \(.fields.server): FAILED" end' 2>/dev/null || echo "   $line"
done
echo ""

# Check offset between servers
echo "4. SERVER OFFSET ANALYSIS:"
echo "   Getting detailed metrics..."
curl -s http://localhost:8080/metrics | grep ntp_server_offset_ms | head -15
echo ""

# Compare API time vs system time
echo "5. API vs SYSTEM TIME ACCURACY:"
for i in {1..5}; do
    api_time=$(curl -s http://localhost:8080/time | jq -r '.data')
    sys_time=$(date +%s%3N)
    diff=$((sys_time - api_time))

    if [ ${diff#-} -le 10 ]; then
        status="✓ EXCELLENT"
    elif [ ${diff#-} -le 50 ]; then
        status="✓ GOOD"
    elif [ ${diff#-} -le 100 ]; then
        status="⚠ ACCEPTABLE"
    else
        status="✗ POOR"
    fi

    printf "   Test %d: Diff = %5d ms  %s\n" "$i" "$diff" "$status"
    sleep 0.5
done
echo ""

# Check for server disagreement
echo "6. SERVER HEALTH:"
curl -s http://localhost:8080/metrics | grep -E "ntp_server_up|ntp_consecutive_failures" | grep -v "^#"
echo ""

echo "═══════════════════════════════════════════════"
echo ""
echo "ACCURACY INDICATORS:"
echo "  • System time offset: Should be < 50ms"
echo "  • API vs System diff: Should be < 10ms"
echo "  • Server agreement: Offsets should be within 100ms"
echo "  • RTT: Lower is better for latency, not accuracy"
echo ""
