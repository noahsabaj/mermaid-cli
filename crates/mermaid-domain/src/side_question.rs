//! `/btw` side questions: ask about the session without touching it.
//!
//! A side question runs as its own one-shot model call beside the main run.
//! It sees the conversation so far (everything committed, not the reply still
//! streaming) plus the newest earlier side exchanges, and its answer shows in
//! a dismissible pane under the composer. Neither the question nor the answer
//! ever enters `session.conversation`: nothing here is persisted, compacted or
//! sent on a later main turn, and the main turn is never interrupted.
//!
//! The thread lives in [`SideQuestions`] for the life of the process (and the
//! conversation: a `/clear`, `/load` or rewind fork starts a fresh one), the
//! way Claude Code keeps its `/btw` thread in memory until exit.

use serde::{Deserialize, Serialize};

use crate::cmd::ChatRequest;
use crate::state::State;
use mermaid_model::models::ChatMessage;

/// How many earlier answered exchanges ride along with each new side question.
pub const SIDE_QUESTION_REPLAY: usize = 20;

/// How many earlier questions the pane lists above the one on view.
pub const SIDE_QUESTION_RECENT_SHOWN: usize = 5;

/// Where one side question stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideStatus {
    /// The call is in flight; `answer` holds what has streamed so far.
    Answering,
    /// The answer is complete.
    Done,
    /// The call failed; the text says why.
    Failed(String),
}

/// One `/btw` question and its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideExchange {
    pub id: u64,
    pub question: String,
    pub answer: String,
    pub status: SideStatus,
}

/// How a side-question call ended, as reported by the effect layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideOutcome {
    /// The model finished. `tried_tools` is set when it asked for a tool
    /// anyway: nothing ran, and the answer says so.
    Done { tried_tools: bool },
    /// The call failed before or during the answer.
    Failed(String),
}

/// The pane's position: which exchange is on view and how far its answer is
/// scrolled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SideView {
    pub index: usize,
    pub scroll: u16,
}

/// The session's side-question thread plus the pane over it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideQuestions {
    /// Oldest first.
    pub exchanges: Vec<SideExchange>,
    /// `Some` while the pane is open.
    pub view: Option<SideView>,
    next_id: u64,
}

/// Appended when the model asked for a tool: side questions have none.
pub const NO_TOOLS_NOTE: &str =
    "(Side questions cannot use tools. The model asked for one, but nothing was run.)";

