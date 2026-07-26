mod literal;
mod natural;
mod quote;
mod scope;

use literal::{literal_git_commit_policy, literal_git_commit_tail};
use natural::{
    find_safe_git_receipt_suffix, natural_git_commit_prefix,
    natural_git_commit_tail_is_safe_constraint, parse_paths, strip_git_commit_politeness,
    trim_natural_commit_tail,
};
use quote::{QuoteEvent, QuoteTracker};
pub(super) use scope::{git_commit_control_text, git_commit_scope_text, unquoted_lowercase_text};

/// The lossless, host-consumable shape of a Git commit request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitCommitIntent {
    /// The request does not unambiguously ask UmaDev to create a Git commit.
    NotCommit,
    /// A staged-only literal command and its optional message.
    LiteralCommand(LiteralGitCommitSpec),
    /// A literal command contains a forbidden option or malformed argument.
    UnsupportedLiteralCommand,
    /// A natural-language request asks to commit the complete dirty set.
    NaturalAllDirty,
    /// A natural-language request names an exact set of paths.
    NaturalPaths(Vec<String>),
    /// A scoped natural request could not be parsed losslessly.
    InvalidNaturalScope,
}

/// The only literal command shape the resident transaction executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiteralGitCommitSpec {
    /// Optional user-supplied commit message from `-m/--message`.
    pub message: Option<String>,
}

/// One exact, host-owned verification that may follow an ordinary Git commit.
///
/// The variants are deliberately closed: callers must never turn the suffix
/// back into shell text or delegate it to a model. [`GitVerifier::ProjectTests`] means the
/// host may resolve the repository's canonical test entry point from trusted
/// project metadata; every other variant maps to one fixed executable/argument
/// shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitVerifier {
    /// Run the repository's canonical test entry point, resolved by the host.
    ProjectTests,
    /// `cargo test`
    CargoTest,
    /// `cargo check`
    CargoCheck,
    /// `cargo clippy`
    CargoClippy,
    /// `npm test`
    NpmTest,
    /// `pnpm test`
    PnpmTest,
    /// `yarn test`
    YarnTest,
    /// `pytest`
    Pytest,
    /// `go test`
    GoTest,
    /// `mvn test`
    MavenTest,
    /// `mvn verify`
    MavenVerify,
}

/// A request the host can execute without asking the base model to interpret
/// Git or shell commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostGitCommitRequest {
    /// The lossless ordinary-commit prefix, excluding the verifier clause.
    ///
    /// Keeping this text separate preserves exact natural-language path scope
    /// and literal `-m/--message` parsing through [`parse_git_commit_intent`].
    pub commit_text: String,
    /// At most one closed, mechanical verification.
    pub verifier: Option<GitVerifier>,
}

/// Parse an ordinary commit, optionally followed by one exact mechanical
/// verifier, into a host-owned request.
///
/// Splitting is quote-aware, so words such as `and` or `然后` inside a commit
/// message never become an action boundary. Shell control operators,
/// redirections, substitutions, newlines, unmatched quotes, additional actions,
/// and verifier arguments are rejected rather than handed to a shell or model.
#[must_use]
pub fn parse_host_git_commit_request(requirement: &str) -> Option<HostGitCommitRequest> {
    let requirement = requirement.trim();
    if requirement.is_empty() || has_unsafe_unquoted_git_connector(requirement)? {
        return None;
    }

    if request_is_git_commit(requirement) {
        return Some(HostGitCommitRequest {
            commit_text: requirement.to_string(),
            verifier: None,
        });
    }

    let mut parsed = None;
    for (index, separator_len) in unquoted_verification_boundaries(requirement)? {
        let commit_text = trim_git_clause(&requirement[..index]);
        let verifier_text = trim_git_clause(&requirement[index + separator_len..]);
        if !request_is_git_commit(commit_text) {
            continue;
        }
        let Some(verifier) = parse_exact_git_verifier(verifier_text) else {
            continue;
        };
        let candidate = HostGitCommitRequest {
            commit_text: commit_text.to_string(),
            verifier: Some(verifier),
        };
        if parsed.replace(candidate).is_some() {
            // More than one valid boundary is ambiguous. Fail closed instead of
            // choosing which clause the user intended as the commit prefix.
            return None;
        }
    }
    parsed
}

