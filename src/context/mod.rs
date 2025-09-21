// Gateway module for context - follows the Train Station Pattern
// All external access must go through this gateway

// Private submodules - not directly accessible from outside
mod loader;
mod ranker;
mod repo_graph;
mod repomap;
mod tree_parser;

// Public re-exports - the ONLY way to access context functionality
pub use loader::{ContextLoader, LoaderConfig};
pub use ranker::{RankerConfig, RepoRanker};
pub use repo_graph::RepoGraph;
pub use repomap::{generate_repo_map, RepoMap, RepoMapStats};
pub use tree_parser::{Symbol, SymbolKind, TreeParser};