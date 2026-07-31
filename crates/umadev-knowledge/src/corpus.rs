//! Canonical knowledge-corpus discovery shared by lexical retrieval, vectors,
//! digests, and agent-facing previews.
//!
//! A project's `knowledge/` directory is additive: it never replaces UmaDev's
//! bundled curated corpus. Every root carries an explicit origin and scope so
//! downstream indexes can preserve provenance even when relative paths collide.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use umadev_state::memory::{MemoryPolicy, MemoryStore};

const MAX_PROVENANCE_HEADER_FILE_BYTES: u64 = 1024 * 1024;

/// Why a corpus root is present.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    Default,
)]
#[serde(rename_all = "snake_case")]
pub enum CorpusOrigin {
    /// Legacy/standalone chunk with no stamped provenance.
    #[default]
    Unknown,
    /// The curated standards corpus shipped and staged by UmaDev.
    BundledCurated,
    /// Project-owned knowledge, including `knowledge/custom` and configured
    /// project-relative expert directories.
    ProjectCustom,
    /// Installed project skill packages (`knowledge/skills` or
    /// `.umadev/skills`).
    ProjectSkillPackage,
    /// Project-local sediment under `.umadev/learned`.
    ProjectLearned,
    /// Privacy-reviewed global sediment under `~/.umadev/learned`.
    GlobalSafeLearned,
}

impl CorpusOrigin {
    /// Stable wire/cache spelling.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::BundledCurated => "bundled_curated",
            Self::ProjectCustom => "project_custom",
            Self::ProjectSkillPackage => "project_skill_package",
            Self::ProjectLearned => "project_learned",
            Self::GlobalSafeLearned => "global_safe_learned",
        }
    }

    /// Whether this source is learned/sedimented rather than authored corpus.
    #[must_use]
    pub const fn is_learned(self) -> bool {
        matches!(self, Self::ProjectLearned | Self::GlobalSafeLearned)
    }
}

/// Authority boundary a corpus root belongs to.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
    Default,
)]
#[serde(rename_all = "snake_case")]
pub enum CorpusScope {
    /// Legacy/standalone chunk with no stamped scope.
    #[default]
    Unknown,
    /// Product-owned bundled material.
    Bundled,
    /// Material owned by the current project.
    Project,
    /// Cross-project material admitted through the global safety policy.
    Global,
}

impl CorpusScope {
    /// Stable wire/cache spelling.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Bundled => "bundled",
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

/// One canonical directory in the ordered corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorpusRoot {
    path: PathBuf,
    origin: CorpusOrigin,
    scope: CorpusScope,
}

impl CorpusRoot {
    /// Canonical root path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Root provenance.
    #[must_use]
    pub const fn origin(&self) -> CorpusOrigin {
        self.origin
    }

    /// Root authority boundary.
    #[must_use]
    pub const fn scope(&self) -> CorpusScope {
        self.scope
    }
}

/// One canonical markdown file selected from a [`CorpusSet`].
#[derive(Debug, Clone)]
pub struct CorpusFile {
    path: PathBuf,
    root: std::sync::Arc<umadev_state::fs::RootedDir>,
    relative: PathBuf,
    relative_path: String,
    len: u64,
    modified: Option<std::time::SystemTime>,
    origin: CorpusOrigin,
    scope: CorpusScope,
}

impl PartialEq for CorpusFile {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.relative == other.relative
            && self.relative_path == other.relative_path
            && self.len == other.len
            && self.modified == other.modified
            && self.origin == other.origin
            && self.scope == other.scope
    }
}

impl Eq for CorpusFile {}

impl CorpusFile {
    /// Canonical absolute path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Slash-normalized path relative to the most-specific owning root.
    #[must_use]
    pub fn relative_path(&self) -> &str {
        &self.relative_path
    }

    /// File provenance.
    #[must_use]
    pub const fn origin(&self) -> CorpusOrigin {
        self.origin
    }

    /// File authority boundary.
    #[must_use]
    pub const fn scope(&self) -> CorpusScope {
        self.scope
    }

    pub(crate) const fn len(&self) -> u64 {
        self.len
    }

    pub(crate) const fn modified(&self) -> Option<std::time::SystemTime> {
        self.modified
    }

    pub(crate) fn read_bounded(&self, max_bytes: u64) -> std::io::Result<Vec<u8>> {
        self.root.read_bounded(&self.relative, max_bytes)
    }
}

