//! Confirmation gates — UD-FLOW-002 / UD-FLOW-003.

use serde::{Deserialize, Serialize};

/// Which gate this represents.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gate {
    /// Before `research` — the worker generated clarifying questions; wait
    /// for the user to answer them before the pipeline continues. The answers
    /// enrich the requirement so research/docs land closer to intent.
    ClarifyGate,
    /// After `docs` phase — wait for explicit user approval of PRD/ARCH/UIUX.
    DocsConfirm,
    /// After `frontend` phase — wait for explicit user approval of preview.
    PreviewConfirm,
}

impl Gate {
    /// Canonical id persisted to `workflow-state.json#active_gate`.
    #[must_use]
    pub const fn id_str(self) -> &'static str {
        match self {
            Self::ClarifyGate => "clarify",
            Self::DocsConfirm => "docs_confirm",
            Self::PreviewConfirm => "preview_confirm",
        }
    }

    /// Inverse of [`Gate::id_str`]: parse a persisted gate id back into the
    /// typed enum. Case-insensitive + whitespace-tolerant; returns `None`
    /// for unknown ids (fail-open). Replaces the ad-hoc string matches the
    /// CLI previously sprinkled across `main.rs`. Mirrors
    /// `umadev_spec::Gate::from_id` so both Gate types stay parseable
    /// from the same persisted strings.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        match id.trim().to_ascii_lowercase().as_str() {
            "clarify" => Some(Self::ClarifyGate),
            "docs_confirm" => Some(Self::DocsConfirm),
            "preview_confirm" => Some(Self::PreviewConfirm),
            _ => None,
        }
    }
}

/// What the user did at the gate.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GateOutcome {
    /// User said `确认 / 通过 / 继续 / lgtm / approve / ...`.
    Approved,
    /// User requested revisions (free-form).
    Revise(String),
    /// User explicitly cancelled the pipeline.
    Cancelled,
}

const APPROVAL_TOKENS: &[&str] = &[
    "确认", "通过", "继续", "approved", "approve", "lgtm", "ship it", "ok",
];

/// Classify a free-form user reply into a gate outcome.
///
/// UD-FLOW-002 rules:
/// - exact match against `APPROVAL_TOKENS` (case-insensitive, trimmed) → Approved
/// - "cancel" / "取消" / "重来" → Cancelled
/// - everything else → Revise(text)
#[must_use]
pub fn classify_reply(reply: &str) -> GateOutcome {
    let lower = reply.trim().to_lowercase();
    if lower.is_empty() {
        return GateOutcome::Revise(String::new());
    }
    if APPROVAL_TOKENS
        .iter()
        .any(|t| t.eq_ignore_ascii_case(&lower))
    {
        return GateOutcome::Approved;
    }
    if matches!(lower.as_str(), "cancel" | "取消" | "重来" | "restart") {
        return GateOutcome::Cancelled;
    }
    GateOutcome::Revise(reply.trim().to_string())
}

/// Heuristic: does this base reply CLAIM it made code changes? Used by the director
/// build loop to decide whether an honesty/QC read is even warranted (a pure
/// chat/plan answer that touched no files has nothing to QC), and — at the app
/// boundary — to anchor a "claimed-but-no-diff" warning. Deliberately broad and
/// bilingual; a false positive only adds an advisory check, never blocks anything
/// (the source-present floor is itself fail-open). Lives here, the agent crate's
/// reply-classification home, so the TUI's public wrapper has ONE source of truth.
#[must_use]
pub fn claims_code_changes(text: &str) -> bool {
    // English change verbs.
    const EN: &[&str] = &[
        "refactor",
        "added",
        "changed",
        "edited",
        "created",
        "updated",
        "modified",
        "removed",
        "deleted",
        "implemented",
        "renamed",
        "rewrote",
        "replaced",
        "inserted",
    ];
    // Chinese change verbs (no case folding needed).
    const ZH: &[&str] = &[
        "重构",
        "新增",
        "删除",
        "修改",
        "实现",
        "修复",
        "改了",
        "改动",
        "更新",
        "增加",
        "移除",
        "重命名",
        "替换",
        "已添加",
        "已修改",
        "写入",
        "创建",
    ];
    let t = text.to_lowercase();
    if EN.iter().any(|k| t.contains(k)) {
        return true;
    }
    ZH.iter().any(|k| text.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_code_changes_detects_change_verbs_bilingually() {
        assert!(claims_code_changes(
            "I created app.ts and updated the route"
        ));
        assert!(claims_code_changes("已实现登录表单，新增了失败路径测试"));
        // A pure chat / plan answer with no change verb → no claim.
        assert!(!claims_code_changes(
            "Here's how I'd approach it conceptually — nothing touched."
        ));
        assert!(!claims_code_changes("这是我的思路，我先和你确认一下方案"));
    }

    #[test]
    fn approval_tokens_match() {
        for t in [
            "确认", "通过", "继续", "approved", "Approve", "LGTM", "ship it",
        ] {
            assert!(matches!(classify_reply(t), GateOutcome::Approved), "{t}");
        }
    }

    #[test]
    fn cancel_tokens_match() {
        for t in ["cancel", "取消", "重来", "restart"] {
            assert!(matches!(classify_reply(t), GateOutcome::Cancelled), "{t}");
        }
    }

    #[test]
    fn revise_default() {
        let out = classify_reply("把图标库换成 lucide");
        if let GateOutcome::Revise(text) = out {
            assert!(text.contains("lucide"));
        } else {
            panic!("expected Revise");
        }
    }

    #[test]
    fn empty_reply_is_revise_with_empty_text() {
        assert!(matches!(classify_reply(""), GateOutcome::Revise(s) if s.is_empty()));
    }

    #[test]
    fn gate_from_id_roundtrips_and_is_case_insensitive() {
        for g in [Gate::ClarifyGate, Gate::DocsConfirm, Gate::PreviewConfirm] {
            assert_eq!(Gate::from_id(g.id_str()), Some(g));
        }
        assert_eq!(Gate::from_id("Docs_Confirm"), Some(Gate::DocsConfirm));
        assert_eq!(
            Gate::from_id("  preview_confirm  "),
            Some(Gate::PreviewConfirm)
        );
        assert_eq!(Gate::from_id("nope"), None);
        assert_eq!(Gate::from_id(""), None);
    }
}
