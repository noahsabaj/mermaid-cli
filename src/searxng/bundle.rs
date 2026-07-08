//! Provisions the sovereign SearXNG bundle so `web_search` needs no container
//! runtime. The bundle — a portable CPython + a venv with Granian + the SearXNG
//! source tree — is published per-platform by the `mermaid-searxng` repo as a
//! checksummed release asset. On the first managed `web_search`, [`ensure_bundle`]
//! downloads the bundle for this platform, verifies it against `SHA256SUMS`
//! (the same anchored flow as `install.sh`, done in-process), and unpacks it
//! under the data dir. Subsequent runs reuse the unpacked tree until the pinned
//! [`SEARXNG_BUNDLE_VERSION`] changes.
//!
//! Callers serialize through the `SearxngManager` mutex, so this runs exactly
//! once per process even under concurrent first-searches.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

/// The `mermaid-searxng` release this build pins. The unpacked bundle carries a
/// `.version` marker; a mismatch triggers a re-download. Bumped by the
/// CI-automated PR that a `mermaid-searxng` release opens against this repo, so
/// bundle updates ride the normal `mermaid update` channel.
//
// TODO: real pin lands with the first `mermaid-searxng` release (plan Part B).
pub const SEARXNG_BUNDLE_VERSION: &str = "v0.1.0";

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
    let target = target_triple().ok_or_else(|| {
        anyhow!(
            "no sovereign SearXNG bundle is available for this platform ({}/{}). \
             Set OLLAMA_API_KEY, or point `[web] searxng_url` at your own instance.",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    provision(&asset_name(target), &runtime).await?;
    Ok(runtime)
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
    marker.trim() == SEARXNG_BUNDLE_VERSION
}

/// Map the compile target to the release-asset triple. `None` on a platform with
/// no published bundle (the caller falls back to `searxng_url` / Ollama).
fn target_triple() -> Option<&'static str> {
    triple_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// Pure OS/arch → asset-triple mapping. Kept separate from [`target_triple`] so
/// every supported combination is unit-testable off-host. Triples match the
/// `mermaid-searxng` release matrix (the same strings as mermaid-cli's own).
fn triple_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        ("macos", "aarch64") => "macos-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        _ => return None,
    })
}

/// Asset file name for a triple: `.tar.zst` on unix, `.zip` on Windows.
fn asset_name(target: &str) -> String {
    let ext = if cfg!(windows) { "zip" } else { "tar.zst" };
    format!("mermaid-searxng-{target}.{ext}")
}

/// Download, verify, and unpack `asset` into `runtime`, replacing any prior tree
/// atomically.
async fn provision(asset: &str, runtime: &Path) -> Result<()> {
    // A visible, non-blocking notice: the bundle is a data payload (not remote
    // code like `mermaid update`), and the container path pulled its image
    // silently too — a one-line heads-up is the right middle ground.
    tracing::info!("fetching the local search engine ({asset}) — first run only");

    let base =
        format!("https://github.com/{BUNDLE_REPO}/releases/download/{SEARXNG_BUNDLE_VERSION}");
    let staging = crate::utils::private_temp_dir()
        .context("creating the private staging dir for the SearXNG bundle")?
        .join("searxng-download");
    // Start from a clean staging dir so a prior partial download can't leak in.
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    // Small manifest first, then the large asset streamed to disk while hashing.
    let sums = http_get_text(&format!("{base}/SHA256SUMS")).await?;
    let want = expected_sha256(&sums, asset)
        .ok_or_else(|| anyhow!("no checksum listed for {asset} in SHA256SUMS"))?
        .to_owned();

    let archive = staging.join(asset);
    let got = download_hashing(&format!("{base}/{asset}"), &archive).await?;
    if !got.eq_ignore_ascii_case(&want) {
        return Err(anyhow!(
            "checksum mismatch for {asset}\n  expected: {want}\n  actual:   {got}"
        ));
    }

    // Unpack beside the final runtime (same filesystem) so the swap-in is an
    // atomic rename, never a half-populated live dir.
    let incoming = runtime.with_file_name(".runtime-incoming");
    let _ = std::fs::remove_dir_all(&incoming);
    std::fs::create_dir_all(&incoming)?;
    extract(&archive, &incoming).with_context(|| format!("unpacking {asset}"))?;
    std::fs::write(incoming.join(".version"), SEARXNG_BUNDLE_VERSION)?;
    swap_into_place(&incoming, runtime)?;

    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
}

