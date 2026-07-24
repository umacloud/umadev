use super::captured_index::CapturedGitIndex;
use super::{
    git_command_failed, git_commit_blocked, git_output, git_required_text, git_std_command,
    same_permissions, GitCommitBaseline, Path, PathBuf, ResidentExecutionBlocked,
};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone)]
pub(crate) struct GitIndexSnapshot {
    pub(crate) root: PathBuf,
    pub(crate) path: PathBuf,
    pub(crate) bytes: Option<Vec<u8>>,
    pub(crate) permissions: Option<std::fs::Permissions>,
    pub(crate) logical_entries: Option<Vec<u8>>,
}

impl GitIndexSnapshot {
    pub(crate) fn capture(root: &Path) -> Result<Self, ResidentExecutionBlocked> {
        let raw = git_required_text(
            root,
            &["rev-parse", "--git-path", "index"],
            "git-index-path-unverifiable",
        )?;
        let path = PathBuf::from(raw);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(git_commit_blocked(
                    "git-index-unverifiable",
                    &format!("无法读取 Git index 元数据 / unable to inspect Git index: {error}"),
                ));
            }
        };
        if metadata
            .as_ref()
            .is_some_and(|item| item.is_dir() || item.file_type().is_symlink())
        {
            return Err(git_commit_blocked(
                "git-index-unsafe-type",
                "Git index 不是普通文件,拒绝执行事务 / Git index is not a regular file",
            ));
        }
        let bytes = metadata
            .as_ref()
            .map(|_| std::fs::read(&path))
            .transpose()
            .map_err(|error| {
                git_commit_blocked(
                    "git-index-unverifiable",
                    &format!("无法读取 Git index / unable to read Git index: {error}"),
                )
            })?;
        let permissions = metadata.map(|item| item.permissions());
        let logical_entries = bytes
            .as_ref()
            .map(|_| git_index_logical_entries(root, &path))
            .transpose()?;
        Ok(Self {
            root: root.to_path_buf(),
            path,
            bytes,
            permissions,
            logical_entries,
        })
    }

    pub(crate) fn verify_unchanged(&self) -> Result<(), ResidentExecutionBlocked> {
        if self.matches_current()? {
            Ok(())
        } else {
            Err(git_commit_blocked(
                "git-index-changed",
                "Git index 在提交基线冻结后发生变化 / Git index changed after the commit baseline was captured",
            ))
        }
    }

    pub(crate) fn matches_current(&self) -> Result<bool, ResidentExecutionBlocked> {
        let current = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                let bytes = std::fs::read(&self.path).map_err(|error| {
                    git_commit_blocked(
                        "git-index-unverifiable",
                        &format!("无法重读 Git index / unable to reread Git index: {error}"),
                    )
                })?;
                Some((bytes, metadata.permissions()))
            }
            Ok(_) => {
                return Err(git_commit_blocked(
                    "git-index-changed",
                    "Git index 文件类型在提交前发生变化 / Git index file type changed before commit",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(git_commit_blocked(
                    "git-index-unverifiable",
                    &format!("无法重读 Git index 元数据 / unable to inspect Git index: {error}"),
                ));
            }
        };
        Ok(match (&self.bytes, &self.permissions, current) {
            (None, None, None) => true,
            (Some(expected), Some(permissions), Some((current, current_permissions))) => {
                expected == &current && same_permissions(permissions, &current_permissions)
            }
            _ => false,
        })
    }

    pub(crate) fn logically_matches_current(&self) -> Result<bool, ResidentExecutionBlocked> {
        let current = GitIndexSnapshot::capture(&self.root)?;
        Ok(self.logical_entries == current.logical_entries
            && match (&self.permissions, &current.permissions) {
                (None, None) => true,
                (Some(expected), Some(current)) => same_permissions(expected, current),
                _ => false,
            })
    }

    pub(crate) fn restore(&self) -> Result<(), ResidentExecutionBlocked> {
        match (&self.bytes, &self.permissions) {
            (Some(bytes), Some(permissions)) => {
                if self.path.parent().is_none_or(|parent| !parent.is_dir()) {
                    return Err(git_commit_blocked(
                        "git-index-restore-failed",
                        "Git index 父目录不存在 / Git index parent directory is unavailable",
                    ));
                }
                umadev_state::fs::atomic_write(&self.path, bytes).map_err(|error| {
                    git_commit_blocked(
                        "git-index-restore-failed",
                        &format!("无法原子恢复 Git index / unable to restore Git index: {error}"),
                    )
                })?;
                std::fs::set_permissions(&self.path, permissions.clone()).map_err(|error| {
                    git_commit_blocked(
                        "git-index-restore-failed",
                        &format!(
                            "Git index 内容已恢复但权限恢复失败 / index bytes restored but permissions failed: {error}"
                        ),
                    )
                })
            }
            (None, None) => match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(git_commit_blocked(
                    "git-index-restore-failed",
                    &format!(
                        "无法恢复原本不存在的 Git index / unable to remove new index: {error}"
                    ),
                )),
            },
            _ => Err(git_commit_blocked(
                "git-index-restore-failed",
                "Git index 快照不完整 / Git index snapshot is incomplete",
            )),
        }
    }
}

