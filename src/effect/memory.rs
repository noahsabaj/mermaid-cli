//! Memory consolidation: the prune plan and the model round-trip that
//! produces it. Touches no `EffectRunner` state.
use std::sync::Arc;

use super::*;

/// Derive a short title for a `/remember` memory from free-text input: the
/// first non-empty line, capped to ~8 words / 60 chars. `write_memory`
/// slugifies it into the filename.
pub(super) fn memory_title_from_text(text: &str) -> String {
    let first = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("memory")
        .trim();
    let title: String = first
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect();
    if title.trim().is_empty() {
        "memory".to_string()
    } else {
        title
    }
}

pub(super) const CONSOLIDATE_SYSTEM_PROMPT: &str = "You maintain a coding agent's durable memory: a set of atomic facts. Your only job is to find facts that are EXACT DUPLICATES or CLEARLY OBSOLETE/SUPERSEDED by another fact, and list their ids for pruning. Never prune facts that are merely related or similar but carry distinct information. Never rewrite or merge facts. When in doubt, keep. Reply with ONLY a JSON object: {\"prune\": [\"id1\", \"id2\"], \"reason\": \"one short sentence\"}. If nothing should be pruned, return an empty prune list.";

#[derive(Debug)]
pub(super) struct PrunePlan {
    pub(super) prune: Vec<String>,
    pub(super) reason: String,
}

/// Extract a `{prune:[...], reason:""}` plan from a model response, tolerating
/// prose or code fences around the JSON object.
pub(super) fn parse_prune_plan(text: &str) -> Option<PrunePlan> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let json: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let prune = json
        .get("prune")?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let reason = json
        .get("reason")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Some(PrunePlan { prune, reason })
}

/// The one-shot prune request: every memory listed with its id, scope,
/// description and flattened body, under the consolidation system prompt.
fn consolidation_request(
    items: &[(mermaid_domain::MemoryEntry, String)],
    model_id: &str,
) -> mermaid_domain::ChatRequest {
    let mut listing = String::new();
    for (entry, body) in items {
        let id = entry
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(entry.name.as_str());
        listing.push_str(&format!(
            "- id: {id}\n  scope: {}\n  description: {}\n  body: {}\n",
            entry.scope.as_str(),
            entry.description,
            body.replace('\n', " ").trim(),
        ));
    }
    let user = format!(
        "Here are {} durable memory facts. Identify exact duplicates and clearly obsolete or superseded facts to prune.\n\n{}",
        items.len(),
        listing
    );
    mermaid_domain::ChatRequest {
        model_id: model_id.to_string(),
        messages: vec![mermaid_model::models::ChatMessage::user(user)],
        system_prompt: CONSOLIDATE_SYSTEM_PROMPT.to_string(),
        instructions: None,
        reasoning: mermaid_model::models::ReasoningLevel::None,
        temperature: 0.0,
        max_tokens: 1024,
        tools: Vec::new(),
        ollama_num_ctx: None,
        ollama_allow_ram_offload: None,
        resolved_context_window: None,
        resolved_max_output: None,
        output_schema: None,
        suppress_auto_compact: false,
        suppressed_builtin_tools: Vec::new(),
    }
}

/// Snapshot the to-be-pruned files first so the prune is reversible. The
/// delete that follows is irreversible, so a failed checkpoint must NOT
/// proceed — otherwise the report would advertise "Recoverable from the latest
/// checkpoint" for a prune with no checkpoint behind it (#F69). The `Err` is
/// the abort message for the user; nothing has been deleted at that point, so
/// no memory is lost.
fn checkpoint_prune_targets(workdir: &std::path::Path, plan: &PrunePlan) -> Result<(), String> {
    let paths: Vec<std::path::PathBuf> = plan
        .prune
        .iter()
        .filter_map(|id| crate::app::memory::find(workdir, id).map(|e| e.path))
        .collect();
    if !paths.is_empty()
        && let Err(e) = mermaid_runtime::create_checkpoint(
            workdir,
            &paths,
            Some(serde_json::json!({ "tool": "consolidate_memory", "reason": plan.reason })),
        )
    {
        return Err(format!(
            "Memory consolidation aborted: couldn't checkpoint the {} file{} marked for pruning, so nothing was deleted (no memory lost). Error: {e}",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" },
        ));
    }
    Ok(())
}

