//! Transaction markers for privacy-sensitive memory lifecycle operations.
//!
//! This module deliberately owns only the operation boundary and metadata. The
//! agent layer knows which files belong to each logical store and moves them
//! into the transaction's payload directory. A tombstone is published only
//! after every payload move succeeds; the deletion audit never stores source
//! paths or memory content.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fs2::FileExt as _;
use serde::{Deserialize, Serialize};

use crate::memory::MemoryStore;

const LOCK_DIR: &str = ".lifecycle.lock";
const LOCK_OWNER: &str = "owner";
const LOCK_LEASE: &str = "lease";
const LOCK_GUARD: &str = ".lifecycle.lock.guard";
const LOCK_ATTEMPTS: usize = 500;
const LOCK_WAIT: Duration = Duration::from_millis(2);
const LOCK_STALE_AFTER_MS: u64 = 5 * 60 * 1_000;
const LOCK_FUTURE_SKEW_MS: u64 = 60 * 1_000;
const MAX_RECORD_BYTES: u64 = 64 * 1024;
const MAX_ACTION_NODES: usize = 40_000;
const MAX_ACTION_DEPTH: usize = 16;
const RECORD_VERSION: u32 = 1;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Explicit ownership boundary recorded for a lifecycle operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleScope {
    /// One canonical project root.
    Project,
    /// The current user's canonical home directory.
    Global,
}

/// Why active memory was moved out of a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LifecycleOperation {
    /// An explicit user forget request.
    Forget,
    /// Enforcement of an explicitly configured age policy.
    Retention,
}

/// Follow-up action applied to one committed, recoverable tombstone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TombstoneAction {
    /// Return the payload to its original active namespace.
    Restore,
    /// Unlink the payload from the filesystem namespace. This is not a claim
    /// that storage media was physically erased.
    LogicalPurge,
}

/// Commit state of one deletion-audit record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuditState {
    /// Payload is staged but the tombstone directory has not been published.
    Prepared,
    /// The tombstone directory was atomically published.
    Committed,
}

/// Content-free marker for memory that remains recoverable under `payload/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TombstoneRecord {
    /// On-disk record schema.
    pub version: u32,
    /// Opaque operation identifier; contains no project or memory content.
    pub id: String,
    /// Explicit boundary selected by the caller.
    pub scope: LifecycleScope,
    /// Logical stores represented by the payload.
    pub stores: Vec<String>,
    /// Operation that created the marker.
    pub operation: LifecycleOperation,
    /// UNIX epoch timestamp in milliseconds.
    pub created_at_ms: u64,
    /// Number of regular files moved out of active storage.
    pub files: usize,
    /// Aggregate byte count reported by the preflight.
    pub bytes: u64,
    /// Whether content was physically destroyed. Soft deletion always writes
    /// `false`; this explicit field prevents audit consumers from guessing.
    pub physically_deleted: bool,
}

/// Content-free audit event for a lifecycle transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionAuditRecord {
    /// On-disk record schema.
    pub version: u32,
    /// Opaque event identifier.
    pub id: String,
    /// Matching tombstone identifier.
    pub tombstone_id: String,
    /// Explicit boundary selected by the caller.
    pub scope: LifecycleScope,
    /// Logical store identifiers only; no source paths are recorded.
    pub stores: Vec<String>,
    /// Operation that created the event.
    pub operation: LifecycleOperation,
    /// Transaction state, conservatively `prepared` until publication succeeds.
    pub state: AuditState,
    /// UNIX epoch timestamp in milliseconds.
    pub created_at_ms: u64,
    /// Number of regular files affected.
    pub files: usize,
    /// Aggregate byte count; never any content or content-derived preview.
    pub bytes: u64,
    /// Whether content was physically destroyed.
    pub physically_deleted: bool,
}

/// Content-free disposition and audit record for a tombstone follow-up.
///
/// The same schema is written inside the tombstone as its terminal
/// disposition and under `audit/lifecycle-actions/` as the public audit. It
/// intentionally contains neither payload paths nor memory-derived values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TombstoneActionRecord {
    /// On-disk record schema.
    pub version: u32,
    /// Opaque action event identifier.
    pub id: String,
    /// Opaque identifier of the affected tombstone.
    pub tombstone_id: String,
    /// Explicit ownership boundary selected by the caller.
    pub scope: LifecycleScope,
    /// Logical store identifiers copied from the tombstone marker.
    pub stores: Vec<String>,
    /// Follow-up action applied to the payload.
    pub action: TombstoneAction,
    /// Durable transaction state.
    pub state: AuditState,
    /// UNIX epoch timestamp in milliseconds.
    pub created_at_ms: u64,
    /// Number of regular payload files affected.
    pub files: usize,
    /// Aggregate payload byte count.
    pub bytes: u64,
    /// `true` only for a committed logical purge. Prepared attempts remain
    /// `false`; this describes namespace unlinking, not media sanitisation.
    pub logically_unlinked: bool,
    /// Always `false`: portable filesystem unlink cannot prove physical media
    /// erasure, including on copy-on-write filesystems and SSDs.
    pub physically_deleted: bool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn operation_id() -> String {
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!("mlc-{nanos:x}-{:x}-{sequence:x}", std::process::id())
}

