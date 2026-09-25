use super::{
    configured_git_bool, git_command_failed, git_commit_blocked, git_mutating_output,
    git_mutating_output_with_input, git_output, git_stage_zero_entry, GitCommitBaseline,
    GitTransactionGuard, ResidentExecutionBlocked,
};
use std::path::Path;
use std::time::Duration;

pub(crate) async fn stage_paths_without_filters(
    root: &Path,
    paths: &[&str],
    hooks_config: &str,
    timeout: Duration,
    transaction: &mut GitTransactionGuard,
) -> Result<(), ResidentExecutionBlocked> {
    reject_index_info_paths(paths)?;
    reject_content_transforming_attributes(root, paths)?;
    // Like native Git, trust the working tree's executable bit only when
    // `core.fileMode` allows it (it is `false` on WSL `/mnt/c`, exFAT, SMB).
    let trust_executable_bit =
        cfg!(unix) && configured_git_bool(root, "core.fileMode")?.unwrap_or(true);
    let mut index_info = Vec::new();
    for path in paths {
        let Some(mode) = raw_index_mode(root, path, trust_executable_bit)? else {
            let zero = deletion_object_id(transaction)?;
            append_index_info_record(&mut index_info, "0", &zero, path);
            continue;
        };

        let hashed = if mode == "120000" {
            let target = symlink_target_bytes(root, path)?;
            git_mutating_output_with_input(
                root,
                &["hash-object", "-w", "--stdin"],
                &target,
                timeout,
                "git-hash-object-timeout",
                "git hash-object",
                transaction,
            )
            .await?
        } else {
            git_mutating_output(
                root,
                &["hash-object", "-w", "--no-filters", "--"],
                &[*path],
                timeout,
                "git-hash-object-timeout",
                "git hash-object",
                transaction,
            )
            .await?
        };
        if !hashed.status.success() {
            return Err(git_command_failed(
                "git-hash-object-failed",
                "git hash-object",
                &hashed,
            ));
        }
        let object = parse_object_id(&hashed.stdout, "git-hash-object-invalid")?;
        append_index_info_record(&mut index_info, &mode, &object, path);
    }

    // `update-index` owns its index lock for the whole NUL-delimited batch.
    // Nothing touches the real index until every path, mode, deletion, and blob
    // id above has been prepared successfully.
    let updated = git_mutating_output_with_input(
        root,
        &["-c", hooks_config, "update-index", "-z", "--index-info"],
        &index_info,
        timeout,
        "git-index-update-timeout",
        "git update-index",
        transaction,
    )
    .await?;
    if !updated.status.success() {
        return Err(git_command_failed(
            "git-index-update-failed",
            "git update-index",
            &updated,
        ));
    }
    transaction.observe_current_index()
}

fn reject_index_info_paths(paths: &[&str]) -> Result<(), ResidentExecutionBlocked> {
    if paths
        .iter()
        .any(|path| path.is_empty() || path.as_bytes().contains(&0))
    {
        return Err(git_commit_blocked(
            "git-index-info-path-invalid",
            "路径不能安全编码到 Git index-info / path cannot be safely encoded in Git index-info",
        ));
    }
    Ok(())
}

fn deletion_object_id(
    transaction: &GitTransactionGuard,
) -> Result<String, ResidentExecutionBlocked> {
    let head = transaction.before_head.as_deref().ok_or_else(|| {
        git_commit_blocked(
            "git-index-delete-unverifiable",
            "删除记录缺少可验证的原 HEAD / a deletion record requires a verifiable original HEAD",
        )
    })?;
    if !matches!(head.len(), 40 | 64) || !head.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(git_commit_blocked(
            "git-index-delete-unverifiable",
            "原 HEAD 的 object id 格式无效 / the original HEAD has an invalid object id",
        ));
    }
    Ok("0".repeat(head.len()))
}

fn append_index_info_record(payload: &mut Vec<u8>, mode: &str, object: &str, path: &str) {
    payload.extend_from_slice(mode.as_bytes());
    payload.push(b' ');
    payload.extend_from_slice(object.as_bytes());
    payload.push(b'\t');
    payload.extend_from_slice(path.as_bytes());
    payload.push(0);
}

