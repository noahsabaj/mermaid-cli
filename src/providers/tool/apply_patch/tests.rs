use super::ApplyPatchTool;
use crate::providers::ctx::test_exec_context;
use crate::providers::tool::ToolExecutor;
use mermaid_domain::{ToolCallId, ToolMetadata, ToolOutcome, TurnId};
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mermaid_applypatch_{}_{}",
        name,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

async fn run(dir: &Path, patch: &str) -> ToolOutcome {
    let (ctx, _rx) = test_exec_context(TurnId(1), ToolCallId(1), dir.to_path_buf());
    ApplyPatchTool
        .execute(serde_json::json!({ "patch": patch }), ctx)
        .await
}

#[tokio::test]
async fn add_file_creates_and_reports_a() {
    let dir = tmp("add");
    let out = run(
        &dir,
        "*** Begin Patch\n*** Add File: hello.txt\n+line one\n+line two\n*** End Patch",
    )
    .await;
    assert!(out.is_success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(dir.join("hello.txt")).unwrap(),
        "line one\nline two\n"
    );
    assert!(out.output().contains("A hello.txt"), "{}", out.output());
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn update_file_modifies_and_renders_diff() {
    let dir = tmp("update");
    fs::write(dir.join("m.py"), "alpha\nold\nomega\n").unwrap();
    let out = run(
        &dir,
        "*** Begin Patch\n*** Update File: m.py\n alpha\n-old\n+new\n omega\n*** End Patch",
    )
    .await;
    assert!(out.is_success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(dir.join("m.py")).unwrap(),
        "alpha\nnew\nomega\n"
    );
    let diff = out.metadata.display_diff.as_deref().expect("display diff");
    assert!(diff.contains("- old"), "diff: {diff}");
    assert!(diff.contains("+ new"), "diff: {diff}");
    assert!(!diff.contains("@@"), "no hunk headers: {diff}");
    assert!(matches!(
        out.metadata.detail,
        ToolMetadata::ApplyPatch { .. }
    ));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn delete_file_removes_and_reports_d() {
    let dir = tmp("delete");
    fs::write(dir.join("gone.txt"), "bye").unwrap();
    let out = run(
        &dir,
        "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch",
    )
    .await;
    assert!(out.is_success(), "{out:?}");
    assert!(!dir.join("gone.txt").exists());
    assert!(out.output().contains("D gone.txt"), "{}", out.output());
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn move_renames_file() {
    let dir = tmp("move");
    fs::write(dir.join("old.rs"), "fn a() {}\n").unwrap();
    let out = run(
        &dir,
        "*** Begin Patch\n*** Update File: old.rs\n*** Move to: new.rs\n-fn a() {}\n+fn b() {}\n*** End Patch",
    )
    .await;
    assert!(out.is_success(), "{out:?}");
    assert!(!dir.join("old.rs").exists(), "source must be removed");
    assert_eq!(
        fs::read_to_string(dir.join("new.rs")).unwrap(),
        "fn b() {}\n"
    );
    assert!(
        out.output().contains("R old.rs -> new.rs"),
        "{}",
        out.output()
    );
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn multi_file_patch_is_one_call() {
    let dir = tmp("multi");
    fs::write(dir.join("keep.txt"), "x\n").unwrap();
    let out = run(
        &dir,
        "*** Begin Patch\n*** Add File: created.txt\n+brand new\n*** Update File: keep.txt\n-x\n+y\n*** End Patch",
    )
    .await;
    assert!(out.is_success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(dir.join("created.txt")).unwrap(),
        "brand new\n"
    );
    assert_eq!(fs::read_to_string(dir.join("keep.txt")).unwrap(), "y\n");
    assert!(out.output().contains("A created.txt"));
    assert!(out.output().contains("M keep.txt"));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn whitespace_drift_applies_with_fuzzy_note() {
    let dir = tmp("fuzzy");
    // File has trailing whitespace the patch omits → matches fuzzily.
    fs::write(dir.join("f.txt"), "keep\nold   \ntail\n").unwrap();
    let out = run(
        &dir,
        "*** Begin Patch\n*** Update File: f.txt\n keep\n-old\n+new\n tail\n*** End Patch",
    )
    .await;
    assert!(out.is_success(), "{out:?}");
    assert_eq!(
        fs::read_to_string(dir.join("f.txt")).unwrap(),
        "keep\nnew\ntail\n"
    );
    assert!(
        out.output().contains("fuzzy"),
        "expected a fuzzy-match note: {}",
        out.output()
    );
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn missing_context_is_an_error() {
    let dir = tmp("ctxfail");
    fs::write(dir.join("c.txt"), "a\nb\n").unwrap();
    let out = run(
        &dir,
        "*** Begin Patch\n*** Update File: c.txt\n@@ nonexistent anchor\n-a\n+z\n*** End Patch",
    )
    .await;
    assert!(!out.is_success(), "missing context anchor must fail");
    // The original file is untouched on failure.
    assert_eq!(fs::read_to_string(dir.join("c.txt")).unwrap(), "a\nb\n");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn parent_relative_path_succeeds() {
    let dir = tmp("parent_rel");
    let evil = dir.parent().unwrap().join("evil.txt");
    let _ = fs::remove_file(&evil);
    let out = run(
        &dir,
        "*** Begin Patch\n*** Add File: ../evil.txt\n+pwned\n*** End Patch",
    )
    .await;
    assert!(
        out.is_success(),
        "parent relative path should succeed: {out:?}"
    );
    assert!(evil.exists());
    assert_eq!(fs::read_to_string(&evil).unwrap(), "pwned\n");
    let _ = fs::remove_file(&evil);
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn malformed_patch_is_an_error() {
    let dir = tmp("malformed");
    let out = run(&dir, "not a patch at all").await;
    assert!(!out.is_success(), "a non-patch string must error");
    let _ = fs::remove_dir_all(&dir);
}

/// A patch whose every hunk lands in the session scratchpad is ungated: it
/// applies in Ask mode with NO approval broker bound (the all-hunks-contained
/// flag bypasses the policy gate), writing into the scratch root.
#[tokio::test]
async fn all_scratch_patch_is_ungated() {
    let base = tmp("scratch");
    let project = base.join("project");
    let scratch = base.join("scratch");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&scratch).unwrap();

    let mut config = mermaid_domain::Config::default();
    config.safety.mode = mermaid_runtime::SafetyMode::Ask;
    let (mut ctx, _rx) = crate::providers::ctx::test_exec_context_with_config(
        TurnId(1),
        ToolCallId(1),
        project.clone(),
        config,
    );
    ctx.scratchpad = Some(scratch.clone());

    let target = scratch.join("plan.md");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+scratch plan\n*** End Patch",
        target.display()
    );
    let out = ApplyPatchTool
        .execute(serde_json::json!({ "patch": patch }), ctx)
        .await;
    assert!(out.is_success(), "{out:?}");
    assert_eq!(fs::read_to_string(&target).unwrap(), "scratch plan\n");
    let _ = fs::remove_dir_all(&base);
}

/// Absolute paths outside the project directory succeed and apply patches.
#[tokio::test]
async fn absolute_path_outside_project_succeeds() {
    let dir = tmp("outside_patch");
    let outside_dir = std::env::temp_dir().join("mermaid_applypatch_outside_root");
    let outside = outside_dir.join("x.txt");
    let _ = fs::remove_dir_all(&outside_dir);
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+created outside\n*** End Patch",
        outside.display()
    );
    let out = run(&dir, &patch).await;
    assert!(out.is_success(), "outside-root add should succeed: {out:?}");
    assert!(outside.exists());
    assert_eq!(fs::read_to_string(&outside).unwrap(), "created outside\n");
    let _ = fs::remove_dir_all(&outside_dir);
    let _ = fs::remove_dir_all(&dir);
}

/// A patch containing standard unified diff headers `@@ -l,c +l,c @@` applies cleanly
/// end-to-end through the tool without failing to find context lines.
#[tokio::test]
async fn unified_diff_range_headers_apply_successfully() {
    let dir = tmp("unified_diff");
    let file = dir.join("providers.rs");
    fs::write(
        &file,
        "// line 1\nProviderProfile {\n    name: \"nvidia\",\n    reasoning_strategy: ReasoningStrategy::None,\n}\nProviderProfile {\n    name: \"cloudflare\",\n    reasoning_strategy: ReasoningStrategy::Effort,\n}\n",
    )
    .unwrap();

    let patch = format!(
        "*** Begin Patch\n*** Update File: {}\n@@ -294,28 +294,56 @@\n ProviderProfile {{\n     name: \"nvidia\",\n-    reasoning_strategy: ReasoningStrategy::None,\n+    reasoning_strategy: ReasoningStrategy::Effort,\n }}\n@@ -325,8 +325,36 @@\n ProviderProfile {{\n     name: \"cloudflare\",\n-    reasoning_strategy: ReasoningStrategy::Effort,\n+    reasoning_strategy: ReasoningStrategy::None,\n }}\n*** End Patch",
        file.display()
    );

    let out = run(&dir, &patch).await;
    assert!(
        out.is_success(),
        "unified diff patch should succeed: {out:?}"
    );
    let content = fs::read_to_string(&file).unwrap();
    assert!(
        content.contains("name: \"nvidia\",\n    reasoning_strategy: ReasoningStrategy::Effort,"),
        "content: {content}"
    );
    assert!(
        content.contains("name: \"cloudflare\",\n    reasoning_strategy: ReasoningStrategy::None,"),
        "content: {content}"
    );
    let _ = fs::remove_dir_all(&dir);
}
