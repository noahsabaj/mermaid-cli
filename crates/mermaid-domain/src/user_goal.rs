//! What the user is trying to do, as the Auto-mode safety classifier sees it.
//!
//! The classifier judges whether a borderline action serves the user's goal.
//! It used to get only the latest user message, so after "yes, go ahead" it
//! had no idea what "ahead" was, and after a compaction it got the checkpoint
//! header instead of a request. [`user_goal`] gathers the conversation that
//! led to the action instead: the user's own messages, the compaction summary
//! when earlier turns were folded away, and the agent's reply that a short
//! answer is agreeing to.
//!
//! Everything is clipped to a fixed budget. The classifier call happens only
//! for actions the rule engine could not settle on its own, so a few thousand
//! extra input tokens there is a small price for a judgment that can see what
//! it is judging.

use mermaid_model::models::{ChatMessage, ChatMessageKind, MessageRole};

use crate::state::Session;

/// One user message is clipped to this many bytes.
const MAX_REQUEST: usize = 2_000;
/// All kept user messages together. The first and the latest are always kept;
/// the most recent others fill what is left.
const MAX_REQUESTS_TOTAL: usize = 8_000;
/// The compaction summary is clipped to this many bytes (its head).
const MAX_SUMMARY: usize = 6_000;
/// The agent's prior reply is clipped to this many bytes (its tail, where the
/// proposal or question a short answer responds to usually sits).
const MAX_PRIOR_REPLY: usize = 2_000;

/// The conversation context the Auto-mode classifier judges an action against.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserGoal {
    /// The latest compaction checkpoint, when earlier turns were summarized
    /// away. Written by the model, not the user.
    pub summary: Option<String>,
    /// The user's own messages since that checkpoint, oldest first. The last
    /// one is the latest message.
    pub requests: Vec<String>,
    /// User messages left out of `requests` to fit the budget. They sit
    /// between the first kept message and the rest.
    pub omitted: usize,
    /// The agent's last reply before the latest user message: what a short
    /// answer like "yes, go ahead" is agreeing to. Written by the model, not
    /// the user.
    pub prior_reply: Option<String>,
}

impl UserGoal {
    /// True when there is nothing to show the classifier at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.summary.is_none() && self.requests.is_empty() && self.prior_reply.is_none()
    }

    /// Just one request, for tests and callers with no conversation.
    #[must_use]
    pub fn from_request(text: impl Into<String>) -> Self {
        Self {
            requests: vec![text.into()],
            ..Self::default()
        }
    }
}

/// Build the classifier's view of the user's goal from the session history.
#[must_use]
pub fn user_goal(session: &Session) -> UserGoal {
    goal_from_messages(session.messages())
}

fn goal_from_messages(messages: &[ChatMessage]) -> UserGoal {
    // Everything before the latest checkpoint is summarized by it.
    let checkpoint = messages
        .iter()
        .rposition(|m| m.role == MessageRole::User && m.kind == ChatMessageKind::ContextCheckpoint);
    let summary = checkpoint
        .map(|i| clip_head(messages[i].content.trim(), MAX_SUMMARY))
        .filter(|s| !s.is_empty());
    let start = checkpoint.map_or(0, |i| i + 1);

    let user_indices: Vec<usize> = (start..messages.len())
        .filter(|&i| {
            let m = &messages[i];
            m.role == MessageRole::User
                && m.kind != ChatMessageKind::ContextCheckpoint
                && !m.content.trim().is_empty()
        })
        .collect();

    let (requests, omitted) = pick_requests(
        &user_indices
            .iter()
            .map(|&i| clip_head(messages[i].content.trim(), MAX_REQUEST))
            .collect::<Vec<_>>(),
    );

    let prior_reply = user_indices.last().and_then(|&latest| {
        let floor = user_indices
            .iter()
            .rev()
            .nth(1)
            .map_or(start, |&previous| previous + 1);
        messages[floor..latest]
            .iter()
            .rev()
            .find(|m| {
                m.role == MessageRole::Assistant
                    && matches!(
                        m.kind,
                        ChatMessageKind::Normal
                            | ChatMessageKind::Continuation
                            | ChatMessageKind::Unknown
                    )
                    && !m.content.trim().is_empty()
            })
            .map(|m| clip_tail(m.content.trim(), MAX_PRIOR_REPLY))
    });

    UserGoal {
        summary,
        requests,
        omitted,
        prior_reply,
    }
}

