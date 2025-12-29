#!/bin/bash
set -e

echo "============================================"
echo "Comprehensive API Testing"
echo "============================================"
echo ""

PASSED=0
FAILED=0

# Helper function
test_endpoint() {
    local name="$1"
    local url="$2"
    local expected_status="$3"
    local check_cmd="$4"

    echo "Testing: $name"
    response=$(curl -s -w "\n%{http_code}" "$url")
    status=$(echo "$response" | tail -n1)
    body=$(echo "$response" | head -n-1)

    if [ "$status" = "$expected_status" ]; then
        if [ -z "$check_cmd" ] || eval "$check_cmd"; then
            echo "  ✓ PASS"
            PASSED=$((PASSED + 1))
        else
            echo "  ✗ FAIL: Check failed"
            echo "  Body: $body"
            FAILED=$((FAILED + 1))
        fi
    else
        echo "  ✗ FAIL: Expected status $expected_status, got $status"
        echo "  Body: $body"
        FAILED=$((FAILED + 1))
    fi
    echo ""
}

# Test 1: /healthz
test_endpoint "/healthz - Liveness probe" \
    "http://localhost:8080/healthz" \
    "200" \
    'echo "$body" | jq -e ".status == \"ok\"" > /dev/null'

# Test 2: /readyz
test_endpoint "/readyz - Readiness probe" \
    "http://localhost:8080/readyz" \
    "200" \
    'echo "$body" | jq -e ".status == \"ready\"" > /dev/null'

# Test 3: /startupz
test_endpoint "/startupz - Startup probe" \
    "http://localhost:8080/startupz" \
    "200" \
    'echo "$body" | jq -e ".status == \"ready\"" > /dev/null'

# Test 4: /time - Basic response
test_endpoint "/time - Basic response" \
    "http://localhost:8080/time" \
    "200" \
    'echo "$body" | jq -e ".status == 200 and .data > 0" > /dev/null'

# Test 5: /metrics - Prometheus metrics
echo "Testing: /metrics - Prometheus format"
metrics=$(curl -s http://localhost:8080/metrics)
if echo "$metrics" | grep -q "http_requests_total" && \
   echo "$metrics" | grep -q "ntp_server_up" && \
   echo "$metrics" | grep -q "build_info"; then
    echo "  ✓ PASS"
    PASSED=$((PASSED + 1))
else
    echo "  ✗ FAIL: Missing expected metrics"
    FAILED=$((FAILED + 1))
fi
echo ""

# Test 6: /performance
test_endpoint "/performance - Performance metrics" \
    "http://localhost:8080/performance" \
    "200" \
    'echo "$body" | jq -e ".status == \"ok\" and .metrics" > /dev/null'

# Test 7: Time advancement
echo "Testing: /time - Time advancement"
t1=$(curl -s http://localhost:8080/time | jq -r '.data')
sleep 1
t2=$(curl -s http://localhost:8080/time | jq -r '.data')
diff=$((t2 - t1))
if [ $diff -ge 900 ] && [ $diff -le 1100 ]; then
    echo "  ✓ PASS (diff: ${diff}ms)"
    PASSED=$((PASSED + 1))
else
    echo "  ✗ FAIL: Time diff is ${diff}ms, expected ~1000ms"
    FAILED=$((FAILED + 1))
fi
echo ""

# Test 8: Monotonic time (never goes backward)
echo "Testing: /time - Monotonic progression"
prev=0
monotonic_pass=true
for i in {1..10}; do
    curr=$(curl -s http://localhost:8080/time | jq -r '.data')
    if [ $prev -gt 0 ] && [ $curr -le $prev ]; then
        echo "  ✗ FAIL: Time went backward ($prev -> $curr)"
        monotonic_pass=false
        break
    fi
    prev=$curr
    sleep 0.1
done
if [ "$monotonic_pass" = true ]; then
    echo "  ✓ PASS"
    PASSED=$((PASSED + 1))
else
    FAILED=$((FAILED + 1))
fi
echo ""

# Test 9: Response format
echo "Testing: /time - Response format"
response=$(curl -s http://localhost:8080/time)
if echo "$response" | jq -e 'has("data") and has("message") and has("status")' > /dev/null; then
    echo "  ✓ PASS"
    PASSED=$((PASSED + 1))
else
    echo "  ✗ FAIL: Missing required fields"
    echo "  Response: $response"
    FAILED=$((FAILED + 1))
fi
echo ""

# Test 10: Invalid endpoint
echo "Testing: Invalid endpoint (404)"
status=$(curl -s -w "%{http_code}" -o /dev/null http://localhost:8080/invalid)
if [ "$status" = "404" ]; then
    echo "  ✓ PASS"
    PASSED=$((PASSED + 1))
else
    echo "  ✗ FAIL: Expected 404, got $status"
    FAILED=$((FAILED + 1))
fi
echo ""

# Summary
echo "============================================"
echo "Test Summary"
echo "============================================"
echo "Passed: $PASSED"
echo "Failed: $FAILED"
echo "Total:  $((PASSED + FAILED))"
echo ""

if [ $FAILED -eq 0 ]; then
    echo "✓ ALL TESTS PASSED"
    exit 0
else
    echo "✗ SOME TESTS FAILED"
    exit 1
fi
