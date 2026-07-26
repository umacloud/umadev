use super::*;
use std::process::Command;
use std::time::Duration;
use umadev_agent::{Budget, Depth, RouteClass, TaskKind};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn dirty_git_repo() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(root.path(), &["config", "user.name", "UmaDev Test"]);
    git(
        root.path(),
        &["config", "user.email", "umadev-test@example.invalid"],
    );
    for path in ["one.txt", "two.txt", "three.txt"] {
        std::fs::write(root.path().join(path), "before\n").unwrap();
    }
    git(root.path(), &["add", "one.txt", "two.txt", "three.txt"]);
    git(root.path(), &["commit", "-q", "-m", "initial"]);
    for path in ["one.txt", "two.txt", "three.txt"] {
        std::fs::write(root.path().join(path), "ready to commit\n").unwrap();
    }
    root
}

fn route(class: RouteClass, depth: Depth, scope: &[&str]) -> RoutePlan {
    RoutePlan {
        class,
        kind: TaskKind::Light,
        depth,
        team: Vec::new(),
        scope: scope.iter().map(|path| (*path).to_string()).collect(),
        needs_clarify: None,
        est_budget: Budget::for_route(class, depth),
        confidence: 1.0,
    }
}

#[test]
fn final_diff_contains_base_and_later_umadev_execution_writes() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::create_dir_all(root.path().join("tests")).unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::Build, Depth::Fast, &["src/", "tests/"]),
        "implement and verify",
    )
    .unwrap();

    // The selected base writes during the resident turn.
    std::fs::write(root.path().join("src/base.rs"), "base").unwrap();
    assert_eq!(
        postcondition.changed_paths(root.path()).unwrap(),
        ["src/base.rs"]
    );
    // A later UmaDev-owned verifier/QC turn also writes before settlement.
    std::fs::write(root.path().join("tests/qc.rs"), "qc").unwrap();
    assert_eq!(
        postcondition.validate_final(root.path()).unwrap(),
        ["src/base.rs", "tests/qc.rs"]
    );
}

#[test]
fn out_of_scope_final_write_is_blocking_not_success() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["src/title.rs"]),
        "change the title",
    )
    .unwrap();
    std::fs::write(root.path().join("src/title.rs"), "allowed").unwrap();
    std::fs::write(root.path().join("package.json"), "{}").unwrap();

    let blocked = postcondition
        .validate_final(root.path())
        .expect_err("an out-of-scope/sensitive write cannot settle successfully")
        .into_note();
    assert!(blocked.contains("[blocked]"));
    assert!(blocked.contains("execution-path-out-of-scope"));
    assert!(blocked.contains("package.json"));
    assert!(!blocked.contains("[ok]"));
}

#[test]
fn quick_edit_change_budget_is_enforced_over_actual_content_diff() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["src/"]),
        "small edit",
    )
    .unwrap();
    for index in 0..5 {
        std::fs::write(root.path().join(format!("src/{index}.rs")), "x").unwrap();
    }
    let note = postcondition
        .validate_final(root.path())
        .unwrap_err()
        .into_note();
    assert!(note.contains("execution-change-budget-exceeded"));
}

#[test]
fn capture_failure_is_explicitly_unverified() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing");
    let note = ResidentExecutionPostcondition::capture(
        &missing,
        &route(RouteClass::Debug, Depth::Fast, &[]),
        "debug",
    )
    .unwrap_err()
    .into_note();
    assert!(note.contains("[blocked]"));
    assert!(note.contains("cannot be marked successful"));
}

#[tokio::test]
async fn git_commit_only_accepts_one_commit_of_three_preexisting_dirty_files() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(
            RouteClass::QuickEdit,
            Depth::Fast,
            &["one.txt", "two.txt", "three.txt"],
        ),
        "提交git记录",
    )
    .unwrap();
    let receipt = postcondition
        .execute_git_commit(root.path(), "提交git记录")
        .await
        .unwrap();
    assert_eq!(receipt.paths, ["one.txt", "three.txt", "two.txt"]);
}

#[tokio::test]
async fn git_commit_only_honors_an_exact_subset_scope() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        r#"提交git记录 "one.txt""#,
    )
    .unwrap();

    let receipt = postcondition
        .execute_git_commit(root.path(), r#"提交git记录 "one.txt""#)
        .await
        .unwrap();

    assert_eq!(receipt.paths, ["one.txt"]);
    assert_eq!(
        git_dirty_paths(root.path()).unwrap(),
        BTreeSet::from(["three.txt".to_string(), "two.txt".to_string()])
    );
}

#[tokio::test]
async fn git_commit_only_honors_a_quoted_multibyte_subset_scope() {
    let root = dirty_git_repo();
    std::fs::create_dir_all(root.path().join("docs")).unwrap();
    let selected = "docs/中文 文件.md";
    std::fs::write(root.path().join(selected), "selected\n").unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[selected]),
        r#"提交git记录 "docs/中文 文件.md""#,
    )
    .unwrap();

    let receipt = postcondition
        .execute_git_commit(root.path(), r#"提交git记录 "docs/中文 文件.md""#)
        .await
        .unwrap();

    assert_eq!(receipt.paths, [selected]);
    assert_eq!(
        git_dirty_paths(root.path()).unwrap(),
        BTreeSet::from([
            "one.txt".to_string(),
            "three.txt".to_string(),
            "two.txt".to_string()
        ])
    );
}

