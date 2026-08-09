//! Shell-command analysis. 1,650 lines that depend only on `RiskClass` — not
//! on `PolicyEngine`, `ActionRequest` or `SafetyMode` — and decide whether a
//! command is read-only, network-touching, or destructive.
//!
//! Extracted bottom-up: `lexer` has no dependencies at all, `tables` is data,
//! `classify` and `destructive` sit on both.

pub(crate) mod classify;
pub(crate) mod destructive;
pub(crate) mod lexer;
pub(in crate::policy) mod powershell;
pub(crate) mod tables;

pub(crate) use classify::*;
pub(crate) use destructive::*;
pub(crate) use lexer::*;
pub(crate) use tables::*;
