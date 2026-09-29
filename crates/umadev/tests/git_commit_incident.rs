//! Release regression for the incident where a plain Git-record request entered
//! the AI pipeline, looped on read-only authority, then launched review/QC.
//!
//! These tests exercise the shipped binary. A deliberately broken `claude`
//! executable is placed first on `PATH`: crossing the host-owned Git boundary
//! therefore leaves an observable marker instead of silently depending on the
//! developer machine's installed base.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_umadev"))
}

struct Fixture {
    repo: TempDir,
    home: TempDir,
    shims: TempDir,
    base_marker: PathBuf,
    docker_marker: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let repo = TempDir::new().expect("repo tempdir");
        let home = TempDir::new().expect("home tempdir");
        let shims = TempDir::new().expect("shim tempdir");
        let base_marker = shims.path().join("base-was-invoked");
        let docker_marker = shims.path().join("docker-was-invoked");
        install_failing_probe(shims.path(), "claude", &base_marker);
        install_failing_probe(shims.path(), "claude-code", &base_marker);
        install_failing_probe(shims.path(), "docker", &docker_marker);

        git(repo.path(), &["init", "-q"]);
        git(
            repo.path(),
            &["config", "user.email", "umadev-test@example.invalid"],
        );
        git(repo.path(), &["config", "user.name", "UmaDev Test"]);
        git(repo.path(), &["config", "commit.gpgSign", "false"]);
        std::fs::write(repo.path().join("tracked.txt"), "before\n").expect("seed file");
        git(repo.path(), &["add", "--", "tracked.txt"]);
        git(repo.path(), &["commit", "-q", "-m", "seed"]);
        std::fs::write(repo.path().join("tracked.txt"), "after\n").expect("dirty file");

        Self {
            repo,
            home,
            shims,
            base_marker,
            docker_marker,
        }
    }

    fn command(&self, requirement: &str, mode: &str) -> Command {
        let mut command = Command::new(bin());
        command
            .current_dir(self.repo.path())
            .args([
                "run",
                requirement,
                "--backend",
                "claude-code",
                "--mode",
                mode,
                // `--mode auto` runs only in a project the user trusts.
                "--trust-project",
                "--project-root",
            ])
            .arg(self.repo.path())
            .env("HOME", self.home.path())
            .env("USERPROFILE", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join(".config"))
            .env("XDG_CACHE_HOME", self.home.path().join(".cache"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("PATH", prepend_path(self.shims.path()))
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("UMADEV_CONTINUOUS")
            .env_remove("UMADEV_LEGACY_RUN");
        command
    }

    fn head(&self) -> String {
        git_output(self.repo.path(), &["rev-parse", "HEAD"])
    }

    fn commit_count(&self) -> u32 {
        git_output(self.repo.path(), &["rev-list", "--count", "HEAD"])
            .parse()
            .expect("commit count")
    }

    fn assert_no_ai_pipeline_side_effects(&self, output: &Output) {
        assert!(
            !self.base_marker.exists(),
            "the Claude base probe/session was reached"
        );
        assert!(
            !self.docker_marker.exists(),
            "the commit-only request invoked Docker"
        );
        for forbidden in [
            ".umadev/plan.json",
            ".umadev/workflow-state.json",
            ".umadev/governance-context.json",
            ".umadev/team-ledger.jsonl",
            ".umadev/operational-review-checkpoint.json",
            ".umadev/audit/tool-calls.jsonl",
            "output",
            "release",
        ] {
            assert!(
                !self.repo.path().join(forbidden).exists(),
                "commit-only request created pipeline/review state `{forbidden}`\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
        .to_ascii_lowercase();
        for forbidden in [
            "team review",
            "review turn timed out",
            "execution contract",
            "docker",
        ] {
            assert!(
                !combined.contains(forbidden),
                "commit-only output leaked pipeline work `{forbidden}`:\n{combined}"
            );
        }
    }
}

#[test]
fn plan_commit_is_a_true_no_write_settle_before_base_review_or_docker() {
    let fixture = Fixture::new();
    let before_head = fixture.head();
    let before_count = fixture.commit_count();

    let output = fixture
        .command("提交git记录", "plan")
        .stdin(Stdio::null())
        .output()
        .expect("run plan commit");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(fixture.head(), before_head);
    assert_eq!(fixture.commit_count(), before_count);
    fixture.assert_no_ai_pipeline_side_effects(&output);
}

#[test]
fn guarded_commit_denial_stops_after_one_host_confirmation() {
    let fixture = Fixture::new();
    let before_head = fixture.head();
    let before_count = fixture.commit_count();

    let mut child = fixture
        .command("提交git记录", "guarded")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn guarded commit");
    child
        .stdin
        .take()
        .expect("guarded stdin")
        .write_all(b"n\n")
        .expect("deny current commit");
    let output = child.wait_with_output().expect("guarded result");

    assert!(
        !output.status.success(),
        "a denied consequential action must be non-zero"
    );
    assert_eq!(fixture.head(), before_head);
    assert_eq!(fixture.commit_count(), before_count);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.matches("[y/N]").count(),
        1,
        "Guarded must ask exactly once, not loop on read-only authority: {stdout}"
    );
    fixture.assert_no_ai_pipeline_side_effects(&output);
}

#[test]
fn guarded_explicit_confirmation_and_auto_each_make_exactly_one_host_commit() {
    for (requirement, mode) in [("确定提交", "guarded"), ("提交git记录", "auto")] {
        let fixture = Fixture::new();
        let before_count = fixture.commit_count();

        let output = fixture
            .command(requirement, mode)
            .stdin(Stdio::null())
            .output()
            .expect("run host commit");

        assert!(
            output.status.success(),
            "{requirement}/{mode}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert_eq!(
            fixture.commit_count(),
            before_count + 1,
            "{requirement}/{mode} must create exactly one commit"
        );
        assert!(
            git_output(fixture.repo.path(), &["status", "--porcelain"]).is_empty(),
            "{requirement}/{mode} must settle the requested dirty file"
        );
        fixture.assert_no_ai_pipeline_side_effects(&output);
    }
}

fn prepend_path(first: &Path) -> OsString {
    let mut paths = vec![first.to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).expect("join PATH")
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("invoke git");
    assert!(
        output.status.success(),
        "git {args:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("invoke git");
    assert!(
        output.status.success(),
        "git {args:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[cfg(unix)]
fn install_failing_probe(directory: &Path, program: &str, marker: &Path) {
    use std::os::unix::fs::PermissionsExt as _;

    let path = directory.join(program);
    let quoted_marker = marker.to_string_lossy().replace('\'', "'\\''");
    std::fs::write(
        &path,
        format!("#!/bin/sh\nprintf invoked > '{quoted_marker}'\nexit 97\n"),
    )
    .expect("write failing probe");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make failing probe executable");
}

#[cfg(windows)]
fn install_failing_probe(directory: &Path, program: &str, marker: &Path) {
    let path = directory.join(format!("{program}.cmd"));
    std::fs::write(
        path,
        format!("@echo invoked>\"{}\"\r\n@exit /b 97\r\n", marker.display()),
    )
    .expect("write failing probe");
}
