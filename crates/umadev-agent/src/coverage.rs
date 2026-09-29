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

const MAX_COVERAGE_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_COVERAGE_TOTAL_BYTES: usize = 6 * 1024 * 1024;
const MAX_CHANGE_DIR_ENTRIES: usize = 256;

/// Functional-requirement ids (`FR-NNN`) the PRD declares but NO task cites —
/// i.e. requirements at risk of being silently dropped. Empty when everything is
/// covered, the PRD has no `FR-` ids, or the files can't be read.
#[must_use]
pub fn uncovered_requirements(project_root: &Path, slug: &str) -> Vec<String> {
    let mut budget =
        crate::bounded_fs::Utf8ReadBudget::new(MAX_COVERAGE_TOTAL_BYTES, MAX_COVERAGE_FILE_BYTES);
    let prd_path = project_root.join("output").join(format!("{slug}-prd.md"));
    let prd = match read_optional(project_root, &prd_path, &mut budget) {
        Ok(Some(prd)) => prd,
        Ok(None) => return Vec::new(),
        Err(error) => return vec![coverage_unavailable(&prd_path, &error)],
    };
    let declared = extract_fr_ids(&prd);
    if declared.is_empty() {
        return Vec::new();
    }
    // A requirement is "covered" if the execution plan OR any task list cites it.
    let execution_path = project_root
        .join("output")
        .join(format!("{slug}-execution-plan.md"));
    let execution = match read_optional(project_root, &execution_path, &mut budget) {
        Ok(content) => content.unwrap_or_default(),
        Err(error) => return vec![coverage_unavailable(&execution_path, &error)],
    };
    let mut cited = extract_fr_ids(&execution);
    let tasks = match latest_tasks(project_root, &mut budget) {
        Ok(tasks) => tasks,
        Err(error) => {
            return vec![format!(
                "[unavailable] requirement coverage could not read the latest tasks completely ({error})"
            )];
        }
    };
    if let Some(tasks) = tasks {
        cited.extend(extract_fr_ids(&tasks));
    }
    declared.difference(&cited).cloned().collect()
}

/// Requirement coverage as the director judges it ([`requirement_coverage`]). Typed
/// so that a source that cannot be read, or the absence of anything a requirement
/// could be traced to, never reads as "every requirement uncovered".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequirementCoverage {
    /// Every declared requirement is traced (or the PRD declares none).
    Covered,
    /// Declared `FR-` ids that no plan step, task list or architecture doc cites.
    Uncovered(Vec<String>),
    /// Coverage cannot be judged; the reason, in the user's language.
    Unavailable(String),
}

/// Requirement coverage for the director path. [`uncovered_requirements`] reads only
/// the legacy pipeline's task artifacts (`output/<slug>-execution-plan.md`, the newest
/// `.umadev/changes/*/tasks.md`), which the director never writes. Here a requirement
/// is also traced when the owned plan (`.umadev/plan.json` step titles and evidence)
/// or the architecture doc cites its id. With none of those on disk there is nothing
/// a requirement could be traced to yet, so coverage is `Unavailable` rather than
/// "all uncovered".
#[must_use]
pub fn requirement_coverage(project_root: &Path, slug: &str) -> RequirementCoverage {
    let mut budget =
        crate::bounded_fs::Utf8ReadBudget::new(MAX_COVERAGE_TOTAL_BYTES, MAX_COVERAGE_FILE_BYTES);
    let unreadable = |path: &Path, error: &std::io::Error| {
        let shown = path.strip_prefix(project_root).unwrap_or(path);
        RequirementCoverage::Unavailable(umadev_i18n::tlf(
            "qc.floor_input_unreadable",
            &[&shown.display().to_string(), &error.to_string()],
        ))
    };
    let prd_path = project_root.join("output").join(format!("{slug}-prd.md"));
    let prd = match read_optional(project_root, &prd_path, &mut budget) {
        Ok(Some(prd)) => prd,
        Ok(None) => return RequirementCoverage::Covered,
        Err(error) => return unreadable(&prd_path, &error),
    };
    let declared = extract_fr_ids(&prd);
    if declared.is_empty() {
        return RequirementCoverage::Covered;
    }
    let mut cited = BTreeSet::new();
    let mut traceable = false;
    for doc in ["execution-plan", "architecture"] {
        let path = project_root.join("output").join(format!("{slug}-{doc}.md"));
        match read_optional(project_root, &path, &mut budget) {
            Ok(Some(text)) => {
                traceable = true;
                cited.extend(extract_fr_ids(&text));
            }
            Ok(None) => {}
            Err(error) => return unreadable(&path, &error),
        }
    }
    match latest_tasks(project_root, &mut budget) {
        Ok(Some(tasks)) => {
            traceable = true;
            cited.extend(extract_fr_ids(&tasks));
        }
        Ok(None) => {}
        Err(error) => {
            return RequirementCoverage::Unavailable(umadev_i18n::tlf(
                "qc.floor_input_unreadable",
                &[".umadev/changes/*/tasks.md", &error.to_string()],
            ));
        }
    }
    if let Some(plan) = crate::plan_state::load(project_root) {
        traceable = true;
        for step in &plan.steps {
            cited.extend(extract_fr_ids(&step.title));
            cited.extend(extract_fr_ids(&step.criterion_label()));
        }
    }
    if !traceable {
        return RequirementCoverage::Unavailable(
            umadev_i18n::tl("qc.coverage_nothing_to_trace").to_string(),
        );
    }
    let uncovered: Vec<String> = declared.difference(&cited).cloned().collect();
    if uncovered.is_empty() {
        RequirementCoverage::Covered
    } else {
        RequirementCoverage::Uncovered(uncovered)
    }
}