/// Ordered, canonical, provenance-aware knowledge roots.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorpusSet {
    roots: Vec<CorpusRoot>,
    recall_filter: Option<CorpusRecallFilter>,
}

/// Immutable policy snapshot attached only to product-discovered corpora.
///
/// `CorpusSet::from_roots` intentionally has no filter: callers that explicitly
/// supply already-classified roots keep a pure, deterministic library API.
/// Normal UmaDev discovery uses this snapshot so a source disabled after
/// inventory cannot re-enter through an overlapping parent directory.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CorpusRecallFilter {
    enabled: BTreeSet<MemoryStore>,
    project_learned: Option<PathBuf>,
    project_learned_skills: Option<PathBuf>,
    project_skill_packages: Vec<PathBuf>,
    global_learned: Option<PathBuf>,
    ambiguous_learned: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecallClass {
    BundledKnowledge,
    CustomKnowledge,
    SkillPackages,
    LessonSediment,
    LearnedSkillMirrors,
    GlobalLessonProjection,
    GlobalLessonsManual,
    UnmanagedExplicit,
    Reject,
}

impl CorpusRecallFilter {
    fn from_boundaries(project_root: &Path, global_boundary: Option<&Path>) -> Self {
        let project_boundary = canonical_real_dir(project_root);
        let global_boundary = global_boundary.and_then(canonical_real_dir);
        let project_policy = project_boundary
            .as_ref()
            .and_then(|root| umadev_state::memory::load_policy(root).ok());
        let global_policy = global_boundary
            .as_ref()
            .and_then(|root| umadev_state::memory::load_policy(root).ok());
        // Keep lexical children of an already canonical authority boundary.
        // They still classify files created after this snapshot; canonicalising
        // only existing children would let a newly-created nested skill folder
        // temporarily inherit its enabled parent source policy.
        let project_learned = project_boundary
            .as_ref()
            .map(|root| root.join(".umadev/learned"));
        let project_learned_skills = project_boundary
            .as_ref()
            .map(|root| root.join(".umadev/learned/skills"));
        let project_skill_packages = project_boundary
            .iter()
            .flat_map(|root| [root.join("knowledge/skills"), root.join(".umadev/skills")])
            .collect();
        let global_learned = global_boundary
            .as_ref()
            .map(|root| root.join(".umadev/learned"));
        let ambiguous_learned = project_learned
            .as_ref()
            .filter(|project| global_learned.as_ref() == Some(*project))
            .cloned();
        let mut enabled = BTreeSet::new();
        for store in [
            MemoryStore::CustomKnowledge,
            MemoryStore::SkillPackages,
            MemoryStore::LessonSediment,
            MemoryStore::LearnedSkillMirrors,
        ] {
            if policy_recall(project_policy.as_ref(), store) {
                enabled.insert(store);
            }
        }
        for store in [
            MemoryStore::BundledKnowledge,
            MemoryStore::GlobalLessonProjection,
            MemoryStore::GlobalLessonsManual,
        ] {
            if policy_recall(global_policy.as_ref(), store) {
                enabled.insert(store);
            }
        }
        Self {
            enabled,
            project_learned,
            project_learned_skills,
            project_skill_packages,
            global_learned,
            ambiguous_learned,
        }
    }

    fn allows(&self, file: &CorpusFile) -> bool {
        let store = match self.classify(file) {
            RecallClass::BundledKnowledge => MemoryStore::BundledKnowledge,
            RecallClass::CustomKnowledge => MemoryStore::CustomKnowledge,
            RecallClass::SkillPackages => MemoryStore::SkillPackages,
            RecallClass::LessonSediment => MemoryStore::LessonSediment,
            RecallClass::LearnedSkillMirrors => MemoryStore::LearnedSkillMirrors,
            RecallClass::GlobalLessonProjection => MemoryStore::GlobalLessonProjection,
            RecallClass::GlobalLessonsManual => MemoryStore::GlobalLessonsManual,
            RecallClass::UnmanagedExplicit => return true,
            RecallClass::Reject => return false,
        };
        self.enabled.contains(&store)
    }

