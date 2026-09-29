//! Commit orders that do not open the request.
//!
//! `提交` is also the everyday word for "submit" (提交按钮, 用户提交后推送通知),
//! so the firewall does not match it anywhere in the text: a commit counts
//! where a clause begins.

use super::natural::strip_git_commit_politeness;
use super::{git_commit_request_has_additional_work, parse_git_commit_clause, GitCommitIntent};

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
