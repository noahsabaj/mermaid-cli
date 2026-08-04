//! Provisions the sovereign SearXNG bundle so `web_search` needs no container
//! runtime. The bundle — a portable CPython + Granian + the SearXNG app — is
//! published per-platform by the `mermaid-searxng` repo as a checksummed release
//! asset. On the first managed `web_search`, [`ensure_bundle`] downloads the
//! bundle for this platform, verifies it against the sha256 pinned in
//! [`super::bundle_manifest`] (the trust anchor — a tampered release asset is
//! rejected), and unpacks it under the data dir. Subsequent runs reuse the
//! unpacked tree until the pinned version changes.
//!
//! The `SearxngManager` serializes first-searches within one process and a
//! filesystem lock serializes provisioning across Mermaid processes.

use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};

use super::bundle_manifest;

/// GitHub repo publishing the bundles.
const BUNDLE_REPO: &str = "noahsabaj/mermaid-searxng";

/// A normal bundle is currently 65-80 MiB. Leave headroom for interpreter and
/// dependency growth while refusing an unexpectedly large response before it
/// can fill the data volume.
const MAX_BUNDLE_BYTES: u64 = 128 * 1024 * 1024;
/// The published runtime is substantially smaller than this after unpacking.
/// Keep enough headroom for CPython and wheels while bounding decompression
/// bombs and accidental oversized releases.
#[cfg(any(unix, test))]
const MAX_UNPACKED_BYTES: u64 = 1024 * 1024 * 1024;
#[cfg(any(unix, test))]
const MAX_ARCHIVE_ENTRIES: u64 = 200_000;
#[cfg(any(unix, test))]
const MAX_ARCHIVE_PATH_BYTES: usize = 4096;
const MAX_POINTER_BYTES: u64 = 256;
/// Without cross-process leases there is no safe way to identify which old
/// generation a live Mermaid process may still be importing from. Bound disk
/// growth by refusing a fifth generation rather than deleting a possibly-live
/// tree. The error instructs the user how to reclaim space once Mermaid stops.
const MAX_GENERATION_DIRECTORIES: usize = 4;
/// Download staging directories normally disappear with their cleanup guard,
/// but a hard process crash bypasses destructors. Apply the same fail-closed
/// retention bound so repeated crashes cannot grow the private temp volume
/// without limit.
const MAX_DOWNLOAD_STAGING_DIRECTORIES: usize = MAX_GENERATION_DIRECTORIES;
#[cfg(any(unix, test))]
const EXTRACTION_HEARTBEAT_BYTES: u64 = 4 * 1024 * 1024;
#[cfg(any(unix, test))]
const EXTRACTION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(5 * 60);
// A crashed owner becomes reapable before a waiter gives up. Live owners renew
// their owner-file timestamp while extracting.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(16 * 60);
const STALE_LOCK_AGE: Duration = Duration::from_secs(15 * 60);
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(250);

static UNIQUE_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Ensure the bundle for this platform is present and current under the data
/// dir, returning the unpacked runtime root. Downloads + sha256-verifies +
/// unpacks on a version miss; otherwise returns immediately.
pub async fn ensure_bundle() -> Result<PathBuf> {
    let target = managed_backend_viability().map_err(anyhow::Error::msg)?;
    let expected_sha = bundle_manifest::bundle_sha256(target)
        .expect("managed_backend_viability verifies the manifest entry");
    let root = runtime_root()?;
    if let Some(runtime) = active_runtime(&root, expected_sha) {
        return Ok(runtime);
    }

    let lock = ProvisionLock::acquire(&root).await?;
    // Recheck under the interprocess lock. The process that held it before us
    // may have published a generation while we waited.
    if let Some(runtime) = active_runtime(&root, expected_sha) {
        return Ok(runtime);
    }
    provision(target, expected_sha, &root, lock).await
}

/// Report whether the managed backend is usable on this compile target and
/// return the pinned release-asset triple when it is. Capability resolution,
/// diagnostics, and tool registration should all consume this single answer.
pub fn managed_backend_viability() -> std::result::Result<&'static str, String> {
    viability_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn viability_for(os: &str, arch: &str) -> std::result::Result<&'static str, String> {
    let target = triple_for(os, arch).ok_or_else(|| unsupported_platform_message(os, arch))?;
    if bundle_manifest::bundle_sha256(target).is_none() {
        return Err(format!(
            "the managed SearXNG bundle manifest has no checksum for {target}"
        ));
    }
    Ok(target)
}

fn unsupported_platform_message(os: &str, arch: &str) -> String {
    format!(
        "no sovereign SearXNG bundle is available for this platform ({}/{}). \
         Configure `[web] search_backend = \"ollama\"` and OLLAMA_API_KEY, or \
         set `search_backend = \"searxng\"` and point `searxng_url` at your own instance.",
        os, arch
    )
}

fn runtime_root() -> Result<PathBuf> {
    Ok(crate::runtime::data_dir()?.join("searxng"))
}

fn versioned_runtime_name(version: &str) -> String {
    let safe = version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    format!("runtime-{safe}")
}

fn pointer_name(version: &str) -> String {
    format!("current-{}.ptr", versioned_runtime_name(version))
}

fn generation_prefix(version: &str, expected_sha: &str) -> String {
    let sha_prefix = expected_sha.get(..12).unwrap_or(expected_sha);
    format!("{}-{sha_prefix}", versioned_runtime_name(version))
}

fn pointer_path(root: &Path) -> PathBuf {
    root.join(pointer_name(bundle_manifest::BUNDLE_VERSION))
}

fn generations_dir(root: &Path) -> PathBuf {
    root.join("generations")
}

fn rejected_dir(root: &Path) -> PathBuf {
    root.join("rejected")
}

/// Read the unpacked bundle's `.version` marker, or "" if absent/unreadable.
fn marker_contents(runtime: &Path) -> String {
    read_bounded_text(&runtime.join(".version"), MAX_POINTER_BYTES).unwrap_or_default()
}

/// Whether an unpacked `.version` marker matches the pinned bundle version.
fn version_is_current(marker: &str) -> bool {
    marker.trim() == bundle_manifest::BUNDLE_VERSION
}

/// A marker alone is not sufficient: a cancelled cleanup, manual deletion, or
/// disk corruption can leave a version-looking tree that can never spawn. The
/// cheap structural check lets the next call repair that tree atomically.
fn runtime_is_current(runtime: &Path, expected_sha: &str) -> bool {
    let is_real_directory =
        std::fs::symlink_metadata(runtime).is_ok_and(|metadata| metadata.file_type().is_dir());
    is_real_directory
        && version_is_current(&marker_contents(runtime))
        && sha_is_current(runtime, expected_sha)
        && super::python_bin(runtime).is_file()
        && runtime.join("python").join("lib").is_dir()
}

fn sha_is_current(runtime: &Path, expected_sha: &str) -> bool {
    read_bounded_text(&runtime.join(".sha256"), MAX_POINTER_BYTES)
        .is_some_and(|sha| sha.trim().eq_ignore_ascii_case(expected_sha))
}

fn read_bounded_text(path: &Path, limit: u64) -> Option<String> {
    if !std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_file()) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (u64::try_from(bytes.len()).ok()? <= limit)
        .then(|| String::from_utf8(bytes).ok())
        .flatten()
}

fn active_runtime(root: &Path, expected_sha: &str) -> Option<PathBuf> {
    let pointer = read_bounded_text(&pointer_path(root), MAX_POINTER_BYTES)?;
    let name = pointer.strip_suffix('\n')?;
    if name.is_empty() || name.contains('\r') || name.contains('\n') {
        return None;
    }
    let name_path = Path::new(name);
    let mut components = name_path.components();
    let required_prefix = format!(
        "{}-",
        generation_prefix(bundle_manifest::BUNDLE_VERSION, expected_sha)
    );
    if !matches!(components.next(), Some(std::path::Component::Normal(_)))
        || components.next().is_some()
        || !name.starts_with(&required_prefix)
    {
        return None;
    }
    let runtime = generations_dir(root).join(name);
    (!runtime_is_rejected(root, &runtime) && runtime_is_current(&runtime, expected_sha))
        .then_some(runtime)
}

fn runtime_is_rejected(root: &Path, runtime: &Path) -> bool {
    runtime
        .file_name()
        .is_some_and(|name| rejected_dir(root).join(name).is_file())
}

