//! LLM-backed safety vetting for `SafetyMode::Auto`.
//!
//! Under Auto mode the rule engine (`mermaid-runtime`) classifies a
//! borderline action as [`PolicyDecision::Classify`] and defers the
//! allow/escalate call to a model. This module is that model call. It lives
//! in `mermaid-cli` (not the runtime crate) because the runtime is
//! deliberately model-free — the policy gate injects an
//! `Arc<dyn AutoClassifier>` into [`ExecContext`] and awaits [`AutoClassifier::vet`]
//! before letting a borderline action run.
//!
//! Authority is **allow-or-escalate only** — the classifier never hard-blocks
//! (destructive patterns are already denied by the rule engine), and any
//! error / timeout / unparseable reply **fails safe** to "escalate to human".
//!
//! [`PolicyDecision::Classify`]: crate::runtime::PolicyDecision
//! [`ExecContext`]: crate::providers::ctx::ExecContext

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::domain::{ChatRequest, TurnId};
use crate::models::{ChatMessage, ReasoningLevel};
use crate::providers::factory::ProviderFactory;

/// How long to wait for the classifier before failing safe (escalating).
const VET_TIMEOUT: Duration = Duration::from_secs(10);
/// Verdicts are tiny — `ALLOW` or `ESCALATE: <short reason>`.
const VET_MAX_TOKENS: usize = 150;

const SYSTEM_PROMPT: &str = "You are a safety reviewer for an AI coding agent running in \"auto\" mode. \
The agent has already decided to take an action; your job is to wave through the routine, aligned ones \
and stop only the genuinely risky or off-task ones. Bias strongly toward ALLOW: most actions an engineer \
would expect while pursuing the stated goal should pass. ESCALATE only when an action is destructive, \
leaks secrets or credentials, reaches untrusted network endpoints, modifies shared/production \
infrastructure, or clearly does not serve the user's goal. When in doubt about real risk, ESCALATE. \
Reply with EXACTLY one line: `ALLOW` or `ESCALATE: <short reason>`.";

/// One action to vet, expressed in the classifier's terms.
#[derive(Debug, Clone)]
pub struct VetRequest {
    pub tool: String,
    pub summary: String,
    pub command: Option<String>,
    pub path: Option<String>,
    /// The user's stated intent for the turn (latest user message), if known.
    pub intent: Option<String>,
    /// Absolute working directory, for context.
    pub workdir: String,
    pub turn: TurnId,
    /// Turn cancellation — a Ctrl+C aborts the vet (which then fails safe).
    pub token: CancellationToken,
}

/// The classifier's verdict. `allow == false` means "escalate to a human".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VetVerdict {
    pub allow: bool,
    pub reason: String,
}

impl VetVerdict {
    pub fn allow() -> Self {
        Self {
            allow: true,
            reason: String::new(),
        }
    }
    pub fn escalate(reason: impl Into<String>) -> Self {
        Self {
            allow: false,
            reason: reason.into(),
        }
    }
}

/// Vets a borderline action against the user's intent. Implementors must be
/// cheap to clone-share (`Arc`) and safe to call concurrently.
#[async_trait]
pub trait AutoClassifier: Send + Sync {
    async fn vet(&self, req: &VetRequest) -> VetVerdict;
}

/// Production classifier: builds a focused one-shot prompt and runs it through
/// a provider (by default the session's own model).
pub struct ModelAutoClassifier {
    providers: Arc<ProviderFactory>,
    model_id: String,
}

impl ModelAutoClassifier {
    pub fn new(providers: Arc<ProviderFactory>, model_id: String) -> Self {
        Self {
            providers,
            model_id,
        }
    }

