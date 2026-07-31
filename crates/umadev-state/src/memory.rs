use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

const POLICY_VERSION: u32 = 1;
const MAX_POLICY_BYTES: u64 = 64 * 1024;
const POLICY_LOCK_DIR: &str = ".policy.lock";
const POLICY_LOCK_OWNER: &str = "owner";
const POLICY_LOCK_LEASE: &str = "lease";
const POLICY_LOCK_GUARD: &str = ".policy.lock.guard";
const POLICY_LOCK_ATTEMPTS: usize = 500;
const POLICY_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(2);
const POLICY_LOCK_STALE_AFTER_MS: u64 = 5 * 60 * 1_000;
const POLICY_LOCK_FUTURE_SKEW_MS: u64 = 60 * 1_000;
static POLICY_LOCK_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemoryStore {
    QualityFailures,
    GateRevisions,
    ValidatedPatterns,
    TechDebt,
    Pitfalls,
    Beliefs,
    PitfallReflections,
    GateAdrs,
    OpenDecisions,
    Facts,
    RunNotes,
    Recipes,
    LearnedSkills,
    KnowledgeReceipts,
    KnowledgeUtility,
    CustomKnowledge,
    SkillPackages,
    ChatSessions,
    InputHistory,
    LessonSediment,
    LearnedSkillMirrors,
    GlobalLessonProjection,
    GlobalLessonsManual,
    KnowledgeIndex,
    RepoMap,
    BundledKnowledge,
    EmbeddingModel,
    Tombstones,
    DeletionAudit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionEnforcement {
    Fixed,
    /// A user-configurable age policy with an explicit executor.
    PolicyOnly,
    Unsupported,
}

impl MemoryStore {
    pub const ALL: [Self; 29] = [
        Self::QualityFailures,
        Self::GateRevisions,
        Self::ValidatedPatterns,
        Self::TechDebt,
        Self::Pitfalls,
        Self::Beliefs,
        Self::PitfallReflections,
        Self::GateAdrs,
        Self::OpenDecisions,
        Self::Facts,
        Self::RunNotes,
        Self::Recipes,
        Self::LearnedSkills,
        Self::KnowledgeReceipts,
        Self::KnowledgeUtility,
        Self::CustomKnowledge,
        Self::SkillPackages,
        Self::ChatSessions,
        Self::InputHistory,
        Self::LessonSediment,
        Self::LearnedSkillMirrors,
        Self::GlobalLessonProjection,
        Self::GlobalLessonsManual,
        Self::KnowledgeIndex,
        Self::RepoMap,
        Self::BundledKnowledge,
        Self::EmbeddingModel,
        Self::Tombstones,
        Self::DeletionAudit,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::QualityFailures => "quality-failures",
            Self::GateRevisions => "gate-revisions",
            Self::ValidatedPatterns => "validated-patterns",
            Self::TechDebt => "tech-debt",
            Self::Pitfalls => "pitfalls",
            Self::Beliefs => "beliefs",
            Self::PitfallReflections => "pitfall-reflections",
            Self::GateAdrs => "gate-adrs",
            Self::OpenDecisions => "open-decisions",
            Self::Facts => "facts",
            Self::RunNotes => "run-notes",
            Self::Recipes => "recipes",
            Self::LearnedSkills => "learned-skills",
            Self::KnowledgeReceipts => "knowledge-receipts",
            Self::KnowledgeUtility => "knowledge-utility",
            Self::CustomKnowledge => "custom-knowledge",
            Self::SkillPackages => "skill-packages",
            Self::ChatSessions => "chat-sessions",
            Self::InputHistory => "input-history",
            Self::LessonSediment => "lesson-sediment",
            Self::LearnedSkillMirrors => "learned-skill-mirrors",
            Self::GlobalLessonProjection => "global-lesson-projection",
            Self::GlobalLessonsManual => "global-lessons-manual",
            Self::KnowledgeIndex => "knowledge-index",
            Self::RepoMap => "repomap",
            Self::BundledKnowledge => "bundled-knowledge",
            Self::EmbeddingModel => "embedding-model",
            Self::Tombstones => "tombstones",
            Self::DeletionAudit => "deletion-audit",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL.into_iter().find(|store| store.id() == normalized)
    }

    #[must_use]
    pub const fn capture_controllable(self) -> bool {
        matches!(
            self,
            Self::QualityFailures
                | Self::GateRevisions
                | Self::ValidatedPatterns
                | Self::TechDebt
                | Self::Pitfalls
                | Self::Beliefs
                | Self::PitfallReflections
                | Self::GateAdrs
                | Self::Facts
                | Self::RunNotes
                | Self::Recipes
                | Self::LearnedSkills
                | Self::KnowledgeReceipts
                | Self::KnowledgeUtility
                | Self::ChatSessions
                | Self::InputHistory
                | Self::LessonSediment
                | Self::LearnedSkillMirrors
                | Self::GlobalLessonProjection
        )
    }

    #[must_use]
    pub const fn recall_controllable(self) -> bool {
        matches!(
            self,
            Self::QualityFailures
                | Self::GateRevisions
                | Self::ValidatedPatterns
                | Self::TechDebt
                | Self::Pitfalls
                | Self::Beliefs
                | Self::PitfallReflections
                | Self::OpenDecisions
                | Self::Facts
                | Self::RunNotes
                | Self::Recipes
                | Self::LearnedSkills
                | Self::KnowledgeUtility
                | Self::CustomKnowledge
                | Self::SkillPackages
                | Self::ChatSessions
                | Self::InputHistory
                | Self::LessonSediment
                | Self::LearnedSkillMirrors
                | Self::GlobalLessonProjection
                | Self::GlobalLessonsManual
                | Self::BundledKnowledge
        )
    }

    #[must_use]
    pub const fn default_capture(self) -> bool {
        self.capture_controllable() && !matches!(self, Self::KnowledgeUtility)
    }

    #[must_use]
    pub const fn default_recall(self) -> bool {
        self.recall_controllable()
    }

    #[must_use]
    pub const fn supports_project_scope(self) -> bool {
        !matches!(
            self,
            Self::KnowledgeUtility
                | Self::GlobalLessonProjection
                | Self::GlobalLessonsManual
                | Self::BundledKnowledge
                | Self::EmbeddingModel
        )
    }

    #[must_use]
    pub const fn supports_global_scope(self) -> bool {
        matches!(
            self,
            Self::KnowledgeUtility
                | Self::GlobalLessonProjection
                | Self::GlobalLessonsManual
                | Self::BundledKnowledge
                | Self::EmbeddingModel
                | Self::Tombstones
                | Self::DeletionAudit
        )
    }

    #[must_use]
    pub const fn derived(self) -> bool {
        matches!(
            self,
            Self::LessonSediment | Self::LearnedSkillMirrors | Self::KnowledgeIndex | Self::RepoMap
        )
    }

    #[must_use]
    pub const fn clearable_cache(self) -> bool {
        matches!(self, Self::KnowledgeIndex | Self::RepoMap)
    }

    #[must_use]
    pub const fn retention_enforcement(self) -> RetentionEnforcement {
        match self {
            Self::Pitfalls
            | Self::Beliefs
            | Self::PitfallReflections
            | Self::Facts
            | Self::RunNotes
            | Self::Recipes
            | Self::LearnedSkills
            | Self::InputHistory => RetentionEnforcement::Fixed,
            Self::QualityFailures
            | Self::GateRevisions
            | Self::ValidatedPatterns
            | Self::TechDebt
            | Self::KnowledgeReceipts
            | Self::KnowledgeUtility
            | Self::ChatSessions
            | Self::GlobalLessonProjection
            | Self::GlobalLessonsManual => RetentionEnforcement::PolicyOnly,
            _ => RetentionEnforcement::Unsupported,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryPolicy {
    #[serde(default = "policy_version")]
    pub version: u32,
    #[serde(default = "enabled")]
    pub capture: bool,
    #[serde(default = "enabled")]
    pub recall: bool,
    #[serde(default)]
    pub stores: BTreeMap<String, StorePolicy>,
}

const fn policy_version() -> u32 {
    POLICY_VERSION
}

const fn enabled() -> bool {
    true
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self {
            version: POLICY_VERSION,
            capture: true,
            recall: true,
            stores: BTreeMap::new(),
        }
    }
}

impl MemoryPolicy {
    #[must_use]
    pub fn capture_enabled(&self, store: MemoryStore) -> bool {
        store.capture_controllable()
            && self.capture
            && self
                .stores
                .get(store.id())
                .and_then(|policy| policy.capture)
                .unwrap_or_else(|| store.default_capture())
    }

    #[must_use]
    pub fn recall_enabled(&self, store: MemoryStore) -> bool {
        store.recall_controllable()
            && self.recall
            && self
                .stores
                .get(store.id())
                .and_then(|policy| policy.recall)
                .unwrap_or_else(|| store.default_recall())
    }

    #[must_use]
    pub fn retention_days(&self, store: MemoryStore) -> Option<u32> {
        self.stores
            .get(store.id())
            .and_then(|policy| policy.retention_days)
    }

    pub fn set_capture(&mut self, store: Option<MemoryStore>, enabled: bool) {
        if let Some(store) = store {
            self.stores
                .entry(store.id().to_string())
                .or_default()
                .capture = Some(enabled);
        } else {
            self.capture = enabled;
        }
    }

    pub fn set_recall(&mut self, store: Option<MemoryStore>, enabled: bool) {
        if let Some(store) = store {
            self.stores
                .entry(store.id().to_string())
                .or_default()
                .recall = Some(enabled);
        } else {
            self.recall = enabled;
        }
    }

    pub fn set_retention_days(&mut self, store: MemoryStore, days: Option<u32>) {
        let entry = self.stores.entry(store.id().to_string()).or_default();
        entry.retention_days = days.filter(|days| *days > 0);
        if entry == &StorePolicy::default() {
            self.stores.remove(store.id());
        }
    }

    fn validate(&self) -> std::io::Result<()> {
        if self.version != POLICY_VERSION {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unsupported memory policy version {}", self.version),
            ));
        }
        if let Some(unknown) = self
            .stores
            .keys()
            .find(|name| MemoryStore::parse(name).is_none_or(|store| store.id() != name.as_str()))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown or non-canonical memory store `{unknown}`"),
            ));
        }
        Ok(())
    }
}

