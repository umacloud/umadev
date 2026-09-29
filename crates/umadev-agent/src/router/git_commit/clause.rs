//! Commit orders that do not open the request, and the Git context that keeps
//! a question or a failure report about a commit read-only.
//!
//! `提交` is also the everyday word for "submit" (提交按钮, 用户提交后推送通知),
//! so neither check matches it anywhere in the text: a commit counts where a
//! clause begins, or next to Git wording, a VCS object or a path.

use super::natural::{
    commit_object, strip_commit_determiner, strip_git_commit_politeness, COMMIT_DETERMINERS,
    COMMIT_EDITS, COMMIT_OBJECTS,
};
use super::scope::{git_commit_control_text, parse_git_commit_paths};
use super::{
    git_commit_request_has_additional_work, parse_git_commit_clause, parse_git_commit_intent,
    GitCommitIntent,
};

/// Whether the control text orders a Git commit as its own clause after other
/// work (`修复登录问题后提交git记录`, `fix the bug and commit`) or after a wish
/// (`我想提交git记录`). Only Git wording counts there: a bare `提交` after other
/// words is as likely the everyday "submit" (`用户填写完表单后提交`). A tail the
/// commit parser does not recognise counts only when it goes on to more work
/// (`然后提交代码并推送`); otherwise it describes a product flow
/// (`点击后提交代码到判题服务`).
pub(super) fn names_git_commit_clause(control: &str) -> bool {
    clause_starts(control).into_iter().any(|start| {
        let Some(clause) = control.get(start..) else {
            return false;
        };
        let clause = strip_clause_leads(clause);
        let (intent, git_wording) = parse_git_commit_clause(clause);
        git_wording
            && match intent {
                GitCommitIntent::NotCommit => false,
                GitCommitIntent::InvalidNaturalScope => {
                    let compact: String = clause.chars().filter(|ch| !ch.is_whitespace()).collect();
                    git_commit_request_has_additional_work(clause, &compact)
                }
                GitCommitIntent::LiteralCommand(_)
                | GitCommitIntent::UnsupportedLiteralCommand
                | GitCommitIntent::NaturalAllDirty
                | GitCommitIntent::NaturalPaths(_) => true,
            }
    })
}

/// Whether the request is about a Git commit at all, so that a question or a
/// failure word in it keeps the turn read-only. `提交` alone does not count
/// (`提交按钮样式有问题`, `提交订单接口报错`): the request must parse as a
/// commit, use Git wording, or put a VCS object or a path next to `提交`.
pub(in crate::router) fn git_commit_context(requirement: &str) -> bool {
    let control = git_commit_control_text(requirement);
    !control.is_empty()
        && (names_git_word(&control)
            || !matches!(
                parse_git_commit_intent(requirement),
                GitCommitIntent::NotCommit
            )
            || submit_names_vcs_object(&control))
}

/// Byte offsets where a clause may begin: the start, and just after each
/// sequencing word or clause mark.
fn clause_starts(text: &str) -> Vec<usize> {
    const MARKS: &[&str] = &[
        "然后",
        "然後",
        "接着",
        "接著",
        "并且",
        "並且",
        "同时",
        "同時",
        "之后",
        "之後",
        "以后",
        "以後",
        "随后",
        "隨後",
        "顺便",
        "順便",
        "最后",
        "最後",
        "并",
        "並",
        "后",
        "後",
        "再",
        "完",
        "，",
        ",",
        "。",
        "；",
        ";",
        "！",
        "!",
        "、",
        "：",
        "\n",
        "&&",
        "||",
        "|",
        "&",
        " and then ",
        " and ",
        " then ",
        " also ",
        ". ",
    ];
    let mut starts = vec![0];
    for (index, _) in text.char_indices() {
        if let Some(mark) = MARKS.iter().find(|mark| text[index..].starts_with(**mark)) {
            starts.push(index + mark.len());
        }
    }
    starts
}

/// Drop politeness, a wish (`我想`, `I want you to`) and a light verb
/// (`做一次`, `do a`) in front of a clause, so `我想提交git记录` reads as its
/// commit. A lead that introduces a product noun (`做一个`) stays in place, so
/// `做一个 git 提交记录页面` is not read as a commit.
fn strip_clause_leads(mut clause: &str) -> &str {
    const LEADS: &[&str] = &[
        "我想要",
        "我想",
        "我要",
        "我需要",
        "我希望",
        "希望你",
        "想要",
        "还要",
        "還要",
        "也",
        "做一次",
        "来一次",
        "來一次",
        "进行一次",
        "進行一次",
        "i want you to ",
        "i want to ",
        "i'd like you to ",
        "i would like you to ",
        "i'd like to ",
        "i need you to ",
        "i need to ",
        "let's ",
        "lets ",
        "also ",
        "do a ",
        "do one ",
        "run ",
    ];
    loop {
        let trimmed = strip_git_commit_politeness(clause);
        match LEADS.iter().find_map(|lead| trimmed.strip_prefix(lead)) {
            Some(rest) => clause = rest,
            None => return trimmed,
        }
    }
}

/// `git` or `commit` as an ASCII word: not `github`, `commitment`, `committee`.
fn names_git_word(text: &str) -> bool {
    text.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|word| {
            matches!(
                word,
                "git" | "commit" | "commits" | "committed" | "committing"
            )
        })
}

/// `提交` next to what a commit takes: a VCS object before or after it
/// (`提交代码`, `代码提交失败`, `把修改提交`, `提交所有修改`), or a path after it
/// (`提交 README.md`).
fn submit_names_vcs_object(control: &str) -> bool {
    control.match_indices("提交").any(|(index, submit)| {
        let after = &control[index + submit.len()..];
        vcs_object_ends(control[..index].trim_end())
            || vcs_object_starts(after.trim_start())
            || path_starts(after)
    })
}

fn vcs_object_starts(text: &str) -> bool {
    let text = text.strip_prefix("一下").unwrap_or(text);
    let (determined, rest) = strip_commit_determiner(text);
    commit_object(rest, determined).is_some_and(|(_, unmistakable)| unmistakable)
}

fn vcs_object_ends(text: &str) -> bool {
    COMMIT_OBJECTS.iter().any(|object| text.ends_with(object))
        || COMMIT_EDITS.iter().any(|edit| {
            text.strip_suffix(edit).is_some_and(|lead| {
                ["把", "将", "將"]
                    .iter()
                    .chain(COMMIT_DETERMINERS)
                    .any(|marker| lead.ends_with(marker))
            })
        })
}

/// A path right after `提交`: after a space, or glued but ASCII-led
/// (`提交README.md`), so `提交表单到/api/submit` stays a form submission.
fn path_starts(text: &str) -> bool {
    let spaced = text.starts_with(char::is_whitespace);
    let text = text.trim_start();
    if !spaced
        && !text
            .starts_with(|character: char| character.is_ascii_alphanumeric() || character == '.')
    {
        return false;
    }
    let token: String = text
        .chars()
        .take_while(|character| {
            !character.is_whitespace()
                && !matches!(
                    character,
                    '，' | '。' | '；' | '！' | '？' | '、' | ',' | ';' | '!' | '?'
                )
        })
        .collect();
    let token = token.trim_end_matches(['吗', '嗎', '呢', '了', '吧']);
    parse_git_commit_paths(token).is_some_and(|paths| !paths.is_empty())
}