/// Keep the first and the latest request always, then as many of the most
/// recent others as fit the budget. Returns the kept requests in order and how
/// many were left out.
fn pick_requests(all: &[String]) -> (Vec<String>, usize) {
    if all.len() <= 2 {
        return (all.to_vec(), 0);
    }
    let first = &all[0];
    let last = &all[all.len() - 1];
    let mut used = first.len() + last.len();
    let middle = &all[1..all.len() - 1];
    let mut kept_from = middle.len();
    for (i, text) in middle.iter().enumerate().rev() {
        if used + text.len() > MAX_REQUESTS_TOTAL {
            break;
        }
        used += text.len();
        kept_from = i;
    }
    let mut kept = Vec::with_capacity(2 + middle.len() - kept_from);
    kept.push(first.clone());
    kept.extend(middle[kept_from..].iter().cloned());
    kept.push(last.clone());
    (kept, kept_from)
}

fn clip_head(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!("{}…", &text[..text.floor_char_boundary(max)])
}

fn clip_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    format!("…{}", &text[text.ceil_char_boundary(text.len() - max)..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> ChatMessage {
        ChatMessage::user(text)
    }

    fn agent(text: &str) -> ChatMessage {
        ChatMessage::assistant(text)
    }

    fn tool(text: &str) -> ChatMessage {
        let mut m = ChatMessage::assistant(text);
        m.role = MessageRole::Tool;
        m
    }

    #[test]
    fn a_short_go_ahead_carries_the_request_and_the_proposal_it_approves() {
        let goal = goal_from_messages(&[
            user("cargo test fails. Fix it and commit the fix, but show me the plan first."),
            agent("I'll read the code."),
            tool("fn median(...)"),
            agent("The median is off by one. I'll change `len / 2` and then commit. OK?"),
            user("yes, go ahead"),
        ]);
        assert_eq!(
            goal.requests,
            vec![
                "cargo test fails. Fix it and commit the fix, but show me the plan first.",
                "yes, go ahead",
            ]
        );
        assert_eq!(
            goal.prior_reply.as_deref(),
            Some("The median is off by one. I'll change `len / 2` and then commit. OK?")
        );
        assert!(goal.summary.is_none());
        assert_eq!(goal.omitted, 0);
    }

    #[test]
    fn the_prior_reply_never_reaches_back_past_the_previous_user_message() {
        let goal = goal_from_messages(&[
            user("first"),
            agent("an old reply"),
            user("second"),
            tool("output"),
            user("third"),
        ]);
        assert_eq!(goal.prior_reply, None);
    }

    #[test]
    fn a_checkpoint_becomes_the_summary_and_hides_what_it_replaced() {
        let mut checkpoint = user("# MERMAID CONTEXT CHECKPOINT\n\nGoal: migrate the DB.");
        checkpoint.kind = ChatMessageKind::ContextCheckpoint;
        let mut receipt = agent("Context compacted.");
        receipt.kind = ChatMessageKind::ContextCheckpoint;
        let goal = goal_from_messages(&[
            user("an old request"),
            checkpoint,
            receipt,
            user("continue"),
        ]);
        assert_eq!(goal.requests, vec!["continue"]);
        assert!(goal.summary.as_deref().unwrap().contains("migrate the DB"));
        // The checkpoint receipt is not a reply the user answered.
        assert_eq!(goal.prior_reply, None);
    }

    #[test]
    fn a_long_conversation_keeps_the_first_and_latest_requests() {
        let filler = "x".repeat(1_500);
        let mut messages = vec![user("the original goal")];
        for i in 0..20 {
            messages.push(agent("ok"));
            messages.push(user(&format!("{i} {filler}")));
        }
        messages.push(agent("done?"));
        messages.push(user("yes"));
        let goal = goal_from_messages(&messages);
        assert_eq!(goal.requests.first().unwrap(), "the original goal");
        assert_eq!(goal.requests.last().unwrap(), "yes");
        assert!(goal.omitted > 0);
        assert_eq!(goal.requests.len() + goal.omitted, 22);
        let total: usize = goal.requests.iter().map(String::len).sum();
        assert!(total <= MAX_REQUESTS_TOTAL, "{total}");
        // The kept middle is the most recent stretch.
        assert!(goal.requests[goal.requests.len() - 2].starts_with("19 "));
    }

    #[test]
    fn clipping_keeps_the_head_of_a_request_and_the_tail_of_a_reply() {
        let long_request = format!("START {}", "a".repeat(5_000));
        let long_reply = format!("{} shall I proceed?", "b".repeat(5_000));
        let goal = goal_from_messages(&[user("go"), agent(&long_reply), user(&long_request)]);
        let latest = goal.requests.last().unwrap();
        assert!(latest.starts_with("START") && latest.len() <= MAX_REQUEST + 3);
        let reply = goal.prior_reply.unwrap();
        assert!(reply.ends_with("shall I proceed?") && reply.len() <= MAX_PRIOR_REPLY + 3);
    }

    #[test]
    fn an_empty_session_has_no_goal() {
        assert!(goal_from_messages(&[]).is_empty());
    }
}
