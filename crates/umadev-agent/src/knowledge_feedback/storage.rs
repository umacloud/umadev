use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{
    ensure_raw_dir, existing_raw_dir, OutcomeIntent, SentMemoryReceipt, RECEIPTS_DIR,
    RECEIPT_VERSION,
};

pub(super) const MAX_RECEIPT_FILE_BYTES: u64 = 512 * 1024;
const MAX_RECEIPTS: usize = 4096;
const RECEIPT_PRUNE_TARGET: usize = 3584;
const MAX_RECEIPT_DIR_ENTRIES: usize = MAX_RECEIPTS * 4;

pub(super) fn ensure_receipts_dir(project_root: &Path) -> Option<PathBuf> {
    let raw = ensure_raw_dir(project_root)?;
    umadev_state::fs::ensure_real_child_dir(&raw, RECEIPTS_DIR).ok()
}

pub(super) fn existing_receipts_dir(project_root: &Path) -> Option<PathBuf> {
    let dir = existing_raw_dir(project_root)?.join(RECEIPTS_DIR);
    umadev_state::fs::real_dir(&dir).then_some(dir)
}

pub(super) fn receipt_path(dir: &Path, receipt_id: &str) -> PathBuf {
    dir.join(format!("{receipt_id}.receipt.json"))
}

pub(super) fn settled_receipt_path(dir: &Path, receipt_id: &str) -> PathBuf {
    dir.join(format!("{receipt_id}.settled-receipt.json"))
}

pub(super) fn intent_path(dir: &Path, receipt_id: &str) -> PathBuf {
    dir.join(format!("{receipt_id}.outcome.json"))
}

pub(super) fn settled_intent_path(dir: &Path, receipt_id: &str) -> PathBuf {
    dir.join(format!("{receipt_id}.settled-outcome.json"))
}

pub(super) fn read_managed_text(path: &Path) -> Option<String> {
    String::from_utf8(umadev_state::fs::read_bounded(path, MAX_RECEIPT_FILE_BYTES).ok()?).ok()
}

pub(super) fn read_receipt(dir: &Path, receipt_id: &str) -> Option<SentMemoryReceipt> {
    if !valid_receipt_id(receipt_id) {
        return None;
    }
    let rooted = umadev_state::fs::RootedDir::open_no_follow(dir).ok()?;
    [
        PathBuf::from(format!("{receipt_id}.receipt.json")),
        PathBuf::from(format!("{receipt_id}.settled-receipt.json")),
    ]
    .into_iter()
    .find_map(|path| {
        rooted
            .read_bounded(&path, MAX_RECEIPT_FILE_BYTES)
            .ok()
            .and_then(|body| String::from_utf8(body).ok())
            .and_then(|body| serde_json::from_str::<SentMemoryReceipt>(&body).ok())
            .filter(|receipt| {
                receipt.version == RECEIPT_VERSION && receipt.receipt_id == receipt_id
            })
    })
}

