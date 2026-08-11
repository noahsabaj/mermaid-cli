# Design: one streaming driver

Status: accepted. Tracks redesign item #6, the last ranked item in the ledger
(#7 Engine extraction shipped as PRs #379-380, #8 the event log as #367-378).
This document is the contract for the PR sequence at the bottom; each PR cites
the section it implements.

## Problem

Every streaming adapter answers the same five questions, and answers them in
its own words:

1. Is the HTTP status a success? If not, what error shape?
2. How do raw TCP chunks become whole frames, and what stops an endless
   frame from eating memory?
3. What does one frame mean? (**The only provider-specific question.**)
4. What happens to the residue left in the buffer when the body closes?
5. Did the stream end, or was it cut?

Four `handle_stream` copies plus `meta.rs`'s inline loop answer all five:

| site | lines | framing | residue | terminates on |
|---|---|---|---|---|
| `adapters/anthropic.rs:946` | 337 | SSE | dropped | `message_stop` frame (`break 'stream`) |
| `adapters/openai_compat.rs:547` | 290 | SSE | dropped | body close |
| `adapters/gemini.rs:728` | 70 (+ `process_chunk_payload`, 200) | SSE | dropped | body close |
| `adapters/ollama.rs:434` | 127 (+ `process_stream_chunk`, 72) | NDJSON | **flushed** | body close |
| `providers/model/meta.rs:136` | 42 | SSE | dropped | `response.completed` frame |

Question 3 is the one that differs. Questions 1, 2, 4 and 5 are answered five
times, and the copies have already drifted:

- **The residue column is an accident, not a decision.** Ollama flushes the
  trailing un-terminated line; the four SSE sites drop it. Nobody chose that —
  Ollama's NDJSON drainer needed it and the SSE sites never grew one. A server
  that closes the body directly after `data: {...}\n` (no blank line) loses its
  final frame on four adapters out of five, silently.
- **The status-check preamble has two spellings.** `anthropic` and `gemini`
  call a shared `http_error_from_response`; `openai_compat` and `ollama`
  open-code the identical status/headers/body/`HttpError` sequence inline.
  `meta` has a third copy with cancellation folded in.
- **Cancellation is checked in two places.** `meta` selects on the token inside
  its read loop; the four adapters rely on the wrapper's outer `select!`
  dropping the future. Both work — dropping the future drops the stream — but
  a reader has to prove that twice.
- **The reassembly cap is copy-pasted five times**, including its message,
  which says "SSE" on the NDJSON path's sibling and "NDJSON" here but not
  there.

And under all four adapters sits the second half of the problem:

**`StreamCallback` is sync, and the sink it feeds is async.** The adapter holds
an `Arc<dyn Fn(StreamEvent) + Send + Sync>`; `StreamContext.sink` is a bounded
`mpsc::Sender`. `stream_bridge.rs` (191 lines) exists solely to join those two:
an unbounded staging channel plus a relay task, because the obvious bridge —
`tokio::spawn` per event — reorders, and a `Done` that overtakes a `ToolCall`
makes the agent forget to run a tool the model asked for (F2).

That is a whole file, a spawned task per turn, an `AbortOnDrop` guard, and a
`join_logged` drain in each of four wrappers, to buy back an ordering property
that a plain `for` loop has for free. The adapter's read loop is *already*
`async` — it is `await`ing `stream.next()` on the line above. It could `await`
the send.

`meta.rs` already does exactly that (`send(&ctx.sink, event).await?`) and needs
no bridge. It is the proof the shape works; it is just on the wrong side of the
crate boundary.

## Shape

**The protocol is sync and pure. The driver is async and owns all I/O.**

```rust
pub trait StreamProtocol {
    /// How this provider's bytes split into frames.
    const FRAMING: Framing;

    /// Consume one frame. Push events for the driver to forward, in order.
    fn on_frame(&mut self, frame: &str, out: &mut Vec<StreamEvent>) -> Result<Flow>;

    /// The stream ended (body closed, or `Flow::Stop`). Build the response —
    /// and emit anything only the ending makes whole, which for the
    /// OpenAI-compatible shape is every tool call.
    fn finish(self, out: &mut Vec<StreamEvent>) -> Result<ModelResponse>;
}

pub enum Framing { Sse, Ndjson }
pub enum Flow { Continue, Stop }
```

and one driver:

```rust
pub async fn drive_stream<P, S, B, E>(
    body: S,
    protocol: P,
    sink: Option<&mpsc::Sender<StreamEvent>>,
) -> Result<ModelResponse>
```

which owns, once: `bytes_stream()`, the reassembly cap, the framing split, the
residue policy, the drain-and-`await` of each frame's events, and `Flow::Stop`.

**Correction, after PR C landed.** The status check does *not* move into the
driver, and the table above overstated it as one of the five. Anthropic tags a
400 that mentions thinking as a signature round-trip bug, and Gemini turns
`PERMISSION_DENIED` into "check that `GOOGLE_API_KEY` is valid and the
Generative Language API is enabled" — those are the messages users actually
read, and a shared handler would have to grow a provider switch to keep them.
So each adapter checks its own status before calling `drive_stream`, and the
two whose shape really is the plain one (`openai_compat`, `ollama` — the
identical status/headers/body/`HttpError` sequence, open-coded twice) share
`plain_http_error` instead. Four of the five questions moved; the fifth was
never shared to begin with.

`drive_stream` also takes the body, not the `reqwest::Response`: nothing below
that line is about HTTP, and a driver whose input is a stream of byte chunks
is one a test can feed recorded chunks — which is what makes the split-chunk
scenario below assertable at all.

Three consequences fall out of the split, and they are the reason for it:

- **Ordering is structural.** `on_frame` pushes into a `Vec`; the driver drains
  it in order and `await`s each send on the bounded sink. There is no second
  channel, no spawned task, and no way to express the F2 bug. `stream_bridge.rs`
  deletes.
- **Backpressure reaches the socket.** The driver `await`s the bounded send
  between reads, so a drowning consumer stalls the read loop and fills the
  provider's TCP window — which is what `StreamContext`'s doc comment always
  claimed happened, and what the unbounded staging channel was quietly
  defeating.
- **Wire parsing is testable without a runtime.** `on_frame` is a sync
  `&str -> Vec<StreamEvent>` function. A conformance fixture feeds it recorded
  frames and asserts the events, with no tokio, no HTTP, and no mock server.
  That is what makes the fixture corpus cheap enough to be worth having.

The residue question gets answered once, by `Framing`: `Ndjson` flushes a
trailing un-terminated line, `Sse` drops it. Same behavior as today on both
paths — but chosen, in one place, instead of inherited.

Cancellation stays where it is. The four wrappers' outer `select!` on
`ctx.token.cancelled()` already drops the driver future and with it the
stream; the driver takes no token and gains no `select!`. `meta`'s inline
token check is redundant with the wrapper it will grow and goes away with it.

### What `Model::chat` takes

`Option<StreamCallback>` becomes `Option<mpsc::Sender<StreamEvent>>`. The
`Some`/`None` split keeps its existing meaning — stream, or one blocking
request — so `decode_non_streaming` stays.

`StreamCallback` itself is deleted. Its one remaining non-chat user is the
Ollama autostart notice, which never carries content: `with_status_notify`
narrows to `Arc<dyn Fn(&str) + Send + Sync>`, the shape
`LocalServerRecovery::ensure_running`'s `notify` parameter already has. On the
chat path that notice reaches the sink through `try_send` — it fires before
any content exists, so the channel is empty and the send cannot fail in
practice; and a status line dropped under backpressure is best-effort by
definition (it is already sent through `let _ =` today).

### Where `meta` lives

`meta.rs` is a `ModelProvider` that hand-rolls a `Model` adapter's job inside
the CLI crate. It ends up as `adapters/meta.rs` implementing `Model` +
`StreamProtocol` like its four siblings, with a thin wrapper left behind in
`src/providers/model/meta.rs`.

The one thing that has to move is its `ChatRequest` dependency:
`mermaid_domain` sits *above* `mermaid_model` in the crate stack, so the
adapter takes `&[ChatMessage] + &ModelConfig` and the wrapper does the
`ChatRequest -> ModelConfig` mapping, exactly as the other four do. Everything
else it needs — `ProviderContinuation::MetaResponses`, `MetaResponseItem`,
`TokenUsage`, `nearest_effort` — is already in the model crate.

## Conformance corpus

One fixture directory per provider, each holding recorded byte streams for the
same scenarios:

| scenario | asserts |
|---|---|
| `text` | plain deltas concatenate; `Text` events in order |
| `reasoning` | reasoning splits from content; `hide_reasoning_trace` suppresses the event but not the accumulator |
| `tool_call` | fragmented arguments reassemble; `ToolCall` arrives before the response |
| `truncation` | a real `length`/`MAX_TOKENS` stop survives as `FinishReason::Length` |
| `error_frame` | a mid-stream error payload becomes a typed `ProviderError`, not a parse failure (#123) |
| `abnormal_close` | a body cut before any terminal frame is a `StreamError`, not a clean empty `Ok` (F56) |
| `split_chunks` | the same bytes delivered one byte at a time produce identical output |

The last one is the payment for the reassembly cap and the framing split
living in one place: it can be asserted generically, for every protocol, by
re-driving the same fixture through a chunker. Today no adapter has that test.

**Two adjustments, after PR D landed.** `usage` did not need a scenario of its
own — every other fixture already carries a usage frame, so the token totals
are asserted alongside the thing they accompany, and `None`-when-never-reported
stays covered by each adapter's own unit tests where it belongs. And
`split_chunks` is not a scenario either: it is how *every* scenario runs, twice,
which is strictly stronger than one fixture testing it once.

One scenario is deliberately not shared. Ollama's NDJSON body can close on a
whole frame with no trailing newline; an SSE event without its blank-line
separator is incomplete by definition, so there is nothing for the other four
to record. `ollama_keeps_the_frame_its_body_closed_on` stands alone, which is
the corpus saying out loud what used to be an accident of which drainer grew a
residue branch.

## Sequence

Each PR is independently green and mergeable.

- **A — unify `StreamEvent`.** `mermaid_model::models::stream::StreamEvent`
  grows the rich `Done { usage, provider_continuation, stop_reason }` the CLI's
  duplicate carries; `providers/ctx.rs` re-exports it instead of defining a
  second one. `forward_callback`'s variant-by-variant mapping deletes.
- **B — the async sink.** `Model::chat` takes
  `Option<mpsc::Sender<StreamEvent>>`. Each `handle_stream` `await`s its sends;
  `process_chunk_payload` / `process_stream_chunk` push into a `Vec` the loop
  drains. `stream_bridge.rs` and `StreamCallback` are deleted, and the four
  wrappers pass `ctx.sink.clone()`. (This is the PR that pays the F2 debt off
  structurally — the guard is that the ordering test survives the deletion of
  the machinery it was written for.)
- **C — the driver.** `StreamProtocol` + `drive_stream`; all four adapters
  ported; the five loop copies deleted.
- **D — the corpus.** Fixtures and the generic harness above.
- **E — `meta` folds in.** `adapters/meta.rs` implements `Model` +
  `StreamProtocol`; `providers/model/meta.rs` becomes a wrapper; meta joins the
  corpus.

**Done**, PRs #381-386. The count that summarizes it: five copies of the
reassembly cap became one, `providers/model/meta.rs` went from 873 lines to
156, `stream_bridge.rs` and its spawned relay task per turn are gone, and the
seven scenarios each run against five wire formats — twice, once a byte at a
time.