/// Parse one request into the shared Git commit policy used by routing and host
/// execution.
#[must_use]
pub fn parse_git_commit_intent(requirement: &str) -> GitCommitIntent {
    let literal_text = strip_git_commit_politeness(requirement.trim());
    if literal_git_commit_tail(literal_text).is_some() {
        return match literal_git_commit_policy(literal_text) {
            Ok(spec) => GitCommitIntent::LiteralCommand(spec),
            Err(()) => GitCommitIntent::UnsupportedLiteralCommand,
        };
    }

    let scope_text = git_commit_scope_text(requirement);
    let command = strip_git_commit_politeness(&scope_text);
    let command_lower = command.to_lowercase();
    let Some((prefix_len, requires_scope, generic_prefix)) =
        natural_git_commit_prefix(&command_lower)
    else {
        return GitCommitIntent::NotCommit;
    };
    let tail = trim_natural_commit_tail(&command[prefix_len..]);
    if tail.is_empty() || (!requires_scope && natural_git_commit_tail_is_safe_constraint(tail)) {
        return if requires_scope {
            GitCommitIntent::InvalidNaturalScope
        } else {
            GitCommitIntent::NaturalAllDirty
        };
    }
    match parse_paths(tail) {
        Some(paths) if !paths.is_empty() => GitCommitIntent::NaturalPaths(paths),
        _ if generic_prefix => GitCommitIntent::NotCommit,
        _ => GitCommitIntent::InvalidNaturalScope,
    }
}

/// Whether the current user text itself explicitly confirms an ordinary commit.
///
/// This is intentionally evaluated only on the raw current turn by the TUI. A
/// confirmation remembered from an older base question must never authorize a
/// later mutation.
#[must_use]
pub fn request_explicitly_confirms_git_commit(requirement: &str) -> bool {
    if !request_is_git_commit(requirement) {
        return false;
    }
    let scope_text = git_commit_scope_text(requirement);
    let command = strip_git_commit_politeness(&scope_text);
    ["确认提交", "確認提交", "确定提交", "確定提交"]
        .iter()
        .any(|prefix| {
            command.strip_prefix(prefix).is_some_and(|tail| {
                let tail = trim_natural_commit_tail(tail);
                tail.is_empty() || natural_git_commit_tail_is_safe_constraint(tail)
            })
        })
}

/// Whether this turn explicitly asks to create an ordinary Git commit.
#[must_use]
pub fn request_is_git_commit(requirement: &str) -> bool {
    let q = git_commit_control_text(requirement);
    if q.is_empty() {
        return false;
    }
    let compact: String = q.chars().filter(|c| !c.is_whitespace()).collect();
    let intent = parse_git_commit_intent(requirement);
    if git_commit_request_is_question_or_negated(&q, &compact)
        || request_is_unsupported_git_commit(requirement)
        || matches!(
            &intent,
            GitCommitIntent::NotCommit
                | GitCommitIntent::UnsupportedLiteralCommand
                | GitCommitIntent::InvalidNaturalScope
        )
    {
        return false;
    }
    !matches!(&intent, GitCommitIntent::LiteralCommand(_))
        || !git_commit_request_has_additional_work(&q, &compact)
}

/// Whether an ordinary commit request uses literal `git commit ...` syntax.
#[must_use]
pub fn request_uses_literal_git_commit_command(requirement: &str) -> bool {
    request_is_git_commit(requirement)
        && matches!(
            parse_git_commit_intent(requirement),
            GitCommitIntent::LiteralCommand(_)
        )
}

