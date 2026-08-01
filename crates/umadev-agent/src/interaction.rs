//! TUI-hosted interaction bridge for the director loop — the task-scoped channel
//! that lets the DEFAULT `/run` engine reach a live user when one exists.
//!
//! The director loop (`crate::director_loop`) is deliberately host-safe: every
//! decision point (a base `NeedApproval`, a spec-MUST confirmation gate) has a
//! deterministic fail-open floor when no host declares that it can resume the
//! interaction. The TUI and the guarded headless CLI both host resumable gates;
//! an unscoped library/CI drive does not block waiting on a human.
//!
//! This module carries the hosting UI's live hooks to the loop WITHOUT threading a
//! parameter through every pump signature: the host scopes a [`RunInteraction`]
//! around the whole drive via [`hosted`] (a tokio task-local), and the loop's
//! decision points consult it fail-open — an unscoped drive reads `None`.
//!
//! Three hooks ride the scope:
//! - **`approval`** — an async callback the loop awaits when the trust floor says
//!   a base action needs confirmation; the TUI backs it with the SAME
//!   `await_user_approval` y/n pause the chat surface uses (bounded, fail-open
//!   deny). Headless keeps the deterministic deny.
//! - **`steer`** — a shared queue of user steering directives (`/plan skip|veto|
//!   add`, text typed mid-build). The loop drains it at each step boundary and
//!   folds the directives into the next doer step, so steering applies mid-run
//!   instead of evaporating.
//! - **`confirm_gates`** — whether the host can actually render + resume a
//!   confirmation gate. Only a hosted, non-auto run pauses at `docs_confirm` /
//!   `preview_confirm`; unhosted library/CI runs drive through instead of wedging.
//!
//! Everything here is fail-open by contract (a poisoned lock / missing scope
//! degrades to "no interaction"), and no new dependency is introduced (tokio's
//! `task_local!` is already in the tree).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// The shared mid-run steering intake: the hosting UI pushes user directives in;
/// the director loop drains them at step boundaries through its internal
/// receiver. A plain
/// `std::sync::Mutex` (never held across an await on either side).
pub type SteerIntake = Arc<Mutex<Vec<String>>>;

/// The future an [`ApprovalFn`] returns: resolves `true` when the live user
/// APPROVED the action, `false` on deny / timeout / cancel (fail-open deny).
pub type ApprovalFuture = Pin<Box<dyn Future<Output = bool> + Send>>;

/// The interactive approval callback: `(action, target) -> approved?`. The TUI
/// implements it over its existing `await_user_approval` pause (surface the item,
/// block on y/n, bounded budget, fail-open deny).
pub type ApprovalFn = Arc<dyn Fn(String, String) -> ApprovalFuture + Send + Sync>;

/// Future returned by a hosting UI for a typed in-flight base request.
pub type HostRequestFuture =
    Pin<Box<dyn Future<Output = Option<umadev_runtime::HostResponse>> + Send>>;

/// Interactive callback for structured questions, MCP elicitation, and plan or
/// permission requests that cannot be represented by the legacy y/n approval.
pub type HostRequestFn =
    Arc<dyn Fn(String, umadev_runtime::HostRequest) -> HostRequestFuture + Send + Sync>;

/// The safe queue lane for natural-language input received while a director run
/// is active.
///
/// This is intentionally **not** the product's semantic intent router. The base
/// model still decides what a deferred turn means when it is eventually sent.
/// This small classifier only prevents an unrelated question or a later task
/// from being injected into the current writer as a revision.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RunningInputDisposition {
    /// A question about the work or its reasoning. Answer it as a later model
    /// turn; never mutate the current plan from a question-shaped message.
    Query,
    /// An explicit adjustment to the task that is currently executing.
    Steer,
    /// A clearly later task, or anything too ambiguous to inject safely.
    Deferred,
}