#[tokio::test]
async fn git_commit_only_without_scope_accepts_more_than_four_dirty_files_and_package_manifest() {
    let root = dirty_git_repo();
    for path in ["four.txt", "five.txt", "package.json"] {
        std::fs::write(root.path().join(path), "ready to commit\n").unwrap();
    }
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[]),
        "提交当前改动",
    )
    .unwrap();
    assert_eq!(
        postcondition
            .execute_git_commit(root.path(), "提交当前改动")
            .await
            .unwrap()
            .paths,
        [
            "five.txt",
            "four.txt",
            "one.txt",
            "package.json",
            "three.txt",
            "two.txt"
        ]
    );
}

#[tokio::test]
async fn literal_git_commit_preserves_native_staged_only_semantics() {
    let root = dirty_git_repo();
    git(root.path(), &["add", "one.txt"]);
    let old_head = git_required_text(root.path(), &["rev-parse", "HEAD"], "test").unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[]),
        "git commit -m \"record staged\"",
    )
    .unwrap();

    let receipt = postcondition
        .execute_git_commit(root.path(), "git commit -m \"record staged\"")
        .await
        .unwrap();

    assert_eq!(receipt.paths, ["one.txt"]);
    assert_eq!(
        git_count(root.path(), &format!("{old_head}..HEAD")).unwrap(),
        1
    );
    assert!(git_staged_paths(root.path()).unwrap().is_empty());
    let dirty = git_dirty_paths(root.path()).unwrap();
    assert!(dirty.contains("two.txt"));
    assert!(dirty.contains("three.txt"));
    assert!(!dirty.contains("one.txt"));
}

#[test]
fn git_commit_only_without_scope_blocks_credentials_before_execution() {
    let root = dirty_git_repo();
    std::fs::write(root.path().join(".env"), "TOKEN=secret\n").unwrap();

    let note = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[]),
        "提交当前改动",
    )
    .unwrap_err()
    .into_note();

    assert!(note.contains("git-sensitive-path-blocked"));
    assert!(note.contains(".env"));
    assert!(
        git_optional_text(root.path(), &["rev-parse", "--verify", "HEAD"])
            .unwrap()
            .is_some()
    );
    assert_eq!(git_count(root.path(), "HEAD").unwrap(), 1);
}

#[test]
fn bare_commit_blocks_common_credential_and_keystore_paths() {
    for path in [
        ".git-credentials",
        ".docker/config.json",
        ".kube/config",
        "release.jks",
        "secrets.toml",
    ] {
        let root = dirty_git_repo();
        let target = root.path().join(path);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "credential material\n").unwrap();
        let note = ResidentExecutionPostcondition::capture(
            root.path(),
            &route(RouteClass::QuickEdit, Depth::Fast, &[]),
            "提交当前改动",
        )
        .unwrap_err()
        .into_note();
        assert!(
            note.contains("git-sensitive-path-blocked"),
            "{path}: {note}"
        );
    }
}

#[tokio::test]
async fn bare_commit_excludes_untracked_umadev_runtime_state() {
    let root = dirty_git_repo();
    std::fs::create_dir_all(root.path().join(".umadev")).unwrap();
    std::fs::write(root.path().join(".umadev/state.json"), "{}").unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[]),
        "提交当前改动",
    )
    .unwrap();
    let receipt = postcondition
        .execute_git_commit(root.path(), "提交当前改动")
        .await
        .unwrap();
    assert!(!receipt
        .paths
        .iter()
        .any(|path| path.starts_with(".umadev/")));
    assert!(root.path().join(".umadev/state.json").exists());
}

#[test]
fn literal_commit_rejects_staged_umadev_runtime_state() {
    let root = dirty_git_repo();
    std::fs::create_dir_all(root.path().join(".umadev")).unwrap();
    std::fs::write(root.path().join(".umadev/state.json"), "{}").unwrap();
    git(root.path(), &["add", "-f", ".umadev/state.json"]);
    let note = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[]),
        "git commit -m runtime",
    )
    .unwrap_err()
    .into_note();
    assert!(note.contains("git-internal-path-blocked"), "{note}");
}

#[test]
fn git_commit_only_explicit_scope_rejects_named_sensitive_path() {
    let root = dirty_git_repo();
    std::fs::write(root.path().join(".env"), "TOKEN=secret\n").unwrap();
    let note = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[".env"]),
        "提交当前改动",
    )
    .unwrap_err()
    .into_note();
    assert!(note.contains("git-sensitive-path-blocked"));
    assert!(note.contains(".env"));
}

#[tokio::test]
async fn git_commit_only_safe_explicit_scope_leaves_sensitive_dirty_path_untouched() {
    let root = dirty_git_repo();
    std::fs::write(root.path().join(".env"), "TOKEN=secret\n").unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();

    let receipt = postcondition
        .execute_git_commit(root.path(), "提交git记录: one.txt")
        .await
        .unwrap();
    assert_eq!(receipt.paths, ["one.txt"]);
    assert!(git_dirty_paths(root.path()).unwrap().contains(".env"));
}

