use crate::cmd::Cmd;
use crate::compaction::{CompactionRequest, CompactionTrigger};
use crate::msg::SlashCmd;
use crate::query::{Query, QueryResult};
use crate::reducer::*;
use crate::reports::*;
use crate::request::*;
use crate::state::{State, TokenUsageTotals, TurnState, UiMode};
use mermaid_model::models::{ChatMessage, MessageRole};
use mermaid_model::records::TaskStatus;

/// Fork the session at `message_index` (a user message): the ORIGINAL
/// session is saved and preserved; a NEW session (new id, lineage stamped)
/// takes over with `messages[..message_index]` and the composer pre-filled
/// with the selected message — edit and resend to branch the timeline.
///
/// Idle-only by construction (the picker opens from an idle double-Esc), so
/// there is no in-flight `TurnId` or scope to reconcile when the id swaps.
/// Replace the render/persist copy of the checklist with the broker's
/// snapshot, firing `Cmd::NotifyTaskCompleted` for each task that flipped to
/// completed relative to the previous copy (a re-sent identical snapshot
/// fires nothing — the diff is the dedupe).
pub fn handle_tasks_updated(state: &mut State, cmds: &mut Vec<Cmd>, store: crate::ChecklistStore) {
    let (completed, total) = store.counts();
    let fresh: Vec<crate::ChecklistItem> = store
        .newly_completed(&state.session.conversation.tasks)
        .into_iter()
        .cloned()
        .collect();
    for task in fresh {
        cmds.push(Cmd::NotifyTaskCompleted {
            task,
            completed: completed as u32,
            total: total as u32,
        });
    }
    state.session.conversation.tasks = store;
    state.runtime.calls_since_task_update = 0;
}

/// `/todos` — the user's side of the checklist. Bare `/todos` prints the
/// list; edits route through `Cmd::UserTaskEdit` to the `TaskBroker` (single
/// writer), whose publish updates the band and whose notice tells the model.
pub fn handle_todos_command(state: &mut State, cmds: &mut Vec<Cmd>, arg: Option<&str>) {
    use crate::UserChecklistEdit;
    let arg = arg.unwrap_or("").trim();
    if arg.is_empty() {
        state
            .session
            .append(ChatMessage::system(todos_text(state)), state.now);
        return;
    }
    let (verb, rest) = arg.split_once(' ').unwrap_or((arg, ""));
    let rest = rest.trim();
    let parse_id =
        |rest: &str| -> Option<u32> { rest.strip_prefix('#').unwrap_or(rest).parse().ok() };
    let edit = match verb {
        "add" if !rest.is_empty() => Some(UserChecklistEdit::Add {
            subject: rest.to_string(),
        }),
        "rm" | "remove" => parse_id(rest).map(|id| UserChecklistEdit::Remove { id }),
        "done" => parse_id(rest).map(|id| UserChecklistEdit::Done { id }),
        "clear" => Some(UserChecklistEdit::Clear),
        _ => None,
    };
    match edit {
        Some(edit) => cmds.push(Cmd::UserTaskEdit(edit)),
        None => state.session.append(
            ChatMessage::system(
                "usage: /todos [add <subject> | rm <id> | done <id> | clear]".to_string(),
            ),
            state.now,
        ),
    }
}

/// The `/todos` listing: the full checklist with descriptions, cost stamps,
/// and recent evidence — the detail view the compact band doesn't show.
#[must_use]
pub fn todos_text(state: &State) -> String {
    let store = &state.session.conversation.tasks;
    if store.is_empty() {
        return "No tasks. The model creates them for multi-step work, or add your own: \
                /todos add <subject>"
            .to_string();
    }
    let mut out = String::from("Task checklist\n");
    for task in store.visible() {
        out.push_str(&format!(
            "  #{} [{}] {}{}\n",
            task.id,
            task.status.as_str(),
            task.subject,
            if task.origin == crate::ChecklistOrigin::User {
                " (you)"
            } else {
                ""
            }
        ));
        if let Some(desc) = &task.description {
            out.push_str(&format!("      {desc}\n"));
        }
        let mut cost = Vec::new();
        if let Some(secs) = task.elapsed_secs() {
            cost.push(format!("{secs}s"));
        }
        if let Some(tok) = task.tokens_spent {
            cost.push(format!("{tok} tokens"));
        }
        if !cost.is_empty() {
            out.push_str(&format!("      cost: {}\n", cost.join(" · ")));
        }
        for entry in task.evidence.iter().rev().take(3).rev() {
            out.push_str(&format!(
                "      evidence: {} {} ({})\n",
                entry.tool, entry.target, entry.status
            ));
        }
    }
    out.push_str(&format!("  {}", store.progress_string()));
    out
}