/// `/consolidate-memory`: a one-shot model pass that names duplicate/obsolete
/// facts to prune (never rewrites — that's the anti-drift rule). The pruned
/// files are snapshotted into a checkpoint first, so the prune is reversible.
pub(super) async fn consolidate_memory(
    tx: MsgSender,
    providers: Option<Arc<ProviderFactory>>,
    workdir: std::path::PathBuf,
    model_id: String,
) {
    let items = crate::app::memory::entries_with_bodies(&workdir);
    if items.len() < 2 {
        let _ = tx
            .send(Msg::RuntimeText(format!(
                "Nothing to consolidate — {} memor{} saved.",
                items.len(),
                if items.len() == 1 { "y" } else { "ies" }
            )))
            .await;
        return;
    }
    let Some(factory) = providers else {
        let _ = tx
            .send(Msg::RuntimeText(
                "Memory consolidation needs a model provider, which isn't bound in this session."
                    .to_string(),
            ))
            .await;
        return;
    };

    let request = consolidation_request(&items, &model_id);

    let provider = match factory.resolve(&model_id).await {
        Ok(p) => p,
        Err(e) => {
            let _ = tx
                .send(Msg::RuntimeText(format!(
                    "Memory consolidation failed: {e}"
                )))
                .await;
            return;
        },
    };
    let token = tokio_util::sync::CancellationToken::new();
    let text =
        match crate::providers::model::collect_text(provider, TurnId(0), request, token).await {
            Ok(collected) => collected.text,
            Err(e) => {
                let _ = tx
                    .send(Msg::RuntimeText(format!(
                        "Memory consolidation failed: {e}"
                    )))
                    .await;
                return;
            },
        };

    let Some(plan) = parse_prune_plan(&text) else {
        let _ = tx
            .send(Msg::RuntimeText(
                "Memory consolidation: couldn't parse the model's plan; nothing changed."
                    .to_string(),
            ))
            .await;
        return;
    };
    if plan.prune.is_empty() {
        let reason = if plan.reason.is_empty() {
            String::new()
        } else {
            format!(" {}", plan.reason)
        };
        let _ = tx
            .send(Msg::RuntimeText(format!(
                "Memory consolidation: nothing to prune.{reason}"
            )))
            .await;
        return;
    }

    if let Err(report) = checkpoint_prune_targets(&workdir, &plan) {
        let _ = tx.send(Msg::RuntimeText(report)).await;
        return;
    }

    let mut pruned = Vec::new();
    for id in &plan.prune {
        if let Ok(Some(_)) = crate::app::memory::delete_memory(&workdir, id) {
            pruned.push(id.clone());
        }
    }

    let cfg = crate::app::load_project_scoped_config(&workdir).memory;
    let (loaded, _) = crate::app::memory::refresh(None, &workdir, &cfg);
    let _ = tx.send(Msg::MemoryChanged(loaded)).await;

    let _ = tx
        .send(Msg::RuntimeText(prune_report(&pruned, &plan.reason)))
        .await;
}

/// The one-line outcome of a prune, naming what went and why.
fn prune_report(pruned: &[String], reason: &str) -> String {
    if pruned.is_empty() {
        return "Memory consolidation: the model named facts to prune, but none matched existing memories."
            .to_string();
    }
    format!(
        "Consolidated memory — pruned {} fact{}: {}.{} Recoverable from the latest checkpoint (/checkpoints, /restore).",
        pruned.len(),
        if pruned.len() == 1 { "" } else { "s" },
        pruned.join(", "),
        if reason.is_empty() {
            String::new()
        } else {
            format!(" Reason: {reason}.")
        },
    )
}
