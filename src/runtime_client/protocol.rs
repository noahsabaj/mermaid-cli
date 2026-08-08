//! Typed daemon control protocol.
//!
//! Every JSON command a `mermaidd` socket client can send, as one enum —
//! the wire shape (`{"command": "...", ...fields}`) is BYTE-IDENTICAL to
//! the previous stringly dispatch; this is a protocol contract being made
//! exhaustive, not a compat shim. The daemon parses requests into this
//! enum (a typo becomes a serde error naming the field instead of a silent
//! `unknown command`), and the client constructs requests from it (a
//! misspelled literal becomes a compile error).
//!
//! Deliberately NO typed response enum: wire responses are heterogeneous
//! per command and the client already owns typed response structs — the
//! exhaustiveness win lives on the request side.

use serde::{Deserialize, Serialize};

/// One daemon control request. `auth.token` rides OUTSIDE this shape (the
/// daemon reads it before the typed parse), so no variant carries it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonRequest {
    Health,
    CreateTask {
        title: String,
        project_path: String,
        model_id: String,
    },
    SessionMessages {
        id: String,
    },
    #[serde(alias = "runtime_snapshot")]
    Snapshot,
    RuntimeDashboard,
    RuntimeDiagnostics,
    RuntimeHygienePreview,
    RuntimeHygieneArchive,
    RuntimeTaskDetail {
        id: String,
    },
    RuntimeApprovalDetail {
        id: String,
    },
    RuntimeCheckpointDetail {
        id: String,
    },
    RuntimeTasks {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    RuntimeProcesses {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    RuntimeToolRuns {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    RuntimeCheckpoints {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u64>,
    },
    RuntimeApprovals,
    RuntimePlugins,
    Run {
        prompt: String,
        /// Empty string means unset (kept for exact wire behavior).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<String>,
    },
    CancelTask {
        id: String,
    },
    UpdateTask {
        id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_report: Option<String>,
    },
    Logs {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tail_bytes: Option<u64>,
    },
    StopProcess {
        id: String,
    },
    RestartProcess {
        id: String,
    },
    OpenProcess {
        id: String,
    },
    Ports,
    RestoreCheckpoint {
        id: String,
    },
    Approve {
        id: String,
    },
    Deny {
        id: String,
    },
    PluginPreview {
        path: String,
    },
    PluginInstall {
        path: String,
    },
    SetPluginEnabled {
        id: String,
        enabled: bool,
    },
    SetSafetyMode {
        mode: String,
    },
    ModelInfo {
        model: String,
    },
    Pair {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttl_days: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_hash: Option<String>,
    },
    /// Attach to a task's live `RunEvent` stream: ack line, then NDJSON
    /// events until the terminal `result`. Streaming — handled outside the
    /// one-shot request/response path.
    SubscribeTask {
        task_id: String,
    },
}

impl DaemonRequest {
    /// Whether the request needs the pairing token on the LOCAL socket
    /// (TCP requires auth for everything). Exhaustive — adding a variant
    /// forces a decision here. Session content flows through
    /// `SubscribeTask`, so it is gated like `session_messages`.
    pub fn requires_auth(&self) -> bool {
        match self {
            Self::Health => false,
            Self::CreateTask { .. }
            | Self::Run { .. }
            | Self::CancelTask { .. }
            | Self::UpdateTask { .. }
            | Self::RestoreCheckpoint { .. }
            | Self::Approve { .. }
            | Self::Deny { .. }
            | Self::StopProcess { .. }
            | Self::RestartProcess { .. }
            | Self::OpenProcess { .. }
            | Self::PluginPreview { .. }
            | Self::PluginInstall { .. }
            | Self::SetPluginEnabled { .. }
            | Self::SetSafetyMode { .. }
            | Self::RuntimeHygieneArchive
            | Self::Pair { .. }
            | Self::Logs { .. }
            | Self::SessionMessages { .. }
            | Self::Snapshot
            | Self::RuntimeDashboard
            | Self::RuntimeDiagnostics
            | Self::RuntimeHygienePreview
            | Self::RuntimeTaskDetail { .. }
            | Self::RuntimeApprovalDetail { .. }
            | Self::RuntimeCheckpointDetail { .. }
            | Self::RuntimeTasks { .. }
            | Self::RuntimeProcesses { .. }
            | Self::RuntimeApprovals
            | Self::RuntimeToolRuns { .. }
            | Self::RuntimeCheckpoints { .. }
            | Self::RuntimePlugins
            | Self::ModelInfo { .. }
            | Self::SubscribeTask { .. } => true,
            // Liveness/discovery stay unauthenticated on the local socket:
            // no project or credential content, and used before pairing.
            Self::Ports => false,
        }
    }

    /// Serialize to the wire `Value` (the shape `request_daemon` injects
    /// `auth` into).
    pub fn to_wire(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("DaemonRequest serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden-style: every variant round-trips through its EXACT wire form.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "predates the lint; see .github/baselines/expect_budget.txt"
    )]
    fn every_variant_round_trips_the_wire_shape() {
        let cases: Vec<(DaemonRequest, &str)> = vec![
            (DaemonRequest::Health, r#"{"command":"health"}"#),
            (
                DaemonRequest::CreateTask {
                    title: "t".into(),
                    project_path: "/p".into(),
                    model_id: "m".into(),
                },
                r#"{"command":"create_task","title":"t","project_path":"/p","model_id":"m"}"#,
            ),
            (
                DaemonRequest::SessionMessages { id: "s1".into() },
                r#"{"command":"session_messages","id":"s1"}"#,
            ),
            (DaemonRequest::Snapshot, r#"{"command":"snapshot"}"#),
            (
                DaemonRequest::RuntimeDashboard,
                r#"{"command":"runtime_dashboard"}"#,
            ),
            (
                DaemonRequest::RuntimeDiagnostics,
                r#"{"command":"runtime_diagnostics"}"#,
            ),
            (
                DaemonRequest::RuntimeHygienePreview,
                r#"{"command":"runtime_hygiene_preview"}"#,
            ),
            (
                DaemonRequest::RuntimeHygieneArchive,
                r#"{"command":"runtime_hygiene_archive"}"#,
            ),
            (
                DaemonRequest::RuntimeTaskDetail { id: "t1".into() },
                r#"{"command":"runtime_task_detail","id":"t1"}"#,
            ),
            (
                DaemonRequest::RuntimeApprovalDetail { id: "a1".into() },
                r#"{"command":"runtime_approval_detail","id":"a1"}"#,
            ),
            (
                DaemonRequest::RuntimeCheckpointDetail { id: "c1".into() },
                r#"{"command":"runtime_checkpoint_detail","id":"c1"}"#,
            ),
            (
                DaemonRequest::RuntimeTasks { limit: Some(50) },
                r#"{"command":"runtime_tasks","limit":50}"#,
            ),
            (
                DaemonRequest::RuntimeProcesses { limit: None },
                r#"{"command":"runtime_processes"}"#,
            ),
            (
                DaemonRequest::RuntimeToolRuns { limit: Some(100) },
                r#"{"command":"runtime_tool_runs","limit":100}"#,
            ),
            (
                DaemonRequest::RuntimeCheckpoints { limit: Some(50) },
                r#"{"command":"runtime_checkpoints","limit":50}"#,
            ),
            (
                DaemonRequest::RuntimeApprovals,
                r#"{"command":"runtime_approvals"}"#,
            ),
            (
                DaemonRequest::RuntimePlugins,
                r#"{"command":"runtime_plugins"}"#,
            ),
            (
                DaemonRequest::Run {
                    prompt: "p".into(),
                    project_path: Some(String::new()),
                    model_id: None,
                    priority: Some("high".into()),
                },
                r#"{"command":"run","prompt":"p","project_path":"","priority":"high"}"#,
            ),
            (
                DaemonRequest::CancelTask { id: "t1".into() },
                r#"{"command":"cancel_task","id":"t1"}"#,
            ),
            (
                DaemonRequest::UpdateTask {
                    id: "t1".into(),
                    status: "completed".into(),
                    final_report: Some("done".into()),
                },
                r#"{"command":"update_task","id":"t1","status":"completed","final_report":"done"}"#,
            ),
            (
                DaemonRequest::Logs {
                    id: "p1".into(),
                    tail_bytes: Some(4096),
                },
                r#"{"command":"logs","id":"p1","tail_bytes":4096}"#,
            ),
            (
                DaemonRequest::StopProcess { id: "p1".into() },
                r#"{"command":"stop_process","id":"p1"}"#,
            ),
            (
                DaemonRequest::RestartProcess { id: "p1".into() },
                r#"{"command":"restart_process","id":"p1"}"#,
            ),
            (
                DaemonRequest::OpenProcess { id: "p1".into() },
                r#"{"command":"open_process","id":"p1"}"#,
            ),
            (DaemonRequest::Ports, r#"{"command":"ports"}"#),
            (
                DaemonRequest::RestoreCheckpoint { id: "c1".into() },
                r#"{"command":"restore_checkpoint","id":"c1"}"#,
            ),
            (
                DaemonRequest::Approve { id: "a1".into() },
                r#"{"command":"approve","id":"a1"}"#,
            ),
            (
                DaemonRequest::Deny { id: "a1".into() },
                r#"{"command":"deny","id":"a1"}"#,
            ),
            (
                DaemonRequest::PluginPreview { path: "/pl".into() },
                r#"{"command":"plugin_preview","path":"/pl"}"#,
            ),
            (
                DaemonRequest::PluginInstall { path: "/pl".into() },
                r#"{"command":"plugin_install","path":"/pl"}"#,
            ),
            (
                DaemonRequest::SetPluginEnabled {
                    id: "pl1".into(),
                    enabled: true,
                },
                r#"{"command":"set_plugin_enabled","id":"pl1","enabled":true}"#,
            ),
            (
                DaemonRequest::SetSafetyMode { mode: "ask".into() },
                r#"{"command":"set_safety_mode","mode":"ask"}"#,
            ),
            (
                DaemonRequest::ModelInfo { model: "m".into() },
                r#"{"command":"model_info","model":"m"}"#,
            ),
            (
                DaemonRequest::Pair {
                    label: Some("laptop".into()),
                    ttl_days: Some(30),
                    token_hash: None,
                },
                r#"{"command":"pair","label":"laptop","ttl_days":30}"#,
            ),
            (
                DaemonRequest::SubscribeTask {
                    task_id: "t1".into(),
                },
                r#"{"command":"subscribe_task","task_id":"t1"}"#,
            ),
        ];
        for (req, wire) in cases {
            assert_eq!(serde_json::to_string(&req).unwrap(), wire, "{req:?}");
            let back: DaemonRequest = serde_json::from_str(wire).unwrap();
            assert_eq!(back, req, "{wire}");
        }
    }

    #[test]
    fn runtime_snapshot_alias_still_parses() {
        let req: DaemonRequest = serde_json::from_str(r#"{"command":"runtime_snapshot"}"#).unwrap();
        assert_eq!(req, DaemonRequest::Snapshot);
    }

    #[test]
    fn requires_auth_matches_the_historical_matrix() {
        assert!(!DaemonRequest::Health.requires_auth());
        assert!(!DaemonRequest::Ports.requires_auth());
        assert!(DaemonRequest::ModelInfo { model: "m".into() }.requires_auth());
        for gated in [
            DaemonRequest::Run {
                prompt: "p".into(),
                project_path: None,
                model_id: None,
                priority: None,
            },
            DaemonRequest::Snapshot,
            DaemonRequest::SessionMessages { id: "s".into() },
            DaemonRequest::Logs {
                id: "p".into(),
                tail_bytes: None,
            },
            DaemonRequest::Pair {
                label: None,
                ttl_days: None,
                token_hash: None,
            },
            DaemonRequest::SubscribeTask {
                task_id: "t".into(),
            },
        ] {
            assert!(gated.requires_auth(), "{gated:?}");
        }
    }

    #[test]
    fn unknown_command_is_a_parse_error() {
        assert!(serde_json::from_str::<DaemonRequest>(r#"{"command":"nope"}"#).is_err());
    }
}
