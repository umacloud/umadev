//! Evidence freshness — the expiry date on a completion claim.
//!
//! **A completion claim is only as fresh as the evidence behind it. Evidence produced
//! before the last change to the code it describes is not evidence.**
//!
//! Every proof UmaDev produces — a green verify run, a runtime proof that booted the
//! app and probed its routes — is a statement about a SPECIFIC state of the source
//! tree. The moment the source changes, that statement stops being about the code we
//! are shipping and becomes a fact about a tree that no longer exists. Left unchecked,
//! that is how "stale green" happens: a proof captured three edits ago is read as
//! today's proof, a broken build ships behind a passing artifact, and the audit trail
//! records a verification that never covered the delivered code.
//!
//! The fix is mechanical, not procedural. Every proof is STAMPED with a fingerprint of
//! the source tree it describes. Re-fingerprint the tree at the moment the proof is
//! CONSUMED; if the two differ, the source moved after the proof was taken and the
//! proof is **stale** — it does not satisfy an evidence contract (the floor re-runs
//! the check for real) and it may not be assembled into a delivery proof-pack (an
//! honest note says so instead).
//!
//! ## It never claims a freshness it cannot establish
//! The fingerprint walks the source tree itself — bounded, but bounded far above any
//! real project ([`MAX_FINGERPRINT_FILES`]). If a tree is SO large that even that bound
//! truncates the walk, the fingerprint **refuses to answer** ([`workspace_fingerprint`]
//! returns `None`) rather than hash a prefix of the tree: a prefix hash would leave
//! every file past the cut-off invisible, so an edit out there would not move the
//! fingerprint and a proof taken BEFORE that edit would be certified as current — the
//! exact "stale green" this module exists to prevent, fired on precisely the large
//! commercial repos where it matters most.
//!
//! ## Cost
//! Metadata only: the walk `lstat`s each candidate (path + size + mtime) and reads no
//! file contents, so a stamp costs a directory walk, not a build. It deliberately does
//! NOT reuse [`crate::acceptance::source_files`], whose 600-file cap and per-file
//! content read serve a different question ("is there substantive source here?"). This
//! one asks "did ANY byte of source move?", and for that a stub file counts too.
//!
//! ## Fail-open (the repo's hard rule)
//! An UNSTAMPED proof (an artifact written by an older build, or one produced where the
//! tree could not be fingerprinted) is **never** stale: an absent fingerprint means "we
//! do not know", and we never block on our own inability to check. A proof that IS
//! stamped, read against a tree we can no longer fingerprint, is not trusted as fresh —
//! but the cost of that is a re-verification, never a block.

use std::path::{Path, PathBuf};

use crate::acceptance::{MAX_SOURCE_DEPTH, SKIP_DIRS, SRC_EXT};
use crate::fswalk::{classify_no_follow, EntryKind};

/// Ceiling on the fingerprint walk. Set far above any plausible hand-written project
/// (a large commercial monorepo's *source* — vendor/build dirs are skipped — lands in
/// the low thousands), because truncating here does not degrade the answer, it VOIDS it:
/// past this the fingerprint refuses to answer at all (see the module docs).
pub const MAX_FINGERPRINT_FILES: usize = 20_000;

/// FNV-1a over the tree digest. Dependency-free, stable across processes and machines
/// (unlike `DefaultHasher`, whose output is not guaranteed stable) — which matters
/// because a fingerprint is PERSISTED in a proof artifact and compared on a later run.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// The bounded, no-follow source walk behind the fingerprint. `false` ⇒ the walk hit
/// [`MAX_FINGERPRINT_FILES`] and the collected set is a PREFIX of the tree, not the tree.
///
/// Same shape as the acceptance floor's walk (same extensions, same skipped
/// build/vendor/VCS/`output` dirs, same depth cap, `symlink_metadata` so a link can
/// never escape the workspace) minus the content read — a stub file still counts here,
/// because a stub file can still CHANGE.
fn collect(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> bool {
    if out.len() >= MAX_FINGERPRINT_FILES {
        return false;
    }
    if depth > MAX_SOURCE_DEPTH {
        return true; // a depth cut is part of the definition, not a truncation
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return true; // an unreadable dir contributes nothing (fail-open)
    };
    let mut complete = true;
    for e in rd.flatten() {
        let p = e.path();
        match classify_no_follow(&p) {
            EntryKind::Dir => {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                    continue;
                }
                if !collect(&p, out, depth + 1) {
                    complete = false;
                }
            }
            EntryKind::File => {
                if out.len() >= MAX_FINGERPRINT_FILES {
                    return false;
                }
                if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                    if SRC_EXT.contains(&ext) {
                        out.push(p);
                    }
                }
            }
            EntryKind::Skip => {}
        }
    }
    complete
}

