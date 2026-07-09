//! The pinned mermaid-searxng bundle: release version + per-target sha256.
//!
//! These values are the trust anchor for the download in [`super::bundle`]: the
//! fetched bundle is verified against the sha256 compiled in here, so a tampered
//! or substituted release asset is rejected even if the release's own
//! `SHA256SUMS` is forged. They are bumped by the mermaid-searxng release CI via
//! `scripts/bump_searxng_bundle.py` (an automated PR, reviewed and merged like
//! any other change) — do not hand-edit the values.

/// The pinned `mermaid-searxng` release tag.
pub const BUNDLE_VERSION: &str = "v0.1.0";

/// sha256 (lowercase hex) of `mermaid-searxng-<target>.tar.zst` for
/// [`BUNDLE_VERSION`], or `None` on a platform with no published bundle
/// (Windows: SearXNG needs Unix-only modules; Intel macOS: CI runner scarcity).
pub fn bundle_sha256(target: &str) -> Option<&'static str> {
    Some(match target {
        "linux-x86_64" => "412415fbbf3d5111bad14c3ae373b89ffb184ee69431d6fa55504da307846ac3",
        "linux-aarch64" => "449c6680b5ddbe880e6aec3e9295428461661dc9fd2c395c13f5ecd28bf07296",
        "macos-aarch64" => "c16316a2a2e858fce0427f2bf56a53cc7d717f92a2ea0422d370babf2df73727",
        _ => return None,
    })
}