fn ensure_memory_dir(root: &crate::fs::RootedDir) -> std::io::Result<PathBuf> {
    root.ensure_dir(Path::new(".umadev"), false)?;
    let memory = PathBuf::from(".umadev/memory");
    root.ensure_dir(&memory, true)?;
    Ok(memory)
}

fn parse_lock_owner(bytes: &[u8]) -> Option<(u64, &str)> {
    let text = std::str::from_utf8(bytes).ok()?;
    let (stamp, nonce) = text.split_once('\n')?;
    let stamp = stamp.parse().ok()?;
    (!nonce.is_empty() && !nonce.contains('\n')).then_some((stamp, nonce))
}

fn lock_owner_matches(root: &crate::fs::RootedDir, path: &Path, nonce: &str) -> bool {
    root.read_bounded(path, 4_096)
        .ok()
        .and_then(|bytes| parse_lock_owner(&bytes).map(|(_, seen)| seen == nonce))
        .unwrap_or(false)
}

#[derive(Debug)]
struct LifecycleLock {
    root: Arc<crate::fs::RootedDir>,
    lock: PathBuf,
    nonce: String,
    lease: Option<std::fs::File>,
    namespace_guard: std::fs::File,
}

impl Drop for LifecycleLock {
    fn drop(&mut self) {
        let owner = self.lock.join(LOCK_OWNER);
        let Ok(bytes) = self.root.read_bounded(&owner, 4_096) else {
            return;
        };
        if parse_lock_owner(&bytes).is_none_or(|(_, nonce)| nonce != self.nonce) {
            return;
        }
        let _ = self.root.remove_regular_file(&owner);
        drop(self.lease.take());
        let _ = self.root.remove_regular_file(&self.lock.join(LOCK_LEASE));
        let _ = self.root.remove_empty_dir(&self.lock);
        let _ = fs2::FileExt::unlock(&self.namespace_guard);
    }
}

