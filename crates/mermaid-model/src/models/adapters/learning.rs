//! Capability discovery by rejection: send optimistically, learn from the 400.
//!
//! Providers don't publish which request parameters each model accepts, so
//! predicting it means a table of model names that is wrong the day a model
//! ships. Instead, every adapter sends the parameters the user asked for
//! (temperature, the effort tier, the newest thinking shape) and lets the
//! provider say no. A 400 or 422 that names one of those parameters is
//! answered by taking that one parameter back, or stepping it down a tier,
//! and sending again. What was rejected is remembered per model in a
//! [`ParamMemory`], and the provider wrapper persists it to the runtime
//! store's `provider_probes` so the next session skips the wasted round trip.
//!
//! The catalog (`super::super::catalog`) is only a hint on top of this: a
//! known model can skip a round trip, an unknown one pays at most a few on
//! its first call and then works.
//!
//! Three guards keep a misread error from teaching the wrong thing:
//!
//! - **Only rejections.** Nothing here ever records a parameter as
//!   *supported*. Some providers (OpenAI-compatible and local servers most of
//!   all) accept a parameter they don't understand and silently ignore it; a
//!   200 proves nothing, so success never writes to the memory. Where silence
//!   is the failure mode, a catalog hint picks the wire shape instead.
//! - **Committed on success.** Rejections are staged for the retry and only
//!   reach the memory once a retry is accepted. An error that blamed the
//!   wrong parameter never gets past the retry, so it is never remembered.
//! - **Bounded and monotonic.** Each retry adds one rejection, a rejection is
//!   never removed within a session, and at most [`MAX_LEARNING_RETRIES`]
//!   retries run per call.

use std::collections::BTreeSet;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::models::error::{BackendError, ModelError, Result};
use crate::models::stream::{StreamEvent, StreamSink, emit};

/// Retries one call may spend learning. Each strips or downgrades one
/// parameter, and no adapter sends more optional parameters than this.
pub const MAX_LEARNING_RETRIES: usize = 4;

/// Whether an HTTP status is one where the body names a rejected parameter:
/// 400 from most providers, 422 from the FastAPI-based local servers.
#[must_use]
pub fn is_rejection(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 400 | 422)
}

/// Every request item a provider has rejected for one model.
///
/// Items are adapter-defined strings: a bare parameter (`"temperature"`)
/// means "never send it", a `name:value` pair (`"effort:xhigh"`) means "never
/// send that value". Keeping them as strings keeps the persisted form a plain
/// JSON array that survives an adapter adding a new kind of item.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rejections(BTreeSet<String>);

impl Rejections {
    /// Nothing rejected.
    #[must_use]
    pub const fn new() -> Self {
        Self(BTreeSet::new())
    }

    /// Whether the provider rejected this item.
    #[must_use]
    pub fn contains(&self, item: &str) -> bool {
        self.0.contains(item)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Everything rejected, sorted.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }

    /// Add everything `other` rejected.
    pub fn extend(&mut self, other: &Self) {
        self.0.extend(other.0.iter().cloned());
    }

    /// Record one rejection; `false` when it was already known.
    pub fn insert(&mut self, item: &str) -> bool {
        self.0.insert(item.to_string())
    }
}

impl<const N: usize> From<[&str; N]> for Rejections {
    fn from(items: [&str; N]) -> Self {
        Self(items.iter().map(|s| (*s).to_string()).collect())
    }
}

/// One adapter's memory of what its model rejected, shared across turns.
#[derive(Debug, Default)]
pub struct ParamMemory(Mutex<Rejections>);

impl ParamMemory {
    /// What has been learned so far.
    #[must_use]
    pub fn snapshot(&self) -> Rejections {
        self.lock().clone()
    }

    /// Fold in rejections learned elsewhere (the persisted cache).
    pub fn seed(&self, learned: &Rejections) {
        self.lock().extend(learned);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Rejections> {
        // A poisoned lock only means a panic elsewhere mid-insert; the set is
        // still a valid set.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// An optional item a request carried, and how a rejection would name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Optional {
    /// What to remember if the provider rejects it (see [`Rejections`]).
    pub remember: String,
    /// How the user-facing notice names it.
    pub label: String,
    /// Lowercase words a rejection naming this item contains. The earliest
    /// mention across all sent items wins: providers lead with the field
    /// they rejected and name alternatives after it.
    pub names: &'static [&'static str],
    /// When non-empty, one of these must also appear. For a rename (OpenAI's
    /// "use `max_completion_tokens` instead") the alternative is the proof
    /// the error is about the spelling and not the value.
    pub requires: &'static [&'static str],
    /// A rejection mentioning any of these is about something else (a
    /// thinking-block signature that failed to round-trip is not "thinking is
    /// unsupported").
    pub unless: &'static [&'static str],
}