fn coverage_unavailable(path: &Path, error: &std::io::Error) -> String {
    format!(
        "[unavailable] requirement coverage could not read {} completely ({error})",
        path.display()
    )
}

fn read_optional(
    project_root: &Path,
    path: &Path,
    budget: &mut crate::bounded_fs::Utf8ReadBudget,
) -> std::io::Result<Option<String>> {
    match budget.read_utf8_beneath(project_root, path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The most-recent `.umadev/changes/<id>/tasks.md`.
///
/// P1-6: change ids are usually timestamp-suffixed, but a hand-named change like
/// `demo-hotfix` sorts lexicographically AFTER any digit-prefixed id, so a plain
/// `dirs.sort()` would pick the WRONG (non-newest) directory and read a stale
/// tasks list — silently misreporting coverage. Pick the newest by filesystem
/// mtime instead, falling back to the directory NAME only when mtime is
/// unavailable (so a deterministic order is still chosen, fail-open).
fn latest_tasks(
    project_root: &Path,
    budget: &mut crate::bounded_fs::Utf8ReadBudget,
) -> std::io::Result<Option<String>> {
    let dir = project_root.join(".umadev").join("changes");
    match std::fs::symlink_metadata(&dir) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    if !crate::bounded_fs::is_real_directory_beneath(project_root, &dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "changes directory is a link or outside the project root",
        ));
    }
    let entries = std::fs::read_dir(&dir)?;
    let mut dirs = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_CHANGE_DIR_ENTRIES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "changes directory entry budget exhausted",
            ));
        }
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if crate::bounded_fs::is_real_directory_beneath(project_root, &path) {
            dirs.push(path);
        }
    }
    // Sort by (mtime, name): mtime is the real recency signal; the name is a
    // stable tiebreaker so two dirs with the same/unknown mtime still order
    // deterministically. A missing mtime sorts oldest (UNIX_EPOCH).
    dirs.sort_by_cached_key(|p| {
        let mtime = std::fs::symlink_metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        (mtime, p.file_name().map(std::ffi::OsString::from))
    });
    let Some(latest) = dirs.last() else {
        return Ok(None);
    };
    read_optional(project_root, &latest.join("tasks.md"), budget)
}

