//! Run-scoped governance attribution for the Director path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runner::RunOptions;

const BASELINE_FILE: &str = ".umadev/director-governance-baseline.json";
const MAX_BASELINE_BYTES: usize = 16 * 1024 * 1024;
const MAX_BASELINE_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_BASELINE_RECORD_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct GovernanceBaseline {
    requirement_hash: String,
    source_hashes: BTreeMap<String, String>,
}

/// Governance findings attributable to the current Director run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttributedGovernanceScan {
    pub(crate) violations: Vec<String>,
    pub(crate) ignored_preexisting: usize,
}

/// Replace any stale baseline with a snapshot captured before a fresh run writes.
pub(crate) fn begin(options: &RunOptions) {
    let path = baseline_path(&options.project_root);
    let _ = std::fs::remove_file(&path);
    let Some(baseline) = capture(options) else {
        return;
    };
    let Ok(body) = serde_json::to_vec(&baseline) else {
        return;
    };
    if body.len() > MAX_BASELINE_RECORD_BYTES {
        return;
    }
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
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
        result.violations.push(format!(
            "{relative}: {} ({})",
            decision
                .reason
                .split('.')
                .next()
                .unwrap_or("violation")
                .trim(),
            decision.clause
        ));
        if result.violations.len() >= 25 {
            break;
        }
    }
    result
}

fn capture(options: &RunOptions) -> Option<GovernanceBaseline> {
    let mut source_hashes = BTreeMap::new();
    let mut bytes = 0usize;
    for path in crate::acceptance::source_files(&options.project_root) {
        let (relative, content, hash) = read_source(&options.project_root, &path)?;
        bytes = bytes.checked_add(content.len())?;
        if bytes > MAX_BASELINE_BYTES {
            return None;
        }
        source_hashes.insert(relative, hash);
    }
    Some(GovernanceBaseline {
        requirement_hash: hash_text(&options.requirement),
        source_hashes,
    })
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
    (baseline.requirement_hash == hash_text(&options.requirement)).then_some(baseline)
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
}