fn lock_modified_at_ms(root: &crate::fs::RootedDir, lock: &Path) -> Option<u64> {
    root.directory_modified(lock)
        .ok()
        .flatten()
        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn lock_age_ms(root: &crate::fs::RootedDir, lock: &Path, now: u64) -> Option<u64> {
    let future_limit = now.saturating_add(LOCK_FUTURE_SKEW_MS);
    let created_at = root
        .read_bounded(&lock.join(LOCK_OWNER), 4_096)
        .ok()
        .and_then(|bytes| parse_lock_owner(&bytes).map(|(stamp, _)| stamp))
        .filter(|stamp| *stamp <= future_limit)
        .or_else(|| lock_modified_at_ms(root, lock))?;
    Some(now.saturating_sub(created_at))
}

fn try_acquire_namespace_guard(
    root: &crate::fs::RootedDir,
    memory: &Path,
) -> std::io::Result<Option<std::fs::File>> {
    let file = root.open_private_lock(&memory.join(LOCK_GUARD), false)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if crate::fs::lock_error_is_contention(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn reclaim_stale_lock(
    root: &crate::fs::RootedDir,
    lock: &Path,
    _namespace_guard: &std::fs::File,
    stale_after_ms: u64,
) {
    if !root.is_real_dir(lock).unwrap_or(false) {
        return;
    }
    if lock_age_ms(root, lock, now_ms()).is_none_or(|age| age <= stale_after_ms) {
        return;
    }
    let lease_path = lock.join(LOCK_LEASE);
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
    let stale = parent.join(format!(".lifecycle.lock.stale.{}", operation_id()));
    drop(lease);
    if root.rename(lock, &stale).is_ok() {
        let _ = root.remove_regular_file(&stale.join(LOCK_OWNER));
        let _ = root.remove_regular_file(&stale.join(LOCK_LEASE));
        let _ = root.remove_empty_dir(&stale);
    }
}

fn acquire_lock(root: Arc<crate::fs::RootedDir>) -> std::io::Result<LifecycleLock> {
    let memory = ensure_memory_dir(&root)?;
    let lock = memory.join(LOCK_DIR);
    for _ in 0..LOCK_ATTEMPTS {
        let Some(namespace_guard) = try_acquire_namespace_guard(&root, &memory)? else {
            std::thread::sleep(LOCK_WAIT);
            continue;
        };
        match root.create_dir(&lock, false) {
            Ok(()) => {
                let nonce = operation_id();
                let owner = format!("{}\n{nonce}", now_ms());
                if let Err(error) =
                    root.atomic_write(&lock.join(LOCK_OWNER), owner.as_bytes(), false)
                {
                    let _ = root.remove_empty_dir(&lock);
                    return Err(error);
                }
                let lease = match root
                    .open_private_lock(&lock.join(LOCK_LEASE), false)
                    .and_then(|file| {
                        file.try_lock_exclusive()?;
                        Ok(file)
                    }) {
                    Ok(file) => file,
                    Err(error) => {
                        if lock_owner_matches(&root, &lock.join(LOCK_OWNER), &nonce) {
                            let _ = root.remove_regular_file(&lock.join(LOCK_OWNER));
                            let _ = root.remove_regular_file(&lock.join(LOCK_LEASE));
                            let _ = root.remove_empty_dir(&lock);
                        }
                        return Err(error);
                    }
                };
                if !lock_owner_matches(&root, &lock.join(LOCK_OWNER), &nonce) {
                    drop(lease);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WouldBlock,
                        "memory lifecycle lock namespace changed during lease acquisition",
                    ));
                }
                return Ok(LifecycleLock {
                    root,
                    lock,
                    nonce,
                    lease: Some(lease),
                    namespace_guard,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                reclaim_stale_lock(&root, &lock, &namespace_guard, LOCK_STALE_AFTER_MS);
                drop(namespace_guard);
                std::thread::sleep(LOCK_WAIT);
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        "memory lifecycle is busy in another UmaDev process",
    ))
}

fn validate_stores(stores: &[MemoryStore]) -> std::io::Result<Vec<String>> {
    let mut ids: Vec<String> = stores.iter().map(|store| store.id().to_string()).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "memory lifecycle operation requires at least one store",
        ));
    }
    Ok(ids)
}

/// Marker-last lifecycle transaction. Dropping an uncommitted transaction
/// never removes its payload; callers must either roll moved files back or
/// leave the `.pending-*` directory for manual recovery.
#[derive(Debug)]
pub struct LifecycleTransaction {
    _lock: LifecycleLock,
    root: Arc<crate::fs::RootedDir>,
    id: String,
    scope: LifecycleScope,
    stores: Vec<String>,
    operation: LifecycleOperation,
    created_at_ms: u64,
    pending_relative: PathBuf,
    final_relative: PathBuf,
    payload_relative: PathBuf,
    audit_relative: PathBuf,
    payload_dir: PathBuf,
    committed: bool,
}

/// Cross-process-serialized follow-up transaction for one tombstone.
///
/// The agent layer owns payload movement/unlinking because it owns the
/// filesystem classifiers. This state-layer guard validates the tombstone,
/// keeps the lifecycle lock for the entire action, and publishes only
/// content-free transaction metadata.
#[derive(Debug)]
pub struct TombstoneActionTransaction {
    _lock: LifecycleLock,
    root: Arc<crate::fs::RootedDir>,
    tombstone: TombstoneRecord,
    action: TombstoneAction,
    action_id: String,
    created_at_ms: u64,
    payload_relative: PathBuf,
    disposition_relative: PathBuf,
    audit_relative: PathBuf,
    payload_dir: PathBuf,
    #[cfg_attr(not(test), allow(dead_code))]
    audit_path: PathBuf,
    prepared: bool,
    committed: bool,
}

impl LifecycleTransaction {
    /// Opaque identifier shared by the tombstone and deletion audit.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Root under which callers preserve files using boundary-relative paths.
    #[must_use]
    pub fn payload_dir(&self) -> &Path {
        &self.payload_dir
    }

    /// Duplicate the boundary capability captured when this transaction
    /// started. Agent-layer payload transfers must use this handle together
    /// with [`Self::payload_relative`] instead of resolving [`Self::payload_dir`]
    /// through the ambient filesystem namespace.
    pub fn rooted_dir(&self) -> std::io::Result<crate::fs::RootedDir> {
        self.root.try_clone()
    }

    /// Capability-relative payload directory owned by this transaction.
    #[must_use]
    pub fn payload_relative(&self) -> &Path {
        &self.payload_relative
    }

    /// Atomically publishes the tombstone after the caller staged all payload.
    ///
    /// The audit is written as `prepared` before publication and upgraded to
    /// `committed` afterward. If that final best-effort upgrade fails, the
    /// conservative prepared audit remains rather than losing the event.
    pub fn commit(&mut self, files: usize, bytes: u64) -> std::io::Result<TombstoneRecord> {
        if self.committed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "memory lifecycle transaction is already committed",
            ));
        }
        let tombstone = TombstoneRecord {
            version: RECORD_VERSION,
            id: self.id.clone(),
            scope: self.scope,
            stores: self.stores.clone(),
            operation: self.operation,
            created_at_ms: self.created_at_ms,
            files,
            bytes,
            physically_deleted: false,
        };
        let mut audit = DeletionAuditRecord {
            version: RECORD_VERSION,
            id: self.id.clone(),
            tombstone_id: self.id.clone(),
            scope: self.scope,
            stores: self.stores.clone(),
            operation: self.operation,
            state: AuditState::Prepared,
            created_at_ms: self.created_at_ms,
            files,
            bytes,
            physically_deleted: false,
        };
        let tombstone_bytes = serde_json::to_vec_pretty(&tombstone).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        let audit_bytes = serde_json::to_vec_pretty(&audit).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        self.root.atomic_write(
            &self.pending_relative.join("tombstone.json"),
            &tombstone_bytes,
            false,
        )?;
        self.root
            .atomic_write(&self.audit_relative, &audit_bytes, false)?;
        self.root
            .rename(&self.pending_relative, &self.final_relative)?;
        self.committed = true;

        audit.state = AuditState::Committed;
        if let Ok(bytes) = serde_json::to_vec_pretty(&audit) {
            let _ = self.root.atomic_write(&self.audit_relative, &bytes, false);
        }
        Ok(tombstone)
    }

    /// Removes metadata for a transaction whose payload has already been fully
    /// rolled back by the caller. Non-empty directories are intentionally left
    /// untouched so this method can never destroy recoverable memory.
    pub fn abort(&mut self) -> std::io::Result<()> {
        if self.committed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a committed lifecycle transaction cannot be aborted",
            ));
        }
        let _ = self
            .root
            .remove_regular_file(&self.pending_relative.join("tombstone.json"))?;
        let _ = self.root.remove_regular_file(&self.audit_relative)?;
        let _ = self.root.remove_empty_directory_tree(
            &self.payload_relative,
            MAX_ACTION_DEPTH,
            MAX_ACTION_NODES,
        )?;
        let _ = self.root.remove_empty_dir(&self.pending_relative)?;
        Ok(())
    }
}