impl SideQuestions {
    /// Record a new question and open the pane on it. Returns its id, which
    /// the effect layer echoes on every chunk and on the finish.
    pub fn ask(&mut self, question: String) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.exchanges.push(SideExchange {
            id,
            question,
            answer: String::new(),
            status: SideStatus::Answering,
        });
        self.view = Some(SideView {
            index: self.exchanges.len() - 1,
            scroll: 0,
        });
        id
    }

    /// Reopen the pane on the newest exchange. `false` when there is none.
    pub fn reopen(&mut self) -> bool {
        if self.exchanges.is_empty() {
            return false;
        }
        self.view = Some(SideView {
            index: self.exchanges.len() - 1,
            scroll: 0,
        });
        true
    }

    /// Forget the whole thread and close the pane. In-flight answers for the
    /// forgotten ids are dropped on arrival.
    pub fn reset(&mut self) {
        self.exchanges.clear();
        self.view = None;
    }

    /// The exchange on view, when the pane is open.
    #[must_use]
    pub fn viewed(&self) -> Option<&SideExchange> {
        self.view.and_then(|v| self.exchanges.get(v.index))
    }

    /// Append a streamed chunk. A chunk for an id no longer held is dropped.
    pub fn push_chunk(&mut self, id: u64, chunk: &str) {
        if let Some(ex) = self.exchanges.iter_mut().find(|e| e.id == id) {
            ex.answer.push_str(chunk);
        }
    }

    /// Settle an exchange. Returns `true` when it was still held, so the
    /// caller can tell the user an answer landed while the pane was closed.
    pub fn finish(&mut self, id: u64, outcome: SideOutcome) -> bool {
        let Some(ex) = self.exchanges.iter_mut().find(|e| e.id == id) else {
            return false;
        };
        match outcome {
            SideOutcome::Done { tried_tools } => {
                if tried_tools {
                    if !ex.answer.trim().is_empty() {
                        ex.answer.push_str("\n\n");
                    }
                    ex.answer.push_str(NO_TOOLS_NOTE);
                }
                ex.status = if ex.answer.trim().is_empty() {
                    SideStatus::Failed("The model returned an empty answer.".to_string())
                } else {
                    SideStatus::Done
                };
            },
            SideOutcome::Failed(reason) => ex.status = SideStatus::Failed(reason),
        }
        true
    }

    /// Close the pane. The thread stays; `/btw` alone reopens it.
    pub fn dismiss(&mut self) {
        self.view = None;
    }

    /// Step to the next older exchange.
    pub fn older(&mut self) {
        if let Some(v) = self.view.as_mut()
            && v.index > 0
        {
            v.index -= 1;
            v.scroll = 0;
        }
    }

    /// Step toward the newest exchange.
    pub fn newer(&mut self) {
        let last = self.exchanges.len().saturating_sub(1);
        if let Some(v) = self.view.as_mut()
            && v.index < last
        {
            v.index += 1;
            v.scroll = 0;
        }
    }

    /// Scroll the answer by `delta` lines. The upper bound is the answer's
    /// line count; the pane clamps again to what actually fits.
    pub fn scroll(&mut self, delta: i32) {
        let Some(max) = self.viewed().map(|e| answer_row_bound(&e.answer)) else {
            return;
        };
        if let Some(v) = self.view.as_mut() {
            let next = (i64::from(v.scroll) + i64::from(delta)).clamp(0, i64::from(max));
            v.scroll = u16::try_from(next).unwrap_or(u16::MAX);
        }
    }

    /// Drop every exchange except the one on view, so later side questions
    /// no longer see them.
    pub fn clear_earlier(&mut self) {
        let Some(v) = self.view else {
            return;
        };
        let kept = self.exchanges.swap_remove(v.index);
        self.exchanges = vec![kept];
        self.view = Some(SideView {
            index: 0,
            scroll: v.scroll,
        });
    }

    /// The newest answered exchanges, oldest first, for replay.
    fn replay(&self) -> Vec<&SideExchange> {
        let mut answered: Vec<&SideExchange> = self
            .exchanges
            .iter()
            .filter(|e| e.status == SideStatus::Done)
            .collect();
        let skip = answered.len().saturating_sub(SIDE_QUESTION_REPLAY);
        answered.drain(..skip);
        answered
    }
}