/// Cap on buffered checklist notices — same spirit as the hook-context cap;
/// notices are one-liners, so a small count bound suffices.
pub const MAX_TASK_NOTICES: usize = 8;

/// Buffer a checklist notice for the model's next request. NOT turn-gated:
/// `/todos` edits and vetoed completions arrive between turns and must
/// survive until the next real dispatch.
pub fn push_task_notice(state: &mut State, text: String) {
    if state.pending_task_notices.len() >= MAX_TASK_NOTICES {
        tracing::warn!("dropping task notice over the {MAX_TASK_NOTICES}-entry cap");
        return;
    }
    state.pending_task_notices.push(text);
}

/// How many model-call cycles an `in_progress` task may sit untouched before
/// the reducer injects a staleness nudge (then re-arms for another window).
pub const TASK_STALENESS_CALLS: u32 = 5;

/// Route a completed `Cmd::Query` lookup into state — one arm per
/// [`QueryResult`] variant, bodies moved verbatim from the former
/// per-`Msg` arms. None of these are turn-scoped; each surface applies
/// its own staleness rule (a picker only fills while it is still open,
/// transcript listings always append).
pub fn handle_query_result(state: &mut State, cmds: &mut Vec<Cmd>, result: QueryResult) {
    match result {
        QueryResult::ConversationLoaded(history) => {
            // If a turn was in flight when the user loaded another conversation
            // (`/load` mid-generation), cancel its scope first. Otherwise we
            // overwrite `state.turn` to `Idle` below and lose the only handle —
            // the turn's CancellationToken + JoinSet — that could stop the
            // running model call and tool tasks, orphaning them uncancellable;
            // their parked approval requests could never be answered either (#2).
            if let Some(id) = state.turn.id() {
                cmds.push(Cmd::CancelScope(id));
                // Drop the cancelled turn's parked approval/question modals and
                // its stale running-tool indicators — the tasks behind them are
                // being torn down.
                clear_parked_tool_requests(state);
                state.ui.live_tool_status.clear();
            }
            // Messages queued against the *previous* conversation must not
            // auto-submit into the one being loaded — drop them (mirrors the
            // clears above).
            state.ui.queued_messages.clear();
            state.session.replace_conversation(*history);
            state.turn = TurnState::Idle;
            // The abandoned run's summary counters die with it: a leaked
            // `run_started` would otherwise let a later `finish_run` (quit)
            // stamp the OLD run's summary into the conversation loaded here.
            reset_run_counters(state);
            state.ui.mode = UiMode::EditingInput;
            // The pause belonged to the previous conversation's failing
            // compaction; the loaded one starts fresh.
            state.runtime.auto_compact_suppressed = false;
            // The loaded conversation has its own id — the previous session's
            // scratch dir no longer applies. Recompute (same as `/clear`).
            refresh_scratchpad(state, cmds);
            emit_title_if_changed(state, cmds);
        },
        QueryResult::AvailableModelsListed(candidates) => {
            // Only fill a picker that is still open — Esc before discovery
            // landed drops the event, exactly like `ConversationsListed`.
            if let UiMode::ModelPicker { query, cursor, .. } = &state.ui.mode {
                let (query, cursor) = (query.clone(), *cursor);
                let matches = filter_model_choices(&candidates, &query).len();
                state.ui.mode = UiMode::ModelPicker {
                    candidates,
                    query,
                    // Keep the cursor if the user already moved it, but never
                    // leave it past the end of the freshly-arrived list.
                    cursor: cursor.min(matches.saturating_sub(1)),
                    loading: false,
                };
            }
        },
        QueryResult::ConversationsListed(candidates) => {
            if let UiMode::ConversationList { .. } = state.ui.mode {
                state.ui.mode = UiMode::ConversationList {
                    candidates,
                    cursor: 0,
                };
            }
            // If the user already navigated away (Esc before the
            // list landed), the event silently drops.
        },
        QueryResult::ProjectFilesListed(files) => {
            state.ui.project_files_loading = false;
            state.ui.project_files = Some(files);
            // Stale-while-revalidate: the user has been filtering the old
            // cache; swap the fresh list in and re-rank the open picker.
            recompute_file_matches(state);
        },
        QueryResult::RuntimeTasksListed(tasks) => {
            append_runtime_note(state, cmds, tasks_text(&tasks));
        },
        QueryResult::RuntimeTaskLoaded { task, events } => {
            append_runtime_note(state, cmds, task_detail_text(task.as_deref(), &events));
        },
        QueryResult::RuntimeProcessesListed(processes) => {
            append_runtime_note(state, cmds, processes_text(&processes));
        },
        QueryResult::RuntimeApprovalsListed(approvals) => {
            append_runtime_note(state, cmds, approvals_text(&approvals));
        },
        QueryResult::RuntimeCheckpointsListed(checkpoints) => {
            append_runtime_note(state, cmds, checkpoints_text(&checkpoints));
        },
        QueryResult::ForkCheckpointsFound(checkpoints) => {
            // Reply to the rewind/fork lookup. Files were NOT rewound —
            // point the user at the oldest checkpoint past the cut (each is
            // a PRE-mutation snapshot, so the oldest holds file state
            // closest to the fork point). Empty = nothing to say.
            if let Some(oldest) = checkpoints.first() {
                push_system(
                    state,
                    cmds,
                    format!(
                        "{} file checkpoint(s) were created after this point. Files were \
                         not rewound. /restore {} restores the files changed by the first \
                         mutation after the fork; /checkpoints lists the rest.",
                        checkpoints.len(),
                        oldest.id
                    ),
                );
            }
        },
        QueryResult::RuntimePluginsListed(plugins) => {
            append_runtime_note(state, cmds, plugins_text(&plugins));
        },
    }
}