impl Optional {
    /// An item that is blamed whenever a rejection names it.
    #[must_use]
    pub fn new(remember: &str, label: &str, names: &'static [&'static str]) -> Self {
        Self {
            remember: remember.to_string(),
            label: label.to_string(),
            names,
            requires: &[],
            unless: &[],
        }
    }

    #[must_use]
    pub const fn requiring(mut self, requires: &'static [&'static str]) -> Self {
        self.requires = requires;
        self
    }

    #[must_use]
    pub const fn unless(mut self, unless: &'static [&'static str]) -> Self {
        self.unless = unless;
        self
    }

    /// Where in `text` this item is named, if the rejection is about it.
    fn position(&self, text: &str) -> Option<usize> {
        if self.unless.iter().any(|w| text.contains(w)) {
            return None;
        }
        if !self.requires.is_empty() && !self.requires.iter().any(|w| text.contains(w)) {
            return None;
        }
        self.names.iter().filter_map(|n| text.find(n)).min()
    }
}

/// The text of a rejection, lowercased, with the model's own id removed —
/// a model called `kimi-k2-thinking` must not make every error it gets read
/// as a complaint about thinking.
#[must_use]
pub fn rejection_text(err: &ModelError, model: &str) -> String {
    let raw = match err {
        ModelError::Backend(BackendError::HttpError { message, .. }) => message.clone(),
        ModelError::Backend(BackendError::ProviderError { code, message, .. }) => {
            format!("{} {message}", code.as_deref().unwrap_or_default())
        },
        other => other.to_string(),
    };
    let mut text = raw.to_ascii_lowercase();
    let full = model.to_ascii_lowercase();
    let bare = full.rsplit('/').next().unwrap_or(&full).to_string();
    let untagged = bare.split(':').next().unwrap_or(&bare).to_string();
    for id in [full, bare, untagged] {
        text = without_id(&text, &id);
    }
    text
}

/// `text` with every whole-id occurrence of `id` blanked. Whole-id only: a
/// model called `m` must not eat the `m` out of "temperature".
fn without_id(text: &str, id: &str) -> String {
    let is_id_char = |c: char| c.is_ascii_alphanumeric() || "-_.:/".contains(c);
    if id.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(id) {
        let (head, tail) = rest.split_at(at);
        let after = tail.strip_prefix(id).unwrap_or_default();
        let whole = !head.chars().next_back().is_some_and(is_id_char)
            && !after.chars().next().is_some_and(is_id_char);
        out.push_str(head);
        if !whole {
            out.push_str(id);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The sent item a rejection is about: the one named earliest.
#[must_use]
pub fn blame<'a>(text: &str, sent: &'a [Optional]) -> Option<&'a Optional> {
    sent.iter()
        .filter_map(|item| item.position(text).map(|at| (at, item)))
        .min_by_key(|(at, _)| *at)
        .map(|(_, item)| item)
}

/// One call's learning loop.
///
/// ```text
/// let mut learning = Learning::start(&self.memory, &self.model_name, sink);
/// let response = loop {
///     let body = self.build_request_body_with(messages, config, learning.rejections());
///     let response = self.send_chat(&body).await?;
///     if !learning.is_retryable(&response) { break response; }
///     let err = http_error_from_response(response).await;
///     learning.retry_or_fail(err, &optionals(&body)).await?;
/// };
/// learning.settle(&response);
/// ```
pub struct Learning<'a> {
    memory: &'a ParamMemory,
    model: &'a str,
    sink: Option<&'a StreamSink>,
    /// The memory plus everything staged this call.
    effective: Rejections,
    staged: Rejections,
    retries: usize,
}

impl<'a> Learning<'a> {
    #[must_use]
    pub fn start(memory: &'a ParamMemory, model: &'a str, sink: Option<&'a StreamSink>) -> Self {
        Self {
            memory,
            model,
            sink,
            effective: memory.snapshot(),
            staged: Rejections::default(),
            retries: 0,
        }
    }