/// Whether a request uses a literal option outside the ordinary new-commit
/// contract or explicitly asks to rewrite history.
#[must_use]
pub fn request_is_unsupported_git_commit(requirement: &str) -> bool {
    let q = git_commit_control_text(requirement);
    let compact: String = q.chars().filter(|c| !c.is_whitespace()).collect();
    matches!(
        parse_git_commit_intent(requirement),
        GitCommitIntent::UnsupportedLiteralCommand
    ) || [
        "修改上次提交",
        "修改上個提交",
        "修改上一个提交",
        "修订上次提交",
        "修訂上次提交",
        "重写上次提交",
        "重寫上次提交",
        "重写提交历史",
        "重寫提交歷史",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
        || [
            "amend the last commit",
            "amend last commit",
            "amend the previous commit",
            "rewrite the last commit",
            "rewrite commit history",
        ]
        .iter()
        .any(|needle| q.contains(needle))
}

/// Whether the current turn asks UmaDev to perform a Git commit operation,
/// including a compound or unsupported shape that the host must refuse.
///
/// This is wider than [`request_is_git_commit`], which recognizes only the
/// ordinary transaction the host can execute. It deliberately excludes
/// questions, negations, status reports, and diagnostics so those remain normal
/// read-only conversation. Callers use this predicate solely as a delegation
/// firewall: commit-shaped mutations that do not parse as
/// [`HostGitCommitRequest`] must never fall through to an AI base.
#[must_use]
pub fn request_has_git_commit_operation(requirement: &str) -> bool {
    let q = git_commit_control_text(requirement);
    if q.is_empty() {
        return false;
    }
    let compact: String = q.chars().filter(|c| !c.is_whitespace()).collect();
    if git_commit_request_is_question_or_negated(&q, &compact)
        || request_is_git_commit_diagnostic(requirement)
    {
        return false;
    }
    if !matches!(
        parse_git_commit_intent(requirement),
        GitCommitIntent::NotCommit
    ) {
        return true;
    }

    q.contains("git commit")
        || [
            "提交git记录",
            "提交git紀錄",
            "提交git纪录",
            "提交git",
            "git提交",
            "确认提交",
            "確認提交",
            "确定提交",
            "確定提交",
            "创建一个提交",
            "創建一個提交",
            "建立一个提交",
            "建立一個提交",
            "提交后推送",
            "提交後推送",
            "提交然后推送",
            "提交然後推送",
        ]
        .iter()
        .any(|needle| compact.contains(needle))
        || [
            "commit these changes",
            "commit the changes",
            "commit current changes",
            "commit all changes",
            "commit my changes",
            "make a commit",
            "create a commit",
        ]
        .iter()
        .any(|needle| q.contains(needle))
}

pub(super) fn git_commit_request_has_additional_work(q: &str, compact: &str) -> bool {
    let commit_context = q.contains("commit") || compact.contains("提交");
    if !commit_context {
        return false;
    }
    let mut english = q.trim_start();
    while let Some(rest) = ["please,", "please ", "now "]
        .iter()
        .find_map(|prefix| english.strip_prefix(prefix))
    {
        english = rest.trim_start();
    }
    if let Some(rest) = english.strip_prefix("go ahead and ") {
        english = rest.trim_start();
    }
    if q.contains("&&") || q.contains("||") || q.contains(';') {
        return true;
    }
    if find_safe_git_receipt_suffix(q)
        .is_some_and(|index| !git_receipt_prefix_has_additional_work(&q[..index]))
    {
        return false;
    }
    [
        "然后", "然後", "并且", "並且", "同时", "同時", "接着", "接著",
    ]
    .iter()
    .any(|separator| compact.contains(separator))
        || [
            "提交后",
            "提交後",
            "提交之后",
            "提交之後",
            "提交完成后",
            "提交完成後",
            "提交完后",
            "提交完後",
            "然后运行",
            "然後運行",
            "然后测试",
            "然後測試",
            "然后修改",
            "然後修改",
            "然后修复",
            "然後修復",
            "然后推送",
            "然後推送",
            "并运行",
            "並運行",
            "并测试",
            "並測試",
            "并修改",
            "並修改",
            "并修复",
            "並修復",
            "并推送",
            "並推送",
            "再运行",
            "再運行",
            "再测试",
            "再測試",
            "再修改",
            "再修复",
            "再修復",
            "再推送",
            "运行测试",
            "運行測試",
            "跑测试",
            "跑測試",
            "修改文件",
            "編輯文件",
            "编辑文件",
            "新增文件",
            "删除文件",
            "刪除文件",
        ]
        .iter()
        .any(|needle| compact.contains(needle))
        || [
            " then ",
            " and run ",
            " and test ",
            " and modify ",
            " and edit ",
            " and fix ",
            " and push ",
            "run tests",
            "run the tests",
            "modify files",
            "edit files",
            "fix files",
            "push after",
        ]
        .iter()
        .any(|needle| english.contains(needle))
        || english.contains(" and ")
        || english.contains(" then ")
        || ((compact.contains("修复") || compact.contains("修復"))
            && !compact.starts_with("提交")
            && !compact.starts_with("git提交"))
        || ((q.starts_with("fix ") || q.starts_with("modify ") || q.starts_with("edit "))
            && q.contains("commit"))
}

fn parse_exact_git_verifier(suffix: &str) -> Option<GitVerifier> {
    let normalized = suffix
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    match normalized.as_str() {
        "运行测试" | "運行測試" | "执行测试" | "執行測試" | "跑测试" | "跑測試" | "run tests"
        | "run the tests" => Some(GitVerifier::ProjectTests),
        "cargo test" => Some(GitVerifier::CargoTest),
        "cargo check" => Some(GitVerifier::CargoCheck),
        "cargo clippy" => Some(GitVerifier::CargoClippy),
        "npm test" => Some(GitVerifier::NpmTest),
        "pnpm test" => Some(GitVerifier::PnpmTest),
        "yarn test" => Some(GitVerifier::YarnTest),
        "pytest" => Some(GitVerifier::Pytest),
        "go test" => Some(GitVerifier::GoTest),
        "mvn test" => Some(GitVerifier::MavenTest),
        "mvn verify" => Some(GitVerifier::MavenVerify),
        _ => None,
    }
}

fn trim_git_clause(text: &str) -> &str {
    text.trim_matches(|character: char| {
        character.is_whitespace()
            || matches!(
                character,
                ',' | '，' | '、' | ':' | '：' | '.' | '。' | '!' | '！'
            )
    })
}

fn has_unsafe_unquoted_git_connector(text: &str) -> Option<bool> {
    let mut quotes = QuoteTracker::new(false);
    for (index, character) in text.char_indices() {
        if !matches!(quotes.step(character), QuoteEvent::Outside) {
            continue;
        }
        if matches!(
            character,
            ';' | '；' | '|' | '&' | '<' | '>' | '`' | '\n' | '\r'
        ) || (character == '$' && text[index..].starts_with("$("))
        {
            return Some(true);
        }
    }
    quotes.is_balanced().then_some(false)
}

fn unquoted_verification_boundaries(text: &str) -> Option<Vec<(usize, usize)>> {
    const SEPARATORS: &[&str] = &[
        "然后", "然後", "接着", "接著", "同时", "同時", "并且", "並且", "并", "並", "后", "後",
        " and ", " then ",
    ];

    let mut boundaries = Vec::new();
    let mut quotes = QuoteTracker::new(false);
    for (index, character) in text.char_indices() {
        if !matches!(quotes.step(character), QuoteEvent::Outside) {
            continue;
        }
        for separator in SEPARATORS {
            let matches = if separator.is_ascii() {
                text[index..]
                    .get(..separator.len())
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(separator))
            } else {
                text[index..].starts_with(separator)
            };
            if matches {
                boundaries.push((index, separator.len()));
                break;
            }
        }
    }
    quotes.is_balanced().then_some(boundaries)
}

