use super::{
    configured_git_bool, configured_git_path, display_paths, git_command_failed,
    git_commit_blocked, git_output, git_output_without_filter_programs, git_required_text,
    BTreeSet, ExecutionContract, Path, PathBuf, ResidentExecutionBlocked,
};
use umadev_agent::{parse_git_commit_intent, GitCommitIntent};

pub(crate) fn validate_contract_paths(
    contract: &ExecutionContract,
    paths: &[String],
) -> Result<(), ResidentExecutionBlocked> {
    let violations = contract.validate_changed_paths(paths.iter().map(String::as_str));
    if violations.is_empty() {
        return Ok(());
    }
    let details = violations
        .iter()
        .map(|violation| format!("- [{}] {}", violation.code, violation.message))
        .collect::<Vec<_>>()
        .join("\n");
    Err(ResidentExecutionBlocked {
        note: format!(
            "[blocked] 执行契约未通过,本轮不能标记成功 / execution contract failed; \
             this turn cannot be marked successful:\n{details}"
        ),
    })
}

pub(crate) fn validate_git_commit_scope(
    contract: &ExecutionContract,
    paths: &[String],
) -> Result<(), ResidentExecutionBlocked> {
    if contract.allowed_paths.is_empty() {
        return Ok(());
    }
    let mut scoped = contract.clone();
    scoped.max_changed_files = scoped.max_changed_files.max(paths.len());
    validate_contract_paths(&scoped, paths)
}

pub(crate) fn is_high_risk_git_commit_path(path: &str) -> bool {
    let normalized = path.trim_start_matches("./").replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    let basename = lower.rsplit('/').next().unwrap_or(lower.as_str());
    let sensitive_extension = Path::new(basename)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension,
                "pem" | "key" | "p12" | "pfx" | "ppk" | "jks" | "keystore" | "kdb" | "kdbx"
            )
        });
    let env_secret = basename == ".env"
        || (basename.starts_with(".env.")
            && !matches!(basename, ".env.example" | ".env.sample" | ".env.template"));
    env_secret
        || matches!(
            basename,
            ".envrc"
                | ".npmrc"
                | ".pypirc"
                | ".netrc"
                | ".git-credentials"
                | "credentials"
                | "credentials.json"
                | "application_default_credentials.json"
                | "service-account.json"
                | "service_account.json"
                | "secrets.json"
                | "secrets.yaml"
                | "secrets.yml"
                | "secrets.toml"
                | ".dockerconfigjson"
                | "id_rsa"
                | "id_dsa"
                | "id_ecdsa"
                | "id_ed25519"
        )
        || sensitive_extension
        || (basename.starts_with("id_")
            && !Path::new(basename)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pub")))
        || lower.starts_with(".ssh/")
        || lower.contains("/.ssh/")
        || path_is_or_ends_with(&lower, ".aws/credentials")
        || path_is_or_ends_with(&lower, ".aws/config")
        || path_is_or_ends_with(&lower, ".docker/config.json")
        || path_is_or_ends_with(&lower, ".kube/config")
        || path_is_or_ends_with(
            &lower,
            ".config/gcloud/application_default_credentials.json",
        )
        || path_is_or_ends_with(&lower, ".azure/accesstokens.json")
        || path_is_or_ends_with(&lower, ".config/gh/hosts.yml")
        || path_is_or_ends_with(&lower, ".config/glab-cli/config.yml")
}

pub(crate) fn path_is_or_ends_with(path: &str, suffix: &str) -> bool {
    path == suffix || path.ends_with(&format!("/{suffix}"))
}

pub(crate) fn is_internal_umadev_path(path: &str) -> bool {
    let normalized = path.trim_start_matches("./").replace('\\', "/");
    normalized == ".umadev" || normalized.starts_with(".umadev/")
}

