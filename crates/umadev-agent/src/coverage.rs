//! Deterministic requirement-coverage check — the "real enforcement" that
//! Spec-Driven-Development research flags as the thing that actually matters: a
//! spec DOCUMENT does not guarantee the implementation (or even the task list)
//! covers it. After the spec phase, cross-check that every functional
//! requirement (`FR-NNN`) declared in the PRD is referenced by at least one
//! task, and surface the orphans so a requirement can't be silently dropped.
//!
//! This is the spec→tasks half of the verification loop; the architecture API
//! contract (`umadev-contract`) is the spec→code half. Pure + fail-open: any IO
//! error yields "nothing uncovered" so the check never blocks the pipeline.

use std::collections::BTreeSet;
use std::path::Path;

/// Functional-requirement ids (`FR-NNN`) the PRD declares but NO task cites —
/// i.e. requirements at risk of being silently dropped. Empty when everything is
/// covered, the PRD has no `FR-` ids, or the files can't be read.
#[must_use]
pub fn uncovered_requirements(project_root: &Path, slug: &str) -> Vec<String> {
    let prd = read(project_root.join("output").join(format!("{slug}-prd.md")));
    let declared = extract_fr_ids(&prd);
    if declared.is_empty() {
        return Vec::new();
    }
    // A requirement is "covered" if the execution plan OR any task list cites it.
    let mut cited = extract_fr_ids(&read(
        project_root
            .join("output")
            .join(format!("{slug}-execution-plan.md")),
    ));
    if let Some(tasks) = latest_tasks(project_root) {
        cited.extend(extract_fr_ids(&tasks));
    }
    declared.difference(&cited).cloned().collect()
}

fn read(path: std::path::PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The most-recent `.umadev/changes/<id>/tasks.md` (the change ids are
/// timestamp-suffixed, so lexicographic max == newest).
fn latest_tasks(project_root: &Path) -> Option<String> {
    let dir = project_root.join(".umadev").join("changes");
    let mut dirs: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    std::fs::read_to_string(dirs.last()?.join("tasks.md")).ok()
}

/// Scan for `FR-<digits>` tokens (case-insensitive on `FR`), normalised to
/// upper-case `FR-NNN`. `FR-` and ASCII digits are single-byte, so byte indexing
/// here is multibyte-safe even amid CJK prose.
fn extract_fr_ids(text: &str) -> BTreeSet<String> {
    let b = text.as_bytes();
    let n = b.len();
    let mut ids = BTreeSet::new();
    let mut i = 0;
    while i + 3 < n {
        let is_fr = (b[i] | 0x20) == b'f' && (b[i + 1] | 0x20) == b'r' && b[i + 2] == b'-';
        if is_fr {
            let mut j = i + 3;
            while j < n && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 3 {
                ids.insert(format!("FR-{}", &text[i + 3..j]));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_and_diffs_fr_ids() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(
            root.join("output").join("demo-prd.md"),
            "| FR-001 | 登录 | WHEN ... SHALL ... |\n| fr-002 | 登出 |\n| FR-003 | 注册 |",
        )
        .unwrap();
        let cdir = root.join(".umadev").join("changes").join("demo-20260101");
        std::fs::create_dir_all(&cdir).unwrap();
        // Tasks cover FR-001 and FR-002 (lowercase), but NOT FR-003.
        std::fs::write(
            cdir.join("tasks.md"),
            "- [ ] 实现登录 _(FR-001)_\n- [ ] 登出 _(fr-002)_",
        )
        .unwrap();
        let uncovered = uncovered_requirements(root, "demo");
        assert_eq!(uncovered, vec!["FR-003".to_string()]);
    }

    #[test]
    fn no_prd_requirements_means_nothing_uncovered() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(uncovered_requirements(tmp.path(), "demo").is_empty());
    }
}
