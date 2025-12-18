#!/bin/bash

# Test script for Mermaid non-interactive mode

echo "Testing Mermaid Non-Interactive Mode"
echo "====================================="
echo

echo "Test 1: Simple text output"
echo "Command: mermaid --prompt 'What is 2+2?' --output-format text"
mermaid --prompt "What is 2+2?" --output-format text 2>&1 | head -5
echo

echo "Test 2: JSON output"
echo "Command: mermaid --prompt 'Say hello' --output-format json"
mermaid --prompt "Say hello" --output-format json 2>&1 | head -10
echo

echo "Test 3: Markdown output"
echo "Command: mermaid --prompt 'Write hello world in Python' --output-format markdown"
mermaid --prompt "Write hello world in Python" --output-format markdown 2>&1 | head -15
echo

echo "Test 4: With specific model"
echo "Command: mermaid --prompt 'Hi' --model ollama/tinyllama --output-format text"
mermaid --prompt "Hi" --model ollama/tinyllama --output-format text 2>&1 | head -5
echo

echo "Test 5: Error handling (invalid model)"
echo "Command: mermaid --prompt 'Test' --model invalid/model --output-format json"
mermaid --prompt "Test" --model invalid/model --output-format json 2>&1 | grep -A5 "errors"
echo

echo "Tests complete!"