#[test]
fn git_commit_only_invalid_scope_does_not_bypass_credential_preflight() {
    let root = dirty_git_repo();
    std::fs::write(root.path().join(".env"), "TOKEN=secret\n").unwrap();

    let note = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["../.env"]),
        "提交git记录",
    )
    .unwrap_err()
    .into_note();

    assert!(
        note.contains("git-scope-not-exact-dirty-path")
            || note.contains("git-sensitive-path-blocked")
    );
}

#[test]
fn git_commit_only_explicit_scope_rejects_unrequested_dirty_path() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    git(root.path(), &["add", "one.txt", "two.txt"]);
    git(root.path(), &["commit", "-q", "-m", "record too much"]);

    let note = postcondition
        .validate_final(root.path())
        .unwrap_err()
        .into_note();
    assert!(note.contains("git-commit-path-set-mismatch"));
    assert!(note.contains("two.txt"));
}

#[tokio::test]
async fn host_commit_preserves_preexisting_staged_files_outside_explicit_scope() {
    let root = dirty_git_repo();
    git(root.path(), &["add", "two.txt"]);
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();

    let receipt = postcondition
        .execute_git_commit(root.path(), "提交信息：fix #123")
        .await
        .unwrap();
    assert_eq!(receipt.paths, ["one.txt"]);
    let staged = git_name_only(root.path(), &["diff", "--cached", "--name-only", "-z"]).unwrap();
    assert_eq!(staged, ["two.txt"]);
    assert_eq!(
        git_required_text(root.path(), &["log", "-1", "--pretty=%s"], "test").unwrap(),
        "fix #123"
    );
}

#[test]
fn dangerous_or_history_rewriting_commit_options_are_rejected() {
    for request in [
        "git commit --amend -m x",
        "git commit --fixup=HEAD",
        "git commit --squash HEAD",
        "git commit --allow-empty -m x",
        "git commit -a -m x",
        "git commit --all -m x",
        "git commit --no-verify -m x",
    ] {
        let note = git_commit_message(request, true).unwrap_err().into_note();
        assert!(
            note.contains("git-commit-option-forbidden"),
            "{request}: {note}"
        );
    }
    assert_eq!(
        git_commit_message("git commit -m 'fix #123'", true).unwrap(),
        "fix #123"
    );
    assert_eq!(
        git_commit_message("git commit -m 'document --amend behavior'", true).unwrap(),
        "document --amend behavior"
    );
    assert_eq!(
        git_commit_message("提交信息: document git commit --amend behavior", false,).unwrap(),
        "document git commit --amend behavior"
    );
    assert!(
        git_commit_message(&format!("git commit -m '{}'", "x".repeat(4_097)), true,)
            .unwrap_err()
            .into_note()
            .contains("git-commit-message-too-long")
    );
    assert!(git_commit_message("git commit -m 'bad\u{0}message'", true)
        .unwrap_err()
        .into_note()
        .contains("git-commit-message-control-character"));
}

#[test]
fn git_commit_only_rejects_two_new_commits() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    git(root.path(), &["add", "one.txt"]);
    git(root.path(), &["commit", "-q", "-m", "first"]);
    std::fs::write(root.path().join("two.txt"), "changed again\n").unwrap();
    git(root.path(), &["add", "two.txt"]);
    git(root.path(), &["commit", "-q", "-m", "second"]);

    let note = postcondition
        .validate_final(root.path())
        .unwrap_err()
        .into_note();
    assert!(note.contains("git-commit-count-invalid"));
}

#[test]
fn git_commit_only_rejects_a_branch_switch() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    git(root.path(), &["switch", "-q", "-c", "other"]);
    git(root.path(), &["add", "one.txt"]);
    git(root.path(), &["commit", "-q", "-m", "wrong branch"]);

    let note = postcondition
        .validate_final(root.path())
        .unwrap_err()
        .into_note();
    assert!(note.contains("git-branch-changed"));
}

#[test]
fn git_commit_only_rejects_a_commit_tree_that_differs_from_the_frozen_tree() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    git(root.path(), &["add", "one.txt"]);
    git(
        root.path(),
        &[
            "commit",
            "-q",
            "--only",
            "-m",
            "record one",
            "--",
            "one.txt",
        ],
    );
    let after = git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();

    let note = baseline
        .validate(
            root.path(),
            &postcondition.baseline,
            &postcondition.contract,
            Some(&after),
            Some("0000000000000000000000000000000000000000"),
        )
        .unwrap_err()
        .into_note();

    assert!(note.contains("git-commit-tree-mismatch"), "{note}");
}

#[test]
fn git_commit_only_rejects_detached_head_before_execution() {
    let root = dirty_git_repo();
    git(root.path(), &["checkout", "-q", "--detach"]);

    let note = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap_err()
    .into_note();
    assert!(note.contains("git-detached-head"));
}

