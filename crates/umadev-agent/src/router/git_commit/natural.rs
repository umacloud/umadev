use super::scope::{marker_matches_case_insensitive, parse_git_commit_paths};

pub(super) fn strip_git_commit_politeness(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim_start();
        let prefix_len = [
            "请你帮我",
            "請你幫我",
            "请帮我",
            "請幫我",
            "请你",
            "請你",
            "请",
            "請",
            "帮我",
            "幫我",
            "只",
            "仅",
            "僅",
            "麻烦",
            "麻煩",
            "请执行 ",
            "請執行 ",
            "执行 ",
            "執行 ",
            "执行",
            "執行",
            "直接",
            "现在",
            "現在",
            "please,",
            "please ",
            "now ",
            "go ahead and ",
        ]
        .iter()
        .find_map(|prefix| {
            marker_matches_case_insensitive(trimmed, prefix).then_some(prefix.len())
        });
        match prefix_len {
            Some(len) => text = &trimmed[len..],
            None => return trimmed,
        }
    }
}

/// A natural-language commit phrase at the start of a command.
#[derive(Debug, Clone, Copy)]
pub(super) struct NaturalPrefix {
    /// Byte length of the phrase in the lowercased command.
    pub(super) len: usize,
    /// The phrase commits only the paths that follow it (`提交这些文件`).
    pub(super) requires_scope: bool,
    /// The phrase names what it commits (代码, 改动, Git 记录, "these changes"),
    /// so a tail that is neither a path nor a modifier fails closed as a
    /// malformed commit. Without such an object (`提交`, `确认提交`,
    /// `创建一个提交`, `提交文件`, a bare `commit`), the same tail means the
    /// words describe something else, such as a submit flow.
    pub(super) names_object: bool,
    /// The words are unmistakably Git. `提交` alone is also the everyday
    /// "submit", so only Git wording still names a commit when it opens a
    /// later clause of the request.
    pub(super) git_wording: bool,
}

impl NaturalPrefix {
    const fn new(len: usize, requires_scope: bool, names_object: bool, git_wording: bool) -> Self {
        Self {
            len,
            requires_scope,
            names_object,
            git_wording,
        }
    }
}

pub(super) fn natural_git_commit_prefix(command: &str) -> Option<NaturalPrefix> {
    const ALL_DIRTY: &[&str] = &[
        "把这些变更提交",
        "把這些變更提交",
        "将这些变更提交",
        "將這些變更提交",
        "把当前改动提交",
        "把當前改動提交",
        "将当前改动提交",
        "將當前改動提交",
        "把当前变更提交",
        "把當前變更提交",
        "将当前变更提交",
        "將當前變更提交",
        "提交这些变更",
        "提交這些變更",
        "提交当前改动",
        "提交當前改動",
        "提交这些改动",
        "提交這些改動",
        "提交本次改动",
        "提交本次變動",
        "提交本次變更",
        "提交git记录",
        "提交git紀錄",
        "提交git纪录",
        "提交 git 记录",
        "提交 git 紀錄",
        "提交 git 纪录",
        "创建一个git提交",
        "建立一個git提交",
        "做一次git提交",
        "执行git提交",
        "執行git提交",
        "执行gitcommit",
        "執行gitcommit",
        "运行gitcommit",
        "運行gitcommit",
        "git提交",
    ];
    // Also the words for a submit button or a form flow (确认提交按钮,
    // 创建一个提交按钮), so they commit only when nothing else follows.
    const SUBMIT_OR_COMMIT: &[&str] = &[
        "确认提交",
        "確認提交",
        "确定提交",
        "確定提交",
        "创建一个提交",
        "創建一個提交",
        "建立一个提交",
        "建立一個提交",
        "创建一次提交",
        "創建一次提交",
    ];
    for prefix in ALL_DIRTY {
        if command.starts_with(prefix) {
            return Some(NaturalPrefix::new(prefix.len(), false, true, true));
        }
    }
    for prefix in ["提交后总结", "提交後總結"] {
        if command.starts_with(prefix) {
            return Some(NaturalPrefix::new(prefix.len(), false, true, false));
        }
    }
    if let Some((len, names_object)) = vcs_object_commit_prefix(command) {
        return Some(NaturalPrefix::new(len, false, names_object, names_object));
    }
    for prefix in SUBMIT_OR_COMMIT {
        if command.starts_with(prefix) {
            return Some(NaturalPrefix::new(prefix.len(), false, false, false));
        }
    }
    // `create a commit hook` names a tool: see `commit_phrase_is_modifier`.
    for prefix in [
        "commit these changes",
        "commit all current changes",
        "commit all changes",
        "commit changes",
        "commit the current changes",
        "commit the changes",
        "commit current changes",
        "commit my changes",
        "commit this change",
        "commit the code",
        "commit my code",
        "commit everything",
        "commit it",
        "commit now",
        "make one commit",
        "make a commit",
        "create one commit",
        "create a commit",
    ] {
        if english_phrase_starts(command, prefix) {
            return Some(NaturalPrefix::new(prefix.len(), false, true, true));
        }
    }

    for prefix in ["提交这些文件", "提交這些文件"] {
        if command.starts_with(prefix) {
            return Some(NaturalPrefix::new(prefix.len(), true, true, false));
        }
    }
    if english_phrase_starts(command, "commit these files") {
        return Some(NaturalPrefix::new(
            "commit these files".len(),
            true,
            true,
            true,
        ));
    }
    // `提交文件时显示上传进度` submits files in a product.
    if command.starts_with("提交文件") {
        return Some(NaturalPrefix::new("提交文件".len(), true, false, false));
    }
    // A bare `commit` commits everything, or the paths that follow it.
    if english_phrase_starts(command, "commit") {
        return Some(NaturalPrefix::new("commit".len(), false, false, true));
    }
    if command.starts_with("提交") {
        return Some(NaturalPrefix::new("提交".len(), true, false, false));
    }
    None
}

