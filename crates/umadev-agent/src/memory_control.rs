//! Inventory and policy operations for persisted UmaDev memory.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest as _, Sha256};

pub use umadev_state::memory::{MemoryPolicy, MemoryStore, RetentionEnforcement, StorePolicy};

const MAX_INVENTORY_FILES: usize = 20_000;
const MAX_INVENTORY_DEPTH: usize = 8;
const MAX_LIFECYCLE_NODES: usize = 40_000;
const MAX_LIFECYCLE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_EXPORT_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EXPORT_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EXPORT_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;
const EXPORT_MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Filesystem boundary in which a memory operation is allowed to act.
pub enum MemoryScope {
    /// State owned by one project under its canonical project root.
    Project,
    /// State explicitly owned by the current user's UmaDev home.
    Global,
}

/// A non-destructive selector grouping related leaf stores for policy changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryGroup {
    /// Raw experience, distilled rules, and their safe projections.
    Lessons,
    /// All automatically learned experience, facts, recipes, and skills.
    Learning,
    /// Curated sources, learned sources, and retrieval feedback.
    Knowledge,
    /// Chat sessions, input history, run notes, and open decisions.
    Conversation,
    /// Rebuildable indexes, mirrors, and sedimented views.
    Derived,
    /// Every leaf store available in the selected scope.
    All,
}

impl MemoryGroup {
    /// Parses a stable group identifier.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "lessons" => Some(Self::Lessons),
            "learning" => Some(Self::Learning),
            "knowledge" => Some(Self::Knowledge),
            "conversation" => Some(Self::Conversation),
            "derived" => Some(Self::Derived),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// One exact leaf store or a safe non-destructive group selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySelector {
    /// One exact store.
    Store(MemoryStore),
    /// A named group expanded only within the selected scope.
    Group(MemoryGroup),
}

impl MemorySelector {
    /// Parses a leaf store identifier first, then a group identifier.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        MemoryStore::parse(value)
            .map(Self::Store)
            .or_else(|| MemoryGroup::parse(value).map(Self::Group))
    }

    /// Expands the selector to disjoint leaf stores available in `scope`.
    #[must_use]
    pub fn stores(self, scope: MemoryScope) -> Vec<MemoryStore> {
        let candidates: &[MemoryStore] = match self {
            Self::Store(store) => {
                return store_supports_scope(store, scope)
                    .then_some(store)
                    .into_iter()
                    .collect()
            }
            Self::Group(MemoryGroup::Lessons) => &[
                MemoryStore::QualityFailures,
                MemoryStore::GateRevisions,
                MemoryStore::ValidatedPatterns,
                MemoryStore::TechDebt,
                MemoryStore::Pitfalls,
                MemoryStore::Beliefs,
                MemoryStore::PitfallReflections,
                MemoryStore::GateAdrs,
                MemoryStore::LessonSediment,
                MemoryStore::GlobalLessonProjection,
                MemoryStore::GlobalLessonsManual,
            ],
            Self::Group(MemoryGroup::Learning) => &[
                MemoryStore::QualityFailures,
                MemoryStore::GateRevisions,
                MemoryStore::ValidatedPatterns,
                MemoryStore::TechDebt,
                MemoryStore::Pitfalls,
                MemoryStore::Beliefs,
                MemoryStore::PitfallReflections,
                MemoryStore::GateAdrs,
                MemoryStore::Facts,
                MemoryStore::Recipes,
                MemoryStore::LearnedSkills,
                MemoryStore::KnowledgeReceipts,
                MemoryStore::KnowledgeUtility,
                MemoryStore::LessonSediment,
                MemoryStore::LearnedSkillMirrors,
                MemoryStore::GlobalLessonProjection,
            ],
            Self::Group(MemoryGroup::Knowledge) => &[
                MemoryStore::KnowledgeReceipts,
                MemoryStore::KnowledgeUtility,
                MemoryStore::CustomKnowledge,
                MemoryStore::SkillPackages,
                MemoryStore::LessonSediment,
                MemoryStore::LearnedSkillMirrors,
                MemoryStore::GlobalLessonProjection,
                MemoryStore::GlobalLessonsManual,
                MemoryStore::BundledKnowledge,
            ],
            Self::Group(MemoryGroup::Conversation) => &[
                MemoryStore::RunNotes,
                MemoryStore::ChatSessions,
                MemoryStore::InputHistory,
                MemoryStore::OpenDecisions,
            ],
            Self::Group(MemoryGroup::Derived) => &[
                MemoryStore::LessonSediment,
                MemoryStore::LearnedSkillMirrors,
                MemoryStore::KnowledgeIndex,
                MemoryStore::RepoMap,
            ],
            Self::Group(MemoryGroup::All) => &MemoryStore::ALL,
        };
        candidates
            .iter()
            .copied()
            .filter(|store| store_supports_scope(*store, scope))
            .collect()
    }

    /// Expands to the stores whose automatic capture is configurable.
    #[must_use]
    pub fn capture_stores(self, scope: MemoryScope) -> Vec<MemoryStore> {
        self.stores(scope)
            .into_iter()
            .filter(|store| store.capture_controllable())
            .collect()
    }

    /// Expands to the stores whose automatic recall is configurable.
    #[must_use]
    pub fn recall_stores(self, scope: MemoryScope) -> Vec<MemoryStore> {
        self.stores(scope)
            .into_iter()
            .filter(|store| store.recall_controllable())
            .collect()
    }

    /// Expands to authoritative/cache stores that may be soft-forgotten. The
    /// tombstone and its deletion audit are deliberately retained so `all`
    /// cannot recursively erase its own recovery evidence.
    #[must_use]
    pub fn forget_stores(self, scope: MemoryScope) -> Vec<MemoryStore> {
        self.stores(scope)
            .into_iter()
            .filter(|store| !matches!(store, MemoryStore::Tombstones | MemoryStore::DeletionAudit))
            .collect()
    }

    /// Expands to stores with an executable user-configurable age policy.
    #[must_use]
    pub fn retention_stores(self, scope: MemoryScope) -> Vec<MemoryStore> {
        self.stores(scope)
            .into_iter()
            .filter(|store| store.retention_enforcement() == RetentionEnforcement::PolicyOnly)
            .collect()
    }
}

impl MemoryScope {
    /// Parses a stable English or Chinese scope name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "project" | "项目" => Some(Self::Project),
            "global" | "全局" => Some(Self::Global),
            _ => None,
        }
    }

    /// Returns the stable command-line identifier for this scope.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// One memory store's policy and disk footprint.
pub struct MemoryInventoryEntry {
    /// Logical store represented by the entry.
    pub store: MemoryStore,
    /// Filesystem boundary used for the measurement.
    pub scope: MemoryScope,
    /// Number of regular files found without following links.
    pub files: usize,
    /// Total bytes in those regular files.
    pub bytes: u64,
    /// Effective capture policy, or `None` when capture is not configurable.
    pub capture: Option<bool>,
    /// Effective recall policy, or `None` when recall is not configurable.
    pub recall: Option<bool>,
    /// Configured retention period; `None` means no age-based limit.
    pub retention_days: Option<u32>,
    /// Whether retention is fixed in code, policy-only, or unavailable.
    pub retention_enforcement: RetentionEnforcement,
    /// Relative managed locations included in the measurement.
    pub locations: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Memory inventory for one scope, including any policy read failure.
pub struct MemoryInventory {
    /// Measured stores with their effective policies.
    pub entries: Vec<MemoryInventoryEntry>,
    /// Privacy-conservative policy error, when the policy could not be read.
    pub policy_error: Option<String>,
}

#[derive(Debug, Default)]
struct Footprint {
    files: usize,
    bytes: u64,
}

fn home_boundary() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("USERPROFILE")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .map(PathBuf::from)
        .and_then(|path| std::fs::canonicalize(path).ok())
        .filter(|path| umadev_state::fs::real_dir(path))
}

const fn store_supports_scope(store: MemoryStore, scope: MemoryScope) -> bool {
    match scope {
        MemoryScope::Project => store.supports_project_scope(),
        MemoryScope::Global => store.supports_global_scope(),
    }
}

/// Resolves and validates the canonical filesystem boundary for a scope.
pub fn scope_boundary(project_root: &Path, scope: MemoryScope) -> std::io::Result<PathBuf> {
    match scope {
        MemoryScope::Project => {
            let root = std::fs::canonicalize(project_root)?;
            if umadev_state::fs::real_dir(&root) {
                Ok(root)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "project root is not a real directory",
                ))
            }
        }
        MemoryScope::Global => home_boundary().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "HOME/USERPROFILE is unavailable",
            )
        }),
    }
}

/// Returns the effective automatic-capture policy for one store and scope.
#[must_use]
pub fn capture_enabled(project_root: &Path, scope: MemoryScope, store: MemoryStore) -> bool {
    store_supports_scope(store, scope)
        && scope_boundary(project_root, scope)
            .is_ok_and(|boundary| umadev_state::memory::capture_enabled(&boundary, store))
}

/// Returns the effective automatic-recall policy for one store and scope.
#[must_use]
pub fn recall_enabled(project_root: &Path, scope: MemoryScope, store: MemoryStore) -> bool {
    store_supports_scope(store, scope)
        && scope_boundary(project_root, scope)
            .is_ok_and(|boundary| umadev_state::memory::recall_enabled(&boundary, store))
}