    fn build_request(&self, req: &VetRequest) -> ChatRequest {
        let action = describe_action(req);
        let intent = req
            .intent
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("(no explicit goal stated this turn)");
        let user = format!(
            "Working directory: {wd}\n\nUser's current goal:\n{intent}\n\nProposed action:\n{action}\n\n\
             Does this action plausibly serve the user's goal and look safe to run automatically?",
            wd = req.workdir,
            intent = intent,
            action = action,
        );
        ChatRequest {
            model_id: self.model_id.clone(),
            messages: vec![ChatMessage::user(user)],
            system_prompt: SYSTEM_PROMPT.to_string(),
            instructions: None,
            // The judgment is simple and we want it fast/cheap — no
            // extended thinking.
            reasoning: ReasoningLevel::None,
            temperature: 0.0,
            max_tokens: VET_MAX_TOKENS,
            tools: Vec::new(),
        }
    }
}

#[async_trait]
impl AutoClassifier for ModelAutoClassifier {
    async fn vet(&self, req: &VetRequest) -> VetVerdict {
        let request = self.build_request(req);
        let providers = Arc::clone(&self.providers);
        let model_id = self.model_id.clone();
        let turn = req.turn;
        let token = req.token.clone();

        let call = async move {
            let provider = providers.resolve(&model_id).await?;
            let (text, _usage) =
                crate::providers::model::collect_text(provider, turn, request, token).await?;
            Ok::<String, crate::models::ModelError>(text)
        };

        match tokio::time::timeout(VET_TIMEOUT, call).await {
            Ok(Ok(text)) => parse_verdict(&text),
            Ok(Err(err)) => VetVerdict::escalate(format!("classifier unavailable: {err}")),
            Err(_) => VetVerdict::escalate("classifier timed out"),
        }
    }
}

fn describe_action(req: &VetRequest) -> String {
    if let Some(cmd) = &req.command {
        format!("Tool `{}` will run a shell command:\n  {}", req.tool, cmd)
    } else if let Some(path) = &req.path {
        format!(
            "Tool `{}` will act on path `{}` ({})",
            req.tool, path, req.summary
        )
    } else {
        format!("Tool `{}`: {}", req.tool, req.summary)
    }
}

/// Parse the classifier's reply. Tolerant: matches a leading `ALLOW` /
/// `ESCALATE` case-insensitively; anything unrecognized fails safe (escalate).
fn parse_verdict(text: &str) -> VetVerdict {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return VetVerdict::escalate("classifier returned an empty response");
    }
    let upper = trimmed.to_ascii_uppercase();
    if upper.starts_with("ALLOW") {
        return VetVerdict::allow();
    }
    if upper.starts_with("ESCALATE") {
        let reason = trimmed
            .split_once(':')
            .map(|(_, r)| r.trim())
            .filter(|r| !r.is_empty())
            .map(clip)
            .unwrap_or_else(|| "flagged by the safety classifier".to_string());
        return VetVerdict::escalate(reason);
    }
    VetVerdict::escalate(format!("unrecognized classifier reply: {}", clip(trimmed)))
}

/// Cap a reason string at a sane length on a char boundary.
fn clip(s: &str) -> String {
    const MAX: usize = 160;
    if s.len() <= MAX {
        return s.to_string();
    }
    let cut = s.floor_char_boundary(MAX);
    format!("{}…", &s[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_parses() {
        assert!(parse_verdict("ALLOW").allow);
        assert!(parse_verdict("  allow\n").allow);
        assert!(parse_verdict("Allow — looks fine").allow);
    }

    #[test]
    fn escalate_parses_with_reason() {
        let v = parse_verdict("ESCALATE: pipes a remote script into sh");
        assert!(!v.allow);
        assert_eq!(v.reason, "pipes a remote script into sh");
    }

    #[test]
    fn escalate_without_reason_has_default() {
        let v = parse_verdict("escalate");
        assert!(!v.allow);
        assert!(!v.reason.is_empty());
    }

    #[test]
    fn garbage_and_empty_fail_safe() {
        // Anything we can't read is treated as "escalate", never "allow".
        for reply in ["", "   ", "maybe?", "yes", "no", "I think it's fine"] {
            assert!(
                !parse_verdict(reply).allow,
                "expected escalate (fail-safe) for {reply:?}",
            );
        }
    }
}