fn is_qc_input_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        name.as_str(),
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "npm-shrinkwrap.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
            | "deno.json"
            | "deno.jsonc"
            | "pyproject.toml"
            | "poetry.lock"
            | "pipfile"
            | "pipfile.lock"
            | "uv.lock"
            | "pytest.ini"
            | "tox.ini"
            | "setup.cfg"
            | "go.mod"
            | "go.sum"
            | "pom.xml"
            | "makefile"
            | "justfile"
            | "dockerfile"
            | "compose.yaml"
            | "compose.yml"
            | "docker-compose.yaml"
            | "docker-compose.yml"
            | ".env"
            | ".npmrc"
            | ".yarnrc"
            | ".yarnrc.yml"
            | "rust-toolchain"
            | ".nvmrc"
            | ".node-version"
            | ".python-version"
            | ".tool-versions"
    ) || name.starts_with("requirements")
        // Data / config a build or test reads: fixtures, app config, SQL seeds
        // and migrations. Metadata only, so secrets are never read.
        || matches!(
            path.extension().and_then(|value| value.to_str()),
            Some("json" | "jsonc" | "yaml" | "yml" | "toml" | "sql")
        )
        || name.starts_with(".env.")
        || name.starts_with(".eslintrc")
        || name.starts_with(".babelrc")
        || name.starts_with(".prettierrc")
        || name.starts_with(".swcrc")
        || name.starts_with("eslint.config.")
        || name.starts_with("babel.config.")
        || name.starts_with("dockerfile.")
        || name.starts_with("tsconfig")
        || name.starts_with("vite.config.")
        || name.starts_with("vitest.config.")
        || name.starts_with("jest.config.")
        || name.starts_with("next.config.")
        || name.starts_with("playwright.config.")
        || name.starts_with("cypress.config.")
        || name.starts_with("build.gradle")
        || name.starts_with("settings.gradle")
        || (path
            .components()
            .any(|component| component.as_os_str() == ".github")
            && matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("yml" | "yaml")
            ))
}

