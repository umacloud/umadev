//! Resident-turn execution-contract settlement.
//!
//! The facade captures workspace truth and delegates host-owned Git commit
//! transactions to small, independently auditable modules.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use umadev_agent::{
    request_is_git_commit, ExecutionContract, RoutePlan, WorkspaceBaseline, WorkspaceSnapshotError,
};

mod common;
mod git_commit;

pub(super) use common::*;
pub(super) use git_commit::*;

/// A content baseline paired with the routed turn's executable scope contract.
#[derive(Debug)]
pub(super) struct ResidentExecutionPostcondition {
    baseline: WorkspaceBaseline,
    contract: ExecutionContract,
    git_commit: Option<GitCommitBaseline>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GitCommitReceipt {
    pub(super) commit: String,
    pub(super) paths: Vec<String>,
}

impl GitCommitReceipt {
    pub(super) fn reply(&self) -> String {
        let short = self.commit.get(..12).unwrap_or(&self.commit);
        format!(
            "[ok] 已创建本地提交 {short}\n提交文件: {}",
            display_paths(&self.paths)
        )
    }
}

impl ResidentExecutionPostcondition {
    /// Capture the pre-writer tree and freeze the route-derived contract.
    pub(super) fn capture(
        root: &Path,
        route: &RoutePlan,
        objective: &str,
    ) -> Result<Self, ResidentExecutionBlocked> {
        let baseline =
            WorkspaceBaseline::capture(root).map_err(|error| snapshot_blocked_at(root, error))?;
        let contract = ExecutionContract::from_route(route, objective);
        let git_commit = (route.class.mutates_workspace() && request_is_git_commit(objective))
            .then(|| GitCommitBaseline::capture(root))
            .transpose()?;
        let git_commit = git_commit
            .map(|baseline| baseline.with_contract(root, &contract))
            .transpose()?;
        Ok(Self {
            baseline,
            contract,
            git_commit,
        })
    }

    /// Current changed paths since the pre-writer baseline.
    ///
    /// This is a non-terminal observation only; callers that can run another base
    /// or UmaDev-owned tool afterward must use [`Self::validate_final`] at settlement.
    pub(super) fn changed_paths(
        &self,
        root: &Path,
    ) -> Result<Vec<String>, ResidentExecutionBlocked> {
        self.baseline.changed_paths(root).map_err(snapshot_blocked)
    }

    /// Validate the final tree after every base and UmaDev-owned execution turn.
    pub(super) fn validate_final(
        &self,
        root: &Path,
    ) -> Result<Vec<String>, ResidentExecutionBlocked> {
        if let Some(git_commit) = &self.git_commit {
            return git_commit.validate(root, &self.baseline, &self.contract, None, None);
        }
        let changed = self.changed_paths(root)?;
        validate_contract_paths(&self.contract, &changed)?;
        Ok(changed)
    }

    /// Execute a commit-only request in the host. No command is delegated to the
    /// selected AI base, so its tool-event timing cannot weaken this boundary.
    pub(super) async fn execute_git_commit(
        &self,
        root: &Path,
        objective: &str,
    ) -> Result<GitCommitReceipt, ResidentExecutionBlocked> {
        self.execute_git_commit_with_timeout(root, objective, git_mutation_timeout())
            .await
    }

    async fn execute_git_commit_with_timeout(
        &self,
        root: &Path,
        objective: &str,
        timeout: Duration,
    ) -> Result<GitCommitReceipt, ResidentExecutionBlocked> {
        let baseline = self.git_commit.as_ref().ok_or_else(|| {
            git_commit_blocked(
                "git-transaction-unavailable",
                "当前路由不是可执行的 Git 仅提交事务 / this route is not an executable commit-only transaction",
            )
        })?;
        let changed_before_execution = self
            .baseline
            .changed_paths(root)
            .map_err(snapshot_blocked)?;
        if !changed_before_execution.is_empty() {
            return Err(git_commit_blocked(
                "git-preflight-content-changed",
                &format!(
                    "提交基线冻结后工作区又发生了变化: {} / workspace content changed after the commit baseline was captured",
                    display_paths(&changed_before_execution)
                ),
            ));
        }
        baseline.verify_pre_execution_state(root)?;
        let mut transaction = GitTransactionGuard::new(root, baseline);
        let commit = match baseline
            .execute(root, objective, timeout, &mut transaction)
            .await
        {
            Ok(commit) => commit,
            Err(failure) => return Err(transaction.finish_failure(failure)),
        };
        match baseline.validate(
            root,
            &self.baseline,
            &self.contract,
            Some(commit.as_str()),
            transaction.expected_tree.as_deref(),
        ) {
            Ok(paths) => {
                transaction.disarm();
                Ok(GitCommitReceipt { commit, paths })
            }
            Err(validation) => {
                match baseline
                    .rollback_after_validation(root, &commit, validation, timeout, &mut transaction)
                    .await
                {
                    GitValidationRollback::Recovered(failure) => {
                        transaction.disarm();
                        Err(failure)
                    }
                    GitValidationRollback::NeedsFallback {
                        validation,
                        rollback,
                    } => Err(transaction.finish_validation_rollback_failure(validation, rollback)),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
