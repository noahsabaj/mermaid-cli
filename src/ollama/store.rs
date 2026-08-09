//! Ollama's on-disk model store, read directly — the installed-model set
//! without the server.
//!
//! `ollama list` and `/api/tags` are both served from this manifest tree; the
//! server is a reader of it, not the source of truth. Reading it ourselves is
//! what lets the enumeration surfaces (`mermaid list`, `/model`, `status`,
//! `doctor`) answer "what is installed?" while the server is deliberately
//! stopped — those surfaces hard-disable autostart so that *observing* can
//! never resurrect a server the user shut down, and before this module that
//! correctness rule left them blind.
//!
//! Layout, stable across Ollama releases: one small JSON manifest per
//! installed tag at `<models>/manifests/<host>/<namespace>/<model>/<tag>`,
//! e.g. `manifests/registry.ollama.ai/library/gemma4/e4b-it-qat`. Ollama
//! writes the manifest only after the blobs land, so a manifest's presence
//! means a complete model. Registered cloud models (`…:cloud`) have manifests
//! too and list naturally.
//!
//! Consistency with autostart: the server mermaid would start inherits
//! mermaid's environment (`server::spawn_serve`), so the `OLLAMA_MODELS` this
//! module honors names exactly the store that server would serve.

use std::path::{Path, PathBuf};

/// The registry host Ollama elides when displaying model names.
const DEFAULT_REGISTRY: &str = "registry.ollama.ai";
/// The namespace Ollama elides (within the default registry) when displaying
/// model names.
const DEFAULT_NAMESPACE: &str = "library";

/// Size cap consulted before reading a candidate manifest. Real manifests are
/// ~1KB of JSON; anything bigger at tag depth is not a manifest, and must not
/// be slurped into memory to find that out.
const MAX_MANIFEST_BYTES: u64 = 1_000_000;

/// Where the store may live, in precedence order. `OLLAMA_MODELS` — how a
/// user relocates the store, and the variable the server itself honors — wins
/// outright. Otherwise the per-user default, plus (on Linux) the systemd
/// service account's home, where the official install script puts it.
fn store_candidates() -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("OLLAMA_MODELS").filter(|dir| !dir.is_empty()) {
        return vec![PathBuf::from(dir)];
    }
    let mut roots = Vec::new();
    if let Some(dirs) = directories::BaseDirs::new() {
        roots.push(dirs.home_dir().join(".ollama").join("models"));
    }
    if cfg!(target_os = "linux") {
        roots.push(PathBuf::from("/usr/share/ollama/.ollama/models"));
    }
    roots
}

/// Every installed model's display name, sorted — or `None` when no candidate
/// store contains a manifest.
///
/// `None` deliberately covers both "no store" and "empty store": an empty
/// walk cannot be distinguished from walking a location a
/// differently-configured server never used, so callers report the server as
/// unreachable rather than claiming "no models installed". Plain `pub` in a
/// private module — reachable only through `super::observe`.
pub fn installed_models() -> Option<Vec<String>> {
    installed_models_in(&store_candidates())
}

/// [`installed_models`] over explicit roots — the first root whose manifest
/// walk yields anything answers. Split out so tests can point it at fixture
/// trees.
fn installed_models_in(roots: &[PathBuf]) -> Option<Vec<String>> {
    roots
        .iter()
        .map(|root| scan(root))
        .find(|models| !models.is_empty())
}

/// All manifest display names under one store root, sorted. Unreadable or
/// oddly-shaped entries contribute nothing — a stray file can hide a model
/// from `ollama pull`'s own bookkeeping, never from us crashing.
fn scan(root: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for (host, host_dir) in subdirs(&root.join("manifests")) {
        for (namespace, ns_dir) in subdirs(&host_dir) {
            for (model, model_dir) in subdirs(&ns_dir) {
                for (tag, manifest) in files_in(&model_dir) {
                    if is_manifest(&manifest) {
                        names.push(display_name(&host, &namespace, &model, &tag));
                    }
                }
            }
        }
    }
    names.sort();
    names
}

/// Child directories of `dir` as `(name, path)`; empty on any error. Entries
/// whose names are not UTF-8 are skipped — they cannot round-trip into a
/// model id.
fn subdirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .collect()
}

/// Child files of `dir` as `(name, path)`; empty on any error.
fn files_in(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter_map(|entry| Some((entry.file_name().into_string().ok()?, entry.path())))
        .collect()
}

/// Whether `path` holds an OCI-style Ollama manifest (`schemaVersion: 2`) —
/// the sanity gate that keeps editor droppings and torn temp files from
/// being reported as installed models.
fn is_manifest(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() > MAX_MANIFEST_BYTES {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|value| value.get("schemaVersion").and_then(|v| v.as_u64()))
        == Some(2)
}