fn logical_locations(store: MemoryStore, scope: MemoryScope) -> Vec<&'static str> {
    use MemoryScope::{Global, Project};
    use MemoryStore::{
        Beliefs, BundledKnowledge, ChatSessions, CustomKnowledge, DeletionAudit, EmbeddingModel,
        Facts, GateAdrs, GateRevisions, GlobalLessonProjection, GlobalLessonsManual, InputHistory,
        KnowledgeIndex, KnowledgeReceipts, KnowledgeUtility, LearnedSkillMirrors, LearnedSkills,
        LessonSediment, OpenDecisions, PitfallReflections, Pitfalls, QualityFailures, Recipes,
        RepoMap, RunNotes, SkillPackages, TechDebt, Tombstones, ValidatedPatterns,
    };
    match (scope, store) {
        (Project, QualityFailures) => vec![".umadev/learned/_raw/quality-failures.jsonl"],
        (Project, GateRevisions) => vec![".umadev/learned/_raw/gate-revisions.jsonl"],
        (Project, ValidatedPatterns) => {
            vec![".umadev/learned/_raw/validated-decisions.jsonl"]
        }
        (Project, TechDebt) => vec![".umadev/learned/_raw/tech-debt.jsonl"],
        (Project, Pitfalls) => vec![".umadev/learned/_raw/dev-errors.jsonl"],
        (Project, Beliefs) => vec![".umadev/learned/_raw/beliefs.jsonl"],
        (Project, PitfallReflections) => vec![".umadev/reflections"],
        (Project, GateAdrs) => vec![".umadev/decisions"],
        (Project, OpenDecisions) => vec!["docs/decisions/OPEN-DECISIONS.md"],
        (Project, Facts) => vec![".umadev/memory/facts.jsonl"],
        (Project, RunNotes) => vec![
            ".umadev/run-notes.md",
            ".umadev/run-notes.prev.md",
            ".umadev/run-notes.prev.pending.md",
        ],
        (Project, Recipes) => vec![".umadev/memory/recipes"],
        (Project, LearnedSkills) => vec![
            ".umadev/memory/learned-skills/skills.jsonl",
            ".umadev/memory/learned-skills/receipts",
            ".umadev/skills/skills.jsonl",
            ".umadev/skills/receipts",
        ],
        (Project, KnowledgeReceipts) => vec![
            ".umadev/learned/_raw/knowledge-receipts",
            ".umadev/learned/_raw/surfaced-chunks.json",
        ],
        (Project, CustomKnowledge) => vec!["knowledge/custom", ".umadev/knowledge.json"],
        (Project, SkillPackages) => vec![".umadev/skills", "knowledge/skills"],
        (Project, ChatSessions) => vec![".umadev/chat"],
        (Project, InputHistory) => vec![".umadev/input-history.txt"],
        (Project, LessonSediment) => vec![".umadev/learned/<domain>/lesson-*.md"],
        (Project, LearnedSkillMirrors) => vec![".umadev/learned/skills"],
        (Project, KnowledgeIndex) => vec![".umadev/kb-index"],
        (Project, RepoMap) => vec![".umadev/repomap-cache"],
        (Project | Global, Tombstones) => vec![".umadev/memory/tombstones"],
        (Project | Global, DeletionAudit) => {
            vec![".umadev/memory/audit/deletions"]
        }
        (Global, KnowledgeUtility) => vec![
            ".umadev/knowledge-outcomes",
            ".umadev/knowledge-usefulness.json",
        ],
        (Global, GlobalLessonProjection | GlobalLessonsManual) => {
            vec![".umadev/learned"]
        }
        (Global, BundledKnowledge) => vec![".umadev/knowledge"],
        (Global, EmbeddingModel) => vec![".umadev/embed-model"],
        _ => Vec::new(),
    }
}

fn add_footprint(path: &Path, depth: usize, footprint: &mut Footprint) {
    if depth > MAX_INVENTORY_DEPTH || footprint.files >= MAX_INVENTORY_FILES {
        return;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if umadev_state::fs::metadata_is_real_file(&metadata) {
        footprint.files += 1;
        footprint.bytes = footprint.bytes.saturating_add(metadata.len());
        return;
    }
    if !umadev_state::fs::metadata_is_real_dir(&metadata) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        add_footprint(&entry.path(), depth + 1, footprint);
        if footprint.files >= MAX_INVENTORY_FILES {
            break;
        }
    }
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
}

fn is_auto_sediment(path: &Path) -> bool {
    if !is_markdown(path) {
        return false;
    }
    let Ok(bytes) = umadev_state::fs::read_bounded(path, 64 * 1024) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return false;
    };
    umadev_knowledge::front_matter_field(text, "maintainer")
        == umadev_knowledge::FrontMatterField::Value("auto-sediment")
}

fn valid_skill_migration_marker(path: &Path) -> bool {
    let Ok(bytes) = umadev_state::fs::read_bounded(path, 16 * 1024) else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    value.get("version").and_then(serde_json::Value::as_u64) == Some(1)
        && value
            .get("store_sha256")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|hash| {
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

fn add_filtered_footprint(
    path: &Path,
    depth: usize,
    footprint: &mut Footprint,
    accept_file: &dyn Fn(&Path) -> bool,
    descend: &dyn Fn(&Path) -> bool,
) {
    if depth > MAX_INVENTORY_DEPTH || footprint.files >= MAX_INVENTORY_FILES {
        return;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if umadev_state::fs::metadata_is_real_file(&metadata) {
        if accept_file(path) {
            footprint.files += 1;
            footprint.bytes = footprint.bytes.saturating_add(metadata.len());
        }
        return;
    }
    if !umadev_state::fs::metadata_is_real_dir(&metadata) || !descend(path) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        add_filtered_footprint(&entry.path(), depth + 1, footprint, accept_file, descend);
        if footprint.files >= MAX_INVENTORY_FILES {
            break;
        }
    }
}

fn add_store_footprint(
    boundary: &Path,
    scope: MemoryScope,
    store: MemoryStore,
    footprint: &mut Footprint,
) {
    match (scope, store) {
        (MemoryScope::Project, MemoryStore::LessonSediment) => {
            let root = boundary.join(".umadev/learned");
            add_filtered_footprint(
                &root,
                0,
                footprint,
                &|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("lesson-"))
                        && is_auto_sediment(path)
                },
                &|path| {
                    path == root
                        || !matches!(
                            path.file_name().and_then(|name| name.to_str()),
                            Some("_raw" | "skills")
                        )
                },
            );
        }
        (MemoryScope::Global, MemoryStore::GlobalLessonProjection) => {
            let root = boundary.join(".umadev/learned");
            add_filtered_footprint(&root, 0, footprint, &|path| is_auto_sediment(path), &|_| {
                true
            });
        }
        (MemoryScope::Global, MemoryStore::GlobalLessonsManual) => {
            let root = boundary.join(".umadev/learned");
            add_filtered_footprint(
                &root,
                0,
                footprint,
                &|path| is_markdown(path) && !is_auto_sediment(path),
                &|_| true,
            );
        }
        (MemoryScope::Project, MemoryStore::SkillPackages) => {
            let installed = boundary.join(".umadev/skills");
            add_filtered_footprint(
                &installed,
                0,
                footprint,
                &|path| {
                    !matches!(
                        path.file_name().and_then(|name| name.to_str()),
                        Some("skills.jsonl")
                    ) && !path.starts_with(installed.join("receipts"))
                },
                &|path| path == installed || !path.starts_with(installed.join("receipts")),
            );
            add_footprint(&boundary.join("knowledge/skills"), 0, footprint);
        }
        (MemoryScope::Project, MemoryStore::LearnedSkills) => {
            let current = boundary.join(".umadev/memory/learned-skills");
            let legacy = boundary.join(".umadev/skills");
            let legacy_store = legacy.join("skills.jsonl");
            let use_current = valid_skill_migration_marker(&current.join("migration-v1.json"))
                || !std::fs::symlink_metadata(&legacy_store)
                    .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_file(&metadata));
            let selected = if use_current { current } else { legacy };
            add_footprint(&selected.join("skills.jsonl"), 0, footprint);
            add_footprint(&selected.join("receipts"), 0, footprint);
        }
        _ => {
            for location in logical_locations(store, scope) {
                if !location.contains('*') && !location.contains('<') {
                    add_footprint(&boundary.join(location), 0, footprint);
                }
            }
        }
    }
}

#[must_use]
/// Measures managed memory without following links or exposing stored content.
pub fn inventory(project_root: &Path, scope: MemoryScope) -> MemoryInventory {
    let Ok(boundary) = scope_boundary(project_root, scope) else {
        return MemoryInventory {
            entries: Vec::new(),
            policy_error: Some("scope boundary unavailable".to_string()),
        };
    };
    let policy_result = umadev_state::memory::load_policy(&boundary);
    let policy_error = policy_result.as_ref().err().map(ToString::to_string);
    let policy = policy_result.unwrap_or_else(|_| MemoryPolicy {
        capture: false,
        recall: false,
        ..MemoryPolicy::default()
    });
    let entries = MemoryStore::ALL
        .into_iter()
        .filter_map(|store| {
            let locations = logical_locations(store, scope);
            if locations.is_empty() {
                return None;
            }
            let mut footprint = Footprint::default();
            add_store_footprint(&boundary, scope, store, &mut footprint);
            Some(MemoryInventoryEntry {
                store,
                scope,
                files: footprint.files,
                bytes: footprint.bytes,
                capture: store
                    .capture_controllable()
                    .then(|| policy.capture_enabled(store)),
                recall: store
                    .recall_controllable()
                    .then(|| policy.recall_enabled(store)),
                retention_days: policy.retention_days(store),
                retention_enforcement: store.retention_enforcement(),
                locations: locations.into_iter().map(str::to_string).collect(),
            })
        })
        .collect();
    MemoryInventory {
        entries,
        policy_error,
    }
}

/// Changes capture globally within a scope or for one configurable store.
pub fn update_capture(
    project_root: &Path,
    scope: MemoryScope,
    store: Option<MemoryStore>,
    enabled: bool,
) -> std::io::Result<()> {
    if store.is_some_and(|store| !store.capture_controllable()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store capture is not user-controllable",
        ));
    }
    if store.is_some_and(|store| !store_supports_scope(store, scope)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store is unavailable in the selected scope",
        ));
    }
    let boundary = scope_boundary(project_root, scope)?;
    umadev_state::memory::update_policy(&boundary, |policy| {
        policy.set_capture(store, enabled);
        Ok(())
    })
    .map(|_| ())
}

/// Changes capture for a non-empty, disjoint set of leaf stores atomically.
pub fn update_capture_stores(
    project_root: &Path,
    scope: MemoryScope,
    stores: &[MemoryStore],
    enabled: bool,
) -> std::io::Result<()> {
    let stores: std::collections::BTreeSet<_> = stores.iter().copied().collect();
    if stores.is_empty()
        || stores
            .iter()
            .any(|store| !store.capture_controllable() || !store_supports_scope(*store, scope))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "capture selector contains no configurable store for this scope",
        ));
    }
    let boundary = scope_boundary(project_root, scope)?;
    umadev_state::memory::update_policy(&boundary, |policy| {
        for store in stores {
            policy.set_capture(Some(store), enabled);
        }
        Ok(())
    })
    .map(|_| ())
}

/// Changes recall globally within a scope or for one configurable store.
pub fn update_recall(
    project_root: &Path,
    scope: MemoryScope,
    store: Option<MemoryStore>,
    enabled: bool,
) -> std::io::Result<()> {
    if store.is_some_and(|store| !store.recall_controllable()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store recall is not user-controllable",
        ));
    }
    if store.is_some_and(|store| !store_supports_scope(store, scope)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store is unavailable in the selected scope",
        ));
    }
    let boundary = scope_boundary(project_root, scope)?;
    umadev_state::memory::update_policy(&boundary, |policy| {
        policy.set_recall(store, enabled);
        Ok(())
    })
    .map(|_| ())
}

/// Changes recall for a non-empty, disjoint set of leaf stores atomically.
pub fn update_recall_stores(
    project_root: &Path,
    scope: MemoryScope,
    stores: &[MemoryStore],
    enabled: bool,
) -> std::io::Result<()> {
    let stores: std::collections::BTreeSet<_> = stores.iter().copied().collect();
    if stores.is_empty()
        || stores
            .iter()
            .any(|store| !store.recall_controllable() || !store_supports_scope(*store, scope))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "recall selector contains no configurable store for this scope",
        ));
    }
    let boundary = scope_boundary(project_root, scope)?;
    umadev_state::memory::update_policy(&boundary, |policy| {
        for store in stores {
            policy.set_recall(Some(store), enabled);
        }
        Ok(())
    })
    .map(|_| ())
}

