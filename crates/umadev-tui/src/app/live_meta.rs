//! Classification for read-only questions answerable from live TUI state.

use super::permissions;

/// Read-only questions the TUI can answer from state it already owns.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum LiveMetaIntent {
    Progress,
    Changes,
    Permissions,
}

/// Bounded, trilingual live-meta classifier. It accepts polite/natural variants,
/// but rejects multiline, future-work, and chained-action requests so a status
/// lookup cannot swallow real work.
pub(super) fn classify_live_meta(text: &str) -> Option<LiveMetaIntent> {
    const CHANGES: &[&str] = &[
        "这次改了什么",
        "这次改了啥",
        "这次都改了什么",
        "这次都改了啥",
        "这次改动都做了什么",
        "这次改动都做了啥",
        "本次改了什么",
        "本次改了啥",
        "本次改动",
        "本轮改动",
        "这轮改动",
        "改了哪些文件",
        "这次改了哪些文件",
        "這次改了什麼",
        "這次都改了什麼",
        "這次改動都做了什麼",
        "本次改動",
        "本輪改動",
        "這輪改動",
        "改了哪些檔案",
        "這次改了哪些檔案",
        "what changed",
        "what did you change",
        "what have you changed",
        "what changes did you make",
        "what files changed",
        "show me the changes",
        "what did this turn change",
    ];
    const PROGRESS: &[&str] = &[
        "当前进度",
        "目前进度",
        "现在进度",
        "进度怎么样",
        "进度怎么样了",
        "现在做到哪了",
        "做到哪了",
        "进行到哪了",
        "现在在做什么",
        "当前在做什么",
        "任务进度",
        "全部完成了吗",
        "全部都完成了吗",
        "都完成了吗",
        "所有问题都解决了吗",
        "问题全部解决了吗",
        "还剩多少任务",
        "还剩什么任务",
        "还有什么没完成",
        "还差多少",
        "距离完成还差多少",
        "當前進度",
        "目前進度",
        "現在進度",
        "進度怎麼樣",
        "進度怎麼樣了",
        "現在做到哪了",
        "進行到哪了",
        "現在在做什麼",
        "當前在做什麼",
        "任務進度",
        "全部完成了嗎",
        "全部都完成了嗎",
        "都完成了嗎",
        "所有問題都解決了嗎",
        "問題全部解決了嗎",
        "還剩多少任務",
        "還剩什麼任務",
        "還有什麼沒完成",
        "還差多少",
        "距離完成還差多少",
        "current progress",
        "progress update",
        "what's the progress",
        "what is the progress",
        "where are we",
        "where are you at",
        "what are you doing",
        "what are you working on",
        "current status",
        "status update",
        "is everything done",
        "are we done",
        "what remains",
        "what is left",
        "how much is left",
    ];
    let lowered = text.trim().to_lowercase();
    let normalized = lowered
        .trim_matches(|c: char| {
            c.is_ascii_punctuation()
                || matches!(c, '，' | '。' | '？' | '！' | '：' | '；' | '、' | '～')
        })
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if CHANGES.contains(&normalized.as_str()) {
        return Some(LiveMetaIntent::Changes);
    }
    if PROGRESS.contains(&normalized.as_str()) {
        return Some(LiveMetaIntent::Progress);
    }

    // Natural variants stay deliberately bounded. A longer instruction or a
    // second requested action belongs to the normal intent router.
    if text.contains(['\n', '\r']) || normalized.chars().count() > 120 {
        return None;
    }
    let compact = normalized.split_whitespace().collect::<String>();
    let contains_any =
        |haystack: &str, needles: &[&str]| needles.iter().any(|n| haystack.contains(n));
    let chained_work = contains_any(
        &compact,
        &[
            "然后修",
            "然后改",
            "然后写",
            "然后跑",
            "然后测试",
            "然後修",
            "然後改",
            "然後寫",
            "然後測試",
            "并修",
            "并改",
            "并写",
            "并补",
            "并测试",
            "並修",
            "並改",
            "並寫",
            "並補",
            "並測試",
            "顺便修",
            "顺便改",
            "順便修",
            "順便改",
            "接着修",
            "接着改",
            "接著修",
            "接著改",
        ],
    ) || contains_any(
        &normalized,
        &[
            " and then ",
            " then fix",
            " then update",
            " then edit",
            " then run",
            " and fix",
            " and update",
            " and edit",
            " and run",
            " also fix",
            " also update",
        ],
    );
    if chained_work {
        return None;
    }
    if permissions::is_pure_question(&normalized, &compact) {
        return Some(LiveMetaIntent::Permissions);
    }

    let zh_scope = contains_any(
        &compact,
        &[
            "这次", "本次", "此次", "这轮", "本轮", "刚才", "刚刚", "這次", "此次", "這輪", "本輪",
            "剛才", "剛剛",
        ],
    );
    let zh_completed_change = contains_any(
        &compact,
        &[
            "改了",
            "修改了",
            "更新了",
            "变更了",
            "變更了",
            "改动",
            "改動",
            "变更",
            "變更",
            "的修改",
            "的更新",
        ],
    );
    let zh_change_question = contains_any(
        &compact,
        &[
            "什么",
            "什麼",
            "啥",
            "哪些",
            "哪几",
            "哪幾",
            "改了些什",
            "做了些什",
        ],
    );
    let zh_present = contains_any(
        &compact,
        &[
            "说下",
            "說下",
            "说说",
            "說說",
            "讲下",
            "講下",
            "告诉我",
            "告訴我",
            "总结",
            "總結",
            "列出",
            "展示",
            "说明",
            "說明",
            "介绍",
            "介紹",
        ],
    );
    let zh_future_change = contains_any(
        &compact,
        &[
            "要改", "需改", "需要", "应该", "應該", "计划", "計劃", "打算", "要求", "目标", "目標",
            "任务", "任務",
        ],
    );
    if !zh_future_change
        && zh_completed_change
        && (zh_change_question || zh_present)
        && (zh_scope || compact.contains("你改了") || compact.contains("您改了"))
    {
        return Some(LiveMetaIntent::Changes);
    }

    let english_future_change = contains_any(
        &normalized,
        &[
            "should change",
            "should update",
            "need to change",
            "need to update",
            "plan to change",
            "changes to make",
            "change requirements",
        ],
    );
    let english_change_question = contains_any(
        &normalized,
        &[
            "what changed",
            "what did you change",
            "what have you changed",
            "what you changed",
            "what files did you change",
            "which files did you change",
            "what did you update",
            "changes you made",
            "changes did you make",
            "summarize the changes",
            "summarise the changes",
            "summary of the changes",
            "show me the changes",
            "list the changes",
            "what was changed",
            "what got changed",
        ],
    );
    if english_change_question && !english_future_change {
        return Some(LiveMetaIntent::Changes);
    }

    let zh_progress = contains_any(&compact, &["进度", "進度", "进展", "進展"]);
    let zh_progress_question = contains_any(
        &compact,
        &[
            "怎么样",
            "怎麼樣",
            "如何",
            "到哪",
            "哪一步",
            "多少",
            "什么进展",
            "什麼進展",
            "啥进展",
            "啥進展",
            "有什么进展",
            "有什麼進展",
            "什么情况",
            "什麼情況",
            "啥情况",
            "嗎",
            "吗",
        ],
    );
    let zh_progress_where = contains_any(
        &compact,
        &[
            "做到哪",
            "进行到哪",
            "進行到哪",
            "处理到哪",
            "處理到哪",
            "弄到哪",
        ],
    );
    let zh_work_object = contains_any(
        &compact,
        &[
            "组件", "組件", "页面", "頁面", "代码", "代碼", "函数", "函數", "接口", "文件", "檔案",
            "配置", "测试", "測試",
        ],
    );
    if !zh_work_object
        && ((zh_progress && (zh_progress_question || zh_present)) || zh_progress_where)
    {
        return Some(LiveMetaIntent::Progress);
    }

    let english_progress = contains_any(
        &normalized,
        &[
            "how is it going",
            "how far along",
            "what's the progress",
            "what is the progress",
            "current progress",
            "progress update",
            "where are we",
            "where are you at",
            "what are you doing",
            "what are you working on",
            "current status",
            "status update",
            "what stage are we",
            "what stage are you",
            "what step are we on",
            "what step are you on",
            "tell me the current progress",
            "give me a progress update",
            "show me the current progress",
        ],
    );
    let english_progress_prompt = contains_any(
        &normalized,
        &[
            "what",
            "where",
            "how",
            "tell me",
            "show me",
            "give me",
            "can you",
            "could you",
            "please",
        ],
    );
    let english_work_object = contains_any(
        &normalized,
        &[
            " component",
            " page",
            " function",
            " endpoint",
            " widget",
            " source file",
            " code",
            " api",
        ],
    );
    (english_progress && english_progress_prompt && !english_work_object)
        .then_some(LiveMetaIntent::Progress)
}