    /// What the next request must avoid.
    #[must_use]
    pub const fn rejections(&self) -> &Rejections {
        &self.effective
    }

    /// Whether this response is a rejection worth reading for a lesson.
    #[must_use]
    pub fn is_retryable(&self, response: &reqwest::Response) -> bool {
        is_rejection(response.status()) && self.retries < MAX_LEARNING_RETRIES
    }

    /// Learn from `err` and return `Ok(())` to retry, or hand `err` back when
    /// it names nothing the request could do without.
    ///
    /// # Errors
    ///
    /// `err` itself, unchanged, when no sent item is blamed or the blamed one
    /// was already known (retrying would send the same request), and the
    /// sink's error if the turn went away while the notice was sent.
    pub async fn retry_or_fail(&mut self, err: ModelError, sent: &[Optional]) -> Result<()> {
        let text = rejection_text(&err, self.model);
        let Some(item) = blame(&text, sent) else {
            return Err(err);
        };
        if !self.effective.insert(&item.remember) {
            return Err(err);
        }
        self.staged.insert(&item.remember);
        self.retries += 1;
        tracing::info!(
            model = self.model,
            rejected = item.remember.as_str(),
            "provider rejected an optional parameter; retrying without it"
        );
        emit(
            self.sink,
            StreamEvent::Status(format!(
                "{} does not accept {}; adjusted the request and retried",
                self.model, item.label
            )),
        )
        .await
    }