pub(crate) fn normalize_exact_git_path(
    path: &str,
    dirty_paths: &BTreeSet<String>,
) -> Result<String, ResidentExecutionBlocked> {
    let normalized = path.trim().trim_matches(['\'', '"']).replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    let unsafe_path = normalized.is_empty()
        || normalized == "."
        || normalized.starts_with('/')
        || normalized.split('/').any(|part| part == "..")
        || normalized.starts_with(':')
        || normalized.contains(['*', '?', '[', ']']);
    if unsafe_path || !dirty_paths.contains(normalized) {
        return Err(git_commit_blocked(
            "git-scope-not-exact-dirty-path",
            &format!(
                "`{path}` 不是本轮开始时的精确待提交文件路径 / scope must name an exact path from the pre-turn dirty set"
            ),
        ));
    }
    Ok(normalized.to_string())
}

pub(crate) fn git_commit_message(
    objective: &str,
    literal_command: bool,
) -> Result<String, ResidentExecutionBlocked> {
    const DEFAULT: &str = "chore: record current changes";
    let lower = objective.to_ascii_lowercase();
    if literal_command {
        return match parse_git_commit_intent(objective) {
            GitCommitIntent::LiteralCommand(spec) => validate_git_commit_message(
                spec.message.unwrap_or_else(|| DEFAULT.to_string()),
            ),
            GitCommitIntent::UnsupportedLiteralCommand => Err(git_commit_blocked(
                "git-commit-option-forbidden",
                "普通仅提交事务只支持 `git commit -m/--message <message>` / unsupported commit argument",
            )),
            GitCommitIntent::NotCommit
            | GitCommitIntent::NaturalAllDirty
            | GitCommitIntent::NaturalPaths(_)
            | GitCommitIntent::InvalidNaturalScope => {
                Err(git_commit_blocked(
                    "git-commit-command-unverifiable",
                    "字面 Git 提交请求缺少可验证命令 / literal Git commit request has no verifiable command",
                ))
            }
        };
    }
    for marker in ["提交说明", "提交信息", "提交消息", "commit message"] {
        if let Some(index) = lower.find(marker) {
            let tail = objective[index + marker.len()..]
                .trim_start_matches(|character: char| {
                    character.is_whitespace() || matches!(character, ':' | '：' | '=' | '为' | '是')
                })
                .trim()
                .trim_matches(['\'', '"']);
            if !tail.is_empty() {
                return validate_git_commit_message(tail.to_string());
            }
        }
    }
    validate_git_commit_message(DEFAULT.to_string())
}

pub(crate) fn validate_git_commit_message(
    message: String,
) -> Result<String, ResidentExecutionBlocked> {
    const MAX_MESSAGE_CHARS: usize = 4_096;
    const MAX_MESSAGE_BYTES: usize = 16_384;
    if message.chars().count() > MAX_MESSAGE_CHARS || message.len() > MAX_MESSAGE_BYTES {
        return Err(git_commit_blocked(
            "git-commit-message-too-long",
            "提交说明超过 4096 字符上限 / commit message exceeds the 4096-character limit",
        ));
    }
    if message
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err(git_commit_blocked(
            "git-commit-message-control-character",
            "提交说明包含不允许的控制字符 / commit message contains a disallowed control character",
        ));
    }
    Ok(message)
}

pub(crate) fn git_name_only(
    root: &Path,
    args: &[&str],
) -> Result<Vec<String>, ResidentExecutionBlocked> {
    let output = git_output_without_filter_programs(root, args)?;
    if !output.status.success() {
        return Err(git_command_failed("git-state-unverifiable", "git", &output));
    }
    output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path)
                .map(str::to_string)
                .map_err(|_| git_commit_blocked("git-state-unverifiable", "Git 路径不是 UTF-8"))
        })
        .collect()
}