impl TombstoneActionTransaction {
    /// Validated marker for the tombstone being acted upon.
    #[must_use]
    pub fn tombstone(&self) -> &TombstoneRecord {
        &self.tombstone
    }

    /// Validated real payload directory. Callers must retain this transaction
    /// while inspecting or changing files under it.
    #[must_use]
    pub fn payload_dir(&self) -> &Path {
        &self.payload_dir
    }

    /// Duplicate the boundary capability retained for the complete action.
    pub fn rooted_dir(&self) -> std::io::Result<crate::fs::RootedDir> {
        self.root.try_clone()
    }

    /// Capability-relative location of this tombstone's payload tree.
    #[must_use]
    pub fn payload_relative(&self) -> &Path {
        &self.payload_relative
    }

    fn record(&self, state: AuditState, files: usize, bytes: u64) -> TombstoneActionRecord {
        TombstoneActionRecord {
            version: RECORD_VERSION,
            id: self.action_id.clone(),
            tombstone_id: self.tombstone.id.clone(),
            scope: self.tombstone.scope,
            stores: self.tombstone.stores.clone(),
            action: self.action,
            state,
            created_at_ms: self.created_at_ms,
            files,
            bytes,
            logically_unlinked: state == AuditState::Committed
                && self.action == TombstoneAction::LogicalPurge,
            physically_deleted: false,
        }
    }

    /// Durably records a prepared action before any active namespace changes
    /// or irreversible unlinks occur.
    ///
    /// Counts must still match the committed tombstone. A mismatch means the
    /// recovery payload changed after publication and must not be trusted.
    pub fn prepare(&mut self, files: usize, bytes: u64) -> std::io::Result<()> {
        if self.prepared || self.committed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "tombstone action is already prepared",
            ));
        }
        if files != self.tombstone.files || bytes != self.tombstone.bytes {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "tombstone payload no longer matches its content-free marker",
            ));
        }
        let record = self.record(AuditState::Prepared, files, bytes);
        let bytes = serde_json::to_vec_pretty(&record).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        self.root
            .publish_new_private(&self.audit_relative, &bytes, false)?;
        self.prepared = true;
        Ok(())
    }

    /// Publishes the terminal disposition after the agent layer has completed
    /// and, for restore, can still roll back every payload move if this call
    /// fails.
    pub fn commit(&mut self) -> std::io::Result<TombstoneActionRecord> {
        if !self.prepared || self.committed {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "tombstone action must be prepared exactly once before commit",
            ));
        }
        self.root.validate_empty_directory_tree(
            &self.payload_relative,
            MAX_ACTION_DEPTH,
            MAX_ACTION_NODES,
        )?;
        let record = self.record(
            AuditState::Committed,
            self.tombstone.files,
            self.tombstone.bytes,
        );
        let bytes = serde_json::to_vec_pretty(&record).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
        })?;
        // Creation, rather than replacement, makes a concurrently published
        // disposition a hard conflict even outside the lifecycle lock.
        self.root
            .publish_new_private(&self.disposition_relative, &bytes, false)?;
        self.committed = true;
        // The committed disposition is authoritative. Preserve a conservative
        // prepared audit if this best-effort state upgrade cannot be written.
        let _ = self.root.atomic_write(&self.audit_relative, &bytes, false);
        Ok(record)
    }
}