pub(crate) fn expected_commit_tree(
    root: &Path,
    baseline: &GitCommitBaseline,
    paths: &[&str],
) -> Result<String, ResidentExecutionBlocked> {
    if baseline.staged_only {
        let temporary = CapturedGitIndex::materialize(&baseline.index)?;
        return git_index_required_text(
            root,
            &temporary.path,
            &["write-tree"],
            "git-expected-tree-unverifiable",
        );
    }

    let temporary = TemporaryGitIndex::create(&baseline.index.path)?;
    git_index_command(
        root,
        &temporary.path,
        &["read-tree", baseline.head.as_deref().unwrap_or("HEAD")],
    )?;
    for path in paths {
        git_index_command(
            root,
            &temporary.path,
            &["update-index", "--force-remove", "--", path],
        )?;
        if let Some((mode, object)) = git_stage_zero_entry(root, path)? {
            git_index_command(
                root,
                &temporary.path,
                &["update-index", "--add", "--cacheinfo", &mode, &object, path],
            )?;
        }
    }
    git_index_required_text(
        root,
        &temporary.path,
        &["write-tree"],
        "git-expected-tree-unverifiable",
    )
}

#[derive(Debug)]
struct TemporaryGitIndex {
    directory: PathBuf,
    path: PathBuf,
}

impl TemporaryGitIndex {
    pub(crate) fn create(real_index: &Path) -> Result<Self, ResidentExecutionBlocked> {
        static TEMP_ID: AtomicU64 = AtomicU64::new(1);
        let parent = real_index.parent().ok_or_else(|| {
            git_commit_blocked(
                "git-expected-tree-unverifiable",
                "Git index 没有可用父目录 / Git index has no usable parent directory",
            )
        })?;
        for _ in 0..64 {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let directory = parent.join(format!(
                ".umadev-index-transaction-{}-{id}",
                std::process::id()
            ));
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        path: directory.join("index"),
                        directory,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(git_commit_blocked(
                        "git-expected-tree-unverifiable",
                        &format!(
                            "无法创建临时 Git index / unable to create a temporary Git index: {error}"
                        ),
                    ));
                }
            }
        }
        Err(git_commit_blocked(
            "git-expected-tree-unverifiable",
            "无法分配唯一临时 Git index / unable to allocate a unique temporary Git index",
        ))
    }
}

impl Drop for TemporaryGitIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

pub(crate) fn git_index_command(
    root: &Path,
    index: &Path,
    args: &[&str],
) -> Result<(), ResidentExecutionBlocked> {
    let index = git_index_env_path(index)?;
    let output = git_std_command(root)
        .args(args)
        .env("GIT_INDEX_FILE", &index)
        .output()
        .map_err(|error| {
            git_commit_blocked(
                "git-command-unavailable",
                &format!("无法执行临时 Git index 命令 / unable to execute temporary-index Git command: {error}"),
            )
        })?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_command_failed(
            "git-expected-tree-unverifiable",
            "git",
            &output,
        ))
    }
}