#[test]
fn git_commit_only_requires_the_project_root_to_equal_the_worktree_root() {
    let root = dirty_git_repo();
    let nested = root.path().join("nested");
    std::fs::create_dir(&nested).unwrap();

    let note = ResidentExecutionPostcondition::capture(
        &nested,
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap_err()
    .into_note();

    assert!(note.contains("git-worktree-root-mismatch"), "{note}");
}

#[cfg(unix)]
fn install_active_hook(root: &Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;

    let hook = git_required_text(
        root,
        &["rev-parse", "--git-path", &format!("hooks/{name}")],
        "test-hook-path",
    )
    .unwrap();
    let hook = PathBuf::from(hook);
    let hook = if hook.is_absolute() {
        hook
    } else {
        root.join(hook)
    };
    std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf executed > umadev-hook-executed\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
fn install_malicious_git_program(root: &Path, name: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let marker = root.join(format!("{name}-executed"));
    let program = root.join(format!("{name}.sh"));
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf executed > '{}'\nmkdir -p dist/build\nprintf injected > 'dist/build/{name}-injected.js'\ncat\n",
            marker.to_string_lossy().replace('\'', "'\\''"),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    (program, marker)
}

#[cfg(unix)]
fn assert_hook_preflight_preserves_exact_state(root: &Path, name: &str) {
    let head = git_required_text(root, &["rev-parse", "HEAD"], "test-head").unwrap();
    let index_path =
        git_required_text(root, &["rev-parse", "--git-path", "index"], "test-index").unwrap();
    let index_path = PathBuf::from(index_path);
    let index_path = if index_path.is_absolute() {
        index_path
    } else {
        root.join(index_path)
    };
    let index = std::fs::read(&index_path).unwrap();
    let workspace = WorkspaceBaseline::capture(root).unwrap();

    let note = ResidentExecutionPostcondition::capture(
        root,
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap_err()
    .into_note();

    assert!(
        note.contains("git-active-hooks-require-native-git"),
        "{note}"
    );
    assert!(note.contains(name), "{note}");
    assert!(note.contains("原生 Git"), "{note}");
    assert_eq!(
        git_required_text(root, &["rev-parse", "HEAD"], "test-head").unwrap(),
        head
    );
    assert_eq!(std::fs::read(index_path).unwrap(), index);
    assert!(workspace.changed_paths(root).unwrap().is_empty());
    assert!(
        !root.join("umadev-hook-executed").exists(),
        "preflight must execute zero hooks"
    );
}

#[cfg(unix)]
#[test]
fn pre_commit_hook_is_rejected_before_any_git_mutation() {
    let root = dirty_git_repo();
    install_active_hook(root.path(), "pre-commit");
    assert_hook_preflight_preserves_exact_state(root.path(), "pre-commit");
}

#[cfg(unix)]
#[test]
fn prepare_commit_message_hook_is_rejected_before_any_git_mutation() {
    let root = dirty_git_repo();
    install_active_hook(root.path(), "prepare-commit-msg");
    assert_hook_preflight_preserves_exact_state(root.path(), "prepare-commit-msg");
}

#[cfg(unix)]
#[test]
fn commit_message_hook_is_rejected_before_any_git_mutation() {
    let root = dirty_git_repo();
    install_active_hook(root.path(), "commit-msg");
    assert_hook_preflight_preserves_exact_state(root.path(), "commit-msg");
}

#[cfg(unix)]
#[test]
fn post_commit_hook_is_rejected_before_any_git_mutation() {
    let root = dirty_git_repo();
    install_active_hook(root.path(), "post-commit");
    assert_hook_preflight_preserves_exact_state(root.path(), "post-commit");
}

#[cfg(unix)]
#[test]
fn reference_transaction_hook_is_rejected_before_any_git_mutation() {
    for name in ["reference-transaction", "post-index-change", "pre-auto-gc"] {
        let root = dirty_git_repo();
        install_active_hook(root.path(), name);
        assert_hook_preflight_preserves_exact_state(root.path(), name);
    }
}

#[cfg(unix)]
#[test]
fn effective_core_hooks_path_is_resolved_and_rejected() {
    let root = dirty_git_repo();
    git(root.path(), &["config", "core.hooksPath", "custom-hooks"]);
    install_active_hook(root.path(), "pre-commit");
    assert_hook_preflight_preserves_exact_state(root.path(), "pre-commit");
}

#[cfg(unix)]
#[tokio::test]
async fn mutation_commands_use_an_inert_hooks_directory_even_after_preflight() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();

    // Model a hook appearing after the last preflight. `execute` is called
    // directly so this test specifically proves the mutation command boundary,
    // independently of the earlier active-hook rejection.
    install_active_hook(root.path(), "pre-commit");
    let mut transaction = GitTransactionGuard::new(root.path(), baseline);
    let commit = baseline
        .execute(
            root.path(),
            "提交git记录: one.txt",
            Duration::from_secs(5),
            &mut transaction,
        )
        .await
        .unwrap();
    transaction.disarm();

    assert_eq!(
        git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap(),
        commit
    );
    assert!(
        !root.path().join("umadev-hook-executed").exists(),
        "the late hook must not run during host-owned git add/commit"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn validation_rollback_also_uses_an_inert_hooks_directory() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();
    let before = baseline.head.clone().unwrap();
    let mut transaction = GitTransactionGuard::new(root.path(), baseline);
    let created = baseline
        .execute(
            root.path(),
            "提交git记录: one.txt",
            Duration::from_secs(5),
            &mut transaction,
        )
        .await
        .unwrap();

    install_active_hook(root.path(), "reference-transaction");
    let validation = git_commit_blocked("test-validation", "force rollback");
    let result = baseline
        .rollback_after_validation(
            root.path(),
            &created,
            validation,
            Duration::from_secs(5),
            &mut transaction,
        )
        .await;
    transaction.disarm();

    assert!(matches!(result, GitValidationRollback::Recovered(_)));
    assert_eq!(
        git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap(),
        before
    );
    assert!(
        !root.path().join("umadev-hook-executed").exists(),
        "a late reference-transaction hook must not run during rollback"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn malicious_clean_filter_is_not_executed_or_allowed_to_widen_the_commit() {
    let root = dirty_git_repo();
    std::fs::write(root.path().join(".gitattributes"), "one.txt filter=evil\n").unwrap();
    git(root.path(), &["add", ".gitattributes"]);
    git(root.path(), &["commit", "-q", "-m", "attributes"]);
    let (program, marker) = install_malicious_git_program(root.path(), "clean-filter");
    git(
        root.path(),
        &[
            "config",
            "filter.evil.clean",
            program.to_string_lossy().as_ref(),
        ],
    );
    git(
        root.path(),
        &[
            "config",
            "filter.evil.smudge",
            program.to_string_lossy().as_ref(),
        ],
    );
    git(
        root.path(),
        &[
            "config",
            "filter.evil.process",
            program.to_string_lossy().as_ref(),
        ],
    );
    git(root.path(), &["config", "filter.evil.required", "true"]);
    std::fs::create_dir_all(root.path().join("dist/build")).unwrap();
    std::fs::write(root.path().join("dist/build/unrelated.js"), "unrelated\n").unwrap();
    assert!(!marker.exists(), "test setup must not execute the filter");
    let before = git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap();

    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let note = postcondition
        .execute_git_commit(root.path(), "提交git记录: one.txt")
        .await
        .unwrap_err()
        .into_note();

    assert!(
        note.contains("git-content-transformation-blocked"),
        "{note}"
    );
    assert!(!marker.exists(), "the configured clean filter must not run");
    assert_eq!(
        git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap(),
        before
    );
    assert!(
        root.path().join("dist/build/unrelated.js").exists(),
        "an unrelated build artifact stays untouched"
    );
    assert!(
        !root
            .path()
            .join("dist/build/clean-filter-injected.js")
            .exists(),
        "the filter cannot create an unrelated build artifact"
    );
    assert!(
        git_dirty_paths(root.path())
            .unwrap()
            .contains("dist/build/unrelated.js"),
        "the unrelated build artifact remains outside the commit"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn malicious_fsmonitor_and_signing_programs_are_not_executed() {
    let root = dirty_git_repo();
    let (fsmonitor, fsmonitor_marker) = install_malicious_git_program(root.path(), "fsmonitor");
    let (signer, signer_marker) = install_malicious_git_program(root.path(), "signer");
    git(
        root.path(),
        &[
            "config",
            "core.fsmonitor",
            fsmonitor.to_string_lossy().as_ref(),
        ],
    );
    git(root.path(), &["config", "core.fsmonitorHookVersion", "2"]);
    git(root.path(), &["config", "commit.gpgSign", "true"]);
    git(
        root.path(),
        &["config", "gpg.program", signer.to_string_lossy().as_ref()],
    );
    assert!(
        !fsmonitor_marker.exists() && !signer_marker.exists(),
        "test setup must not execute configured programs"
    );

    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let receipt = postcondition
        .execute_git_commit(root.path(), "提交git记录: one.txt")
        .await
        .unwrap();

    assert_eq!(receipt.paths, ["one.txt"]);
    assert!(
        !fsmonitor_marker.exists(),
        "the configured fsmonitor program must not run"
    );
    assert!(
        !signer_marker.exists(),
        "the configured signing program must not run"
    );
}

#[tokio::test]
async fn content_transforming_attributes_fail_closed_without_creating_a_commit() {
    let root = dirty_git_repo();
    std::fs::write(root.path().join(".gitattributes"), "one.txt text eol=lf\n").unwrap();
    git(root.path(), &["add", ".gitattributes"]);
    git(root.path(), &["commit", "-q", "-m", "attributes"]);
    std::fs::write(root.path().join("one.txt"), b"ready\r\nto commit\r\n").unwrap();
    let before = git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap();

    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let note = postcondition
        .execute_git_commit(root.path(), "提交git记录: one.txt")
        .await
        .unwrap_err()
        .into_note();

    assert!(
        note.contains("git-content-transformation-blocked"),
        "{note}"
    );
    assert_eq!(
        git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap(),
        before
    );
    assert_eq!(
        std::fs::read(root.path().join("one.txt")).unwrap(),
        b"ready\r\nto commit\r\n"
    );
}

#[tokio::test]
async fn staging_preparation_failure_never_partially_stages_an_earlier_path() {
    let root = dirty_git_repo();
    std::fs::create_dir(root.path().join("unsupported-directory")).unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();
    let mut transaction = GitTransactionGuard::new(root.path(), baseline);
    let hooks = InertHooksDirectory::create().unwrap();

    let note = stage_paths_without_filters(
        root.path(),
        &["one.txt", "unsupported-directory"],
        &hooks.config_value(),
        Duration::from_secs(5),
        &mut transaction,
    )
    .await
    .unwrap_err()
    .into_note();

    assert!(note.contains("git-path-type-unsupported"), "{note}");
    assert!(
        baseline.index.matches_current().unwrap(),
        "a later preparation failure must leave the real index byte-for-byte unchanged"
    );
    assert!(
        git_staged_paths(root.path()).unwrap().is_empty(),
        "the successfully hashed first path must never be staged"
    );
    transaction.disarm();
}

#[tokio::test]
async fn failed_batch_index_update_leaves_no_partial_staging() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt", "two.txt"]),
        "提交git记录",
    )
    .unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();
    let mut transaction = GitTransactionGuard::new(root.path(), baseline);
    let hooks = InertHooksDirectory::create().unwrap();
    let mut lock_name = baseline.index.path.as_os_str().to_os_string();
    lock_name.push(".lock");
    let lock_path = PathBuf::from(lock_name);
    std::fs::write(&lock_path, b"force update-index lock failure").unwrap();

    let note = stage_paths_without_filters(
        root.path(),
        &["one.txt", "two.txt"],
        &hooks.config_value(),
        Duration::from_secs(5),
        &mut transaction,
    )
    .await
    .unwrap_err()
    .into_note();

    assert!(note.contains("git-index-update-failed"), "{note}");
    assert!(
        baseline.index.matches_current().unwrap(),
        "a rejected update-index batch must leave the real index byte-for-byte unchanged"
    );
    std::fs::remove_file(lock_path).unwrap();
    assert!(git_staged_paths(root.path()).unwrap().is_empty());
    transaction.disarm();
}

#[tokio::test]
async fn batch_index_info_commits_modifications_and_deletions_together() {
    let root = dirty_git_repo();
    std::fs::remove_file(root.path().join("two.txt")).unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt", "two.txt"]),
        "提交git记录",
    )
    .unwrap();

    let receipt = postcondition
        .execute_git_commit(root.path(), "提交git记录")
        .await
        .unwrap();

    assert_eq!(receipt.paths, ["one.txt", "two.txt"]);
    assert_eq!(
        git_required_text(root.path(), &["show", "HEAD:one.txt"], "test-one").unwrap(),
        "ready to commit"
    );
    assert!(
        !git_output(root.path(), &["cat-file", "-e", "HEAD:two.txt"])
            .unwrap()
            .status
            .success(),
        "the zero object id record must delete the selected path"
    );
    assert_eq!(
        git_dirty_paths(root.path()).unwrap(),
        BTreeSet::from(["three.txt".to_string()])
    );
}

#[tokio::test]
async fn batch_index_info_preserves_split_index_semantics() {
    let root = dirty_git_repo();
    git(root.path(), &["update-index", "--split-index"]);
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();

    let receipt = postcondition
        .execute_git_commit(root.path(), "提交git记录: one.txt")
        .await
        .unwrap();

    assert_eq!(receipt.paths, ["one.txt"]);
    assert!(git_staged_paths(root.path()).unwrap().is_empty());
    assert_eq!(
        git_dirty_paths(root.path()).unwrap(),
        BTreeSet::from(["three.txt".to_string(), "two.txt".to_string()])
    );
}

#[cfg(unix)]
#[tokio::test]
async fn host_commit_hashes_a_symlink_target_without_following_it() {
    use std::os::unix::fs::symlink;

    let root = dirty_git_repo();
    let outside = tempfile::TempDir::new().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "must-not-enter-the-commit\n").unwrap();
    symlink(&secret, root.path().join("link.txt")).unwrap();

    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["link.txt"]),
        "提交git记录: link.txt",
    )
    .unwrap();
    let receipt = postcondition
        .execute_git_commit(root.path(), "提交git记录: link.txt")
        .await
        .unwrap();

    assert_eq!(receipt.paths, ["link.txt"]);
    let entry = git_required_text(
        root.path(),
        &["ls-tree", "HEAD", "--", "link.txt"],
        "test-tree",
    )
    .unwrap();
    assert!(entry.starts_with("120000 blob "), "{entry}");
    let blob = git_required_text(
        root.path(),
        &["cat-file", "-p", "HEAD:link.txt"],
        "test-link-blob",
    )
    .unwrap();
    assert_eq!(blob, secret.to_string_lossy());
    assert_ne!(blob, "must-not-enter-the-commit");
}

#[cfg(unix)]
#[tokio::test]
async fn process_timeout_and_bounded_output_helpers_work_without_hooks() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();

    let mut noisy_transaction = GitTransactionGuard::new(root.path(), baseline);
    let noisy = git_mutating_output(
        root.path(),
        &[
            "-c",
            "alias.umadev-noisy=!yes 0123456789abcdef | head -c 4194304 >&2; exit 1",
            "umadev-noisy",
        ],
        &[],
        Duration::from_secs(5),
        "test-noisy-timeout",
        "git noisy helper",
        &mut noisy_transaction,
    )
    .await
    .unwrap_err();
    noisy_transaction.disarm();
    assert!(noisy.note.contains("git-output-limit-exceeded"));

    let mut timeout_transaction = GitTransactionGuard::new(root.path(), baseline);
    let started = std::time::Instant::now();
    let timeout = git_mutating_output(
        root.path(),
        &["-c", "alias.umadev-sleep=!sleep 30", "umadev-sleep"],
        &[],
        Duration::from_millis(150),
        "test-process-timeout",
        "git sleep helper",
        &mut timeout_transaction,
    )
    .await
    .unwrap_err();
    let note = timeout_transaction.finish_failure(timeout).into_note();
    assert!(note.contains("test-process-timeout"), "{note}");
    assert!(started.elapsed() < Duration::from_secs(5));

    let marker = root.path().join(".descendant-survived");
    let mut descendant_transaction = GitTransactionGuard::new(root.path(), baseline);
    let timeout = git_mutating_output(
        root.path(),
        &[
            "-c",
            "alias.umadev-descendant=!(sleep 1; : > .descendant-survived) & sleep 30",
            "umadev-descendant",
        ],
        &[],
        Duration::from_millis(150),
        "test-descendant-timeout",
        "git descendant helper",
        &mut descendant_transaction,
    )
    .await
    .unwrap_err();
    let note = descendant_transaction.finish_failure(timeout).into_note();
    assert!(note.contains("test-descendant-timeout"), "{note}");
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    assert!(!marker.exists(), "Git descendant survived timeout cleanup");
}

