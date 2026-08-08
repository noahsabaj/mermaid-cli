//! Inline approval broker for interactive `ask` mode + Auto-mode escalations.
//!
//! When a tool is gated and a human decision is needed, the policy gate calls
//! [`ApprovalBroker::request`] (the broker is injected into [`ExecContext`]).
//! That sends a `Msg::ApprovalRequested` to the reducer — which renders a modal
//! — and parks the tool task on a oneshot until the user answers. The reducer
//! (pure) emits `Cmd::ResolveApproval`; the `EffectRunner` calls
//! [`ApprovalBroker::resolve`]; the parked task wakes and the tool proceeds or
//! is denied. The turn pauses for free: while parked, the task hasn't sent
//! `Msg::ToolFinished`, so its outcome slot stays `None` and no follow-up model
//! call fires.
//!
//! Lock discipline: `pending`/`allowlist` use [`std::sync::Mutex`] (whose guard
//! is `!Send`) so a guard accidentally held across an `.await` fails to
//! compile. Every critical section here is tiny and fully synchronous.
//!
//! [`ExecContext`]: crate::providers::ctx::ExecContext

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use mermaid_domain::{ApprovalChoice, ApprovalKind, Msg, ToolCallId, TurnId};

/// The user's decision, broker-side. `Cmd::ResolveApproval` carries the pure
/// `domain::ApprovalChoice`; the `EffectRunner` maps it to this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approve,
    ApproveAlways,
    Deny,
}

impl From<ApprovalChoice> for ApprovalDecision {
    fn from(choice: ApprovalChoice) -> Self {
        match choice {
            ApprovalChoice::Approve => ApprovalDecision::Approve,
            ApprovalChoice::ApproveAlways => ApprovalDecision::ApproveAlways,
            ApprovalChoice::Deny => ApprovalDecision::Deny,
        }
    }
}

struct PendingEntry {
    tx: oneshot::Sender<ApprovalDecision>,
    allowlist_key: String,
}

/// Owned by the interactive `EffectRunner`, cloned into each `ExecContext`.
/// Absent (`None`) in headless runs — the gate then falls back to the
/// out-of-band DB-approval flow.
#[derive(Clone)]
pub struct ApprovalBroker {
    pending: Arc<Mutex<HashMap<ToolCallId, PendingEntry>>>,
    allowlist: Arc<Mutex<HashSet<String>>>,
    msg_tx: mpsc::Sender<Msg>,
}

impl ApprovalBroker {
    pub fn new(msg_tx: mpsc::Sender<Msg>) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            allowlist: Arc::new(Mutex::new(HashSet::new())),
            msg_tx,
        }
    }

    /// True if the user already chose "don't ask again" for this key.
    pub fn is_allowlisted(&self, key: &str) -> bool {
        self.allowlist
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(key)
    }

    /// Prompt the user and block until they answer (or the turn is cancelled).
    /// Fail-safe: a dropped sender, a gone reducer, or a cancel all resolve to
    /// `Deny`.
    #[expect(clippy::too_many_arguments)]
    pub async fn request(
        &self,
        token: &CancellationToken,
        turn: TurnId,
        call_id: ToolCallId,
        tool: String,
        risk: String,
        kind: ApprovalKind,
        prompt: String,
        allowlist_key: String,
    ) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        // Register the sender. The guard drops at the end of this statement —
        // never held across the awaits below.
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                call_id,
                PendingEntry {
                    tx,
                    allowlist_key: allowlist_key.clone(),
                },
            );

        let sent = self
            .msg_tx
            .send(Msg::ApprovalRequested {
                turn,
                call_id,
                tool,
                risk,
                kind,
                prompt,
                allowlist_scope: allowlist_key,
            })
            .await;
        if sent.is_err() {
            // Reducer is gone — clean up and deny.
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&call_id);
            return ApprovalDecision::Deny;
        }

        tokio::select! {
            biased;
            _ = token.cancelled() => {
                self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&call_id);
                ApprovalDecision::Deny
            }
            decision = rx => decision.unwrap_or(ApprovalDecision::Deny),
        }
    }

    /// Deliver the user's decision to the parked task. On `ApproveAlways`,
    /// remember the key so future matching actions skip the prompt this session.
    pub fn resolve(&self, call_id: ToolCallId, decision: ApprovalDecision) {
        let entry = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&call_id);
        if let Some(entry) = entry {
            // An empty key marks a non-allowlistable action (content-bearing
            // external tools): never persist it, so "approve always" can't be
            // recorded for them even if the choice somehow arrives (#6, #31).
            if decision == ApprovalDecision::ApproveAlways && !entry.allowlist_key.is_empty() {
                self.allowlist
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .insert(entry.allowlist_key);
            }
            let _ = entry.tx.send(decision);
        }
    }
}

/// External tools whose risk depends on live runtime context (which window has
/// focus, what page is loaded, which untrusted server answers), not just their
/// arguments. A blanket "don't ask again" for these is unsafe — it would mean
/// "always send any URL/query", "always type anything", or "always run any MCP
/// tool" — so they are non-allowlistable: an empty key, which the gate and
/// modal treat as "no approve-always" (#6, #31).
const NON_ALLOWLISTABLE_TOOLS: &[&str] = &[
    "web_fetch",
    "web_search",
    "type_text",
    "press_key",
    "click",
    "mouse_move",
    "scroll",
    "mcp_proxy",
];

