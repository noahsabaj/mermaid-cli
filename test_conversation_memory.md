# Conversation Memory Test Plan

## Test 1: Basic Memory Test
1. Start mermaid
2. Send: "Remember the number 42 for me"
3. Wait for response
4. Send: "What number did I ask you to remember?"
5. Expected: Model should recall the number 42

## Test 2: Context Continuation
1. Start mermaid
2. Send: "Let's write a function to calculate fibonacci"
3. Wait for response with code
4. Send: "Now can you add memoization to the function above?"
5. Expected: Model should reference the previously written fibonacci function

## Test 3: Multiple Turns
1. Start mermaid
2. Send: "My name is Alice"
3. Wait for response
4. Send: "I like programming in Rust"
5. Wait for response
6. Send: "What's my name and what language do I like?"
7. Expected: Model should recall both Alice and Rust

## Manual Testing Instructions:
```bash
# Start mermaid with a model that has good memory
mermaid --model ollama/llama3.2:3b

# Or with OpenAI
mermaid --model openai/gpt-4o-mini
```

## What to Look For:
- The AI should maintain context across messages
- Previous messages should influence responses
- The AI should be able to refer to earlier parts of the conversation
- File operations and code should build on previous context