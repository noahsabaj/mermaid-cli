//! Provisions the sovereign SearXNG bundle so `web_search` needs no container
//! runtime. The bundle — a portable CPython + Granian + the SearXNG app — is
//! published per-platform by the `mermaid-searxng` repo as a checksummed release
//! asset. On the first managed `web_search`, [`ensure_bundle`] downloads the
//! bundle for this platform, verifies it against the sha256 pinned in
//! [`super::bundle_manifest`] (the trust anchor — a tampered release asset is
//! rejected), and unpacks it under the data dir. Subsequent runs reuse the
//! unpacked tree until the pinned version changes.
//!
//! Callers serialize through the `SearxngManager` mutex, so this runs exactly
//! once per process even under concurrent first-searches.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use super::bundle_manifest;

/// GitHub repo publishing the bundles.
const BUNDLE_REPO: &str = "noahsabaj/mermaid-searxng";

/// Ensure the bundle for this platform is present and current under the data
/// dir, returning the unpacked runtime root. Downloads + sha256-verifies +
/// unpacks on a version miss; otherwise returns immediately.
pub async fn ensure_bundle() -> Result<PathBuf> {
    let runtime = runtime_dir()?;
    if version_is_current(&marker_contents(&runtime)) {
        return Ok(runtime);
    }
    let target = target_triple().ok_or_else(unsupported_platform)?;
    let expected_sha = bundle_manifest::bundle_sha256(target).ok_or_else(unsupported_platform)?;
    provision(target, expected_sha, &runtime).await?;
    Ok(runtime)
}

fn unsupported_platform() -> anyhow::Error {
    anyhow!(
        "no sovereign SearXNG bundle is available for this platform ({}/{}). \
         Set OLLAMA_API_KEY, or point `[web] searxng_url` at your own instance.",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

/// Where the unpacked bundle lives: `<data_dir>/searxng/runtime`. Sits under the
/// data dir (durable state), not the config dir.
fn runtime_dir() -> Result<PathBuf> {
    Ok(crate::runtime::data_dir()?.join("searxng").join("runtime"))
}

/// Read the unpacked bundle's `.version` marker, or "" if absent/unreadable.
fn marker_contents(runtime: &Path) -> String {
    std::fs::read_to_string(runtime.join(".version")).unwrap_or_default()
}

/// Whether an unpacked `.version` marker matches the pinned bundle version.
fn version_is_current(marker: &str) -> bool {
    marker.trim() == bundle_manifest::BUNDLE_VERSION
}

/// Map the compile target to the release-asset triple. `None` on a platform with
/// no published bundle (the caller falls back to `searxng_url` / Ollama).
fn target_triple() -> Option<&'static str> {
    triple_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Pure OS/arch → asset-triple mapping, unit-testable off-host. Covers exactly
/// the targets the `mermaid-searxng` release publishes; Windows (SearXNG needs
/// Unix-only modules) and Intel macOS (CI runner scarcity) are unsupported.
fn triple_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "aarch64") => "macos-aarch64",
        _ => return None,
    })
}

/// Asset file name for a triple (every published bundle is a `.tar.zst`).
fn asset_name(target: &str) -> String {
    format!("mermaid-searxng-{target}.tar.zst")
}