/// Append a runtime listing (or generic runtime text) to the transcript as a
/// system note and persist — the shared tail of every `/runtime`-family
/// answer and of `Msg::RuntimeText`.
pub fn append_runtime_note(state: &mut State, cmds: &mut Vec<Cmd>, text: String) {
    state.session.append(ChatMessage::system(text), state.now);
    cmds.push(state.session.save_conversation_cmd());
}

#[expect(
    clippy::too_many_lines,
    reason = "predates the lint; see .github/baselines/expect_budget.txt"
)]
pub fn handle_slash(state: &mut State, cmds: &mut Vec<Cmd>, cmd: SlashCmd) {
    match cmd {
        SlashCmd::Model(None) => {
            // Open the picker immediately and fill it when discovery lands, so
            // a slow provider costs a late row rather than a frozen keystroke.
            state.ui.mode = UiMode::ModelPicker {
                candidates: Vec::new(),
                query: String::new(),
                cursor: 0,
                loading: true,
            };
            cmds.push(Cmd::Query(Query::ListAvailableModels));
        },
        SlashCmd::Model(Some(new_model)) => {
            switch_model(state, cmds, new_model);
        },
        SlashCmd::Reasoning(None) => {
            push_system(
                state,
                cmds,
                format!("Reasoning: {}", state.session.reasoning.as_str()),
            );
        },
        SlashCmd::Reasoning(Some(level)) => {
            state.session.reasoning = level;
            cmds.push(Cmd::PersistReasoningFor {
                model_id: state.session.model_id.clone(),
                level,
            });
        },
        SlashCmd::Safety(None) => {
            push_system(
                state,
                cmds,
                format!(
                    "Safety: {} — options: plan, read_only, ask, auto, full_access (Shift+Tab \
                     cycles)",
                    state.session.safety_mode.as_str()
                ),
            );
        },
        SlashCmd::Safety(Some(mode)) => {
            // `plan` is a mode like any other here — `apply_safety_mode` runs
            // the plan-file allocation / teardown that entering or leaving it
            // needs. Session-scoped (mirrors Shift+Tab) — not written to config.
            apply_safety_mode(state, cmds, mode);
            // The bottom status bar shows the new mode — no banner.
        },
        SlashCmd::Plan(arg) => match arg.as_deref().map(str::trim) {
            None | Some("") | Some("on") => enter_plan_mode(state, cmds),
            Some("off") => exit_plan_mode(state, cmds),
            Some("show") => match &state.session.plan {
                Some(plan) => {
                    let path = plan_path_display(state, &plan.plan_path.clone());
                    push_system(state, cmds, format!("Plan file (drafting): {path}"));
                },
                None => push_system(
                    state,
                    cmds,
                    "Not in plan mode (/plan or Shift+Tab enters it)",
                ),
            },
            Some("config") => {
                state.ui.mode = UiMode::PlanConfig { cursor: 0 };
            },
            Some(_) => push_system(state, cmds, "Usage: /plan [off|show|config]"),
        },
        SlashCmd::Config => {
            state.ui.mode = UiMode::PlanConfig { cursor: 0 };
        },
        SlashCmd::VisibleReasoning(arg) => {
            match visible_reasoning_value(arg.as_deref(), state.ui.show_reasoning) {
                Ok(next) => {
                    state.ui.show_reasoning = next;
                    push_system(
                        state,
                        cmds,
                        if next {
                            "Visible reasoning: on"
                        } else {
                            "Visible reasoning: off"
                        },
                    );
                },
                Err(usage) => {
                    push_system(state, cmds, usage);
                },
            }
        },
        SlashCmd::Clear => {
            // Guard with a confirmation modal.
            state.confirm = Some(crate::state::Confirmation {
                prompt: "Clear conversation history?".to_string(),
                accept_msg_token: crate::state::ConfirmationTarget::ClearConversation,
            });
        },
        SlashCmd::Save(_name) => {
            cmds.push(state.session.save_conversation_cmd());
        },
        SlashCmd::Load(Some(id)) => {
            cmds.push(Cmd::Query(Query::LoadConversation { id }));
        },
        SlashCmd::Load(None) | SlashCmd::List => {
            // Transition to the picker. Effect handler scans the
            // conversations directory; the reducer fills in
            // candidates when `QueryResult::ConversationsListed` arrives.
            state.ui.mode = UiMode::ConversationList {
                candidates: Vec::new(),
                cursor: 0,
            };
            cmds.push(Cmd::Query(Query::ListConversations));
        },
        SlashCmd::Usage => {
            state
                .session
                .append(ChatMessage::system(usage_text(state)), state.now);
            cmds.push(state.session.save_conversation_cmd());
        },
        SlashCmd::Todos(arg) => {
            handle_todos_command(state, cmds, arg.as_deref());
        },
        SlashCmd::Scratchpad => {
            // Listing needs the filesystem, so it runs as an effect; the
            // reducer only answers when there is no directory to list.
            match &state.session.scratchpad {
                Some(path) => cmds.push(Cmd::ListScratchpad { path: path.clone() }),
                None => {
                    state.session.append(
                        ChatMessage::system(
                            "No scratchpad yet for this session — it is created at startup \
                             and stamped shortly after; try again in a moment."
                                .to_string(),
                        ),
                        state.now,
                    );
                },
            }
        },
        SlashCmd::Context(cmd) => {
            use crate::ContextCmd;
            let model_id = state.session.model_id.clone();
            let is_ollama = model_id.starts_with("ollama/");
            match cmd {
                ContextCmd::Show => {
                    state
                        .session
                        .append(ChatMessage::system(context_text(state)), state.now);
                    cmds.push(state.session.save_conversation_cmd());
                },
                // The sizing knobs only affect Ollama's num_ctx.
                _ if !is_ollama => {
                    push_system(
                        state,
                        cmds,
                        format!(
                            "/context sizing applies to Ollama models; the active model is {model_id}."
                        ),
                    );
                },
                ContextCmd::Set(n) => {
                    state
                        .settings
                        .ollama_num_ctx_per_model
                        .insert(model_id.clone(), n);
                    cmds.push(Cmd::PersistOllamaNumCtxFor {
                        model_id,
                        num_ctx: Some(n),
                    });
                    push_system(
                        state,
                        cmds,
                        format!("Context window set to {n} tokens — applies to the next message."),
                    );
                },
                ContextCmd::Auto => {
                    state.settings.ollama_num_ctx_per_model.remove(&model_id);
                    // Also drop any auto-converged value so it re-fits from scratch.
                    state.runtime.ollama_converged_num_ctx.remove(&model_id);
                    cmds.push(Cmd::PersistOllamaNumCtxFor {
                        model_id,
                        num_ctx: None,
                    });
                    push_system(
                        state,
                        cmds,
                        "Context window back to auto-fit (sized to your GPU's VRAM) — applies to the next message.",
                    );
                },
                ContextCmd::Max => {
                    match state
                        .runtime
                        .ollama_context
                        .as_ref()
                        .and_then(|c| c.model_max)
                    {
                        Some(max) => {
                            let max_u32 = max.min(u32::MAX as usize) as u32;
                            state
                                .settings
                                .ollama_num_ctx_per_model
                                .insert(model_id.clone(), max_u32);
                            cmds.push(Cmd::PersistOllamaNumCtxFor {
                                model_id,
                                num_ctx: Some(max_u32),
                            });
                            push_system(
                                state,
                                cmds,
                                format!(
                                    "Context window set to the model's max ({max} tokens) — applies to the next message. \
                                     This may exceed VRAM; if it gets slow, enable `/context offload on`."
                                ),
                            );
                        },
                        None => {
                            push_system(
                                state,
                                cmds,
                                "Model's max window isn't known yet — send a message first, then `/context max`.",
                            );
                        },
                    }
                },
                ContextCmd::Offload(on) => {
                    state.settings.ollama.allow_ram_offload = on;
                    cmds.push(Cmd::PersistOllamaOffload(on));
                    push_system(
                        state,
                        cmds,
                        format!(
                            "RAM offload {} — applies to the next message. {}",
                            if on { "enabled" } else { "disabled" },
                            if on {
                                "Larger context windows are allowed, but inference may be much slower."
                            } else {
                                "Context auto-fits to VRAM to stay fast."
                            }
                        ),
                    );
                },
            }
        },
        SlashCmd::Compact(instructions) => {
            handle_manual_compact(state, cmds, instructions);
        },
        SlashCmd::Memory => {
            cmds.push(Cmd::ListMemory);
        },
        SlashCmd::Remember(text) => {
            cmds.push(Cmd::RememberMemory { text });
        },
        SlashCmd::Forget(id) => {
            cmds.push(Cmd::ForgetMemory { id });
        },
        SlashCmd::ConsolidateMemory => {
            cmds.push(Cmd::ConsolidateMemory {
                model_id: state.session.model_id.clone(),
            });
        },
        SlashCmd::Doctor => {
            state
                .session
                .append(ChatMessage::system(doctor_text(state)), state.now);
            cmds.push(state.session.save_conversation_cmd());
        },
        SlashCmd::Tasks => {
            cmds.push(Cmd::Query(Query::ListRuntimeTasks { limit: 10 }));
        },
        SlashCmd::Task(id) => {
            cmds.push(Cmd::Query(Query::LoadRuntimeTask { id }));
        },
        SlashCmd::Pause(id) => {
            cmds.push(Cmd::UpdateRuntimeTaskStatus {
                id,
                status: TaskStatus::Blocked,
                final_report: Some("Paused from TUI".to_string()),
            });
        },
        SlashCmd::Resume(id) => {
            cmds.push(Cmd::UpdateRuntimeTaskStatus {
                id,
                status: TaskStatus::Running,
                final_report: None,
            });
        },
        SlashCmd::Cancel(Some(id)) => {
            cmds.push(Cmd::UpdateRuntimeTaskStatus {
                id,
                status: TaskStatus::Cancelled,
                final_report: Some("Cancelled from TUI".to_string()),
            });
        },
        SlashCmd::Cancel(None) => {
            if matches!(state.turn, TurnState::Idle) {
                push_system(state, cmds, "No active turn to cancel.");
            } else {
                handle_cancel_turn(state, cmds);
            }
        },
        SlashCmd::Handoff(Some(id)) | SlashCmd::Report(Some(id)) => {
            cmds.push(Cmd::Query(Query::LoadRuntimeTask { id }));
        },
        SlashCmd::Handoff(None) => {
            let text = format!(
                "Handoff report\n\n{}\n\n{}",
                context_text(state),
                usage_text(state)
            );
            state.session.append(ChatMessage::system(text), state.now);
            cmds.push(state.session.save_conversation_cmd());
        },
        SlashCmd::Report(None) => {
            let text = format!(
                "Runtime report\n\n{}\n\n{}",
                context_text(state),
                usage_text(state)
            );
            state.session.append(ChatMessage::system(text), state.now);
            cmds.push(state.session.save_conversation_cmd());
        },
        SlashCmd::Processes => {
            cmds.push(Cmd::Query(Query::ListRuntimeProcesses { limit: 10 }));
        },
        SlashCmd::Agents(arg) => {
            handle_slash_agents(state, cmds, arg.as_deref());
        },
        SlashCmd::Logs(id) => {
            cmds.push(Cmd::ShowRuntimeProcessLogs { id });
        },
        SlashCmd::Stop(id) => {
            cmds.push(Cmd::StopRuntimeProcess { id });
        },
        SlashCmd::Restart(id) => {
            cmds.push(Cmd::RestartRuntimeProcess { id });
        },
        SlashCmd::Open(target) => {
            cmds.push(Cmd::OpenRuntimeTarget { target });
        },
        SlashCmd::Ports => {
            cmds.push(Cmd::ShowRuntimePorts);
        },
        SlashCmd::Approvals => {
            cmds.push(Cmd::Query(Query::ListRuntimeApprovals));
        },
        SlashCmd::Approve(id) => {
            cmds.push(Cmd::DecideRuntimeApproval {
                id,
                decision: "approved".to_string(),
            });
        },
        SlashCmd::Deny(id) => {
            cmds.push(Cmd::DecideRuntimeApproval {
                id,
                decision: "denied".to_string(),
            });
        },
        SlashCmd::Checkpoint(paths) => {
            let paths = paths
                .split_whitespace()
                .map(std::path::PathBuf::from)
                .collect::<Vec<_>>();
            cmds.push(Cmd::CreateRuntimeCheckpoint { paths });
        },
        SlashCmd::Checkpoints => {
            cmds.push(Cmd::Query(Query::ListRuntimeCheckpoints { limit: 10 }));
        },
        SlashCmd::Restore(id) => {
            cmds.push(Cmd::RestoreRuntimeCheckpoint { id });
        },
        SlashCmd::Plugins => {
            cmds.push(Cmd::Query(Query::ListRuntimePlugins));
        },
        SlashCmd::ModelInfo(model) => {
            cmds.push(Cmd::ShowRuntimeModelInfo { model });
        },
        SlashCmd::CloudSetup => {
            // Cloud setup needs interactive stdin (rpassword) which
            // fights with ratatui's raw mode. The in-TUI command
            // points users at the `mermaid cloud-setup` subcommand
            // instead — clean separation of modes.
            push_system(
                state,
                cmds,
                "Run `mermaid cloud-setup` from your shell, then restart mermaid.",
            );
        },
        SlashCmd::Theme(arg) => {
            use crate::ThemeChoice;
            // The trailing NO_COLOR note keeps a persisted-but-invisible
            // switch from reading as a broken command.
            let no_color_note = if state.ui.no_color {
                " NO_COLOR is set, so colors stay disabled until it is unset."
            } else {
                ""
            };
            let choice = match arg.as_deref().map(str::trim) {
                None | Some("") => {
                    push_system(
                        state,
                        cmds,
                        format!(
                            "Theme: {}. Usage: /theme <dark|light>.{}",
                            state.ui.theme.as_str(),
                            no_color_note
                        ),
                    );
                    return;
                },
                Some("dark") => ThemeChoice::Dark,
                Some("light") => ThemeChoice::Light,
                Some(other) => {
                    push_system(
                        state,
                        cmds,
                        format!("Unknown theme '{other}'. Usage: /theme <dark|light>"),
                    );
                    return;
                },
            };
            state.ui.theme = choice;
            cmds.push(Cmd::PersistUiTheme(choice));
            push_system(
                state,
                cmds,
                format!(
                    "Theme set to {} (persisted).{}",
                    choice.as_str(),
                    no_color_note
                ),
            );
        },
        SlashCmd::Editor => {
            // `/editor` opens on whatever draft remains after the command
            // itself was consumed (usually empty); Ctrl+O is the
            // draft-preserving path.
            cmds.push(Cmd::ComposeInEditor {
                text: state.ui.input_buffer.clone(),
            });
        },
        SlashCmd::Help => {
            state.session.append(
                ChatMessage::system(help_text(&state.plugin_commands)),
                state.now,
            );
            cmds.push(state.session.save_conversation_cmd());
        },
        SlashCmd::Quit => {
            request_exit(state, cmds);
        },
        SlashCmd::MissingArg(usage) => {
            push_system(state, cmds, usage);
        },
        SlashCmd::Unknown(name) => {
            push_system(state, cmds, format!("Unknown command: /{name}"));
        },
    }
}