/// Extract the expected sha256 for `asset` from a `SHA256SUMS` body. Matches the
/// asset only as the whole final field — the anchored equivalent of install.sh's
/// `grep " ${asset}$"` — so `mermaid-searxng-linux-x86_64` can never accidentally
/// match against `...-x86_64-something`. Tolerates the one/two-space and binary
/// (`*name`) forms `sha256sum`/`shasum` emit.
fn expected_sha256<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?.trim_start_matches('*');
        // Reject lines with trailing junk fields; a valid entry is exactly two.
        if parts.next().is_some() {
            return None;
        }
        (name == asset).then_some(hash)
    })
}

/// GET a small text resource (the `SHA256SUMS` manifest).
async fn http_get_text(url: &str) -> Result<String> {
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("fetching {url}"))?;
    Ok(resp.text().await?)
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
    // Remove a stale/partial prior runtime, then move the new tree into place.
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
fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let file = std::fs::File::open(archive)?;
    zip::ZipArchive::new(file)
        .context("opening the .zip bundle")?
        .extract(dest)
        .context("unpacking the .zip bundle")
}

/// Lowercase-hex a byte slice, matching the `SHA256SUMS` digest format (mirrors
/// the idiom in `searxng::random_secret`).
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn triple_covers_every_release_target() {
        assert_eq!(triple_for("linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(triple_for("linux", "aarch64"), Some("linux-aarch64"));
        assert_eq!(triple_for("macos", "x86_64"), Some("macos-x86_64"));
        assert_eq!(triple_for("macos", "aarch64"), Some("macos-aarch64"));
        assert_eq!(triple_for("windows", "x86_64"), Some("windows-x86_64"));
    }

    #[test]
    fn triple_is_none_for_unsupported_platforms() {
        assert_eq!(triple_for("windows", "aarch64"), None); // no windows-arm64 asset
        assert_eq!(triple_for("freebsd", "x86_64"), None);
        assert_eq!(triple_for("linux", "riscv64"), None);
    }

    #[test]
    fn asset_name_has_the_platform_extension() {
        let name = asset_name("linux-x86_64");
        assert!(name.starts_with("mermaid-searxng-linux-x86_64."));
        if cfg!(windows) {
            assert!(name.ends_with(".zip"), "{name}");
        } else {
            assert!(name.ends_with(".tar.zst"), "{name}");
        }
    }

    #[test]
    fn the_current_host_maps_to_a_bundle() {
        // CI runs on the five supported targets; each must resolve.
        assert!(target_triple().is_some(), "unmapped host build target");
    }

    #[test]
    fn expected_sha256_matches_the_whole_asset_field() {
        let asset = "mermaid-searxng-linux-x86_64.tar.zst";
        let sums = format!(
            "1111111111111111111111111111111111111111111111111111111111111111  other.tar.zst\n\
             abc0000000000000000000000000000000000000000000000000000000000def  {asset}\n\
             2222222222222222222222222222222222222222222222222222222222222222  mermaid-searxng-linux-aarch64.tar.zst\n"
        );
        assert_eq!(
            expected_sha256(&sums, asset),
            Some("abc0000000000000000000000000000000000000000000000000000000000def")
        );
    }

    #[test]
    fn expected_sha256_never_matches_a_substring_or_prefix() {
        // A different asset whose name has ours as a prefix must not match.
        let sums = "9999999999999999999999999999999999999999999999999999999999999999  mermaid-searxng-linux-x86_64.tar.zst.bak\n";
        assert_eq!(
            expected_sha256(sums, "mermaid-searxng-linux-x86_64.tar.zst"),
            None
        );
    }

    #[test]
    fn expected_sha256_tolerates_binary_marker_and_is_absent_when_unlisted() {
        let asset = "mermaid-searxng-macos-aarch64.zip";
        let sums = format!("deadbeef00000000000000000000000000000000000000000000000000000000 *{asset}\n");
        assert_eq!(
            expected_sha256(&sums, asset),
            Some("deadbeef00000000000000000000000000000000000000000000000000000000")
        );
        assert_eq!(expected_sha256(&sums, "not-listed.zip"), None);
    }

    #[test]
    fn version_marker_compare_trims_whitespace() {
        assert!(version_is_current(SEARXNG_BUNDLE_VERSION));
        assert!(version_is_current(&format!("  {SEARXNG_BUNDLE_VERSION}\n")));
        assert!(!version_is_current("v0.0.1"));
        assert!(!version_is_current(""));
    }

    #[test]
    fn hex_lower_is_zero_padded_lowercase() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