/// Conservatively separate a mid-run natural-language message into a question,
/// an explicit current-task adjustment, or a deferred turn.
///
/// The ordering is deliberate: question and future-task signals win over edit
/// verbs ("after this, change X" and "why did you change X?" are not current
/// steering). Only a clear imperative/current-artifact correction is steered;
/// ambiguity is deferred for the model to interpret after the run settles.
#[must_use]
pub fn classify_running_input(text: &str) -> RunningInputDisposition {
    let raw = text.trim();
    let lower = raw.to_lowercase();

    if raw.is_empty() || looks_like_question(raw, &lower) {
        return RunningInputDisposition::Query;
    }
    if refers_to_later_work(&lower) {
        return RunningInputDisposition::Deferred;
    }
    if explicitly_steers_current_work(raw, &lower) {
        return RunningInputDisposition::Steer;
    }
    RunningInputDisposition::Deferred
}

/// Whether a message explicitly schedules separate work for after the current
/// run. Exposed for confirmation-gate routing, where ambiguous non-steering text
/// is safer to answer read-only than to leave in a queue indefinitely.
#[must_use]
pub fn is_explicit_later_work(text: &str) -> bool {
    refers_to_later_work(&text.trim().to_lowercase())
}

/// Whether a natural-language message is an exact request to cancel the run
/// itself. Kept separate from [`RunningInputDisposition`]: cancel is a control
/// action, not content to inject into either queue.
#[must_use]
pub fn is_running_cancel_intent(text: &str) -> bool {
    let normalized = normalize_cancel_phrase(text);
    matches!(
        normalized.as_str(),
        "取消"
            | "取消当前任务"
            | "取消當前任務"
            | "取消本次任务"
            | "取消本次任務"
            | "停止"
            | "停止当前任务"
            | "停止當前任務"
            | "停止本次任务"
            | "停止本次任務"
            | "停"
            | "终止当前任务"
            | "終止當前任務"
            | "别做了"
            | "別做了"
            | "重来"
            | "重來"
            | "cancel"
            | "cancel it"
            | "cancel current task"
            | "cancel the current task"
            | "cancel current run"
            | "cancel the current run"
            | "stop"
            | "stop it"
            | "stop current task"
            | "stop the current task"
            | "stop current run"
            | "stop the current run"
            | "abort"
            | "abort it"
            | "abort current task"
            | "abort the current task"
            | "restart"
    )
}

/// Whether a line is an exact host-level request to resume the currently parked
/// run. This is deliberately a tiny control vocabulary, not semantic intent
/// classification: callers must additionally prove that an unfinished run or an
/// open gate exists before acting on it. That context guard keeps ordinary prose
/// such as "继续按钮" or "确认页面" model-owned, while a real parked run can never
/// lose its continuation to a slow/unavailable intent-routing child.
#[must_use]
pub fn is_run_resume_intent(text: &str) -> bool {
    let normalized = normalize_resume_phrase(text);
    matches!(
        normalized.as_str(),
        "继续"
            | "繼續"
            | "继续执行"
            | "繼續執行"
            | "继续推进"
            | "繼續推進"
            | "继续推进执行"
            | "繼續推進執行"
            | "继续按计划执行"
            | "繼續按計劃執行"
            | "继续按原计划执行"
            | "繼續按原計劃執行"
            | "按计划继续"
            | "按計劃繼續"
            | "按原计划继续"
            | "按原計劃繼續"
            | "批准执行"
            | "批准執行"
            | "批准继续执行"
            | "批准繼續執行"
            | "确认"
            | "確認"
            | "确认执行"
            | "確認執行"
            | "授权执行"
            | "授權執行"
            | "授权继续执行"
            | "授權繼續執行"
            | "授权按流水线继续执行"
            | "授權按流水線繼續執行"
            | "continue"
            | "continue execution"
            | "continue executing"
            | "continue the plan"
            | "continue with the plan"
            | "continue with the pipeline"
            | "resume"
            | "resume execution"
            | "resume the plan"
            | "resume the existing plan"
            | "approve"
            | "approved"
            | "approve execution"
            | "proceed"
            | "proceed with execution"
    )
}