    fn classify(&self, file: &CorpusFile) -> RecallClass {
        // Managed ownership wins over caller/root labels. This order closes the
        // nested-root bypass where a disabled `knowledge/skills` file could be
        // rediscovered through its enabled `knowledge/` parent.
        if self
            .ambiguous_learned
            .as_ref()
            .is_some_and(|root| file.path.starts_with(root))
        {
            return RecallClass::Reject;
        }
        if self
            .project_learned_skills
            .as_ref()
            .is_some_and(|root| file.path.starts_with(root))
        {
            return RecallClass::LearnedSkillMirrors;
        }
        if self
            .project_learned
            .as_ref()
            .is_some_and(|root| file.path.starts_with(root))
        {
            return RecallClass::LessonSediment;
        }
        if self
            .project_skill_packages
            .iter()
            .any(|root| file.path.starts_with(root))
        {
            return RecallClass::SkillPackages;
        }
        if self
            .global_learned
            .as_ref()
            .is_some_and(|root| file.path.starts_with(root))
        {
            return classify_global_learned(file);
        }
        match file.origin {
            CorpusOrigin::BundledCurated => RecallClass::BundledKnowledge,
            CorpusOrigin::ProjectCustom => RecallClass::CustomKnowledge,
            CorpusOrigin::ProjectSkillPackage => RecallClass::SkillPackages,
            CorpusOrigin::ProjectLearned => RecallClass::LessonSediment,
            CorpusOrigin::GlobalSafeLearned => classify_global_learned(file),
            CorpusOrigin::Unknown => RecallClass::UnmanagedExplicit,
        }
    }
}

fn policy_recall(policy: Option<&MemoryPolicy>, store: MemoryStore) -> bool {
    policy.is_some_and(|policy| policy.recall_enabled(store))
}

fn classify_global_learned(file: &CorpusFile) -> RecallClass {
    let Ok(bytes) = file.read_bounded(MAX_PROVENANCE_HEADER_FILE_BYTES) else {
        return RecallClass::Reject;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return RecallClass::Reject;
    };
    match crate::front_matter_field(text, "maintainer") {
        crate::FrontMatterField::Value("auto-sediment")
            if crate::front_matter_field(text, "global_safety")
                == crate::FrontMatterField::Value("classifier-family-v2") =>
        {
            RecallClass::GlobalLessonProjection
        }
        crate::FrontMatterField::Value("auto-sediment") => RecallClass::Reject,
        crate::FrontMatterField::NoHeader
        | crate::FrontMatterField::Missing
        | crate::FrontMatterField::Value(_) => RecallClass::GlobalLessonsManual,
        crate::FrontMatterField::Invalid => RecallClass::Reject,
    }
}

impl CorpusSet {
    /// An empty corpus.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            roots: Vec::new(),
            recall_filter: None,
        }
    }

    /// Build a canonical set from explicitly classified roots. Invalid,
    /// missing, symlink-root, and duplicate paths are skipped. First identity
    /// wins, making the caller's origin precedence deterministic.
    #[must_use]
    pub fn from_roots(
        roots: impl IntoIterator<Item = (PathBuf, CorpusOrigin, CorpusScope)>,
    ) -> Self {
        let mut seen = BTreeSet::<String>::new();
        let mut selected = Vec::new();
        for (path, origin, scope) in roots {
            let Some(path) = canonical_real_dir(&path) else {
                continue;
            };
            if !seen.insert(canonical_key(&path)) {
                continue;
            }
            selected.push(CorpusRoot {
                path,
                origin,
                scope,
            });
        }
        Self {
            roots: selected,
            recall_filter: None,
        }
    }

    /// Ordered roots.
    #[must_use]
    pub fn roots(&self) -> &[CorpusRoot] {
        &self.roots
    }

    /// Whether no valid roots were discovered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Canonical root paths, preserving corpus order.
    #[must_use]
    pub fn paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|root| root.path.clone()).collect()
    }

    /// Stable, canonical, no-follow markdown files across every root.
    ///
    /// Nested roots are intentional (`knowledge/skills` inside `knowledge`). A
    /// file reachable through both is emitted once and attributed to the most
    /// specific root, preserving skill-package provenance without double
    /// indexing it.
    #[must_use]
    pub fn markdown_files(&self) -> Vec<CorpusFile> {
        #[derive(Clone)]
        struct Candidate {
            file: CorpusFile,
            root_index: usize,
            specificity: usize,
        }

        let mut chosen = BTreeMap::<String, Candidate>::new();
        for (root_index, root) in self.roots.iter().enumerate() {
            let Ok(rooted) = umadev_state::fs::RootedDir::open_no_follow(&root.path) else {
                continue;
            };
            let Some(paths) = crate::index::walk_md_rooted(&rooted) else {
                continue;
            };
            let rooted = std::sync::Arc::new(rooted);
            for selected in paths {
                let canonical = root.path.join(&selected.relative);
                let relative_path = slash_path(&selected.relative);
                let candidate = Candidate {
                    file: CorpusFile {
                        path: canonical.clone(),
                        root: std::sync::Arc::clone(&rooted),
                        relative: selected.relative,
                        relative_path,
                        len: selected.len,
                        modified: selected.modified,
                        origin: root.origin,
                        scope: root.scope,
                    },
                    root_index,
                    specificity: root.path.components().count(),
                };
                let key = canonical_key(&canonical);
                match chosen.get(&key) {
                    Some(current)
                        if current.specificity > candidate.specificity
                            || current.specificity == candidate.specificity
                                && current.root_index <= candidate.root_index => {}
                    _ => {
                        chosen.insert(key, candidate);
                    }
                }
            }
        }
        let mut files = chosen
            .into_values()
            .filter(|candidate| {
                self.recall_filter
                    .as_ref()
                    .is_none_or(|filter| filter.allows(&candidate.file))
            })
            .collect::<Vec<_>>();
        files.sort_by(|left, right| {
            left.root_index
                .cmp(&right.root_index)
                .then_with(|| left.file.relative_path.cmp(&right.file.relative_path))
                .then_with(|| left.file.path.cmp(&right.file.path))
        });
        files.into_iter().map(|candidate| candidate.file).collect()
    }
}