/// Whether `command` starts with the English `phrase` as whole words, so
/// `make a commit` does not match `make a commitment`.
fn english_phrase_starts(command: &str, phrase: &str) -> bool {
    command.strip_prefix(phrase).is_some_and(|tail| {
        tail.chars()
            .next()
            .is_none_or(|first| !first.is_ascii_alphanumeric())
    })
}

/// What a commit takes when named next to `提交`: code or changes.
pub(super) const COMMIT_OBJECTS: &[&str] = &[
    "代码", "代碼", "改动", "改動", "变更", "變更", "变动", "變動",
];

/// `修改` / `更改` also read as "edited" (提交修改后的表单), so they name what a
/// commit takes only after a determiner or in the object-first 把 form.
pub(super) const COMMIT_EDITS: &[&str] = &["修改", "更改"];

/// Determiners between `提交` and what it commits: 当前, 这些, 所有, 刚才的…
pub(super) const COMMIT_DETERMINERS: &[&str] = &[
    "当前的",
    "當前的",
    "当前",
    "當前",
    "这些",
    "這些",
    "这次的",
    "這次的",
    "这次",
    "這次",
    "本次的",
    "本次",
    "所有的",
    "所有",
    "全部的",
    "全部",
    "刚才的",
    "剛才的",
    "刚才",
    "剛才",
    "刚刚的",
    "剛剛的",
    "刚刚",
    "剛剛",
    "目前的",
    "现在的",
    "現在的",
    "我的",
];

/// Strip one determiner (当前, 这些, 所有…) and report whether there was one.
pub(super) fn strip_commit_determiner(text: &str) -> (bool, &str) {
    COMMIT_DETERMINERS
        .iter()
        .find_map(|determiner| text.strip_prefix(determiner))
        .map_or((false, text), |rest| (true, rest))
}

/// The VCS object at the start of `text` (代码, 改动, 变更, 修改…): its byte
/// length, and whether it is unmistakable (see [`COMMIT_EDITS`]).
pub(super) fn commit_object(text: &str, determined: bool) -> Option<(usize, bool)> {
    if let Some(object) = COMMIT_OBJECTS
        .iter()
        .find(|object| text.starts_with(**object))
    {
        return Some((object.len(), true));
    }
    COMMIT_EDITS
        .iter()
        .find(|edit| text.starts_with(**edit))
        .map(|edit| (edit.len(), determined))
}

