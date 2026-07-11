use super::ApplyPatchTool;
use crate::domain::{ToolCallId, ToolMetadata, ToolOutcome, TurnId};
use crate::providers::ctx::test_exec_context;
use crate::providers::tool::ToolExecutor;
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
async fn path_escape_is_rejected() {
    let dir = tmp("escape");
    let out = run(
        &dir,
        "*** Begin Patch\n*** Add File: ../evil.txt\n+pwned\n*** End Patch",
    )
    .await;
    assert!(!out.is_success(), "path escape must be rejected");
    assert!(!dir.parent().unwrap().join("evil.txt").exists());
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

    let mut config = crate::app::Config::default();
    config.safety.mode = crate::runtime::SafetyMode::Ask;
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    let mut ctx = crate::providers::ctx::ExecContext::new(
        tokio_util::sync::CancellationToken::new(),
        tx,
        ToolCallId(1),
        TurnId(1),
        project.clone(),
        std::sync::Arc::new(config),
        String::new(),
        None,
        None,
        None,
        crate::runtime::SafetyMode::Ask,
        None,
        None,
        None,
        None,
        None,
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

/// Without a scratchpad, an absolute scratch-shaped path stays rejected.
#[tokio::test]
async fn absolute_path_without_scratchpad_is_rejected() {
    let dir = tmp("noscratch");
    let outside = std::env::temp_dir().join("mermaid_applypatch_not_a_root/x.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+pwned\n*** End Patch",
        outside.display()
    );
    let out = run(&dir, &patch).await;
    assert!(!out.is_success(), "outside-root add must be rejected");
    assert!(!outside.exists());
    let _ = fs::remove_dir_all(&dir);
}