pub fn visible_reasoning_value(arg: Option<&str>, current: bool) -> Result<bool, &'static str> {
    match arg.map(str::trim).filter(|s| !s.is_empty()) {
        None | Some("toggle") => Ok(!current),
        Some("on") | Some("true") | Some("yes") | Some("show") => Ok(true),
        Some("off") | Some("false") | Some("no") | Some("hide") => Ok(false),
        Some(_) => Err("Usage: /visible-reasoning [on|off|toggle]"),
    }
}

/// Append a one-off system note to the chat transcript (and persist it).
///
/// This is where command feedback, errors, and query answers go now that the
/// transient status banner above the input is gone — they live in the
/// scrollable transcript instead of flashing in the spinner's row. The zone
/// above the input is reserved for the generation spinner alone.
pub fn push_system(state: &mut State, cmds: &mut Vec<Cmd>, text: impl Into<String>) {
    push_system_kind(
        state,
        cmds,
        text,
        mermaid_model::models::ChatMessageKind::Normal,
    );
}

/// `push_system` with an explicit message kind. The recovery tails use it to
/// stamp their one-shot nudges `RecoveryNudge` so the transcript hides them
/// and `sweep_spent_nudges` retires them at the next turn-end.
pub fn push_system_kind(
    state: &mut State,
    cmds: &mut Vec<Cmd>,
    text: impl Into<String>,
    kind: mermaid_model::models::ChatMessageKind,
) {
    // While tools are mid-flight the trailing message is the committed
    // `assistant(tool_calls)` whose `tool` results haven't landed yet. Appending
    // a system note *after* it wedges a message between the `tool_use` and its
    // `tool_result` — which OpenAI- and Ollama-shaped providers reject on the
    // next request (Anthropic and Gemini happen to drop mid-history system
    // messages, but we can't lean on that for every backend). Insert the note
    // just *before* that assistant message so the pair stays adjacent; as a
    // bonus the assistant message stays last, so in-flight tool actions/images
    // still attach to it. Anywhere else, plain append.
    let messages = state.session.conversation.messages();
    // Also guard `Compacting`: a `ContextLimitRetry`/`TruncationRecovery`
    // compaction keeps a trailing unpaired `tool_use` (see `preserve_pending_tail`),
    // so a mid-compaction `push_system` (e.g. `McpServerErrored`) must insert
    // before it too — otherwise the next request wedges a system note between the
    // `tool_use` and its `tool_result`.
    let would_split = matches!(
        state.turn,
        TurnState::ExecutingTools { .. } | TurnState::Compacting { .. }
    ) && messages
        .last()
        .is_some_and(|m| m.role == MessageRole::Assistant && m.tool_calls.is_some());
    let mut msg = ChatMessage::system(text.into());
    msg.kind = kind;
    if would_split {
        state.session.insert_before_last(msg);
    } else {
        state.session.append(msg, state.now);
    }
    // A `RecoveryNudge` is swept at the next turn end, so persisting it buys
    // nothing and costs a full transcript re-serialization. The plan-mode
    // reminder is one of these and rides EVERY model call, so this fired once
    // per dispatch for a byte-identical message that is guaranteed to be gone
    // before the save could ever be read back. Durable kinds still save.
    if kind != mermaid_model::models::ChatMessageKind::RecoveryNudge {
        cmds.push(state.session.save_conversation_cmd());
    }
}