/// `registry.ollama.ai/library/gemma4/e4b-it-qat` → `gemma4:e4b-it-qat`.
///
/// Matches how Ollama itself shortens names (and therefore how `/api/tags`
/// reports them): the default registry disappears, and within it the
/// `library` namespace does too. Anything else keeps its qualifiers, so two
/// same-named models from different sources cannot collapse into one row.
fn display_name(host: &str, namespace: &str, model: &str, tag: &str) -> String {
    if host != DEFAULT_REGISTRY {
        return format!("{host}/{namespace}/{model}:{tag}");
    }
    if namespace != DEFAULT_NAMESPACE {
        return format!("{namespace}/{model}:{tag}");
    }
    format!("{model}:{tag}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manifest body shaped like the real thing (config + one model layer).
    const MANIFEST: &str = r#"{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{"digest":"sha256:aa","size":545},"layers":[{"mediaType":"application/vnd.ollama.image.model","digest":"sha256:bb","size":5154939136}]}"#;

    struct FixtureStore(PathBuf);

    impl FixtureStore {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("mermaid-ollama-store-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            Self(root)
        }

        fn write(&self, host: &str, namespace: &str, model: &str, tag: &str, body: &str) {
            let dir = self
                .0
                .join("manifests")
                .join(host)
                .join(namespace)
                .join(model);
            std::fs::create_dir_all(&dir).expect("create manifest dir");
            std::fs::write(dir.join(tag), body).expect("write manifest");
        }
    }

    impl Drop for FixtureStore {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The elision rules, against a tree shaped like a real store (this
    /// machine's own layout was the reference): default registry + library
    /// collapse to `model:tag`, a foreign namespace keeps its prefix, a
    /// foreign host keeps everything.
    #[test]
    fn display_names_match_api_tags_shortening() {
        let store = FixtureStore::new("elision");
        store.write(
            "registry.ollama.ai",
            "library",
            "gemma4",
            "e4b-it-qat",
            MANIFEST,
        );
        store.write(
            "registry.ollama.ai",
            "library",
            "nemotron-3-ultra",
            "cloud",
            MANIFEST,
        );
        store.write(
            "registry.ollama.ai",
            "jmorganca",
            "mymodel",
            "latest",
            MANIFEST,
        );
        store.write("hf.co", "someone", "some-gguf", "Q4_K_M", MANIFEST);
        assert_eq!(
            installed_models_in(std::slice::from_ref(&store.0)).expect("models found"),
            vec![
                "gemma4:e4b-it-qat",
                "hf.co/someone/some-gguf:Q4_K_M",
                "jmorganca/mymodel:latest",
                "nemotron-3-ultra:cloud",
            ]
        );
    }

    /// Junk tolerance: only JSON files with `schemaVersion: 2` at exactly tag
    /// depth count. Wrong depth, wrong shape, and non-JSON all contribute
    /// nothing — and never panic.
    #[test]
    fn junk_entries_are_ignored() {
        let store = FixtureStore::new("junk");
        store.write("registry.ollama.ai", "library", "real", "latest", MANIFEST);
        // Non-JSON at tag depth.
        store.write(
            "registry.ollama.ai",
            "library",
            "real",
            ".DS_Store",
            "\0\0junk",
        );
        // Wrong schema version at tag depth.
        store.write(
            "registry.ollama.ai",
            "library",
            "real",
            "v1",
            r#"{"schemaVersion":1}"#,
        );
        // A file at namespace depth (wrong level).
        std::fs::write(
            store
                .0
                .join("manifests")
                .join("registry.ollama.ai")
                .join("stray.json"),
            MANIFEST,
        )
        .expect("write stray");
        // A directory at tag depth (partial download scaffolding).
        std::fs::create_dir_all(
            store
                .0
                .join("manifests")
                .join("registry.ollama.ai")
                .join("library")
                .join("real")
                .join("not-a-file"),
        )
        .expect("create stray dir");
        assert_eq!(
            installed_models_in(std::slice::from_ref(&store.0)),
            Some(vec!["real:latest".to_string()])
        );
    }

    /// Empty and missing stores are `None` — the caller must fall back to
    /// "unreachable", never claim an empty install it cannot prove. The first
    /// candidate root with content answers.
    #[test]
    fn empty_stores_yield_none_and_first_populated_root_wins() {
        let empty = FixtureStore::new("empty");
        std::fs::create_dir_all(empty.0.join("manifests")).expect("create manifests dir");
        let missing = FixtureStore::new("missing");
        let populated = FixtureStore::new("populated");
        populated.write("registry.ollama.ai", "library", "qwen3", "8b", MANIFEST);

        assert_eq!(installed_models_in(std::slice::from_ref(&empty.0)), None);
        assert_eq!(installed_models_in(std::slice::from_ref(&missing.0)), None);
        assert_eq!(
            installed_models_in(&[missing.0.clone(), empty.0.clone(), populated.0.clone()]),
            Some(vec!["qwen3:8b".to_string()])
        );
    }

    /// `OLLAMA_MODELS` replaces the candidate list outright (it is where the
    /// server itself would look), and an empty value means unset.
    #[test]
    fn ollama_models_env_overrides_candidates() {
        temp_env::with_var("OLLAMA_MODELS", Some("/relocated/models"), || {
            assert_eq!(store_candidates(), vec![PathBuf::from("/relocated/models")]);
        });
        temp_env::with_var("OLLAMA_MODELS", Some(""), || {
            assert!(
                store_candidates()
                    .iter()
                    .all(|root| root != &PathBuf::from("")),
                "empty OLLAMA_MODELS must be ignored"
            );
        });
    }
}