/// Sets or removes a store's age-based retention policy.
pub fn update_retention(
    project_root: &Path,
    scope: MemoryScope,
    store: MemoryStore,
    days: Option<u32>,
) -> std::io::Result<()> {
    if !store_supports_scope(store, scope) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store is unavailable in the selected scope",
        ));
    }
    if store.retention_enforcement() != RetentionEnforcement::PolicyOnly {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store does not support a configurable age policy",
        ));
    }
    if days == Some(0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "retention days must be greater than zero; use no value to clear the policy",
        ));
    }
    let boundary = scope_boundary(project_root, scope)?;
    umadev_state::memory::update_policy(&boundary, |policy| {
        policy.set_retention_days(store, days);
        Ok(())
    })
    .map(|_| ())
}

#[derive(Debug, Clone)]
struct ManagedFile {
    path: PathBuf,
    relative: PathBuf,
    bytes: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Result of an explicit, recoverable forget operation.
pub struct ForgetReport {
    /// Published tombstone identifier, absent when the selected stores were empty.
    pub tombstone_id: Option<String>,
    /// Number of active regular files moved into the tombstone payload.
    pub files: usize,
    /// Aggregate bytes moved without reading or logging their contents.
    pub bytes: u64,
    /// Disjoint logical stores selected by the caller.
    pub stores: Vec<MemoryStore>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Result of restoring one recoverable tombstone without replacing active data.
pub struct RestoreReport {
    /// Opaque tombstone identifier supplied by the caller.
    pub tombstone_id: String,
    /// Regular files returned to their original active namespace.
    pub files: usize,
    /// Aggregate restored bytes.
    pub bytes: u64,
    /// Logical stores recorded by the original tombstone.
    pub stores: Vec<MemoryStore>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Result of unlinking one tombstone payload from its logical namespace.
pub struct PurgeReport {
    /// Opaque tombstone identifier supplied by the caller.
    pub tombstone_id: String,
    /// Regular payload files unlinked.
    pub files: usize,
    /// Aggregate unlinked bytes.
    pub bytes: u64,
    /// Logical stores recorded by the original tombstone.
    pub stores: Vec<MemoryStore>,
    /// Whether all selected filesystem names were unlinked.
    pub logically_unlinked: bool,
    /// Always `false`; unlink cannot prove storage-media erasure.
    pub physically_deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Result of one executable age-retention pass.
pub struct RetentionReport {
    /// Store whose configured policy was evaluated.
    pub store: MemoryStore,
    /// Configured age in days, or `None` when no policy is active.
    pub retention_days: Option<u32>,
    /// Regular files safely inspected.
    pub scanned_files: usize,
    /// Stale files moved out of active storage.
    pub forgotten_files: usize,
    /// Aggregate bytes moved into recoverable storage.
    pub bytes: u64,
    /// Published retention tombstone, when at least one stale file existed.
    pub tombstone_id: Option<String>,
    /// Non-persisted fail-open diagnostic from the convenience wrapper.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Summary of a confirmed memory export archive.
pub struct ExportReport {
    /// Explicit destination created without replacing an existing path.
    pub destination: PathBuf,
    /// Number of unique regular files exported.
    pub files: usize,
    /// Aggregate uncompressed content bytes.
    pub bytes: u64,
    /// Disjoint logical stores requested by the caller.
    pub stores: Vec<MemoryStore>,
}

fn invalid_data(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn permission_denied(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, message.into())
}

fn validate_relative_path(path: &Path) -> std::io::Result<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(permission_denied(
            "managed memory path escapes its boundary",
        ));
    }
    Ok(())
}

fn push_managed_file(
    boundary: &Path,
    path: &Path,
    metadata: &std::fs::Metadata,
    files: &mut Vec<ManagedFile>,
    total_bytes: &mut u64,
) -> std::io::Result<()> {
    if files.len() >= MAX_INVENTORY_FILES {
        return Err(invalid_data(
            "memory operation exceeds the managed file limit",
        ));
    }
    let relative = path
        .strip_prefix(boundary)
        .map_err(|_| permission_denied("managed memory path escapes its boundary"))?
        .to_path_buf();
    validate_relative_path(&relative)?;
    *total_bytes = total_bytes
        .checked_add(metadata.len())
        .ok_or_else(|| invalid_data("memory operation byte count overflow"))?;
    if *total_bytes > MAX_LIFECYCLE_BYTES {
        return Err(invalid_data(
            "memory operation exceeds the managed byte limit",
        ));
    }
    files.push(ManagedFile {
        path: path.to_path_buf(),
        relative,
        bytes: metadata.len(),
        modified: metadata.modified().ok(),
    });
    Ok(())
}

fn validate_managed_ancestors(boundary: &Path, path: &Path) -> std::io::Result<()> {
    let relative = path
        .strip_prefix(boundary)
        .map_err(|_| permission_denied("managed memory path escapes its boundary"))?;
    validate_relative_path(relative)?;
    let components: Vec<_> = relative.components().collect();
    let mut current = boundary.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(permission_denied(
                "managed memory path escapes its boundary",
            ));
        };
        current.push(name);
        let metadata = match std::fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let last = index + 1 == components.len();
        if (!last && !umadev_state::fs::metadata_is_real_dir(&metadata))
            || (last
                && !umadev_state::fs::metadata_is_real_dir(&metadata)
                && !umadev_state::fs::metadata_is_real_file(&metadata))
        {
            return Err(permission_denied(
                "managed memory path contains a linked or special component",
            ));
        }
    }
    Ok(())
}

struct FileCollector<'a> {
    boundary: &'a Path,
    files: &'a mut Vec<ManagedFile>,
    total_bytes: &'a mut u64,
    visited_nodes: &'a mut usize,
    accept_file: &'a dyn Fn(&Path) -> std::io::Result<bool>,
    skip_path: &'a dyn Fn(&Path) -> bool,
}

impl FileCollector<'_> {
    fn walk(&mut self, root: &Path, path: &Path, depth: usize) -> std::io::Result<()> {
        if path != root && (self.skip_path)(path) {
            return Ok(());
        }
        if depth > MAX_INVENTORY_DEPTH {
            return Err(invalid_data("memory tree exceeds the managed depth limit"));
        }
        if *self.visited_nodes >= MAX_LIFECYCLE_NODES {
            return Err(invalid_data("memory tree exceeds the managed node limit"));
        }
        *self.visited_nodes += 1;
        if depth == 0 {
            validate_managed_ancestors(self.boundary, path)?;
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if umadev_state::fs::metadata_is_real_file(&metadata) {
            if (self.accept_file)(path)? {
                push_managed_file(self.boundary, path, &metadata, self.files, self.total_bytes)?;
            }
            return Ok(());
        }
        if !umadev_state::fs::metadata_is_real_dir(&metadata) {
            return Err(permission_denied(
                "refusing to inspect linked or special memory state",
            ));
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            if entries.len() >= MAX_LIFECYCLE_NODES {
                return Err(invalid_data(
                    "memory directory exceeds the managed node limit",
                ));
            }
            entries.push(entry?);
        }
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            self.walk(root, &entry.path(), depth + 1)?;
        }
        Ok(())
    }
}

fn auto_sediment_strict(path: &Path) -> std::io::Result<bool> {
    if !is_markdown(path) {
        return Ok(false);
    }
    let bytes = umadev_state::fs::read_bounded(path, MAX_EXPORT_FILE_BYTES)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| invalid_data(format!("memory markdown is not UTF-8: {error}")))?;
    Ok(umadev_knowledge::front_matter_field(text, "maintainer")
        == umadev_knowledge::FrontMatterField::Value("auto-sediment"))
}

fn selected_learned_skill_root(boundary: &Path) -> PathBuf {
    let current = boundary.join(".umadev/memory/learned-skills");
    let legacy = boundary.join(".umadev/skills");
    let legacy_store = legacy.join("skills.jsonl");
    if valid_skill_migration_marker(&current.join("migration-v1.json"))
        || !std::fs::symlink_metadata(&legacy_store)
            .is_ok_and(|metadata| umadev_state::fs::metadata_is_real_file(&metadata))
    {
        current
    } else {
        legacy
    }
}

fn collect_exact_path(
    boundary: &Path,
    path: &Path,
    files: &mut Vec<ManagedFile>,
    total_bytes: &mut u64,
    visited_nodes: &mut usize,
) -> std::io::Result<()> {
    FileCollector {
        boundary,
        files,
        total_bytes,
        visited_nodes,
        accept_file: &|_| Ok(true),
        skip_path: &|_| false,
    }
    .walk(path, path, 0)
}

fn collect_store_files(
    boundary: &Path,
    scope: MemoryScope,
    store: MemoryStore,
) -> std::io::Result<Vec<ManagedFile>> {
    if !store_supports_scope(store, scope) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store is unavailable in the selected scope",
        ));
    }
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    let mut visited_nodes = 0usize;
    let skip_none = |_: &Path| false;

    match (scope, store) {
        (MemoryScope::Project, MemoryStore::LessonSediment) => {
            let root = boundary.join(".umadev/learned");
            let raw = root.join("_raw");
            let skills = root.join("skills");
            let accept = |path: &Path| {
                Ok(path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("lesson-"))
                    && auto_sediment_strict(path)?)
            };
            let skip = |path: &Path| path.starts_with(&raw) || path.starts_with(&skills);
            FileCollector {
                boundary,
                files: &mut files,
                total_bytes: &mut total_bytes,
                visited_nodes: &mut visited_nodes,
                accept_file: &accept,
                skip_path: &skip,
            }
            .walk(&root, &root, 0)?;
        }
        (MemoryScope::Global, MemoryStore::GlobalLessonProjection) => {
            let root = boundary.join(".umadev/learned");
            FileCollector {
                boundary,
                files: &mut files,
                total_bytes: &mut total_bytes,
                visited_nodes: &mut visited_nodes,
                accept_file: &auto_sediment_strict,
                skip_path: &skip_none,
            }
            .walk(&root, &root, 0)?;
        }
        (MemoryScope::Global, MemoryStore::GlobalLessonsManual) => {
            let root = boundary.join(".umadev/learned");
            let accept = |path: &Path| Ok(is_markdown(path) && !auto_sediment_strict(path)?);
            FileCollector {
                boundary,
                files: &mut files,
                total_bytes: &mut total_bytes,
                visited_nodes: &mut visited_nodes,
                accept_file: &accept,
                skip_path: &skip_none,
            }
            .walk(&root, &root, 0)?;
        }
        (MemoryScope::Project, MemoryStore::SkillPackages) => {
            let installed = boundary.join(".umadev/skills");
            let receipts = installed.join("receipts");
            let learned_store = installed.join("skills.jsonl");
            let accept = |path: &Path| Ok(path != learned_store);
            let skip = |path: &Path| path.starts_with(&receipts);
            FileCollector {
                boundary,
                files: &mut files,
                total_bytes: &mut total_bytes,
                visited_nodes: &mut visited_nodes,
                accept_file: &accept,
                skip_path: &skip,
            }
            .walk(&installed, &installed, 0)?;
            collect_exact_path(
                boundary,
                &boundary.join("knowledge/skills"),
                &mut files,
                &mut total_bytes,
                &mut visited_nodes,
            )?;
        }
        (MemoryScope::Project, MemoryStore::LearnedSkills) => {
            let selected = selected_learned_skill_root(boundary);
            collect_exact_path(
                boundary,
                &selected.join("skills.jsonl"),
                &mut files,
                &mut total_bytes,
                &mut visited_nodes,
            )?;
            collect_exact_path(
                boundary,
                &selected.join("receipts"),
                &mut files,
                &mut total_bytes,
                &mut visited_nodes,
            )?;
        }
        _ => {
            for location in logical_locations(store, scope) {
                if !location.contains('*') && !location.contains('<') {
                    collect_exact_path(
                        boundary,
                        &boundary.join(location),
                        &mut files,
                        &mut total_bytes,
                        &mut visited_nodes,
                    )?;
                }
            }
        }
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    files.dedup_by(|left, right| left.relative == right.relative);
    Ok(files)
}