/// Download the bundle for `target`, verify it against the pinned `expected_sha`,
/// and unpack it into `runtime`, replacing any prior tree atomically.
async fn provision(target: &str, expected_sha: &str, runtime: &Path) -> Result<()> {
    let asset = asset_name(target);
    // A visible, non-blocking notice: the bundle is a data payload, and the
    // container path pulled its image silently too — a one-line heads-up is the
    // right middle ground.
    tracing::info!("fetching the local search engine ({asset}) — first run only");

    let url = format!(
        "https://github.com/{BUNDLE_REPO}/releases/download/{}/{asset}",
        bundle_manifest::BUNDLE_VERSION
    );
    let staging = crate::utils::private_temp_dir()
        .context("creating the private staging dir for the SearXNG bundle")?
        .join("searxng-download");
    // Start from a clean staging dir so a prior partial download can't leak in.
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    // Stream the asset to disk while hashing, then compare against the compiled-in
    // pin — the release's own SHA256SUMS is never trusted.
    let archive = staging.join(&asset);
    let got = download_hashing(&url, &archive).await?;
    if !got.eq_ignore_ascii_case(expected_sha) {
        return Err(anyhow!(
            "checksum mismatch for {asset}\n  expected: {expected_sha}\n  actual:   {got}"
        ));
    }

    // Unpack beside the final runtime (same filesystem) so the swap-in is an
    // atomic rename, never a half-populated live dir.
    let incoming = runtime.with_file_name(".runtime-incoming");
    let _ = std::fs::remove_dir_all(&incoming);
    std::fs::create_dir_all(&incoming)?;
    extract(&archive, &incoming).with_context(|| format!("unpacking {asset}"))?;
    std::fs::write(incoming.join(".version"), bundle_manifest::BUNDLE_VERSION)?;
    swap_into_place(&incoming, runtime)?;

    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

/// Stream `url` to `dest` while computing its sha256 in one pass, returning the
/// lowercase-hex digest. Streaming (rather than buffering the whole bundle in
/// memory) keeps a multi-hundred-MB download bounded.
async fn download_hashing(url: &str, dest: &Path) -> Result<String> {
    use futures::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the bundle stream")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    Ok(hex_lower(&hasher.finalize()))
}

/// Atomically replace `to` with the freshly-unpacked tree at `from`. `from` and
/// `to` share a parent (both under `<data_dir>/searxng`), so the rename stays on
/// one filesystem.
fn swap_into_place(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if to.exists() {
        std::fs::remove_dir_all(to)
            .with_context(|| format!("removing the previous bundle at {}", to.display()))?;
    }
    std::fs::rename(from, to)
        .with_context(|| format!("moving the unpacked bundle into {}", to.display()))
}

#[cfg(unix)]
fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive)?;
    let decoder = ruzstd::decoding::StreamingDecoder::new(file)
        .map_err(|e| anyhow!("initializing the zstd decoder: {e}"))?;
    tar::Archive::new(decoder)
        .unpack(dest)
        .context("unpacking the .tar.zst bundle")
}

#[cfg(windows)]
fn extract(_archive: &Path, _dest: &Path) -> Result<()> {
    // No Windows bundle is published (SearXNG imports Unix-only modules like
    // `pwd`); ensure_bundle returns the unsupported-platform error long before
    // this is reachable. Present only so the crate compiles on Windows.
    Err(anyhow!("SearXNG bundles are not published for Windows"))
}

/// Lowercase-hex a byte slice, matching the pinned digest format (mirrors the
/// idiom in `searxng::random_secret`).
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_covers_the_published_targets() {
        assert_eq!(triple_for("linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(triple_for("linux", "aarch64"), Some("linux-aarch64"));
        assert_eq!(triple_for("macos", "aarch64"), Some("macos-aarch64"));
    }

    #[test]
    fn triple_is_none_for_unpublished_platforms() {
        assert_eq!(triple_for("windows", "x86_64"), None); // SearXNG needs `pwd`
        assert_eq!(triple_for("macos", "x86_64"), None); // Intel-mac runner scarcity
        assert_eq!(triple_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn every_published_triple_has_a_pinned_checksum() {
        // The platform list and the checksum table must not drift apart.
        for t in ["linux-x86_64", "linux-aarch64", "macos-aarch64"] {
            let sha = bundle_manifest::bundle_sha256(t).unwrap_or_else(|| panic!("no sha for {t}"));
            assert_eq!(sha.len(), 64, "{t} sha is not 64 hex chars");
            assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "{t} sha not hex");
        }
    }

    #[test]
    fn the_current_host_target_is_pinned_when_supported() {
        if let Some(t) = target_triple() {
            assert!(
                bundle_manifest::bundle_sha256(t).is_some(),
                "published target {t} has no pinned checksum"
            );
        }
    }

    #[test]
    fn asset_name_is_tar_zst() {
        assert_eq!(
            asset_name("linux-x86_64"),
            "mermaid-searxng-linux-x86_64.tar.zst"
        );
    }

    #[test]
    fn version_marker_compare_trims_whitespace() {
        assert!(version_is_current(bundle_manifest::BUNDLE_VERSION));
        assert!(version_is_current(&format!(
            "  {}\n",
            bundle_manifest::BUNDLE_VERSION
        )));
        assert!(!version_is_current("v0.0.1"));
        assert!(!version_is_current(""));
    }

    #[test]
    fn hex_lower_is_zero_padded_lowercase() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