/// Scan for `FR-<digits>` tokens (case-insensitive on `FR`, starting at a word
/// boundary so `NFR-<digits>` is not an FR), normalised to a
/// canonical zero-padded `FR-NNN`. `FR-` and ASCII digits are single-byte, so
/// byte indexing here is multibyte-safe even amid CJK prose.
///
/// P1-7: the digit run is PARSED to a number and re-formatted zero-padded, so
/// `FR-1`, `FR-01`, and `FR-001` all canonicalise to the SAME `FR-001`. Without
/// this, a PRD that writes `FR-001` and a tasks file that writes `FR-1` looked
/// like DIFFERENT requirements and produced a phantom "uncovered" report. A run
/// of digits that overflows `u32` (absurd, but fail-open) keeps the raw digits.
fn extract_fr_ids(text: &str) -> BTreeSet<String> {
    let b = text.as_bytes();
    let n = b.len();
    let mut ids = BTreeSet::new();
    let mut i = 0;
    while i + 3 < n {
        // Word boundary before `FR`: `NFR-004` (a non-functional requirement)
        // or `xFR-5` must not be read as `FR-004` / `FR-005`.
        let at_boundary = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        let is_fr =
            at_boundary && (b[i] | 0x20) == b'f' && (b[i + 1] | 0x20) == b'r' && b[i + 2] == b'-';
        if is_fr {
            let mut j = i + 3;
            while j < n && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 3 {
                ids.insert(normalize_fr(&text[i + 3..j]));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    ids
}

/// Canonicalise a run of FR digits to zero-padded `FR-NNN` so `1` / `01` / `001`
/// compare equal. Falls back to the raw digits (still prefixed) if the number
/// can't be parsed (e.g. it overflows `u32`) — fail-open, never panics.
fn normalize_fr(digits: &str) -> String {
    match digits.parse::<u32>() {
        Ok(num) => format!("FR-{num:03}"),
        Err(_) => format!("FR-{digits}"),
    }
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
    fn non_functional_ids_are_not_functional_requirements() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(
            root.join("output").join("demo-prd.md"),
            "| FR-001 | 登录 |\n| NFR-002 | p95 < 200ms |",
        )
        .unwrap();
        let cdir = root.join(".umadev").join("changes").join("demo-20260101");
        std::fs::create_dir_all(&cdir).unwrap();
        std::fs::write(cdir.join("tasks.md"), "- [ ] 实现登录 _(FR-001)_").unwrap();
        assert!(uncovered_requirements(root, "demo").is_empty());
        assert_eq!(
            extract_fr_ids("NFR-004, xFR-5, (FR-6), 需求FR-7"),
            ["FR-006", "FR-007"].map(String::from).into()
        );
    }

    #[test]
    fn no_prd_requirements_means_nothing_uncovered() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(uncovered_requirements(tmp.path(), "demo").is_empty());
    }

    #[test]
    fn fr_ids_normalize_so_fr_1_equals_fr_001() {
        // P1-7: a PRD that writes FR-001 and a tasks file that writes FR-1 (or
        // FR-01) must be treated as the SAME requirement — no phantom "uncovered".
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(
            root.join("output").join("demo-prd.md"),
            "| FR-001 | 登录 |\n| FR-002 | 登出 |\n| FR-010 | 注册 |",
        )
        .unwrap();
        let cdir = root.join(".umadev").join("changes").join("demo-20260101");
        std::fs::create_dir_all(&cdir).unwrap();
        // Tasks cite the SAME requirements with un-padded ids: FR-1, FR-2, FR-10.
        std::fs::write(
            cdir.join("tasks.md"),
            "- [ ] 登录 _(FR-1)_\n- [ ] 登出 _(FR-2)_\n- [ ] 注册 _(FR-10)_",
        )
        .unwrap();
        assert!(
            uncovered_requirements(root, "demo").is_empty(),
            "FR-1/FR-2/FR-10 must cover FR-001/FR-002/FR-010"
        );
    }

    #[test]
    fn normalize_fr_canonicalises_padding() {
        assert_eq!(normalize_fr("1"), "FR-001");
        assert_eq!(normalize_fr("01"), "FR-001");
        assert_eq!(normalize_fr("001"), "FR-001");
        assert_eq!(normalize_fr("42"), "FR-042");
        assert_eq!(normalize_fr("1000"), "FR-1000"); // 4-digit keeps its width
    }

    #[test]
    fn latest_tasks_picks_newest_by_mtime_not_lexicographic() {
        // P1-6: a hand-named change `demo-hotfix` sorts lexicographically AFTER a
        // timestamped `demo-20260101`, so a naive name sort would read the OLDER
        // dir. The NEWER dir (by mtime) must win regardless of name.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(
            root.join("output").join("demo-prd.md"),
            "| FR-001 | login |\n| FR-002 | logout |",
        )
        .unwrap();
        let changes = root.join(".umadev").join("changes");
        // The OLDER, lexicographically-LARGER dir covers only FR-001.
        let older = changes.join("demo-hotfix");
        std::fs::create_dir_all(&older).unwrap();
        std::fs::write(older.join("tasks.md"), "- [ ] login _(FR-001)_").unwrap();
        // Make the timestamped dir clearly NEWER by mtime, even though its name
        // sorts BEFORE `demo-hotfix`. It covers BOTH requirements.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = changes.join("demo-20260101");
        std::fs::create_dir_all(&newer).unwrap();
        std::fs::write(
            newer.join("tasks.md"),
            "- [ ] login _(FR-001)_\n- [ ] logout _(FR-002)_",
        )
        .unwrap();
        // If latest_tasks picked the newer (correct) dir, BOTH are covered →
        // nothing uncovered. If it wrongly picked `demo-hotfix`, FR-002 leaks.
        assert!(
            uncovered_requirements(root, "demo").is_empty(),
            "the newest-by-mtime tasks dir must be the one consulted"
        );
    }

    #[test]
    fn oversized_prd_is_reported_unavailable_instead_of_clean() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path().join("output/demo-prd.md"),
            vec![b'x'; MAX_COVERAGE_FILE_BYTES + 1],
        )
        .unwrap();
        let findings = uncovered_requirements(tmp.path(), "demo");
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("[unavailable]"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_tasks_are_not_accepted_as_coverage_evidence() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(tmp.path().join("output/demo-prd.md"), "| FR-001 | login |").unwrap();
        let change = tmp.path().join(".umadev/changes/demo");
        std::fs::create_dir_all(&change).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "- [ ] fake _(FR-001)_").unwrap();
        symlink(outside.path(), change.join("tasks.md")).unwrap();

        let findings = uncovered_requirements(tmp.path(), "demo");
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("[unavailable]"));
    }

    #[test]
    fn relative_project_root_is_supported() {
        let tmp = tempfile::Builder::new()
            .prefix("umadev-coverage-relative-")
            .tempdir_in(".")
            .unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(tmp.path().join("output/demo-prd.md"), "| FR-001 | login |").unwrap();
        std::fs::write(
            tmp.path().join("output/demo-execution-plan.md"),
            "- [ ] login _(FR-001)_",
        )
        .unwrap();
        let cwd = std::env::current_dir().unwrap();
        let relative = tmp.path().strip_prefix(cwd).unwrap_or(tmp.path());
        assert!(uncovered_requirements(relative, "demo").is_empty());
    }

    /// A plan whose single step's title (or evidence) may cite requirement ids.
    fn save_plan_step_titled(root: &Path, title: &str) {
        use crate::plan_state::{AcceptanceSpec, Plan, PlanStep, StepFiles, StepKind, StepStatus};
        let plan = Plan {
            steps: vec![PlanStep {
                id: "auth".into(),
                title: title.into(),
                seat: crate::critics::Seat::BackendEngineer,
                kind: StepKind::Build,
                depends_on: Vec::new(),
                acceptance: AcceptanceSpec::SourcePresent,
                evidence: Vec::new(),
                files: StepFiles {
                    create: vec!["src/auth/".into()],
                    modify: Vec::new(),
                },
                status: StepStatus::Pending,
            }],
            risks: Vec::new(),
            open_questions: Vec::new(),
        };
        crate::plan_state::save(&plan, root).unwrap();
    }

    #[test]
    fn requirement_coverage_is_traced_by_the_director_plan_and_architecture_doc() {
        // The director writes no legacy task list: its owned plan and the
        // architecture doc are what trace a requirement to the work.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(
            root.join("output/demo-prd.md"),
            "| FR-001 | login |\n| FR-002 | logout |\n| FR-003 | signup |\n",
        )
        .unwrap();
        // Nothing yet a requirement could be traced to: not judged, never "all
        // uncovered", and the reason is in the user's language.
        assert_eq!(
            requirement_coverage(root, "demo"),
            RequirementCoverage::Unavailable(
                umadev_i18n::tl("qc.coverage_nothing_to_trace").to_string()
            )
        );

        save_plan_step_titled(root, "实现登录接口 (FR-001)");
        std::fs::write(
            root.join("output/demo-architecture.md"),
            "| POST | /api/logout | end the session (FR-2) |\n",
        )
        .unwrap();
        assert_eq!(
            requirement_coverage(root, "demo"),
            RequirementCoverage::Uncovered(vec!["FR-003".to_string()]),
            "FR-003 is the one requirement nothing traces"
        );
        // The legacy spec gate still reads only its own task artifacts.
        assert_eq!(uncovered_requirements(root, "demo").len(), 3);

        save_plan_step_titled(root, "实现登录与注册 (FR-001, FR-003)");
        assert_eq!(
            requirement_coverage(root, "demo"),
            RequirementCoverage::Covered
        );
    }

    #[test]
    fn requirement_coverage_without_declared_requirements_is_covered() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert_eq!(
            requirement_coverage(tmp.path(), "demo"),
            RequirementCoverage::Covered
        );
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(tmp.path().join("output/demo-prd.md"), "# PRD\n\nno ids\n").unwrap();
        assert_eq!(
            requirement_coverage(tmp.path(), "demo"),
            RequirementCoverage::Covered
        );
    }

    #[test]
    fn requirement_coverage_with_an_unreadable_trace_is_unavailable() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(
            tmp.path().join("output/demo-prd.md"),
            "| FR-001 | login |\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("output/demo-architecture.md"),
            vec![b'x'; MAX_COVERAGE_FILE_BYTES + 1],
        )
        .unwrap();
        assert!(matches!(
            requirement_coverage(tmp.path(), "demo"),
            RequirementCoverage::Unavailable(reason) if reason.contains("demo-architecture.md")
        ));
    }
}
