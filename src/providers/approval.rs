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
            ApprovalChoice::Approve => Self::Approve,
            ApprovalChoice::ApproveAlways => Self::ApproveAlways,
            ApprovalChoice::Deny => Self::Deny,
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
    #[must_use]
    pub fn new(msg_tx: mpsc::Sender<Msg>) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            allowlist: Arc::new(Mutex::new(HashSet::new())),
            msg_tx,
        }
    }

    /// True if the user already chose "don't ask again" for this key.
    #[must_use]
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
/// focus, what desktop state exists, which untrusted server answers), not just their
/// arguments. A blanket "don't ask again" for these is unsafe — it would mean
/// "always type anything" or "always run any MCP tool" — so they are
/// non-allowlistable: an empty key, which the gate and modal treat as "no
/// approve-always" (#6, #31).
/// The filesystem tools whose external-path approvals are scoped to the
/// target's directory (see [`allowlist_key`]).
const FILE_TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "apply_patch",
    "delete_file",
    "create_directory",
];

const NON_ALLOWLISTABLE_TOOLS: &[&str] = &[
    "type_text",
    "press_key",
    "click",
    "mouse_move",
    "scroll",
    "mcp_proxy",
];

/// Extract the normalized lowercase authority (`host` or `host:port` when port
/// is non-default) from a raw URL or command string (e.g. `web_fetch https://docs.x.ai/path`
/// or `https://localhost:8080/api`).
#[must_use]
pub fn extract_url_authority(raw: &str) -> Option<String> {
    let input = raw.strip_prefix("web_fetch ").unwrap_or(raw).trim();
    if input.is_empty() {
        return None;
    }
    // Try parsing as a full URL
    if let Ok(url) = reqwest::Url::parse(input)
        && let Some(host) = url.host_str()
    {
        let host_lower = host.to_ascii_lowercase();
        if let Some(port) = url.port() {
            return Some(format!("{host_lower}:{port}"));
        }
        return Some(host_lower);
    }
    // If no scheme was provided (e.g. "docs.x.ai/path" or "localhost:8080"), try with "https://"
    if let Ok(url) = reqwest::Url::parse(&format!("https://{input}"))
        && let Some(host) = url.host_str()
    {
        let host_lower = host.to_ascii_lowercase();
        if let Some(port) = url.port() {
            return Some(format!("{host_lower}:{port}"));
        }
        return Some(host_lower);
    }
    None
}

/// Check if a URL or command matches any entry in `allowed_domains`.
#[must_use]
pub fn is_domain_allowed(allowed_domains: &[String], url_or_command: &str) -> bool {
    if allowed_domains.is_empty() {
        return false;
    }
    let Some(target_auth) = extract_url_authority(url_or_command) else {
        return false;
    };
    allowed_domains.iter().any(|allowed| {
        extract_url_authority(allowed)
            .as_deref()
            .is_some_and(|a| a.eq_ignore_ascii_case(&target_auth))
            || allowed.trim().eq_ignore_ascii_case(&target_auth)
    })
}

/// Compute the session "don't ask again" allowlist key.
///
/// - Content-bearing external desktop/MCP tools ([`NON_ALLOWLISTABLE_TOOLS`]) return an
///   **empty** key, meaning non-allowlistable: the modal hides the
///   approve-always option and the broker never persists an entry.
/// - `execute_command` keys on the **full normalized command** (whitespace
///   collapsed), so approving `curl https://safe.example` does NOT also clear
///   `curl https://evil.example` — argv0 keying was too coarse for a tool whose
///   danger lives entirely in its arguments (#6). A command run OUTSIDE the
///   project additionally keys on its working directory: `make` approved in
///   the project must not cover `make` in a directory whose Makefile the
///   model chose later, since the same string runs different code there.
/// - `web_fetch` keys on the normalized authority (`web_fetch:<host>` or `web_fetch:<host>:<port>`),
///   so approving one URL on a domain clears subsequent fetches to that domain.
/// - `web_search` keys on `"web_search"`.
/// - Everything else keys per-tool.
#[must_use]
pub fn allowlist_key(tool: &str, command: Option<&str>, external_path: Option<&str>) -> String {
    if NON_ALLOWLISTABLE_TOOLS.contains(&tool) {
        return String::new();
    }
    // A file tool reaching outside the project is allowlisted per directory,
    // never per tool: "don't ask again" for `read_file` on `~/notes/a.md` must
    // not silently cover `~/.ssh/id_rsa` later in the session.
    if let Some(path) = external_path
        && FILE_TOOLS.contains(&tool)
    {
        let dir = std::path::Path::new(path)
            .parent()
            .map_or_else(|| path.to_string(), |d| d.display().to_string());
        return format!("{tool}:{dir}");
    }
    if tool == "execute_command" {
        // For an out-of-project command the gate's `path` IS the working
        // directory (the exec tool sets it to the effective workdir on
        // escalation), so it goes into the key whole.
        let scope = external_path.map_or_else(String::new, |dir| format!("{dir}:"));
        if let Some(cmd) = command {
            let normalized = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
            if !normalized.is_empty() {
                return format!("execute_command:{scope}{normalized}");
            }
        }
        return format!("execute_command:{scope}")
            .trim_end_matches(':')
            .to_string();
    }
    if tool == "web_fetch" {
        if let Some(cmd) = command
            && let Some(auth) = extract_url_authority(cmd)
        {
            return format!("web_fetch:{auth}");
        }
        return "web_fetch".to_string();
    }
    if tool == "web_search" {
        return "web_search".to_string();
    }
    tool.to_string()
}