fn auto_sediment_rooted(
    root: &umadev_state::fs::RootedDir,
    relative: &Path,
) -> std::io::Result<bool> {
    if !is_markdown(relative) {
        return Ok(false);
    }
    let bytes = root.read_bounded(relative, MAX_EXPORT_FILE_BYTES)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|error| invalid_data(format!("memory markdown is not UTF-8: {error}")))?;
    Ok(umadev_knowledge::front_matter_field(text, "maintainer")
        == umadev_knowledge::FrontMatterField::Value("auto-sediment"))
}

fn valid_skill_migration_marker_rooted(
    root: &umadev_state::fs::RootedDir,
    relative: &Path,
) -> std::io::Result<bool> {
    let bytes = match root.read_bounded(relative, 16 * 1024) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let value = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|error| invalid_data(format!("invalid skill migration marker: {error}")))?;
    Ok(
        value.get("version").and_then(serde_json::Value::as_u64) == Some(1)
            && value
                .get("store_sha256")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|hash| {
                    hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                }),
    )
}

fn collect_exact_rooted(
    root: &umadev_state::fs::RootedDir,
    boundary: &Path,
    relative: &Path,
    files: &mut Vec<ManagedFile>,
    total_bytes: &mut u64,
    accept: &dyn Fn(&Path) -> std::io::Result<bool>,
) -> std::io::Result<()> {
    root.validate_path(relative)?;
    if files.len() >= MAX_INVENTORY_FILES {
        return Err(invalid_data(
            "memory operation exceeds the managed file limit",
        ));
    }
    let discovered = root.list_regular_tree(
        relative,
        MAX_INVENTORY_DEPTH,
        MAX_LIFECYCLE_NODES,
        MAX_LIFECYCLE_NODES,
        u64::MAX,
    )?;
    for file in discovered {
        if !accept(&file.relative)? {
            continue;
        }
        if files.len() >= MAX_INVENTORY_FILES {
            return Err(invalid_data(
                "memory operation exceeds the managed file limit",
            ));
        }
        *total_bytes = total_bytes
            .checked_add(file.len)
            .ok_or_else(|| invalid_data("memory operation byte count overflow"))?;
        if *total_bytes > MAX_LIFECYCLE_BYTES {
            return Err(invalid_data(
                "memory operation exceeds the managed byte limit",
            ));
        }
        files.push(ManagedFile {
            path: boundary.join(&file.relative),
            relative: file.relative,
            bytes: file.len,
            modified: file.modified,
        });
    }
    Ok(())
}

/// Lifecycle-only collector. Unlike inventory/export discovery, every tree
/// walk and classification read is made through the root capability captured
/// before preflight, so a renamed workspace cannot redirect even read-side
/// lifecycle decisions.
fn collect_store_files_rooted(
    root: &umadev_state::fs::RootedDir,
    boundary: &Path,
    scope: MemoryScope,
    store: MemoryStore,
) -> std::io::Result<Vec<ManagedFile>> {
    if !store_supports_scope(store, scope) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store is unavailable in the selected scope",
        ));
    }
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    let accept_all = |_: &Path| Ok(true);

    match (scope, store) {
        (MemoryScope::Project, MemoryStore::LessonSediment) => {
            let raw = Path::new(".umadev/learned/_raw");
            let skills = Path::new(".umadev/learned/skills");
            collect_exact_rooted(
                root,
                boundary,
                Path::new(".umadev/learned"),
                &mut files,
                &mut total_bytes,
                &|relative| {
                    Ok(!relative.starts_with(raw)
                        && !relative.starts_with(skills)
                        && relative
                            .file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("lesson-"))
                        && auto_sediment_rooted(root, relative)?)
                },
            )?;
        }
        (MemoryScope::Global, MemoryStore::GlobalLessonProjection) => {
            collect_exact_rooted(
                root,
                boundary,
                Path::new(".umadev/learned"),
                &mut files,
                &mut total_bytes,
                &|relative| auto_sediment_rooted(root, relative),
            )?;
        }
        (MemoryScope::Global, MemoryStore::GlobalLessonsManual) => {
            collect_exact_rooted(
                root,
                boundary,
                Path::new(".umadev/learned"),
                &mut files,
                &mut total_bytes,
                &|relative| Ok(is_markdown(relative) && !auto_sediment_rooted(root, relative)?),
            )?;
        }
        (MemoryScope::Project, MemoryStore::SkillPackages) => {
            let receipts = Path::new(".umadev/skills/receipts");
            let learned_store = Path::new(".umadev/skills/skills.jsonl");
            collect_exact_rooted(
                root,
                boundary,
                Path::new(".umadev/skills"),
                &mut files,
                &mut total_bytes,
                &|relative| Ok(relative != learned_store && !relative.starts_with(receipts)),
            )?;
            collect_exact_rooted(
                root,
                boundary,
                Path::new("knowledge/skills"),
                &mut files,
                &mut total_bytes,
                &accept_all,
            )?;
        }
        (MemoryScope::Project, MemoryStore::LearnedSkills) => {
            let current = PathBuf::from(".umadev/memory/learned-skills");
            let legacy = PathBuf::from(".umadev/skills");
            let legacy_store = legacy.join("skills.jsonl");
            let selected =
                if valid_skill_migration_marker_rooted(root, &current.join("migration-v1.json"))?
                    || !root.regular_file_exists(&legacy_store)?
                {
                    current
                } else {
                    legacy
                };
            collect_exact_rooted(
                root,
                boundary,
                &selected.join("skills.jsonl"),
                &mut files,
                &mut total_bytes,
                &accept_all,
            )?;
            collect_exact_rooted(
                root,
                boundary,
                &selected.join("receipts"),
                &mut files,
                &mut total_bytes,
                &accept_all,
            )?;
        }
        _ => {
            for location in logical_locations(store, scope) {
                if !location.contains('*') && !location.contains('<') {
                    collect_exact_rooted(
                        root,
                        boundary,
                        Path::new(location),
                        &mut files,
                        &mut total_bytes,
                        &accept_all,
                    )?;
                }
            }
        }
    }
    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    files.dedup_by(|left, right| left.relative == right.relative);
    Ok(files)
}

fn normalize_stores(
    scope: MemoryScope,
    stores: &[MemoryStore],
    allow_lifecycle_metadata: bool,
) -> std::io::Result<Vec<MemoryStore>> {
    let stores: BTreeSet<_> = stores.iter().copied().collect();
    if stores.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "memory operation requires at least one store",
        ));
    }
    if stores
        .iter()
        .any(|store| !store_supports_scope(*store, scope))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "memory selector contains a store unavailable in the selected scope",
        ));
    }
    if !allow_lifecycle_metadata
        && stores
            .iter()
            .any(|store| matches!(store, MemoryStore::Tombstones | MemoryStore::DeletionAudit))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "tombstones and deletion audit cannot forget themselves",
        ));
    }
    Ok(stores.into_iter().collect())
}

fn lifecycle_scope(scope: MemoryScope) -> umadev_state::lifecycle::LifecycleScope {
    match scope {
        MemoryScope::Project => umadev_state::lifecycle::LifecycleScope::Project,
        MemoryScope::Global => umadev_state::lifecycle::LifecycleScope::Global,
    }
}

fn file_still_matches(file: &ManagedFile) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(&file.path)?;
    if !umadev_state::fs::metadata_is_real_file(&metadata)
        || metadata.len() != file.bytes
        || metadata.modified().ok() != file.modified
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory changed after preflight; retry when writers are idle",
        ));
    }
    Ok(())
}

fn open_scope_root(
    project_root: &Path,
    scope: MemoryScope,
) -> std::io::Result<(PathBuf, umadev_state::fs::RootedDir)> {
    let boundary = scope_boundary(project_root, scope)?;
    let root = umadev_state::fs::RootedDir::open(&boundary)?;
    if !root.matches_path(&boundary)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory scope changed while its capability was opened",
        ));
    }
    Ok((boundary, root))
}

#[derive(Debug, Clone)]
struct RootedTransfer {
    source: PathBuf,
    destination: PathBuf,
    bytes: u64,
    sha256: [u8; 32],
}

/// Copy one complete file into a no-replace rooted destination, verify both
/// identities and contents, and only then unlink the source name. The two
/// roots may be separate capabilities; no ambient path is resolved.
fn transfer_rooted_file(
    source_root: &umadev_state::fs::RootedDir,
    source: &Path,
    destination_root: &umadev_state::fs::RootedDir,
    destination: &Path,
    expected_bytes: u64,
    expected_sha256: Option<[u8; 32]>,
) -> std::io::Result<[u8; 32]> {
    validate_relative_path(source)?;
    validate_relative_path(destination)?;
    let source_read = source_root.read_bounded_verified(source, expected_bytes)?;
    if source_read.len() != expected_bytes {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory file length changed after lifecycle preflight",
        ));
    }
    let sha256: [u8; 32] = Sha256::digest(source_read.bytes()).into();
    if expected_sha256.is_some_and(|expected| expected != sha256) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory contents changed before lifecycle rollback",
        ));
    }
    destination_root.publish_new_private(destination, source_read.bytes(), true)?;
    let destination_read = match destination_root.read_bounded_verified(destination, expected_bytes)
    {
        Ok(read) if read.bytes() == source_read.bytes() => read,
        Ok(read) => {
            let _ = destination_root.remove_regular_file_if_unchanged(destination, &read);
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "memory payload changed during no-replace publication",
            ));
        }
        Err(error) => return Err(error),
    };
    match source_root.remove_regular_file_if_unchanged(source, &source_read) {
        Ok(true) => match source_root.regular_file_exists(source) {
            Ok(false) => Ok(sha256),
            Ok(true) => Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "a concurrent memory source appeared after rooted unlink",
            )),
            Err(error) => Err(error),
        },
        Ok(false) => {
            let _ =
                destination_root.remove_regular_file_if_unchanged(destination, &destination_read);
            Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "memory source disappeared before rooted unlink",
            ))
        }
        Err(error) => {
            let cleanup =
                destination_root.remove_regular_file_if_unchanged(destination, &destination_read);
            match cleanup {
                Ok(true) => Err(error),
                Ok(false) => Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!("{error}; published recovery copy disappeared during rollback"),
                )),
                Err(cleanup_error) => Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    format!(
                        "{error}; published recovery copy could not be safely rolled back ({cleanup_error})"
                    ),
                )),
            }
        }
    }
}