/// Reject this generation for future starters without touching its immutable
/// files or the version-scoped pointer. Existing processes may continue using
/// it; the next starter resolves the external rejection marker and provisions a
/// fresh generation.
pub(super) fn invalidate_runtime(runtime: &Path) -> Result<()> {
    let generations = runtime
        .parent()
        .context("the SearXNG generation has no parent")?;
    let root = generations
        .parent()
        .context("the SearXNG generations directory has no parent")?;
    anyhow::ensure!(
        generations
            .file_name()
            .is_some_and(|name| name == "generations"),
        "refusing to reject a path outside the SearXNG generations directory"
    );
    let name = runtime
        .file_name()
        .context("the SearXNG generation has no file name")?;
    let rejected = rejected_dir(root);
    std::fs::create_dir_all(&rejected)?;
    let marker = rejected.join(name);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
    {
        Ok(mut file) => {
            file.write_all(b"rejected\n")?;
            file.sync_all()?;
        },
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {},
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

/// Pure OS/arch → asset-triple mapping, unit-testable off-host. Covers exactly
/// the targets the `mermaid-searxng` release publishes; Windows (SearXNG needs
/// Unix-only modules) is unsupported.
fn triple_for(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "aarch64") => "macos-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        _ => return None,
    })
}

/// Asset file name for a triple (every published bundle is a `.tar.zst`).
fn asset_name(target: &str) -> String {
    format!("mermaid-searxng-{target}.tar.zst")
}

/// Download the bundle, unpack it into a new immutable generation, and atomically
/// select that generation for future starters.
async fn provision(
    target: &str,
    expected_sha: &str,
    root: &Path,
    lock: ProvisionLock,
) -> Result<PathBuf> {
    let asset = asset_name(target);
    // A visible, non-blocking notice: the bundle is a data payload, and the
    // container path pulled its image silently too — a one-line heads-up is the
    // right middle ground.
    tracing::info!("fetching the local search engine ({asset}) — first run only");

    let url = format!(
        "https://github.com/{BUNDLE_REPO}/releases/download/{}/{asset}",
        bundle_manifest::BUNDLE_VERSION
    );
    let staging_root = crate::utils::private_temp_dir()
        .context("creating the private staging dir for the SearXNG bundle")?;
    ensure_provisioning_capacity(root, &staging_root)?;
    let (staging, staging_cleanup) = create_unique_dir(&staging_root, "searxng-download")?;

    // Stream the asset to disk while hashing, then compare against the compiled-in
    // pin — the release's own SHA256SUMS is never trusted.
    let archive = staging.join(&asset);
    let got = download_hashing(&url, &archive).await?;
    if !got.eq_ignore_ascii_case(expected_sha) {
        return Err(anyhow!(
            "checksum mismatch for {asset}\n  expected: {expected_sha}\n  actual:   {got}"
        ));
    }

    // Extract into an unreachable incoming directory. Publication first moves
    // it to a unique immutable generation name and only then switches the small
    // version-scoped pointer.
    let generations = generations_dir(root);
    let (incoming, incoming_cleanup) = create_unique_dir(&generations, ".runtime-incoming")?;
    let root = root.to_path_buf();
    let expected_sha = expected_sha.to_string();
    let asset_for_error = asset.clone();
    let cancellation = Arc::new(AtomicBool::new(false));
    let worker_cancellation = Arc::clone(&cancellation);
    let mut cancel_on_drop = CancelOnDrop::new(cancellation);

    // Decompression and filesystem publication are blocking work. Move every
    // ownership guard into the blocking task: if the async caller is cancelled,
    // the worker observes the signal while streaming, while the interprocess
    // lock and both cleanup guards remain alive until it stops and cleans up.
    let worker = tokio::task::spawn_blocking(move || -> Result<PathBuf> {
        let _staging_cleanup = staging_cleanup;
        let mut incoming_cleanup = incoming_cleanup;
        extract(&archive, &incoming, &lock, Arc::clone(&worker_cancellation))
            .with_context(|| format!("unpacking {asset_for_error}"))?;
        anyhow::ensure!(
            !worker_cancellation.load(Ordering::Acquire),
            "SearXNG extraction was cancelled"
        );
        // Claim the lock transition once and retain it through every operation
        // that makes this generation observable. Heartbeats, release, and stale
        // reaping all use the same gate and therefore cannot interleave here.
        let publication_claim = lock.claim_for_publication()?;
        let generation =
            complete_generation_under_claim(&incoming, &root, &expected_sha, &publication_claim)?;
        incoming_cleanup.disarm();
        drop(publication_claim);
        Ok(generation)
    });
    let joined = worker.await;
    cancel_on_drop.disarm();
    joined.map_err(|error| anyhow!("the SearXNG extraction task failed: {error}"))?
}

/// Stream `url` to `dest` while computing its sha256 in one pass, returning the
/// lowercase-hex digest. Streaming (rather than buffering the whole bundle in
/// memory) keeps a multi-hundred-MB download bounded.
async fn download_hashing(url: &str, dest: &Path) -> Result<String> {
    tokio::time::timeout(DOWNLOAD_TIMEOUT, download_hashing_inner(url, dest))
        .await
        .map_err(|_| {
            anyhow!(
                "downloading the SearXNG bundle exceeded {} seconds",
                DOWNLOAD_TIMEOUT.as_secs()
            )
        })?
}

async fn download_hashing_inner(url: &str, dest: &Path) -> Result<String> {
    use futures::StreamExt;
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(DOWNLOAD_TIMEOUT)
        .redirect(reqwest::redirect::Policy::limited(5))
        .https_only(true)
        .user_agent("mermaid-cli-searxng-provisioner")
        .build()
        .context("building the SearXNG bundle download client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;

    if let Some(advertised) = resp.content_length() {
        ensure_download_size(advertised)?;
    }

    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut stream = resp.bytes_stream();
    let mut total = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the bundle stream")?;
        total = total
            .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| anyhow!("the SearXNG bundle size overflowed"))?;
        ensure_download_size(total)?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    Ok(hex_lower(&hasher.finalize()))
}

fn ensure_download_size(bytes: u64) -> Result<()> {
    anyhow::ensure!(
        bytes <= MAX_BUNDLE_BYTES,
        "the SearXNG bundle exceeded the {} MiB download limit",
        MAX_BUNDLE_BYTES / (1024 * 1024)
    );
    Ok(())
}

fn ensure_generation_capacity(root: &Path) -> Result<()> {
    let generations = generations_dir(root);
    let count = count_managed_directories(
        &generations,
        &["runtime-", ".runtime-incoming-"],
        "runtime generation",
    )?;
    anyhow::ensure!(
        count < MAX_GENERATION_DIRECTORIES,
        "managed SearXNG retained {count} runtime generations, reaching the safe limit of \
         {MAX_GENERATION_DIRECTORIES}; stop all Mermaid processes, remove unused directories \
         under {}, and retry",
        generations.display()
    );
    Ok(())
}

fn ensure_download_staging_capacity(staging_root: &Path) -> Result<()> {
    let count =
        count_managed_directories(staging_root, &["searxng-download-"], "download staging")?;
    anyhow::ensure!(
        count < MAX_DOWNLOAD_STAGING_DIRECTORIES,
        "managed SearXNG retained {count} download staging directories, reaching the safe limit \
         of {MAX_DOWNLOAD_STAGING_DIRECTORIES}; stop all Mermaid processes, remove unused \
         `searxng-download-*` directories under {}, and retry",
        staging_root.display()
    );
    Ok(())
}

fn ensure_provisioning_capacity(root: &Path, staging_root: &Path) -> Result<()> {
    ensure_generation_capacity(root)?;
    ensure_download_staging_capacity(staging_root)
}

fn count_managed_directories(parent: &Path, prefixes: &[&str], label: &str) -> Result<usize> {
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).with_context(|| format!("enumerating SearXNG {label} directories"));
        },
    };
    count_capacity_candidates(entries.map(|entry| {
        let entry = entry.with_context(|| format!("enumerating SearXNG {label} directories"))?;
        let name = entry.file_name();
        if !prefixes
            .iter()
            .any(|prefix| name.to_string_lossy().starts_with(prefix))
        {
            return Ok(false);
        }
        let kind = entry.file_type().with_context(|| {
            format!(
                "inspecting SearXNG {label} candidate {}",
                entry.path().display()
            )
        })?;
        anyhow::ensure!(
            kind.is_dir() || kind.is_symlink(),
            "SearXNG {label} candidate {} is not a directory",
            entry.path().display()
        );
        Ok(true)
    }))
}

