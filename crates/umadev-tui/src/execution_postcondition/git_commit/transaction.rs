use super::{
    bounded_text, combined_git_failure, git_command_failed, git_commit_blocked, git_optional_text,
    git_output, git_required_text, GitCommitBaseline, GitIndexSnapshot, Path, PathBuf,
    ResidentExecutionBlocked,
};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub(crate) struct GitTransactionGuard {
    pub(crate) root: PathBuf,
    pub(crate) original_index: GitIndexSnapshot,
    pub(crate) expected_current_index: GitIndexSnapshot,
    pub(crate) before_head: Option<String>,
    pub(crate) reference: Option<String>,
    pub(crate) expected_tree: Option<String>,
    pub(crate) owned_commit: Option<String>,
    reflog_action: String,
    pub(crate) armed: bool,
}

#[derive(Debug)]
pub(crate) enum GitValidationRollback {
    Recovered(ResidentExecutionBlocked),
    NeedsFallback {
        validation: ResidentExecutionBlocked,
        rollback: ResidentExecutionBlocked,
    },
}

#[derive(Debug)]
pub(crate) struct InertHooksDirectory {
    path: PathBuf,
}

impl InertHooksDirectory {
    pub(crate) fn create() -> Result<Self, ResidentExecutionBlocked> {
        static HOOKS_ID: AtomicU64 = AtomicU64::new(1);
        let parent = std::env::temp_dir();
        for _ in 0..64 {
            let id = HOOKS_ID.fetch_add(1, Ordering::Relaxed);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let path = parent.join(format!(
                "umadev-inert-hooks-{}-{nonce:x}-{id}",
                std::process::id()
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Err(error) =
                            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        {
                            let _ = std::fs::remove_dir(&path);
                            return Err(git_commit_blocked(
                                "git-inert-hooks-unavailable",
                                &format!(
                                    "无法保护临时空 hook 目录 / unable to protect the temporary inert hooks directory: {error}"
                                ),
                            ));
                        }
                    }
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(git_commit_blocked(
                        "git-inert-hooks-unavailable",
                        &format!(
                            "无法创建临时空 hook 目录 / unable to create a temporary inert hooks directory: {error}"
                        ),
                    ));
                }
            }
        }
        Err(git_commit_blocked(
            "git-inert-hooks-unavailable",
            "无法分配临时空 hook 目录 / unable to allocate a temporary inert hooks directory",
        ))
    }

    pub(crate) fn config_value(&self) -> String {
        format!("core.hooksPath={}", self.path.to_string_lossy())
    }
}

impl Drop for InertHooksDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}

impl GitTransactionGuard {
    pub(crate) fn new(root: &Path, baseline: &GitCommitBaseline) -> Self {
        static TRANSACTION_ID: AtomicU64 = AtomicU64::new(1);
        let id = TRANSACTION_ID.fetch_add(1, Ordering::Relaxed);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        Self {
            root: root.to_path_buf(),
            original_index: baseline.index.clone(),
            expected_current_index: baseline.index.clone(),
            before_head: baseline.head.clone(),
            reference: baseline.symbolic_head.clone(),
            expected_tree: None,
            owned_commit: None,
            reflog_action: format!("umadev-host-commit-{}-{nonce:x}-{id}", std::process::id()),
            armed: true,
        }
    }

    pub(crate) fn reflog_action(&self) -> &str {
        &self.reflog_action
    }

    pub(crate) fn mark_owned_commit(
        &mut self,
        commit: String,
    ) -> Result<(), ResidentExecutionBlocked> {
        self.verify_current_head_owned(&commit)?;
        self.owned_commit = Some(commit);
        Ok(())
    }

