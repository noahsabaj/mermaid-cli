//! The request/response envelope for read-only lookups.
//!
//! Eleven `Cmd` variants used to pair 1:1 with eleven `Msg` variants — every
//! picker fill and `/runtime`-family listing was its own request enum arm, its
//! own response enum arm, its own reducer arm, and its own spawn-and-wrap
//! block in the effect dispatcher, all shaped identically: ask for a listing,
//! get a value back, no turn scoping, no side effects. This module is that
//! shape said once: the reducer emits `Cmd::Query(Query::…)`, the effect
//! layer runs the lookup and answers with `Msg::QueryResult(QueryResult::…)`,
//! and the reducer routes the result to state in `handle_query_result`.
//!
//! The pairing is positional by name: each [`Query`] variant documents the
//! [`QueryResult`] variant that answers it, and the result variants keep the
//! names the old `Msg` variants had (`ConversationsListed`, `RuntimeTasksListed`,
//! …) so recordings and greps stay familiar — one level deeper, same
//! vocabulary.
//!
//! What does NOT belong here: anything turn-scoped (stream events, tool
//! outcomes), anything that mutates (those stay explicit `Cmd`s), and the
//! fire-and-forget notices (`Msg::RuntimeText`, `Msg::ScratchpadReady`) whose
//! staleness rules differ.

use serde::{Deserialize, Serialize};

use crate::state::{ConversationSummary, ModelChoice};
use mermaid_runtime::{
    ApprovalRecord, CheckpointRecord, PluginInstallRecord, ProcessRecord, TaskRecord,
    TaskTimelineEvent,
};

/// A read-only lookup the reducer asks the effect layer to run. Inert data,
/// like every `Cmd`; the effect layer's `dispatch_query` owns the I/O.
#[derive(Debug, Clone)]
pub enum Query {
    /// `/load <id>` — read one saved conversation off disk. Answered by
    /// [`QueryResult::ConversationLoaded`]; a missing/corrupt file answers
    /// nothing (the failure is logged, the picker simply doesn't advance).
    LoadConversation { id: String },
    /// Scan the conversations directory for the `/load` picker (newest
    /// first). Answered by [`QueryResult::ConversationsListed`].
    ListConversations,
    /// Discover every model the user could switch to, for the `/model`
    /// picker. Best-effort and strictly read-only: a dead Ollama is NOT
    /// started, an unreachable provider is skipped. Answered by
    /// [`QueryResult::AvailableModelsListed`].
    ListAvailableModels,
    /// Walk the project for the @-mention file picker (gitignore-aware,
    /// capped, sorted; directories carry a trailing `/`). Answered by
    /// [`QueryResult::ProjectFilesListed`].
    ListProjectFiles,
    /// `/tasks` — durable runtime tasks. Answered by
    /// [`QueryResult::RuntimeTasksListed`].
    ListRuntimeTasks { limit: usize },
    /// `/task <id>` — one durable task plus its timeline. Answered by
    /// [`QueryResult::RuntimeTaskLoaded`].
    LoadRuntimeTask { id: String },
    /// `/processes` — durable background processes. Answered by
    /// [`QueryResult::RuntimeProcessesListed`].
    ListRuntimeProcesses { limit: usize },
    /// `/approvals` — pending approval records. Answered by
    /// [`QueryResult::RuntimeApprovalsListed`].
    ListRuntimeApprovals,
    /// `/checkpoints` — restore checkpoints. Answered by
    /// [`QueryResult::RuntimeCheckpointsListed`].
    ListRuntimeCheckpoints { limit: usize },
    /// Checkpoints of `session_id` anchored strictly past `message_index` —
    /// fired by rewind/fork so the user learns which file checkpoints the
    /// discarded timeline left behind. Answered by
    /// [`QueryResult::ForkCheckpointsFound`].
    ListForkCheckpoints {
        session_id: String,
        message_index: usize,
    },
    /// `/plugins` — installed plugins. Answered by
    /// [`QueryResult::RuntimePluginsListed`].
    ListRuntimePlugins,
}