/// `提交` + an optional `一下` and determiner + a VCS object (提交代码,
/// 帮我提交一下代码, 提交所有修改), or the object-first 把/将 form (把代码提交了,
/// 把刚才的修改提交). Returns the phrase length and whether it names its object
/// unmistakably. The object must end the phrase: `提交代码审查` and `提交变更单`
/// name a review and a form, not code to commit.
fn vcs_object_commit_prefix(command: &str) -> Option<(usize, bool)> {
    if let Some(rest) = command.strip_prefix("提交") {
        let rest = rest.strip_prefix("一下").unwrap_or(rest);
        let (determined, rest) = strip_commit_determiner(rest);
        let (object_len, unmistakable) = commit_object(rest, determined)?;
        let after = &rest[object_len..];
        return commit_object_ends(after).then_some((command.len() - after.len(), unmistakable));
    }
    let rest = ["把", "将", "將"]
        .iter()
        .find_map(|lead| command.strip_prefix(lead))?;
    let (_, rest) = strip_commit_determiner(rest);
    let (object_len, _) = commit_object(rest, true)?;
    let after = rest[object_len..].strip_prefix("提交")?;
    let after = after.strip_prefix("了").unwrap_or(after);
    Some((command.len() - after.len(), true))
}

/// Whether the words after a VCS object end the commit phrase: the end of the
/// text, a space or clause mark, a sequencing word, or a known tail (吧, 一下,
/// 到本地仓库, 时, 前).
fn commit_object_ends(after: &str) -> bool {
    after.is_empty()
        || after.starts_with(|ch: char| {
            ch.is_whitespace()
                || matches!(
                    ch,
                    ',' | '，' | '、' | ';' | '；' | ':' | '：' | '!' | '！' | '?' | '？' | '。'
                )
        })
        || [
            "吧", "一下", "到", "进", "進", "时", "時", "前", "后", "後", "的", "然后", "然後",
            "并", "並", "再", "和", "与", "與", "接着", "接著", "同时", "同時", "之后", "之後",
            "以后", "以後", "完",
        ]
        .iter()
        .any(|marker| after.starts_with(marker))
}

/// Whether the words after a commit phrase make it name a moment or a thing
/// instead of ordering a commit: `提交代码时…`, `提交按钮`, `git commit 前自动跑 lint`,
/// `create a commit hook`.
pub(super) fn commit_phrase_is_modifier(tail: &str) -> bool {
    const NOUNS: &[&str] = &[
        "按钮", "按鈕", "页面", "頁面", "界面", "接口", "表单", "表單", "功能", "模块", "模塊",
        "组件", "組件", "弹窗", "彈窗", "流程", "逻辑", "邏輯", "模板", "钩子", "鉤子", "规范",
        "規範", "次数", "次數", "权限", "權限", "脚本", "腳本", "记录", "記錄", "纪录", "紀錄",
        "历史", "歷史", "消息", "訊息", "信息",
    ];
    let tail = tail.trim_start();
    // A habit rather than a one-off order: 提交代码前自动跑 lint, 提交后都要通知.
    let habit = [
        "之前", "之后", "之後", "以前", "以后", "以後", "前", "后", "後",
    ]
    .iter()
    .any(|lead| {
        tail.strip_prefix(lead).is_some_and(|rest| {
            ["自动", "自動", "都", "会", "會"]
                .iter()
                .any(|marker| rest.trim_start().starts_with(marker))
        })
    });
    habit
        || ["时", "時", "的"]
            .iter()
            .any(|marker| tail.starts_with(marker))
        || NOUNS.iter().any(|noun| tail.starts_with(noun))
        || tail
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .next()
            .is_some_and(|word| {
                matches!(
                    word.to_ascii_lowercase().as_str(),
                    "hook"
                        | "hooks"
                        | "template"
                        | "templates"
                        | "message"
                        | "messages"
                        | "history"
                        | "log"
                        | "logs"
                        | "button"
                        | "count"
                )
            })
}

/// Whether the words after a commit phrase go on to more Git or delivery work
/// (`然后推送`, `后运行测试`, `and push`). After a phrase that names no object,
/// anything else (`后跳转到首页`) describes a submit flow instead.
pub(super) fn commit_tail_chains_git_work(tail: &str) -> bool {
    const CONNECTORS: &[&str] = &[
        "完成后",
        "完成後",
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
        "并",
        "並",
        "后",
        "後",
        "再",
        "完",
        "and then ",
        "then ",
        "and ",
        "&&",
    ];
    const WORK: &[&str] = &[
        "推送",
        "运行",
        "運行",
        "执行",
        "執行",
        "测试",
        "測試",
        "跑",
        "部署",
        "发布",
        "發布",
        "上线",
        "上線",
        "合并",
        "合併",
        "打标签",
        "打標籤",
    ];
    let lower = tail.trim_start().to_lowercase();
    let Some(rest) = CONNECTORS
        .iter()
        .find_map(|connector| lower.strip_prefix(connector))
    else {
        return false;
    };
    let rest = rest.trim_start();
    let rest = ["再", "就", "马上", "馬上", "立即"]
        .iter()
        .find_map(|marker| rest.strip_prefix(marker))
        .unwrap_or(rest);
    let notifies = ["推送通知", "推送消息", "推送给", "推送給"]
        .iter()
        .any(|marker| rest.starts_with(marker));
    (WORK.iter().any(|work| rest.starts_with(work)) && !notifies)
        || rest
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .next()
            .is_some_and(|word| {
                matches!(
                    word,
                    "push"
                        | "run"
                        | "test"
                        | "deploy"
                        | "release"
                        | "publish"
                        | "merge"
                        | "amend"
                        | "tag"
                )
            })
}

