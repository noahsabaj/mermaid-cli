//! Snapshot suite for `render()`: pins the FULL frame for a curated set of
//! scenes at two terminal sizes, so any unintended visual drift — spacing,
//! prefixes, modal layout, status-line wording — fails loudly with a diff
//! instead of slipping past the substring assertions in `tests`.
//!
//! Determinism: every input the frame can observe is pinned — the injected
//! clock (`fixed_now`, a PAST date so user timestamps take the absolute-date
//! branch regardless of when the suite runs), the `RenderCache` host/user
//! strings, busy-turn start times (derived from the fixture clock), and `TZ`
//! (via `temp_env`, whose global lock also serializes the env mutation).
//! `determinism_same_scene_twice` guards the harness itself: if a residual
//! env or clock read sneaks into `render()`, it fails here before the pinned
//! snapshots start flaking across machines.
//!
//! Workflow: a mismatch panics with a diff and writes a `.snap.new` sibling
//! (gitignored); run `just snapshots` (`cargo insta review`) to accept or
//! reject. Deliberate visual changes update the `.snap` files in the same PR.

use super::{RenderCache, render_frame};
use crate::app::Config;
use crate::domain::{
    ActionDetails, ActionDisplay, ActionResult, ApprovalKind, GenPhase, PendingApproval,
    PendingToolCall, QueuedMessage, State, ToolCallId, TurnId, TurnState, UiMode,
};
use crate::models::{ChatMessage, ChatMessageKind};

/// The two frame sizes every scene is pinned at: the classic minimum and a
/// roomy modern terminal (exercises wrapping and layout at both extremes).
const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];

/// Fixture clock: a fixed PAST instant (relative-timestamp rendering falls
/// through to the absolute-date branch, immune to the live "Today" boundary).
fn fixed_now() -> chrono::DateTime<chrono::Local> {
    chrono::DateTime::parse_from_rfc3339("2026-01-02T03:04:05+00:00")
        .expect("fixture timestamp parses")
        .with_timezone(&chrono::Local)
}

/// A `RenderCache` with the process-environment and compile-time reads pinned
/// (the status bar renders `user@host` and `mermaid v<version>`; a pinned
/// version keeps the suite immune to release bumps).
fn snapshot_cache() -> RenderCache {
    let mut cache = RenderCache::new();
    cache.hostname = "snaphost".to_string();
    cache.username = "snapuser".to_string();
    cache.version = "0.0.0".to_string();
    cache
}

/// Base state for every scene: fixed cwd/model/clock so ids, titles, and the
/// status bar are byte-stable.
fn scene_state() -> State {
    State::new(
        Config::default(),
        std::path::PathBuf::from("/project/demo"),
        "ollama/test".to_string(),
        fixed_now(),
    )
}

/// Render `build()`'s scene at every pinned size under a UTC clock. The state
/// is built INSIDE the TZ guard so message timestamps and the render share one
/// timezone view; `with_var`'s global lock serializes the suite.
fn assert_scene(name: &str, build: impl Fn() -> State) {
    temp_env::with_var("TZ", Some("UTC"), || {
        let state = build();
        for (width, height) in SIZES {
            let frame = render_frame(&state, &mut snapshot_cache(), width, height);
            insta::assert_snapshot!(format!("{name}_{width}x{height}"), frame);
        }
    });
}

/// `kind`-stamped message, mirroring the helper in `tests`.
fn kinded(mut msg: ChatMessage, kind: ChatMessageKind) -> ChatMessage {
    msg.kind = kind;
    msg
}

#[test]
fn idle_empty() {
    assert_scene("idle_empty", scene_state);
}

#[test]
fn chat_transcript() {
    assert_scene("chat_transcript", || {
        let mut s = scene_state();
        s.session
            .append(ChatMessage::user("run the tests and summarize"), s.now);
        let mut reply = ChatMessage::assistant(
            "All 42 tests pass. The flaky one was fixed by pinning the clock:\n\n\
             ```rust\nlet now = fixed_now();\n```\n\nNothing else changed.",
        );
        reply.actions.push(ActionDisplay {
            action_type: "Bash".to_string(),
            target: "cargo test".to_string(),
            result: ActionResult::Success {
                output: "42 passed".to_string(),
                images: None,
            },
            details: ActionDetails::Simple,
            duration_seconds: Some(2.5),
            metadata: None,
        });
        s.session.append(reply, s.now);
        s.session.append(
            kinded(
                ChatMessage::system("Worked for 12s · 1.2k tokens"),
                ChatMessageKind::RunSummary,
            ),
            s.now,
        );
        s
    });
}

