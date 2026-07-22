pub(super) const DEFERRED_CRITIC_REVIEW_NOTE: &str =
    "team · deterministic blockers found — deferring critic review until the repaired candidate is clean";

pub(super) fn should_run_critic_review(deterministic_blockers: &[String]) -> bool {
    deterministic_blockers.is_empty()
}

/// Read a written `runtime-proof.json` and, if it recorded a real (non-skipped)
/// FAILURE to boot/answer, return a blocking line. A missing file → `None` (the
/// runtime check simply wasn't run this loop — neutral, never a fabricated fail).
/// A written-but-not-verified proof whose reason is a SKIP (no dev server / no
/// curl) is also neutral; only a proof that ran and failed blocks. Fail-open: an
/// unreadable / unparseable file → `None`.
///
/// FRESHNESS: a proof whose source fingerprint no longer matches the tree is STALE
/// and is not read at all — in EITHER direction. A stale FAILURE must not block code
/// that has since been fixed, and (more importantly) a stale PASS must not be mistaken
/// for evidence about the code we are shipping. A proof produced before the last change
/// to the code it describes is not a proof; the check has to be re-run for real, which
/// is exactly what the floor does on its own path. Fail-open: an unstamped proof (an
/// older artifact) has no fingerprint to contradict, so it is read as before.
pub(super) fn runtime_proof_blocking(root: &std::path::Path) -> Option<String> {
    let path = root.join(crate::runtime_proof::runtime_proof_rel_path());
    let body = std::fs::read_to_string(path).ok()?;
    let proof: crate::runtime_proof::RuntimeProof = serde_json::from_str(&body).ok()?;
    if proof.is_stale(root) {
        return None; // describes a tree that no longer exists → says nothing about this one
    }
    if proof.status.is_verified() {
        return None; // booted + answered → no problem
    }
    // Not verified. Distinguish a real failure from a neutral skip: a skip reason
    // names an absent precondition (no dev server / curl / not detected). Only a
    // genuine boot/route failure is blocking.
    let reason = proof.summary_line().to_ascii_lowercase();
    let is_skip = reason.contains("not found")
        || reason.contains("no dev server")
        || reason.contains("not detected")
        || reason.contains("skipped");
    if is_skip {
        return None;
    }
    Some(format!(
        "runtime-proof: the app did not boot + answer its routes — {} (fix the cause so it \
         actually runs, then re-verify)",
        proof.summary_line()
    ))
}

/// Heuristic: does the project carry at least one real test file? Used only for the
/// Bugfix reproduction-test floor. Looks for the universal test-file conventions
/// (`*.test.*` / `*.spec.*` / a `tests/` or `__tests__` dir / a `test_*.py` /
/// `*_test.go` / a Rust `#[test]`). Pure + fail-open (bounded by `source_files`):
/// an empty tree → `false`. Conservative — a false "has a test" only DROPS a
/// blocking floor (never fabricates one), so we require a reasonably strong signal.
pub(super) fn has_reproduction_test(root: &std::path::Path) -> bool {
    for f in crate::acceptance::source_files(root) {
        let name = f
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let path_str = f.to_string_lossy().to_ascii_lowercase();
        let by_name = name.contains(".test.")
            || name.contains(".spec.")
            || name.starts_with("test_")
            || name.ends_with("_test.go")
            || name.ends_with("_test.py")
            || name.ends_with(".test.rs");
        let by_dir = path_str.contains("/tests/")
            || path_str.contains("/__tests__/")
            || path_str.contains("/test/")
            || path_str.contains("/spec/");
        if by_name || by_dir {
            return true;
        }
        // A Rust file carrying `#[test]` / `#[tokio::test]` is a real test too.
        if name.to_ascii_lowercase().ends_with(".rs") {
            if let Ok(content) = std::fs::read_to_string(&f) {
                if content.contains("#[test]") || content.contains("#[tokio::test]") {
                    return true;
                }
            }
        }
    }
    false
}