/// Starts a cross-process serialized soft-deletion transaction.
pub fn begin_transaction(
    boundary: &Path,
    scope: LifecycleScope,
    stores: &[MemoryStore],
    operation: LifecycleOperation,
) -> std::io::Result<LifecycleTransaction> {
    let boundary_path = std::fs::canonicalize(boundary)?;
    let root = crate::fs::RootedDir::open(&boundary_path)?;
    if !root.matches_path(&boundary_path)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory lifecycle boundary changed while the transaction started",
        ));
    }
    begin_transaction_rooted_inner(root, Some(boundary_path), scope, stores, operation)
}

/// Starts a lifecycle transaction from a directory capability the caller
/// opened before inventory/preflight. This closes the ambient-root replacement
/// window between discovery and the first lifecycle mutation.
pub fn begin_transaction_rooted(
    root: crate::fs::RootedDir,
    scope: LifecycleScope,
    stores: &[MemoryStore],
    operation: LifecycleOperation,
) -> std::io::Result<LifecycleTransaction> {
    begin_transaction_rooted_inner(root, None, scope, stores, operation)
}

fn begin_transaction_rooted_inner(
    root: crate::fs::RootedDir,
    display_root: Option<PathBuf>,
    scope: LifecycleScope,
    stores: &[MemoryStore],
    operation: LifecycleOperation,
) -> std::io::Result<LifecycleTransaction> {
    let stores = validate_stores(stores)?;
    let root = Arc::new(root);
    let lock = acquire_lock(Arc::clone(&root))?;
    let memory = ensure_memory_dir(&root)?;
    let tombstones = memory.join("tombstones");
    root.ensure_dir(&tombstones, false)?;
    let audit = memory.join("audit");
    root.ensure_dir(&audit, false)?;
    let deletions = audit.join("deletions");
    root.ensure_dir(&deletions, false)?;
    let id = operation_id();
    let pending_name = format!(".pending-{id}");
    let pending_relative = tombstones.join(&pending_name);
    root.create_dir(&pending_relative, false)?;
    let payload_relative = pending_relative.join("payload");
    root.ensure_dir(&payload_relative, false)?;
    // Kept only for backwards-compatible diagnostics/tests. Capability-aware
    // callers use `rooted_dir()` plus `payload_relative()`.
    let payload_dir = display_root.map_or_else(
        || PathBuf::from(&payload_relative),
        |boundary| boundary.join(&payload_relative),
    );
    Ok(LifecycleTransaction {
        _lock: lock,
        root,
        id: id.clone(),
        scope,
        stores,
        operation,
        created_at_ms: now_ms(),
        pending_relative,
        final_relative: tombstones.join(&id),
        payload_relative,
        audit_relative: deletions.join(format!("{id}.json")),
        payload_dir,
        committed: false,
    })
}

fn validate_operation_id(id: &str) -> std::io::Result<()> {
    if id.len() < 8
        || id.len() > 128
        || !id.starts_with("mlc-")
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid opaque memory lifecycle identifier",
        ));
    }
    Ok(())
}

fn validate_store_ids(stores: &[String]) -> std::io::Result<()> {
    let mut previous: Option<&str> = None;
    for id in stores {
        let Some(store) = MemoryStore::parse(id) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "lifecycle record contains an unknown logical store",
            ));
        };
        if store.id() != id || previous.is_some_and(|seen| seen >= id.as_str()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "lifecycle store set is non-canonical, duplicated, or unsorted",
            ));
        }
        previous = Some(id);
    }
    if stores.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "lifecycle record contains an empty logical store set",
        ));
    }
    Ok(())
}

/// Starts a serialized restore or logical-purge transaction for a committed
/// tombstone.
///
/// The marker, scope, payload directory, and terminal-disposition absence are
/// all validated without following links. The returned guard retains the
/// lifecycle lock until it is dropped.
pub fn begin_tombstone_action(
    boundary: &Path,
    scope: LifecycleScope,
    tombstone_id: &str,
    action: TombstoneAction,
) -> std::io::Result<TombstoneActionTransaction> {
    let boundary_path = std::fs::canonicalize(boundary)?;
    let root = crate::fs::RootedDir::open(&boundary_path)?;
    if !root.matches_path(&boundary_path)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "memory lifecycle boundary changed while the action started",
        ));
    }
    begin_tombstone_action_rooted_inner(root, Some(boundary_path), scope, tombstone_id, action)
}

/// Starts a tombstone action using a boundary capability captured by the
/// caller before any action-specific preflight.
pub fn begin_tombstone_action_rooted(
    root: crate::fs::RootedDir,
    scope: LifecycleScope,
    tombstone_id: &str,
    action: TombstoneAction,
) -> std::io::Result<TombstoneActionTransaction> {
    begin_tombstone_action_rooted_inner(root, None, scope, tombstone_id, action)
}

