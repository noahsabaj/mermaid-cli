//! Background config watcher (#45).
//!
//! Polls `MERMAID.md` and the memory directory on a fixed interval and emits
//! [`Msg::InstructionsChanged`] / [`Msg::MemoryChanged`] whenever they change,
//! so the reducer reads fresh instructions/memory as injected data and never
//! does the I/O itself. `update(State, Msg)` therefore stays pure.
//!
//! This is the live-loop counterpart to the `state.now` clock injection: a
//! replay driver feeds the recorded `*Changed` Msgs instead of polling, so
//! folding a `--record` log reconstructs State without re-statting the live
//! filesystem. The watcher must only run in the live loop.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::app::MemoryConfig;
use crate::app::instructions::{self, LoadedInstructions, ReloadOutcome};
use crate::app::memory::{self, LoadedMemory, MemoryReloadOutcome};
use crate::domain::Msg;

use super::MsgSender;

/// How often the watcher re-stats config. A single `MERMAID.md` stat is
/// microseconds and only reads on an mtime change; the memory refresh re-reads
/// its (small) directory. 250ms keeps both negligible while making an edit
/// effective well before the user submits.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// One poll. Returns the refreshed values (carried as the next prior) plus any
/// `*Changed` Msgs to emit. Channel-free so it's unit-testable against a temp
/// dir; the watcher loop owns the send.
fn poll_config(
    instr: Option<LoadedInstructions>,
    mem: Option<LoadedMemory>,
    cwd: &Path,
    mem_cfg: &MemoryConfig,
) -> (Option<LoadedInstructions>, Option<LoadedMemory>, Vec<Msg>) {
    let mut msgs = Vec::new();
    let (instr, instr_outcome) = instructions::refresh(instr, cwd);
    if instr_outcome != ReloadOutcome::Unchanged {
        msgs.push(Msg::InstructionsChanged(instr.clone()));
    }
    let (mem, mem_outcome) = memory::refresh(mem, cwd, mem_cfg);
    if mem_outcome != MemoryReloadOutcome::Unchanged {
        msgs.push(Msg::MemoryChanged(mem.clone()));
    }
    (instr, mem, msgs)
}

/// The watcher task. Spawned detached at startup; runs until the Msg channel
/// closes at shutdown. tokio's interval fires its first tick immediately, so
/// the initial load lands before the user can submit.
pub(super) async fn config_watcher(tx: MsgSender, cwd: PathBuf, mem_cfg: MemoryConfig) {
    let mut instr = None;
    let mut mem = None;
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;
        let (next_instr, next_mem, msgs) = poll_config(instr, mem, &cwd, &mem_cfg);
        instr = next_instr;
        mem = next_mem;
        for msg in msgs {
            if tx.send(msg).await.is_err() {
                return; // channel closed — shutting down
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mermaid-watch-{}-{}-{}",
            tag,
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn poll_emits_on_first_load_then_silent_when_unchanged() {
        let dir = unique_dir("load");
        std::fs::write(dir.join("MERMAID.md"), "# initial instructions\n").unwrap();
        let cfg = MemoryConfig::default();

        // First poll loads MERMAID.md → InstructionsChanged(Some).
        let (instr, mem, msgs) = poll_config(None, None, &dir, &cfg);
        assert!(
            msgs.iter()
                .any(|m| matches!(m, Msg::InstructionsChanged(Some(_)))),
            "first poll with MERMAID.md present should emit InstructionsChanged(Some)"
        );

        // A second, unchanged poll is silent — no churn, no recording bloat.
        let (_instr, _mem, msgs) = poll_config(instr, mem, &dir, &cfg);
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, Msg::InstructionsChanged(_))),
            "an unchanged poll must not re-emit InstructionsChanged"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_is_silent_when_no_instructions_present() {
        let dir = unique_dir("empty");
        let cfg = MemoryConfig::default();
        let (_instr, _mem, msgs) = poll_config(None, None, &dir, &cfg);
        assert!(
            !msgs
                .iter()
                .any(|m| matches!(m, Msg::InstructionsChanged(_))),
            "no MERMAID.md → no InstructionsChanged (outcome is Unchanged)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