fn count_capacity_candidates(candidates: impl IntoIterator<Item = Result<bool>>) -> Result<usize> {
    candidates
        .into_iter()
        .try_fold(0_usize, |count, candidate| {
            candidate.map(|matched| count + usize::from(matched))
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PublicationPhase {
    MarkersSynced,
    GenerationFinalized,
    PointerPublished,
}

/// Complete one generation while the caller retains the ownership transition
/// claim. Taking the guard as an argument makes it impossible for the normal
/// publication path to release exclusion between durable markers, the
/// generation rename, and the pointer switch.
fn complete_generation_under_claim(
    incoming: &Path,
    root: &Path,
    expected_sha: &str,
    claim: &OwnershipClaim<'_>,
) -> Result<PathBuf> {
    complete_generation_under_claim_with(incoming, root, expected_sha, claim, |_| Ok(()))
}

fn complete_generation_under_claim_with(
    incoming: &Path,
    root: &Path,
    expected_sha: &str,
    _claim: &OwnershipClaim<'_>,
    mut after_phase: impl FnMut(PublicationPhase) -> Result<()>,
) -> Result<PathBuf> {
    write_synced_file(
        &incoming.join(".version"),
        bundle_manifest::BUNDLE_VERSION.as_bytes(),
    )?;
    write_synced_file(
        &incoming.join(".sha256"),
        format!("{expected_sha}\n").as_bytes(),
    )?;
    sync_runtime_tree(incoming)?;
    after_phase(PublicationPhase::MarkersSynced)?;

    let generation = finalize_generation(incoming, root, expected_sha)?;
    after_phase(PublicationPhase::GenerationFinalized)?;

    publish_pointer(root, &generation, expected_sha)?;
    after_phase(PublicationPhase::PointerPublished)?;
    Ok(generation)
}

fn write_synced_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    Ok(())
}

fn sync_runtime_tree(path: &Path) -> Result<()> {
    sync_runtime_tree_with(
        path,
        &mut |file| {
            std::fs::File::open(file)?.sync_all()?;
            Ok(())
        },
        &mut sync_directory,
    )
}

fn sync_runtime_tree_with(
    path: &Path,
    sync_file: &mut impl FnMut(&Path) -> Result<()>,
    sync_dir: &mut impl FnMut(&Path) -> Result<()>,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting extracted SearXNG path {}", path.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_dir(),
        "the extracted SearXNG runtime {} is not a directory",
        path.display()
    );
    for entry in std::fs::read_dir(path)
        .with_context(|| format!("enumerating extracted SearXNG path {}", path.display()))?
    {
        let entry = entry
            .with_context(|| format!("enumerating extracted SearXNG path {}", path.display()))?;
        let child = entry.path();
        let kind = entry
            .file_type()
            .with_context(|| format!("inspecting extracted SearXNG path {}", child.display()))?;
        if kind.is_dir() {
            sync_runtime_tree_with(&child, sync_file, sync_dir)?;
        } else if kind.is_file() {
            sync_file(&child)
                .with_context(|| format!("syncing extracted SearXNG file {}", child.display()))?;
        } else if !kind.is_symlink() {
            return Err(anyhow!(
                "the extracted SearXNG runtime contains an unsupported filesystem entry: {}",
                child.display()
            ));
        }
    }
    sync_dir(path)
        .with_context(|| format!("syncing extracted SearXNG directory {}", path.display()))?;
    Ok(())
}

fn finalize_generation(incoming: &Path, root: &Path, expected_sha: &str) -> Result<PathBuf> {
    let generations = generations_dir(root);
    let prefix = generation_prefix(bundle_manifest::BUNDLE_VERSION, expected_sha);
    for _ in 0..32 {
        let generation = unique_candidate(&generations, &prefix);
        if generation.exists() {
            continue;
        }
        std::fs::rename(incoming, &generation).with_context(|| {
            format!(
                "publishing immutable SearXNG generation {}",
                generation.display()
            )
        })?;
        sync_directory(&generations)?;
        return Ok(generation);
    }
    Err(anyhow!("could not allocate a unique SearXNG generation"))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<()> {
    // Windows directory handles require platform-specific flags. Managed
    // provisioning is unavailable there; keep lock tests portable while the
    // production pointer operation continues to fail closed.
    Ok(())
}

fn publish_pointer(root: &Path, generation: &Path, expected_sha: &str) -> Result<()> {
    anyhow::ensure!(
        runtime_is_current(generation, expected_sha),
        "refusing to publish an incomplete SearXNG generation"
    );
    let name = generation
        .file_name()
        .and_then(|name| name.to_str())
        .context("the SearXNG generation name is not valid UTF-8")?;
    let pointer = pointer_path(root);
    let temp = unique_candidate(root, ".current-pointer");
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(format!("{name}\n").as_bytes())?;
        file.sync_all()?;
        drop(file);
        atomic_replace_pointer(&temp, &pointer)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(unix)]
fn atomic_replace_pointer(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to)
        .with_context(|| format!("atomically switching SearXNG pointer {}", to.display()))?;
    if let Some(parent) = to.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(windows)]
fn atomic_replace_pointer(_from: &Path, _to: &Path) -> Result<()> {
    // Managed provisioning is rejected before reaching this function on
    // Windows. `std::fs::rename` cannot atomically replace an existing file on
    // Windows, so fail closed instead of introducing a remove/rename gap.
    Err(anyhow!(
        "atomic managed-SearXNG pointer replacement is unavailable on Windows"
    ))
}

/// Cross-process installation ownership. A permanent advisory-lock file
/// serializes every transition, while this atomically published directory
/// records the current nonce-scoped owner and its heartbeat sequence.
struct ProvisionLock {
    path: PathBuf,
    owner: PathBuf,
    owner_token: String,
    heartbeat_sequence: AtomicU64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum LockSnapshot {
    Owned {
        owner: PathBuf,
        contents: Vec<u8>,
        modified: SystemTime,
    },
    Ownerless {
        modified: SystemTime,
    },
}

impl LockSnapshot {
    fn capture(path: &Path) -> Option<Self> {
        let lock_metadata = std::fs::symlink_metadata(path).ok()?;
        if !lock_metadata.file_type().is_dir() {
            return None;
        }
        let lock_modified = lock_metadata.modified().ok()?;
        let mut owners = std::fs::read_dir(path)
            .ok()?
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".owner-"))
            .collect::<Vec<_>>();
        if owners.len() > 1 {
            return None;
        }
        let Some(owner) = owners.pop() else {
            return Some(Self::Ownerless {
                modified: lock_modified,
            });
        };
        let before = std::fs::symlink_metadata(owner.path()).ok()?;
        if !before.file_type().is_file() || before.len() > MAX_POINTER_BYTES {
            return None;
        }
        let contents = std::fs::read(owner.path()).ok()?;
        let after = std::fs::symlink_metadata(owner.path()).ok()?;
        if !after.file_type().is_file()
            || before.len() != after.len()
            || before.modified().ok()? != after.modified().ok()?
            || u64::try_from(contents.len()).ok()? != after.len()
        {
            return None;
        }
        Some(Self::Owned {
            owner: owner.path(),
            contents,
            modified: after.modified().ok()?,
        })
    }

    fn modified(&self) -> SystemTime {
        match self {
            Self::Owned { modified, .. } | Self::Ownerless { modified } => *modified,
        }
    }

    fn same_owner_after_claim(&self, current: &Self) -> bool {
        self == current
    }
}

/// OS-released transition gate shared by ownership creation, heartbeat, stale
/// reaping, release, and final publication. The lock file lives beside the
/// canonical ownership directory and is never renamed or deleted, so a crash
/// cannot wedge a persistent claim and an old owner cannot target a replacement.
struct TransitionClaim {
    _file: std::fs::File,
}

impl TransitionClaim {
    fn open(lock_path: &Path) -> Result<std::fs::File> {
        let root = lock_path
            .parent()
            .context("the SearXNG provisioning lock has no parent")?;
        std::fs::create_dir_all(root)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(root.join(".provision-transition.lock"))
            .context("opening the SearXNG provisioning transition lock")
    }

    fn try_acquire(lock_path: &Path) -> Result<Option<Self>> {
        let file = Self::open(lock_path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn acquire(lock_path: &Path) -> Result<Self> {
        let file = Self::open(lock_path)?;
        file.lock()?;
        Ok(Self { _file: file })
    }
}

struct OwnershipClaim<'a> {
    _lock: &'a ProvisionLock,
    _transition: TransitionClaim,
}

impl ProvisionLock {
    async fn acquire(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let path = root.join(".provision-lock");
        let deadline = Instant::now() + LOCK_WAIT_TIMEOUT;
        loop {
            match try_create_lock(&path)? {
                Some(lock) => return Ok(lock),
                None => {
                    remove_stale_lock(&path);
                    if Instant::now() >= deadline {
                        return Err(anyhow!(
                            "timed out waiting for another Mermaid process to provision SearXNG"
                        ));
                    }
                    tokio::time::sleep(LOCK_POLL_INTERVAL).await;
                },
            }
        }
    }

