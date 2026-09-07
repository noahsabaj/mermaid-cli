//! Snapshot suite for `render()`: pins the FULL frame for a curated set of
//! scenes at two terminal sizes, so any unintended visual drift — spacing,
//! prefixes, modal layout, status-line wording — fails loudly with a diff
//! instead of slipping past the substring assertions in `tests`.
//!
//! Determinism: every input the frame can observe is pinned — the injected
//! clock (`fixed_now`, which now also decides the day-relative timestamp
//! branch, since the widget derives `today` from it rather than from the
//! wall clock), the `RenderCache` host/user strings, busy-turn start times
//! (derived from the fixture clock), and the cwd (a literal, so it prints the
//! same on Windows).
//! `determinism_same_scene_twice` guards the harness itself: if a residual
//! env or clock read sneaks into `render()`, it fails here before the pinned
//! snapshots start flaking across machines.
//!
//! Timezone: the suite is TZ-INDEPENDENT rather than TZ-pinned (#296). It used
//! to set `TZ=UTC` around each scene — which is what kept it off Windows, where
//! chrono reads the system zone and ignores `TZ`. That pinning did not work on
//! unix either: chrono resolves the local zone once per process and caches it,
//! so mutating `TZ` mid-test changes nothing. The snapshots matched because CI
//! runs in UTC.
//!
//! `fixed_now` now names a fixed LOCAL wall clock instead of a fixed instant,
//! so the one thing a timezone can move — the rendered date and time — reads
//! the same everywhere, and one set of `.snap` files serves every platform.
//! `fixture_clock_reads_the_pinned_wall_clock` pins that string directly, so a
//! regression reports its own cause instead of arriving as a shifted line in
//! twenty-odd frame diffs.
//!
//! Workflow: a mismatch panics with a diff and writes a `.snap.new` sibling
//! (gitignored); run `just snapshots` (`cargo insta review`) to accept or
//! reject. Deliberate visual changes update the `.snap` files in the same PR.

use super::{RenderCache, render_frame};
use mermaid_domain::Config;
use mermaid_domain::{
    ActionDetails, ActionDisplay, ActionResult, ApprovalKind, GenPhase, PendingApproval,
    PendingToolCall, QueuedMessage, State, ToolCallId, TurnId, TurnState, UiMode,
};
use mermaid_model::models::{ChatMessage, ChatMessageKind};

/// The two frame sizes every scene is pinned at: the classic minimum and a
/// roomy modern terminal (exercises wrapping and layout at both extremes).
const SIZES: [(u16, u16); 2] = [(80, 24), (120, 40)];

/// Fixture clock: a fixed local wall clock. Every scene-relative time (the
/// elapsed seconds on a spinner, the age of a checkpoint) is derived by
/// subtracting from this, so their differences stay invariant, and nothing in
/// a frame prints the clock itself any more.
fn fixed_now() -> chrono::DateTime<chrono::Local> {
    use chrono::TimeZone;
    // `earliest` rather than `single`: a DST fold would make this ambiguous.
    // January 2nd at 03:04 sits in no real zone's transition, so this is
    // belt-and-braces — but a panic here would be a baffling way to learn it.
    chrono::Local
        .with_ymd_and_hms(2026, 1, 2, 3, 4, 5)
        .earliest()
        .expect("fixture wall clock exists in the local timezone")
}

/// A `RenderCache` with the machine-dependent values pinned: the home
/// directory (for the session header's `~`) by construction, and the version
/// assigned over, which keeps the suite immune to release bumps.
fn snapshot_cache() -> RenderCache {
    let mut cache = RenderCache::new(Some(std::path::PathBuf::from("/home/snapuser")));
    cache.version = "0.0.0".to_string();
    // Pin the exec-row shell label: frames must not vary by build platform.
    cache.host_shell = mermaid_model::safety::HostShell::Posix;
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
        std::path::PathBuf::from("/tmp"),
    )
}