pub fn policy_path(boundary: &Path) -> std::io::Result<PathBuf> {
    let root = std::fs::canonicalize(boundary)?;
    if !crate::fs::real_dir(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "memory policy boundary is not a real directory",
        ));
    }
    Ok(root.join(".umadev").join("memory").join("policy.toml"))
}

/// Resolve the memory policy below an explicit `UmaDev` state directory.
///
/// Most callers pass a user/project boundary and use [`policy_path`], which
/// appends `.umadev`. `UMADEV_HOME`, however, already names that state
/// directory, so global stores must not append a second `.umadev` component.
pub fn policy_path_in_state(state: &Path) -> std::io::Result<PathBuf> {
    let state = std::fs::canonicalize(state)?;
    if !crate::fs::real_dir(&state) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "memory policy state root is not a real directory",
        ));
    }
    Ok(state.join("memory").join("policy.toml"))
}

fn memory_relative(state_directory: bool) -> PathBuf {
    if state_directory {
        PathBuf::from("memory")
    } else {
        PathBuf::from(".umadev/memory")
    }
}

fn ensure_memory_dir(
    root: &crate::fs::RootedDir,
    state_directory: bool,
) -> std::io::Result<PathBuf> {
    if !state_directory {
        root.ensure_dir(Path::new(".umadev"), false)?;
    }
    let memory = memory_relative(state_directory);
    root.ensure_dir(&memory, true)?;
    Ok(memory)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn lock_nonce() -> String {
    let sequence = POLICY_LOCK_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{}-{sequence}", std::process::id(), now_ms())
}

fn parse_lock_owner(bytes: &[u8]) -> Option<(u64, &str)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (stamp, nonce) = text.split_once('\n')?;
    let stamp = stamp.parse().ok()?;
    (!nonce.is_empty() && !nonce.contains('\n')).then_some((stamp, nonce))
}