fn rollback_rooted_transfers(
    root: &umadev_state::fs::RootedDir,
    transfers: &[RootedTransfer],
) -> std::io::Result<()> {
    let mut first_error = None;
    for transfer in transfers.iter().rev() {
        if let Err(error) = transfer_rooted_file(
            root,
            &transfer.destination,
            root,
            &transfer.source,
            transfer.bytes,
            Some(transfer.sha256),
        ) {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn soft_delete_files(
    root: umadev_state::fs::RootedDir,
    scope: MemoryScope,
    stores: &[MemoryStore],
    operation: umadev_state::lifecycle::LifecycleOperation,
    files: &[ManagedFile],
) -> std::io::Result<Option<String>> {
    soft_delete_files_with_hook(root, scope, stores, operation, files, || {})
}

fn soft_delete_files_with_hook(
    root: umadev_state::fs::RootedDir,
    scope: MemoryScope,
    stores: &[MemoryStore],
    operation: umadev_state::lifecycle::LifecycleOperation,
    files: &[ManagedFile],
    after_begin: impl FnOnce(),
) -> std::io::Result<Option<String>> {
    if files.is_empty() {
        return Ok(None);
    }
    let mut transaction = umadev_state::lifecycle::begin_transaction_rooted(
        root,
        lifecycle_scope(scope),
        stores,
        operation,
    )?;
    after_begin();
    let rooted = transaction.rooted_dir()?;
    let payload = transaction.payload_relative().to_path_buf();
    let mut destinations = Vec::with_capacity(files.len());
    for file in files {
        let source_read = match rooted.read_bounded_verified(&file.relative, file.bytes) {
            Ok(read) if read.len() == file.bytes => read,
            Ok(_) => {
                let _ = transaction.abort();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "memory changed after lifecycle preflight",
                ));
            }
            Err(error) => {
                let _ = transaction.abort();
                return Err(error);
            }
        };
        drop(source_read);
        let destination = payload.join(&file.relative);
        if let Err(error) = rooted.validate_path(&destination) {
            let _ = transaction.abort();
            return Err(error);
        }
        match rooted.regular_file_exists(&destination) {
            Ok(false) => {}
            Ok(true) => {
                let _ = transaction.abort();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "memory payload destination already exists",
                ));
            }
            Err(error) => {
                let _ = transaction.abort();
                return Err(error);
            }
        }
        destinations.push(destination);
    }

    let mut moved: Vec<RootedTransfer> = Vec::with_capacity(files.len());
    for (file, destination) in files.iter().zip(&destinations) {
        let sha256 = match transfer_rooted_file(
            &rooted,
            &file.relative,
            &rooted,
            destination,
            file.bytes,
            None,
        ) {
            Ok(sha256) => sha256,
            Err(error) => {
                let rollback = rollback_rooted_transfers(&rooted, &moved);
                let _ = transaction.abort();
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(std::io::Error::new(
                        error.kind(),
                        format!(
                            "{error}; rollback failed ({rollback_error}); recover payload {} manually",
                            transaction.id()
                        ),
                    )),
                };
            }
        };
        moved.push(RootedTransfer {
            source: file.relative.clone(),
            destination: destination.clone(),
            bytes: file.bytes,
            sha256,
        });
    }
    let bytes = files
        .iter()
        .fold(0u64, |sum, file| sum.saturating_add(file.bytes));
    match transaction.commit(files.len(), bytes) {
        Ok(record) => Ok(Some(record.id)),
        Err(error) => {
            let rollback = rollback_rooted_transfers(&rooted, &moved);
            let _ = transaction.abort();
            match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(std::io::Error::new(
                    error.kind(),
                    format!(
                        "{error}; rollback failed ({rollback_error}); recover payload {} manually",
                        transaction.id()
                    ),
                )),
            }
        }
    }
}

/// Moves selected active memory into recoverable tombstone storage.
///
/// This operation never physically deletes content. Both an explicit scope and
/// an explicit confirmation are required. Tombstone and audit stores are not
/// recursive forget targets.
pub fn forget(
    project_root: &Path,
    scope: MemoryScope,
    stores: &[MemoryStore],
    confirmed: bool,
) -> std::io::Result<ForgetReport> {
    if !confirmed {
        return Err(permission_denied(
            "forget requires explicit confirmation and performs only soft deletion",
        ));
    }
    let stores = normalize_stores(scope, stores, false)?;
    let (boundary, root) = open_scope_root(project_root, scope)?;
    let mut unique = BTreeMap::new();
    for store in &stores {
        for file in collect_store_files_rooted(&root, &boundary, scope, *store)? {
            unique.entry(file.relative.clone()).or_insert(file);
        }
        if unique.len() > MAX_INVENTORY_FILES {
            return Err(invalid_data(
                "memory operation exceeds the aggregate managed file limit",
            ));
        }
    }
    let files: Vec<ManagedFile> = unique.into_values().collect();
    let bytes = files
        .iter()
        .fold(0u64, |sum, file| sum.saturating_add(file.bytes));
    if bytes > MAX_LIFECYCLE_BYTES {
        return Err(invalid_data(
            "memory operation exceeds the aggregate managed byte limit",
        ));
    }
    let tombstone_id = soft_delete_files(
        root,
        scope,
        &stores,
        umadev_state::lifecycle::LifecycleOperation::Forget,
        &files,
    )?;
    Ok(ForgetReport {
        tombstone_id,
        files: files.len(),
        bytes,
        stores,
    })
}

#[derive(Debug, Clone)]
struct LifecycleFile {
    relative: PathBuf,
    bytes: u64,
}

fn collect_tombstone_payload(
    root: &umadev_state::fs::RootedDir,
    payload: &Path,
) -> std::io::Result<Vec<LifecycleFile>> {
    if !root.is_real_dir(payload)? {
        return Err(permission_denied(
            "tombstone payload root is missing, linked, or special",
        ));
    }
    root.list_regular_tree(
        payload,
        MAX_INVENTORY_DEPTH,
        MAX_LIFECYCLE_NODES,
        MAX_INVENTORY_FILES,
        MAX_LIFECYCLE_BYTES,
    )?
    .into_iter()
    .map(|file| {
        let relative = file
            .relative
            .strip_prefix(payload)
            .map_err(|_| permission_denied("tombstone payload escaped its rooted boundary"))?
            .to_path_buf();
        validate_relative_path(&relative)?;
        Ok(LifecycleFile {
            relative,
            bytes: file.len,
        })
    })
    .collect()
}

fn tombstone_stores(
    record: &umadev_state::lifecycle::TombstoneRecord,
    scope: MemoryScope,
) -> std::io::Result<Vec<MemoryStore>> {
    let stores = record
        .stores
        .iter()
        .map(|store| {
            MemoryStore::parse(store)
                .ok_or_else(|| invalid_data("tombstone contains an unknown logical store"))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    normalize_stores(scope, &stores, false)
}

fn restore_failure(
    error: std::io::Error,
    root: &umadev_state::fs::RootedDir,
    transfers: &[RootedTransfer],
) -> std::io::Result<RestoreReport> {
    match rollback_rooted_transfers(root, transfers) {
        Ok(()) => Err(error),
        Err(rollback) => Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            format!(
                "restore stopped ({error}); safe rollback was incomplete ({rollback}); prepared audit retained"
            ),
        )),
    }
}

/// Restores a committed tombstone without replacing any active path.
///
/// Every source and destination is preflighted before mutation. Each file is
/// copied through fixed directory capabilities, published no-replace, checked
/// byte-for-byte, and only then unlinked from the payload. Conflicts are never
/// overwritten.
pub fn restore(
    project_root: &Path,
    scope: MemoryScope,
    tombstone_id: &str,
    confirmed: bool,
) -> std::io::Result<RestoreReport> {
    if !confirmed {
        return Err(permission_denied(
            "restore requires explicit confirmation and never replaces active memory",
        ));
    }
    let (_boundary, root) = open_scope_root(project_root, scope)?;
    let mut action = umadev_state::lifecycle::begin_tombstone_action_rooted(
        root,
        lifecycle_scope(scope),
        tombstone_id,
        umadev_state::lifecycle::TombstoneAction::Restore,
    )?;
    let stores = tombstone_stores(action.tombstone(), scope)?;
    let rooted = action.rooted_dir()?;
    let payload = action.payload_relative().to_path_buf();
    let files = collect_tombstone_payload(&rooted, &payload)?;
    let bytes = files
        .iter()
        .try_fold(0u64, |sum, file| sum.checked_add(file.bytes))
        .ok_or_else(|| invalid_data("tombstone payload byte count overflow"))?;

    // Complete preflight happens before parent creation or active publication.
    for file in &files {
        let source = payload.join(&file.relative);
        let source_read = rooted.read_bounded_verified(&source, file.bytes)?;
        if source_read.len() != file.bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "tombstone payload changed during restore preflight",
            ));
        }
        rooted.validate_path(&file.relative)?;
        match rooted.regular_file_exists(&file.relative) {
            Ok(false) => {}
            Ok(true) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "restore refuses to replace an existing active memory path",
                ));
            }
            Err(error) => return Err(error),
        }
    }
    action.prepare(files.len(), bytes)?;

    let mut transfers = Vec::with_capacity(files.len());
    for file in &files {
        let source = payload.join(&file.relative);
        let sha256 =
            match transfer_rooted_file(&rooted, &source, &rooted, &file.relative, file.bytes, None)
            {
                Ok(sha256) => sha256,
                Err(error) => return restore_failure(error, &rooted, &transfers),
            };
        transfers.push(RootedTransfer {
            source,
            destination: file.relative.clone(),
            bytes: file.bytes,
            sha256,
        });
    }
    if let Err(error) = action.commit() {
        return restore_failure(error, &rooted, &transfers);
    }
    Ok(RestoreReport {
        tombstone_id: tombstone_id.to_string(),
        files: files.len(),
        bytes,
        stores,
    })
}

