//! Jittered backoff, shared by the one retry ladder.
//!
//! This file used to hold a second, weaker ladder: `retry_async` (zero
//! callers) and `retry_async_if` (one caller, `web_client`, using it at the
//! wrong scope — the whole request pipeline was inside the retry closure, so a
//! JSON parse failure cost three attempts and every attempt re-acquired the
//! download permits). Both are gone; `models::retry::retry_transient_http` is
//! the single ladder now, and `jitter` is what it shares with nothing else.

/// Apply ±20% jitter to `delay_ms` using real entropy so concurrent clients —
/// and processes restarting at the same time — don't retry in lockstep (a
/// thundering herd). `pub(crate)` so the effect-layer retry middleware shares
/// this single impl rather than duplicating a weaker clock-based one (#87).
#[must_use]
pub fn jitter(delay_ms: u64) -> u64 {
    let span = delay_ms / 5;
    if span == 0 {
        return delay_ms;
    }
    let mut bytes = [0u8; 8];
    let entropy = match getrandom::fill(&mut bytes) {
        Ok(()) => u64::from_le_bytes(bytes),
        // getrandom shouldn't fail on supported targets; degrade to the
        // unjittered delay rather than panic.
        Err(_) => return delay_ms,
    };
    let offset = entropy % (2 * span + 1);
    delay_ms - span + offset
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_band() {
        // ±20% band: jitter(1000) ∈ [800, 1200].
        for _ in 0..100 {
            let j = jitter(1000);
            assert!((800..=1200).contains(&j), "jitter out of band: {j}");
        }
        // Tiny delays (span 0) pass through unchanged.
        assert_eq!(jitter(3), 3);
    }
}