fn normalize_resume_phrase(text: &str) -> String {
    let mut value = text
        .trim()
        .to_lowercase()
        .trim_matches(|ch: char| matches!(ch, '。' | '！' | '!' | '.' | ',' | '，' | ';' | '；'))
        .trim()
        .to_string();

    loop {
        let before = value.clone();
        for prefix in ["please ", "请先", "請先", "请", "請", "麻烦", "麻煩", "先"] {
            if let Some(rest) = value.strip_prefix(prefix) {
                value = rest.trim().to_string();
                break;
            }
        }
        if value == before {
            break;
        }
    }
    loop {
        let before = value.clone();
        for suffix in [" please", "一下吧", "一下", "吧"] {
            if let Some(rest) = value.strip_suffix(suffix) {
                value = rest.trim().to_string();
                break;
            }
        }
        if value == before {
            break;
        }
    }
    value
}

/// Remove only conventional politeness/softening around an otherwise exact
/// cancellation phrase. The core still must match the allow-list above, so
/// ordinary content such as “实现停止按钮” cannot accidentally become control.
fn normalize_cancel_phrase(text: &str) -> String {
    let mut value = text
        .trim()
        .to_lowercase()
        .trim_matches(|ch: char| matches!(ch, '。' | '！' | '!' | '.' | ',' | '，'))
        .trim()
        .to_string();

    loop {
        let before = value.clone();
        for prefix in ["please ", "请先", "請先", "请", "請", "麻烦", "麻煩", "先"] {
            if let Some(rest) = value.strip_prefix(prefix) {
                value = rest.trim().to_string();
                break;
            }
        }
        if value == before {
            break;
        }
    }
    loop {
        let before = value.clone();
        for suffix in [" please", "一下吧", "一下", "吧"] {
            if let Some(rest) = value.strip_suffix(suffix) {
                value = rest.trim().to_string();
                break;
            }
        }
        if value == before {
            break;
        }
    }
    value
}