pub(super) fn trim_natural_commit_tail(mut tail: &str) -> &str {
    tail = tail.trim_start_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, ',' | '，' | '、' | ':' | '：')
    });
    tail = tail.trim_end_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, ',' | '，' | '、' | ':' | '：' | '.' | '。' | '!' | '！')
    });
    if let Some(index) = find_safe_git_receipt_suffix(tail) {
        tail = &tail[..index];
    }
    tail = tail.trim_start_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, ',' | '，' | '、' | ':' | '：')
    });
    tail = tail.trim_end_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, ',' | '，' | '、' | ':' | '：' | '.' | '。' | '!' | '！')
    });
    for suffix in [
        "到 git 仓库",
        "到 git 倉庫",
        "到git仓库",
        "到git倉庫",
        "进 git 仓库",
        "進 git 倉庫",
        "进git仓库",
        "進git倉庫",
        "到本地 git 仓库",
        "到本地 git 倉庫",
        "到本地git仓库",
        "到本地git倉庫",
        "到本地仓库",
        "到本地倉庫",
        "一下",
        "吧",
        " now",
    ] {
        let lower = tail.to_lowercase();
        if lower.ends_with(suffix) {
            tail = tail[..tail.len() - suffix.len()]
                .trim_end_matches(|ch: char| ch.is_whitespace() || matches!(ch, ',' | '，' | '、'));
        }
    }
    tail
}

/// Whether an all-dirty commit request is followed only by a scope-narrowing
/// modifier, rather than another piece of work. This deliberately accepts a
/// small exact language: unknown prose remains compound/invalid and is never
/// swallowed into the host-owned commit lane.
pub(super) fn natural_git_commit_tail_is_safe_constraint(tail: &str) -> bool {
    let trimmed = tail.trim_matches(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                ',' | '，' | '、' | ';' | '；' | ':' | '：' | '.' | '。' | '!' | '！'
            )
    });
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_lowercase();
    if [
        "即可",
        "就行",
        "就好",
        "就可以",
        "就可以了",
        "而已",
        "only",
        "only commit",
        "commit only",
    ]
    .contains(&lower.as_str())
    {
        return true;
    }

    let clauses = lower.split([',', '，', '、', ';', '；', '/', '／']);
    let mut saw_clause = false;
    for raw in clauses {
        let mut clause = raw.trim();
        while let Some(rest) = [
            "并且", "並且", "并", "並", "同时", "同時", "然后", "然後", "and ",
        ]
        .iter()
        .find_map(|prefix| clause.strip_prefix(prefix))
        {
            clause = rest.trim_start();
        }
        if clause.is_empty() {
            continue;
        }
        saw_clause = true;
        if ![
            "即可",
            "就行",
            "就好",
            "就可以",
            "就可以了",
            "而已",
            "only",
            "only commit",
            "commit only",
            "不要跑评审",
            "不要跑評審",
            "不要评审",
            "不要評審",
            "不要启动评审",
            "不要啟動評審",
            "不要团队评审",
            "不要團隊評審",
            "不要跑qc",
            "不要运行qc",
            "不要運行qc",
            "不要修改代码",
            "不要修改代碼",
            "不要改代码",
            "不要改代碼",
            "不要修改文件",
            "不要改文件",
            "不要做其他事情",
            "不要做其它事情",
            "不要做额外工作",
            "不要做額外工作",
            "不要运行测试",
            "不要運行測試",
            "不要跑测试",
            "不要跑測試",
            "别跑评审",
            "別跑評審",
            "别评审",
            "別評審",
            "别改代码",
            "別改代碼",
            "别改文件",
            "別改文件",
            "do not review",
            "don't review",
            "dont review",
            "do not run reviews",
            "don't run reviews",
            "dont run reviews",
            "do not modify code",
            "don't modify code",
            "dont modify code",
            "do not edit files",
            "don't edit files",
            "dont edit files",
            "do nothing else",
        ]
        .contains(&clause)
        {
            return false;
        }
    }
    saw_clause
}