pub(crate) fn git_operation_in_progress(
    root: &Path,
) -> Result<Option<&'static str>, ResidentExecutionBlocked> {
    for (name, marker) in [
        ("merge", "MERGE_HEAD"),
        ("cherry-pick", "CHERRY_PICK_HEAD"),
        ("revert", "REVERT_HEAD"),
        ("rebase", "rebase-merge"),
        ("rebase", "rebase-apply"),
        ("sequencer", "sequencer"),
    ] {
        let path = git_required_text(
            root,
            &["rev-parse", "--git-path", marker],
            "git-path-unverifiable",
        )?;
        let path = Path::new(&path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        if path.exists() {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

pub(crate) fn reject_active_commit_hooks(root: &Path) -> Result<(), ResidentExecutionBlocked> {
    // This lane may stage paths, refresh the index, create a commit/ref, and let
    // Git run automatic maintenance. Every hook reachable from those actions is
    // refused rather than bypassed, because hooks are allowed to edit the worktree.
    const COMMIT_HOOKS: [&str; 7] = [
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
        "post-index-change",
        "reference-transaction",
        "pre-auto-gc",
    ];
    let configured_hooks = configured_git_path(root, "core.hooksPath")?;
    if configured_hooks
        .as_deref()
        .is_some_and(configured_hooks_path_is_inert)
    {
        return Ok(());
    }
    let configured_hooks = configured_hooks.map(|path| {
        if path.is_absolute() {
            path
        } else {
            root.join(path)
        }
    });
    let default_hooks = if configured_hooks.is_none() {
        let raw = git_required_text(
            root,
            &["rev-parse", "--git-common-dir"],
            "git-hooks-unverifiable",
        )?;
        let path = PathBuf::from(raw);
        Some(
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
            .join("hooks"),
        )
    } else {
        None
    };
    let mut active = Vec::new();
    for name in COMMIT_HOOKS {
        let path = if let Some(directory) = &configured_hooks {
            directory.join(name)
        } else {
            default_hooks
                .as_ref()
                .expect("default hook directory exists without core.hooksPath")
                .join(name)
        };
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(git_commit_blocked(
                    "git-hooks-unverifiable",
                    &format!(
                        "无法核对 Git hook `{name}` / unable to inspect Git hook `{name}`: {error}"
                    ),
                ));
            }
        };
        if metadata.is_file() && hook_is_executable(&metadata.permissions()) {
            active.push(name.to_string());
        }
    }
    if active.is_empty() {
        Ok(())
    } else {
        Err(git_commit_blocked(
            "git-active-hooks-require-native-git",
            &format!(
                "检测到本次提交会执行的 hook: {}; host-only 事务不会运行可能改写工作区的 hook。请先处理这些 hook,或使用原生 Git 明确执行 / active commit hooks can modify the worktree, so this host-only transaction was refused before staging; handle the hooks or use native Git explicitly",
                display_paths(&active)
            ),
        ))
    }
}

/// The lane cannot run the user's signing program (it could be anything the
/// configuration names), and an unsigned commit reported as success is only
/// discovered when a protected branch rejects the push. Refuse up front, the
/// same way active hooks are refused.
pub(crate) fn reject_commit_signing(root: &Path) -> Result<(), ResidentExecutionBlocked> {
    if configured_git_bool(root, "commit.gpgSign")? == Some(true) {
        return Err(git_commit_blocked(
            "git-signing-requires-native-git",
            "检测到 commit.gpgSign=true;host-only 事务不会运行签名程序,也不会创建未签名的提交。请用原生 Git 提交(例如 `git commit`) / commit signing is enabled (commit.gpgSign=true); this host-only transaction never runs a signing program and will not create an unsigned commit, so it was refused before staging; commit with native Git instead",
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
fn configured_hooks_path_is_inert(path: &Path) -> bool {
    path == Path::new("/dev/null")
}

#[cfg(windows)]
fn configured_hooks_path_is_inert(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case("NUL")
}

#[cfg(unix)]
pub(crate) fn hook_is_executable(permissions: &std::fs::Permissions) -> bool {
    use std::os::unix::fs::PermissionsExt;
    permissions.mode() & 0o111 != 0
}

#[cfg(not(unix))]
pub(crate) fn hook_is_executable(_permissions: &std::fs::Permissions) -> bool {
    true
}

pub(crate) fn git_optional_text(
    root: &Path,
    args: &[&str],
) -> Result<Option<String>, ResidentExecutionBlocked> {
    let output = git_output(root, args)?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).map_err(|_| {
        git_commit_blocked(
            "git-output-invalid",
            "Git 返回了非 UTF-8 引用,无法可靠验证提交 / Git returned a non-UTF-8 ref",
        )
    })?;
    let value = value.trim();
    Ok((!value.is_empty()).then(|| value.to_string()))
}

pub(crate) fn git_count(root: &Path, range: &str) -> Result<usize, ResidentExecutionBlocked> {
    let output = git_output(root, &["rev-list", "--count", range])?;
    if !output.status.success() {
        return Err(git_commit_blocked(
            "git-history-unverifiable",
            "无法计算本轮新增提交数 / unable to count commits created by this turn",
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|_| {
            git_commit_blocked(
                "git-history-unverifiable",
                "Git 返回了无效的提交计数 / Git returned an invalid commit count",
            )
        })
}

pub(crate) fn git_staged_paths(root: &Path) -> Result<BTreeSet<String>, ResidentExecutionBlocked> {
    Ok(git_name_only(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--cached",
            "--name-only",
            "--no-renames",
            "-z",
        ],
    )?
    .into_iter()
    .collect())
}

pub(crate) fn git_committed_paths(
    root: &Path,
    commit: &str,
) -> Result<Vec<String>, ResidentExecutionBlocked> {
    let output = git_output(
        root,
        &[
            "diff-tree",
            "--no-ext-diff",
            "--root",
            "--no-commit-id",
            "--name-only",
            "--no-renames",
            "-r",
            "-z",
            commit,
        ],
    )?;
    if !output.status.success() {
        return Err(git_commit_blocked(
            "git-commit-paths-unverifiable",
            "无法读取新提交的文件清单 / unable to read the new commit's file list",
        ));
    }
    let mut paths = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            std::str::from_utf8(path).map(str::to_string).map_err(|_| {
                git_commit_blocked(
                    "git-commit-path-invalid",
                    "新提交包含非 UTF-8 路径,无法执行路径契约 / the new commit contains a non-UTF-8 path",
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

pub(crate) fn git_dirty_paths(root: &Path) -> Result<BTreeSet<String>, ResidentExecutionBlocked> {
    let output = git_output_without_filter_programs(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    if !output.status.success() {
        return Err(git_commit_blocked(
            "git-status-unverifiable",
            "无法读取提交前的待提交路径 / unable to read the pre-turn dirty path set",
        ));
    }
    let records = output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .collect::<Vec<_>>();
    let mut paths = BTreeSet::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if record.len() < 4 || record[2] != b' ' {
            return Err(git_commit_blocked(
                "git-status-invalid",
                "Git 返回了无法解析的 porcelain 状态 / Git returned malformed porcelain status",
            ));
        }
        insert_git_path(&mut paths, &record[3..])?;
        let renamed = matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C');
        if renamed {
            index += 1;
            let source = records.get(index).ok_or_else(|| {
                git_commit_blocked(
                    "git-status-invalid",
                    "Git rename 状态缺少原路径 / Git rename status omitted its source path",
                )
            })?;
            insert_git_path(&mut paths, source)?;
        }
        index += 1;
    }
    Ok(paths)
}

pub(crate) fn insert_git_path(
    paths: &mut BTreeSet<String>,
    raw: &[u8],
) -> Result<(), ResidentExecutionBlocked> {
    let path = std::str::from_utf8(raw).map_err(|_| {
        git_commit_blocked(
            "git-status-path-invalid",
            "待提交集合包含非 UTF-8 路径,无法安全核对 / the dirty path set contains a non-UTF-8 path",
        )
    })?;
    paths.insert(path.to_string());
    Ok(())
}
