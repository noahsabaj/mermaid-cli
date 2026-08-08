//! Inline question broker for the `ask_user_question` tool.
//!
//! Generalizes the approval flow (`super::approval::ApprovalBroker`) from a
//! yes/no decision to an arbitrary set of multiple-choice answers. When the
//! tool runs, it calls [`QuestionBroker::request`] (injected into
//! [`ExecContext`]). That sends a `Msg::QuestionAsked` to the reducer — which
//! renders a selectable modal — and parks the tool task on a oneshot until the
//! user answers. The reducer (pure) emits `Cmd::ResolveQuestion`; the
//! `EffectRunner` calls [`QuestionBroker::resolve`]; the parked task wakes with
//! the user's answers and the tool returns them to the model. The turn pauses
//! for free: while parked, the task hasn't sent `Msg::ToolFinished`, so its
//! outcome slot stays `None` and no follow-up model call fires.
//!
//! Lock discipline mirrors the approval broker: `pending` uses
//! [`std::sync::Mutex`] (whose guard is `!Send`) so a guard held across an
//! `.await` fails to compile. Every critical section is tiny and synchronous.
//!
//! [`ExecContext`]: crate::providers::ctx::ExecContext

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use mermaid_domain::{Msg, Question, QuestionResolution, ToolCallId, TurnId};

/// Owned by the interactive `EffectRunner`, cloned into each `ExecContext`.
/// Absent (`None`) in headless runs — the tool then proceeds without a human.
#[derive(Clone)]
pub struct QuestionBroker {
    pending: Arc<Mutex<HashMap<ToolCallId, oneshot::Sender<QuestionResolution>>>>,
    msg_tx: mpsc::Sender<Msg>,
}

impl QuestionBroker {
    #[must_use]
    pub fn new(msg_tx: mpsc::Sender<Msg>) -> Self {
        Self {
            pending: Arc::new(Mutex::new(HashMap::new())),
            msg_tx,
        }
    }

    /// Ask the user a batch of questions and block until they answer (or the
    /// turn is cancelled). Fail-safe: a dropped sender, a gone reducer, or a
    /// cancel all resolve to `Dismissed`.
    pub async fn request(
        &self,
        token: &CancellationToken,
        turn: TurnId,
        call_id: ToolCallId,
        questions: Vec<Question>,
    ) -> QuestionResolution {
        let (tx, rx) = oneshot::channel();
        // Register the sender; the guard drops at the end of this statement —
        // never held across the awaits below.
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(call_id, tx);

        let sent = self
            .msg_tx
            .send(Msg::QuestionAsked {
                turn,
                call_id,
                questions,
            })
            .await;
        if sent.is_err() {
            // Reducer is gone — clean up and dismiss.
            self.pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&call_id);
            return QuestionResolution::Dismissed;
        }

        tokio::select! {
            biased;
            _ = token.cancelled() => {
                self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).remove(&call_id);
                QuestionResolution::Dismissed
            }
            resolution = rx => resolution.unwrap_or(QuestionResolution::Dismissed),
        }
    }

    /// Deliver the user's answers to the parked task.
    pub fn resolve(&self, call_id: ToolCallId, resolution: QuestionResolution) {
        let entry = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&call_id);
        if let Some(tx) = entry {
            let _ = tx.send(resolution);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mermaid_domain::{QuestionAnswer, QuestionOption};

    fn sample_questions() -> Vec<Question> {
        vec![Question {
            header: "Database".to_string(),
            question: "Which database?".to_string(),
            kind: mermaid_domain::QuestionKind::Select,
            options: vec![QuestionOption {
                label: "PostgreSQL".to_string(),
                description: None,
                recommended: true,
                preview: None,
            }],
            memory_key: None,
        }]
    }

    #[tokio::test]
    async fn resolve_delivers_answers() {
        let (tx, _rx) = mpsc::channel::<Msg>(8);
        let broker = QuestionBroker::new(tx);

        let b2 = broker.clone();
        let handle = tokio::spawn(async move {
            b2.request(
                &CancellationToken::new(),
                TurnId(1),
                ToolCallId(1),
                sample_questions(),
            )
            .await
        });
        // Poll until the request has registered, then resolve it.
        let answers = vec![QuestionAnswer {
            header: "Database".to_string(),
            question: "Which database?".to_string(),
            selected: vec!["PostgreSQL".to_string()],
            note: None,
        }];
        for _ in 0..100 {
            broker.resolve(
                ToolCallId(1),
                QuestionResolution::Answered {
                    answers: answers.clone(),
                    remember: false,
                },
            );
            tokio::task::yield_now().await;
            if broker
                .pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_empty()
            {
                break;
            }
        }
        let resolution = handle.await.unwrap();
        assert_eq!(
            resolution,
            QuestionResolution::Answered {
                answers,
                remember: false
            }
        );
    }

    #[tokio::test]
    async fn cancel_token_dismisses() {
        let (tx, _rx) = mpsc::channel::<Msg>(8);
        let broker = QuestionBroker::new(tx);
        let token = CancellationToken::new();
        let token2 = token.clone();
        let handle = tokio::spawn(async move {
            broker
                .request(&token2, TurnId(1), ToolCallId(2), sample_questions())
                .await
        });
        tokio::task::yield_now().await;
        token.cancel();
        assert_eq!(handle.await.unwrap(), QuestionResolution::Dismissed);
    }
}
