//! Run-scoped governance attribution for the Director path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runner::RunOptions;

const BASELINE_FILE: &str = ".umadev/director-governance-baseline.json";
const BASELINE_FILENAME: &str = "director-governance-baseline.json";
const MAX_BASELINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_BASELINE_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BASELINE_RECORD_BYTES: usize = 256 * 1024;
const MAX_POST_GATE_FINDINGS: usize = 25;
const MAX_POST_GATE_FINDING_CHARS: usize = 1_024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GovernanceBaseline {
    #[serde(default)]
    requirement_fingerprint: [u8; 32],
    /// Legacy unkeyed SHA-256. Read for JSON compatibility, never trusted or rewritten.
    #[serde(default, rename = "requirement_hash", skip_serializing)]
    _legacy_requirement_hash: String,
    source_hashes: BTreeMap<String, String>,
    #[serde(default)]
    preexisting_findings: Vec<String>,
    #[serde(default)]
    post_gate_inputs_consumed: bool,
}

/// Governance findings attributable to the current Director run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttributedGovernanceScan {
    pub(crate) violations: Vec<String>,
    pub(crate) ignored_preexisting: usize,
}

/// Replace any stale baseline with a snapshot captured before a fresh run writes.
pub(crate) fn begin(options: &RunOptions) {
    let Ok(root) = std::fs::canonicalize(&options.project_root) else {
        return;
    };
    if !umadev_state::fs::real_dir(&root) {
        return;
    }
    let Ok(dir) = umadev_state::fs::ensure_real_child_dir(&root, ".umadev") else {
        return;
    };
    let path = dir.join(BASELINE_FILENAME);
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if umadev_state::fs::metadata_is_real_file(&metadata) => {
            if umadev_state::fs::remove_regular_file(&path).is_err() {
                return;
            }
        }
        Ok(_) => return,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return,
    }
    let Some(baseline) = capture(options) else {
        return;
    };
    let Ok(body) = serde_json::to_vec(&baseline) else {
        return;
    };
    if body.len() > MAX_BASELINE_RECORD_BYTES {
        return;
    }
    let _ = umadev_state::fs::atomic_write(&path, &body);
}

/// Scan current source, suppressing only violations in byte-identical pre-run files.
pub(crate) fn scan(options: &RunOptions) -> AttributedGovernanceScan {
    let baseline = load(options);
    let policy = umadev_governance::Policy::load(&options.project_root);
    let ctx = project_context_for(options);
    let mut result = AttributedGovernanceScan::default();

    for path in crate::acceptance::source_files(&options.project_root) {
        let Some((relative, content, hash)) = read_source(&options.project_root, &path) else {
            continue;
        };
        let decision =
            umadev_governance::scan_content_with_context(&relative, &content, &policy, ctx);
        if !decision.block {
            continue;
        }
        if baseline
            .as_ref()
            .and_then(|snapshot| snapshot.source_hashes.get(&relative))
            .is_some_and(|before| before == &hash)
        {
            result.ignored_preexisting = result.ignored_preexisting.saturating_add(1);
            continue;
        }
        result.violations.push(render_finding(&relative, &decision));
        if result.violations.len() >= 25 {
            break;
        }
    }
    result
}

fn capture(options: &RunOptions) -> Option<GovernanceBaseline> {
    let key = umadev_state::privacy::installation_key()?;
    let mut source_hashes = BTreeMap::new();
    let mut preexisting_findings = Vec::new();
    let mut bytes = 0usize;
    let policy = umadev_governance::Policy::load(&options.project_root);
    let ctx = project_context_for(options);
    for path in crate::acceptance::source_files(&options.project_root) {
        let (relative, content, hash) = read_source(&options.project_root, &path)?;
        bytes = bytes.checked_add(content.len())?;
        if bytes > MAX_BASELINE_BYTES {
            return None;
        }
        if preexisting_findings.len() < MAX_POST_GATE_FINDINGS {
            let decision =
                umadev_governance::scan_content_with_context(&relative, &content, &policy, ctx);
            if decision.block {
                preexisting_findings.push(render_finding(&relative, &decision));
            }
        }
        source_hashes.insert(relative, hash);
    }
    Some(GovernanceBaseline {
        requirement_fingerprint: umadev_governance::requirement_fingerprint(
            &key,
            &options.requirement,
        ),
        _legacy_requirement_hash: String::new(),
        source_hashes,
        preexisting_findings,
        post_gate_inputs_consumed: false,
    })
}

/// Consume the bounded pre-run findings once, after the documentation gate.
///
/// They are context for the already-planned implementation/review, never current-block defects.
pub(crate) fn take_post_gate_inputs(options: &RunOptions) -> Vec<String> {
    let Some(mut baseline) = load(options) else {
        return Vec::new();
    };
    if baseline.post_gate_inputs_consumed || baseline.preexisting_findings.is_empty() {
        return Vec::new();
    }
    baseline.post_gate_inputs_consumed = true;
    if !write_baseline(&options.project_root, &baseline) {
        return Vec::new();
    }
    baseline.preexisting_findings
}

/// Render the exactly-once pre-run findings as bounded implementation context.
pub(crate) fn post_gate_context(options: &RunOptions) -> Option<(usize, String)> {
    let inputs = take_post_gate_inputs(options);
    if inputs.is_empty() {
        return None;
    }
    let mut context = String::from(
        "\n\nPost-gate brownfield inputs (these existed before this run; do not widen the current step or edit an out-of-scope path):\n",
    );
    for input in &inputs {
        context.push_str("- ");
        context.push_str(input);
        context.push('\n');
    }
    Some((inputs.len(), context))
}

