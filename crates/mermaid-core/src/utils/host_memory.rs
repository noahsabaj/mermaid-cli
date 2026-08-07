//! Best-effort host memory detection for Ollama auto-sizing.
//!
//! Mermaid sizes an Ollama model's `num_ctx` to fit the host's memory so the
//! model stays on the GPU (CPU/RAM offload is 5–20× slower, so it's off by
//! default). That needs two numbers: total system RAM (the budget when the user
//! opts into RAM offload) and total VRAM (the default budget).
//!
//! Both are **best-effort and cached** — VRAM detection is GPU-vendor specific
//! and there is no Ollama API for it, so we shell out to `nvidia-smi` and fall
//! back gracefully. When VRAM can't be detected the sizing logic uses a
//! conservative fixed cap (see [`crate::models::adapters::ollama_sizing`]), so a
//! `None` here never breaks anything.

use crate::constants::NVIDIA_SMI_TIMEOUT_SECS;
use std::sync::OnceLock;

/// Total physical system RAM in bytes, or `None` if it can't be read. Cached for
/// the process (RAM doesn't change during a session).
pub fn system_ram_bytes() -> Option<u64> {
    static CACHE: OnceLock<Option<u64>> = OnceLock::new();
    *CACHE.get_or_init(detect_system_ram)
}

fn detect_system_ram() -> Option<u64> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let total = sys.total_memory(); // bytes (sysinfo ≥ 0.30)
    (total > 0).then_some(total)
}

/// Total GPU VRAM in bytes (best-effort), or `None` if no GPU is detected.
///
/// On unified-memory Macs there is no separate, slow "RAM offload" target, so
/// system RAM *is* the GPU budget. On NVIDIA we read `nvidia-smi`. Everything
/// else (AMD, integrated, headless) returns `None` and the caller uses the
/// conservative fallback cap. Cached for the process.
pub async fn gpu_vram_bytes() -> Option<u64> {
    static CACHE: tokio::sync::OnceCell<Option<u64>> = tokio::sync::OnceCell::const_new();
    *CACHE.get_or_init(detect_vram).await
}

async fn detect_vram() -> Option<u64> {
    if let Some(v) = nvidia_vram_bytes().await {
        return Some(v);
    }
    // Apple Silicon / Intel Macs: unified memory — the GPU draws from system RAM,
    // and there's no slow separate pool to "offload" to, so treat RAM as VRAM.
    if cfg!(target_os = "macos") {
        return system_ram_bytes();
    }
    None
}

/// Largest single NVIDIA GPU's total VRAM via `nvidia-smi`, in bytes. A model
/// loads onto one GPU unless it spans, so we take the max single GPU rather than
/// assume the pool. Best-effort: absent binary, non-zero exit, or timeout → `None`.
async fn nvidia_vram_bytes() -> Option<u64> {
    let mut cmd = tokio::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"]);
    cmd.kill_on_drop(true);

    let out = tokio::time::timeout(
        std::time::Duration::from_secs(NVIDIA_SMI_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .ok()? // timed out
    .ok()?; // failed to spawn (nvidia-smi not installed)

    if !out.status.success() {
        return None;
    }

    // One line per GPU, in MiB (`nounits`).
    let max_mib = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u64>().ok())
        .max()?;
    max_mib.checked_mul(1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_ram_is_detected_and_plausible() {
        // Any real machine (including CI runners) reports > 0 RAM, and a sane
        // total is at least ~256 MiB — this also catches a units regression
        // (KiB vs bytes) in the underlying crate.
        let ram = system_ram_bytes().expect("system RAM should be detectable");
        assert!(
            ram > 256 * 1024 * 1024,
            "implausibly small RAM ({ram} bytes) — units regression?"
        );
    }

    #[tokio::test]
    async fn gpu_vram_probe_is_best_effort() {
        // Some on a GPU box, None on CI — must not panic or hang either way.
        let _ = gpu_vram_bytes().await;
    }
}