pub(crate) async fn create_owned_commit_from_tree(
    root: &Path,
    baseline: &GitCommitBaseline,
    message: &str,
    hooks_config: &str,
    timeout: Duration,
    transaction: &mut GitTransactionGuard,
) -> Result<String, ResidentExecutionBlocked> {
    let tree = transaction.expected_tree.clone().ok_or_else(|| {
        git_commit_blocked(
            "git-expected-tree-unverifiable",
            "事务尚未冻结预期 tree / the transaction has no frozen expected tree",
        )
    })?;
    let parent = baseline.head.as_deref().ok_or_else(|| {
        git_commit_blocked(
            "git-unborn-branch-unsupported",
            "当前分支尚无初始提交 / an unborn branch is unsupported",
        )
    })?;
    let reference = baseline.symbolic_head.as_deref().ok_or_else(|| {
        git_commit_blocked(
            "git-detached-head",
            "当前分支引用不可用 / the current branch reference is unavailable",
        )
    })?;
    let identity_name = format!("user.name={}", baseline.identity_name);
    let identity_email = format!("user.email={}", baseline.identity_email);
    let created = git_mutating_output(
        root,
        &[
            "-c",
            hooks_config,
            "-c",
            &identity_name,
            "-c",
            &identity_email,
            "-c",
            "commit.gpgSign=false",
            "commit-tree",
            &tree,
            "-p",
            parent,
            "-m",
            message,
        ],
        &[],
        timeout,
        "git-commit-timeout",
        "git commit-tree",
        transaction,
    )
    .await?;
    if !created.status.success() {
        return Err(git_command_failed(
            "git-commit-failed",
            "git commit-tree",
            &created,
        ));
    }
    let created = parse_object_id(&created.stdout, "git-commit-not-created")?;
    let reflog = format!("{}: {message}", transaction.reflog_action());
    let updated = git_mutating_output(
        root,
        &[
            "-c",
            hooks_config,
            "-c",
            "core.logAllRefUpdates=true",
            "update-ref",
            "--create-reflog",
            "-m",
            &reflog,
            reference,
            &created,
            parent,
        ],
        &[],
        timeout,
        "git-update-ref-timeout",
        "git update-ref",
        transaction,
    )
    .await?;
    if !updated.status.success() {
        return Err(git_command_failed(
            "git-update-ref-failed",
            "git update-ref",
            &updated,
        ));
    }
    transaction.mark_owned_commit(created.clone())?;
    Ok(created)
}

fn symlink_target_bytes(root: &Path, path: &str) -> Result<Vec<u8>, ResidentExecutionBlocked> {
    let target = std::fs::read_link(root.join(path)).map_err(|error| {
        git_commit_blocked(
            "git-symlink-unverifiable",
            &format!("无法读取符号链接 `{path}` / unable to read symbolic link: {error}"),
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(target.as_os_str().as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        target
            .to_str()
            .map(|value| value.as_bytes().to_vec())
            .ok_or_else(|| {
                git_commit_blocked(
                    "git-symlink-unverifiable",
                    "符号链接目标不是有效 Unicode / symbolic-link target is not valid Unicode",
                )
            })
    }
}

fn raw_index_mode(
    root: &Path,
    path: &str,
    trust_executable_bit: bool,
) -> Result<Option<String>, ResidentExecutionBlocked> {
    let metadata = match std::fs::symlink_metadata(root.join(path)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(git_commit_blocked(
                "git-path-unverifiable",
                &format!("无法读取 `{path}` / unable to inspect path: {error}"),
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(Some("120000".to_string()));
    }
    if !metadata.is_file() {
        return Err(git_commit_blocked(
            "git-path-type-unsupported",
            &format!("`{path}` 不是普通文件或符号链接 / unsupported path type"),
        ));
    }
    #[cfg(unix)]
    if trust_executable_bit {
        use std::os::unix::fs::PermissionsExt as _;
        // Git records only the owner's executable bit.
        return Ok(Some(
            if metadata.permissions().mode() & 0o100 == 0 {
                "100644"
            } else {
                "100755"
            }
            .to_string(),
        ));
    }
    // Without a trusted executable bit Git keeps a regular file's recorded
    // mode and gives new files 100644.
    let executable = !trust_executable_bit
        && git_stage_zero_entry(root, path)?.is_some_and(|(mode, _)| mode == "100755");
    Ok(Some(
        if executable { "100755" } else { "100644" }.to_string(),
    ))
}

fn reject_content_transforming_attributes(
    root: &Path,
    paths: &[&str],
) -> Result<(), ResidentExecutionBlocked> {
    let mut args = vec![
        "check-attr",
        "-z",
        "filter",
        "ident",
        "text",
        "eol",
        "working-tree-encoding",
        "--",
    ];
    args.extend_from_slice(paths);
    let output = git_output(root, &args)?;
    if !output.status.success() {
        return Err(git_command_failed(
            "git-attributes-unverifiable",
            "git check-attr",
            &output,
        ));
    }
    let records = output.stdout.split(|byte| *byte == 0).collect::<Vec<_>>();
    for fields in records.chunks_exact(3) {
        let value = String::from_utf8_lossy(fields[2]);
        if !value.is_empty() && value != "unspecified" && value != "unset" {
            let path = String::from_utf8_lossy(fields[0]);
            let attribute = String::from_utf8_lossy(fields[1]);
            return Err(git_commit_blocked(
                "git-content-transformation-blocked",
                &format!(
                    "`{path}` 启用了 `{attribute}={value}`,普通仅提交事务拒绝隐式内容转换 / content-transforming attributes require native Git"
                ),
            ));
        }
    }
    Ok(())
}

fn parse_object_id(bytes: &[u8], code: &'static str) -> Result<String, ResidentExecutionBlocked> {
    let value = std::str::from_utf8(bytes)
        .map_err(|_| git_commit_blocked(code, "Git object id 不是 UTF-8"))?
        .trim();
    if !matches!(value.len(), 40 | 64) || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(git_commit_blocked(
            code,
            "Git 返回了无效 object id / Git returned an invalid object id",
        ));
    }
    Ok(value.to_string())
}
