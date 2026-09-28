//! Safety policy for tool actions: the engine (`engine`), the shell
//! classifier (`shell/`), and scratchpad containment (`scratch`). The
//! VOCABULARY (modes, risk classes, requests, decisions) lives in
//! `mermaid_model::safety` so the pure domain crate can speak it without a
//! dependency on this crate; it is re-exported here because it is this
//! module's API surface -- `PolicyEngine::decide` takes `ActionRequest` and
//! answers `PolicyDecision`.

mod engine;

pub use engine::PolicyEngine;
pub use mermaid_model::safety::{
    ActionRequest, FloorLevel, HostShell, PolicyDecision, PolicyOverride, PolicyOverrideDecision,
    READ_ONLY_DENIAL_MARKER, RiskClass, SafetyMode, ToolCategory,
};

pub(crate) mod scratch;
pub(crate) mod shell;

// The public half of the split, named explicitly: `lib.rs` re-exports these,
// and a `pub(crate)` glob cannot carry a name across the crate boundary.
pub use scratch::token_provably_in_scratch;
pub use shell::destructive::{destructive_rule, is_destructive_command};