#[test]
fn busy_streaming() {
    assert_scene("busy_streaming", || {
        let mut s = scene_state();
        s.session
            .append(ChatMessage::user("explain the plan"), s.now);
        s.turn = TurnState::Generating {
            id: TurnId(1),
            started: std::time::SystemTime::from(fixed_now() - chrono::Duration::seconds(3)),
            partial_text: "The plan has three phases: first we".to_string(),
            partial_reasoning: String::new(),
            tokens: 12,
            phase: GenPhase::Streaming,
            provider_continuation: None,
            pending_tool_calls: Vec::new(),
            continuation: false,
        };
        s
    });
}

#[test]
fn busy_tools_with_queue() {
    assert_scene("busy_tools_with_queue", || {
        let mut s = scene_state();
        s.session
            .append(ChatMessage::user("start the server"), s.now);
        s.turn = TurnState::ExecutingTools {
            id: TurnId(1),
            started: std::time::SystemTime::from(fixed_now() - chrono::Duration::seconds(3)),
            calls: vec![PendingToolCall {
                call_id: ToolCallId(1),
                source: crate::models::tool_call::ToolCall {
                    id: Some("c1".to_string()),
                    function: crate::models::tool_call::FunctionCall {
                        name: "execute_command".to_string(),
                        arguments: serde_json::json!({"command": "npm run dev"}),
                    },
                },
            }],
            outcomes: vec![None],
        };
        s.ui.queued_messages.push_back(QueuedMessage {
            text: "also check the logs afterwards".to_string(),
            attachment_ids: Vec::new(),
        });
        s
    });
}

/// A mid-run checklist: 2 completed (one with cost stamps), the active task,
/// pendings, and one user-added task. Exercises glyphs, strikethrough, the
/// cost suffix, the `(you)` marker, and the spinner-headline takeover.
fn task_run_state() -> State {
    use crate::domain::tasks::{Stamp, TaskEdit, TaskSpec, TaskStatus};
    let mut s = scene_state();
    s.session
        .append(ChatMessage::user("ship the feature"), s.now);
    let steps = [
        ("Audit the call sites", "Auditing the call sites"),
        (
            "Add TaskStore to the domain",
            "Adding TaskStore to the domain",
        ),
        (
            "Wire the broker through ExecContext",
            "Wiring the broker through ExecContext",
        ),
        ("Render the checklist band", "Rendering the checklist band"),
        ("Update the changelog", "Updating the changelog"),
    ];
    s.session.conversation.tasks.create(
        steps
            .iter()
            .map(|(subject, active)| TaskSpec {
                subject: (*subject).to_string(),
                active_form: (*active).to_string(),
                description: None,
                in_progress: false,
            })
            .collect(),
        crate::domain::TaskOrigin::Model,
        Stamp::default(),
    );
    s.session.conversation.tasks.create(
        vec![TaskSpec {
            subject: "Double-check the docs".to_string(),
            active_form: "Double-checking the docs".to_string(),
            description: None,
            in_progress: false,
        }],
        crate::domain::TaskOrigin::User,
        Stamp::default(),
    );
    let edit = |id, status| TaskEdit {
        id,
        status: Some(status),
        ..TaskEdit::default()
    };
    s.session.conversation.tasks.apply(
        &[edit(1, TaskStatus::InProgress)],
        Stamp {
            now_epoch: 100,
            run_tokens: 500,
        },
    );
    s.session.conversation.tasks.apply(
        &[
            edit(1, TaskStatus::Completed),
            edit(2, TaskStatus::InProgress),
        ],
        Stamp {
            now_epoch: 230,
            run_tokens: 8_900,
        },
    );
    s.session.conversation.tasks.apply(
        &[
            edit(2, TaskStatus::Completed),
            edit(3, TaskStatus::InProgress),
        ],
        Stamp {
            now_epoch: 300,
            run_tokens: 12_400,
        },
    );
    s.turn = TurnState::ExecutingTools {
        id: TurnId(1),
        started: std::time::SystemTime::from(fixed_now() - chrono::Duration::seconds(3)),
        calls: vec![PendingToolCall {
            call_id: ToolCallId(1),
            source: crate::models::tool_call::ToolCall {
                id: Some("c1".to_string()),
                function: crate::models::tool_call::FunctionCall {
                    name: "execute_command".to_string(),
                    arguments: serde_json::json!({"command": "cargo check"}),
                },
            },
        }],
        outcomes: vec![None],
    };
    s
}