/// Logically unlinks one recoverable tombstone payload.
///
/// This operation is irreversible at the namespace level and requires
/// explicit confirmation. It never claims secure or physical media erasure.
/// A partial I/O failure leaves a durable prepared audit rather than claiming
/// completion.
pub fn purge(
    project_root: &Path,
    scope: MemoryScope,
    tombstone_id: &str,
    confirmed: bool,
) -> std::io::Result<PurgeReport> {
    if !confirmed {
        return Err(permission_denied(
            "logical purge requires explicit confirmation and is not physical erasure",
        ));
    }
    let (_boundary, root) = open_scope_root(project_root, scope)?;
    let mut action = umadev_state::lifecycle::begin_tombstone_action_rooted(
        root,
        lifecycle_scope(scope),
        tombstone_id,
        umadev_state::lifecycle::TombstoneAction::LogicalPurge,
    )?;
    let stores = tombstone_stores(action.tombstone(), scope)?;
    let rooted = action.rooted_dir()?;
    let payload = action.payload_relative().to_path_buf();
    let files = collect_tombstone_payload(&rooted, &payload)?;
    let bytes = files
        .iter()
        .try_fold(0u64, |sum, file| sum.checked_add(file.bytes))
        .ok_or_else(|| invalid_data("tombstone payload byte count overflow"))?;
    // Collection fully validates the bounded tree before the prepared record
    // is published and before the first irreversible unlink.
    action.prepare(files.len(), bytes)?;
    for file in &files {
        let source = payload.join(&file.relative);
        let proof = rooted.read_bounded_verified(&source, file.bytes)?;
        if proof.len() != file.bytes || !rooted.remove_regular_file_if_unchanged(&source, &proof)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "tombstone payload changed before logical purge",
            ));
        }
    }
    let disposition = action.commit()?;
    Ok(PurgeReport {
        tombstone_id: tombstone_id.to_string(),
        files: files.len(),
        bytes,
        stores,
        logically_unlinked: disposition.logically_unlinked,
        physically_deleted: disposition.physically_deleted,
    })
}

fn enforce_retention_at(
    project_root: &Path,
    scope: MemoryScope,
    store: MemoryStore,
    now: SystemTime,
) -> std::io::Result<RetentionReport> {
    if !store_supports_scope(store, scope)
        || store.retention_enforcement() != RetentionEnforcement::PolicyOnly
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "store does not have an executable age-retention adapter in this scope",
        ));
    }
    let (boundary, root) = open_scope_root(project_root, scope)?;
    let policy = umadev_state::memory::load_policy_rooted(&root, false)?;
    let Some(days) = policy.retention_days(store) else {
        return Ok(RetentionReport {
            store,
            retention_days: None,
            scanned_files: 0,
            forgotten_files: 0,
            bytes: 0,
            tombstone_id: None,
            error: None,
        });
    };
    let max_age = Duration::from_secs(u64::from(days).saturating_mul(24 * 60 * 60));
    let cutoff = now.checked_sub(max_age).unwrap_or(SystemTime::UNIX_EPOCH);
    let scanned = collect_store_files_rooted(&root, &boundary, scope, store)?;
    let stale: Vec<ManagedFile> = scanned
        .iter()
        .filter(|file| file.modified.is_some_and(|modified| modified <= cutoff))
        .cloned()
        .collect();
    let bytes = stale
        .iter()
        .fold(0u64, |sum, file| sum.saturating_add(file.bytes));
    let tombstone_id = soft_delete_files(
        root,
        scope,
        &[store],
        umadev_state::lifecycle::LifecycleOperation::Retention,
        &stale,
    )?;
    Ok(RetentionReport {
        store,
        retention_days: Some(days),
        scanned_files: scanned.len(),
        forgotten_files: stale.len(),
        bytes,
        tombstone_id,
        error: None,
    })
}

/// Executes one configured age-retention adapter using file modification time.
/// Stale files are soft-deleted into a recoverable tombstone, never destroyed.
pub fn enforce_retention(
    project_root: &Path,
    scope: MemoryScope,
    store: MemoryStore,
) -> std::io::Result<RetentionReport> {
    enforce_retention_at(project_root, scope, store, SystemTime::now())
}

/// Business-path convenience wrapper: lifecycle-policy corruption or I/O
/// failure is reported but never promoted into an orchestration failure.
#[must_use]
pub fn enforce_retention_fail_open(
    project_root: &Path,
    scope: MemoryScope,
    store: MemoryStore,
) -> RetentionReport {
    enforce_retention(project_root, scope, store).unwrap_or_else(|error| RetentionReport {
        store,
        retention_days: None,
        scanned_files: 0,
        forgotten_files: 0,
        bytes: 0,
        tombstone_id: None,
        error: Some(error.to_string()),
    })
}

#[derive(Debug, serde::Serialize)]
struct ExportManifest {
    version: u32,
    scope: &'static str,
    stores: Vec<String>,
    files: Vec<ExportManifestFile>,
    total_bytes: u64,
}

#[derive(Debug, serde::Serialize)]
struct ExportManifestFile {
    archive_path: String,
    stores: Vec<String>,
    bytes: u64,
    sha256: String,
}

fn archive_relative_path(relative: &Path) -> std::io::Result<String> {
    validate_relative_path(relative)?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Component::Normal(value) = component else {
            return Err(permission_denied("memory export path escapes its boundary"));
        };
        let value = value.to_str().ok_or_else(|| {
            invalid_data("memory export cannot losslessly encode a non-Unicode filename")
        })?;
        parts.push(value);
    }
    Ok(parts.join("/"))
}

/// Creates a bounded ZIP archive of selected memory at an explicit path.
///
/// Stored memory may contain source code, prompts, and private project facts,
/// so the caller must explicitly confirm sensitive export. Existing output is
/// never replaced and no source link is followed.
pub fn export(
    project_root: &Path,
    scope: MemoryScope,
    stores: &[MemoryStore],
    destination: &Path,
    confirmed_sensitive: bool,
) -> std::io::Result<ExportReport> {
    if !confirmed_sensitive {
        return Err(permission_denied(
            "memory export contains sensitive content and requires explicit confirmation",
        ));
    }
    if !destination.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "memory export requires an explicit absolute destination",
        ));
    }
    let parent = destination.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "memory export destination has no parent",
        )
    })?;
    if !umadev_state::fs::real_dir(parent) {
        return Err(permission_denied(
            "memory export parent is linked, missing, or special",
        ));
    }
    match std::fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "memory export never replaces an existing path",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let stores = normalize_stores(scope, stores, true)?;
    let boundary = scope_boundary(project_root, scope)?;
    let mut unique: BTreeMap<PathBuf, (ManagedFile, BTreeSet<MemoryStore>)> = BTreeMap::new();
    for store in &stores {
        for file in collect_store_files(&boundary, scope, *store)? {
            unique
                .entry(file.relative.clone())
                .and_modify(|(_, owners)| {
                    owners.insert(*store);
                })
                .or_insert_with(|| (file, BTreeSet::from([*store])));
        }
        if unique.len() > MAX_INVENTORY_FILES {
            return Err(invalid_data(
                "memory export exceeds the aggregate managed file limit",
            ));
        }
    }
    let total_bytes = unique.values().try_fold(0u64, |sum, (file, _)| {
        if file.bytes > MAX_EXPORT_FILE_BYTES {
            return Err(invalid_data("memory export contains an oversized file"));
        }
        let next = sum
            .checked_add(file.bytes)
            .ok_or_else(|| invalid_data("memory export byte count overflow"))?;
        if next > MAX_EXPORT_TOTAL_BYTES {
            return Err(invalid_data(
                "memory export exceeds the bounded archive limit",
            ));
        }
        Ok(next)
    })?;

    let cursor = Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    let mut manifest_files = Vec::with_capacity(unique.len());
    for (relative, (file, owners)) in &unique {
        file_still_matches(file)?;
        let relative = archive_relative_path(relative)?;
        let archive_path = format!("memory/{relative}");
        let bytes = umadev_state::fs::read_bounded(&file.path, MAX_EXPORT_FILE_BYTES)?;
        if u64::try_from(bytes.len()) != Ok(file.bytes) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "memory changed while the export archive was being assembled",
            ));
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        archive.start_file(&archive_path, options)?;
        archive.write_all(&bytes)?;
        manifest_files.push(ExportManifestFile {
            archive_path,
            stores: owners.iter().map(|store| store.id().to_string()).collect(),
            bytes: file.bytes,
            sha256,
        });
    }
    let manifest = ExportManifest {
        version: EXPORT_MANIFEST_VERSION,
        scope: scope.id(),
        stores: stores.iter().map(|store| store.id().to_string()).collect(),
        files: manifest_files,
        total_bytes,
    };
    let manifest_bytes =
        serde_json::to_vec_pretty(&manifest).map_err(|error| invalid_data(error.to_string()))?;
    archive.start_file("manifest.json", options)?;
    archive.write_all(&manifest_bytes)?;
    let bytes = archive.finish()?.into_inner();
    if bytes.len() > MAX_EXPORT_ARCHIVE_BYTES {
        return Err(invalid_data(
            "compressed memory export exceeds the bounded archive limit",
        ));
    }
    umadev_state::fs::write_new_private(destination, &bytes)?;
    Ok(ExportReport {
        destination: destination.to_path_buf(),
        files: unique.len(),
        bytes: total_bytes,
        stores,
    })
}

/// Deletes only a rebuildable project cache and returns removed files and bytes.
pub fn clear_derived_cache(
    project_root: &Path,
    store: MemoryStore,
) -> std::io::Result<(usize, u64)> {
    let location = match store {
        MemoryStore::KnowledgeIndex => ".umadev/kb-index",
        MemoryStore::RepoMap => ".umadev/repomap-cache",
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "only derived caches may be cleared without a tombstone",
            ));
        }
    };
    let (_boundary, root) = open_scope_root(project_root, MemoryScope::Project)?;
    clear_derived_cache_rooted(&root, Path::new(location))
}