/// Whether text at the clarification gate is clearly an answer to the current
/// clarification rather than an unrelated deferred turn.
#[must_use]
pub fn is_explicit_clarification_answer(text: &str) -> bool {
    let raw = text.trim();
    let lower = raw.to_lowercase();
    if raw.is_empty()
        || looks_like_question(raw, &lower)
        || refers_to_later_work(&lower)
        || is_running_cancel_intent(raw)
    {
        return false;
    }
    // A clarification answer may legitimately be a bare value ("PostgreSQL",
    // "管理员", "dark"), so do not require an imperative prefix. Only phrases
    // that clearly introduce a separate conversational turn are deferred.
    ![
        "另一个问题",
        "另一個問題",
        "另外一个问题",
        "另外一個問題",
        "another question",
        "separate question",
        "unrelated question",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
}

fn looks_like_question(raw: &str, lower: &str) -> bool {
    if raw.contains('?') || raw.contains('？') {
        return true;
    }

    const ZH_PREFIXES: &[&str] = &[
        "为什么",
        "為什麼",
        "为何",
        "為何",
        "怎么",
        "怎麼",
        "如何",
        "是否",
        "能否",
        "可否",
        "什么",
        "什麼",
        "哪个",
        "哪個",
        "哪里",
        "哪裡",
        "请问",
        "請問",
        "想问",
        "想問",
        "解释一下",
        "解釋一下",
        "说明一下",
        "說明一下",
        "告诉我",
        "告訴我",
    ];
    if ZH_PREFIXES.iter().any(|prefix| raw.starts_with(prefix)) {
        return true;
    }

    // Natural Chinese questions often omit `？` and put a short context before
    // the interrogative ("这次为什么…", "本次改了什么"). Do not mistake those
    // change verbs for an imperative. A strong object-first imperative remains
    // exempt so e.g. "把‘为什么’这个标题改掉" is still steering.
    const ZH_IMPERATIVE_PREFIXES: &[&str] = &["把", "将", "將", "请把", "請把", "帮我把", "幫我把"];
    const ZH_QUESTION_WORDS: &[&str] = &[
        "为什么",
        "為什麼",
        "为何",
        "為何",
        "怎么",
        "怎麼",
        "如何",
        "什么",
        "什麼",
        "哪些",
        "哪個",
        "是否",
        "能否",
        "有何",
    ];
    if !ZH_IMPERATIVE_PREFIXES
        .iter()
        .any(|prefix| raw.starts_with(prefix))
        && ZH_QUESTION_WORDS
            .iter()
            .any(|question_word| raw.contains(question_word))
    {
        return true;
    }

    const EN_PREFIXES: &[&str] = &[
        "what ",
        "why ",
        "how ",
        "when ",
        "where ",
        "which ",
        "who ",
        "whose ",
        "is ",
        "are ",
        "was ",
        "were ",
        "do ",
        "does ",
        "did ",
        "can ",
        "could ",
        "would ",
        "will ",
        "should ",
        "explain ",
        "tell me ",
        "show me ",
        "please explain ",
        "please tell me ",
        "could you explain ",
        "can you explain ",
    ];
    EN_PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
}

fn refers_to_later_work(lower: &str) -> bool {
    const MARKERS: &[&str] = &[
        "完成后",
        "完成後",
        "做完后",
        "做完後",
        "跑完后",
        "跑完後",
        "结束后",
        "結束後",
        "之后再",
        "之後再",
        "然后再",
        "然後再",
        "接下来再",
        "接下來再",
        "下一步",
        "下一个任务",
        "下一個任務",
        "等你完成",
        "等当前任务",
        "等目前任務",
        "稍后",
        "稍後",
        "待会",
        "待會",
        "等会",
        "等會",
        "晚点",
        "晚點",
        "以后再",
        "以後再",
        "after this",
        "after that",
        "after the current",
        "when this is done",
        "when the current task is done",
        "once this is done",
        "once the current task finishes",
        "as the next task",
        "next task",
        "then later",
        "afterwards",
        "later on",
    ];
    MARKERS.iter().any(|marker| lower.contains(marker))
        || lower.starts_with("later ")
        || lower.ends_with(" later")
        || lower.contains(" later ")
}

fn explicitly_steers_current_work(raw: &str, lower: &str) -> bool {
    const ZH_MUTATIONS: &[&str] = &[
        "改", "换", "換", "替换", "替換", "删", "刪", "移除", "去掉", "加", "增加", "调整", "調整",
        "修复", "修復", "重做", "回退", "撤销", "撤銷", "停止", "暂停", "暫停", "继续", "繼續",
        "使用", "保留",
    ];
    const ZH_CURRENT: &[&str] = &[
        "当前", "當前", "目前", "这次", "這次", "本次", "现在", "現在", "正在", "这个", "這個",
        "这里", "這裡", "刚才", "剛才",
    ];
    let zh_directive = ["请", "請", "麻烦", "麻煩", "帮我", "幫我"]
        .iter()
        .find_map(|prefix| raw.strip_prefix(prefix))
        .map_or(raw, str::trim_start);
    let strong_zh_imperative = ["把", "将", "將"]
        .iter()
        .any(|prefix| zh_directive.starts_with(prefix))
        && ZH_MUTATIONS.iter().any(|verb| zh_directive.contains(verb));
    let current_zh_correction = ZH_CURRENT.iter().any(|marker| raw.contains(marker))
        && ZH_MUTATIONS.iter().any(|verb| raw.contains(verb));
    let direct_zh_imperative = [
        "改成",
        "换成",
        "換成",
        "替换",
        "替換",
        "删掉",
        "刪掉",
        "删除",
        "刪除",
        "去掉",
        "移除",
        "加上",
        "增加",
        "调整",
        "調整",
        "修复",
        "修復",
        "重做",
        "回退",
        "撤销",
        "撤銷",
        "停止",
        "暂停",
        "暫停",
        "继续当前",
        "繼續當前",
        "不要",
        "别继续",
        "別繼續",
        "别再",
        "別再",
    ]
    .iter()
    .any(|prefix| zh_directive.starts_with(prefix));
    let explicit_zh_feedback = ["不对", "不對", "不行", "不好", "有问题", "有問題"]
        .iter()
        .any(|marker| raw.contains(marker));
    if strong_zh_imperative || current_zh_correction || direct_zh_imperative || explicit_zh_feedback
    {
        return true;
    }

    const EN_IMPERATIVES: &[&str] = &[
        "change ",
        "switch ",
        "replace ",
        "remove ",
        "delete ",
        "drop ",
        "add ",
        "rename ",
        "update ",
        "adjust ",
        "fix ",
        "redo ",
        "revert ",
        "undo ",
        "stop ",
        "pause ",
        "continue the current ",
        "use ",
        "keep ",
        "make ",
        "don't ",
        "do not ",
    ];
    const EN_CURRENT: &[&str] = &[
        "current ",
        "this task",
        "this change",
        "this run",
        "this implementation",
        "what you're doing",
        "what you are doing",
    ];
    const EN_MUTATIONS: &[&str] = &[
        "change", "switch", "replace", "remove", "delete", "add", "rename", "update", "adjust",
        "fix", "redo", "revert", "undo", "stop", "pause", "continue", "use", "keep", "make",
    ];
    let en_directive = lower.strip_prefix("please ").unwrap_or(lower);
    EN_IMPERATIVES
        .iter()
        .any(|prefix| en_directive.starts_with(prefix))
        || matches!(
            en_directive,
            "stop" | "pause" | "undo" | "revert" | "redo" | "continue"
        )
        || (EN_CURRENT.iter().any(|marker| lower.contains(marker))
            && EN_MUTATIONS.iter().any(|verb| lower.contains(verb)))
        || [
            "not right",
            "looks wrong",
            "is wrong",
            "doesn't work",
            "does not work",
        ]
        .iter()
        .any(|marker| lower.contains(marker))
}

/// The hooks a hosting UI provides for one director-loop run. `Default` (all
/// `None` / `false`) is exactly "headless" — the loop behaves as today.
#[derive(Clone, Default)]
pub struct RunInteraction {
    /// Mid-run steering intake; `None` = no steering surface.
    pub steer: Option<SteerIntake>,
    /// Interactive approval callback; `None` = headless
    /// (the deterministic trust floor auto-decides, exactly as today).
    pub approval: Option<ApprovalFn>,
    /// Typed in-flight host request callback; `None` safely rejects requests
    /// that need richer input than the legacy approval surface.
    pub host_request: Option<HostRequestFn>,
    /// Whether the host renders + resumes confirmation gates. Only a hosted,
    /// non-auto run pauses at `docs_confirm` / `preview_confirm`.
    pub confirm_gates: bool,
}

impl std::fmt::Debug for RunInteraction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunInteraction")
            .field("steer", &self.steer.as_ref().map(|_| "<intake>"))
            .field("approval", &self.approval.as_ref().map(|_| "<callback>"))
            .field(
                "host_request",
                &self.host_request.as_ref().map(|_| "<callback>"),
            )
            .field("confirm_gates", &self.confirm_gates)
            .finish()
    }
}

