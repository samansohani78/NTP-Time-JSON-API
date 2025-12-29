#!/bin/bash

echo "=== Testing Different NTP Servers ==="
echo ""

servers=(
  "time.google.com"
  "time.cloudflare.com"
  "pool.ntp.org"
  "ntp.iranet.ir"
  "ntp.day.ir"
)

for server in "${servers[@]}"; do
  echo "Testing $server..."
  # Use sntp command if available
  if command -v sntp &> /dev/null; then
    sntp -t 2 "$server" 2>&1 | head -5
  elif command -v ntpdate &> /dev/null; then
    ntpdate -q "$server" 2>&1 | head -3
  else
    echo "  No NTP client tool available (sntp/ntpdate)"
  fi
  echo ""
done

echo "Current system time:"
date
echo "System epoch: $(date +%s)"