#[cfg(unix)]
#[tokio::test]
async fn transaction_cas_helper_rolls_back_owned_commit_and_exact_index() {
    let root = dirty_git_repo();
    git(root.path(), &["add", "two.txt"]);
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();
    let old_head = baseline.head.clone().unwrap();
    let mut transaction = GitTransactionGuard::new(root.path(), baseline);
    let paths = ["one.txt"];

    let add = git_mutating_output(
        root.path(),
        &["add", "--"],
        &paths,
        Duration::from_secs(5),
        "test-add-timeout",
        "git add",
        &mut transaction,
    )
    .await
    .unwrap();
    assert!(add.status.success());
    transaction.observe_current_index().unwrap();
    transaction.expected_tree = Some(expected_commit_tree(root.path(), baseline, &paths).unwrap());
    let commit = git_mutating_output(
        root.path(),
        &["commit", "--only", "-m", "test owned commit", "--"],
        &paths,
        Duration::from_secs(5),
        "test-commit-timeout",
        "git commit",
        &mut transaction,
    )
    .await
    .unwrap();
    assert!(commit.status.success());
    transaction.observe_current_index().unwrap();
    assert_ne!(
        git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap(),
        old_head
    );

    transaction.rollback_owned_head_sync().unwrap();
    transaction.restore_original_index_guarded().unwrap();
    transaction.disarm();
    assert_eq!(
        git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap(),
        old_head
    );
    baseline.index.verify_unchanged().unwrap();
    assert_eq!(
        std::fs::read_to_string(root.path().join("one.txt")).unwrap(),
        "ready to commit\n"
    );
}