    #[cfg(any(unix, test))]
    fn heartbeat(&self) -> Result<()> {
        let claim = self.claim_ownership()?;
        self.write_heartbeat_claimed()?;
        drop(claim);
        Ok(())
    }

    fn claim_for_publication(&self) -> Result<OwnershipClaim<'_>> {
        let claim = self.claim_ownership()?;
        self.write_heartbeat_claimed()?;
        Ok(claim)
    }

    fn claim_ownership(&self) -> Result<OwnershipClaim<'_>> {
        let transition = TransitionClaim::try_acquire(&self.path)?
            .ok_or_else(|| anyhow!("the SearXNG lock transition is already claimed"))?;
        self.verify_owner_claimed()?;
        Ok(OwnershipClaim {
            _lock: self,
            _transition: transition,
        })
    }

    fn verify_owner_claimed(&self) -> Result<()> {
        let snapshot = LockSnapshot::capture(&self.path)
            .context("the SearXNG provisioning owner disappeared")?;
        let LockSnapshot::Owned {
            owner, contents, ..
        } = snapshot
        else {
            return Err(anyhow!("the SearXNG provisioning lock has no owner"));
        };
        anyhow::ensure!(
            owner == self.owner && owner_record_has_token(&contents, &self.owner_token),
            "the SearXNG provisioning lock is owned by another worker"
        );
        Ok(())
    }

    fn write_heartbeat_claimed(&self) -> Result<()> {
        self.verify_owner_claimed()?;
        let previous = self
            .heartbeat_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |sequence| {
                sequence.checked_add(1)
            })
            .map_err(|_| anyhow!("the SearXNG lock heartbeat sequence overflowed"))?;
        let sequence = previous + 1;
        replace_owner_record(
            &self.owner,
            owner_record(&self.owner_token, sequence).as_bytes(),
        )
        .with_context(|| {
            format!(
                "updating the SearXNG provisioning heartbeat at {}",
                self.path.display()
            )
        })
    }
}

fn replace_owner_record(path: &Path, contents: &[u8]) -> Result<()> {
    replace_owner_record_with(path, contents, || Ok(()))
}

/// Persist a complete replacement before exposing it at the canonical owner
/// path. A crash can leave the previous complete record or the next complete
/// record, never a truncated sole owner that wedges stale-lock recovery.
fn replace_owner_record_with(
    path: &Path,
    contents: &[u8],
    before_replace: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let parent = path
        .parent()
        .context("the SearXNG provisioning owner has no parent")?;
    let temp = unique_candidate(parent, ".heartbeat-next");
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut next = options.open(&temp)?;
        next.write_all(contents)?;
        next.sync_all()?;
        drop(next);
        before_replace()?;
        replace_file(&temp, path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(unix)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    // Managed provisioning is unavailable on Windows. Keep lock protocol tests
    // portable; the transition claim prevents another compliant actor from
    // observing the short remove/rename interval on this unsupported target.
    std::fs::remove_file(to)?;
    std::fs::rename(from, to)
}

/// Signals a blocking extraction when its async waiter is dropped. The worker
/// owns the provisioning lock and cleanup guards, so cancellation requests an
/// orderly stop without releasing those resources from the async task.
struct CancelOnDrop {
    signal: Arc<AtomicBool>,
    armed: bool,
}

impl CancelOnDrop {
    fn new(signal: Arc<AtomicBool>) -> Self {
        Self {
            signal,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.signal.store(true, Ordering::Release);
        }
    }
}

impl Drop for ProvisionLock {
    fn drop(&mut self) {
        // Wait for a concurrent heartbeat/publication/reaper transition rather
        // than abandoning an owner directory. Advisory locks are released by
        // the OS if that claimant crashes.
        let Ok(claim) = TransitionClaim::acquire(&self.path) else {
            return;
        };
        if self.verify_owner_claimed().is_err() {
            return;
        }
        let quarantine = match quarantine_claimed_lock(&self.path, ".provision-lock-release") {
            Ok(quarantine) => quarantine,
            Err(error) => {
                tracing::warn!(path = %self.path.display(), %error, "could not release the SearXNG provisioning lock");
                return;
            },
        };
        drop(claim);
        remove_quarantined_lock(&quarantine);
    }
}

fn try_create_lock(path: &Path) -> Result<Option<ProvisionLock>> {
    let Some(claim) = TransitionClaim::try_acquire(path)? else {
        return Ok(None);
    };
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Ok(None),
        Err(error) if error.kind() == ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }

    let root = path
        .parent()
        .context("the SearXNG provisioning lock has no parent")?;
    let (prepared, mut prepared_cleanup) = create_unique_dir(root, ".provision-lock-prepared")?;
    let owner_token = new_owner_token()?;
    let owner_name = format!(".owner-{owner_token}");
    let prepared_owner = prepared.join(&owner_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut owner = options.open(&prepared_owner)?;
    owner.write_all(owner_record(&owner_token, 0).as_bytes())?;
    owner.sync_all()?;
    drop(owner);
    sync_directory(&prepared)?;

    match std::fs::rename(&prepared, path) {
        Ok(()) => {},
        Err(error) if error.kind() == ErrorKind::AlreadyExists => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    prepared_cleanup.disarm();
    sync_directory(root)?;
    drop(claim);

    Ok(Some(ProvisionLock {
        path: path.to_path_buf(),
        owner: path.join(owner_name),
        owner_token,
        heartbeat_sequence: AtomicU64::new(0),
    }))
}

fn new_owner_token() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow!("secure SearXNG lock-owner entropy is unavailable: {error}"))?;
    Ok(hex_lower(&bytes))
}

fn owner_record(token: &str, sequence: u64) -> String {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{token}\n{}\n{sequence}\n{stamp}\n", std::process::id())
}

fn remove_stale_lock(path: &Path) {
    match remove_stale_lock_with(path, lock_snapshot_is_stale, || Ok(())) {
        Ok(true) => {
            tracing::warn!(path = %path.display(), "removed a stale SearXNG provisioning lock")
        },
        Ok(false) => {},
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "could not inspect or remove a stale SearXNG provisioning lock")
        },
    }
}

fn lock_snapshot_is_stale(snapshot: &LockSnapshot) -> bool {
    snapshot
        .modified()
        .elapsed()
        .is_ok_and(|age| age >= STALE_LOCK_AGE)
}

fn remove_stale_lock_with(
    path: &Path,
    is_stale: impl Fn(&LockSnapshot) -> bool,
    after_observation: impl FnOnce() -> Result<()>,
) -> Result<bool> {
    let Some(observed) = LockSnapshot::capture(path) else {
        return Ok(false);
    };
    if !is_stale(&observed) {
        return Ok(false);
    }
    after_observation()?;
    let Some(claim) = TransitionClaim::try_acquire(path)? else {
        return Ok(false);
    };
    let Some(current) = LockSnapshot::capture(path) else {
        return Ok(false);
    };
    if !observed.same_owner_after_claim(&current)
        || matches!(current, LockSnapshot::Owned { .. }) && !is_stale(&current)
    {
        return Ok(false);
    }
    let quarantine = quarantine_claimed_lock(path, ".provision-lock-stale")?;
    drop(claim);
    remove_quarantined_lock(&quarantine);
    Ok(true)
}

fn owner_record_has_token(contents: &[u8], token: &str) -> bool {
    contents
        .split(|byte| *byte == b'\n')
        .next()
        .is_some_and(|line| line == token.as_bytes())
}

fn quarantine_claimed_lock(path: &Path, prefix: &str) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("the provisioning lock has no parent")?;
    let quarantine = unique_candidate(parent, prefix);
    std::fs::rename(path, &quarantine)?;
    sync_directory(parent)?;
    Ok(quarantine)
}

fn remove_quarantined_lock(quarantine: &Path) {
    let parent = quarantine.parent();
    if let Err(error) = std::fs::remove_dir_all(quarantine) {
        if error.kind() != ErrorKind::NotFound {
            tracing::warn!(path = %quarantine.display(), %error, "could not remove a quarantined SearXNG provisioning lock");
        }
        return;
    }
    if let Some(parent) = parent
        && let Err(error) = sync_directory(parent)
    {
        tracing::warn!(path = %parent.display(), %error, "could not sync the SearXNG lock directory after quarantine cleanup");
    }
}

struct CleanupDir {
    path: Option<PathBuf>,
}

impl CleanupDir {
    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for CleanupDir {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(error) = std::fs::remove_dir_all(&path)
            && error.kind() != ErrorKind::NotFound
        {
            tracing::warn!(path = %path.display(), %error, "could not remove a SearXNG staging directory");
        }
    }
}