    pub(crate) fn verify_current_head_owned(
        &self,
        expected_commit: &str,
    ) -> Result<(), ResidentExecutionBlocked> {
        let current = git_optional_text(&self.root, &["rev-parse", "--verify", "HEAD"])?;
        if current.as_deref() != Some(expected_commit) {
            return Err(git_commit_blocked(
                "git-transaction-ownership-unproven",
                "当前 HEAD 不是本事务记录的精确提交,拒绝回滚 / current HEAD is not the exact commit recorded by this transaction",
            ));
        }
        if self
            .owned_commit
            .as_deref()
            .is_some_and(|owned| owned != expected_commit)
        {
            return Err(git_commit_blocked(
                "git-transaction-ownership-unproven",
                "待回滚提交与本事务已确认提交不一致 / the rollback target differs from the commit confirmed for this transaction",
            ));
        }
        let Some(reference) = self.reference.as_deref() else {
            return Err(git_commit_blocked(
                "git-transaction-ownership-unproven",
                "原分支引用不可用,无法证明提交归属 / the original branch ref is unavailable, so commit ownership cannot be proven",
            ));
        };
        let symbolic = git_optional_text(&self.root, &["symbolic-ref", "--quiet", "HEAD"])?;
        if symbolic.as_deref() != Some(reference) {
            return Err(git_commit_blocked(
                "git-transaction-ownership-unproven",
                "当前分支不是本事务开始时的精确引用,拒绝回滚 / current branch is not the exact ref captured by this transaction",
            ));
        }
        let output = git_output(
            &self.root,
            &["reflog", "show", "-n", "1", "--format=%H%x00%gs", reference],
        )?;
        if !output.status.success() {
            return Err(git_command_failed(
                "git-transaction-ownership-unproven",
                "git reflog show",
                &output,
            ));
        }
        let entry = String::from_utf8(output.stdout).map_err(|_| {
            git_commit_blocked(
                "git-transaction-ownership-unproven",
                "Git reflog 返回了非 UTF-8 输出,无法证明提交归属 / Git reflog returned non-UTF-8 output",
            )
        })?;
        let entry = entry.trim_end_matches(['\r', '\n']);
        let Some((reflog_commit, subject)) = entry.split_once('\0') else {
            return Err(git_commit_blocked(
                "git-transaction-ownership-unproven",
                "Git reflog 缺少可验证事务标记 / Git reflog is missing a verifiable transaction marker",
            ));
        };
        let marker = format!("{}:", self.reflog_action);
        if reflog_commit != expected_commit || !subject.starts_with(&marker) {
            return Err(git_commit_blocked(
                "git-transaction-ownership-unproven",
                "最新 reflog 项不属于本事务,拒绝覆盖外部提交 / the latest reflog entry is not owned by this transaction; an external commit was preserved",
            ));
        }
        Ok(())
    }

    pub(crate) fn observe_current_index(&mut self) -> Result<(), ResidentExecutionBlocked> {
        self.expected_current_index = GitIndexSnapshot::capture(&self.root)?;
        Ok(())
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }

    pub(crate) fn finish_failure(
        &mut self,
        failure: ResidentExecutionBlocked,
    ) -> ResidentExecutionBlocked {
        let recovery = self.recover_sync();
        self.disarm();
        match recovery {
            Ok(()) => failure,
            Err(recovery) => combined_git_failure(
                "git-transaction-recovery-failed",
                &failure,
                &recovery,
                "Git 事务失败且无法证明安全恢复; 已保留现场 / Git failed and safe recovery could not be proven; repository state was left intact",
            ),
        }
    }

    pub(crate) fn finish_validation_rollback_failure(
        &mut self,
        validation: ResidentExecutionBlocked,
        rollback: ResidentExecutionBlocked,
    ) -> ResidentExecutionBlocked {
        let fallback = self.recover_sync();
        self.disarm();
        match fallback {
            Ok(()) => ResidentExecutionBlocked {
                note: format!(
                    "{}; [rollback-fallback-ok] 异步回滚失败后,已用同步 CAS 恢复原 HEAD 和原始 index / synchronous CAS restored the original HEAD and exact index after async rollback failed\n异步回滚失败: {}",
                    validation.note, rollback.note
                ),
            },
            Err(fallback) => ResidentExecutionBlocked {
                note: format!(
                    "[blocked] Git 仅提交契约未通过 [git-rollback-incomplete]: 后验验证失败且两级安全回滚均未完成 / validation failed and both rollback paths were incomplete\n验证失败: {}\n异步回滚失败: {}\n同步回滚失败: {}",
                    validation.note, rollback.note, fallback.note
                ),
            },
        }
    }

    pub(crate) fn recover_sync(&mut self) -> Result<(), ResidentExecutionBlocked> {
        self.rollback_owned_head_sync()?;
        self.restore_original_index_guarded()
    }

