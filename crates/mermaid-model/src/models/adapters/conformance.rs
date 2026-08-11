//! One test suite, five wire formats.
//!
//! Every adapter is asked the same questions against a recorded response
//! body, and the answers are compared in NORMALIZED terms — the streamed
//! text, the streamed reasoning, the tool calls, the stop reason, the token
//! totals — never in the provider's own vocabulary. Anthropic's
//! `max_tokens`, Gemini's `MAX_TOKENS`, OpenAI's `length` and Ollama's
//! `done_reason: "length"` are four spellings of one fact, and the point of
//! the corpus is that the layer above cannot tell them apart.
//!
//! Everything here runs through the real [`drive_stream`] — the same
//! function production calls, over a `stream::iter` of the recorded bytes
//! instead of a socket. Which is the whole reason the driver takes a body
//! rather than a `reqwest::Response`.
//!
//! Each scenario runs TWICE: once with the body delivered whole, once one
//! byte at a time. Both must agree. That is the guard for the reassembly
//! the driver now owns for everyone — before it, a chunk boundary landing
//! inside a frame was untested on every adapter.
//!
//! Fixtures live in `tests/fixtures/streams/<provider>/<scenario>` and are
//! raw response bodies, byte for byte.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::models::adapters::driver::{StreamProtocol, drive_stream};
use crate::models::error::{BackendError, ModelError};
use crate::models::stream::StreamEvent;
use crate::models::types::{FinishReason, ModelResponse};

/// What a scenario produced, in terms no provider owns.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    /// Every `StreamEvent::Text` payload, concatenated.
    streamed_text: String,
    /// Every `StreamEvent::Reasoning` payload, concatenated.
    streamed_reasoning: String,
    /// Every `StreamEvent::ToolCall`, as `(name, arguments-as-json)`.
    streamed_tool_calls: Vec<(String, String)>,
    /// `ModelResponse.content`.
    content: String,
    thinking: Option<String>,
    /// The response's tool calls, same shape as the streamed ones. These
    /// must agree — the renderer reads one and the reducer the other, and a
    /// turn where they disagree runs a tool nobody saw announced.
    response_tool_calls: Vec<(String, String)>,
    stop_reason: Option<FinishReason>,
    /// `(input total, output total)`, or `None` when the stream never
    /// reported usage. Totals rather than raw fields, because providers
    /// carve cached and reasoning tokens out of the headline numbers
    /// differently.
    usage: Option<(usize, usize)>,
}

/// A scenario that failed, in terms no provider owns.
#[derive(Debug, PartialEq, Eq)]
enum Failure {
    /// The provider sent a typed error frame mid-stream.
    ProviderError,
    /// The body ended without a terminal frame (F56).
    StreamCut,
}

type ScenarioResult = std::result::Result<Outcome, Failure>;

/// How a recorded body reaches the driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// One chunk, the whole body.
    Whole,
    /// One byte per chunk — every frame boundary lands mid-chunk.
    OneByteAtATime,
}

/// Run one protocol over one recorded body through the real driver.
async fn run<P: StreamProtocol>(protocol: P, body: &str, delivery: Delivery) -> ScenarioResult {
    let bytes = body.as_bytes();
    let chunks: Vec<std::result::Result<Vec<u8>, String>> = match delivery {
        Delivery::Whole => vec![Ok(bytes.to_vec())],
        Delivery::OneByteAtATime => bytes.iter().map(|b| Ok(vec![*b])).collect(),
    };

    // Roomy enough that no fixture can fill it, so a single task suffices;
    // the driver's own tests are where backpressure is exercised.
    let (tx, mut rx) = mpsc::channel::<StreamEvent>(4096);
    let result = drive_stream(futures::stream::iter(chunks), protocol, Some(&tx)).await;
    drop(tx);

    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }

    match result {
        Ok(response) => Ok(normalize(&events, response)),
        Err(e) => Err(classify(&e)),
    }
}

/// Collapse a provider's error into the shared vocabulary.
///
/// Only two kinds can reach here, and asserting that is part of the point:
/// a scenario that failed some third way (a parse error, a connection
/// failure) is a broken fixture, not a finding.
fn classify(err: &ModelError) -> Failure {
    if matches!(err, ModelError::Backend(BackendError::ProviderError { .. })) {
        return Failure::ProviderError;
    }
    assert!(
        matches!(err, ModelError::StreamError(_)),
        "scenario failed in an unexpected way: {err:?}"
    );
    Failure::StreamCut
}