/// Retire spent recovery nudges. A `RecoveryNudge` steers exactly one request
/// (auto-continue resume, stalled-turn retry); by the time any turn-end
/// arrives that request has already gone out, so the note is dead weight —
/// worse, if it lingered it would keep instructing the model on the user's
/// *next, unrelated* turn while the transcript hides it. Returns whether
/// anything was removed so callers on paths without a save can persist.
pub fn sweep_spent_nudges(state: &mut State) -> bool {
    let messages = state.session.conversation.messages_mut();
    let before = messages.len();
    messages.retain(|m| m.kind != mermaid_model::models::ChatMessageKind::RecoveryNudge);
    messages.len() != before
}

#[must_use]
pub fn ollama_pull_target(model_id: &str) -> Option<String> {
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }
    let (provider, model) = match model_id.split_once('/') {
        Some((provider, model)) => (provider, model),
        None => ("ollama", model_id),
    };
    if !provider.eq_ignore_ascii_case("ollama") {
        return None;
    }
    let model = model.trim();
    if model.is_empty() || model.ends_with(":cloud") {
        None
    } else {
        Some(model.to_string())
    }
}

pub fn handle_manual_compact(state: &mut State, cmds: &mut Vec<Cmd>, instructions: Option<String>) {
    if !matches!(state.turn, TurnState::Idle) {
        push_system(state, cmds, "Cannot compact while a turn is active.");
        return;
    }

    if state.session.messages().len() < 3 {
        push_system(state, cmds, "Not enough conversation history to compact.");
        return;
    }

    // Instructions/memory are kept fresh by the config watcher (#45); read as
    // injected data so the reducer does no I/O before building the request.
    let turn = state.ids.fresh_turn();
    state.turn = TurnState::Compacting {
        id: turn,
        started: std::time::SystemTime::from(state.now),
        trigger: CompactionTrigger::Manual,
        resume_continuation: false,
    };
    // The live "Compacting…" status comes from the TurnState::Compacting status
    // line (the blue indicator); no separate gray status message — it was a
    // redundant duplicate. The completion receipt is set on CompactionFinished.
    // An explicit /compact is the user's retry lever: un-pause auto-compaction
    // so this attempt (and later turns) get a fresh shot.
    state.runtime.auto_compact_suppressed = false;
    cmds.push(Cmd::CompactConversation {
        turn,
        request: CompactionRequest::manual(
            build_chat_request(state),
            instructions,
            state.settings.compaction.policy(),
        ),
    });
}