tokio::task_local! {
    /// The hosting UI's interaction hooks for the CURRENT director-loop task.
    /// Unset (embedded/CI callers and tests that do not opt in) → every consult
    /// below fails open to "no interaction".
    static RUN_INTERACTION: RunInteraction;
}

/// Run `fut` with `interaction` scoped as the current task's interaction hooks.
/// The TUI and guarded headless CLI wrap their director-loop drive in this;
/// everything the loop awaits inside inherits the scope.
pub async fn hosted<F: Future>(interaction: RunInteraction, fut: F) -> F::Output {
    RUN_INTERACTION.scope(interaction, fut).await
}

/// Whether the current task is hosted by a surface that renders or reports, then
/// resumes, confirmation gates. `false` when unscoped — fail-open.
#[must_use]
pub(crate) fn gates_hosted() -> bool {
    RUN_INTERACTION
        .try_with(|i| i.confirm_gates)
        .unwrap_or(false)
}

/// Whether the current task carries a steering intake — i.e. a reply the user
/// types mid-run CAN be folded into the next step's directive. Drives the honest
/// `AskUserQuestion` hint variant (A2#6): with an intake, "your answer applies as
/// follow-up steering" is literally true; without one the caller keeps its
/// existing framing. `false` when unscoped (fail-open).
#[must_use]
pub(crate) fn steering_hosted() -> bool {
    RUN_INTERACTION
        .try_with(|i| i.steer.is_some())
        .unwrap_or(false)
}