    pub(crate) fn rollback_owned_head_sync(&self) -> Result<(), ResidentExecutionBlocked> {
        let current = git_optional_text(&self.root, &["rev-parse", "--verify", "HEAD"])?;
        if current == self.before_head {
            return Ok(());
        }
        let Some(before) = self.before_head.as_deref() else {
            return Err(git_commit_blocked(
                "git-cancel-head-recovery-unverifiable",
                "取消时 HEAD 已变化且原 HEAD 不可用 / HEAD changed during cancellation and the original HEAD is unavailable",
            ));
        };
        let Some(current) = current.as_deref() else {
            return Err(git_commit_blocked(
                "git-cancel-head-recovery-unverifiable",
                "取消时 HEAD 消失,无法执行安全回滚 / HEAD disappeared during cancellation",
            ));
        };
        let Some(reference) = self.reference.as_deref() else {
            return Err(git_commit_blocked(
                "git-cancel-head-recovery-unverifiable",
                "取消时原分支引用不可用 / the original branch ref is unavailable during cancellation",
            ));
        };
        let Some(expected_tree) = self.expected_tree.as_deref() else {
            return Err(git_commit_blocked(
                "git-cancel-head-recovery-unverifiable",
                "取消时检测到新 HEAD,但事务尚未冻结预期 tree / a new HEAD exists but the transaction had not frozen its expected tree",
            ));
        };
        self.verify_current_head_owned(current)?;
        let parents = git_required_text(
            &self.root,
            &["rev-list", "--parents", "-n", "1", current],
            "git-cancel-head-recovery-unverifiable",
        )?;
        let parts = parents.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 2 || parts[0] != current || parts[1] != before {
            return Err(git_commit_blocked(
                "git-cancel-head-recovery-not-owned",
                "取消时的新 HEAD 不是原 HEAD 的单亲事务提交,拒绝回滚 / the new HEAD is not the transaction's single-parent child",
            ));
        }
        let current_tree = git_required_text(
            &self.root,
            &["rev-parse", &format!("{current}^{{tree}}")],
            "git-cancel-head-recovery-unverifiable",
        )?;
        if current_tree != expected_tree {
            return Err(git_commit_blocked(
                "git-cancel-head-recovery-tree-mismatch",
                "取消时的新提交 tree 与事务预计算值不同,拒绝回滚 / the new commit tree differs from the precomputed transaction tree",
            ));
        }
        let inert_hooks = InertHooksDirectory::create()?;
        let hooks_config = inert_hooks.config_value();
        let rollback = git_output(
            &self.root,
            &[
                "-c",
                &hooks_config,
                "update-ref",
                reference,
                before,
                current,
            ],
        )?;
        if !rollback.status.success() {
            return Err(git_command_failed(
                "git-cancel-head-recovery-cas-failed",
                "git update-ref",
                &rollback,
            ));
        }
        Ok(())
    }

    pub(crate) fn restore_original_index_guarded(&self) -> Result<(), ResidentExecutionBlocked> {
        if self.original_index.matches_current()? {
            return Ok(());
        }
        if !self.expected_current_index.matches_current()?
            && !self.expected_current_index.logically_matches_current()?
        {
            return Err(git_commit_blocked(
                "git-index-recovery-race",
                "Git index 已偏离本事务可证明状态,拒绝覆盖可能的外部改动 / the index diverged from the transaction-owned state; possible external changes were not overwritten",
            ));
        }
        self.original_index.restore()
    }

    pub(crate) fn record_recovery_warning(&self, error: &ResidentExecutionBlocked) {
        static WARNING_ID: AtomicU64 = AtomicU64::new(1);
        let runtime = self.root.join(".umadev");
        if std::fs::symlink_metadata(&runtime)
            .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            return;
        }
        let directory = runtime.join("recovery");
        if std::fs::create_dir_all(&directory).is_err() {
            return;
        }
        let id = WARNING_ID.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "git-transaction-{}-{id}.warning",
            std::process::id()
        ));
        let note = bounded_text(&error.note, 8_192);
        if let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(path) {
            let _ = file.write_all(note.as_bytes());
            let _ = file.sync_all();
        }
    }
}

impl Drop for GitTransactionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = self.recover_sync() {
            self.record_recovery_warning(&error);
        }
        self.armed = false;
    }
}
