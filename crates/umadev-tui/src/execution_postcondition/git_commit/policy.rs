use super::{
    create_owned_commit_from_tree, display_paths, expected_commit_tree, git_command_failed,
    git_commit_blocked, git_commit_message, git_committed_paths, git_count, git_dirty_paths,
    git_identity_config, git_mutating_output, git_name_only, git_operation_in_progress,
    git_optional_text, git_output, git_required_text, git_staged_paths,
    is_high_risk_git_commit_path, is_internal_umadev_path, normalize_exact_git_path,
    reject_active_commit_hooks, reject_commit_signing, reject_git_environment_redirects,
    snapshot_blocked, stage_paths_without_filters, validate_git_commit_scope, BTreeSet, Duration,
    ExecutionContract, GitCommitBaseline, GitIndexSnapshot, GitTransactionGuard,
    GitValidationRollback, InertHooksDirectory, Path, ResidentExecutionBlocked, WorkspaceBaseline,
};

impl GitCommitBaseline {
    pub(crate) fn capture(root: &Path) -> Result<Self, ResidentExecutionBlocked> {
        reject_git_environment_redirects()?;
        let inside = git_output(root, &["rev-parse", "--is-inside-work-tree"])?;
        if !inside.status.success() || String::from_utf8_lossy(&inside.stdout).trim() != "true" {
            return Err(git_commit_blocked(
                "git-worktree-unavailable",
                "当前目录不是可验证的 Git 工作树,不能执行仅提交契约 / the current directory is not a verifiable Git worktree",
            ));
        }
        let requested_root = std::fs::canonicalize(root).map_err(|error| {
            git_commit_blocked(
                "git-worktree-root-unverifiable",
                &format!("无法解析项目根目录 / unable to canonicalize the project root: {error}"),
            )
        })?;
        let top_level = git_required_text(
            root,
            &["rev-parse", "--show-toplevel"],
            "git-worktree-root-unverifiable",
        )?;
        let top_level = std::fs::canonicalize(&top_level).map_err(|error| {
            git_commit_blocked(
                "git-worktree-root-unverifiable",
                &format!(
                    "无法解析 Git 工作树根目录 / unable to canonicalize the Git worktree root: {error}"
                ),
            )
        })?;
        if requested_root != top_level {
            return Err(git_commit_blocked(
                "git-worktree-root-mismatch",
                &format!(
                    "UmaDev 项目根目录 `{}` 与 Git 工作树根目录 `{}` 不一致 / the host-only commit requires the project root to equal the Git worktree root",
                    requested_root.display(),
                    top_level.display()
                ),
            ));
        }
        // Reject executable hooks before any status/index command that might
        // refresh the index. A second check immediately before mutation closes
        // the ordinary preflight window; every mutation also overrides hooks.
        reject_active_commit_hooks(root)?;
        reject_commit_signing(root)?;
        let (identity_name, identity_email) = git_identity_config(root)?;
        Ok(Self {
            head: git_optional_text(root, &["rev-parse", "--verify", "HEAD"])?,
            symbolic_head: git_optional_text(root, &["symbolic-ref", "--quiet", "HEAD"])?,
            dirty_paths: git_dirty_paths(root)?,
            expected_paths: BTreeSet::new(),
            staged_only: false,
            index: GitIndexSnapshot::capture(root)?,
            identity_name,
            identity_email,
        })
    }