#[test]
fn literal_expected_tree_is_frozen_from_captured_index_snapshot() {
    let root = dirty_git_repo();
    git(root.path(), &["add", "one.txt"]);
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[]),
        "git commit -m frozen",
    )
    .unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();
    assert!(baseline.staged_only);

    let frozen = expected_commit_tree(root.path(), baseline, &[]).unwrap();
    let raced_content = root.path().join(".raced-index-content");
    std::fs::write(&raced_content, "concurrent index replacement\n").unwrap();
    let raced_blob = git_required_text(
        root.path(),
        &[
            "hash-object",
            "-w",
            raced_content.to_str().expect("UTF-8 test path"),
        ],
        "test-hash-object",
    )
    .unwrap();
    git(
        root.path(),
        &[
            "update-index",
            "--cacheinfo",
            "100644",
            &raced_blob,
            "one.txt",
        ],
    );

    let live = git_required_text(root.path(), &["write-tree"], "test-live-tree").unwrap();
    assert_ne!(live, frozen, "the live index must model a TOCTOU race");
    assert_eq!(
        expected_commit_tree(root.path(), baseline, &[]).unwrap(),
        frozen,
        "expected staged tree must continue to come from the captured index bytes"
    );
}

#[tokio::test]
async fn rollback_preserves_external_same_tree_single_parent_commit() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录: one.txt",
    )
    .unwrap();
    let baseline = postcondition.git_commit.as_ref().unwrap();
    let mut transaction = GitTransactionGuard::new(root.path(), baseline);
    transaction.expected_tree =
        Some(expected_commit_tree(root.path(), baseline, &["one.txt"]).unwrap());
    let before = baseline.head.clone().unwrap();
    let reference = baseline.symbolic_head.clone().unwrap();

    // This external commit has exactly the parent and tree that the old
    // heuristic accepted as transaction-owned. It deliberately lacks this
    // guard's private reflog action marker.
    let expected_tree = transaction.expected_tree.clone().unwrap();
    let external = git_required_text(
        root.path(),
        &[
            "commit-tree",
            &expected_tree,
            "-p",
            &before,
            "-m",
            "external same-tree commit",
        ],
        "test-external-commit",
    )
    .unwrap();
    git(root.path(), &["update-ref", &reference, &external, &before]);
    assert_eq!(
        git_required_text(
            root.path(),
            &["rev-parse", "HEAD^{tree}"],
            "test-external-tree"
        )
        .unwrap(),
        expected_tree
    );

    let validation = git_commit_blocked("test-validation", "force rollback");
    let async_result = baseline
        .rollback_after_validation(
            root.path(),
            &external,
            validation,
            Duration::from_secs(5),
            &mut transaction,
        )
        .await;
    match async_result {
        GitValidationRollback::NeedsFallback { rollback, .. } => {
            assert!(
                rollback.note.contains("git-transaction-ownership-unproven"),
                "{}",
                rollback.note
            );
        }
        GitValidationRollback::Recovered(_) => {
            panic!("an external same-tree commit must never be rolled back")
        }
    }
    assert_eq!(
        git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap(),
        external
    );

    let sync = transaction
        .rollback_owned_head_sync()
        .unwrap_err()
        .into_note();
    assert!(
        sync.contains("git-transaction-ownership-unproven"),
        "{sync}"
    );
    assert_eq!(
        git_required_text(root.path(), &["rev-parse", "HEAD"], "test-head").unwrap(),
        external
    );
    transaction.disarm();
}