pub(super) fn find_safe_git_receipt_suffix(text: &str) -> Option<usize> {
    const STARTS: &[&str] = &[
        "然后", "然後", "后", "後", "并", "並", "接着", "接著", "and ", "then ",
    ];
    const MAX_RECEIPT_SUFFIX_BYTES: usize = 512;
    let mut window_start = text.len().saturating_sub(MAX_RECEIPT_SUFFIX_BYTES);
    while window_start < text.len() && !text.is_char_boundary(window_start) {
        window_start += 1;
    }
    let mut earliest = None;
    for (relative_index, _) in text[window_start..].char_indices() {
        let index = window_start + relative_index;
        for marker in STARTS {
            if marker_matches_case_insensitive(&text[index..], marker)
                && git_receipt_suffix_is_safe(&text[index..])
            {
                earliest = Some(earliest.map_or(index, |current: usize| current.min(index)));
            }
        }
    }
    earliest
}

fn git_receipt_suffix_is_safe(text: &str) -> bool {
    let trimmed = text.trim_matches(|ch: char| {
        ch.is_whitespace()
            || matches!(
                ch,
                ',' | '，' | '、' | ';' | '；' | ':' | '：' | '.' | '。' | '!' | '！'
            )
    });
    let lower = trimmed.to_lowercase();
    let english = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    if [
        "and tell me the hash",
        "then tell me the hash",
        "and tell me the commit hash",
        "then tell me the commit hash",
        "and show me the hash",
        "then show me the hash",
        "and show me the commit hash",
        "then show me the commit hash",
        "and report the hash",
        "then report the hash",
        "and report the commit hash",
        "then report the commit hash",
        "and give me the hash",
        "then give me the hash",
        "and summarize the commit",
        "then summarize the commit",
        "and summarise the commit",
        "then summarise the commit",
        "and summarize this commit",
        "then summarize this commit",
        "and summarize the committed changes",
        "then summarize the committed changes",
        "and give me a summary of the commit",
        "then give me a summary of the commit",
        "and report the commit result",
        "then report the commit result",
    ]
    .contains(&english.as_str())
    {
        return true;
    }

    let compact: String = lower
        .chars()
        .filter(|ch| {
            !ch.is_whitespace()
                && !matches!(
                    ch,
                    ',' | '，' | '、' | ';' | '；' | ':' | '：' | '.' | '。' | '!' | '！'
                )
        })
        .collect();
    [
        "然后告诉我hash",
        "然後告訴我hash",
        "后告诉我hash",
        "後告訴我hash",
        "并告诉我hash",
        "並告訴我hash",
        "接着告诉我hash",
        "接著告訴我hash",
        "然后告诉我哈希",
        "然後告訴我哈希",
        "后告诉我哈希",
        "後告訴我哈希",
        "并告诉我哈希",
        "並告訴我哈希",
        "接着告诉我哈希",
        "接著告訴我哈希",
        "然后告诉我提交哈希",
        "然後告訴我提交哈希",
        "后告诉我提交哈希",
        "後告訴我提交哈希",
        "并告诉我提交哈希",
        "並告訴我提交哈希",
        "接着告诉我提交哈希",
        "接著告訴我提交哈希",
        "然后给我hash",
        "然後給我hash",
        "后给我hash",
        "後給我hash",
        "然后给我哈希",
        "然後給我哈希",
        "后给我哈希",
        "後給我哈希",
        "后总结",
        "後總結",
        "然后总结本次提交",
        "然後總結本次提交",
        "然后总结这次提交",
        "然後總結這次提交",
        "后总结本次提交",
        "後總結本次提交",
        "后汇报提交结果",
        "後匯報提交結果",
        "然后汇报提交结果",
        "然後匯報提交結果",
        "并返回hash",
        "並返回hash",
        "然后返回hash",
        "然後返回hash",
        "并返回哈希",
        "並返回哈希",
        "然后返回哈希",
        "然後返回哈希",
        "并返回提交哈希",
        "並返回提交哈希",
        "然后返回提交哈希",
        "然後返回提交哈希",
    ]
    .contains(&compact.as_str())
}

pub(super) fn parse_paths(tail: &str) -> Option<Vec<String>> {
    parse_git_commit_paths(tail)
}