pub(super) fn valid_receipt_id(receipt_id: &str) -> bool {
    receipt_id.strip_prefix("kr1-").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublishResult {
    Created,
    AlreadyExists,
    Unavailable,
}

/// Publish bytes at `path` without ever replacing an existing writer. The temp
/// is fully written and synced before the atomic hard-link create-new step.
pub(super) fn publish_create_new(path: &Path, body: &[u8]) -> PublishResult {
    let Some(parent) = path.parent() else {
        return PublishResult::Unavailable;
    };
    if body.len() > MAX_RECEIPT_FILE_BYTES as usize || !umadev_state::fs::real_dir(parent) {
        return PublishResult::Unavailable;
    }
    match umadev_state::fs::publish_new_private(path, body) {
        Ok(()) => PublishResult::Created,
        Err(error)
            if error.kind() == std::io::ErrorKind::AlreadyExists
                || std::fs::symlink_metadata(path).is_ok() =>
        {
            PublishResult::AlreadyExists
        }
        Err(_) => PublishResult::Unavailable,
    }
}

fn publish_matches<T>(path: &Path, value: &T) -> bool
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq,
{
    let Some(body) = serde_json::to_vec(value).ok() else {
        return false;
    };
    match publish_create_new(path, &body) {
        PublishResult::Created => true,
        PublishResult::AlreadyExists => read_managed_text(path)
            .and_then(|existing| serde_json::from_str::<T>(&existing).ok())
            .is_some_and(|existing| existing == *value),
        PublishResult::Unavailable => false,
    }
}

#[cfg(test)]
pub(super) fn receipt_artifact_name(name: &str) -> bool {
    name.strip_suffix(".receipt.json")
        .or_else(|| name.strip_suffix(".settled-receipt.json"))
        .is_some_and(valid_receipt_id)
}

struct ReceiptInventory {
    root: umadev_state::fs::RootedDir,
    total: usize,
    active: usize,
    settled: Vec<(std::time::SystemTime, String)>,
}

fn scan_receipts_to(dir: &Path, max_entries: usize) -> Option<ReceiptInventory> {
    let root = umadev_state::fs::RootedDir::open_no_follow(dir).ok()?;
    let entries = root.list_entries(Path::new(""), max_entries).ok()?;
    let mut inventory = ReceiptInventory {
        root,
        total: 0,
        active: 0,
        settled: Vec::new(),
    };
    for entry in entries {
        if entry.kind != umadev_state::fs::RootedEntryKind::RegularFile {
            continue;
        }
        let Some(name) = entry.name.to_str() else {
            continue;
        };
        if let Some(receipt_id) = name
            .strip_suffix(".settled-receipt.json")
            .filter(|receipt_id| valid_receipt_id(receipt_id))
        {
            inventory.total = inventory.total.saturating_add(1);
            inventory.settled.push((
                entry.modified.unwrap_or(std::time::UNIX_EPOCH),
                receipt_id.to_string(),
            ));
        } else if name
            .strip_suffix(".receipt.json")
            .is_some_and(valid_receipt_id)
        {
            inventory.total = inventory.total.saturating_add(1);
            inventory.active = inventory.active.saturating_add(1);
        }
    }
    Some(inventory)
}

pub(super) fn prune_settled_receipts_to(dir: &Path, maximum: usize, target: usize) {
    if maximum == 0 || target >= maximum {
        return;
    }
    let Some(mut inventory) = scan_receipts_to(dir, MAX_RECEIPT_DIR_ENTRIES) else {
        return;
    };
    if inventory.total < maximum {
        return;
    }

    let keep_capacity = target.saturating_sub(inventory.active);
    inventory.settled.sort_by(|left, right| right.cmp(left));
    for (_, receipt_id) in inventory.settled.into_iter().skip(keep_capacity) {
        if inventory
            .root
            .remove_regular_file(&PathBuf::from(format!("{receipt_id}.settled-receipt.json")))
            .ok()
            == Some(true)
        {
            let _ = inventory
                .root
                .remove_regular_file(&PathBuf::from(format!("{receipt_id}.settled-outcome.json")));
        }
    }
}

fn prune_settled_receipts(dir: &Path) {
    prune_settled_receipts_to(dir, MAX_RECEIPTS, RECEIPT_PRUNE_TARGET);
}

pub(super) fn receipt_capacity_available(dir: &Path) -> bool {
    prune_settled_receipts(dir);
    scan_receipts_to(dir, MAX_RECEIPT_DIR_ENTRIES)
        .is_some_and(|inventory| inventory.total < MAX_RECEIPTS)
}

pub(super) fn finalize_local_settlement(
    dir: &Path,
    receipt: &SentMemoryReceipt,
    intent: &OutcomeIntent,
) -> bool {
    if !publish_matches(&settled_intent_path(dir, &intent.receipt_id), intent) {
        return false;
    }
    if !publish_matches(&settled_receipt_path(dir, &receipt.receipt_id), receipt) {
        return false;
    }
    let _ = umadev_state::fs::remove_regular_file(&receipt_path(dir, &receipt.receipt_id));
    let _ = umadev_state::fs::remove_regular_file(&intent_path(dir, &intent.receipt_id));
    prune_settled_receipts(dir);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_directory_scan_fails_closed_at_its_entry_budget() {
        let directory = tempfile::tempdir().unwrap();
        for index in 0..4 {
            std::fs::write(directory.path().join(format!("unknown-{index}")), b"x").unwrap();
        }
        assert!(
            scan_receipts_to(directory.path(), 3).is_none(),
            "unknown files must still consume the directory traversal budget"
        );
    }

    #[test]
    fn receipt_lookup_rejects_non_opaque_ids_before_path_construction() {
        let directory = tempfile::tempdir().unwrap();
        assert!(read_receipt(directory.path(), "../../outside").is_none());
    }
}