#[cfg(test)]
mod tests {
    use super::{classify_live_meta, LiveMetaIntent};

    #[test]
    fn classifier_is_bounded_and_trilingual() {
        for (text, expected) in [
            ("这次改动都做了啥？", LiveMetaIntent::Changes),
            ("能说下这次都改了哪些内容吗？", LiveMetaIntent::Changes),
            ("你这次都改了些什么？", LiveMetaIntent::Changes),
            ("本輪改動", LiveMetaIntent::Changes),
            ("what did you change?", LiveMetaIntent::Changes),
            (
                "could you tell me what changed this time?",
                LiveMetaIntent::Changes,
            ),
            ("what files did you change?", LiveMetaIntent::Changes),
            ("当前进度", LiveMetaIntent::Progress),
            ("现在进展到哪一步啦？", LiveMetaIntent::Progress),
            ("目前什么进展了", LiveMetaIntent::Progress),
            ("现在啥进展了？", LiveMetaIntent::Progress),
            ("当前有什么进展", LiveMetaIntent::Progress),
            ("全部完成了吗？", LiveMetaIntent::Progress),
            ("所有问题都解决了吗？", LiveMetaIntent::Progress),
            ("还剩什么任务？", LiveMetaIntent::Progress),
            ("目前有什麼進展了？", LiveMetaIntent::Progress),
            ("全部完成了嗎？", LiveMetaIntent::Progress),
            ("目前進度？", LiveMetaIntent::Progress),
            ("what are you working on?", LiveMetaIntent::Progress),
            ("is everything done?", LiveMetaIntent::Progress),
            ("what remains?", LiveMetaIntent::Progress),
            ("how far along are you?", LiveMetaIntent::Progress),
            (
                "could you give me a current progress update?",
                LiveMetaIntent::Progress,
            ),
            ("怎么给你权限？", LiveMetaIntent::Permissions),
            ("为什么是只读？", LiveMetaIntent::Permissions),
            ("你现在能修改文件吗？", LiveMetaIntent::Permissions),
            ("请问你能写文件吗？", LiveMetaIntent::Permissions),
            ("怎麼給你權限？", LiveMetaIntent::Permissions),
            ("為什麼是唯讀？", LiveMetaIntent::Permissions),
            (
                "how can I grant you permission?",
                LiveMetaIntent::Permissions,
            ),
            ("do you have write access?", LiveMetaIntent::Permissions),
        ] {
            assert_eq!(classify_live_meta(text), Some(expected), "{text}");
        }
        for mutation in [
            "修改当前进度组件",
            "当前进度组件如何修改",
            "把本次改动写进 CHANGELOG",
            "这次修改有哪些要求",
            "总结本次改动并补测试",
            "show me the changes and then fix the tests",
            "what changes should we make?",
            "build a current status component",
            "修改权限管理页面",
            "修复只读模式切换",
            "把权限说明写入 README",
            "为什么只读，然后修复登录",
            "你能写文件吗？请修改登录页面",
            "can you write files? please update the login page",
        ] {
            assert_eq!(classify_live_meta(mutation), None, "{mutation}");
        }
    }
}