/// Render `build()`'s scene at every pinned size.
fn assert_scene(name: &str, build: impl Fn() -> State) {
    let state = build();
    for (width, height) in SIZES {
        let frame = render_frame(&state, &mut snapshot_cache(), width, height);
        insta::assert_snapshot!(format!("{name}_{width}x{height}"), frame);
    }
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
                source: mermaid_model::models::tool_call::ToolCall {
                    id: Some("c1".to_string()),
                    function: mermaid_model::models::tool_call::FunctionCall {
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
    use mermaid_domain::checklist::{ChecklistEdit, ChecklistSpec, ChecklistStatus, Stamp};
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
            .map(|(subject, active)| ChecklistSpec {
                subject: (*subject).to_string(),
                active_form: (*active).to_string(),
                description: None,
                in_progress: false,
            })
            .collect(),
        mermaid_domain::ChecklistOrigin::Model,
        Stamp::default(),
    );
    s.session.conversation.tasks.create(
        vec![ChecklistSpec {
            subject: "Double-check the docs".to_string(),
            active_form: "Double-checking the docs".to_string(),
            description: None,
            in_progress: false,
        }],
        mermaid_domain::ChecklistOrigin::User,
        Stamp::default(),
    );
    let edit = |id, status| ChecklistEdit {
        id,
        status: Some(status),
        ..ChecklistEdit::default()
    };
    s.session.conversation.tasks.apply(
        &[edit(1, ChecklistStatus::InProgress)],
        Stamp {
            now_epoch: 100,
            run_tokens: 500,
        },
    );
    s.session.conversation.tasks.apply(
        &[
            edit(1, ChecklistStatus::Completed),
            edit(2, ChecklistStatus::InProgress),
        ],
        Stamp {
            now_epoch: 230,
            run_tokens: 8_900,
        },
    );
    s.session.conversation.tasks.apply(
        &[
            edit(2, ChecklistStatus::Completed),
            edit(3, ChecklistStatus::InProgress),
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
            source: mermaid_model::models::tool_call::ToolCall {
                id: Some("c1".to_string()),
                function: mermaid_model::models::tool_call::FunctionCall {
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
        use mermaid_domain::checklist::{ChecklistEdit, ChecklistStatus, Stamp};
        let mut s = task_run_state();
        let ids: Vec<u32> = s
            .session
            .conversation
            .tasks
            .visible()
            .map(|t| t.id)
            .collect();
        let edits: Vec<ChecklistEdit> = ids
            .into_iter()
            .map(|id| ChecklistEdit {
                id,
                status: Some(ChecklistStatus::Completed),
                ..ChecklistEdit::default()
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
            source: mermaid_model::models::tool_call::ToolCall {
                id: Some(format!("c{id}")),
                function: mermaid_model::models::tool_call::FunctionCall {
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
            mermaid_domain::LiveToolStatus {
                activity: "read_file…".to_string(),
                tokens: 12_300,
            },
        );
        s.ui.live_tool_status.insert(
            ToolCallId(2),
            mermaid_domain::LiveToolStatus {
                activity: "thinking".to_string(),
                tokens: 8_100,
            },
        );
        // One agent detached earlier via Ctrl+B: still on the panel, marked bg.
        s.runtime
            .background_agents
            .push(mermaid_domain::runtime::BackgroundAgent {
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
                    source: mermaid_model::models::tool_call::ToolCall {
                        id: Some("c1".to_string()),
                        function: mermaid_model::models::tool_call::FunctionCall {
                            name: "execute_command".to_string(),
                            arguments: serde_json::json!({"command": "cargo test"}),
                        },
                    },
                },
                PendingToolCall {
                    call_id: ToolCallId(2),
                    source: mermaid_model::models::tool_call::ToolCall {
                        id: Some("c2".to_string()),
                        function: mermaid_model::models::tool_call::FunctionCall {
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
            mermaid_domain::LiveToolStatus {
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
        use mermaid_model::question::{PendingQuestionSet, Question, QuestionOption};
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
        use mermaid_domain::ConversationSummary;
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
    let mut s = scene_state();
    s.session
        .append(ChatMessage::user("determinism probe"), s.now);
    s.session
        .append(ChatMessage::assistant("stable output"), s.now);
    let first = render_frame(&s, &mut snapshot_cache(), 80, 24);
    let second = render_frame(&s, &mut snapshot_cache(), 80, 24);
    assert_eq!(first, second, "render must be a pure function of State");
}

// ---------------------------------------------------------------------------
// The chrome rules behind the snapshots, asserted directly so a regression
// reads as a sentence rather than a frame diff.
// ---------------------------------------------------------------------------

fn frame_lines(state: &State, width: u16, height: u16) -> Vec<String> {
    render_frame(state, &mut snapshot_cache(), width, height)
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect()
}

#[test]
fn session_header_only_on_an_empty_transcript() {
    let empty = frame_lines(&scene_state(), 80, 24);
    assert!(empty[0].contains("mermaid v0.0.0"), "{empty:?}");
    assert!(empty[1].contains("/help for commands"), "{empty:?}");

    // A system-only transcript (the startup capability notice on a degraded
    // machine) keeps the header, so the frame is not environment-dependent.
    let mut notice = scene_state();
    notice.session.append(
        ChatMessage::system("Web capabilities - fetch: native; search: none."),
        notice.now,
    );
    let with_notice = frame_lines(&notice, 80, 24);
    assert!(with_notice[0].contains("mermaid v0.0.0"), "{with_notice:?}");

    let mut chatted = scene_state();
    chatted.session.append(ChatMessage::user("hi"), chatted.now);
    let with_user = frame_lines(&chatted, 80, 24);
    assert!(
        !with_user.iter().any(|l| l.contains("/help for commands")),
        "the first message hides the header: {with_user:?}"
    );
}

#[test]
fn footer_is_one_muted_line_with_no_host_or_cwd() {
    let lines = frame_lines(&scene_state(), 80, 24);
    let footer = lines.last().expect("frame has rows");
    assert!(
        footer.contains("safety:") && footer.contains("reasoning:"),
        "{footer}"
    );
    assert!(
        !footer.contains('@'),
        "no user@host in the footer: {footer}"
    );
    assert!(
        !footer.contains("context"),
        "no gauge until usage is known: {footer}"
    );
    let above = &lines[lines.len() - 2];
    assert!(
        above.starts_with('╰'),
        "one footer row, under the band: {above}"
    );
}

#[test]
fn chrome_never_uses_the_bullet_as_a_separator() {
    let mut busy = scene_state();
    busy.session.append(ChatMessage::user("explain"), busy.now);
    busy.turn = TurnState::Generating {
        id: TurnId(1),
        started: std::time::SystemTime::from(fixed_now() - chrono::Duration::seconds(3)),
        partial_text: "so far".to_string(),
        partial_reasoning: String::new(),
        tokens: 12,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    for state in [scene_state(), busy] {
        for line in frame_lines(&state, 120, 40) {
            assert!(!line.contains('\u{2022}'), "bullet used as chrome: {line}");
        }
    }
}

#[test]
fn spinner_frame_is_a_pure_function_of_elapsed_time() {
    use crate::render::widgets::spinner_glyph;
    use std::time::Duration;
    use unicode_width::UnicodeWidthStr;
    let at = |ms: u64| spinner_glyph(Duration::from_millis(ms));
    assert_eq!(at(0), at(0));
    assert_eq!(
        [at(0), at(150), at(300), at(450), at(600)],
        ["◐ ", "◓ ", "◑ ", "◒ ", "◐ "]
    );
    assert_ne!(at(149), at(150));
    for ms in [0, 150, 300, 450] {
        assert_eq!(at(ms).width(), 2, "every frame is two cells wide");
    }
}

#[test]
fn streaming_meta_has_one_arrow_and_middots() {
    let mut s = scene_state();
    s.session.append(ChatMessage::user("explain"), s.now);
    s.turn = TurnState::Generating {
        id: TurnId(1),
        started: std::time::SystemTime::from(fixed_now() - chrono::Duration::seconds(3)),
        partial_text: "so far".to_string(),
        partial_reasoning: String::new(),
        tokens: 1_234,
        phase: GenPhase::Streaming,
        provider_continuation: None,
        pending_tool_calls: Vec::new(),
        continuation: false,
    };
    let lines = frame_lines(&s, 120, 40);
    let row = lines
        .iter()
        .find(|l| l.contains("Streaming..."))
        .expect("streaming row");
    assert_eq!(row.matches('↓').count(), 1, "{row}");
    assert!(!row.contains('↑') && !row.contains('~'), "{row}");
    assert!(row.contains("· 3s · ↓ 1.2k tokens"), "{row}");
}

#[test]
fn user_prompts_carry_no_timestamp() {
    let mut s = scene_state();
    s.session
        .append(ChatMessage::user("what time is it"), s.now);
    let lines = frame_lines(&s, 120, 40);
    let row = lines
        .iter()
        .find(|l| l.contains("what time is it"))
        .expect("user row");
    assert!(!row.contains(" at "), "{row}");
    assert_eq!(row.trim(), "> what time is it");
}

/// A transcript shorter than the chat area sits against the composer, not
/// at the top of an empty screen: the row above the composer band carries
/// the newest line and the chat area's first row is blank.
#[test]
fn a_short_transcript_sits_against_the_composer() {
    let mut s = scene_state();
    s.session
        .append(ChatMessage::user("only one line so far"), s.now);
    let lines = frame_lines(&s, 80, 24);
    let row = lines
        .iter()
        .position(|l| l.contains("only one line so far"))
        .expect("user row");
    let composer_top = lines
        .iter()
        .position(|l| l.trim_start().starts_with('╭'))
        .expect("composer top border");
    assert!(
        composer_top - row <= 3,
        "the message sits {} rows above the composer:\n{}",
        composer_top - row,
        lines.join("\n")
    );
    assert!(
        lines[0].trim().is_empty(),
        "first chat row must be blank: {:?}",
        lines[0]
    );
}