#[test]
fn commit_receipt_path_summary_is_bounded() {
    let receipt = GitCommitReceipt {
        commit: "a".repeat(40),
        paths: (0..50).map(|index| format!("src/{index}.rs")).collect(),
    };
    let summary = receipt.reply();
    assert!(summary.contains("... (+30)"));
    assert!(!summary.contains("src/49.rs"));
}

#[test]
fn commit_receipt_sanitizes_control_characters_in_paths() {
    let receipt = GitCommitReceipt {
        commit: "b".repeat(40),
        paths: vec!["safe\u{1b}[31m\n\tname.txt".to_string()],
    };
    let reply = receipt.reply();
    let paths = reply.split_once("提交文件: ").unwrap().1;
    assert!(!paths.chars().any(char::is_control), "{reply:?}");
    assert!(!paths.contains("\u{1b}[31m"), "{reply:?}");
}

#[test]
fn read_only_git_request_does_not_capture_mutating_git_postcondition() {
    let root = tempfile::tempdir().unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::Explain, Depth::Fast, &[]),
        "提交当前改动",
    )
    .unwrap();

    assert!(postcondition.git_commit.is_none());
    assert!(postcondition
        .validate_final(root.path())
        .unwrap()
        .is_empty());
}

#[test]
fn git_commit_only_blocks_content_edits_made_after_capture() {
    let root = dirty_git_repo();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::write(root.path().join("src/existing.rs"), "before\n").unwrap();
    git(root.path(), &["add", "src/existing.rs"]);
    git(root.path(), &["commit", "-q", "-m", "add source"]);
    std::fs::write(root.path().join("one.txt"), "ready again\n").unwrap();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录",
    )
    .unwrap();
    git(root.path(), &["add", "one.txt"]);
    git(
        root.path(),
        &["commit", "-q", "-m", "record requested change"],
    );
    std::fs::write(root.path().join("src/existing.rs"), "cleanup\n").unwrap();

    let note = postcondition
        .validate_final(root.path())
        .unwrap_err()
        .into_note();
    assert!(note.contains("git-only-content-modified"));
    assert!(note.contains("src/existing.rs"));
}