fn normalize(events: &[StreamEvent], response: ModelResponse) -> Outcome {
    let mut streamed_text = String::new();
    let mut streamed_reasoning = String::new();
    let mut streamed_tool_calls = Vec::new();
    // Adapters never emit `Status` or `Done`: the first is the Ollama
    // autostart notice, which comes from outside the stream, and the second
    // comes from the provider wrapper (F3). Collected rather than ignored so
    // an adapter that starts emitting one is caught here.
    let mut off_contract: Vec<String> = Vec::new();
    for event in events {
        match event {
            StreamEvent::Text(t) => streamed_text.push_str(t),
            StreamEvent::Reasoning(c) => streamed_reasoning.push_str(&c.text),
            StreamEvent::ToolCall(tc) => streamed_tool_calls
                .push((tc.function.name.clone(), tc.function.arguments.to_string())),
            StreamEvent::Status(_) | StreamEvent::Done { .. } => {
                off_contract.push(format!("{event:?}"));
            },
        }
    }
    assert!(
        off_contract.is_empty(),
        "adapter emitted {off_contract:?} from inside the stream"
    );
    Outcome {
        streamed_text,
        streamed_reasoning,
        streamed_tool_calls,
        content: response.content,
        thinking: response.thinking,
        response_tool_calls: response
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| (tc.function.name, tc.function.arguments.to_string()))
            .collect(),
        stop_reason: response.stop_reason,
        usage: response
            .usage
            .map(|u| (u.input_total_tokens(), u.output_total_tokens())),
    }
}

fn fixture(provider: &str, name: &str) -> String {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "streams",
        provider,
        name,
    ]
    .iter()
    .collect();
    assert!(path.is_file(), "missing fixture {}", path.display());
    std::fs::read_to_string(&path).expect("fixture reads")
}

/// Every provider's protocol, built fresh — a protocol is consumed by
/// `finish`, and every scenario runs it twice.
mod protocols {
    use super::super::anthropic::AnthropicStream;
    use super::super::gemini::GeminiStream;
    use super::super::meta::MetaStream;
    use super::super::ollama::OllamaStream;
    use super::super::openai_compat::OpenAICompatStream;

    pub(super) fn anthropic(hide_reasoning: bool) -> AnthropicStream {
        AnthropicStream::new("claude-test".to_string(), hide_reasoning)
    }

    pub(super) fn gemini(hide_reasoning: bool) -> GeminiStream {
        GeminiStream::new("gemini-test".to_string(), hide_reasoning)
    }

    pub(super) fn ollama(hide_reasoning: bool) -> OllamaStream {
        OllamaStream::new("ollama-test".to_string(), hide_reasoning)
    }

    /// Meta is the one protocol with no reasoning-trace switch: the
    /// Responses API sends summaries, and mermaid asks for them
    /// unconditionally. The flag is accepted and ignored so the shared
    /// harness keeps one shape.
    pub(super) fn meta(_hide_reasoning: bool) -> MetaStream {
        MetaStream::new("muse-spark-test".to_string())
    }

    /// `deepinfra` carries `ReasoningExtraction::DeltaContentField
    /// ("reasoning_content")`, the shape the reasoning fixture records.
    pub(super) fn openai_compat(hide_reasoning: bool) -> OpenAICompatStream {
        let profile = crate::models::lookup_provider("deepinfra").expect("registry has deepinfra");
        OpenAICompatStream::new(profile, "openai-test".to_string(), hide_reasoning)
    }
}

/// Run a fixture whole AND one byte at a time, assert the two agree, and
/// return the shared result.
macro_rules! scenario {
    ($build:expr, $provider:literal, $file:literal) => {{
        let body = fixture($provider, $file);
        let whole = run($build(false), &body, Delivery::Whole).await;
        let dribbled = run($build(false), &body, Delivery::OneByteAtATime).await;
        assert_eq!(
            whole, dribbled,
            "{}/{}: chunk boundaries changed the result",
            $provider, $file
        );
        whole
    }};
}

/// One scenario across every provider that records it, in provider order.
macro_rules! all_providers {
    ($sse:literal, $ndjson:literal) => {
        vec![
            scenario!(protocols::anthropic, "anthropic", $sse),
            scenario!(protocols::gemini, "gemini", $sse),
            scenario!(protocols::openai_compat, "openai_compat", $sse),
            scenario!(protocols::meta, "meta", $sse),
            scenario!(protocols::ollama, "ollama", $ndjson),
        ]
    };
}

// ---------------------------------------------------------------------------
// The scenarios. Each asserts the SAME normalized answer from all four.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn text_deltas_concatenate_everywhere() {
    for outcome in all_providers!("text.sse", "text.ndjson") {
        let outcome = outcome.expect("text scenario succeeds");
        assert_eq!(outcome.streamed_text, "Hello, world");
        assert_eq!(outcome.content, "Hello, world");
        assert_eq!(outcome.streamed_reasoning, "");
        assert_eq!(outcome.stop_reason, Some(FinishReason::Stop));
        assert_eq!(outcome.usage, Some((12, 7)));
    }
}