/// Drain every queued mid-run steering directive (FIFO). Empty when unscoped,
/// no intake was provided, or the queue is empty — all fail-open.
#[must_use]
pub(crate) fn take_steer() -> Vec<String> {
    RUN_INTERACTION
        .try_with(|i| i.steer.clone())
        .ok()
        .flatten()
        .and_then(|q| q.lock().ok().map(|mut v| std::mem::take(&mut *v)))
        .unwrap_or_default()
}

/// Ask the live user to approve `action` on `target` via the host's callback.
/// `None` when the run is headless (no scope / no callback) — the caller then
/// applies today's deterministic floor decision. `Some(approved)` otherwise.
pub(crate) async fn request_approval(action: &str, target: &str) -> Option<bool> {
    let cb = RUN_INTERACTION
        .try_with(|i| i.approval.clone())
        .ok()
        .flatten()?;
    Some(cb(action.to_string(), target.to_string()).await)
}

/// Ask the hosting UI to answer a typed in-flight request. `None` means no
/// interactive surface is available (or it cancelled), so the caller must use
/// [`umadev_runtime::HostRequest::safe_rejection`].
pub(crate) async fn request_host_response(
    req_id: &str,
    request: &umadev_runtime::HostRequest,
) -> Option<umadev_runtime::HostResponse> {
    let callback = RUN_INTERACTION
        .try_with(|interaction| interaction.host_request.clone())
        .ok()
        .flatten()?;
    callback(req_id.to_string(), request.clone()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_input_split_is_conservative_and_trilingual() {
        for text in [
            "为什么正在跑 Maven？",
            "為什麼正在執行評審",
            "这次改了什么",
            "這次為什麼改成這樣",
            "what are you changing?",
            "what changed",
            "could you explain the current choice",
        ] {
            assert_eq!(
                classify_running_input(text),
                RunningInputDisposition::Query,
                "{text}"
            );
        }

        for text in [
            "把配色换成暗色",
            "請把目前的圖示換成 lucide",
            "不要再运行评审",
            "停止",
            "改成暗色",
            "不要继续",
            "請停止目前的評審",
            "change the current header to be sticky",
            "stop",
            "please stop the current review",
            "this implementation is wrong; revert it",
        ] {
            assert_eq!(
                classify_running_input(text),
                RunningInputDisposition::Steer,
                "{text}"
            );
        }

        for text in [
            "完成后再做登录",
            "這個做完後再處理付款",
            "把登录稍后再加",
            "把登入稍後再加",
            "after this, add account export",
            "change it later",
            "add account export afterwards",
            "另一个问题",
            "SEO",
            "看看这个",
        ] {
            assert_eq!(
                classify_running_input(text),
                RunningInputDisposition::Deferred,
                "{text}"
            );
        }
    }

    #[test]
    fn question_or_later_signal_wins_over_an_edit_verb() {
        assert_eq!(
            classify_running_input("为什么把配色换成暗色？"),
            RunningInputDisposition::Query
        );
        assert_eq!(
            classify_running_input("当前任务完成后，把配色换成暗色"),
            RunningInputDisposition::Deferred
        );
    }

    #[test]
    fn cancel_is_a_control_intent_and_clarification_answers_are_explicit() {
        for text in [
            "取消",
            "取消当前任务",
            "停止",
            "终止当前任务",
            "别做了",
            "重来",
            "cancel",
            "cancel the current run",
            "stop",
            "abort current task",
            "restart",
            "请停止当前任务",
            "請先取消本次任務吧",
            "先停一下",
            "取消吧。",
            "please stop",
            "cancel it",
            "please abort it!",
        ] {
            assert!(is_running_cancel_intent(text), "{text}");
        }
        for text in ["停止按钮", "取消按钮", "cancel button", "不要继续评审"] {
            assert!(!is_running_cancel_intent(text), "{text}");
        }

        for text in [
            "面向个人开发者",
            "管理员",
            "PostgreSQL",
            "dark",
            "回答：使用 PostgreSQL",
            "answer 1",
            "my answer is PostgreSQL",
            "use postgres",
        ] {
            assert!(is_explicit_clarification_answer(text), "{text}");
        }
        for text in [
            "为什么需要 PostgreSQL？",
            "完成后再做登录",
            "afterwards add export",
            "另一个问题",
            "请取消",
            "請先停止本次任務吧",
            "please cancel it",
        ] {
            assert!(!is_explicit_clarification_answer(text), "{text}");
        }
    }

    #[test]
    fn resume_is_an_exact_control_intent_not_a_keyword_guess() {
        for text in [
            "继续",
            "请继续推进执行",
            "批准执行",
            "确认",
            "授权按流水线继续执行",
            "請繼續按原計劃執行吧",
            "continue",
            "please resume the existing plan",
            "proceed with execution",
        ] {
            assert!(is_run_resume_intent(text), "{text}");
        }
        for text in [
            "继续按钮不能点击",
            "确认页面的样式",
            "不要继续评审",
            "解释如何继续",
            "after this, continue with SEO",
            "continue button",
        ] {
            assert!(!is_run_resume_intent(text), "{text}");
        }
    }

    #[tokio::test]
    async fn unscoped_task_fails_open_to_headless() {
        // No scope → gates unhosted, no steering, no approval callback — the
        // exact headless posture the CLI / CI relies on.
        assert!(!gates_hosted());
        assert!(take_steer().is_empty());
        assert!(request_approval("bash", "rm -rf /").await.is_none());
    }

    #[tokio::test]
    async fn scoped_task_reads_the_hosted_hooks() {
        let steer: SteerIntake = Arc::new(Mutex::new(vec!["skip step 2".to_string()]));
        let approval: ApprovalFn = Arc::new(|_a, _t| Box::pin(async { true }) as ApprovalFuture);
        let interaction = RunInteraction {
            steer: Some(Arc::clone(&steer)),
            approval: Some(approval),
            host_request: None,
            confirm_gates: true,
        };
        hosted(interaction, async {
            assert!(gates_hosted());
            // The intake drains FIFO and then reads empty (consumed).
            assert_eq!(take_steer(), vec!["skip step 2".to_string()]);
            assert!(take_steer().is_empty());
            // The approval callback is consulted and its verdict returned.
            assert_eq!(request_approval("write", "src/x.rs").await, Some(true));
        })
        .await;
        // Outside the scope the task-local is gone again (fail-open headless).
        assert!(!gates_hosted());
    }

    #[test]
    fn debug_impl_never_dumps_the_callback() {
        let i = RunInteraction {
            steer: Some(Arc::new(Mutex::new(Vec::new()))),
            approval: Some(Arc::new(|_a, _t| {
                Box::pin(async { false }) as ApprovalFuture
            })),
            host_request: None,
            confirm_gates: true,
        };
        let s = format!("{i:?}");
        assert!(s.contains("confirm_gates: true"));
    }
}