/// An upper bound on the rows an answer can wrap to: its lines, plus one more
/// for every 40 cells a long line runs past. Generous on purpose; the pane
/// clamps to the real wrap.
fn answer_row_bound(answer: &str) -> u16 {
    let rows: usize = answer.lines().map(|l| 1 + l.chars().count() / 40).sum();
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// The words around a side question on the wire. The model has to know it is
/// answering beside its main task, from what it already has, with no tools.
fn side_question_prompt(question: &str) -> String {
    format!(
        "<side_question>\n\
         The user asks a side question while your main task continues separately. \
         Answer it only from what is already in this conversation. \
         Do not call tools and do not offer to run anything: nothing you request here runs. \
         Do not continue the main task. Answer briefly. \
         If the conversation does not hold the answer, say so.\n\n\
         {question}\n\
         </side_question>"
    )
}

/// The task a forked side question starts with. The child already holds the
/// conversation; this hands it the side exchange and says what to do with it.
#[must_use]
pub fn fork_prompt(question: &str, answer: &str) -> String {
    format!(
        "During this session the user asked a side question, and it was answered \
         without tools. Continue from that exchange with full tool access: do the work \
         it points to (check, investigate, or fix), then report what you found or changed.\n\n\
         Side question: {question}\n\n\
         Answer given: {answer}"
    )
}

/// The request for a side question: the main turn's request as it stands now
/// (same system prompt, instructions, tools and history, so a warm prompt
/// cache still applies), then the earlier side exchanges, then the question.
///
/// Built from `state` BEFORE the new question is recorded, so it never
/// replays itself.
#[must_use]
pub fn side_question_request(state: &State, question: &str) -> ChatRequest {
    let mut request = crate::build_chat_request(state);
    for earlier in state.side_questions.replay() {
        request
            .messages
            .push(ChatMessage::user(side_question_prompt(&earlier.question)));
        request
            .messages
            .push(ChatMessage::assistant(earlier.answer.clone()));
    }
    request
        .messages
        .push(ChatMessage::user(side_question_prompt(question)));
    // A side question never compacts: it must not change the conversation,
    // and a summary of it would be thrown away.
    request.suppress_auto_compact = true;
    request.requested_compaction = None;
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(q: &mut SideQuestions, question: &str, answer: &str) -> u64 {
        let id = q.ask(question.to_string());
        q.push_chunk(id, answer);
        assert!(q.finish(id, SideOutcome::Done { tried_tools: false }));
        id
    }

    #[test]
    fn ask_opens_the_pane_on_the_new_question() {
        let mut q = SideQuestions::default();
        let id = q.ask("which file?".to_string());
        assert_eq!(q.viewed().map(|e| e.id), Some(id));
        assert_eq!(q.viewed().map(|e| &e.status), Some(&SideStatus::Answering));
    }

    #[test]
    fn chunks_and_finish_for_an_unknown_id_are_dropped() {
        let mut q = SideQuestions::default();
        q.push_chunk(9, "stray");
        assert!(!q.finish(9, SideOutcome::Done { tried_tools: false }));
        assert!(q.exchanges.is_empty());
    }

    #[test]
    fn a_tool_request_is_noted_and_never_hidden() {
        let mut q = SideQuestions::default();
        let id = q.ask("run the tests?".to_string());
        q.finish(id, SideOutcome::Done { tried_tools: true });
        let ex = q.viewed().unwrap();
        assert_eq!(ex.status, SideStatus::Done);
        assert_eq!(ex.answer, NO_TOOLS_NOTE);
    }

    #[test]
    fn an_empty_answer_reads_as_a_failure() {
        let mut q = SideQuestions::default();
        let id = q.ask("hm?".to_string());
        q.finish(id, SideOutcome::Done { tried_tools: false });
        assert!(matches!(q.viewed().unwrap().status, SideStatus::Failed(_)));
    }

    #[test]
    fn stepping_stays_in_bounds() {
        let mut q = SideQuestions::default();
        done(&mut q, "a", "1");
        done(&mut q, "b", "2");
        q.newer();
        assert_eq!(q.viewed().unwrap().question, "b");
        q.older();
        q.older();
        assert_eq!(q.viewed().unwrap().question, "a");
        q.newer();
        assert_eq!(q.viewed().unwrap().question, "b");
    }

    #[test]
    fn scroll_clamps_at_both_ends() {
        let mut q = SideQuestions::default();
        done(&mut q, "a", "one\ntwo\nthree");
        q.scroll(-4);
        assert_eq!(q.view.unwrap().scroll, 0);
        q.scroll(100);
        assert_eq!(q.view.unwrap().scroll, 3);
    }

    #[test]
    fn clear_earlier_keeps_only_the_exchange_on_view() {
        let mut q = SideQuestions::default();
        done(&mut q, "a", "1");
        done(&mut q, "b", "2");
        done(&mut q, "c", "3");
        q.older();
        q.clear_earlier();
        assert_eq!(q.exchanges.len(), 1);
        assert_eq!(q.viewed().unwrap().question, "b");
    }

    #[test]
    fn replay_takes_the_newest_answered_exchanges() {
        let mut q = SideQuestions::default();
        for i in 0..(SIDE_QUESTION_REPLAY + 3) {
            done(&mut q, &format!("q{i}"), "a");
        }
        let failed = q.ask("broken".to_string());
        q.finish(failed, SideOutcome::Failed("network".to_string()));
        q.ask("pending".to_string());
        let replay = q.replay();
        assert_eq!(replay.len(), SIDE_QUESTION_REPLAY);
        assert_eq!(replay.first().unwrap().question, "q3");
        assert_eq!(
            replay.last().unwrap().question,
            format!("q{}", SIDE_QUESTION_REPLAY + 2)
        );
    }
}