fn git_receipt_prefix_has_additional_work(text: &str) -> bool {
    let lower = text.to_lowercase();
    let compact: String = lower
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if compact.contains("提交")
        && [
            "修复", "修復", "修改", "新增", "删除", "刪除", "实现", "實現", "重构", "重構",
        ]
        .iter()
        .any(|verb| compact.starts_with(verb))
    {
        return true;
    }
    if lower.contains("commit")
        && [
            "fix ",
            "modify ",
            "edit ",
            "change ",
            "implement ",
            "remove ",
            "delete ",
        ]
        .iter()
        .any(|verb| lower.trim_start().starts_with(verb))
    {
        return true;
    }
    [
        "然后运行",
        "然後運行",
        "然后测试",
        "然後測試",
        "然后修改",
        "然後修改",
        "然后修复",
        "然後修復",
        "然后推送",
        "然後推送",
        "并运行",
        "並運行",
        "并测试",
        "並測試",
        "并修改",
        "並修改",
        "并修复",
        "並修復",
        "并推送",
        "並推送",
        " and run ",
        " then run ",
        " and test ",
        " then test ",
        " and modify ",
        " then modify ",
        " and edit ",
        " then edit ",
        " and fix ",
        " then fix ",
        " and push ",
        " then push ",
        " and deploy ",
        " then deploy ",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn git_commit_request_is_question_or_negated(q: &str, compact: &str) -> bool {
    let english_question = q.split(|c: char| !c.is_ascii_alphabetic()).any(|word| {
        matches!(
            word,
            "should"
                | "can"
                | "could"
                | "would"
                | "may"
                | "might"
                | "why"
                | "how"
                | "what"
                | "when"
                | "where"
                | "whether"
        )
    });
    q.contains('?')
        || q.contains('？')
        || compact.ends_with('吗')
        || compact.ends_with('嗎')
        || compact.ends_with('呢')
        || english_question
        || [
            "怎么",
            "怎麼",
            "如何",
            "为什么",
            "為什麼",
            "是什么",
            "是什麼",
            "是否",
            "能否",
            "能不能",
            "可不可以",
            "要不要",
            "该不该",
            "該不該",
            "好不好",
            "提交了吗",
            "提交了嗎",
            "提交没",
            "提交沒",
            "有没有提交",
            "有沒有提交",
            "不要提交",
            "别提交",
            "別提交",
            "勿提交",
            "无需提交",
            "無需提交",
            "暂不提交",
            "暫不提交",
        ]
        .iter()
        .any(|needle| compact.contains(needle))
        || [
            "do not commit",
            "don't commit",
            "dont commit",
            "never commit",
            "did you commit",
            "have you committed",
        ]
        .iter()
        .any(|needle| q.contains(needle))
}

pub(super) fn request_is_git_commit_diagnostic(requirement: &str) -> bool {
    let q = git_commit_control_text(requirement);
    let compact: String = q.chars().filter(|c| !c.is_whitespace()).collect();
    let diagnostic = [
        "失败", "失敗", "报错", "報錯", "错误", "錯誤", "异常", "異常", "问题", "問題", "故障",
        "原因", "状态", "狀態", "日志", "日誌", "解释", "解釋", "分析", "排查", "诊断", "診斷",
    ]
    .iter()
    .any(|needle| compact.contains(needle))
        || [
            "git commit failed",
            "git commit failure",
            "git commit error",
            "git commit broke",
            "commit problem",
            "commit issue",
            "commit failed",
            "commit failure",
            "commit error",
            "commit broke",
            "problem with commit",
            "issue with commit",
            "explain git commit",
            "explain the commit",
            "diagnose git commit",
            "diagnose the commit",
            "debug git commit",
            "debug the commit",
        ]
        .iter()
        .any(|needle| q.contains(needle));
    let commit_context = q.contains("commit") || compact.contains("提交");
    commit_context
        && !git_commit_request_has_additional_work(&q, &compact)
        && diagnostic
        && matches!(
            parse_git_commit_intent(requirement),
            GitCommitIntent::NotCommit
                | GitCommitIntent::UnsupportedLiteralCommand
                | GitCommitIntent::InvalidNaturalScope
        )
}