fn policy_owner_matches(root: &crate::fs::RootedDir, path: &Path, nonce: &str) -> bool {
    root.read_bounded(path, 4_096)
        .ok()
        .and_then(|bytes| parse_lock_owner(&bytes).map(|(_, seen)| seen == nonce))
        .unwrap_or(false)
}

struct PolicyLock {
    root: Arc<crate::fs::RootedDir>,
    lock: PathBuf,
    nonce: String,
    lease: Option<std::fs::File>,
    namespace_guard: std::fs::File,
}

impl Drop for PolicyLock {
    fn drop(&mut self) {
        let owner = self.lock.join(POLICY_LOCK_OWNER);
        let Ok(bytes) = self.root.read_bounded(&owner, 4_096) else {
            return;
        };
        if parse_lock_owner(&bytes).is_none_or(|(_, nonce)| nonce != self.nonce) {
            return;
        }
        let _ = self.root.remove_regular_file(&owner);
        drop(self.lease.take());
        let _ = self
            .root
            .remove_regular_file(&self.lock.join(POLICY_LOCK_LEASE));
        let _ = self.root.remove_empty_dir(&self.lock);
        let _ = fs2::FileExt::unlock(&self.namespace_guard);
    }
}

fn policy_lock_modified_at_ms(root: &crate::fs::RootedDir, lock: &Path) -> Option<u64> {
    root.directory_modified(lock)
        .ok()
        .flatten()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn policy_lock_age_ms(root: &crate::fs::RootedDir, lock: &Path, now: u64) -> Option<u64> {
    let future_limit = now.saturating_add(POLICY_LOCK_FUTURE_SKEW_MS);
    let created_at = root
        .read_bounded(&lock.join(POLICY_LOCK_OWNER), 4_096)
        .ok()
        .and_then(|bytes| parse_lock_owner(&bytes).map(|(stamp, _)| stamp))
        .filter(|stamp| *stamp <= future_limit)
        .or_else(|| policy_lock_modified_at_ms(root, lock))?;
    Some(now.saturating_sub(created_at))
}

fn try_acquire_policy_namespace_guard(
    root: &crate::fs::RootedDir,
    memory: &Path,
) -> std::io::Result<Option<std::fs::File>> {
    let file = root.open_private_lock(&memory.join(POLICY_LOCK_GUARD), false)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if crate::fs::lock_error_is_contention(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn reclaim_stale_policy_lock(
    root: &crate::fs::RootedDir,
    lock: &Path,
    _namespace_guard: &std::fs::File,
    stale_after_ms: u64,
) {
    if !root.is_real_dir(lock).unwrap_or(false) {
        return;
    }
    if policy_lock_age_ms(root, lock, now_ms()).is_none_or(|age| age <= stale_after_ms) {
        return;
    }
    let lease_path = lock.join(POLICY_LOCK_LEASE);
    let lease = match root.open_private_existing_lock(&lease_path) {
        Ok(file) => match file.try_lock_exclusive() {
            Ok(()) => Some(file),
            Err(error) if crate::fs::lock_error_is_contention(&error) => return,
            Err(_) => return,
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return,
    };
    let Some(parent) = lock.parent() else {
        return;
    };
    let tomb = parent.join(format!(".policy.lock.stale.{}", lock_nonce()));
    drop(lease);
    if root.rename(lock, &tomb).is_ok() {
        let _ = root.remove_regular_file(&tomb.join(POLICY_LOCK_OWNER));
        let _ = root.remove_regular_file(&tomb.join(POLICY_LOCK_LEASE));
        let _ = root.remove_empty_dir(&tomb);
    }
}

fn acquire_policy_lock(
    root: Arc<crate::fs::RootedDir>,
    state_directory: bool,
) -> std::io::Result<PolicyLock> {
    let memory = ensure_memory_dir(&root, state_directory)?;
    let lock = memory.join(POLICY_LOCK_DIR);
    for _ in 0..POLICY_LOCK_ATTEMPTS {
        let Some(namespace_guard) = try_acquire_policy_namespace_guard(&root, &memory)? else {
            std::thread::sleep(POLICY_LOCK_WAIT);
            continue;
        };
        match root.create_dir(&lock, false) {
            Ok(()) => {
                let nonce = lock_nonce();
                let owner = format!("{}\n{nonce}", now_ms());
                if let Err(error) =
                    root.atomic_write(&lock.join(POLICY_LOCK_OWNER), owner.as_bytes(), false)
                {
                    let _ = root.remove_empty_dir(&lock);
                    return Err(error);
                }
                let lease = match root
                    .open_private_lock(&lock.join(POLICY_LOCK_LEASE), false)
                    .and_then(|file| {
                        file.try_lock_exclusive()?;
                        Ok(file)
                    }) {
                    Ok(file) => file,
                    Err(error) => {
                        if policy_owner_matches(&root, &lock.join(POLICY_LOCK_OWNER), &nonce) {
                            let _ = root.remove_regular_file(&lock.join(POLICY_LOCK_OWNER));
                            let _ = root.remove_regular_file(&lock.join(POLICY_LOCK_LEASE));
                            let _ = root.remove_empty_dir(&lock);
                        }
                        return Err(error);
                    }
                };
                if !policy_owner_matches(&root, &lock.join(POLICY_LOCK_OWNER), &nonce) {
                    drop(lease);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "memory policy lock namespace changed during lease acquisition",
                    ));
                }
                return Ok(PolicyLock {
                    root,
                    lock,
                    nonce,
                    lease: Some(lease),
                    namespace_guard,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                reclaim_stale_policy_lock(
                    &root,
                    &lock,
                    &namespace_guard,
                    POLICY_LOCK_STALE_AFTER_MS,
                );
                drop(namespace_guard);
                std::thread::sleep(POLICY_LOCK_WAIT);
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "memory policy is busy in another UmaDev process",
    ))
}

fn parse_policy(bytes: &[u8]) -> std::io::Result<MemoryPolicy> {
    let text = std::str::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let policy: MemoryPolicy = toml::from_str(text)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    policy.validate()?;
    Ok(policy)
}

/// Load the boundary-scoped policy through an already captured root
/// capability. This is used by multi-step lifecycle operations so an ambient
/// boundary rename cannot redirect the policy read between preflight and
/// mutation.
pub fn load_policy_rooted(
    root: &crate::fs::RootedDir,
    state_directory: bool,
) -> std::io::Result<MemoryPolicy> {
    let path = memory_relative(state_directory).join("policy.toml");
    match root.read_bounded(&path, MAX_POLICY_BYTES) {
        Ok(bytes) => parse_policy(&bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(MemoryPolicy::default()),
        Err(error) => Err(error),
    }
}

pub fn load_policy(boundary: &Path) -> std::io::Result<MemoryPolicy> {
    let root = crate::fs::RootedDir::open(boundary)?;
    load_policy_rooted(&root, false)
}

/// Load policy when the supplied path is the `UmaDev` state directory itself.
pub fn load_policy_in_state(state: &Path) -> std::io::Result<MemoryPolicy> {
    let root = crate::fs::RootedDir::open(state)?;
    load_policy_rooted(&root, true)
}

fn save_policy_unlocked(
    root: &crate::fs::RootedDir,
    state_directory: bool,
    policy: &MemoryPolicy,
) -> std::io::Result<()> {
    policy.validate()?;
    let memory = ensure_memory_dir(root, state_directory)?;
    let path = memory.join("policy.toml");
    let text = toml::to_string_pretty(policy)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    root.atomic_write(&path, text.as_bytes(), false)
}

pub fn save_policy(boundary: &Path, policy: &MemoryPolicy) -> std::io::Result<()> {
    let root = Arc::new(crate::fs::RootedDir::open(boundary)?);
    let _lock = acquire_policy_lock(Arc::clone(&root), false)?;
    save_policy_unlocked(&root, false, policy)
}

pub fn update_policy(
    boundary: &Path,
    update: impl FnOnce(&mut MemoryPolicy) -> std::io::Result<()>,
) -> std::io::Result<MemoryPolicy> {
    let root = Arc::new(crate::fs::RootedDir::open(boundary)?);
    let _lock = acquire_policy_lock(Arc::clone(&root), false)?;
    let mut policy = load_policy_rooted(&root, false)?;
    update(&mut policy)?;
    save_policy_unlocked(&root, false, &policy)?;
    Ok(policy)
}

#[must_use]
pub fn capture_enabled(boundary: &Path, store: MemoryStore) -> bool {
    store.capture_controllable()
        && load_policy(boundary).is_ok_and(|policy| policy.capture_enabled(store))
}

#[must_use]
pub fn recall_enabled(boundary: &Path, store: MemoryStore) -> bool {
    store.recall_controllable()
        && load_policy(boundary).is_ok_and(|policy| policy.recall_enabled(store))
}

/// State-directory variant of [`capture_enabled`], used for `UMADEV_HOME`.
#[must_use]
pub fn capture_enabled_in_state(state: &Path, store: MemoryStore) -> bool {
    store.capture_controllable()
        && load_policy_in_state(state).is_ok_and(|policy| policy.capture_enabled(store))
}

/// State-directory variant of [`recall_enabled`], used for `UMADEV_HOME`.
#[must_use]
pub fn recall_enabled_in_state(state: &Path, store: MemoryStore) -> bool {
    store.recall_controllable()
        && load_policy_in_state(state).is_ok_and(|policy| policy.recall_enabled(store))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_policy_enables_capture_and_recall() {
        let temp = tempfile::tempdir().unwrap();
        assert!(capture_enabled(temp.path(), MemoryStore::Facts));
        assert!(recall_enabled(temp.path(), MemoryStore::Facts));
    }

    #[test]
    fn explicit_state_policy_does_not_append_a_second_umadev_directory() {
        let state = tempfile::tempdir().unwrap();
        let memory = crate::fs::ensure_real_child_dir(state.path(), "memory").unwrap();
        crate::fs::atomic_write(
            &memory.join("policy.toml"),
            b"version = 1\ncapture = true\nrecall = true\n",
        )
        .unwrap();
        assert!(capture_enabled_in_state(state.path(), MemoryStore::Facts));
        assert!(recall_enabled_in_state(state.path(), MemoryStore::Facts));
        let canonical_state = std::fs::canonicalize(state.path()).unwrap();
        assert_eq!(
            policy_path_in_state(state.path()).unwrap(),
            canonical_state.join("memory/policy.toml")
        );
        assert!(!state.path().join(".umadev").exists());
    }

    #[test]
    fn store_override_roundtrips_and_replaces_on_second_save() {
        let temp = tempfile::tempdir().unwrap();
        let mut policy = MemoryPolicy::default();
        policy.set_capture(Some(MemoryStore::Facts), false);
        save_policy(temp.path(), &policy).unwrap();
        assert!(!capture_enabled(temp.path(), MemoryStore::Facts));
        assert!(capture_enabled(temp.path(), MemoryStore::Pitfalls));

        policy.set_recall(Some(MemoryStore::Facts), false);
        save_policy(temp.path(), &policy).unwrap();
        let loaded = load_policy(temp.path()).unwrap();
        assert!(!loaded.capture_enabled(MemoryStore::Facts));
        assert!(!loaded.recall_enabled(MemoryStore::Facts));
    }

    #[test]
    fn malformed_policy_disables_use_instead_of_ignoring_privacy_intent() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::fs::RootedDir::open(temp.path()).unwrap();
        let path = ensure_memory_dir(&root, false).unwrap().join("policy.toml");
        root.atomic_write(&path, b"capture = maybe", false).unwrap();
        assert!(!capture_enabled(temp.path(), MemoryStore::Facts));
        assert!(!recall_enabled(temp.path(), MemoryStore::Facts));
    }

    #[test]
    fn derived_and_user_owned_stores_are_not_toggled_as_learned_memory() {
        let temp = tempfile::tempdir().unwrap();
        for store in [
            MemoryStore::KnowledgeIndex,
            MemoryStore::RepoMap,
            MemoryStore::SkillPackages,
            MemoryStore::DeletionAudit,
        ] {
            assert!(!capture_enabled(temp.path(), store));
        }
        assert!(!recall_enabled(temp.path(), MemoryStore::KnowledgeIndex));
        assert!(capture_enabled(temp.path(), MemoryStore::GateAdrs));
        assert!(!recall_enabled(temp.path(), MemoryStore::GateAdrs));
    }

    #[test]
    fn global_utility_capture_requires_an_explicit_opt_in() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!capture_enabled(temp.path(), MemoryStore::KnowledgeUtility));
        let mut policy = MemoryPolicy::default();
        policy.set_capture(Some(MemoryStore::KnowledgeUtility), true);
        save_policy(temp.path(), &policy).unwrap();
        assert!(capture_enabled(temp.path(), MemoryStore::KnowledgeUtility));
        assert!(recall_enabled(temp.path(), MemoryStore::KnowledgeUtility));
    }

    #[test]
    fn transactional_updates_do_not_lose_independent_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root_a = temp.path().to_path_buf();
        let root_b = root_a.clone();
        let first = std::thread::spawn(move || {
            update_policy(&root_a, |policy| {
                policy.set_capture(Some(MemoryStore::Facts), false);
                std::thread::sleep(std::time::Duration::from_millis(20));
                Ok(())
            })
            .unwrap();
        });
        let second = std::thread::spawn(move || {
            update_policy(&root_b, |policy| {
                policy.set_recall(Some(MemoryStore::Pitfalls), false);
                Ok(())
            })
            .unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();

        let policy = load_policy(temp.path()).unwrap();
        assert!(!policy.capture_enabled(MemoryStore::Facts));
        assert!(!policy.recall_enabled(MemoryStore::Pitfalls));
        assert!(!temp.path().join(".umadev/memory/.policy.lock").exists());
    }

    #[test]
    fn stale_policy_lock_is_reclaimed_without_recursive_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::fs::RootedDir::open(temp.path()).unwrap();
        let memory = ensure_memory_dir(&root, false).unwrap();
        let lock = memory.join(POLICY_LOCK_DIR);
        root.create_dir(&lock, false).unwrap();
        let owner = format!("{}\nstale-test", now_ms() - POLICY_LOCK_STALE_AFTER_MS - 1);
        root.atomic_write(&lock.join(POLICY_LOCK_OWNER), owner.as_bytes(), false)
            .unwrap();

        update_policy(temp.path(), |policy| {
            policy.set_capture(Some(MemoryStore::Facts), false);
            Ok(())
        })
        .unwrap();
        assert!(!capture_enabled(temp.path(), MemoryStore::Facts));
        assert!(!temp.path().join(lock).exists());
    }

    #[test]
    fn ownerless_crash_policy_lock_is_reclaimable() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::fs::RootedDir::open(temp.path()).unwrap();
        let memory = ensure_memory_dir(&root, false).unwrap();
        let lock = memory.join(POLICY_LOCK_DIR);
        root.create_dir(&lock, false).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));

        let namespace_guard = try_acquire_policy_namespace_guard(&root, &memory)
            .unwrap()
            .unwrap();
        reclaim_stale_policy_lock(&root, &lock, &namespace_guard, 0);
        drop(namespace_guard);
        assert!(!temp.path().join(lock).exists());
        save_policy(temp.path(), &MemoryPolicy::default()).unwrap();
    }

    #[test]
    fn released_stale_policy_lease_is_reclaimed_with_parent_arbitration() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::fs::RootedDir::open(temp.path()).unwrap();
        let memory = ensure_memory_dir(&root, false).unwrap();
        let lock = memory.join(POLICY_LOCK_DIR);
        root.create_dir(&lock, false).unwrap();
        root.atomic_write(
            &lock.join(POLICY_LOCK_OWNER),
            format!("0\n{}", lock_nonce()).as_bytes(),
            false,
        )
        .unwrap();
        let lease = root
            .open_private_lock(&lock.join(POLICY_LOCK_LEASE), false)
            .unwrap();
        lease.try_lock_exclusive().unwrap();
        drop(lease);

        let namespace_guard = try_acquire_policy_namespace_guard(&root, &memory)
            .unwrap()
            .unwrap();
        reclaim_stale_policy_lock(&root, &lock, &namespace_guard, 0);
        assert!(!temp.path().join(lock).exists());
    }

    #[test]
    fn active_policy_lease_cannot_be_reclaimed_by_age() {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(crate::fs::RootedDir::open(temp.path()).unwrap());
        let guard = acquire_policy_lock(root, false).unwrap();
        let owner = format!("0\n{}", guard.nonce);
        guard
            .root
            .atomic_write(&guard.lock.join(POLICY_LOCK_OWNER), owner.as_bytes(), false)
            .unwrap();

        reclaim_stale_policy_lock(&guard.root, &guard.lock, &guard.namespace_guard, 0);
        assert!(guard.root.is_real_dir(&guard.lock).unwrap());
        drop(guard);
        assert!(!temp.path().join(".umadev/memory/.policy.lock").exists());
    }

    #[test]
    fn non_canonical_store_keys_fail_closed_instead_of_being_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::fs::RootedDir::open(temp.path()).unwrap();
        let path = ensure_memory_dir(&root, false).unwrap().join("policy.toml");
        root.atomic_write(
            &path,
            b"version = 1\ncapture = true\nrecall = true\n\n[stores.FACTS]\ncapture = false\n",
            false,
        )
        .unwrap();

        assert_eq!(
            load_policy(temp.path()).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert!(!capture_enabled(temp.path(), MemoryStore::Facts));
    }

    #[cfg(unix)]
    #[test]
    fn policy_transaction_never_writes_to_a_replacement_boundary() {
        use std::os::unix::fs::symlink;

        let container = tempfile::tempdir().unwrap();
        let boundary = container.path().join("boundary");
        let moved = container.path().join("moved");
        let outside = container.path().join("outside");
        std::fs::create_dir(&boundary).unwrap();
        std::fs::create_dir(&outside).unwrap();

        update_policy(&boundary, |policy| {
            std::fs::rename(&boundary, &moved)?;
            symlink(&outside, &boundary)?;
            policy.set_capture(Some(MemoryStore::Facts), false);
            Ok(())
        })
        .unwrap();

        assert!(outside.read_dir().unwrap().next().is_none());
        assert!(!load_policy(&moved)
            .unwrap()
            .capture_enabled(MemoryStore::Facts));
        assert!(!moved.join(".umadev/memory/.policy.lock").exists());
    }
}
