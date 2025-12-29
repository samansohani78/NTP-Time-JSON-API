#!/bin/bash
# Test NTP Failure Resilience
# This blocks ONLY NTP (UDP 123) while keeping HTTP API accessible

set -e

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "======================================================"
echo "  NTP Failure Resilience Test"
echo "======================================================"
echo ""

# Function to cleanup on exit
cleanup() {
    echo ""
    echo "${YELLOW}=== Cleanup: Restoring NTP access ===${NC}"
    sudo iptables -D OUTPUT -p udp --dport 123 -j DROP 2>/dev/null || true
    echo "${GREEN}✓ NTP access restored${NC}"
}

trap cleanup EXIT

# Step 1: Verify API is working
echo "${YELLOW}[1/7] Verifying API is accessible...${NC}"
if ! curl -s --connect-timeout 3 http://localhost:8080/time >/dev/null 2>&1; then
    echo "${RED}✗ API is not accessible! Check if container is running.${NC}"
    echo "Run: docker compose up -d"
    exit 1
fi
echo "${GREEN}✓ API is accessible${NC}"
echo ""

# Step 2: Get current time
echo "${YELLOW}[2/7] Getting current time (before NTP failure)...${NC}"
TIME_BEFORE=$(curl -s http://localhost:8080/time | python3 -c "import sys,json; print(json.load(sys.stdin)['data'])")
echo "   Time: ${TIME_BEFORE} ms"
echo "${GREEN}✓ Current time retrieved${NC}"
echo ""

# Step 3: Check current NTP status
echo "${YELLOW}[3/7] Checking current NTP status...${NC}"
FAILURES_BEFORE=$(curl -s http://localhost:8080/metrics | grep "ntp_consecutive_failures" | awk '{print $2}')
echo "   Consecutive failures: ${FAILURES_BEFORE:-0}"
echo "${GREEN}✓ NTP status checked${NC}"
echo ""

# Step 4: Block NTP traffic (but NOT HTTP!)
echo "${YELLOW}[4/7] Blocking NTP traffic (UDP port 123 only)...${NC}"
echo "   ⚠ This will cause NTP sync to fail"
echo "   ℹ HTTP API (port 8080) remains accessible"
sudo iptables -A OUTPUT -p udp --dport 123 -j DROP
echo "${GREEN}✓ NTP traffic blocked${NC}"
echo ""

# Step 5: Wait for NTP sync to fail
echo "${YELLOW}[5/7] Waiting 35 seconds for NTP sync to fail...${NC}"
for i in {35..1}; do
    printf "\r   Waiting... %2d seconds remaining" $i
    sleep 1
done
echo ""
echo "${GREEN}✓ Wait complete${NC}"
echo ""

# Step 6: Verify API still works
echo "${YELLOW}[6/7] Testing API after NTP failure...${NC}"
API_RESPONSE=$(curl -s http://localhost:8080/time)
TIME_AFTER=$(echo "$API_RESPONSE" | python3 -c "import sys,json; print(json.load(sys.stdin)['data'])")
STATUS=$(echo "$API_RESPONSE" | python3 -c "import sys,json; print(json.load(sys.stdin)['status'])")
MESSAGE=$(echo "$API_RESPONSE" | python3 -c "import sys,json; print(json.load(sys.stdin)['message'])")

if [ "$STATUS" == "200" ]; then
    echo "${GREEN}✓ API returned 200 OK!${NC}"
    echo "   Time: ${TIME_AFTER} ms"
    echo "   Message: ${MESSAGE}"

    # Calculate time difference
    TIME_DIFF=$((TIME_AFTER - TIME_BEFORE))
    echo "   Time progressed: ${TIME_DIFF} ms (~$((TIME_DIFF/1000)) seconds)"
else
    echo "${RED}✗ API returned status ${STATUS}${NC}"
    echo "   Message: ${MESSAGE}"
fi
echo ""

# Step 7: Check logs for warnings
echo "${YELLOW}[7/7] Checking logs for NTP failure warnings...${NC}"
WARNINGS=$(docker logs ntp-time-api 2>&1 | grep "NTP sync failed; serving from cache" | tail -2)
if [ ! -z "$WARNINGS" ]; then
    echo "${GREEN}✓ Found NTP failure warnings in logs:${NC}"
    echo "$WARNINGS" | while read line; do
        echo "   $line"
    done
else
    echo "${YELLOW}⚠ No warnings found yet (may need to wait longer)${NC}"
fi
echo ""

# Final results
echo "======================================================"
echo "  Test Results"
echo "======================================================"

if [ "$STATUS" == "200" ]; then
    echo "${GREEN}✓ SUCCESS: Service continues working despite NTP failure!${NC}"
    echo ""
    echo "Summary:"
    echo "  • API Status: 200 OK"
    echo "  • Time Before: ${TIME_BEFORE} ms"
    echo "  • Time After:  ${TIME_AFTER} ms"
    echo "  • Difference:  ${TIME_DIFF} ms (~$((TIME_DIFF/1000))s)"
    echo "  • Service:     ${GREEN}RESILIENT${NC}"
    echo ""
    echo "The service is using the monotonic clock to continue"
    echo "serving accurate time even when all NTP servers fail."
else
    echo "${RED}✗ FAILED: Service stopped working${NC}"
    echo "  Status: ${STATUS}"
    echo "  Message: ${MESSAGE}"
fi

echo ""
echo "NTP access will be restored in 3 seconds..."
sleep 3