/// Compute the session "don't ask again" allowlist key.
///
/// - Content-bearing external tools ([`NON_ALLOWLISTABLE_TOOLS`]) return an
///   **empty** key, meaning non-allowlistable: the modal hides the
///   approve-always option and the broker never persists an entry.
/// - `execute_command` keys on the **full normalized command** (whitespace
///   collapsed), so approving `curl https://safe.example` does NOT also clear
///   `curl https://evil.example` — argv0 keying was too coarse for a tool whose
///   danger lives entirely in its arguments (#6).
/// - Everything else keys per-tool.
pub fn allowlist_key(tool: &str, command: Option<&str>) -> String {
    if NON_ALLOWLISTABLE_TOOLS.contains(&tool) {
        return String::new();
    }
    if tool == "execute_command" {
        if let Some(cmd) = command {
            let normalized = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
            if !normalized.is_empty() {
                return format!("execute_command:{normalized}");
            }
        }
        return "execute_command".to_string();
    }
    tool.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_key_is_per_tool_with_full_command() {
        assert_eq!(allowlist_key("write_file", None), "write_file");
        // #6: execute_command keys on the FULL normalized command, so approving
        // one invocation can't clear a different-argument one.
        assert_eq!(
            allowlist_key("execute_command", Some("ls -la")),
            "execute_command:ls -la"
        );
        // No command falls back to the bare tool key.
        assert_eq!(allowlist_key("execute_command", None), "execute_command");
    }

    #[test]
    fn allowlist_key_distinguishes_argument_variants() {
        // #6: approving `curl https://safe` must NOT also clear `curl https://evil`.
        assert_ne!(
            allowlist_key("execute_command", Some("curl https://safe.example")),
            allowlist_key("execute_command", Some("curl https://evil.example")),
        );
        // Whitespace is normalized so trivial spacing differences still match.
        assert_eq!(
            allowlist_key("execute_command", Some("cargo   build")),
            allowlist_key("execute_command", Some("cargo build")),
        );
        assert_eq!(
            allowlist_key("execute_command", Some("npm test")),
            "execute_command:npm test"
        );
        assert_ne!(
            allowlist_key("execute_command", Some("npm test")),
            allowlist_key("execute_command", Some("npm run build")),
        );
    }

    #[test]
    fn content_bearing_tools_are_non_allowlistable() {
        // #6/#31: a blanket "approve always" for these is unsafe (their risk is
        // context-dependent), so the key is empty ⇒ non-allowlistable.
        for tool in [
            "web_fetch",
            "web_search",
            "type_text",
            "press_key",
            "click",
            "mouse_move",
            "scroll",
            "mcp_proxy",
        ] {
            assert_eq!(
                allowlist_key(tool, None),
                "",
                "{tool} must be non-allowlistable"
            );
        }
    }

    #[tokio::test]
    async fn resolve_delivers_decision_and_approve_always_allowlists() {
        let (tx, _rx) = mpsc::channel::<Msg>(8);
        let broker = ApprovalBroker::new(tx);
        let token = CancellationToken::new();

        // Spawn a request; resolve it from "the reducer side".
        let b2 = broker.clone();
        let handle = tokio::spawn(async move {
            b2.request(
                &CancellationToken::new(),
                TurnId(1),
                ToolCallId(1),
                "execute_command".to_string(),
                "shell_mutation".to_string(),
                ApprovalKind::Shell,
                "$ npm test".to_string(),
                "execute_command:npm".to_string(),
            )
            .await
        });
        // Give the task a beat to register, then resolve.
        tokio::task::yield_now().await;
        // Poll until registered (the send + insert happen before the await).
        for _ in 0..100 {
            broker.resolve(ToolCallId(1), ApprovalDecision::ApproveAlways);
            if broker.is_allowlisted("execute_command:npm") {
                break;
            }
            tokio::task::yield_now().await;
        }
        let decision = handle.await.unwrap();
        assert_eq!(decision, ApprovalDecision::ApproveAlways);
        assert!(broker.is_allowlisted("execute_command:npm"));
        let _ = token; // silence unused in this path
    }

    #[tokio::test]
    async fn cancel_token_denies() {
        let (tx, _rx) = mpsc::channel::<Msg>(8);
        let broker = ApprovalBroker::new(tx);
        let token = CancellationToken::new();
        let token2 = token.clone();
        let handle = tokio::spawn(async move {
            broker
                .request(
                    &token2,
                    TurnId(1),
                    ToolCallId(2),
                    "web_fetch".to_string(),
                    "network".to_string(),
                    ApprovalKind::Web,
                    "web_fetch https://x".to_string(),
                    "web_fetch".to_string(),
                )
                .await
        });
        tokio::task::yield_now().await;
        token.cancel();
        assert_eq!(handle.await.unwrap(), ApprovalDecision::Deny);
    }
}
