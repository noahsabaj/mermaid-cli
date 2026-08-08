//! Client side of the daemon protocol.
//!
//! `RuntimeClient` talks to a running `mermaidd` over its control socket, and
//! falls back to opening the SQLite store directly when no daemon is up.
//! `DaemonRequest` is the typed wire contract both sides share.
//!
//! Named `runtime_client`, not `runtime`. The old name collided with the
//! `mermaid-runtime` crate it sits above, and the module was mostly a
//! `pub use mermaid_runtime::*` glob — so `crate::runtime::Foo` might be a
//! local type or a re-exported one, with nothing at the call site to say
//! which. `src/providers/tool/workspace.rs` had a doc comment saying
//! `mermaid_runtime::worktree` directly above code saying
//! `crate::runtime::worktree`, which is what that ambiguity costs. The glob is
//! gone; everything below the boundary is spelled `mermaid_runtime::`.

pub mod client;
pub mod protocol;

pub use client::*;
pub use protocol::DaemonRequest;
