# Adding a model provider

Mermaid speaks five provider shapes today: Ollama, Anthropic, Gemini, Meta Responses, and the OpenAI-compatible long tail. Adding another — whether because a new vendor ships a bespoke API or because your self-hosted model uses an untyped variant — means one file + one case in `ProviderFactory`.

## 1. Implement `ModelProvider`

```rust
// src/providers/model/my_provider.rs
use async_trait::async_trait;

use crate::domain::ChatRequest;
use crate::models::Result;
use crate::providers::capabilities::Capabilities;
use crate::providers::ctx::{FinalResponse, StreamContext};
use crate::providers::model::ModelProvider;

pub struct MyProvider {
    // HTTP client, base URL, api key, model name, capabilities …
    capabilities: Capabilities,
}

#[async_trait]
impl ModelProvider for MyProvider {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn chat(
        &self,
        request: ChatRequest,
        ctx: StreamContext,
    ) -> Result<FinalResponse> {
        // 1. Translate request.messages into the provider's wire
        //    shape. Handle request.reasoning, request.tools, etc.
        // 2. POST to the streaming endpoint.
        // 3. Parse chunks, emit ctx.sink.send(StreamEvent::Text / Reasoning /
        //    ToolCall) as you go.
        // 4. MUST select! on ctx.token.cancelled() inside any await
        //    that could block meaningfully — it's the contract that
        //    replaces v0.6's check_interrupt polling.
        // 5. Send StreamEvent::Done { usage, thinking_signature } at
        //    the end. The reducer transitions out of Generating on
        //    this event.
        // 6. Return FinalResponse for subagents / logging. Full_text
        //    and full_thinking are only consumed by nested reducers.
        todo!()
    }
}
```

If your provider's wire format is close enough to an existing one, save work by delegating. Three of the four current providers are thin wrappers over the adapters that already speak the wire format correctly — see `src/providers/model/anthropic.rs` for a delegating wrapper.

## 2. Route to it in `ProviderFactory`

```rust
// src/providers/factory.rs, in build_provider:
if provider_lc == "myprovider" {
    let user_cfg = config.providers.get("myprovider");
    let base_url = user_cfg
        .and_then(|c| c.base_url.clone())
        .unwrap_or_else(|| "https://api.myprovider.com/v1".to_string());
    let api_key = require_key("myprovider", "MYPROVIDER_API_KEY")?;
    let p = MyProvider::new(api_key, model_name.to_string(), base_url)?;
    return Ok(Box::new(p));
}
```

## 3. Describe capabilities honestly

`Capabilities` advertises what the reducer should assume. In particular:

- `supports_reasoning: ReasoningCapability::Binary` → `ReasoningLevel::None → off`, anything else → on.
- `ReasoningCapability::Levels(vec)` → `nearest_effort()` maps the user's choice onto the supported set.
- `emits_thinking_signature: true` → the reducer round-trips your `thinking_signature` back on the next request. Only set this for providers like Anthropic that require encrypted server state to round-trip.

## 4. Honor cancellation properly

Every long-running await in the body of `chat()` must be under a `select!` with `ctx.token.cancelled()`. The most common missing case is the SSE reader loop — if you do `while let Some(chunk) = stream.next().await`, wrap it:

```rust
loop {
    tokio::select! {
        biased;
        _ = ctx.token.cancelled() => {
            return Err(ModelError::StreamError("cancelled".into()));
        },
        chunk = stream.next() => match chunk {
            Some(Ok(bytes)) => { /* parse and emit */ }
            Some(Err(e)) => return Err(e.into()),
            None => break,
        },
    }
}
```

Without this, Ctrl+C during a long streaming response waits for the stream to finish. With it, the abort latency is microseconds.

## 5. Testing

Unit test the helpers (`build_request_body`, wire-format translation, error classification) — those are pure functions and don't need a runtime.

Integration test the full `chat()` by pointing it at a test server. `src/effect/middleware.rs` tests drive real HTTP against an in-process `TcpListener` (fake responses, including `Retry-After`); copy that pattern if you need the same.

## Custom providers without new code

If your provider speaks the OpenAI-compat shape (most self-hosted stuff does), don't write a new impl. Add an entry under `[providers.<name>]` in `config.toml`:

```toml
[providers.my-vllm]
base_url = "http://192.168.1.42:8000/v1"
api_key_env = "VLLM_KEY"
compat = "openai-effort"
```

`compat` values: `"openai"`, `"openai-effort"`, `"openrouter"`. The `OpenAICompatProvider` handles the rest.