fn begin_tombstone_action_rooted_inner(
    root: crate::fs::RootedDir,
    display_root: Option<PathBuf>,
    scope: LifecycleScope,
    tombstone_id: &str,
    action: TombstoneAction,
) -> std::io::Result<TombstoneActionTransaction> {
    validate_operation_id(tombstone_id)?;
    let root = Arc::new(root);
    let lock = acquire_lock(Arc::clone(&root))?;
    let memory = ensure_memory_dir(&root)?;
    let tombstones = memory.join("tombstones");
    root.ensure_dir(&tombstones, false)?;
    let tombstone_dir = tombstones.join(tombstone_id);
    if !root.is_real_dir(&tombstone_dir)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "committed memory tombstone is unavailable or unsafe",
        ));
    }
    let tombstone = read_tombstone_bytes(
        &root.read_bounded(&tombstone_dir.join("tombstone.json"), MAX_RECORD_BYTES)?,
    )?;
    if tombstone.id != tombstone_id || tombstone.scope != scope {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "tombstone identity or scope does not match the requested boundary",
        ));
    }
    let payload_relative = tombstone_dir.join("payload");
    if !root.is_real_dir(&payload_relative)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "tombstone payload is missing, linked, or special",
        ));
    }
    let disposition_relative = tombstone_dir.join("disposition.json");
    match root.regular_file_exists(&disposition_relative) {
        Ok(true) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "tombstone already has a terminal disposition",
            ));
        }
        Ok(false) => {}
        Err(error) => return Err(error),
    }
    let audit = memory.join("audit");
    root.ensure_dir(&audit, false)?;
    let actions = audit.join("lifecycle-actions");
    root.ensure_dir(&actions, false)?;
    let action_id = operation_id();
    let audit_relative = actions.join(format!("{action_id}.json"));
    let audit_path = display_root.as_ref().map_or_else(
        || PathBuf::from(&audit_relative),
        |boundary| boundary.join(&audit_relative),
    );
    let payload_dir = display_root.map_or_else(
        || PathBuf::from(&payload_relative),
        |boundary| boundary.join(&payload_relative),
    );
    Ok(TombstoneActionTransaction {
        _lock: lock,
        root,
        tombstone,
        action,
        audit_path,
        audit_relative,
        action_id,
        created_at_ms: now_ms(),
        payload_dir,
        payload_relative,
        disposition_relative,
        prepared: false,
        committed: false,
    })
}

/// Reads one tombstone marker without following links or accepting oversized
/// lifecycle metadata.
pub fn read_tombstone(path: &Path) -> std::io::Result<TombstoneRecord> {
    let bytes = crate::fs::read_bounded(path, MAX_RECORD_BYTES)?;
    read_tombstone_bytes(&bytes)
}

fn read_tombstone_bytes(bytes: &[u8]) -> std::io::Result<TombstoneRecord> {
    let record: TombstoneRecord = serde_json::from_slice(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    if record.version != RECORD_VERSION || record.physically_deleted {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported or unsafe memory tombstone record",
        ));
    }
    validate_operation_id(&record.id)?;
    validate_store_ids(&record.stores)?;
    Ok(record)
}

/// Reads one content-free deletion audit without following links.
pub fn read_deletion_audit(path: &Path) -> std::io::Result<DeletionAuditRecord> {
    let bytes = crate::fs::read_bounded(path, MAX_RECORD_BYTES)?;
    let record: DeletionAuditRecord = serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    if record.version != RECORD_VERSION
        || record.physically_deleted
        || record.id != record.tombstone_id
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported or unsafe memory deletion audit record",
        ));
    }
    validate_operation_id(&record.id)?;
    validate_store_ids(&record.stores)?;
    Ok(record)
}

