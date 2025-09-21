#!/bin/bash

echo "Testing Mermaid startup performance..."
echo "======================================="
echo ""

# Test 1: Time until first output (with proxy already running)
echo "Test 1: Startup with proxy already running"
echo -n "Starting... "
START=$(date +%s.%N)
timeout 0.5s mermaid --no-auto-install 2>&1 | head -1 &
PID=$!
wait $PID 2>/dev/null
END=$(date +%s.%N)
DIFF=$(echo "$END - $START" | bc)
echo "Time to first output: ${DIFF}s"

echo ""
echo "Test 2: Full startup sequence (checking all components)"
echo "Starting with progress indicators..."
( echo -e "\033[?25l"; timeout 2s mermaid --no-auto-install 2>&1 | grep -E "^\[|→|🧜‍♀️" | head -20; echo -e "\033[?25h" )

echo ""
echo "======================================="
echo "Optimization Results:"
echo "- Lazy loading: Files load in background after UI starts"
echo "- Async operations: Ollama/proxy checks run in parallel"
echo "- Smart polling: Proxy checks every 100ms instead of 3s wait"
echo "- Reduced timeouts: Health check reduced from 2s to 200ms"