//! Path components that must never be snapshotted or restored because a
//! filesystem may resolve them to `.git` or `.umadev`.
//!
//! Comparing bytes is not enough. Restores run automatically (the startup heal)
//! from trees UmaDev did not necessarily write, and on macOS and Windows `.GIT`
//! opens the real `.git`: a tree entry `.GIT/hooks/pre-commit` would install a
//! hook, and `.Git/config` a `core.fsmonitor`. This mirrors Git's own
//! `verify_path` guards (`is_hfs_dotgit`, `is_ntfs_dotgit`) for the two names
//! UmaDev protects.

/// Code points HFS+ ignores when comparing names, so `.g\u{200c}it` is `.git`.
fn hfs_ignorable(character: char) -> bool {
    matches!(
        character,
        '\u{200c}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{206a}'..='\u{206f}' | '\u{feff}'
    )
}

/// Whether `name` may resolve to `.git` or `.umadev` on some filesystem:
/// case-insensitively, ignoring HFS+ zero-width code points, after NTFS drops
/// trailing dots and spaces or an alternate-data-stream suffix (`.git::$DATA`),
/// or as an NTFS 8.3 short name (`GIT~1`, Git's hashed `GI7EBA~1`, `UMADEV~1`).
/// A file of that name counts too: a `.git` file redirects repository discovery.
pub(super) fn is_reserved_alias(name: &str) -> bool {
    let folded = name
        .chars()
        .filter(|character| !hfs_ignorable(*character))
        .collect::<String>()
        .to_ascii_lowercase();
    let stem = folded.split(':').next().unwrap_or_default();
    let stem = stem.trim_end_matches(['.', ' ']);
    if matches!(stem, ".git" | ".umadev") {
        return true;
    }
    // Short names keep at most six characters before `~<n>`; Git also rejects
    // its hashed `gi<4 hex>~<n>` form, so any `gi`/`um` prefix of that shape is
    // refused.
    stem.split_once('~').is_some_and(|(prefix, ordinal)| {
        (prefix.starts_with("gi") || prefix.starts_with("um"))
            && prefix.len() <= 6
            && !ordinal.is_empty()
            && ordinal.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use super::super::validated_tree_path;
    #[cfg(unix)]
    use super::super::{
        create_checkpoint, git, git_with_input, restore_checkpoint, scan_checkpoint_files,
    };
    use super::is_reserved_alias;

    #[test]
    fn every_filesystem_spelling_of_the_protected_directories_is_reserved() {
        for name in [
            ".git",
            ".GIT",
            ".Git",
            ".git.",
            ".git ",
            ".git. .",
            ".git::$INDEX_ALLOCATION",
            ".g\u{200c}it",
            "\u{feff}.GIT",
            "GIT~1",
            "git~2",
            "GI7EBA~1",
            ".umadev",
            ".UMADEV",
            ".UmaDev.",
            "UMADEV~1",
        ] {
            assert!(is_reserved_alias(name), "{name:?}");
        }
    }

    #[test]
    fn ordinary_names_are_not_reserved() {
        for name in [
            ".gitignore",
            ".gitattributes",
            ".github",
            "git",
            "digit.ts",
            ".umadevrc",
            "src",
            "notes~",
            "gitlab-ci~draft",
        ] {
            assert!(!is_reserved_alias(name), "{name:?}");
        }
    }

    #[test]
    fn restore_refuses_trees_that_alias_the_repository_on_any_filesystem() {
        for path in [
            ".GIT/hooks/pre-commit",
            ".Git/config",
            ".git./config",
            "GIT~1/config",
            ".UMADEV/temp-rewind.json",
            "vendor/lib/.git",
        ] {
            assert!(validated_tree_path(path).is_err(), "{path}");
        }
        assert!(validated_tree_path("src/.gitignore").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_submodule_gitfile_is_never_snapshotted() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("vendor/lib")).unwrap();
        std::fs::write(root.join("vendor/lib/.git"), "gitdir: /elsewhere\n").unwrap();
        std::fs::write(root.join("vendor/lib/lib.rs"), "fn main() {}\n").unwrap();
        let paths = scan_checkpoint_files(root)
            .unwrap()
            .into_iter()
            .map(|file| file.path)
            .collect::<Vec<_>>();
        assert_eq!(paths, ["vendor/lib/lib.rs"]);
    }

    /// A checkpoint whose tree holds `.GIT/hooks/pre-commit` (which `.git`
    /// resolves to on a case-insensitive filesystem) must not be restored.
    #[cfg(unix)]
    #[test]
    fn a_checkpoint_naming_dot_git_in_another_case_is_not_restored() {
        let has_git = std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !has_git {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join("app.rs"), "one").unwrap();
        create_checkpoint(root, "base").expect("checkpoint");

        let text = |args: &[&str], input: &[u8]| -> String {
            let output = git_with_input(root, args, input, 64 * 1024).unwrap();
            assert!(
                output.status.is_some_and(|status| status.success()),
                "{args:?}"
            );
            String::from_utf8(output.stdout).unwrap().trim().to_string()
        };
        let blob = text(
            &["hash-object", "-w", "--stdin"],
            b"#!/bin/sh\ntouch pwned\n",
        );
        let hooks = text(
            &["mktree"],
            format!("100755 blob {blob}\tpre-commit\n").as_bytes(),
        );
        let dot_git = text(
            &["mktree"],
            format!("040000 tree {hooks}\thooks\n").as_bytes(),
        );
        let app = text(&["hash-object", "-w", "--stdin"], b"two");
        let root_tree = text(
            &["mktree"],
            format!("040000 tree {dot_git}\t.GIT\n100644 blob {app}\tapp.rs\n").as_bytes(),
        );
        let commit = text(
            &["commit-tree", &root_tree, "-p", "HEAD", "-F", "-"],
            b"hostile",
        );
        let moved = git(root, &["update-ref", "HEAD", &commit]).unwrap();
        assert!(moved.status.success());

        let error = restore_checkpoint(root, &commit[..12]).unwrap_err();
        assert!(error.contains("unsafe path"), "{error}");
        assert!(!root.join(".GIT").exists());
        assert!(!root.join(".git/hooks/pre-commit").exists());
        assert_eq!(std::fs::read_to_string(root.join("app.rs")).unwrap(), "one");
    }
}
