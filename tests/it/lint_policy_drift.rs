//! Drift guard for the lint policy — the same registry-backed truth pattern
//! `readme_drift.rs` uses.
//!
//! The `[lints.clippy]` table is DUPLICATED into all three manifests rather
//! than inherited via `[workspace.lints]` + `lints.workspace = true`. That is
//! not an oversight. The `isolated-crate-build` CI job copies `crates/.` into a
//! directory with no workspace root above it, and `lints.workspace = true` is a
//! hard manifest-parse error there — it would break the very job that exists
//! because a manifest bug half-released v0.21.0. Building a workspace stub for
//! it would reintroduce the feature unification that job exists to prevent.
//!
//! Duplication needs a guard, so here it is.

const ROOT: &str = include_str!("../../Cargo.toml");
const DOMAIN: &str = include_str!("../../crates/mermaid-domain/Cargo.toml");
const MODEL: &str = include_str!("../../crates/mermaid-model/Cargo.toml");
const RUNTIME: &str = include_str!("../../crates/mermaid-runtime/Cargo.toml");
const UI: &str = include_str!("../../crates/mermaid-ui/Cargo.toml");

/// The `[lints.clippy]` table body: everything from the header to the next
/// table header, comments included. Comments are part of the comparison on
/// purpose — a rationale that drifts is as bad as a level that drifts.
fn lints_table(manifest: &str) -> String {
    let manifest = manifest.replace("\r\n", "\n");
    let start = manifest
        .find("[lints.clippy]")
        .expect("manifest has no [lints.clippy] table");
    let body = &manifest[start + "[lints.clippy]".len()..];
    let end = body.find("\n[").map_or(body.len(), |offset| offset + 1);
    body[..end].trim().to_string()
}

#[test]
fn every_crate_carries_the_same_lint_policy() {
    let root = lints_table(ROOT);
    assert!(
        !root.is_empty(),
        "the root manifest's [lints.clippy] table is empty"
    );
    assert_eq!(
        root,
        lints_table(DOMAIN),
        "mermaid-domain's [lints.clippy] table drifted from the root's"
    );
    assert_eq!(
        root,
        lints_table(MODEL),
        "mermaid-model's [lints.clippy] table drifted from the root's"
    );
    assert_eq!(
        root,
        lints_table(RUNTIME),
        "mermaid-runtime's [lints.clippy] table drifted from the root's"
    );
    assert_eq!(
        root,
        lints_table(UI),
        "mermaid-ui's [lints.clippy] table drifted from the root's"
    );
}

/// The cap AGENTS.md names by number. It went years configured-but-unenforced:
/// `.clippy.toml` set the threshold while the lint stayed off, so the doc's
/// claim was false and `update_step` reached 774 lines. Pin both halves.
#[test]
fn the_hundred_line_cap_is_both_configured_and_enabled() {
    const CLIPPY_TOML: &str = include_str!("../../.clippy.toml");
    assert!(
        CLIPPY_TOML.contains("too-many-lines-threshold = 100"),
        ".clippy.toml no longer sets the 100-line threshold AGENTS.md cites"
    );
    for (name, manifest) in [
        ("root", ROOT),
        ("domain", DOMAIN),
        ("model", MODEL),
        ("runtime", RUNTIME),
        ("ui", UI),
    ] {
        assert!(
            lints_table(manifest).contains("too_many_lines = \"deny\""),
            "{name}'s manifest configures the threshold but does not enable \
             the lint — exactly the state that made the AGENTS.md claim false"
        );
    }
}