fn create_unique_dir(parent: &Path, prefix: &str) -> Result<(PathBuf, CleanupDir)> {
    std::fs::create_dir_all(parent)?;
    for _ in 0..32 {
        let path = unique_candidate(parent, prefix);
        match std::fs::create_dir(&path) {
            Ok(()) => {
                return Ok((path.clone(), CleanupDir { path: Some(path) }));
            },
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error.into()),
        }
    }
    Err(anyhow!(
        "could not allocate a unique SearXNG staging directory"
    ))
}

fn unique_candidate(parent: &Path, prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let count = UNIQUE_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(
        "{prefix}-{}-{nanos:x}-{count:x}",
        std::process::id()
    ))
}

#[cfg(unix)]
fn extract(
    archive: &Path,
    dest: &Path,
    lock: &ProvisionLock,
    cancellation: Arc<AtomicBool>,
) -> Result<()> {
    lock.heartbeat()?;
    let file = std::fs::File::open(archive)?;
    let decoder = ruzstd::decoding::StreamingDecoder::new(file)
        .map_err(|e| anyhow!("initializing the zstd decoder: {e}"))?;
    let bounded = DecodedLimitReader::new(decoder, MAX_UNPACKED_BYTES);
    let progressing = ExtractionProgressReader::new(
        bounded,
        cancellation,
        || lock.heartbeat(),
        EXTRACTION_HEARTBEAT_BYTES,
        EXTRACTION_HEARTBEAT_INTERVAL,
    );
    extract_tar(progressing, dest)
}

#[cfg(windows)]
fn extract(
    _archive: &Path,
    _dest: &Path,
    _lock: &ProvisionLock,
    _cancellation: Arc<AtomicBool>,
) -> Result<()> {
    // No Windows bundle is published (SearXNG imports Unix-only modules like
    // `pwd`); ensure_bundle returns the unsupported-platform error long before
    // this is reachable. Present only so the crate compiles on Windows.
    Err(anyhow!("SearXNG bundles are not published for Windows"))
}

#[cfg(any(unix, test))]
#[derive(Debug, Default)]
struct ExtractionBudget {
    entries: u64,
    bytes: u64,
}

/// Bound the complete decoded tar stream, including PAX/GNU metadata that the
/// `tar` iterator consumes internally before yielding an entry. Per-entry
/// accounting alone cannot see those extension records and therefore cannot
/// defend against a compressed metadata bomb.
#[cfg(any(unix, test))]
struct DecodedLimitReader<R> {
    inner: R,
    remaining: u64,
}

#[cfg(any(unix, test))]
impl<R> DecodedLimitReader<R> {
    fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
        }
    }
}

#[cfg(any(unix, test))]
impl<R: std::io::Read> std::io::Read for DecodedLimitReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        // Read one byte beyond the remaining allowance so exact-limit streams
        // can still reach EOF while oversized streams fail deterministically.
        let allowed = self
            .remaining
            .saturating_add(1)
            .min(u64::try_from(output.len()).unwrap_or(u64::MAX));
        let allowed = usize::try_from(allowed).unwrap_or(output.len());
        let read = self.inner.read(&mut output[..allowed])?;
        let read_u64 = u64::try_from(read).unwrap_or(u64::MAX);
        if read_u64 > self.remaining {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "the SearXNG archive exceeded the {} MiB decoded-stream limit",
                    MAX_UNPACKED_BYTES / (1024 * 1024)
                ),
            ));
        }
        self.remaining -= read_u64;
        Ok(read)
    }
}

/// A streaming archive reader that checks cancellation on every read and
/// renews provisioning ownership by decoded-byte progress or elapsed time.
/// Because `tar::Entry::unpack_in` ultimately reads through the archive's
/// source, this also heartbeats throughout one very large file entry.
#[cfg(any(unix, test))]
struct ExtractionProgressReader<R, H> {
    inner: R,
    cancellation: Arc<AtomicBool>,
    heartbeat: H,
    bytes_since_heartbeat: u64,
    heartbeat_bytes: u64,
    last_heartbeat: Instant,
    heartbeat_interval: Duration,
}

#[cfg(any(unix, test))]
impl<R, H> ExtractionProgressReader<R, H> {
    fn new(
        inner: R,
        cancellation: Arc<AtomicBool>,
        heartbeat: H,
        heartbeat_bytes: u64,
        heartbeat_interval: Duration,
    ) -> Self {
        Self {
            inner,
            cancellation,
            heartbeat,
            bytes_since_heartbeat: 0,
            heartbeat_bytes,
            last_heartbeat: Instant::now(),
            heartbeat_interval,
        }
    }

    fn cancelled_error() -> std::io::Error {
        std::io::Error::new(
            ErrorKind::Interrupted,
            "SearXNG archive extraction was cancelled",
        )
    }
}

#[cfg(any(unix, test))]
impl<R, H> std::io::Read for ExtractionProgressReader<R, H>
where
    R: std::io::Read,
    H: FnMut() -> Result<()>,
{
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if self.cancellation.load(Ordering::Acquire) {
            return Err(Self::cancelled_error());
        }
        let read = self.inner.read(output)?;
        if self.cancellation.load(Ordering::Acquire) {
            return Err(Self::cancelled_error());
        }
        self.bytes_since_heartbeat = self
            .bytes_since_heartbeat
            .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if self.bytes_since_heartbeat >= self.heartbeat_bytes
            || self.last_heartbeat.elapsed() >= self.heartbeat_interval
        {
            (self.heartbeat)().map_err(|error| std::io::Error::other(error.to_string()))?;
            self.bytes_since_heartbeat = 0;
            self.last_heartbeat = Instant::now();
        }
        Ok(read)
    }
}

#[cfg(any(unix, test))]
impl ExtractionBudget {
    fn account(&mut self, path: &Path, path_bytes: usize, size: u64) -> Result<()> {
        validate_archive_path(path, path_bytes)?;
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| anyhow!("the SearXNG archive entry count overflowed"))?;
        anyhow::ensure!(
            self.entries <= MAX_ARCHIVE_ENTRIES,
            "the SearXNG archive exceeded the {MAX_ARCHIVE_ENTRIES} entry limit"
        );
        self.bytes = self
            .bytes
            .checked_add(size)
            .ok_or_else(|| anyhow!("the SearXNG archive unpacked size overflowed"))?;
        anyhow::ensure!(
            self.bytes <= MAX_UNPACKED_BYTES,
            "the SearXNG archive exceeded the {} MiB unpacked-size limit",
            MAX_UNPACKED_BYTES / (1024 * 1024)
        );
        Ok(())
    }
}