impl Query {
    /// Stable tag for traces and `Cmd::summary`, mirroring the tags the
    /// standalone `Cmd` variants carried.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::LoadConversation { .. } => "load_conversation",
            Self::ListConversations => "list_conversations",
            Self::ListAvailableModels => "list_available_models",
            Self::ListProjectFiles => "list_project_files",
            Self::ListRuntimeTasks { .. } => "list_runtime_tasks",
            Self::LoadRuntimeTask { .. } => "load_runtime_task",
            Self::ListRuntimeProcesses { .. } => "list_runtime_processes",
            Self::ListRuntimeApprovals => "list_runtime_approvals",
            Self::ListRuntimeCheckpoints { .. } => "list_runtime_checkpoints",
            Self::ListForkCheckpoints { .. } => "list_fork_checkpoints",
            Self::ListRuntimePlugins => "list_runtime_plugins",
        }
    }

    /// Compact identifying summary for traces and the `--record` file —
    /// carries the same identifying fields the standalone `Cmd` summaries
    /// carried, never a payload.
    #[must_use]
    pub fn summary(&self) -> String {
        match self {
            Self::LoadConversation { id } => format!("load_conversation({id})"),
            Self::ListRuntimeTasks { limit } => format!("list_runtime_tasks(limit={limit})"),
            Self::LoadRuntimeTask { id } => format!("load_runtime_task({id})"),
            Self::ListRuntimeProcesses { limit } => {
                format!("list_runtime_processes(limit={limit})")
            },
            Self::ListRuntimeCheckpoints { limit } => {
                format!("list_runtime_checkpoints(limit={limit})")
            },
            Self::ListForkCheckpoints {
                session_id,
                message_index,
            } => format!("list_fork_checkpoints({session_id} > {message_index})"),
            Self::ListConversations
            | Self::ListAvailableModels
            | Self::ListProjectFiles
            | Self::ListRuntimeApprovals
            | Self::ListRuntimePlugins => self.tag().to_string(),
        }
    }
}

/// The answer to one [`Query`], delivered as `Msg::QueryResult`. Variant
/// names are the names the standalone `Msg` variants had, so a recorded
/// session reads the same one level deeper. Serde derives exist for
/// `--record` / `--replay`, exactly like `Msg`'s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueryResult {
    /// A saved conversation has been read off disk (`/load`, the rewind
    /// picker's source timeline). Boxed: a whole transcript dwarfs every
    /// sibling variant, and `clippy::large_enum_variant` is right that the
    /// envelope should not carry it inline.
    ConversationLoaded(Box<crate::ConversationHistory>),
    /// Candidates for the `/load` picker, newest first.
    ConversationsListed(Vec<ConversationSummary>),
    /// Everything the user can switch to, already grouped and sorted for
    /// the `/model` picker.
    AvailableModelsListed(Vec<ModelChoice>),
    /// Relative project paths for the @-mention picker.
    ProjectFilesListed(Vec<String>),
    /// Response to `/tasks`.
    RuntimeTasksListed(Vec<TaskRecord>),
    /// Response to `/task <id>`. The record is boxed for the same reason
    /// the conversation is: one fat resident would make every
    /// `QueryResult` this size.
    RuntimeTaskLoaded {
        task: Option<Box<TaskRecord>>,
        events: Vec<TaskTimelineEvent>,
    },
    /// Response to `/processes`.
    RuntimeProcessesListed(Vec<ProcessRecord>),
    /// Response to `/approvals`.
    RuntimeApprovalsListed(Vec<ApprovalRecord>),
    /// Response to `/checkpoints`.
    RuntimeCheckpointsListed(Vec<CheckpointRecord>),
    /// Reply to [`Query::ListForkCheckpoints`]: file checkpoints anchored
    /// past a rewind's fork point (oldest first). Empty means no notice.
    ForkCheckpointsFound(Vec<CheckpointRecord>),
    /// Response to `/plugins`.
    RuntimePluginsListed(Vec<PluginInstallRecord>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_tags_are_stable() {
        assert_eq!(Query::ListConversations.tag(), "list_conversations");
        assert_eq!(
            Query::LoadRuntimeTask {
                id: "t1".to_string()
            }
            .tag(),
            "load_runtime_task"
        );
    }

    #[test]
    fn query_summary_carries_identifying_fields_never_payloads() {
        assert_eq!(
            Query::ListRuntimeTasks { limit: 10 }.summary(),
            "list_runtime_tasks(limit=10)"
        );
        assert_eq!(
            Query::ListForkCheckpoints {
                session_id: "s".to_string(),
                message_index: 4
            }
            .summary(),
            "list_fork_checkpoints(s > 4)"
        );
        assert_eq!(Query::ListProjectFiles.summary(), "list_project_files");
    }
}
