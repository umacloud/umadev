//! Backend timeout contract kept in its own integration-test process.
//!
//! This test deliberately measures real wall-clock time and process-tree
//! cleanup. Running it beside every full-pipeline E2E in the same test process
//! turns host scheduler starvation into part of the product timeout contract,
//! so Cargo gives this resource-sensitive contract an isolated test binary.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_umadev"))
}

fn hermetic_command(cwd: &Path) -> Command {
    let home = cwd.join(".umadev").join("e2e-home");
    let empty_model = home.join("empty-embed-model");
    std::fs::create_dir_all(&empty_model).expect("create hermetic timeout-test home");

    let mut command = Command::new(bin());
    command
        .current_dir(cwd)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("UMADEV_EMBED_MODEL_DIR", &empty_model)
        .env_remove("OPENAI_EMBED_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("UMADEV_ALLOW_CLOUD_EMBED")
        .env_remove("OPENAI_EMBED_BASE");
    command
}

#[test]
fn backend_timeout_pauses_bounded_with_an_explicit_offline_placeholder() {
    // Pass installation/authentication probes, then wedge only the real model
    // invocation. The child sleep proves whole-tree termination, not merely
    // direct launcher termination.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let fake = root.join("fake-claude");
    let call_log = root.join("model-calls.log");
    let pid_log = root.join("model-pids.log");
    std::fs::write(
        &fake,
        "#!/bin/sh\n\
         if [ \"$1\" = \"--version\" ]; then echo '2.1.0'; exit 0; fi\n\
         if [ \"$1\" = \"auth\" ]; then echo '{\"loggedIn\":true}'; exit 0; fi\n\
         printf 'call\\n' >> \"$UMADEV_TEST_CALL_LOG\"\n\
         sleep 30 &\n\
         leaf=$!\n\
         printf '%s\\n' \"$leaf\" >> \"$UMADEV_TEST_PID_LOG\"\n\
         wait \"$leaf\"\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).unwrap();

    let started = Instant::now();
    let output = hermetic_command(root)
        .args([
            "run",
            "build x",
            "--slug",
            "timeout",
            "--backend",
            "claude-code",
        ])
        .env("UMADEV_CLAUDE_BIN", &fake)
        .env("UMADEV_TEST_CALL_LOG", &call_log)
        .env("UMADEV_TEST_PID_LOG", &pid_log)
        .env("UMADEV_WORKER_TIMEOUT", "1")
        .env("UMADEV_RETRY_BASE_MS", "1")
        .env("UMADEV_LEGACY_PIPELINE", "1")
        .env("UMADEV_CONTINUOUS", "0")
        .output()
        .expect("spawn the timeout contract run");

    assert!(
        started.elapsed() < Duration::from_secs(25),
        "the wedged base was not terminated within the bounded retry budget"
    );
    let calls = std::fs::read_to_string(&call_log).expect("the fake base recorded model calls");
    assert_eq!(
        calls.lines().count(),
        3,
        "one streaming timeout per retry is enough; a cold fallback must not double-spend it"
    );
    for pid in std::fs::read_to_string(&pid_log)
        .expect("the fake base recorded descendant pids")
        .lines()
    {
        let mut alive = true;
        for _ in 0..100 {
            alive = Command::new("kill")
                .args(["-0", pid])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if !alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !alive,
            "the timeout path left model descendant pid {pid} running"
        );
    }

    let diagnostic = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "the explicit gate pause failed: {diagnostic}"
    );
    assert!(
        diagnostic.contains("timed out"),
        "the bounded failure must retain the real timeout cause: {diagnostic}"
    );
    assert!(diagnostic.contains("pipeline paused") || diagnostic.contains("Pipeline paused"));
    assert!(
        diagnostic.contains("离线骨架") && diagnostic.contains("非真实生成"),
        "the fallback must be disclosed as a non-generated placeholder: {diagnostic}"
    );
    assert!(
        !diagnostic.contains("Pipeline complete"),
        "a placeholder must never be rendered as completed work: {diagnostic}"
    );
    let placeholder = root.join("output/timeout-clarify.md");
    assert!(
        placeholder.is_file(),
        "the disclosed placeholder should remain available for gate review"
    );
    let placeholder = std::fs::read_to_string(placeholder).unwrap();
    assert!(placeholder.contains("##") && placeholder.len() > 100);
}