#[tokio::test]
async fn reasoning_splits_from_content_everywhere() {
    for outcome in all_providers!("reasoning.sse", "reasoning.ndjson") {
        let outcome = outcome.expect("reasoning scenario succeeds");
        // Reasoning never leaks into the assistant message, and content
        // never leaks into the trace.
        assert_eq!(outcome.streamed_reasoning, "weighing options");
        assert_eq!(outcome.thinking.as_deref(), Some("weighing options"));
        assert_eq!(outcome.streamed_text, "the answer");
        assert_eq!(outcome.content, "the answer");
        assert_eq!(outcome.usage, Some((20, 11)));
    }
}

#[tokio::test]
async fn hiding_the_trace_suppresses_the_event_not_the_accumulator_everywhere() {
    // Hiding reasoning is a display choice. `ModelResponse.thinking` still
    // has to carry it — that is what round-trips into the next request.
    let hidden = vec![
        run(
            protocols::anthropic(true),
            &fixture("anthropic", "reasoning.sse"),
            Delivery::Whole,
        )
        .await,
        run(
            protocols::gemini(true),
            &fixture("gemini", "reasoning.sse"),
            Delivery::Whole,
        )
        .await,
        run(
            protocols::openai_compat(true),
            &fixture("openai_compat", "reasoning.sse"),
            Delivery::Whole,
        )
        .await,
        run(
            protocols::ollama(true),
            &fixture("ollama", "reasoning.ndjson"),
            Delivery::Whole,
        )
        .await,
    ];
    for outcome in hidden {
        let outcome = outcome.expect("hidden reasoning still succeeds");
        assert_eq!(outcome.streamed_reasoning, "");
        assert_eq!(outcome.thinking.as_deref(), Some("weighing options"));
        assert_eq!(outcome.streamed_text, "the answer");
    }
}

#[tokio::test]
async fn tool_calls_reassemble_everywhere() {
    for outcome in all_providers!("tool_call.sse", "tool_call.ndjson") {
        let outcome = outcome.expect("tool_call scenario succeeds");
        let expected = vec![("read_file".to_string(), r#"{"path":"a.txt"}"#.to_string())];
        // Two of the four fragment the arguments across frames; all four
        // have to hand back one whole call with parsed JSON.
        assert_eq!(outcome.streamed_tool_calls, expected);
        // And the streamed view must match the response view, or the turn
        // runs a tool nobody saw announced.
        assert_eq!(outcome.response_tool_calls, expected);
        assert_eq!(outcome.usage, Some((30, 15)));
    }
}

#[tokio::test]
async fn a_real_truncation_survives_as_length_everywhere() {
    // Four spellings of one fact: `max_tokens`, `MAX_TOKENS`, `length`,
    // `done_reason: "length"`. This is the one that must NOT be confused
    // with the abnormal close below — compact-and-continue keys on it.
    for outcome in all_providers!("truncation.sse", "truncation.ndjson") {
        let outcome = outcome.expect("truncation is a completion, not a failure");
        assert_eq!(outcome.stop_reason, Some(FinishReason::Length));
        assert_eq!(outcome.content, "cut off");
    }
}

#[tokio::test]
async fn a_mid_stream_error_frame_is_typed_everywhere() {
    // #123: without this, an OpenRouter error frame surfaced as "missing
    // field choices" — a parse failure blaming the client for the
    // provider's rate limit.
    for outcome in all_providers!("error_frame.sse", "error_frame.ndjson") {
        assert_eq!(outcome, Err(Failure::ProviderError));
    }
}

#[tokio::test]
async fn a_body_cut_before_the_terminal_frame_is_an_error_everywhere() {
    // F56. A clean `Ok` here would be indistinguishable from a real, short
    // completion, and the caller would commit a half-finished turn.
    for outcome in all_providers!("abnormal_close.sse", "abnormal_close.ndjson") {
        assert_eq!(outcome, Err(Failure::StreamCut));
    }
}

#[tokio::test]
async fn ollama_keeps_the_frame_its_body_closed_on() {
    // The divergence `Framing` made deliberate. Ollama's NDJSON body can end
    // on a whole object with no trailing newline; the SSE providers have no
    // equivalent, because an event without its blank-line separator is
    // incomplete by definition. Only Ollama has this fixture — and dropping
    // its residue would lose the `done` frame, turning a clean completion
    // into the abnormal-close error above.
    let outcome = scenario!(protocols::ollama, "ollama", "no_trailing_newline.ndjson")
        .expect("the final object still counts");
    assert_eq!(outcome.content, "Hello");
    assert_eq!(outcome.stop_reason, Some(FinishReason::Stop));
    assert_eq!(outcome.usage, Some((12, 7)));
}