#[cfg(test)]
mod tests {
    /// The same command string in two out-of-project directories runs two
    /// different Makefiles; one approve-always must not cover both, and an
    /// in-project approval must not cover either.
    #[test]
    fn execute_command_key_scopes_an_external_cwd() {
        let inside = allowlist_key("execute_command", Some("make  test"), None);
        let a = allowlist_key("execute_command", Some("make test"), Some("/srv/a"));
        let b = allowlist_key("execute_command", Some("make test"), Some("/srv/b"));
        assert_eq!(inside, "execute_command:make test");
        assert_eq!(a, "execute_command:/srv/a:make test");
        assert_ne!(a, b);
        assert_ne!(a, inside);
        assert_eq!(
            allowlist_key("execute_command", None, Some("/srv/a")),
            "execute_command:/srv/a"
        );
        assert_eq!(
            allowlist_key("execute_command", None, None),
            "execute_command"
        );
    }

    use super::*;

    #[test]
    fn allowlist_key_is_per_tool_with_full_command() {
        assert_eq!(allowlist_key("write_file", None, None), "write_file");
        // #6: execute_command keys on the FULL normalized command, so approving
        // one invocation can't clear a different-argument one.
        assert_eq!(
            allowlist_key("execute_command", Some("ls -la"), None),
            "execute_command:ls -la"
        );
        // No command falls back to the bare tool key.
        assert_eq!(
            allowlist_key("execute_command", None, None),
            "execute_command"
        );
    }

    #[test]
    fn allowlist_key_distinguishes_argument_variants() {
        // #6: approving `curl https://safe` must NOT also clear `curl https://evil`.
        assert_ne!(
            allowlist_key("execute_command", Some("curl https://safe.example"), None),
            allowlist_key("execute_command", Some("curl https://evil.example"), None),
        );
        // Whitespace is normalized so trivial spacing differences still match.
        assert_eq!(
            allowlist_key("execute_command", Some("cargo   build"), None),
            allowlist_key("execute_command", Some("cargo build"), None),
        );
        assert_eq!(
            allowlist_key("execute_command", Some("npm test"), None),
            "execute_command:npm test"
        );
        assert_ne!(
            allowlist_key("execute_command", Some("npm test"), None),
            allowlist_key("execute_command", Some("npm run build"), None),
        );
    }

    #[test]
    fn web_fetch_and_search_allowlist_keys() {
        assert_eq!(
            allowlist_key(
                "web_fetch",
                Some("web_fetch https://docs.x.ai/docs/overview"),
                None,
            ),
            "web_fetch:docs.x.ai"
        );
        assert_eq!(
            allowlist_key("web_fetch", Some("https://DOCS.X.AI/api"), None),
            "web_fetch:docs.x.ai"
        );
        assert_eq!(
            allowlist_key(
                "web_fetch",
                Some("web_fetch http://localhost:8080/query"),
                None
            ),
            "web_fetch:localhost:8080"
        );
        assert_eq!(allowlist_key("web_fetch", None, None), "web_fetch");
        assert_eq!(
            allowlist_key("web_search", Some("web_search rust documentation"), None),
            "web_search"
        );
        assert_eq!(allowlist_key("web_search", None, None), "web_search");
    }

    #[test]
    fn domain_allowlist_matching() {
        let allowed = vec!["docs.x.ai".to_string(), "localhost:8080".to_string()];
        assert!(is_domain_allowed(&allowed, "https://docs.x.ai/overview"));
        assert!(is_domain_allowed(
            &allowed,
            "web_fetch https://DOCS.X.AI/api"
        ));
        assert!(is_domain_allowed(&allowed, "http://localhost:8080/metrics"));
        assert!(!is_domain_allowed(&allowed, "https://api.x.ai/v1"));
        assert!(!is_domain_allowed(
            &allowed,
            "http://localhost:3000/metrics"
        ));
        assert!(!is_domain_allowed(&[], "https://docs.x.ai/overview"));
    }

    #[test]
    fn content_bearing_tools_are_non_allowlistable() {
        // #6/#31: a blanket "approve always" for these is unsafe (their risk is
        // context-dependent), so the key is empty ⇒ non-allowlistable.
        for tool in [
            "type_text",
            "press_key",
            "click",
            "mouse_move",
            "scroll",
            "mcp_proxy",
        ] {
            assert_eq!(
                allowlist_key(tool, None, None),
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