fn with_recall_filter(mut corpus: CorpusSet, filter: CorpusRecallFilter) -> CorpusSet {
    corpus.recall_filter = Some(filter);
    corpus
}

/// Discover the complete corpus used by retrieval and vector indexing.
///
/// Stable precedence is bundled curated, project custom, project skill
/// packages, project learned, then global safety-reviewed learned material.
/// `knowledge_hint` preserves the path-based retrieval API: an external hint is
/// a bundled/curated source, while an explicit project `knowledge/` hint is an
/// exact project-only request and does not silently pull a staged home corpus.
/// Pass `None` for normal product discovery (bundled + project). `custom_dirs`
/// must be project-relative and cannot escape through `..` or a symlink root.
#[must_use]
pub fn knowledge_roots(
    project_root: &Path,
    knowledge_hint: Option<&Path>,
    custom_dirs: &[String],
) -> CorpusSet {
    // An explicit path is a self-contained library request. In particular it
    // must not inspect a developer's real HOME during tests or embedding use.
    let global_boundary = knowledge_hint.is_none().then(home_dir).flatten();
    discover_knowledge_roots(
        project_root,
        knowledge_hint,
        custom_dirs,
        global_boundary.as_deref(),
    )
}

/// Discover the product corpus and enforce the effective memory recall policy
/// for every source leaf before indexing.
///
/// `global_boundary` is explicit by design: the binary resolves the current
/// user's real HOME once and passes it here, while tests and embedders can use a
/// temporary boundary without reading or mutating the process user's account.
/// A missing or malformed policy is privacy-conservative: the affected scope's
/// sources are excluded. Explicit [`CorpusSet::from_roots`] remains unfiltered.
#[must_use]
pub fn knowledge_roots_with_recall_policy(
    project_root: &Path,
    knowledge_hint: Option<&Path>,
    custom_dirs: &[String],
    global_boundary: Option<&Path>,
) -> CorpusSet {
    let global_boundary = global_boundary.and_then(canonical_real_dir);
    let filter = CorpusRecallFilter::from_boundaries(project_root, global_boundary.as_deref());
    with_recall_filter(
        discover_knowledge_roots(
            project_root,
            knowledge_hint,
            custom_dirs,
            global_boundary.as_deref(),
        ),
        filter,
    )
}

