/// A QC result keeps product defects separate from reviewer infrastructure
/// failures. Only `blocking` may ever become a source-repair directive.
#[derive(Debug, Clone, Default)]
pub(super) struct QcReport {
    pub(super) blocking: Vec<String>,
    pub(super) operational: Vec<String>,
    pub(super) raw_failure_log: Option<String>,
}

impl QcReport {
    pub(super) fn is_clean(&self) -> bool {
        self.blocking.is_empty() && self.operational.is_empty()
    }

    pub(super) fn has_operational_failure(&self) -> bool {
        !self.operational.is_empty()
    }

    pub(super) fn residual_evidence(&self) -> Vec<String> {
        let mut evidence = self.blocking.clone();
        evidence.extend(self.operational.iter().cloned());
        evidence
    }

    #[cfg(test)]
    pub(super) fn fix_directive(&self) -> String {
        self.fix_directive_with_context("")
    }

    #[cfg(test)]
    pub(super) fn fix_directive_with_context(&self, prefix: &str) -> String {
        let mut tracker = crate::blocker::BlockerSetTracker::default();
        let assessments = self.assess_blockers(&mut tracker, false);
        self.fix_directive_with_assessments(prefix, &assessments)
    }

    pub(super) fn assess_blockers(
        &self,
        tracker: &mut crate::blocker::BlockerSetTracker,
        workspace_progress: bool,
    ) -> Vec<crate::blocker::BlockerAssessment> {
        let mut evidence = self.blocking.clone();
        if let Some(raw) = self
            .raw_failure_log
            .as_ref()
            .filter(|raw| !raw.trim().is_empty())
        {
            evidence.push(raw.clone());
        }
        tracker.assess_all(&evidence, "objective QC", true, workspace_progress)
    }

    pub(super) fn fix_directive_with_assessments(
        &self,
        prefix: &str,
        assessments: &[crate::blocker::BlockerAssessment],
    ) -> String {
        // Deliberately render only `blocking`: a mixed review may still contain
        // actionable semantic findings, but its infrastructure failures can only
        // require a re-review, never a source edit.
        let mut body = String::new();
        for b in &self.blocking {
            body.push_str("- ");
            body.push_str(b);
            body.push('\n');
        }
        let lead = if prefix.trim().is_empty() {
            String::new()
        } else {
            format!("{}\n\n", prefix.trim_end())
        };
        let raw = match self.raw_failure_log.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => {
                format!("\n\n## Raw failing build/test output (verbatim tail)\n```text\n{t}\n```")
            }
            _ => String::new(),
        };
        let diagnosis = assessments
            .iter()
            .take(8)
            .map(crate::blocker::BlockerAssessment::prompt_block)
            .collect::<Vec<_>>()
            .join("\n\n");
        format!(
            "{lead}An objective check of what you just built surfaced problems that must be \
             fixed (these are real facts read from disk / review, not your memory):\n\
             {body}\n{diagnosis}\n\nFix the cause of each one yourself with your tools — edit/create \
             the real files — then RUN the project's own build and tests to confirm \
             they pass. When it is genuinely clean, end your turn and report honestly \
             what you fixed.{raw}"
        )
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ReviewEvidence {
    pub(super) blocking: Vec<String>,
    pub(super) operational: Vec<String>,
}

pub(super) fn split_review_evidence(review: &crate::director::ReviewResult) -> ReviewEvidence {
    ReviewEvidence {
        // Model-authored blocker text is always product evidence. Free text is
        // not authority to reinterpret a semantic verdict as infrastructure.
        blocking: review.blocking.clone(),
        // Only the host-owned typed unavailable channel can pause a review.
        operational: review
            .unavailable
            .iter()
            .map(|item| format!("review unavailable: {item}"))
            .collect(),
    }
}

pub(super) fn operational_stop_note(operational: &[String]) -> String {
    let evidence = operational
        .iter()
        .take(4)
        .map(|item| item.chars().take(240).collect::<String>())
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "team · required review infrastructure unavailable — stopping incomplete without \
         source rework (retry review; code edits/builds cannot repair this): {evidence}"
    )
}

pub(super) fn operational_recheck_note(operational: &[String]) -> String {
    let evidence = operational
        .iter()
        .take(4)
        .map(|item| item.chars().take(240).collect::<String>())
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "team · semantic findings were sent for repair, but the required re-review \
         was unavailable — the old findings are not asserted as still present; \
         completion remains unverified: {evidence}"
    )
}

pub(super) fn operational_mixed_note(operational: &[String]) -> String {
    let evidence = operational
        .iter()
        .take(4)
        .map(|item| item.chars().take(240).collect::<String>())
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "team · semantic findings are retained at this paused boundary, but no \
         source repair starts while required reviewer infrastructure is unavailable; \
         retry the complete review before acting on them: {evidence}"
    )
}

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
    let body =
        crate::bounded_fs::read_utf8_beneath(root, &path, super::DIRECTOR_EVIDENCE_FILE_BYTES)
            .ok()?;
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
    let mut budget = crate::bounded_fs::Utf8ReadBudget::new(
        super::DIRECTOR_SOURCE_TOTAL_BYTES,
        crate::acceptance::MAX_SOURCE_FILE_BYTES,
    );
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
            if let Ok(content) = budget.read_utf8_beneath(root, &f) {
                if content.contains("#[test]") || content.contains("#[tokio::test]") {
                    return true;
                }
            }
        }
    }
    false
}