fn clear_derived_cache_rooted(
    root: &umadev_state::fs::RootedDir,
    relative: &Path,
) -> std::io::Result<(usize, u64)> {
    root.validate_path(relative)?;
    if !root.is_real_dir(relative)? {
        return Ok((0, 0));
    }
    // The complete bounded walk happens before the first unlink so a static
    // link/reparse/special entry never causes a partial cache clear.
    let files = root.list_regular_tree(
        relative,
        MAX_INVENTORY_DEPTH,
        MAX_LIFECYCLE_NODES,
        MAX_INVENTORY_FILES,
        MAX_LIFECYCLE_BYTES,
    )?;
    let bytes = files.iter().try_fold(0u64, |total, file| {
        total
            .checked_add(file.len)
            .ok_or_else(|| invalid_data("cache byte count overflow"))
    })?;
    for file in &files {
        let proof = root.read_bounded_verified(&file.relative, file.len)?;
        if proof.len() != file.len
            || !root.remove_regular_file_if_unchanged(&file.relative, &proof)?
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "cache file changed during rooted removal",
            ));
        }
    }
    root.remove_empty_directory_tree(relative, MAX_INVENTORY_DEPTH, MAX_LIFECYCLE_NODES)?;
    Ok((files.len(), bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_separates_authoritative_and_derived_stores() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join(".umadev/memory")).unwrap();
        std::fs::write(temp.path().join(".umadev/memory/facts.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(temp.path().join(".umadev/kb-index")).unwrap();
        std::fs::write(temp.path().join(".umadev/kb-index/bm25.bin"), "cache").unwrap();

        let report = inventory(temp.path(), MemoryScope::Project);
        let facts = report
            .entries
            .iter()
            .find(|entry| entry.store == MemoryStore::Facts)
            .unwrap();
        let cache = report
            .entries
            .iter()
            .find(|entry| entry.store == MemoryStore::KnowledgeIndex)
            .unwrap();
        assert_eq!(facts.files, 1);
        assert_eq!(facts.capture, Some(true));
        assert_eq!(cache.files, 1);
        assert_eq!(cache.capture, None);
    }

    #[test]
    fn capture_and_recall_are_independent() {
        let temp = tempfile::tempdir().unwrap();
        update_capture(
            temp.path(),
            MemoryScope::Project,
            Some(MemoryStore::Facts),
            false,
        )
        .unwrap();
        let report = inventory(temp.path(), MemoryScope::Project);
        let facts = report
            .entries
            .iter()
            .find(|entry| entry.store == MemoryStore::Facts)
            .unwrap();
        assert_eq!(facts.capture, Some(false));
        assert_eq!(facts.recall, Some(true));
    }

    #[test]
    fn only_derived_caches_can_be_physically_cleared() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join(".umadev/kb-index");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("bm25.bin"), "cache").unwrap();
        assert_eq!(
            clear_derived_cache(temp.path(), MemoryStore::KnowledgeIndex).unwrap(),
            (1, 5)
        );
        assert!(!cache.exists());
        assert!(clear_derived_cache(temp.path(), MemoryStore::Facts).is_err());
    }

    #[test]
    fn inventory_uses_disjoint_leaf_classifiers() {
        let temp = tempfile::tempdir().unwrap();
        let raw = temp.path().join(".umadev/learned/_raw");
        std::fs::create_dir_all(&raw).unwrap();
        std::fs::write(raw.join("quality-failures.jsonl"), "quality\n").unwrap();
        std::fs::write(raw.join("dev-errors.jsonl"), "pitfall\n").unwrap();
        let domain = temp.path().join(".umadev/learned/api");
        std::fs::create_dir_all(&domain).unwrap();
        std::fs::write(
            domain.join("lesson-managed.md"),
            "---\nmaintainer: auto-sediment\n---\n# Managed\n",
        )
        .unwrap();
        std::fs::write(domain.join("manual.md"), "# User owned\n").unwrap();
        let mirrors = temp.path().join(".umadev/learned/skills");
        std::fs::create_dir_all(&mirrors).unwrap();
        std::fs::write(mirrors.join("skill.md"), "# Mirror\n").unwrap();

        let report = inventory(temp.path(), MemoryScope::Project);
        let files = |store| {
            report
                .entries
                .iter()
                .find(|entry| entry.store == store)
                .unwrap()
                .files
        };
        assert_eq!(files(MemoryStore::QualityFailures), 1);
        assert_eq!(files(MemoryStore::Pitfalls), 1);
        assert_eq!(files(MemoryStore::LessonSediment), 1);
        assert_eq!(files(MemoryStore::LearnedSkillMirrors), 1);
    }

    #[test]
    fn selector_policy_update_is_atomic_and_scope_checked() {
        let temp = tempfile::tempdir().unwrap();
        let stores =
            MemorySelector::Group(MemoryGroup::Conversation).recall_stores(MemoryScope::Project);
        update_recall_stores(temp.path(), MemoryScope::Project, &stores, false).unwrap();
        let policy = umadev_state::memory::load_policy(temp.path()).unwrap();
        for store in stores {
            assert!(!policy.recall_enabled(store), "{} stayed on", store.id());
        }
        assert!(update_capture(
            temp.path(),
            MemoryScope::Global,
            Some(MemoryStore::Facts),
            false,
        )
        .is_err());
    }

    #[test]
    fn gate_adrs_are_default_on_capture_only_learning_memory() {
        let temp = tempfile::tempdir().unwrap();
        let lessons =
            MemorySelector::Group(MemoryGroup::Lessons).capture_stores(MemoryScope::Project);
        let learning =
            MemorySelector::Group(MemoryGroup::Learning).capture_stores(MemoryScope::Project);
        assert!(lessons.contains(&MemoryStore::GateAdrs));
        assert!(learning.contains(&MemoryStore::GateAdrs));
        assert!(capture_enabled(
            temp.path(),
            MemoryScope::Project,
            MemoryStore::GateAdrs
        ));
        assert!(!recall_enabled(
            temp.path(),
            MemoryScope::Project,
            MemoryStore::GateAdrs
        ));
        let forget_all =
            MemorySelector::Group(MemoryGroup::All).forget_stores(MemoryScope::Project);
        assert!(!forget_all.contains(&MemoryStore::Tombstones));
        assert!(!forget_all.contains(&MemoryStore::DeletionAudit));
        assert!(forget_all.contains(&MemoryStore::GateAdrs));
    }

    #[test]
    fn retention_policy_has_a_real_soft_delete_executor() {
        let temp = tempfile::tempdir().unwrap();
        let chat = temp.path().join(".umadev/chat");
        std::fs::create_dir_all(&chat).unwrap();
        let active = chat.join("old-session.json");
        std::fs::write(&active, r#"{"private":"do-not-log"}"#).unwrap();
        update_retention(
            temp.path(),
            MemoryScope::Project,
            MemoryStore::ChatSessions,
            Some(30),
        )
        .unwrap();
        let future = SystemTime::now() + Duration::from_secs(31 * 24 * 60 * 60);
        let report = enforce_retention_at(
            temp.path(),
            MemoryScope::Project,
            MemoryStore::ChatSessions,
            future,
        )
        .unwrap();
        assert_eq!(report.retention_days, Some(30));
        assert_eq!(report.scanned_files, 1);
        assert_eq!(report.forgotten_files, 1);
        assert!(!active.exists());
        let id = report.tombstone_id.unwrap();
        let payload = temp
            .path()
            .join(".umadev/memory/tombstones")
            .join(id)
            .join("payload/.umadev/chat/old-session.json");
        assert_eq!(
            std::fs::read_to_string(payload).unwrap(),
            r#"{"private":"do-not-log"}"#
        );
    }

    #[test]
    fn retention_rejects_fixed_or_zero_day_policies() {
        let temp = tempfile::tempdir().unwrap();
        let fixed = update_retention(
            temp.path(),
            MemoryScope::Project,
            MemoryStore::Facts,
            Some(30),
        )
        .unwrap_err();
        assert_eq!(fixed.kind(), std::io::ErrorKind::InvalidInput);
        let zero = update_retention(
            temp.path(),
            MemoryScope::Project,
            MemoryStore::ChatSessions,
            Some(0),
        )
        .unwrap_err();
        assert_eq!(zero.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn forget_is_confirmed_soft_deletion_with_content_free_audit() {
        let temp = tempfile::tempdir().unwrap();
        let memory = temp.path().join(".umadev/memory");
        std::fs::create_dir_all(&memory).unwrap();
        let active = memory.join("facts.jsonl");
        let secret = "customer-secret-fact";
        std::fs::write(&active, secret).unwrap();
        assert!(forget(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            false,
        )
        .is_err());
        assert!(active.exists());

        let report = forget(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            true,
        )
        .unwrap();
        assert_eq!(report.files, 1);
        assert!(!active.exists());
        let id = report.tombstone_id.unwrap();
        let payload = temp
            .path()
            .join(".umadev/memory/tombstones")
            .join(&id)
            .join("payload/.umadev/memory/facts.jsonl");
        assert_eq!(std::fs::read_to_string(payload).unwrap(), secret);
        let audit = temp
            .path()
            .join(".umadev/memory/audit/deletions")
            .join(format!("{id}.json"));
        let audit_text = std::fs::read_to_string(audit).unwrap();
        assert!(!audit_text.contains(secret));
        assert!(!audit_text.contains("facts.jsonl"));
        assert!(audit_text.contains("\"physically_deleted\": false"));
    }

    #[test]
    fn restore_is_no_clobber_and_publishes_a_content_free_disposition() {
        let temp = tempfile::tempdir().unwrap();
        let memory = temp.path().join(".umadev/memory");
        std::fs::create_dir_all(&memory).unwrap();
        let active = memory.join("facts.jsonl");
        let secret = "private-restored-fact";
        std::fs::write(&active, secret).unwrap();
        let forgotten = forget(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            true,
        )
        .unwrap();
        let id = forgotten.tombstone_id.unwrap();

        assert!(restore(temp.path(), MemoryScope::Project, &id, false).is_err());
        let restored = restore(temp.path(), MemoryScope::Project, &id, true).unwrap();
        assert_eq!(restored.files, 1);
        assert_eq!(std::fs::read_to_string(&active).unwrap(), secret);
        let tombstone = temp.path().join(".umadev/memory/tombstones").join(&id);
        assert!(!tombstone
            .join("payload/.umadev/memory/facts.jsonl")
            .exists());
        let disposition =
            umadev_state::lifecycle::read_tombstone_action(&tombstone.join("disposition.json"))
                .unwrap();
        assert_eq!(
            disposition.action,
            umadev_state::lifecycle::TombstoneAction::Restore
        );
        assert!(!disposition.logically_unlinked);
        assert!(!disposition.physically_deleted);
        let audit = temp
            .path()
            .join(".umadev/memory/audit/lifecycle-actions")
            .join(format!("{}.json", disposition.id));
        let audit_text = std::fs::read_to_string(audit).unwrap();
        assert!(!audit_text.contains(secret));
        assert!(!audit_text.contains("facts.jsonl"));
        assert_eq!(
            restore(temp.path(), MemoryScope::Project, &id, true)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn restore_conflict_preserves_both_active_and_recoverable_content() {
        let temp = tempfile::tempdir().unwrap();
        let memory = temp.path().join(".umadev/memory");
        std::fs::create_dir_all(&memory).unwrap();
        let active = memory.join("facts.jsonl");
        std::fs::write(&active, "old-private-fact").unwrap();
        let forgotten = forget(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            true,
        )
        .unwrap();
        let id = forgotten.tombstone_id.unwrap();
        std::fs::write(&active, "concurrent-new-fact").unwrap();

        let error = restore(temp.path(), MemoryScope::Project, &id, true).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read_to_string(&active).unwrap(),
            "concurrent-new-fact"
        );
        let tombstone = temp.path().join(".umadev/memory/tombstones").join(id);
        assert_eq!(
            std::fs::read_to_string(tombstone.join("payload/.umadev/memory/facts.jsonl")).unwrap(),
            "old-private-fact"
        );
        assert!(!tombstone.join("disposition.json").exists());
    }

    #[test]
    fn rollback_never_unlinks_a_concurrently_replaced_target() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("payload"), "recoverable").unwrap();
        std::fs::write(temp.path().join("active"), "concurrent!").unwrap();
        let root = umadev_state::fs::RootedDir::open(temp.path()).unwrap();
        let error = rollback_rooted_transfers(
            &root,
            &[RootedTransfer {
                source: PathBuf::from("payload"),
                destination: PathBuf::from("active"),
                bytes: 11,
                sha256: Sha256::digest(b"recoverable").into(),
            }],
        )
        .unwrap_err();
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::WouldBlock
        ));
        assert_eq!(
            std::fs::read_to_string(temp.path().join("payload")).unwrap(),
            "recoverable"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("active")).unwrap(),
            "concurrent!"
        );
    }

    #[test]
    fn rollback_never_recreates_payload_from_a_replaced_active_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("active"), "concurrent!").unwrap();
        let root = umadev_state::fs::RootedDir::open(temp.path()).unwrap();
        let error = rollback_rooted_transfers(
            &root,
            &[RootedTransfer {
                source: PathBuf::from("payload"),
                destination: PathBuf::from("active"),
                bytes: 11,
                sha256: Sha256::digest(b"recoverable").into(),
            }],
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(!temp.path().join("payload").exists());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("active")).unwrap(),
            "concurrent!"
        );
    }

    #[test]
    fn purge_is_explicit_logical_unlink_not_physical_erasure() {
        let temp = tempfile::tempdir().unwrap();
        let memory = temp.path().join(".umadev/memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("facts.jsonl"), "private-purge-value").unwrap();
        let forgotten = forget(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            true,
        )
        .unwrap();
        let id = forgotten.tombstone_id.unwrap();
        assert!(purge(temp.path(), MemoryScope::Project, &id, false).is_err());

        let report = purge(temp.path(), MemoryScope::Project, &id, true).unwrap();
        assert_eq!(report.files, 1);
        assert!(report.logically_unlinked);
        assert!(!report.physically_deleted);
        let tombstone = temp.path().join(".umadev/memory/tombstones").join(id);
        assert!(!tombstone
            .join("payload/.umadev/memory/facts.jsonl")
            .exists());
        let disposition =
            umadev_state::lifecycle::read_tombstone_action(&tombstone.join("disposition.json"))
                .unwrap();
        assert!(disposition.logically_unlinked);
        assert!(!disposition.physically_deleted);
    }

    #[test]
    fn export_requires_absolute_path_confirmation_and_never_replaces() {
        let temp = tempfile::tempdir().unwrap();
        let memory = temp.path().join(".umadev/memory");
        std::fs::create_dir_all(&memory).unwrap();
        let secret = "private-export-value";
        std::fs::write(memory.join("facts.jsonl"), secret).unwrap();
        let destination = temp.path().join("memory-export.zip");
        assert!(export(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            &destination,
            false,
        )
        .is_err());
        assert!(!destination.exists());
        assert!(export(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            Path::new("relative.zip"),
            true,
        )
        .is_err());

        let report = export(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            &destination,
            true,
        )
        .unwrap();
        assert_eq!(report.files, 1);
        let file = std::fs::File::open(&destination).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut content = String::new();
        std::io::Read::read_to_string(
            &mut archive
                .by_name("memory/.umadev/memory/facts.jsonl")
                .unwrap(),
            &mut content,
        )
        .unwrap();
        assert_eq!(content, secret);
        let mut manifest_text = String::new();
        std::io::Read::read_to_string(
            &mut archive.by_name("manifest.json").unwrap(),
            &mut manifest_text,
        )
        .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(manifest["version"].as_u64(), Some(2));
        let exported = &manifest["files"][0];
        assert_eq!(exported["bytes"].as_u64(), Some(secret.len() as u64));
        assert_eq!(
            exported["sha256"].as_str(),
            Some(format!("{:x}", Sha256::digest(secret.as_bytes())).as_str())
        );
        assert_eq!(
            export(
                temp.path(),
                MemoryScope::Project,
                &[MemoryStore::Facts],
                &destination,
                true,
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn malformed_retention_policy_is_fail_open_for_business_paths() {
        let temp = tempfile::tempdir().unwrap();
        let chat = temp.path().join(".umadev/chat");
        std::fs::create_dir_all(&chat).unwrap();
        let active = chat.join("session.json");
        std::fs::write(&active, "private").unwrap();
        let policy = temp.path().join(".umadev/memory/policy.toml");
        std::fs::create_dir_all(policy.parent().unwrap()).unwrap();
        std::fs::write(policy, "capture = maybe").unwrap();
        let report = enforce_retention_fail_open(
            temp.path(),
            MemoryScope::Project,
            MemoryStore::ChatSessions,
        );
        assert!(report.error.is_some());
        assert!(active.exists());
    }

    #[cfg(unix)]
    #[test]
    fn cache_preflight_rejects_links_before_deleting_any_file() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join(".umadev/kb-index");
        std::fs::create_dir_all(&cache).unwrap();
        let keep = cache.join("keep.bin");
        std::fs::write(&keep, "keep").unwrap();
        let outside = temp.path().join("outside");
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, cache.join("linked.bin")).unwrap();

        assert!(clear_derived_cache(temp.path(), MemoryStore::KnowledgeIndex).is_err());
        assert_eq!(std::fs::read_to_string(keep).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside");
    }

    #[cfg(unix)]
    #[test]
    fn forget_preflight_rejects_links_before_moving_any_file() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let umadev = temp.path().join(".umadev");
        std::fs::create_dir_all(&umadev).unwrap();
        let keep = umadev.join("run-notes.md");
        std::fs::write(&keep, "keep").unwrap();
        let outside = temp.path().join("outside.md");
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, umadev.join("run-notes.prev.md")).unwrap();

        assert!(forget(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::RunNotes],
            true,
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(keep).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside");
        assert!(!umadev.join("memory/tombstones").exists());
    }

    #[cfg(unix)]
    #[test]
    fn purge_rejects_a_tampered_link_before_unlinking_payload() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let memory = temp.path().join(".umadev/memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("facts.jsonl"), "recoverable").unwrap();
        let forgotten = forget(
            temp.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            true,
        )
        .unwrap();
        let id = forgotten.tombstone_id.unwrap();
        let tombstone = temp.path().join(".umadev/memory/tombstones").join(&id);
        let payload_file = tombstone.join("payload/.umadev/memory/facts.jsonl");
        let outside = temp.path().join("outside");
        std::fs::write(&outside, "outside").unwrap();
        symlink(&outside, tombstone.join("payload/tampered-link")).unwrap();

        assert!(purge(temp.path(), MemoryScope::Project, &id, true).is_err());
        assert_eq!(
            std::fs::read_to_string(payload_file).unwrap(),
            "recoverable"
        );
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "outside");
        assert!(!tombstone.join("disposition.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_operations_reject_linked_ancestor_directories() {
        use std::os::unix::fs::symlink;

        let project = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_memory = outside.path().join("memory");
        std::fs::create_dir_all(&outside_memory).unwrap();
        let fact = outside_memory.join("facts.jsonl");
        std::fs::write(&fact, "outside-private-fact").unwrap();
        symlink(outside.path(), project.path().join(".umadev")).unwrap();

        let error = forget(
            project.path(),
            MemoryScope::Project,
            &[MemoryStore::Facts],
            true,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            std::fs::read_to_string(fact).unwrap(),
            "outside-private-fact"
        );
        assert!(!outside_memory.join("tombstones").exists());
    }

    #[cfg(unix)]
    #[test]
    fn soft_delete_stays_on_captured_root_after_workspace_path_replacement() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let moved_workspace = parent.path().join("workspace-captured");
        let outside = parent.path().join("outside");
        std::fs::create_dir_all(workspace.join(".umadev/memory")).unwrap();
        std::fs::create_dir_all(outside.join(".umadev/memory")).unwrap();
        std::fs::write(
            workspace.join(".umadev/memory/facts.jsonl"),
            "captured-private-fact",
        )
        .unwrap();
        let outside_fact = outside.join(".umadev/memory/facts.jsonl");
        std::fs::write(&outside_fact, "outside-must-survive").unwrap();

        let boundary = scope_boundary(&workspace, MemoryScope::Project).unwrap();
        let root = umadev_state::fs::RootedDir::open(&boundary).unwrap();
        let files =
            collect_store_files(&boundary, MemoryScope::Project, MemoryStore::Facts).unwrap();
        let report = soft_delete_files_with_hook(
            root,
            MemoryScope::Project,
            &[MemoryStore::Facts],
            umadev_state::lifecycle::LifecycleOperation::Forget,
            &files,
            || {
                std::fs::rename(&workspace, &moved_workspace).unwrap();
                symlink(&outside, &workspace).unwrap();
            },
        )
        .unwrap();

        let id = report.unwrap();
        assert_eq!(
            std::fs::read_to_string(&outside_fact).unwrap(),
            "outside-must-survive"
        );
        assert!(!outside.join(".umadev/memory/tombstones").exists());
        assert!(!moved_workspace.join(".umadev/memory/facts.jsonl").exists());
        assert_eq!(
            std::fs::read_to_string(
                moved_workspace
                    .join(".umadev/memory/tombstones")
                    .join(id)
                    .join("payload/.umadev/memory/facts.jsonl")
            )
            .unwrap(),
            "captured-private-fact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replaced_active_and_payload_ancestor_fails_without_touching_outside() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let original_umadev = workspace.join(".umadev-captured");
        let outside = parent.path().join("outside");
        std::fs::create_dir_all(workspace.join(".umadev/memory")).unwrap();
        std::fs::create_dir_all(outside.join("memory")).unwrap();
        std::fs::write(
            workspace.join(".umadev/memory/facts.jsonl"),
            "captured-private-fact",
        )
        .unwrap();
        let outside_fact = outside.join("memory/facts.jsonl");
        std::fs::write(&outside_fact, "outside-must-survive").unwrap();

        let boundary = scope_boundary(&workspace, MemoryScope::Project).unwrap();
        let root = umadev_state::fs::RootedDir::open(&boundary).unwrap();
        let files =
            collect_store_files(&boundary, MemoryScope::Project, MemoryStore::Facts).unwrap();
        let _error = soft_delete_files_with_hook(
            root,
            MemoryScope::Project,
            &[MemoryStore::Facts],
            umadev_state::lifecycle::LifecycleOperation::Forget,
            &files,
            || {
                std::fs::rename(workspace.join(".umadev"), &original_umadev).unwrap();
                symlink(&outside, workspace.join(".umadev")).unwrap();
            },
        )
        .unwrap_err();

        assert_eq!(
            std::fs::read_to_string(&outside_fact).unwrap(),
            "outside-must-survive"
        );
        assert!(!outside.join("memory/tombstones").exists());
        assert_eq!(
            std::fs::read_to_string(original_umadev.join("memory/facts.jsonl")).unwrap(),
            "captured-private-fact"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cache_clear_uses_captured_root_after_workspace_path_replacement() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let moved_workspace = parent.path().join("workspace-captured");
        let outside = parent.path().join("outside");
        std::fs::create_dir_all(workspace.join(".umadev/kb-index/nested")).unwrap();
        std::fs::create_dir_all(outside.join(".umadev/kb-index")).unwrap();
        std::fs::write(
            workspace.join(".umadev/kb-index/nested/cache.bin"),
            "captured-cache",
        )
        .unwrap();
        let outside_cache = outside.join(".umadev/kb-index/cache.bin");
        std::fs::write(&outside_cache, "outside-cache").unwrap();
        let root = umadev_state::fs::RootedDir::open(&workspace).unwrap();
        std::fs::rename(&workspace, &moved_workspace).unwrap();
        symlink(&outside, &workspace).unwrap();

        assert_eq!(
            clear_derived_cache_rooted(&root, Path::new(".umadev/kb-index")).unwrap(),
            (1, 14)
        );
        assert_eq!(
            std::fs::read_to_string(&outside_cache).unwrap(),
            "outside-cache"
        );
        assert!(!moved_workspace.join(".umadev/kb-index").exists());
    }
}