    pub(crate) fn with_contract(
        mut self,
        root: &Path,
        contract: &ExecutionContract,
    ) -> Result<Self, ResidentExecutionBlocked> {
        self.dirty_paths
            .retain(|path| !is_internal_umadev_path(path));
        self.staged_only =
            umadev_agent::router::request_uses_literal_git_commit_command(&contract.objective);
        if self.symbolic_head.is_none() {
            return Err(git_commit_blocked(
                "git-detached-head",
                "当前处于 detached HEAD,普通仅提交事务拒绝运行 / a normal commit-only transaction cannot run on detached HEAD",
            ));
        }
        let unmerged = git_name_only(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--name-only",
                "--diff-filter=U",
                "-z",
            ],
        )?;
        if !unmerged.is_empty() {
            return Err(git_commit_blocked(
                "git-unmerged-paths",
                &format!(
                    "工作区存在未解决冲突: {} / unresolved conflicts must be handled explicitly",
                    display_paths(&unmerged)
                ),
            ));
        }
        if let Some(operation) = git_operation_in_progress(root)? {
            return Err(git_commit_blocked(
                "git-operation-in-progress",
                &format!(
                    "仓库正在执行 {operation},普通仅提交事务拒绝介入 / an in-progress Git operation requires an explicit workflow"
                ),
            ));
        }
        reject_active_commit_hooks(root)?;
        if self.dirty_paths.is_empty() {
            return Err(git_commit_blocked(
                "git-nothing-to-commit",
                "当前没有可提交的工作区改动 / there are no workspace changes to commit",
            ));
        }
        self.expected_paths = if self.staged_only {
            git_staged_paths(root)?
        } else if contract.allowed_paths.is_empty() {
            self.dirty_paths.clone()
        } else {
            contract
                .allowed_paths
                .iter()
                .map(|path| normalize_exact_git_path(path, &self.dirty_paths))
                .collect::<Result<BTreeSet<_>, _>>()?
        };
        let internal = self
            .expected_paths
            .iter()
            .filter(|path| is_internal_umadev_path(path))
            .cloned()
            .collect::<Vec<_>>();
        if !internal.is_empty() {
            return Err(git_commit_blocked(
                "git-internal-path-blocked",
                &format!(
                    "UmaDev 运行时路径不会进入用户提交: {} / UmaDev runtime paths cannot be committed by this lane",
                    display_paths(&internal)
                ),
            ));
        }
        let requested_sensitive = self
            .expected_paths
            .iter()
            .filter(|path| is_high_risk_git_commit_path(path))
            .cloned()
            .collect::<Vec<_>>();
        if !requested_sensitive.is_empty() {
            return Err(git_commit_blocked(
                "git-sensitive-path-blocked",
                &format!(
                    "显式提交范围包含敏感路径: {} / the explicit commit scope contains sensitive paths",
                    display_paths(&requested_sensitive)
                ),
            ));
        }
        if self.expected_paths.is_empty() {
            return Err(git_commit_blocked(
                "git-scope-empty",
                "请求没有解析出可安全提交的精确文件路径 / no exact safe file paths were authorized",
            ));
        }
        Ok(self)
    }

    pub(crate) fn verify_pre_execution_state(
        &self,
        root: &Path,
    ) -> Result<(), ResidentExecutionBlocked> {
        let current_head = git_optional_text(root, &["rev-parse", "--verify", "HEAD"])?;
        let current_symbolic = git_optional_text(root, &["symbolic-ref", "--quiet", "HEAD"])?;
        let mut current_dirty = git_dirty_paths(root)?;
        current_dirty.retain(|path| !is_internal_umadev_path(path));
        if current_head != self.head
            || current_symbolic != self.symbolic_head
            || current_dirty != self.dirty_paths
        {
            return Err(git_commit_blocked(
                "git-preflight-state-changed",
                "提交基线冻结后 HEAD、分支或待提交集合发生了变化 / HEAD, branch, or the dirty path set changed after capture",
            ));
        }
        reject_active_commit_hooks(root)?;
        reject_commit_signing(root)?;
        self.index.verify_unchanged()
    }

    pub(crate) async fn execute(
        &self,
        root: &Path,
        objective: &str,
        timeout: Duration,
        transaction: &mut GitTransactionGuard,
    ) -> Result<String, ResidentExecutionBlocked> {
        if self.head.is_none() {
            return Err(git_commit_blocked(
                "git-unborn-branch-unsupported",
                "当前分支尚无初始提交,请先显式创建初始提交 / an unborn branch requires an explicit initial-commit workflow",
            ));
        }
        let message = git_commit_message(objective, self.staged_only)?;
        let inert_hooks = InertHooksDirectory::create()?;
        let hooks_config = inert_hooks.config_value();
        let paths = self
            .expected_paths
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        if !self.staged_only {
            stage_paths_without_filters(root, &paths, &hooks_config, timeout, transaction).await?;
        }
        transaction.expected_tree = Some(expected_commit_tree(root, self, &paths)?);
        create_owned_commit_from_tree(root, self, &message, &hooks_config, timeout, transaction)
            .await
    }

    pub(crate) fn validate(
        &self,
        root: &Path,
        baseline: &WorkspaceBaseline,
        contract: &ExecutionContract,
        expected_after: Option<&str>,
        expected_tree: Option<&str>,
    ) -> Result<Vec<String>, ResidentExecutionBlocked> {
        let after =
            git_optional_text(root, &["rev-parse", "--verify", "HEAD"])?.ok_or_else(|| {
                git_commit_blocked(
                    "git-commit-not-created",
                    "没有检测到新提交 / no new commit was created",
                )
            })?;
        if expected_after.is_some_and(|expected| expected != after) {
            return Err(git_commit_blocked(
                "git-head-changed-during-transaction",
                "Git 提交返回后 HEAD 又发生变化 / HEAD changed after the host commit completed",
            ));
        }
        if self.head.as_deref() == Some(after.as_str()) {
            return Err(git_commit_blocked(
                "git-commit-not-created",
                "HEAD 未前进,没有检测到新提交 / HEAD did not advance; no new commit was created",
            ));
        }
        let actual_tree = git_required_text(
            root,
            &["rev-parse", &format!("{after}^{{tree}}")],
            "git-commit-tree-unverifiable",
        )?;
        if expected_tree.is_some_and(|expected| expected != actual_tree) {
            return Err(git_commit_blocked(
                "git-commit-tree-mismatch",
                "新提交的 tree 与执行前冻结的精确内容不一致 / the new commit tree differs from the exact tree frozen before commit",
            ));
        }
        let symbolic_head = git_optional_text(root, &["symbolic-ref", "--quiet", "HEAD"])?;
        if symbolic_head != self.symbolic_head {
            return Err(git_commit_blocked(
                "git-branch-changed",
                "仅提交任务不得创建、切换或脱离当前分支 / a commit-only task must not create, switch, or detach from the current branch",
            ));
        }

        let count = if let Some(before) = &self.head {
            let ancestor = git_output(root, &["merge-base", "--is-ancestor", before, &after])?;
            if !ancestor.status.success() {
                return Err(git_commit_blocked(
                    "git-commit-history-diverged",
                    "新 HEAD 不是原 HEAD 的后代 / the new HEAD is not a descendant of the pre-turn HEAD",
                ));
            }
            git_count(root, &format!("{before}..{after}"))?
        } else {
            git_count(root, &after)?
        };
        if count != 1 {
            return Err(git_commit_blocked(
                "git-commit-count-invalid",
                &format!(
                    "仅提交任务必须且只能创建 1 个提交,实际检测到 {count} 个 / commit-only tasks must create exactly one commit; observed {count}"
                ),
            ));
        }

        let extra_changes = baseline.changed_paths(root).map_err(snapshot_blocked)?;
        if !extra_changes.is_empty() {
            return Err(git_commit_blocked(
                "git-only-content-modified",
                &format!(
                    "提交期间又修改了工作区内容,已拒绝把无关编辑当成提交成功: {} / workspace content changed after the commit baseline; unrelated edits cannot settle a commit-only request",
                    display_paths(&extra_changes)
                ),
            ));
        }

        let committed = git_committed_paths(root, &after)?;
        if committed.is_empty() {
            return Err(git_commit_blocked(
                "git-commit-empty",
                "新提交没有文件内容,不能证明请求的现有改动已提交 / the new commit contains no file changes",
            ));
        }
        let created_after_capture = committed
            .iter()
            .filter(|path| !self.dirty_paths.contains(path.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !created_after_capture.is_empty() {
            return Err(git_commit_blocked(
                "git-commit-created-content",
                &format!(
                    "新提交包含本轮开始时并不存在于待提交集合中的路径: {} / the commit contains paths that were not dirty, staged, or untracked when the turn started",
                    display_paths(&created_after_capture)
                ),
            ));
        }
        let committed_set = committed.iter().cloned().collect::<BTreeSet<_>>();
        if committed_set != self.expected_paths {
            let missing = self
                .expected_paths
                .difference(&committed_set)
                .cloned()
                .collect::<Vec<_>>();
            let unexpected = committed_set
                .difference(&self.expected_paths)
                .cloned()
                .collect::<Vec<_>>();
            return Err(git_commit_blocked(
                "git-commit-path-set-mismatch",
                &format!(
                    "提交文件集合与本轮授权不一致; missing: {}; unexpected: {} / committed paths must exactly match this turn's authorization",
                    display_paths(&missing),
                    display_paths(&unexpected)
                ),
            ));
        }
        let remaining = if self.staged_only {
            git_staged_paths(root)?
        } else {
            git_dirty_paths(root)?
        };
        let residual = remaining
            .intersection(&self.expected_paths)
            .cloned()
            .collect::<Vec<_>>();
        if !residual.is_empty() {
            return Err(git_commit_blocked(
                "git-requested-paths-still-dirty",
                &format!(
                    "已请求文件提交后仍有残留改动: {} / requested paths remain dirty after the commit",
                    display_paths(&residual)
                ),
            ));
        }
        validate_git_commit_scope(contract, &committed)?;
        Ok(committed)
    }

    pub(crate) async fn rollback_after_validation(
        &self,
        root: &Path,
        created: &str,
        validation: ResidentExecutionBlocked,
        timeout: Duration,
        transaction: &mut GitTransactionGuard,
    ) -> GitValidationRollback {
        let Some(before) = self.head.as_deref() else {
            return GitValidationRollback::NeedsFallback {
                validation,
                rollback: git_commit_blocked(
                    "git-rollback-unavailable",
                    "提交已创建但原 HEAD 不可用,无法回滚 / a commit was created but the old HEAD is unavailable",
                ),
            };
        };
        let Some(reference) = self.symbolic_head.as_deref() else {
            return GitValidationRollback::NeedsFallback {
                validation,
                rollback: git_commit_blocked(
                    "git-rollback-unavailable",
                    "提交已创建但原分支引用不可用,无法回滚 / the original branch ref is unavailable",
                ),
            };
        };
        if let Err(rollback) = transaction.verify_current_head_owned(created) {
            return GitValidationRollback::NeedsFallback {
                validation,
                rollback,
            };
        }
        let inert_hooks = match InertHooksDirectory::create() {
            Ok(inert_hooks) => inert_hooks,
            Err(rollback) => {
                return GitValidationRollback::NeedsFallback {
                    validation,
                    rollback,
                };
            }
        };
        let hooks_config = inert_hooks.config_value();
        let update = git_mutating_output(
            root,
            &[
                "-c",
                &hooks_config,
                "update-ref",
                reference,
                before,
                created,
            ],
            &[],
            timeout,
            "git-rollback-timeout",
            "git update-ref",
            transaction,
        )
        .await;
        let rollback_error = match update {
            Ok(output) if output.status.success() => None,
            Ok(output) => Some(git_command_failed(
                "git-rollback-ref-failed",
                "git update-ref",
                &output,
            )),
            Err(error) => Some(error),
        };
        if let Some(rollback) = rollback_error {
            return GitValidationRollback::NeedsFallback {
                validation,
                rollback,
            };
        }
        match transaction.restore_original_index_guarded() {
            Ok(()) => GitValidationRollback::Recovered(ResidentExecutionBlocked {
                note: format!(
                    "{}; [rollback-ok] 已用 CAS 恢复原 HEAD 和原始 index / original HEAD and exact index restored",
                    validation.note
                ),
            }),
            Err(restore) => GitValidationRollback::NeedsFallback {
                validation,
                rollback: restore,
            },
        }
    }
}
