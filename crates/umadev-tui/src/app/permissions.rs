//! Permission questions and Codex sandbox presentation.
//!
//! These answers come from host state, not from a base model. Keeping the
//! classifier conservative prevents a permission preface from swallowing a
//! real edit request.

use umadev_agent::{config::CodexSandbox, TrustMode};

const ZH_QUESTIONS: &[&str] = &[
    "怎么给你权限",
    "如何给你权限",
    "怎么给权限",
    "如何给权限",
    "怎么授权",
    "如何授权",
    "怎麼給你權限",
    "如何給你權限",
    "怎麼給權限",
    "如何給權限",
    "怎麼授權",
    "如何授權",
    "为什么只读",
    "为什么是只读",
    "为何只读",
    "為什麼唯讀",
    "為什麼是唯讀",
    "為何唯讀",
    "你能写文件吗",
    "能写文件吗",
    "能不能写文件",
    "可以写文件吗",
    "你能修改文件吗",
    "能修改文件吗",
    "可以修改文件吗",
    "你有写权限吗",
    "当前能写吗",
    "能否写入",
    "怎么切换为可写会话",
    "如何切换为可写会话",
    "怎么开启可写会话",
    "如何开启可写会话",
    "我授权你操作文件",
    "授权你操作文件",
    "我授权你修改文件",
    "允许你写文件",
    "现在是可写会话",
    "现在是可写会话吗",
    "当前是可写会话",
    "当前是可写会话吗",
    "codex是可写的为什么umadev说只读",
    "codex是可写的，为什么umadev说只读",
    "codex可写为什么umadev说只读",
    "你能寫檔案嗎",
    "能寫檔案嗎",
    "能不能寫檔案",
    "可以寫檔案嗎",
    "你能修改檔案嗎",
    "能修改檔案嗎",
    "可以修改檔案嗎",
    "你有寫入權限嗎",
    "當前能寫嗎",
    "能否寫入",
    "怎麼切換為可寫會話",
    "如何切換為可寫會話",
    "怎麼開啟可寫會話",
    "如何開啟可寫會話",
    "我授權你操作檔案",
    "授權你操作檔案",
    "我授權你修改檔案",
    "允許你寫檔案",
    "現在是可寫會話",
    "現在是可寫會話嗎",
    "當前是可寫會話",
    "當前是可寫會話嗎",
    "codex是可寫的為什麼umadev說唯讀",
    "codex是可寫的，為什麼umadev說唯讀",
    "codex可寫為什麼umadev說唯讀",
];

const EN_QUESTIONS: &[&str] = &[
    "how do i give you permission",
    "how can i give you permission",
    "how do i grant you permission",
    "how can i grant you permission",
    "why are you read-only",
    "why are you read only",
    "can you write files",
    "can you edit files",
    "do you have write access",
    "are you able to write files",
    "how do i switch to a writable session",
    "how can i switch to a writable session",
    "how do i enable a writable session",
    "i grant you permission to edit files",
    "you have permission to edit files",
    "is this session writable",
    "is the current session writable",
    "codex is writable why does umadev say read-only",
    "codex is writable, why does umadev say read-only",
    "codex is writable why does umadev say read only",
    "codex is writable, why does umadev say read only",
];

/// Match only a standalone permission question. Polite wrappers are accepted,
/// but any remaining text makes the request fall through to normal intent
/// routing; e.g. “你能写文件吗？请修改登录页面” must remain an edit request.
pub(super) fn is_pure_question(normalized: &str, compact: &str) -> bool {
    if ZH_QUESTIONS.contains(&compact) || EN_QUESTIONS.contains(&normalized) {
        return true;
    }

    let mut zh = compact;
    for prefix in [
        "请问",
        "請問",
        "麻烦告诉我",
        "麻煩告訴我",
        "请告诉我",
        "請告訴我",
        "我想知道",
        "你现在",
        "你現在",
        "你目前",
        "您现在",
        "您現在",
        "您目前",
        "现在",
        "現在",
        "目前",
    ] {
        if let Some(rest) = zh.strip_prefix(prefix) {
            zh = rest;
            break;
        }
    }
    for suffix in ["呢", "呀", "啊", "吧", "可以吗", "可以嗎"] {
        if let Some(rest) = zh.strip_suffix(suffix) {
            zh = rest;
            break;
        }
    }
    if ZH_QUESTIONS.contains(&zh) {
        return true;
    }

    let mut en = normalized;
    for prefix in [
        "please ",
        "could you tell me ",
        "can you tell me ",
        "i want to know ",
    ] {
        if let Some(rest) = en.strip_prefix(prefix) {
            en = rest;
            break;
        }
    }
    for suffix in [" please", " right now", " currently"] {
        if let Some(rest) = en.strip_suffix(suffix) {
            en = rest;
            break;
        }
    }
    if EN_QUESTIONS.contains(&en) {
        return true;
    }
    false
}

pub(super) fn slash_mutates_workspace(command: &str, rest: &str) -> bool {
    match command {
        "init" | "adopt" | "preview" | "deploy" | "pr" | "checkpoint" | "bug" => true,
        "revise" | "rewind" | "design" | "template" | "sandbox" => !rest.is_empty(),
        "memory" => super::parse_memory_command(rest).is_ok_and(|command| command.mutates()),
        _ => false,
    }
}

pub(super) fn reply(
    lang: umadev_i18n::Lang,
    backend: Option<&str>,
    trust: TrustMode,
    codex_sandbox: CodexSandbox,
) -> String {
    let mut lines = vec![umadev_i18n::tf(
        lang,
        "live_meta.permissions.state",
        &[trust.as_str()],
    )];
    if backend == Some("codex") {
        lines.push(umadev_i18n::tf(
            lang,
            "live_meta.permissions.codex_sandbox",
            &[codex_sandbox.as_codex_arg()],
        ));
        let guidance = if trust == TrustMode::Plan {
            "live_meta.permissions.plan"
        } else if codex_sandbox == CodexSandbox::ReadOnly {
            "live_meta.permissions.codex_read_only"
        } else {
            "live_meta.permissions.ready"
        };
        lines.push(umadev_i18n::t(lang, guidance).to_string());
    } else if trust == TrustMode::Plan {
        lines.push(umadev_i18n::t(lang, "live_meta.permissions.plan").to_string());
    } else {
        lines.push(umadev_i18n::t(lang, "live_meta.permissions.other_base").to_string());
    }
    lines.join("\n")
}

/// Publish the configured Codex preference. The host applies the current trust
/// ceiling when it opens a concrete session.
pub(super) fn resolve_and_publish_codex_sandbox(project_root: &std::path::Path) {
    if umadev_host::codex_session::codex_sandbox_override()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return;
    }
    let _ = umadev_agent::config::migrate_legacy_generated_codex_sandbox(project_root);
    let mode = umadev_agent::config::load_project_config(project_root)
        .codex
        .resolved_sandbox();
    umadev_host::codex_session::set_codex_sandbox(Some(mode.as_codex_arg()));
}

pub(crate) fn codex_read_only_sandbox_blocks_route(
    backend: &str,
    native_command: bool,
    execution_read_only: bool,
    mutates_workspace: bool,
    resolved_sandbox: &str,
) -> bool {
    backend == "codex"
        && !native_command
        && !execution_read_only
        && mutates_workspace
        && resolved_sandbox == "read-only"
}