#[test]
fn task_checklist_expanded() {
    assert_scene("task_checklist_expanded", task_run_state);
}

#[test]
fn task_checklist_collapsed() {
    assert_scene("task_checklist_collapsed", || {
        let mut s = task_run_state();
        s.ui.tasks_collapsed = true;
        s
    });
}

#[test]
fn task_checklist_retires_when_done_and_idle() {
    assert_scene("task_checklist_retired", || {
        use crate::domain::tasks::{Stamp, TaskEdit, TaskStatus};
        let mut s = task_run_state();
        let ids: Vec<u32> = s
            .session
            .conversation
            .tasks
            .visible()
            .map(|t| t.id)
            .collect();
        let edits: Vec<TaskEdit> = ids
            .into_iter()
            .map(|id| TaskEdit {
                id,
                status: Some(TaskStatus::Completed),
                ..TaskEdit::default()
            })
            .collect();
        s.session.conversation.tasks.apply(&edits, Stamp::default());
        s.turn = TurnState::Idle;
        s
    });
}

#[test]
fn busy_agents_panel() {
    assert_scene("busy_agents_panel", || {
        let mut s = scene_state();
        s.session
            .append(ChatMessage::user("audit the codebase (use agents)"), s.now);
        let agent_call = |id: u64, description: &str| PendingToolCall {
            call_id: ToolCallId(id),
            source: crate::models::tool_call::ToolCall {
                id: Some(format!("c{id}")),
                function: crate::models::tool_call::FunctionCall {
                    name: "agent".to_string(),
                    arguments: serde_json::json!({"description": description, "type": "explore"}),
                },
            },
        };
        s.turn = TurnState::ExecutingTools {
            id: TurnId(1),
            started: std::time::SystemTime::from(fixed_now() - chrono::Duration::seconds(45)),
            calls: vec![
                agent_call(1, "Map repo structure"),
                agent_call(2, "Audit source architecture"),
                agent_call(3, "Audit security & secrets"),
            ],
            outcomes: vec![None, None, None],
        };
        s.ui.live_tool_status.insert(
            ToolCallId(1),
            crate::domain::LiveToolStatus {
                activity: "read_file…".to_string(),
                tokens: 12_300,
            },
        );
        s.ui.live_tool_status.insert(
            ToolCallId(2),
            crate::domain::LiveToolStatus {
                activity: "thinking".to_string(),
                tokens: 8_100,
            },
        );
        // One agent detached earlier via Ctrl+B: still on the panel, marked bg.
        s.runtime
            .background_agents
            .push(crate::domain::runtime::BackgroundAgent {
                agent_id: "a9".to_string(),
                description: "Audit docs and conventions".to_string(),
                started: std::time::SystemTime::from(fixed_now() - chrono::Duration::seconds(90)),
                activity: "execute_command…".to_string(),
                tokens: 27_900,
            });
        s
    });
}

#[test]
fn mixed_exec_and_agent_turn() {
    assert_scene("mixed_exec_and_agent_turn", || {
        let mut s = scene_state();
        s.session
            .append(ChatMessage::user("run tests and audit in parallel"), s.now);
        s.turn = TurnState::ExecutingTools {
            id: TurnId(1),
            started: std::time::SystemTime::from(fixed_now() - chrono::Duration::seconds(9)),
            calls: vec![
                PendingToolCall {
                    call_id: ToolCallId(1),
                    source: crate::models::tool_call::ToolCall {
                        id: Some("c1".to_string()),
                        function: crate::models::tool_call::FunctionCall {
                            name: "execute_command".to_string(),
                            arguments: serde_json::json!({"command": "cargo test"}),
                        },
                    },
                },
                PendingToolCall {
                    call_id: ToolCallId(2),
                    source: crate::models::tool_call::ToolCall {
                        id: Some("c2".to_string()),
                        function: crate::models::tool_call::FunctionCall {
                            name: "agent".to_string(),
                            arguments: serde_json::json!({"description": "Audit deps"}),
                        },
                    },
                },
            ],
            outcomes: vec![None, None],
        };
        s.ui.live_tool_status.insert(
            ToolCallId(2),
            crate::domain::LiveToolStatus {
                activity: "starting…".to_string(),
                tokens: 0,
            },
        );
        s
    });
}

