//! The pinned mermaid-searxng bundle: release version + per-target sha256.
//!
//! These values are the trust anchor for the download in [`super::bundle`]: the
//! fetched bundle is verified against the sha256 compiled in here, so a tampered
//! or substituted release asset is rejected even if the release's own
//! `SHA256SUMS` is forged. They are bumped by the mermaid-searxng release CI via
//! `scripts/bump_searxng_bundle.py` (an automated PR, reviewed and merged like
//! any other change) — do not hand-edit the values.

/// The pinned `mermaid-searxng` release tag.
pub const BUNDLE_VERSION: &str = "v0.3.0";

/// sha256 (lowercase hex) of `mermaid-searxng-<target>.tar.zst` for
/// [`BUNDLE_VERSION`], or `None` on a platform with no published bundle
/// (Windows: SearXNG needs Unix-only modules).
pub fn bundle_sha256(target: &str) -> Option<&'static str> {
    Some(match target {
        "linux-x86_64" => "1d315ab5235cac5182021585cea6567ebc6e90e6c39474e29004974044e06822",
        "linux-aarch64" => "d5a982de009b534635a435cf06e0f09c9f1dafcb9d14bd391f18b1efb7823cb7",
        "macos-aarch64" => "332bb48e3529cdb5ce99faa59460364710a237ac3c3a0b10182c6323ce18d3a0",
        // Placeholder slot until the first release that publishes macos-x86_64
        // (mermaid-searxng v0.3.0): all-zeros matches no real digest, so a
        // fetch fails closed until the automated bump PR fills it in.
        "macos-x86_64" => "12b236f0072649793da41a31ea14be30afd2d014598f24f17ab502908374af02",
        _ => return None,
    })
}
