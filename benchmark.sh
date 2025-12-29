#!/bin/bash
# Simple benchmark script for NTP Time JSON API

set -e

URL="${1:-http://localhost:8080/time}"
REQUESTS="${2:-1000}"
CONCURRENCY="${3:-10}"

echo "==================================="
echo "NTP Time JSON API Benchmark"
echo "==================================="
echo "URL: $URL"
echo "Total Requests: $REQUESTS"
echo "Concurrency: $CONCURRENCY"
echo "==================================="

# Create temporary directory for results
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Function to run requests in parallel
run_requests() {
    local worker_id=$1
    local requests_per_worker=$2
    local output_file="$TEMP_DIR/worker_${worker_id}.txt"

    for ((i=0; i<requests_per_worker; i++)); do
        curl -s -w "%{time_total}\n" -o /dev/null "$URL" >> "$output_file"
    done
}

# Calculate requests per worker
REQUESTS_PER_WORKER=$((REQUESTS / CONCURRENCY))

# Start timer
START_TIME=$(date +%s.%N)

# Launch parallel workers
echo "Running benchmark..."
for ((i=0; i<CONCURRENCY; i++)); do
    run_requests $i $REQUESTS_PER_WORKER &
done

# Wait for all workers to complete
wait

# End timer
END_TIME=$(date +%s.%N)

# Calculate total time
TOTAL_TIME=$(echo "$END_TIME - $START_TIME" | bc)

# Collect all response times
cat $TEMP_DIR/worker_*.txt > $TEMP_DIR/all_times.txt

# Calculate statistics
TOTAL_REQUESTS=$(wc -l < $TEMP_DIR/all_times.txt)
AVG_TIME=$(awk '{sum+=$1} END {printf "%.6f", sum/NR}' $TEMP_DIR/all_times.txt)
MIN_TIME=$(sort -n $TEMP_DIR/all_times.txt | head -1)
MAX_TIME=$(sort -n $TEMP_DIR/all_times.txt | tail -1)

# Calculate requests per second
RPS=$(echo "scale=2; $TOTAL_REQUESTS / $TOTAL_TIME" | bc)

# Calculate percentiles
P50=$(awk '{print $1}' $TEMP_DIR/all_times.txt | sort -n | awk -v p=50 'BEGIN{c=0} {a[c++]=$1} END{print a[int(c*p/100)]}')
P95=$(awk '{print $1}' $TEMP_DIR/all_times.txt | sort -n | awk -v p=95 'BEGIN{c=0} {a[c++]=$1} END{print a[int(c*p/100)]}')
P99=$(awk '{print $1}' $TEMP_DIR/all_times.txt | sort -n | awk -v p=99 'BEGIN{c=0} {a[c++]=$1} END{print a[int(c*p/100)]}')

echo ""
echo "==================================="
echo "Results"
echo "==================================="
echo "Total Requests:    $TOTAL_REQUESTS"
echo "Total Time:        ${TOTAL_TIME}s"
echo "Requests/sec:      $RPS"
echo ""
echo "Response Times (seconds):"
echo "  Min:             $MIN_TIME"
echo "  Avg:             $AVG_TIME"
echo "  Max:             $MAX_TIME"
echo "  P50:             $P50"
echo "  P95:             $P95"
echo "  P99:             $P99"
echo "==================================="

# Test one response to verify JSON
echo ""
echo "Sample Response:"
curl -s "$URL" | jq . 2>/dev/null || echo "Warning: Response is not valid JSON"
echo ""