    /// Commit what this call learned once the provider accepted a request.
    /// Anything else (an unrelated failure, retries exhausted) discards it:
    /// a lesson that never led to an accepted request is unproven.
    pub fn settle(self, response: &reqwest::Response) {
        if response.status().is_success() && !self.staged.is_empty() {
            self.memory.seed(&self.staged);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_400(message: &str) -> ModelError {
        ModelError::Backend(BackendError::HttpError {
            status: 400,
            message: message.to_string(),
            debug: crate::models::error::ResponseDebugContext::default(),
        })
    }

    fn temperature() -> Optional {
        Optional::new("temperature", "temperature", &["temperature"])
    }

    fn effort(value: &str) -> Optional {
        Optional::new(&format!("effort:{value}"), "that effort", &["effort"])
    }

    #[test]
    fn blame_picks_the_earliest_named_item() {
        // Anthropic names the field it rejected first and the fix after it.
        let sent = [effort("high"), temperature()];
        let text = "temperature is not supported for this model. use output_config.effort instead";
        assert_eq!(
            blame(text, &sent).map(|o| o.remember.as_str()),
            Some("temperature")
        );
    }

    #[test]
    fn blame_needs_a_named_item() {
        let sent = [temperature()];
        assert_eq!(
            blame("messages: at least one message is required", &sent),
            None
        );
        assert_eq!(blame("", &sent), None);
    }

    #[test]
    fn unless_words_veto_the_blame() {
        let thinking = Optional::new("thinking:adaptive", "adaptive thinking", &["thinking"])
            .unless(&["signature"]);
        let sent = [thinking];
        assert_eq!(
            blame(
                "messages.1.content.0.thinking.signature: field required",
                &sent
            ),
            None
        );
        assert!(blame("adaptive thinking is not supported on this model", &sent).is_some());
    }

    #[test]
    fn requires_words_gate_a_rename() {
        let spelling = Optional::new("max_tokens", "max_tokens", &["max_tokens"])
            .requiring(&["max_completion_tokens"]);
        let sent = [spelling];
        // The output-cap wording names max_tokens too, but offers no rename.
        assert_eq!(blame("max_tokens is too large: 200000", &sent), None);
        assert!(
            blame(
                "unsupported parameter: 'max_tokens' is not supported with this model. use 'max_completion_tokens' instead.",
                &sent
            )
            .is_some()
        );
    }

    #[test]
    fn rejection_text_drops_the_model_id() {
        let err = http_400(
            "max_tokens (521276) exceeds model's maximum output tokens for model kimi-k2-thinking:cloud",
        );
        let text = rejection_text(&err, "ollama/kimi-k2-thinking:cloud");
        assert!(!text.contains("think"), "{text}");
        let think = Optional::new("think:bool", "think", &["think"]);
        assert_eq!(blame(&text, &[think]), None);
    }

    #[test]
    fn rejection_text_only_drops_whole_ids() {
        let err = http_400("temperature is not supported for model m");
        let text = rejection_text(&err, "m");
        assert!(text.contains("temperature"), "{text}");
        assert!(!text.ends_with(" m"), "{text}");
        // A quoted id is still whole.
        let quoted = http_400(r#""qwen3:8b" does not support thinking"#);
        assert_eq!(
            rejection_text(&quoted, "qwen3:8b"),
            r#""" does not support thinking"#
        );
    }

    #[test]
    fn rejection_text_reads_provider_errors() {
        let err = ModelError::Backend(BackendError::ProviderError {
            provider: "anthropic".into(),
            code: Some("invalid_request_error".into()),
            message: "Temperature is deprecated for this model".into(),
            debug: crate::models::error::ResponseDebugContext::default(),
        });
        assert!(rejection_text(&err, "claude-x").contains("temperature is deprecated"));
    }

    #[test]
    fn rejections_persist_as_a_plain_json_array() {
        let r = Rejections::from(["temperature", "effort:xhigh"]);
        let json = serde_json::to_string(&r).expect("ser");
        assert_eq!(json, r#"["effort:xhigh","temperature"]"#);
        let back: Rejections = serde_json::from_str(&json).expect("de");
        assert_eq!(back, r);
    }

    #[tokio::test]
    async fn a_lesson_is_staged_until_a_request_succeeds() {
        let memory = ParamMemory::default();
        let mut learning = Learning::start(&memory, "m", None);
        learning
            .retry_or_fail(http_400("temperature is not supported"), &[temperature()])
            .await
            .expect("retry");
        assert!(learning.rejections().contains("temperature"));
        // Not in the memory yet: nothing has proven the lesson.
        assert!(memory.snapshot().is_empty());
        drop(learning);
        assert!(
            memory.snapshot().is_empty(),
            "an unsettled call teaches nothing"
        );
    }

    fn response(status: u16) -> reqwest::Response {
        reqwest::Response::from(
            http::Response::builder()
                .status(status)
                .body(Vec::<u8>::new())
                .expect("response"),
        )
    }

    #[tokio::test]
    async fn an_accepted_retry_commits_the_lesson() {
        let memory = ParamMemory::default();
        let mut learning = Learning::start(&memory, "m", None);
        learning
            .retry_or_fail(http_400("temperature is not supported"), &[temperature()])
            .await
            .expect("retry");
        learning.settle(&response(200));
        assert_eq!(memory.snapshot(), Rejections::from(["temperature"]));

        // A retry that failed some other way commits nothing.
        let memory = ParamMemory::default();
        let mut learning = Learning::start(&memory, "m", None);
        learning
            .retry_or_fail(http_400("temperature is not supported"), &[temperature()])
            .await
            .expect("retry");
        learning.settle(&response(500));
        assert!(memory.snapshot().is_empty());
    }

    #[tokio::test]
    async fn retries_are_bounded() {
        let memory = ParamMemory::default();
        let mut learning = Learning::start(&memory, "m", None);
        for i in 0..MAX_LEARNING_RETRIES {
            let item = Optional::new(&format!("p{i}"), "p", &["param"]);
            learning
                .retry_or_fail(http_400("param rejected"), &[item])
                .await
                .expect("within budget");
        }
        assert!(!learning.is_retryable(&response(400)));
        assert!(!learning.is_retryable(&response(200)));
    }

    #[tokio::test]
    async fn the_same_rejection_twice_is_an_error_not_a_loop() {
        let memory = ParamMemory::default();
        let mut learning = Learning::start(&memory, "m", None);
        learning
            .retry_or_fail(http_400("temperature is not supported"), &[temperature()])
            .await
            .expect("first");
        let err = learning
            .retry_or_fail(http_400("temperature is not supported"), &[temperature()])
            .await
            .expect_err("already known");
        assert!(err.to_string().contains("temperature"));
    }

    #[tokio::test]
    async fn the_notice_names_the_model_and_the_parameter() {
        let memory = ParamMemory::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut learning = Learning::start(&memory, "brand-new-model", Some(&tx));
        learning
            .retry_or_fail(http_400("temperature: unsupported"), &[temperature()])
            .await
            .expect("retry");
        match rx.recv().await {
            Some(StreamEvent::Status(s)) => {
                assert!(
                    s.contains("brand-new-model") && s.contains("temperature"),
                    "{s}"
                );
            },
            other => panic!("expected a status, got {other:?}"),
        }
    }
}
