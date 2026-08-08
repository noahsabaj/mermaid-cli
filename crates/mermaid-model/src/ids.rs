//! Typed identifiers used throughout the reducer.
//!
//! These are nonces, not indices — the reducer uses them to drop stale
//! effect results that arrive after the turn they belong to has been
//! superseded. `TurnId` is the most important: every `Msg` that carries
//! effect output tags itself with the `TurnId` of the turn that produced
//! it; the reducer compares against `state.turn.id()` and ignores any
//! mismatch. That turns the whole "stale stream event fires after the
//! user cancelled" class of bugs into a type-level non-issue.
//!
//! None of these types do anything clever. They wrap `u64` so they're
//! Copy + Ord + serializable (useful for `--record` / `--replay`), and
//! they're newtypes so the type system catches accidental swaps
//! (a `ToolCallId` can't be used where a `TurnId` is expected).

use serde::{Deserialize, Serialize};
use std::fmt;

/// One "turn" = one user prompt + the entire model+tools cascade that
/// follows, ending when the reducer returns to `TurnState::Idle`. A
/// `CancelTurn` ends the current turn immediately (after cleanup
/// effects dispatch); the next prompt starts a fresh `TurnId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TurnId(pub u64);

impl TurnId {
    pub const ZERO: Self = Self(0);
}

impl fmt::Display for TurnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "turn#{}", self.0)
    }
}

/// Stable identifier for a single tool call inside a turn. The reducer
/// uses this to match `ToolFinished` results back to the slot in
/// `TurnState::ExecutingTools::outcomes` so results can land out of
/// order without ambiguity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ToolCallId(pub u64);

impl fmt::Display for ToolCallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tool#{}", self.0)
    }
}

/// Monotonic ID allocator. `State` owns one of these per "kind" (turn,
/// tool call) and hands out fresh IDs by incrementing. Reset happens
/// only on a full session replay.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct IdAllocator {
    next: u64,
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl IdAllocator {
    /// Start at 1 so `TurnId::ZERO` / `ToolCallId(0)` stay reserved as
    /// sentinel values — no real allocation ever collides with them.
    pub const fn new() -> Self {
        Self { next: 1 }
    }

    /// Start handing out IDs from `next`. Used when resuming a conversation to
    /// continue a global counter past the highest value already persisted (e.g.
    /// image numbers), so a resumed session never re-issues a number that an
    /// earlier message already used.
    pub const fn starting_at(next: u64) -> Self {
        Self { next }
    }

    /// Hand out the next ID.
    #[expect(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        let id = self.next;
        self.next = self.next.saturating_add(1);
        id
    }

    /// Reset to 1. Used by `--replay` when loading a fresh log.
    pub fn reset(&mut self) {
        self.next = 1;
    }

    /// Peek without advancing.
    pub fn peek(&self) -> u64 {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_allocator_hands_out_monotonic_ids() {
        let mut alloc = IdAllocator::new();
        assert_eq!(alloc.next(), 1);
        assert_eq!(alloc.next(), 2);
        assert_eq!(alloc.next(), 3);
    }

    #[test]
    fn id_allocator_reset_starts_from_one() {
        let mut alloc = IdAllocator::new();
        alloc.next();
        alloc.next();
        alloc.reset();
        assert_eq!(alloc.next(), 1);
    }

    #[test]
    fn id_types_are_distinct_at_the_type_level() {
        // Compile-time check: can't accidentally pass a ToolCallId where
        // a TurnId is expected. No `From` impl between them.
        fn only_turn(_: TurnId) {}
        only_turn(TurnId(1));
        // only_turn(ToolCallId(1));  // would fail to compile — correct
    }

    #[test]
    fn turn_id_display_format() {
        assert_eq!(format!("{}", TurnId(42)), "turn#42");
        assert_eq!(format!("{}", ToolCallId(7)), "tool#7");
    }

    #[test]
    fn ids_serialize_as_bare_numbers() {
        // Wrapped u64 means JSON is just the number — keeps the replay
        // log readable and small.
        let json = serde_json::to_string(&TurnId(7)).unwrap();
        assert_eq!(json, "7");
    }
}