/// Reads one content-free tombstone action record without following links.
pub fn read_tombstone_action(path: &Path) -> std::io::Result<TombstoneActionRecord> {
    let bytes = crate::fs::read_bounded(path, MAX_RECORD_BYTES)?;
    let record: TombstoneActionRecord = serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let expected_unlinked =
        record.state == AuditState::Committed && record.action == TombstoneAction::LogicalPurge;
    if record.version != RECORD_VERSION
        || record.physically_deleted
        || record.logically_unlinked != expected_unlinked
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported or unsafe tombstone action record",
        ));
    }
    validate_operation_id(&record.id)?;
    validate_operation_id(&record.tombstone_id)?;
    validate_store_ids(&record.stores)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_is_published_last_and_audit_has_no_content_or_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut transaction = begin_transaction(
            temp.path(),
            LifecycleScope::Project,
            &[MemoryStore::Facts],
            LifecycleOperation::Forget,
        )
        .unwrap();
        let secret = "customer-secret-do-not-audit";
        crate::fs::atomic_write(
            &transaction.payload_dir().join("payload.bin"),
            secret.as_bytes(),
        )
        .unwrap();
        let id = transaction.id().to_string();
        let tombstone = transaction.commit(1, secret.len() as u64).unwrap();
        assert_eq!(tombstone.id, id);

        let tombstone_path = temp
            .path()
            .join(".umadev/memory/tombstones")
            .join(&id)
            .join("tombstone.json");
        assert_eq!(read_tombstone(&tombstone_path).unwrap(), tombstone);
        let audit_path = temp
            .path()
            .join(".umadev/memory/audit/deletions")
            .join(format!("{id}.json"));
        let audit = read_deletion_audit(&audit_path).unwrap();
        assert_eq!(audit.state, AuditState::Committed);
        let audit_text = std::fs::read_to_string(audit_path).unwrap();
        assert!(!audit_text.contains(secret));
        assert!(!audit_text.contains("payload.bin"));
        assert!(!audit.physically_deleted);
    }

    #[test]
    fn uncommitted_transaction_never_claims_a_tombstone() {
        let temp = tempfile::tempdir().unwrap();
        let transaction = begin_transaction(
            temp.path(),
            LifecycleScope::Project,
            &[MemoryStore::Facts],
            LifecycleOperation::Forget,
        )
        .unwrap();
        let id = transaction.id().to_string();
        drop(transaction);
        assert!(!temp
            .path()
            .join(".umadev/memory/tombstones")
            .join(id)
            .exists());
    }

    #[test]
    fn tombstone_action_is_prepared_then_published_without_paths_or_content() {
        let temp = tempfile::tempdir().unwrap();
        let secret = "private-memory-value";
        let mut deletion = begin_transaction(
            temp.path(),
            LifecycleScope::Project,
            &[MemoryStore::Facts],
            LifecycleOperation::Forget,
        )
        .unwrap();
        crate::fs::atomic_write(
            &deletion.payload_dir().join("opaque.bin"),
            secret.as_bytes(),
        )
        .unwrap();
        let id = deletion.id().to_string();
        deletion.commit(1, secret.len() as u64).unwrap();
        drop(deletion);

        let mut action = begin_tombstone_action(
            temp.path(),
            LifecycleScope::Project,
            &id,
            TombstoneAction::LogicalPurge,
        )
        .unwrap();
        assert_eq!(
            action.prepare(2, secret.len() as u64).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        action.prepare(1, secret.len() as u64).unwrap();
        let prepared = read_tombstone_action(&action.audit_path).unwrap();
        assert_eq!(prepared.state, AuditState::Prepared);
        assert!(!prepared.logically_unlinked);
        assert!(!prepared.physically_deleted);
        assert_eq!(
            action.commit().unwrap_err().kind(),
            std::io::ErrorKind::DirectoryNotEmpty
        );
        crate::fs::remove_regular_file(&action.payload_dir().join("opaque.bin")).unwrap();
        let disposition = action.commit().unwrap();
        assert_eq!(disposition.state, AuditState::Committed);
        assert!(disposition.logically_unlinked);
        assert!(!disposition.physically_deleted);

        let disposition_path = temp
            .path()
            .join(".umadev/memory/tombstones")
            .join(&id)
            .join("disposition.json");
        assert_eq!(
            read_tombstone_action(&disposition_path).unwrap(),
            disposition
        );
        let audit_path = temp
            .path()
            .join(".umadev/memory/audit/lifecycle-actions")
            .join(format!("{}.json", disposition.id));
        assert_eq!(read_tombstone_action(&audit_path).unwrap(), disposition);
        let public_audit = std::fs::read_to_string(audit_path).unwrap();
        assert!(!public_audit.contains(secret));
        assert!(!public_audit.contains("opaque.bin"));
        assert!(!public_audit.contains("payload"));

        drop(action);
        assert_eq!(
            begin_tombstone_action(
                temp.path(),
                LifecycleScope::Project,
                &id,
                TombstoneAction::Restore,
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::AlreadyExists
        );
    }

    #[test]
    fn tombstone_identity_tampering_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let mut deletion = begin_transaction(
            temp.path(),
            LifecycleScope::Project,
            &[MemoryStore::Facts],
            LifecycleOperation::Forget,
        )
        .unwrap();
        crate::fs::atomic_write(&deletion.payload_dir().join("opaque.bin"), b"x").unwrap();
        let id = deletion.id().to_string();
        deletion.commit(1, 1).unwrap();
        drop(deletion);

        let marker = temp
            .path()
            .join(".umadev/memory/tombstones")
            .join(&id)
            .join("tombstone.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&crate::fs::read_bounded(&marker, MAX_RECORD_BYTES).unwrap())
                .unwrap();
        value["id"] = serde_json::Value::String("mlc-tampered".to_string());
        crate::fs::atomic_write(&marker, &serde_json::to_vec(&value).unwrap()).unwrap();

        assert_eq!(
            begin_tombstone_action(
                temp.path(),
                LifecycleScope::Project,
                &id,
                TombstoneAction::Restore,
            )
            .unwrap_err()
            .kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn linked_audit_boundary_is_rejected_before_transaction_starts() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = crate::fs::RootedDir::open(temp.path()).unwrap();
        let memory = ensure_memory_dir(&root).unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), temp.path().join(memory).join("audit")).unwrap();
        let error = begin_transaction(
            temp.path(),
            LifecycleScope::Project,
            &[MemoryStore::Facts],
            LifecycleOperation::Forget,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(outside.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn ownerless_crash_lifecycle_lock_is_reclaimable() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::fs::RootedDir::open(temp.path()).unwrap();
        let memory = ensure_memory_dir(&root).unwrap();
        let lock = memory.join(LOCK_DIR);
        root.create_dir(&lock, false).unwrap();
        std::thread::sleep(Duration::from_millis(2));

        let namespace_guard = try_acquire_namespace_guard(&root, &memory)
            .unwrap()
            .unwrap();
        reclaim_stale_lock(&root, &lock, &namespace_guard, 0);
        drop(namespace_guard);
        assert!(!temp.path().join(lock).exists());
        let transaction = begin_transaction(
            temp.path(),
            LifecycleScope::Project,
            &[MemoryStore::Facts],
            LifecycleOperation::Forget,
        )
        .unwrap();
        drop(transaction);
    }

    #[test]
    fn released_stale_lifecycle_lease_is_reclaimed_with_parent_arbitration() {
        let temp = tempfile::tempdir().unwrap();
        let root = crate::fs::RootedDir::open(temp.path()).unwrap();
        let memory = ensure_memory_dir(&root).unwrap();
        let lock = memory.join(LOCK_DIR);
        root.create_dir(&lock, false).unwrap();
        root.atomic_write(
            &lock.join(LOCK_OWNER),
            format!("0\n{}", operation_id()).as_bytes(),
            false,
        )
        .unwrap();
        let lease = root
            .open_private_lock(&lock.join(LOCK_LEASE), false)
            .unwrap();
        lease.try_lock_exclusive().unwrap();
        drop(lease);

        let namespace_guard = try_acquire_namespace_guard(&root, &memory)
            .unwrap()
            .unwrap();
        reclaim_stale_lock(&root, &lock, &namespace_guard, 0);
        assert!(!temp.path().join(lock).exists());
    }

    #[test]
    fn active_lifecycle_lease_cannot_be_reclaimed_by_age() {
        let temp = tempfile::tempdir().unwrap();
        let root = Arc::new(crate::fs::RootedDir::open(temp.path()).unwrap());
        let guard = acquire_lock(root).unwrap();
        let owner = format!("0\n{}", guard.nonce);
        guard
            .root
            .atomic_write(&guard.lock.join(LOCK_OWNER), owner.as_bytes(), false)
            .unwrap();

        reclaim_stale_lock(&guard.root, &guard.lock, &guard.namespace_guard, 0);
        assert!(guard.root.is_real_dir(&guard.lock).unwrap());
        drop(guard);
        assert!(!temp.path().join(".umadev/memory/.lifecycle.lock").exists());
    }

    #[test]
    fn tampered_lifecycle_audit_identity_and_store_sets_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let valid = DeletionAuditRecord {
            version: RECORD_VERSION,
            id: "mlc-audit01".to_string(),
            tombstone_id: "mlc-audit01".to_string(),
            scope: LifecycleScope::Project,
            stores: vec![MemoryStore::Facts.id().to_string()],
            operation: LifecycleOperation::Forget,
            state: AuditState::Committed,
            created_at_ms: 1,
            files: 1,
            bytes: 1,
            physically_deleted: false,
        };
        let path = temp.path().join("audit.json");

        let mut mismatched = valid.clone();
        mismatched.tombstone_id = "mlc-other01".to_string();
        crate::fs::atomic_write(&path, &serde_json::to_vec(&mismatched).unwrap()).unwrap();
        assert_eq!(
            read_deletion_audit(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );

        let mut aliased = valid;
        aliased.stores = vec!["FACTS".to_string()];
        crate::fs::atomic_write(&path, &serde_json::to_vec(&aliased).unwrap()).unwrap();
        assert_eq!(
            read_deletion_audit(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_commit_never_writes_to_a_replacement_boundary() {
        use std::os::unix::fs::symlink;

        let container = tempfile::tempdir().unwrap();
        let boundary = container.path().join("boundary");
        let moved = container.path().join("moved");
        let outside = container.path().join("outside");
        std::fs::create_dir(&boundary).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let mut transaction = begin_transaction(
            &boundary,
            LifecycleScope::Project,
            &[MemoryStore::Facts],
            LifecycleOperation::Forget,
        )
        .unwrap();
        let id = transaction.id().to_string();

        std::fs::rename(&boundary, &moved).unwrap();
        symlink(&outside, &boundary).unwrap();
        transaction.commit(0, 0).unwrap();
        drop(transaction);

        assert!(outside.read_dir().unwrap().next().is_none());
        assert!(moved
            .join(".umadev/memory/tombstones")
            .join(&id)
            .join("tombstone.json")
            .is_file());
        assert!(!moved.join(".umadev/memory/.lifecycle.lock").exists());
    }
}