/// Add build/test inputs that are not ordinary source extensions.
///
/// Hidden vendor/runtime trees stay excluded, while `.github` is traversed
/// because workflow changes can alter the exact verifier that produced a QC
/// receipt. Metadata only; file contents and secrets are never read.
fn collect_qc_inputs(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> bool {
    if out.len() >= MAX_FINGERPRINT_FILES {
        return false;
    }
    if depth > MAX_SOURCE_DEPTH {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    let mut complete = true;
    for entry in entries.flatten() {
        let path = entry.path();
        match classify_no_follow(&path) {
            EntryKind::Dir => {
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default();
                if SKIP_DIRS.contains(&name)
                    || (name.starts_with('.') && name != ".github" && name != ".cargo")
                    || crate::acceptance::is_python_venv(&path)
                {
                    continue;
                }
                if !collect_qc_inputs(&path, out, depth + 1) {
                    complete = false;
                }
            }
            EntryKind::File => {
                if out.len() >= MAX_FINGERPRINT_FILES {
                    return false;
                }
                if is_qc_input_file(&path) {
                    out.push(path);
                }
            }
            EntryKind::Skip => {}
        }
    }
    complete
}

fn fingerprint_files(root: &Path, mut files: Vec<PathBuf>) -> String {
    files.sort();
    files.dedup();
    let mut digest = String::with_capacity(files.len() * 48);
    for file in &files {
        let rel = file
            .strip_prefix(root)
            .unwrap_or(file)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let (len, mtime) = match std::fs::metadata(file) {
            Ok(metadata) => {
                let modified = metadata
                    .modified()
                    .ok()
                    .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |duration| duration.as_nanos());
                (metadata.len(), modified)
            }
            Err(_) => (0, 0),
        };
        digest.push_str(&format!("{rel}\u{1f}{len}\u{1f}{mtime}\n"));
    }
    format!("{:016x}-{}", fnv1a(digest.as_bytes()), files.len())
}

/// A fingerprint of the project's source tree — the identity of the exact code state a
/// proof describes — or `None` when the tree is too large to fingerprint honestly.
///
/// Hashes each file's workspace-relative path, byte length, and modification time.
/// Metadata only (no file is read), so this is a directory walk, not a scan.
///
/// Any mutation of the source that a build would notice (an edited file, a new file, a
/// deleted file) changes the fingerprint. A merely-touched file also changes it: the
/// fingerprint errs toward "the tree moved", because a false STALE only costs a
/// re-verification while a false FRESH ships an unverified claim.
///
/// **`None` means "we cannot establish this"**, never "nothing changed": the walk hit
/// [`MAX_FINGERPRINT_FILES`] and any hash we produced would only cover a prefix of the
/// tree, silently blessing every edit beyond it. An unreadable or empty tree is a
/// different thing entirely — it fingerprints as the empty set (the walk is fail-open),
/// which is an honest answer about a tree with no source in it.
#[must_use]
pub fn workspace_fingerprint(root: &Path) -> Option<String> {
    let mut files = Vec::new();
    if !collect(root, &mut files, 0) {
        // Truncated. Refuse to answer rather than certify a prefix.
        tracing::warn!(
            cap = MAX_FINGERPRINT_FILES,
            root = %root.display(),
            "source tree exceeds the freshness fingerprint cap; evidence in this workspace \
             carries no freshness stamp (checks re-verify rather than trust a cached proof)"
        );
        return None;
    }
    Some(fingerprint_files(root, files))
}

/// Fingerprint every input that can invalidate a final quality review receipt:
/// source/test files plus manifests, lockfiles, verifier configs, containers,
/// and CI workflows.
///
/// A final-review continuation may skip build/test only when this identity
/// still matches. Unlike [`workspace_fingerprint`], changing `Cargo.toml`,
/// `package.json`, a lockfile, or a test/CI config therefore expires the receipt.
#[must_use]
pub fn workspace_qc_fingerprint(root: &Path) -> Option<String> {
    let mut files = Vec::new();
    if !collect(root, &mut files, 0) || !collect_qc_inputs(root, &mut files, 0) {
        tracing::warn!(
            cap = MAX_FINGERPRINT_FILES,
            root = %root.display(),
            "QC input tree exceeds the freshness fingerprint cap; final review will re-run checks"
        );
        return None;
    }
    Some(fingerprint_files(root, files))
}

/// Whether a proof stamped with `recorded` is STALE against the project's source
/// tree as it stands NOW — i.e. the code changed after the proof was taken, so the
/// proof no longer describes what we are about to ship.
///
/// - `recorded == None` (an unstamped / older artifact, or one produced where the tree
///   could not be fingerprinted) ⇒ **not stale**. We do not know what tree it described,
///   and an unknown is never a finding. FAIL-OPEN.
/// - `recorded == Some(fp)` and the tree can no longer be fingerprinted (it grew past
///   [`MAX_FINGERPRINT_FILES`]) ⇒ **stale**. We cannot establish that the proof still
///   describes this tree, and we do not claim a freshness we cannot establish. The cost
///   is a re-verification (the floor runs the check for real) — not a block.
/// - Otherwise: stale exactly when the recorded fingerprint DIFFERS from the current one
///   — a positive, evidenced finding.
#[must_use]
pub fn is_stale(root: &Path, recorded: Option<&str>) -> bool {
    let Some(recorded) = recorded.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    match workspace_fingerprint(root) {
        Some(current) => recorded != current,
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn fingerprint_is_stable_for_an_unchanged_tree() {
        let tmp = tempfile::TempDir::new().unwrap();
        write(tmp.path(), "src/main.rs", "fn main() { let a = 1; }\n");
        let a = workspace_fingerprint(tmp.path());
        let b = workspace_fingerprint(tmp.path());
        assert!(a.is_some());
        assert_eq!(a, b, "an untouched tree fingerprints identically");
    }

    #[test]
    fn fingerprint_changes_on_edit_add_and_delete() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/main.rs", "fn main() { let a = 1; }\n");
        let base = workspace_fingerprint(root);

        // Edit: content (and therefore length) moves.
        write(root, "src/main.rs", "fn main() { let a = 1; let b = 2; }\n");
        let edited = workspace_fingerprint(root);
        assert_ne!(base, edited, "an edit must move the fingerprint");

        // Add.
        write(root, "src/extra.rs", "pub fn extra() -> u8 { 7 }\n");
        let added = workspace_fingerprint(root);
        assert_ne!(edited, added, "a new file must move the fingerprint");

        // Delete.
        std::fs::remove_file(root.join("src/extra.rs")).unwrap();
        let deleted = workspace_fingerprint(root);
        assert_ne!(added, deleted, "a deletion must move the fingerprint");
        assert_eq!(deleted, edited, "and it returns to the pre-add identity");
    }

    #[test]
    fn qc_fingerprint_changes_for_manifest_lock_and_ci_inputs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/main.rs", "fn main() {}\n");
        write(root, "Cargo.toml", "[package]\nname = \"demo\"\n");
        let initial = workspace_qc_fingerprint(root);

        write(
            root,
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        );
        let manifest = workspace_qc_fingerprint(root);
        assert_ne!(initial, manifest);

        write(root, "Cargo.lock", "version = 4\n");
        let lockfile = workspace_qc_fingerprint(root);
        assert_ne!(manifest, lockfile);

        write(root, ".github/workflows/ci.yml", "jobs: {}\n");
        let workflow = workspace_qc_fingerprint(root);
        assert_ne!(lockfile, workflow);
    }

    #[test]
    fn qc_fingerprint_changes_for_data_and_tool_config_inputs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/main.ts", "export const a = 1;\n");
        let inputs = [
            "tests/fixtures/users.json",
            "config/app.yaml",
            "config/app.yml",
            "config/settings.toml",
            "db/seed.sql",
            ".env.test",
            ".eslintrc",
            ".babelrc",
            "eslint.config.mjs",
            "babel.config.js",
            "rust-toolchain",
        ];
        for rel in inputs {
            write(root, rel, "a");
        }
        for rel in inputs {
            let before = workspace_qc_fingerprint(root);
            write(root, rel, "changed");
            assert_ne!(
                before,
                workspace_qc_fingerprint(root),
                "{rel} must expire the receipt"
            );
        }
        // Dependency / virtualenv trees stay out of the QC identity.
        let before = workspace_qc_fingerprint(root);
        write(root, "node_modules/pkg/package.json", "{}");
        write(root, "env/pyvenv.cfg", "home = /usr/bin\n");
        write(root, "env/lib/site-packages/pkg/data.json", "{}");
        assert_eq!(before, workspace_qc_fingerprint(root));
    }

    #[test]
    fn stale_only_when_a_recorded_fingerprint_actually_differs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        write(root, "src/main.rs", "fn main() { let a = 1; }\n");
        let taken = workspace_fingerprint(root).expect("a small tree is fingerprintable");

        // Fresh: the proof describes the tree as it stands.
        assert!(
            !is_stale(root, Some(&taken)),
            "a proof taken on THIS tree is fresh"
        );

        // The source moves after the proof was taken → the proof is now stale.
        write(root, "src/main.rs", "fn main() { let a = 999; }\n");
        assert!(
            is_stale(root, Some(&taken)),
            "evidence produced before the last change to the code it describes is not evidence"
        );

        // FAIL-OPEN: an unstamped proof is never stale (we do not know what it saw).
        assert!(
            !is_stale(root, None),
            "an absent fingerprint is not a finding"
        );
        assert!(
            !is_stale(root, Some("   ")),
            "a blank fingerprint is not a finding"
        );
    }

    #[test]
    fn an_edit_far_past_the_acceptance_walks_600_file_cap_still_moves_the_fingerprint() {
        // N1 — THE FALSE-FRESH BUG. The fingerprint used to be taken over
        // `acceptance::source_files`, which stops at 600 files. On any repo bigger than
        // that, an edit BEYOND the truncation point was never hashed and the file count
        // stayed pinned at the cap — so the fingerprint did not move, `is_stale` said
        // "fresh", and a proof taken BEFORE the edit was accepted as current. That is
        // exactly the "stale green" this module exists to prevent, firing on precisely
        // the large commercial repos where it matters.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        let beyond_the_old_cap = crate::acceptance::MAX_SOURCE_FILES + 200;
        for i in 0..beyond_the_old_cap {
            write(root, &format!("src/m{i:04}/f.ts"), "export const x = 1;\n");
        }
        let taken = workspace_fingerprint(root).expect("a few hundred files still fingerprint");

        // Edit a file that a 600-file walk would never even have seen.
        let far_past_the_cap = beyond_the_old_cap - 1;
        write(
            root,
            &format!("src/m{far_past_the_cap:04}/f.ts"),
            "export const x = 999; // the edit the old fingerprint could not see\n",
        );

        assert!(
            is_stale(root, Some(&taken)),
            "an edit beyond the acceptance walk's cap MUST move the fingerprint — otherwise a \
             proof taken before it is certified as current"
        );
    }

    #[test]
    fn a_tree_too_large_to_fingerprint_refuses_to_answer_instead_of_certifying_a_prefix() {
        // The bound has to exist; what it must NEVER do is answer with a prefix hash. A
        // refusal is honest ("we cannot establish this"), and it costs a re-verification.
        // A prefix hash is a LIE ("this tree is unchanged") and it ships an unverified
        // claim. Simulated at the walk level so the test does not need 20k real files.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        for i in 0..8 {
            write(root, &format!("src/f{i}.ts"), "export const x = 1;\n");
        }
        let mut files = Vec::new();
        assert!(
            collect(root, &mut files, 0),
            "a small tree walks to completion"
        );

        // Pre-fill the accumulator to the cap: the very next entry truncates the walk.
        let mut saturated: Vec<PathBuf> = (0..MAX_FINGERPRINT_FILES)
            .map(|i| PathBuf::from(format!("pad{i}.ts")))
            .collect();
        assert!(
            !collect(root, &mut saturated, 0),
            "a walk that hits the cap reports TRUNCATED, not a partial success"
        );

        // And a truncated walk yields no fingerprint at all — which `is_stale` reads as
        // "not trusted as fresh" for a stamped proof, and still fail-open for an
        // unstamped one.
        assert!(
            !is_stale(root, None),
            "no stamp is still never a finding, whatever the tree looks like"
        );
    }
}
