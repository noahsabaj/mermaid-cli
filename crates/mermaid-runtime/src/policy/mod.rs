//! Safety policy for tool actions: the vocabulary (`types`), the engine
//! (`engine`), the shell classifier (`shell/`), and the plan-mode carve-outs
//! (`plan_gate`). This gateway re-exports the public surface so callers keep
//! the `crate::policy::*` / `mermaid_runtime::*` paths they always had.

mod engine;
mod types;

pub use engine::PolicyEngine;
pub use types::{
    ActionRequest, FloorLevel, HostShell, PolicyDecision, PolicyOverride, PolicyOverrideDecision,
    RiskClass, SafetyMode, ToolCategory,
};

pub(crate) mod plan_gate;
pub(crate) mod shell;

// The public half of the split, named explicitly: `lib.rs` re-exports these,
// and a `pub(crate)` glob cannot carry a name across the crate boundary.
pub use plan_gate::{
    PLAN_DENIAL_MARKER, READ_ONLY_DENIAL_MARKER, is_plan_file_only_write, is_plan_file_path,
    is_plan_safe_build_command,
};
pub use shell::destructive::is_destructive_command;