#[cfg(any(unix, test))]
fn validate_archive_path(path: &Path, path_bytes: usize) -> Result<()> {
    use std::path::Component;

    anyhow::ensure!(
        !path.as_os_str().is_empty(),
        "the SearXNG archive contains an empty path"
    );
    anyhow::ensure!(
        path_bytes <= MAX_ARCHIVE_PATH_BYTES,
        "the SearXNG archive contains a path longer than {MAX_ARCHIVE_PATH_BYTES} bytes"
    );
    for component in path.components() {
        anyhow::ensure!(
            matches!(component, Component::Normal(_) | Component::CurDir),
            "the SearXNG archive contains an unsafe path: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(any(unix, test))]
fn extract_tar(reader: impl std::io::Read, dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    let mut budget = ExtractionBudget::default();
    for entry in archive.entries().context("reading the .tar.zst bundle")? {
        let mut entry = entry.context("reading an entry from the .tar.zst bundle")?;
        let path = entry.path()?.into_owned();
        budget.account(&path, entry.path_bytes().len(), entry.size())?;
        let kind = entry.header().entry_type();
        anyhow::ensure!(
            kind.is_file()
                || kind.is_dir()
                || kind.is_symlink()
                || kind.is_hard_link()
                || kind.is_contiguous(),
            "the SearXNG archive contains unsupported entry type {:?} at {}",
            kind,
            path.display()
        );
        if kind.is_symlink() || kind.is_hard_link() {
            let target = entry
                .link_name()?
                .ok_or_else(|| anyhow!("archive link {} has no target", path.display()))?;
            validate_archive_link(
                &path,
                &target,
                entry.link_name_bytes().map_or(0, |v| v.len()),
                kind.is_hard_link(),
            )?;
        }
        anyhow::ensure!(
            entry.unpack_in(dest)?,
            "the SearXNG archive entry escaped its destination: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(any(unix, test))]
fn validate_archive_link(
    entry_path: &Path,
    target: &Path,
    target_bytes: usize,
    root_relative: bool,
) -> Result<()> {
    use std::path::Component;

    anyhow::ensure!(
        target_bytes <= MAX_ARCHIVE_PATH_BYTES,
        "archive link target is longer than {MAX_ARCHIVE_PATH_BYTES} bytes"
    );
    let mut depth = if root_relative {
        0
    } else {
        entry_path.parent().map_or(0, |path| {
            path.components()
                .filter(|part| matches!(part, Component::Normal(_)))
                .count()
        })
    };
    for component in target.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {},
            Component::ParentDir if depth > 0 => depth -= 1,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!(
                    "archive link {} escapes its destination via {}",
                    entry_path.display(),
                    target.display()
                ));
            },
        }
    }
    Ok(())
}

/// Lowercase-hex a byte slice, matching the pinned digest format (mirrors the
/// idiom in `searxng::random_secret`).
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn write_complete_runtime(runtime: &Path, sha: &str) {
        std::fs::create_dir_all(runtime).unwrap();
        std::fs::write(runtime.join(".version"), bundle_manifest::BUNDLE_VERSION).unwrap();
        std::fs::write(runtime.join(".sha256"), format!("{sha}\n")).unwrap();
        let python = crate::searxng::python_bin(runtime);
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(python, b"python").unwrap();
        std::fs::create_dir_all(runtime.join("python").join("lib")).unwrap();
    }

    fn write_generation(root: &Path, suffix: &str, sha: &str) -> PathBuf {
        let name = format!(
            "{}-{suffix}",
            generation_prefix(bundle_manifest::BUNDLE_VERSION, sha)
        );
        let runtime = generations_dir(root).join(name);
        write_complete_runtime(&runtime, sha);
        runtime
    }

    fn select_generation_directly(root: &Path, runtime: &Path) {
        std::fs::write(
            pointer_path(root),
            format!("{}\n", runtime.file_name().unwrap().to_string_lossy()),
        )
        .unwrap();
    }

    #[test]
    fn triple_covers_the_published_targets() {
        assert_eq!(triple_for("linux", "x86_64"), Some("linux-x86_64"));
        assert_eq!(triple_for("linux", "aarch64"), Some("linux-aarch64"));
        assert_eq!(triple_for("macos", "aarch64"), Some("macos-aarch64"));
        assert_eq!(triple_for("macos", "x86_64"), Some("macos-x86_64"));
    }

    #[test]
    fn triple_is_none_for_unpublished_platforms() {
        assert_eq!(triple_for("windows", "x86_64"), None); // SearXNG needs `pwd`
        assert_eq!(triple_for("freebsd", "x86_64"), None);
    }

    #[test]
    fn viability_explains_unsupported_windows() {
        let error = viability_for("windows", "x86_64").unwrap_err();
        assert!(error.contains("windows/x86_64"), "{error}");
        assert!(error.contains("searxng_url"), "{error}");
    }

    #[test]
    fn every_published_triple_has_a_pinned_checksum() {
        // The platform list and the checksum table must not drift apart.
        for t in [
            "linux-x86_64",
            "linux-aarch64",
            "macos-aarch64",
            "macos-x86_64",
        ] {
            let sha = bundle_manifest::bundle_sha256(t).unwrap_or_else(|| panic!("no sha for {t}"));
            assert_eq!(sha.len(), 64, "{t} sha is not 64 hex chars");
            assert!(
                sha.chars().all(|c| c.is_ascii_hexdigit()),
                "{t} sha not hex"
            );
        }
    }

    #[test]
    fn the_current_host_target_is_pinned_when_supported() {
        if let Ok(t) = managed_backend_viability() {
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
    fn current_runtime_requires_marker_interpreter_and_library_tree() {
        let (runtime, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-runtime-check").unwrap();
        std::fs::write(runtime.join(".version"), bundle_manifest::BUNDLE_VERSION).unwrap();
        assert!(
            !runtime_is_current(&runtime, TEST_SHA),
            "marker alone was trusted"
        );

        let python = crate::searxng::python_bin(&runtime);
        std::fs::create_dir_all(python.parent().unwrap()).unwrap();
        std::fs::write(&python, b"python").unwrap();
        assert!(
            !runtime_is_current(&runtime, TEST_SHA),
            "missing library tree was trusted"
        );

        std::fs::create_dir_all(runtime.join("python").join("lib")).unwrap();
        assert!(
            !runtime_is_current(&runtime, TEST_SHA),
            "missing checksum marker was trusted"
        );
        std::fs::write(runtime.join(".sha256"), TEST_SHA).unwrap();
        assert!(runtime_is_current(&runtime, TEST_SHA));
        assert!(!runtime_is_current(&runtime, &"b".repeat(64)));
    }

    #[test]
    fn extraction_budget_rejects_size_count_and_unsafe_paths() {
        let mut budget = ExtractionBudget {
            bytes: MAX_UNPACKED_BYTES,
            ..ExtractionBudget::default()
        };
        let error = budget.account(Path::new("python/file"), 11, 1).unwrap_err();
        assert!(error.to_string().contains("unpacked-size"), "{error}");

        let mut budget = ExtractionBudget {
            entries: MAX_ARCHIVE_ENTRIES,
            bytes: 0,
        };
        let error = budget.account(Path::new("python/file"), 11, 0).unwrap_err();
        assert!(error.to_string().contains("entry limit"), "{error}");

        for path in [Path::new("../escape"), Path::new("/absolute")] {
            assert!(
                validate_archive_path(path, path.as_os_str().len()).is_err(),
                "accepted {}",
                path.display()
            );
        }
        assert!(
            validate_archive_path(Path::new("python/file"), MAX_ARCHIVE_PATH_BYTES + 1).is_err()
        );
    }

    #[test]
    fn decoded_reader_rejects_hidden_archive_overhead_past_limit() {
        use std::io::{Cursor, Read};

        let mut reader = DecodedLimitReader::new(Cursor::new(b"12345"), 4);
        let mut decoded = Vec::new();
        let error = reader.read_to_end(&mut decoded).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert!(error.to_string().contains("decoded-stream"), "{error}");
        assert!(decoded.len() <= 4, "decoded past the configured limit");
    }

    #[test]
    fn bounded_tar_extraction_accepts_files_and_rejects_traversal() {
        use std::io::Cursor;

        let mut valid = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut valid);
            let body = b"runtime";
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "python/lib/runtime.txt", &body[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let (dest, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-extract-test").unwrap();
        extract_tar(Cursor::new(valid), &dest).unwrap();
        assert_eq!(
            std::fs::read(dest.join("python/lib/runtime.txt")).unwrap(),
            b"runtime"
        );

        assert!(
            validate_archive_link(
                Path::new("python/bin/python3"),
                Path::new("../lib/python3"),
                14,
                false
            )
            .is_ok()
        );
        assert!(
            validate_archive_link(
                Path::new("python-link"),
                Path::new("../../outside"),
                13,
                false
            )
            .is_err()
        );
    }

    #[test]
    fn large_single_entry_heartbeats_by_streamed_bytes() {
        use std::io::Cursor;

        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let body = vec![b'x'; 128 * 1024];
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, "python/lib/large.bin", &body[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let heartbeats = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&heartbeats);
        let progress = ExtractionProgressReader::new(
            Cursor::new(tar_bytes),
            Arc::new(AtomicBool::new(false)),
            move || {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            4 * 1024,
            Duration::from_secs(60 * 60),
        );
        let (dest, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-large-entry-test").unwrap();

        extract_tar(progress, &dest).unwrap();

        assert!(
            heartbeats.load(Ordering::Relaxed) > 1,
            "a large entry did not renew ownership while streaming"
        );
        assert_eq!(
            std::fs::metadata(dest.join("python/lib/large.bin"))
                .unwrap()
                .len(),
            128 * 1024
        );
    }

    #[test]
    fn extraction_cancellation_interrupts_reader_and_guard_signals_drop() {
        use std::io::Cursor;

        let cancellation = Arc::new(AtomicBool::new(false));
        {
            let _guard = CancelOnDrop::new(Arc::clone(&cancellation));
        }
        assert!(cancellation.load(Ordering::Acquire));

        let mut reader = ExtractionProgressReader::new(
            Cursor::new(b"archive"),
            Arc::clone(&cancellation),
            || Ok(()),
            EXTRACTION_HEARTBEAT_BYTES,
            EXTRACTION_HEARTBEAT_INTERVAL,
        );
        let mut output = [0_u8; 8];
        let error = reader.read(&mut output).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Interrupted);
    }

    #[test]
    fn elapsed_time_triggers_heartbeat_even_below_byte_threshold() {
        use std::io::Cursor;

        let heartbeats = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&heartbeats);
        let mut reader = ExtractionProgressReader::new(
            Cursor::new(b"small"),
            Arc::new(AtomicBool::new(false)),
            move || {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(())
            },
            u64::MAX,
            Duration::ZERO,
        );
        let mut decoded = Vec::new();
        reader.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, b"small");
        assert!(heartbeats.load(Ordering::Relaxed) >= 1);
    }

    #[test]
    fn runtime_directory_is_versioned_and_path_safe() {
        assert_eq!(versioned_runtime_name("v0.3.0"), "runtime-v0.3.0");
        assert_eq!(
            versioned_runtime_name("../next version"),
            "runtime-..-next-version"
        );
        assert!(pointer_name("v0.3.0").starts_with("current-runtime-v0.3.0"));
    }

    #[test]
    fn bundle_size_limit_is_inclusive() {
        assert!(ensure_download_size(MAX_BUNDLE_BYTES).is_ok());
        let error = ensure_download_size(MAX_BUNDLE_BYTES + 1).unwrap_err();
        assert!(error.to_string().contains("128 MiB"), "{error}");
    }

    #[test]
    fn generation_retention_refuses_growth_without_deleting_live_candidates() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-generation-cap-test")
                .unwrap();
        let generations = generations_dir(&root);
        std::fs::create_dir_all(&generations).unwrap();
        let mut retained = Vec::new();
        for index in 0..MAX_GENERATION_DIRECTORIES {
            let prefix = if index + 1 == MAX_GENERATION_DIRECTORIES {
                ".runtime-incoming"
            } else {
                "runtime-retained"
            };
            let generation = generations.join(format!("{prefix}-{index}"));
            std::fs::create_dir(&generation).unwrap();
            retained.push(generation);
        }

        let error = ensure_generation_capacity(&root).unwrap_err();
        assert!(error.to_string().contains("safe limit"), "{error}");
        assert!(error.to_string().contains("stop all Mermaid"), "{error}");
        assert!(
            retained.iter().all(|generation| generation.is_dir()),
            "capacity enforcement deleted a generation a live process might use"
        );

        std::fs::remove_dir(retained.pop().unwrap()).unwrap();
        assert!(ensure_generation_capacity(&root).is_ok());
    }

    #[test]
    fn capacity_enumeration_fails_closed_on_candidate_errors() {
        let error = count_capacity_candidates([
            Ok(true),
            Err(anyhow!("injected directory-entry inspection failure")),
        ])
        .unwrap_err();
        assert!(error.to_string().contains("inspection failure"), "{error}");

        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-capacity-type-test").unwrap();
        let generations = generations_dir(&root);
        std::fs::create_dir_all(&generations).unwrap();
        std::fs::write(generations.join("runtime-not-a-directory"), b"invalid").unwrap();
        let error = ensure_generation_capacity(&root).unwrap_err();
        assert!(error.to_string().contains("is not a directory"), "{error}");
    }

    #[test]
    fn crashed_download_staging_is_bounded() {
        let (staging, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-download-cap-test").unwrap();
        let mut retained = Vec::new();
        for index in 0..MAX_DOWNLOAD_STAGING_DIRECTORIES {
            let path = staging.join(format!("searxng-download-crashed-{index}"));
            std::fs::create_dir(&path).unwrap();
            retained.push(path);
        }

        let error = ensure_download_staging_capacity(&staging).unwrap_err();
        assert!(error.to_string().contains("download staging"), "{error}");
        assert!(error.to_string().contains("safe limit"), "{error}");
        assert!(retained.iter().all(|path| path.is_dir()));

        std::fs::remove_dir(retained.pop().unwrap()).unwrap();
        assert!(ensure_download_staging_capacity(&staging).is_ok());
    }

    #[test]
    fn runtime_tree_sync_visits_files_before_their_directories() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-sync-tree-test").unwrap();
        let nested = root.join("python").join("lib");
        std::fs::create_dir_all(&nested).unwrap();
        let payload = nested.join("module.py");
        std::fs::write(&payload, b"pass\n").unwrap();

        let mut synced_files = Vec::new();
        let mut synced_directories = Vec::new();
        sync_runtime_tree_with(
            &root,
            &mut |path| {
                synced_files.push(path.to_path_buf());
                Ok(())
            },
            &mut |path| {
                synced_directories.push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(synced_files, [payload]);
        assert!(synced_directories.contains(&nested));
        assert_eq!(synced_directories.last(), Some(&root));
    }

    #[test]
    fn provision_lock_is_exclusive_and_released_on_drop() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-lock-test").unwrap();
        let path = root.join("lock");
        let first = try_create_lock(&path).unwrap().expect("first lock");
        assert!(try_create_lock(&path).unwrap().is_none());
        drop(first);
        assert!(try_create_lock(&path).unwrap().is_some());
    }

    #[test]
    fn heartbeat_after_stale_observation_prevents_reaping() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-lock-toctou-test").unwrap();
        let path = root.join("lock");
        let lock = try_create_lock(&path).unwrap().expect("lock");
        let observed = LockSnapshot::capture(&path).unwrap();

        let removed = remove_stale_lock_with(&path, |_| true, || lock.heartbeat()).unwrap();

        assert!(!removed, "a renewed lock was reaped");
        assert!(path.is_dir());
        assert_ne!(LockSnapshot::capture(&path).unwrap(), observed);
        lock.heartbeat().unwrap();
    }

    #[test]
    fn owner_heartbeat_replacement_never_truncates_the_live_record() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-owner-replace-test").unwrap();
        let owner = root.join("owner");
        std::fs::write(&owner, b"complete-old-record").unwrap();

        let error = replace_owner_record_with(&owner, b"complete-new-record", || {
            Err(anyhow!("injected pre-publication failure"))
        })
        .unwrap_err();
        assert!(error.to_string().contains("pre-publication"), "{error}");
        assert_eq!(std::fs::read(&owner).unwrap(), b"complete-old-record");
        assert!(
            std::fs::read_dir(&root)
                .unwrap()
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".heartbeat-next-")),
            "failed replacement leaked its prepared heartbeat"
        );

        replace_owner_record(&owner, b"complete-new-record").unwrap();
        assert_eq!(std::fs::read(&owner).unwrap(), b"complete-new-record");
    }

    #[test]
    fn quarantine_is_retained_until_the_transition_claim_is_released() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-quarantine-test").unwrap();
        let path = root.join("lock");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("owner"), b"record").unwrap();
        let claim = TransitionClaim::acquire(&path).unwrap();

        let quarantine = quarantine_claimed_lock(&path, ".test-quarantine").unwrap();
        assert!(!path.exists());
        assert!(quarantine.is_dir());

        drop(claim);
        remove_quarantined_lock(&quarantine);
        assert!(!quarantine.exists());
    }

    #[test]
    fn transition_claim_blocks_heartbeat_and_stale_reaping() {
        let (root, _cleanup) = create_unique_dir(
            &std::env::temp_dir(),
            "mermaid-searxng-lock-transition-test",
        )
        .unwrap();
        let path = root.join("lock");
        let lock = try_create_lock(&path).unwrap().expect("lock");
        let before = std::fs::read(&lock.owner).unwrap();
        let claim = TransitionClaim::try_acquire(&path)
            .unwrap()
            .expect("transition claim");

        let heartbeat_error = lock.heartbeat().unwrap_err();
        assert!(heartbeat_error.to_string().contains("already claimed"));
        assert_eq!(std::fs::read(&lock.owner).unwrap(), before);
        assert!(!remove_stale_lock_with(&path, |_| true, || Ok(())).unwrap());

        drop(claim);
        lock.heartbeat().unwrap();
    }

    #[test]
    fn reaped_lock_owner_cannot_release_a_replacement() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-lock-race-test").unwrap();
        let path = root.join("lock");
        let first = try_create_lock(&path).unwrap().expect("first lock");
        let claim = TransitionClaim::acquire(&path).unwrap();
        let quarantine = quarantine_claimed_lock(&path, ".test-stale-lock").unwrap();
        drop(claim);
        remove_quarantined_lock(&quarantine);

        let second = try_create_lock(&path).unwrap().expect("replacement lock");
        assert_ne!(first.owner_token, second.owner_token);
        drop(first);
        assert!(
            try_create_lock(&path).unwrap().is_none(),
            "the old owner removed the replacement lock"
        );
        drop(second);
    }

    #[test]
    fn reaped_lock_owner_cannot_heartbeat_into_a_replacement() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-heartbeat-race-test")
                .unwrap();
        let path = root.join("lock");
        let first = try_create_lock(&path).unwrap().expect("first lock");
        let claim = TransitionClaim::acquire(&path).unwrap();
        let quarantine = quarantine_claimed_lock(&path, ".test-stale-heartbeat").unwrap();
        drop(claim);
        remove_quarantined_lock(&quarantine);
        let second = try_create_lock(&path).unwrap().expect("replacement lock");
        let replacement_before = std::fs::read(&second.owner).unwrap();

        assert!(first.heartbeat().is_err(), "lost owner token was recreated");
        assert_eq!(std::fs::read(&second.owner).unwrap(), replacement_before);
        assert!(!first.owner.exists());
        drop(first);
        drop(second);
    }

    const TRANSITION_EXIT_ROOT_ENV: &str = "MERMAID_SEARXNG_TRANSITION_EXIT_TEST_ROOT";

    #[test]
    fn transition_lock_process_exit_helper() {
        let Some(root) = std::env::var_os(TRANSITION_EXIT_ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let path = root.join("lock");
        let _claim = TransitionClaim::acquire(&path).unwrap();
        // Bypass Rust destructors. The kernel must release the advisory lock.
        std::process::exit(0);
    }

    #[test]
    fn transition_claim_is_recoverable_after_claimant_exit() {
        use std::process::{Command, Stdio};

        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-claim-exit-test").unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "searxng::bundle::tests::transition_lock_process_exit_helper",
                "--test-threads=1",
            ])
            .env(TRANSITION_EXIT_ROOT_ENV, &root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());

        let path = root.join("lock");
        assert!(
            TransitionClaim::try_acquire(&path).unwrap().is_some(),
            "claimant exit left a persistent transition wedge"
        );
    }

    #[test]
    fn rejection_is_external_and_never_mutates_a_generation() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-reject-test").unwrap();
        let old = write_generation(&root, "old", TEST_SHA);
        let replacement = write_generation(&root, "replacement", TEST_SHA);
        std::fs::write(old.join("payload"), b"old payload").unwrap();
        select_generation_directly(&root, &replacement);

        invalidate_runtime(&old).unwrap();

        assert_eq!(std::fs::read(old.join("payload")).unwrap(), b"old payload");
        assert!(runtime_is_current(&old, TEST_SHA));
        assert_eq!(active_runtime(&root, TEST_SHA).unwrap(), replacement);
        assert!(runtime_is_rejected(&root, &old));
    }

    #[test]
    fn pointer_resolution_rejects_partial_traversal_and_bad_generations() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-pointer-test").unwrap();
        let valid = write_generation(&root, "valid", TEST_SHA);
        select_generation_directly(&root, &valid);
        assert_eq!(active_runtime(&root, TEST_SHA).unwrap(), valid);

        for invalid in ["../outside\n".to_string(), "x".repeat(300)] {
            std::fs::write(pointer_path(&root), invalid).unwrap();
            assert!(active_runtime(&root, TEST_SHA).is_none());
        }

        let corrupt = write_generation(&root, "corrupt", TEST_SHA);
        std::fs::remove_file(corrupt.join(".sha256")).unwrap();
        select_generation_directly(&root, &corrupt);
        assert!(active_runtime(&root, TEST_SHA).is_none());

        select_generation_directly(&root, &valid);
        assert!(active_runtime(&root, &"b".repeat(64)).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn pointer_resolution_rejects_symlinked_pointer_and_generation() {
        use std::os::unix::fs::symlink;

        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-symlink-test").unwrap();
        let valid = write_generation(&root, "valid", TEST_SHA);
        let external_pointer = root.join("external-pointer");
        std::fs::write(
            &external_pointer,
            format!("{}\n", valid.file_name().unwrap().to_string_lossy()),
        )
        .unwrap();
        symlink(&external_pointer, pointer_path(&root)).unwrap();
        assert!(active_runtime(&root, TEST_SHA).is_none());

        std::fs::remove_file(pointer_path(&root)).unwrap();
        let linked = generations_dir(&root).join(format!(
            "{}-linked",
            generation_prefix(bundle_manifest::BUNDLE_VERSION, TEST_SHA)
        ));
        symlink(&valid, &linked).unwrap();
        select_generation_directly(&root, &linked);
        assert!(active_runtime(&root, TEST_SHA).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn pointer_switch_retains_every_published_generation() {
        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-publish-test").unwrap();
        let first = write_generation(&root, "first", TEST_SHA);
        let second = write_generation(&root, "second", TEST_SHA);
        std::fs::write(first.join("payload"), b"first").unwrap();

        publish_pointer(&root, &first, TEST_SHA).unwrap();
        let old_pointer = std::fs::File::open(pointer_path(&root)).unwrap();
        publish_pointer(&root, &second, TEST_SHA).unwrap();

        assert_eq!(active_runtime(&root, TEST_SHA).unwrap(), second);
        assert_eq!(std::fs::read(first.join("payload")).unwrap(), b"first");
        let mut old_contents = String::new();
        old_pointer
            .take(MAX_POINTER_BYTES)
            .read_to_string(&mut old_contents)
            .unwrap();
        assert!(old_contents.contains("first"));
    }

    #[cfg(unix)]
    #[test]
    fn publication_claim_spans_markers_generation_and_pointer() {
        let (root, _cleanup) = create_unique_dir(
            &std::env::temp_dir(),
            "mermaid-searxng-publication-claim-test",
        )
        .unwrap();
        let lock_path = root.join(".provision-lock");
        let lock = try_create_lock(&lock_path).unwrap().expect("lock");
        let (incoming, mut incoming_cleanup) =
            create_unique_dir(&generations_dir(&root), ".runtime-incoming").unwrap();
        write_complete_runtime(&incoming, TEST_SHA);
        let publication_claim = lock.claim_for_publication().unwrap();
        let mut phases = Vec::new();

        let generation = complete_generation_under_claim_with(
            &incoming,
            &root,
            TEST_SHA,
            &publication_claim,
            |phase| {
                assert!(
                    TransitionClaim::try_acquire(&lock_path)?.is_none(),
                    "transition exclusion was released during {phase:?}"
                );
                assert!(
                    lock.heartbeat().is_err(),
                    "heartbeat interleaved during {phase:?}"
                );
                phases.push(phase);
                Ok(())
            },
        )
        .unwrap();
        incoming_cleanup.disarm();

        assert_eq!(
            phases,
            vec![
                PublicationPhase::MarkersSynced,
                PublicationPhase::GenerationFinalized,
                PublicationPhase::PointerPublished,
            ]
        );
        assert_eq!(active_runtime(&root, TEST_SHA).unwrap(), generation);
        drop(publication_claim);
        lock.heartbeat().unwrap();
    }

    #[cfg(unix)]
    const MULTIPROCESS_ROOT_ENV: &str = "MERMAID_SEARXNG_MULTIPROCESS_TEST_ROOT";

    #[cfg(unix)]
    #[test]
    fn multiprocess_provision_helper() {
        let Some(root) = std::env::var_os(MULTIPROCESS_ROOT_ENV).map(PathBuf::from) else {
            return;
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let lock = ProvisionLock::acquire(&root).await.unwrap();
            if active_runtime(&root, TEST_SHA).is_none() {
                let generations = generations_dir(&root);
                let (incoming, mut cleanup) =
                    create_unique_dir(&generations, ".runtime-incoming").unwrap();
                write_complete_runtime(&incoming, TEST_SHA);
                tokio::time::sleep(Duration::from_millis(100)).await;
                let publication_claim = lock.claim_for_publication().unwrap();
                let generation =
                    complete_generation_under_claim(&incoming, &root, TEST_SHA, &publication_claim)
                        .unwrap();
                cleanup.disarm();
                drop(publication_claim);
                assert_eq!(active_runtime(&root, TEST_SHA).unwrap(), generation);
            }
            drop(lock);
        });
    }

    #[cfg(unix)]
    #[test]
    fn two_processes_publish_exactly_one_generation() {
        use std::process::{Command, Stdio};

        let (root, _cleanup) =
            create_unique_dir(&std::env::temp_dir(), "mermaid-searxng-multiprocess-test").unwrap();
        let executable = std::env::current_exe().unwrap();
        let spawn = || {
            Command::new(&executable)
                .args([
                    "--exact",
                    "searxng::bundle::tests::multiprocess_provision_helper",
                    "--test-threads=1",
                ])
                .env(MULTIPROCESS_ROOT_ENV, &root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());

        let generation_count = std::fs::read_dir(generations_dir(&root))
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&generation_prefix(
                        bundle_manifest::BUNDLE_VERSION,
                        TEST_SHA,
                    ))
            })
            .count();
        assert_eq!(generation_count, 1);
        assert!(active_runtime(&root, TEST_SHA).is_some());
    }

    #[test]
    fn hex_lower_is_zero_padded_lowercase() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }
}