fn load(options: &RunOptions) -> Option<GovernanceBaseline> {
    let path = baseline_path(&options.project_root);
    let body = crate::bounded_fs::read_utf8_beneath(
        &options.project_root,
        &path,
        MAX_BASELINE_RECORD_BYTES,
    )
    .ok()?;
    let baseline: GovernanceBaseline = serde_json::from_str(&body).ok()?;
    let key = umadev_state::privacy::installation_key()?;
    (baseline.requirement_fingerprint
        == umadev_governance::requirement_fingerprint(&key, &options.requirement))
    .then_some(baseline)
}

fn read_source(root: &Path, path: &Path) -> Option<(String, String, String)> {
    let content = crate::bounded_fs::read_utf8_beneath(root, path, MAX_BASELINE_FILE_BYTES).ok()?;
    let relative = path
        .strip_prefix(root)
        .ok()?
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let hash = hash_text(&content);
    Some((relative, content, hash))
}

fn hash_text(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn render_finding(relative: &str, decision: &umadev_governance::Decision) -> String {
    let summary = decision
        .reason
        .split('.')
        .next()
        .unwrap_or("violation")
        .replace(['\r', '\n'], " ");
    format!("{relative}: {} ({})", summary.trim(), decision.clause)
        .chars()
        .take(MAX_POST_GATE_FINDING_CHARS)
        .collect()
}

fn write_baseline(root: &Path, baseline: &GovernanceBaseline) -> bool {
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    if !umadev_state::fs::real_dir(&root) {
        return false;
    }
    let Ok(dir) = umadev_state::fs::ensure_real_child_dir(&root, ".umadev") else {
        return false;
    };
    let Ok(body) = serde_json::to_vec(baseline) else {
        return false;
    };
    body.len() <= MAX_BASELINE_RECORD_BYTES
        && umadev_state::fs::atomic_write(&dir.join(BASELINE_FILENAME), &body).is_ok()
}

fn baseline_path(root: &Path) -> PathBuf {
    root.join(BASELINE_FILE)
}

fn project_context_for(options: &RunOptions) -> umadev_governance::ProjectContext {
    crate::planner::derive_project_context(
        &options.requirement,
        &options.project_root,
        &options.effective_slug(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::TrustMode;

    fn options(root: &Path) -> RunOptions {
        RunOptions {
            project_root: root.to_path_buf(),
            requirement: "write documentation before implementation".to_string(),
            slug: "docs".to_string(),
            backend: "codex".to_string(),
            model: String::new(),
            design_system: String::new(),
            seed_template: String::new(),
            mode: TrustMode::Guarded,
            strict_coverage: false,
        }
    }

    #[test]
    fn unchanged_preexisting_findings_are_not_attributed_to_the_run() {
        let root = tempfile::TempDir::new().unwrap();
        let source = root.path().join("button.tsx");
        std::fs::write(
            &source,
            "export const Btn = () => <button>\u{1F680} Launch</button>;",
        )
        .unwrap();
        let options = options(root.path());
        begin(&options);

        let unchanged = scan(&options);
        assert!(unchanged.violations.is_empty());
        assert_eq!(unchanged.ignored_preexisting, 1);

        std::fs::write(
            source,
            "export const Btn = () => <button>\u{1F680} Launch now</button>;",
        )
        .unwrap();
        let changed = scan(&options);
        assert_eq!(changed.ignored_preexisting, 0);
        assert_eq!(changed.violations.len(), 1);
    }

    #[test]
    fn preexisting_findings_become_exactly_once_post_gate_inputs() {
        let root = tempfile::TempDir::new().unwrap();
        std::fs::write(
            root.path().join("button.tsx"),
            "export const Btn = () => <button>\u{1F680} Launch</button>;",
        )
        .unwrap();
        let options = options(root.path());
        begin(&options);

        let (count, first) = post_gate_context(&options).expect("captured finding");
        assert_eq!(count, 1);
        assert!(first.contains("- button.tsx"));
        assert!(post_gate_context(&options).is_none());
    }

    #[test]
    fn baseline_uses_keyed_provenance_and_legacy_unkeyed_hashes_fail_closed() {
        let root = tempfile::TempDir::new().unwrap();
        let options = options(root.path());
        begin(&options);
        let path = baseline_path(root.path());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(&options.requirement));
        assert!(raw.contains("requirement_fingerprint"));
        assert!(!raw.contains("requirement_hash"));
        assert!(
            load(&options).is_some(),
            "same-installation cache remains reusable"
        );

        let legacy = serde_json::json!({
            "requirement_hash": hash_text(&options.requirement),
            "source_hashes": {}
        });
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert!(
            load(&options).is_none(),
            "an old dictionary-testable SHA stamp is re-captured rather than trusted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn begin_never_follows_a_linked_managed_directory_or_baseline_leaf() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let outside_baseline = outside.path().join(BASELINE_FILENAME);
        std::fs::write(&outside_baseline, b"outside").unwrap();
        let options = options(root.path());

        symlink(outside.path(), root.path().join(".umadev")).unwrap();
        begin(&options);
        assert_eq!(std::fs::read(&outside_baseline).unwrap(), b"outside");

        std::fs::remove_file(root.path().join(".umadev")).unwrap();
        std::fs::create_dir(root.path().join(".umadev")).unwrap();
        symlink(&outside_baseline, root.path().join(BASELINE_FILE)).unwrap();
        begin(&options);
        assert_eq!(std::fs::read(&outside_baseline).unwrap(), b"outside");
    }
}