#[test]
fn approval_modal() {
    assert_scene("approval_modal", || {
        let mut s = scene_state();
        s.session
            .append(ChatMessage::user("clean the workspace"), s.now);
        s.pending_approval.push_back(PendingApproval {
            turn: TurnId(1),
            call_id: ToolCallId(1),
            tool: "execute_command".to_string(),
            risk: "destructive".to_string(),
            kind: ApprovalKind::Shell,
            prompt: "rm -rf target/".to_string(),
            allowlist_scope: "execute_command(rm)".to_string(),
            selected_option: 0,
        });
        s
    });
}

#[test]
fn question_modal() {
    assert_scene("question_modal", || {
        use crate::domain::question::{PendingQuestionSet, Question, QuestionOption};
        let mut s = scene_state();
        s.session
            .append(ChatMessage::user("set up the database"), s.now);
        s.pending_question.push_back(PendingQuestionSet::new(
            TurnId(1),
            ToolCallId(1),
            vec![Question {
                header: "Database".to_string(),
                question: "Which database should the service use?".to_string(),
                kind: Default::default(),
                options: vec![
                    QuestionOption {
                        label: "PostgreSQL".to_string(),
                        description: Some("Relational, battle-tested".to_string()),
                        recommended: true,
                        preview: None,
                    },
                    QuestionOption {
                        label: "SQLite".to_string(),
                        description: Some("Embedded, zero-ops".to_string()),
                        recommended: false,
                        preview: None,
                    },
                ],
                memory_key: None,
            }],
        ));
        s
    });
}

#[test]
fn conversation_list() {
    assert_scene("conversation_list", || {
        use crate::domain::ConversationSummary;
        let mut s = scene_state();
        s.ui.mode = UiMode::ConversationList {
            candidates: vec![
                ConversationSummary {
                    id: "20260101_120000_000".to_string(),
                    title: "Fix the resolver panic".to_string(),
                    message_count: 14,
                    updated_at: "2026-01-01 12:34".to_string(),
                },
                ConversationSummary {
                    id: "20251231_090000_000".to_string(),
                    title: "Write release notes".to_string(),
                    message_count: 6,
                    updated_at: "2025-12-31 09:15".to_string(),
                },
            ],
            cursor: 0,
        };
        s
    });
}

#[test]
fn slash_palette() {
    assert_scene("slash_palette", || {
        let mut s = scene_state();
        s.ui.input_buffer = "/mo".to_string();
        s.ui.input_cursor = 3;
        s
    });
}

#[test]
fn system_notice_and_checkpoint() {
    assert_scene("system_notice_and_checkpoint", || {
        let mut s = scene_state();
        s.session.append(
            ChatMessage::system("Safety mode changed to read-only"),
            s.now,
        );
        s.session.append(
            kinded(
                ChatMessage::assistant(
                    "Context compacted: 18 messages archived. Summary: fixed the resolver \
                     panic and added regression tests.",
                ),
                ChatMessageKind::ContextCheckpoint,
            ),
            s.now,
        );
        s
    });
}

/// Harness self-check: the same scene rendered twice into FRESH caches must
/// be byte-identical. Catches residual env/clock reads inside `render()`
/// before they surface as cross-machine snapshot flakes.
#[test]
fn determinism_same_scene_twice() {
    temp_env::with_var("TZ", Some("UTC"), || {
        let mut s = scene_state();
        s.session
            .append(ChatMessage::user("determinism probe"), s.now);
        s.session
            .append(ChatMessage::assistant("stable output"), s.now);
        let first = render_frame(&s, &mut snapshot_cache(), 80, 24);
        let second = render_frame(&s, &mut snapshot_cache(), 80, 24);
        assert_eq!(first, second, "render must be a pure function of State");
    });
}