#[test]
fn git_commit_only_blocks_when_head_did_not_advance() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &["one.txt"]),
        "提交git记录",
    )
    .unwrap();

    let note = postcondition
        .validate_final(root.path())
        .unwrap_err()
        .into_note();
    assert!(note.contains("git-commit-not-created"));
}

#[test]
fn git_commit_only_rejects_content_created_then_hidden_after_capture() {
    let root = dirty_git_repo();
    let postcondition = ResidentExecutionPostcondition::capture(
        root.path(),
        &route(RouteClass::QuickEdit, Depth::Fast, &[]),
        "提交git记录",
    )
    .unwrap();
    std::fs::write(root.path().join("unrelated.txt"), "created during turn\n").unwrap();
    git(root.path(), &["add", "one.txt", "unrelated.txt"]);
    git(root.path(), &["commit", "-q", "-m", "mixed commit"]);
    std::fs::remove_file(root.path().join("unrelated.txt")).unwrap();

    let note = postcondition
        .validate_final(root.path())
        .unwrap_err()
        .into_note();
    assert!(note.contains("git-commit-created-content"));
    assert!(note.contains("unrelated.txt"));
}

#[test]
fn git_dirty_snapshot_keeps_both_sides_of_a_rename() {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q"]);
    git(root.path(), &["config", "user.name", "UmaDev Test"]);
    git(
        root.path(),
        &["config", "user.email", "umadev-test@example.invalid"],
    );
    std::fs::write(root.path().join("before.txt"), "content\n").unwrap();
    git(root.path(), &["add", "before.txt"]);
    git(root.path(), &["commit", "-q", "-m", "initial"]);
    git(root.path(), &["mv", "before.txt", "after.txt"]);

    assert_eq!(
        git_dirty_paths(root.path()).unwrap(),
        BTreeSet::from(["after.txt".to_string(), "before.txt".to_string()])
    );
}

#[test]
fn legacy_porcelain_and_fact_helpers_remain_stable() {
    let before = " M a.rs\n?? keep.rs\n";
    let after = " M a.rs\nMM a.rs2\n?? new.rs\n";
    assert_eq!(
        changed_files_between(before, after),
        ["a.rs2", "keep.rs", "new.rs"]
    );
    assert!(agentic_fact_line(Some(&[]), true)
        .unwrap()
        .contains("[warn]"));
}
