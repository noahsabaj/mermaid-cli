#!/bin/bash

echo "🚀 Testing Cache Performance Improvements"
echo "========================================="
echo ""
echo "System Info:"
echo "CPU Cores: $(nproc)"
echo "Memory: $(free -h | grep Mem | awk '{print $2}')"
echo ""

CACHE_DIR="$HOME/.cache/mermaid"

echo "📊 Cache Statistics Before:"
if [ -d "$CACHE_DIR" ]; then
    echo "Cache directory exists: $CACHE_DIR"
    echo "Cache size: $(du -sh $CACHE_DIR 2>/dev/null | cut -f1)"
    echo "Number of cache files: $(find $CACHE_DIR -type f 2>/dev/null | wc -l)"
else
    echo "No cache directory found (first run)"
fi
echo ""

echo "🔄 First Run (Cold Cache):"
echo "Starting mermaid in the current directory..."
time timeout 5s mermaid 2>&1 | head -5

echo ""
echo "📊 Cache Statistics After First Run:"
if [ -d "$CACHE_DIR" ]; then
    echo "Cache size: $(du -sh $CACHE_DIR 2>/dev/null | cut -f1)"
    echo "Number of cache files: $(find $CACHE_DIR -type f 2>/dev/null | wc -l)"
fi
echo ""

echo "🔥 Second Run (Warm Cache):"
echo "Starting mermaid again (should be faster with cache)..."
time timeout 5s mermaid 2>&1 | head -5

echo ""
echo "✨ Performance Summary:"
echo "- Parallel file loading with $(nproc) cores"
echo "- AST caching with LZ4 compression"
echo "- Token count caching per file"
echo "- Incremental context updates"
echo ""
echo "Cache location: $CACHE_DIR"
echo "To clear cache: rm -rf $CACHE_DIR"