fn discover_knowledge_roots(
    project_root: &Path,
    knowledge_hint: Option<&Path>,
    custom_dirs: &[String],
    global_boundary: Option<&Path>,
) -> CorpusSet {
    let project_knowledge = project_root.join("knowledge");
    let project_knowledge_id = canonical_real_dir(&project_knowledge).map(|p| canonical_key(&p));
    let hint = knowledge_hint.and_then(canonical_real_dir);
    let hint_is_project = hint
        .as_ref()
        .is_some_and(|path| project_knowledge_id.as_deref() == Some(canonical_key(path).as_str()));

    let mut roots = Vec::<(PathBuf, CorpusOrigin, CorpusScope)>::new();

    // Exactly one bundled corpus version wins. An explicit external hint is
    // authoritative. Automatic env/home discovery happens only when there was
    // no explicit hint; this keeps path-based library calls and tests isolated
    // while the product-level `knowledge_corpus()` opts into the complete set.
    let explicit_bundled = hint.as_ref().filter(|_| !hint_is_project).cloned();
    let env_bundled = std::env::var_os("UMADEV_KNOWLEDGE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let staged_bundled = global_boundary.map(|home| home.join(".umadev").join("knowledge"));
    let bundled = if knowledge_hint.is_some() {
        explicit_bundled
    } else {
        env_bundled
            .and_then(|path| canonical_real_dir(&path))
            .or_else(|| staged_bundled.and_then(|path| canonical_real_dir(&path)))
    };
    if let Some(bundled) = bundled {
        roots.push((bundled, CorpusOrigin::BundledCurated, CorpusScope::Bundled));
    }

    roots.push((
        project_knowledge,
        CorpusOrigin::ProjectCustom,
        CorpusScope::Project,
    ));
    for custom in custom_dirs {
        if let Some(path) = safe_project_relative_dir(project_root, custom) {
            roots.push((path, CorpusOrigin::ProjectCustom, CorpusScope::Project));
        }
    }

    roots.push((
        project_root.join("knowledge").join("skills"),
        CorpusOrigin::ProjectSkillPackage,
        CorpusScope::Project,
    ));
    roots.push((
        project_root.join(".umadev").join("skills"),
        CorpusOrigin::ProjectSkillPackage,
        CorpusScope::Project,
    ));

    if let Some(learned) = managed_child_dir(project_root, "learned") {
        roots.push((learned, CorpusOrigin::ProjectLearned, CorpusScope::Project));
    }
    if let Some(home) = global_boundary {
        if let Some(learned) = managed_child_dir(home, "learned") {
            roots.push((
                learned,
                CorpusOrigin::GlobalSafeLearned,
                CorpusScope::Global,
            ));
        }
    }
    CorpusSet::from_roots(roots)
}

/// Classify an older path-only corpus list for compatibility callers.
#[must_use]
pub(crate) fn corpus_from_paths(project_root: &Path, paths: &[PathBuf]) -> CorpusSet {
    let local_learned = managed_child_dir(project_root, "learned")
        .and_then(|path| canonical_real_dir(&path))
        .map(|path| canonical_key(&path));
    let project_knowledge =
        canonical_real_dir(&project_root.join("knowledge")).map(|path| canonical_key(&path));
    let roots = paths.iter().map(|path| {
        let canonical = canonical_real_dir(path);
        let key = canonical.as_ref().map(|path| canonical_key(path));
        let skill = path.ends_with(Path::new("knowledge/skills"))
            || path.ends_with(Path::new(".umadev/skills"));
        let learned = path.file_name().is_some_and(|name| name == "learned")
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == ".umadev");
        let (origin, scope) = if skill {
            (CorpusOrigin::ProjectSkillPackage, CorpusScope::Project)
        } else if learned && key == local_learned {
            (CorpusOrigin::ProjectLearned, CorpusScope::Project)
        } else if learned {
            (CorpusOrigin::GlobalSafeLearned, CorpusScope::Global)
        } else if key == project_knowledge {
            (CorpusOrigin::ProjectCustom, CorpusScope::Project)
        } else {
            (CorpusOrigin::BundledCurated, CorpusScope::Bundled)
        };
        (path.clone(), origin, scope)
    });
    CorpusSet::from_roots(roots)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn canonical_real_dir(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() {
        return None;
    }
    std::fs::canonicalize(path).ok()
}

fn managed_child_dir(boundary: &Path, child: &str) -> Option<PathBuf> {
    let boundary = std::fs::canonicalize(boundary).ok()?;
    let umadev = boundary.join(".umadev");
    let umadev_meta = std::fs::symlink_metadata(&umadev).ok()?;
    if umadev_meta.file_type().is_symlink() || !umadev_meta.file_type().is_dir() {
        return None;
    }
    let path = umadev.join(child);
    canonical_real_dir(&path)
}

fn safe_project_relative_dir(project_root: &Path, value: &str) -> Option<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let relative = Path::new(value);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let project = std::fs::canonicalize(project_root).ok()?;
    let candidate = canonical_real_dir(&project.join(relative))?;
    candidate.starts_with(&project).then_some(candidate)
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn canonical_key(path: &Path) -> String {
    let key = slash_path(path);
    #[cfg(windows)]
    {
        key.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn bare_project_keeps_explicit_bundled_corpus() {
        let project = tempfile::tempdir().unwrap();
        let bundled = tempfile::tempdir().unwrap();
        write(&bundled.path().join("backend/bundled.md"), "# bundled");
        let set = knowledge_roots(project.path(), Some(bundled.path()), &[]);
        let bundled_root = set
            .roots()
            .iter()
            .find(|root| root.origin() == CorpusOrigin::BundledCurated)
            .expect("explicit bundled root");
        assert_eq!(bundled_root.path(), bundled.path().canonicalize().unwrap());
        assert!(set.markdown_files().iter().any(|file| {
            file.origin() == CorpusOrigin::BundledCurated
                && file.relative_path() == "backend/bundled.md"
        }));
    }

    #[test]
    fn project_custom_and_skill_packages_are_additive_to_bundled() {
        let project = tempfile::tempdir().unwrap();
        let bundled = tempfile::tempdir().unwrap();
        write(&bundled.path().join("curated.md"), "# curated");
        write(&project.path().join("knowledge/custom/team.md"), "# team");
        write(
            &project.path().join("knowledge/skills/review/SKILL.md"),
            "# review skill",
        );
        let set = knowledge_roots(project.path(), Some(bundled.path()), &[]);
        let files = set.markdown_files();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].origin(), CorpusOrigin::BundledCurated);
        assert_eq!(files[1].origin(), CorpusOrigin::ProjectCustom);
        assert_eq!(files[2].origin(), CorpusOrigin::ProjectSkillPackage);
        assert_eq!(files[2].relative_path(), "review/SKILL.md");
    }

    #[test]
    fn canonical_duplicates_and_symlink_roots_never_duplicate_files() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join("knowledge/a.md"), "# a");
        let duplicate = root.path().join("knowledge/.");
        let roots = vec![
            (
                root.path().join("knowledge"),
                CorpusOrigin::ProjectCustom,
                CorpusScope::Project,
            ),
            (
                duplicate,
                CorpusOrigin::BundledCurated,
                CorpusScope::Bundled,
            ),
        ];
        #[cfg(unix)]
        let roots = {
            let mut roots = roots;
            std::os::unix::fs::symlink(root.path().join("knowledge"), root.path().join("linked"))
                .unwrap();
            roots.push((
                root.path().join("linked"),
                CorpusOrigin::BundledCurated,
                CorpusScope::Bundled,
            ));
            roots
        };
        let set = CorpusSet::from_roots(roots);
        assert_eq!(set.roots().len(), 1);
        assert_eq!(set.markdown_files().len(), 1);
        assert_eq!(set.roots()[0].origin(), CorpusOrigin::ProjectCustom);
    }

    #[cfg(unix)]
    #[test]
    fn selected_file_read_stays_on_the_inventoried_root_generation() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("knowledge");
        std::fs::create_dir(&root).unwrap();
        write(&root.join("source.md"), "# original");
        let set = CorpusSet::from_roots([(
            root.clone(),
            CorpusOrigin::ProjectCustom,
            CorpusScope::Project,
        )]);
        let file = set.markdown_files().pop().unwrap();

        let moved = parent.path().join("knowledge-moved");
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        write(&root.join("source.md"), "# replacement");

        let text = String::from_utf8(file.read_bounded(1024).unwrap()).unwrap();
        assert_eq!(text, "# original");
    }

    #[test]
    fn root_and_file_order_is_stable_and_origin_ordered() {
        let project = tempfile::tempdir().unwrap();
        let bundled = tempfile::tempdir().unwrap();
        write(&bundled.path().join("z.md"), "# z");
        write(&bundled.path().join("a.md"), "# a");
        write(&project.path().join("knowledge/z.md"), "# project z");
        write(&project.path().join("knowledge/a.md"), "# project a");
        let first = knowledge_roots(project.path(), Some(bundled.path()), &[]).markdown_files();
        let second = knowledge_roots(project.path(), Some(bundled.path()), &[]).markdown_files();
        let view = |files: &[CorpusFile]| {
            files
                .iter()
                .map(|file| (file.origin(), file.relative_path().to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(view(&first), view(&second));
        assert_eq!(
            view(&first),
            vec![
                (CorpusOrigin::BundledCurated, "a.md".to_string()),
                (CorpusOrigin::BundledCurated, "z.md".to_string()),
                (CorpusOrigin::ProjectCustom, "a.md".to_string()),
                (CorpusOrigin::ProjectCustom, "z.md".to_string()),
            ]
        );
    }

    fn policy_fixture() -> (tempfile::TempDir, tempfile::TempDir) {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            &home.path().join(".umadev/knowledge/bundled-only.md"),
            "# bundled",
        );
        write(&project.path().join("knowledge/custom-only.md"), "# custom");
        write(
            &project
                .path()
                .join("knowledge/skills/package/package-only.md"),
            "# package",
        );
        write(
            &project.path().join(".umadev/learned/api/sediment-only.md"),
            "---\nmaintainer: auto-sediment\n---\n# sediment",
        );
        write(
            &project.path().join(".umadev/learned/skills/mirror-only.md"),
            "# learned skill mirror",
        );
        write(
            &home
                .path()
                .join(".umadev/learned/api/projection-only.md"),
            "---\nmaintainer: auto-sediment\nglobal_safety: classifier-family-v2\n---\n# projection",
        );
        write(
            &home.path().join(".umadev/learned/manual-only.md"),
            "# manually curated global lesson",
        );
        (project, home)
    }

    fn policy_files(project: &Path, home: &Path, custom_dirs: &[String]) -> Vec<String> {
        knowledge_roots_with_recall_policy(project, None, custom_dirs, Some(home))
            .markdown_files()
            .iter()
            .filter_map(|file| {
                file.path()
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
            })
            .collect()
    }

    #[test]
    fn each_source_leaf_recall_policy_excludes_only_its_owned_files() {
        let cases = [
            (
                CorpusScope::Bundled,
                MemoryStore::BundledKnowledge,
                "bundled-only.md",
            ),
            (
                CorpusScope::Project,
                MemoryStore::CustomKnowledge,
                "custom-only.md",
            ),
            (
                CorpusScope::Project,
                MemoryStore::SkillPackages,
                "package-only.md",
            ),
            (
                CorpusScope::Project,
                MemoryStore::LessonSediment,
                "sediment-only.md",
            ),
            (
                CorpusScope::Project,
                MemoryStore::LearnedSkillMirrors,
                "mirror-only.md",
            ),
            (
                CorpusScope::Global,
                MemoryStore::GlobalLessonProjection,
                "projection-only.md",
            ),
            (
                CorpusScope::Global,
                MemoryStore::GlobalLessonsManual,
                "manual-only.md",
            ),
        ];
        for (scope, store, excluded) in cases {
            let (project, home) = policy_fixture();
            let mut project_policy = MemoryPolicy::default();
            let mut global_policy = MemoryPolicy::default();
            match scope {
                CorpusScope::Project => project_policy.set_recall(Some(store), false),
                CorpusScope::Bundled | CorpusScope::Global => {
                    global_policy.set_recall(Some(store), false);
                }
                CorpusScope::Unknown => unreachable!(),
            }
            umadev_state::memory::save_policy(project.path(), &project_policy).unwrap();
            umadev_state::memory::save_policy(home.path(), &global_policy).unwrap();
            let files = policy_files(project.path(), home.path(), &[]);
            assert_eq!(files.len(), 6, "{store:?} disabled: {files:?}");
            assert!(
                !files.iter().any(|name| name == excluded),
                "{store:?} did not exclude {excluded}: {files:?}"
            );
        }
    }

    #[test]
    fn nested_managed_sources_cannot_reenter_through_custom_parent_roots() {
        let (project, home) = policy_fixture();
        let mut policy = MemoryPolicy::default();
        policy.set_recall(Some(MemoryStore::SkillPackages), false);
        policy.set_recall(Some(MemoryStore::LearnedSkillMirrors), false);
        umadev_state::memory::save_policy(project.path(), &policy).unwrap();
        let files = policy_files(
            project.path(),
            home.path(),
            &[
                "knowledge/skills".to_string(),
                ".umadev/learned/skills".to_string(),
            ],
        );
        assert!(!files.iter().any(|name| name == "package-only.md"));
        assert!(!files.iter().any(|name| name == "mirror-only.md"));
        assert!(files.iter().any(|name| name == "custom-only.md"));
    }

    #[test]
    fn source_ownership_also_covers_nested_directories_created_after_snapshot() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(&project.path().join("knowledge/custom-only.md"), "# custom");
        let mut policy = MemoryPolicy::default();
        policy.set_recall(Some(MemoryStore::SkillPackages), false);
        umadev_state::memory::save_policy(project.path(), &policy).unwrap();
        let corpus =
            knowledge_roots_with_recall_policy(project.path(), None, &[], Some(home.path()));

        write(
            &project.path().join("knowledge/skills/late-package.md"),
            "# package created after corpus discovery",
        );
        let files = corpus.markdown_files();
        assert!(files
            .iter()
            .any(|file| file.path().ends_with("custom-only.md")));
        assert!(!files
            .iter()
            .any(|file| file.path().ends_with("late-package.md")));
    }

    #[test]
    fn scope_wide_recall_switches_are_independent() {
        let (project, home) = policy_fixture();
        let mut project_policy = MemoryPolicy::default();
        project_policy.set_recall(None, false);
        umadev_state::memory::save_policy(project.path(), &project_policy).unwrap();
        let files = policy_files(project.path(), home.path(), &[]);
        assert_eq!(
            files,
            vec![
                "bundled-only.md".to_string(),
                "projection-only.md".to_string(),
                "manual-only.md".to_string(),
            ]
        );

        let (project, home) = policy_fixture();
        let mut global_policy = MemoryPolicy::default();
        global_policy.set_recall(None, false);
        umadev_state::memory::save_policy(home.path(), &global_policy).unwrap();
        let files = policy_files(project.path(), home.path(), &[]);
        assert_eq!(
            files,
            vec![
                "custom-only.md".to_string(),
                "package-only.md".to_string(),
                "sediment-only.md".to_string(),
                "mirror-only.md".to_string(),
            ]
        );
    }

    #[test]
    fn overlapping_project_and_global_learned_authorities_fail_closed() {
        let boundary = tempfile::tempdir().unwrap();
        write(
            &boundary.path().join(".umadev/learned/manual.md"),
            "# ambiguous authority",
        );
        let corpus =
            knowledge_roots_with_recall_policy(boundary.path(), None, &[], Some(boundary.path()));
        assert!(corpus.markdown_files().is_empty());
    }

    #[test]
    fn global_manual_and_projection_classification_fails_closed_on_ambiguity() {
        let (project, home) = policy_fixture();
        write(
            &home.path().join(".umadev/learned/unsafe-auto.md"),
            "---\nmaintainer: auto-sediment\n---\n# missing safety marker",
        );
        write(
            &home.path().join(".umadev/learned/ambiguous-auto.md"),
            "---\nmaintainer: auto-sediment\nmaintainer: human\n---\n# ambiguous",
        );
        let files = policy_files(project.path(), home.path(), &[]);
        assert!(files.iter().any(|name| name == "projection-only.md"));
        assert!(files.iter().any(|name| name == "manual-only.md"));
        assert!(!files.iter().any(|name| name == "unsafe-auto.md"));
        assert!(!files.iter().any(|name| name == "ambiguous-auto.md"));

        let mut policy = MemoryPolicy::default();
        policy.set_recall(Some(MemoryStore::GlobalLessonProjection), false);
        umadev_state::memory::save_policy(home.path(), &policy).unwrap();
        let files = policy_files(project.path(), home.path(), &[]);
        assert!(!files.iter().any(|name| name == "projection-only.md"));
        assert!(files.iter().any(|name| name == "manual-only.md"));
    }

    #[test]
    fn malformed_scope_policy_excludes_that_scope_without_touching_real_home() {
        let (project, home) = policy_fixture();
        let policy_path = project.path().join(".umadev/memory/policy.toml");
        std::fs::create_dir_all(policy_path.parent().unwrap()).unwrap();
        std::fs::write(policy_path, "recall = maybe").unwrap();
        let files = policy_files(project.path(), home.path(), &[]);
        for project_file in [
            "custom-only.md",
            "package-only.md",
            "sediment-only.md",
            "mirror-only.md",
        ] {
            assert!(!files.iter().any(|name| name == project_file));
        }
        assert!(files.iter().any(|name| name == "bundled-only.md"));
        assert!(files.iter().any(|name| name == "projection-only.md"));
        assert!(files.iter().any(|name| name == "manual-only.md"));
    }
}