pub(crate) fn git_index_required_text(
    root: &Path,
    index: &Path,
    args: &[&str],
    code: &'static str,
) -> Result<String, ResidentExecutionBlocked> {
    let index = git_index_env_path(index)?;
    let output = git_std_command(root)
        .args(args)
        .env("GIT_INDEX_FILE", &index)
        .output()
        .map_err(|error| {
            git_commit_blocked(
                "git-command-unavailable",
                &format!("无法执行临时 Git index 命令 / unable to execute temporary-index Git command: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err(git_command_failed(code, "git", &output));
    }
    let value = String::from_utf8(output.stdout)
        .map_err(|_| git_commit_blocked(code, "临时 Git index 返回了非 UTF-8 输出"))?;
    let value = value.trim();
    if value.is_empty() {
        Err(git_commit_blocked(
            code,
            "临时 Git index 返回了空结果 / temporary Git index returned an empty result",
        ))
    } else {
        Ok(value.to_string())
    }
}

pub(crate) fn git_index_logical_entries(
    root: &Path,
    index: &Path,
) -> Result<Vec<u8>, ResidentExecutionBlocked> {
    let mut canonical = Vec::new();
    let index = git_index_env_path(index)?;
    for args in [
        ["ls-files", "--stage", "-z"].as_slice(),
        ["ls-files", "-v", "-z"].as_slice(),
    ] {
        let output = git_std_command(root)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .map_err(|error| {
                git_commit_blocked(
                    "git-index-unverifiable",
                    &format!(
                        "无法读取 Git index 逻辑条目 / unable to inspect logical index entries: {error}"
                    ),
                )
            })?;
        if !output.status.success() {
            return Err(git_command_failed(
                "git-index-unverifiable",
                "git ls-files",
                &output,
            ));
        }
        canonical.extend_from_slice(&output.stdout);
        canonical.push(0xff);
    }
    Ok(canonical)
}

#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)]
fn git_index_env_path(index: &Path) -> Result<PathBuf, ResidentExecutionBlocked> {
    Ok(index.to_path_buf())
}

#[cfg(windows)]
fn git_index_env_path(index: &Path) -> Result<PathBuf, ResidentExecutionBlocked> {
    use std::ffi::OsString;
    use std::path::{Component, Prefix};

    let mut components = index.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Ok(index.to_path_buf());
    };
    let mut plain = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", drive as char)),
        Prefix::VerbatimUNC(server, share) => {
            let mut root = OsString::from(r"\\");
            root.push(server);
            root.push("\\");
            root.push(share);
            PathBuf::from(root)
        }
        Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
            return Err(git_commit_blocked(
                "git-index-path-unverifiable",
                "Git 不支持该临时 index 设备路径 / Git cannot use this temporary-index device path",
            ));
        }
        _ => return Ok(index.to_path_buf()),
    };
    for component in components {
        if !matches!(component, Component::RootDir) {
            plain.push(component.as_os_str());
        }
    }
    Ok(plain)
}

pub(crate) fn git_stage_zero_entry(
    root: &Path,
    path: &str,
) -> Result<Option<(String, String)>, ResidentExecutionBlocked> {
    let output = git_output(root, &["ls-files", "--stage", "-z", "--", path])?;
    if !output.status.success() {
        return Err(git_command_failed(
            "git-expected-tree-unverifiable",
            "git ls-files",
            &output,
        ));
    }
    let records = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect::<Vec<_>>();
    if records.is_empty() {
        return Ok(None);
    }
    if records.len() != 1 {
        return Err(git_commit_blocked(
            "git-expected-tree-unverifiable",
            "精确路径对应多个 index stage / exact path has multiple index stages",
        ));
    }
    let record = std::str::from_utf8(records[0]).map_err(|_| {
        git_commit_blocked(
            "git-expected-tree-unverifiable",
            "Git index entry 不是 UTF-8 / Git index entry is not UTF-8",
        )
    })?;
    let (metadata, recorded_path) = record.split_once('\t').ok_or_else(|| {
        git_commit_blocked(
            "git-expected-tree-unverifiable",
            "Git index entry 格式无效 / malformed Git index entry",
        )
    })?;
    if recorded_path != path {
        return Err(git_commit_blocked(
            "git-expected-tree-unverifiable",
            "Git index 返回了非请求路径 / Git index returned a different path",
        ));
    }
    let fields = metadata.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 3 || fields[2] != "0" {
        return Err(git_commit_blocked(
            "git-expected-tree-unverifiable",
            "Git index entry 不是 stage 0 / Git index entry is not stage zero",
        ));
    }
    Ok(Some((fields[0].to_string(), fields[1].to_string())))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::git_index_env_path;
    use std::path::{Path, PathBuf};

    #[test]
    fn git_index_env_path_converts_only_supported_verbatim_paths() {
        assert_eq!(
            git_index_env_path(Path::new(r"\\?\C:\项目 with space\.git\index")).unwrap(),
            PathBuf::from(r"C:\项目 with space\.git\index")
        );
        assert_eq!(
            git_index_env_path(Path::new(r"\\?\UNC\server\share\项目\.git\index")).unwrap(),
            PathBuf::from(r"\\server\share\项目\.git\index")
        );
        assert_eq!(
            git_index_env_path(Path::new(r"C:\plain\.git\index")).unwrap(),
            PathBuf::from(r"C:\plain\.git\index")
        );
        assert!(git_index_env_path(Path::new(
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\index"
        ))
        .is_err());
        assert!(git_index_env_path(Path::new(r"\\.\C:\device\index")).is_err());
    }
}