pub fn handle_confirm_accepted(state: &mut State, cmds: &mut Vec<Cmd>) {
    let Some(confirm) = state.confirm.take() else {
        return;
    };
    match confirm.accept_msg_token {
        crate::state::ConfirmationTarget::ClearConversation => {
            // If a turn was still in flight when the user cleared, cancel its
            // scope first and reset to `Idle` — mirroring `QueryResult::ConversationLoaded`
            // (#2, F34). Without this the orphaned model/tool tasks keep running
            // (tools keep mutating files after a "clear"), and the still-active
            // turn's same-id `StreamDone`/`ToolFinished` would pass the stale
            // filter and commit a stray message into the freshly-cleared
            // conversation. The cancelled turn's parked approval requests can
            // never be answered, so drop them too.
            if let Some(id) = state.turn.id() {
                cmds.push(Cmd::CancelScope(id));
                // Drop the cancelled turn's parked approval/question modals and
                // stale running-tool indicators before wiping the conversation.
                clear_parked_tool_requests(state);
                state.ui.live_tool_status.clear();
            }
            // A message queued mid-turn belonged to the conversation being
            // wiped — don't let it auto-submit into the fresh one.
            state.ui.queued_messages.clear();
            // Clear = start a fresh conversation: new ID, new default
            // title, empty history, zero cumulative tokens. Matches
            // user mental model ("wipe everything").
            let project_path = state.session.conversation.project_path.clone();
            let model_name = state.session.conversation.model_name.clone();
            // Carry the git branch forward: the impure startup can't re-detect
            // it inside the pure reducer, and the cleared session is still the
            // same working tree.
            let git_branch = state.session.conversation.git_branch.clone();
            state.session.conversation =
                crate::ConversationHistory::new(project_path, model_name, state.now);
            state.session.conversation.git_branch = git_branch;
            state.session.last_token_usage = None;
            state.session.cumulative_token_usage = TokenUsageTotals::default();
            // Same rationale as `ConversationLoaded`: the cleared-away run's
            // summary counters must not survive into the fresh conversation.
            reset_run_counters(state);
            // A cleared conversation is not a conversation of unknown size — it
            // is a KNOWN, non-zero one: the system prompt and every advertised
            // tool schema still ride the next request, routinely tens of
            // thousands of tokens before the user types anything. Blanking the
            // gauge to `context: n/a` hid that floor until the first reply came
            // back. Same treatment as a rewind (see `estimate_current_context`).
            //
            // Cumulative spend above stays reset: those tokens were really
            // spent, and that is a different number.
            state.session.context_usage = Some(estimate_current_context(state));
            state.runtime.auto_compact_suppressed = false;
            state.turn = TurnState::Idle;
            // The fresh conversation starts with an empty checklist; the
            // broker must forget the old one too (single-writer sync).
            cmds.push(Cmd::SyncTaskStore(crate::ChecklistStore::default()));
            // New conversation id -> new scratch dir. The old one stays on
            // disk until the sweep reaps it (its pid lock expires with us).
            refresh_scratchpad(state, cmds);
            emit_title_if_changed(state, cmds);
        },
    }
}
