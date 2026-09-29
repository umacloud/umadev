//! App-runtime-model awareness — the **built app's** runtime LLM is a separate
//! concern from the **dev base** UmaDev wields.
//!
//! ## Why this exists (user-reported gap)
//!
//! At init the user picks the DEV BASE — the brain UmaDev borrows to write code
//! (e.g. Claude Code). But when UmaDev builds an app that itself calls an LLM at
//! RUNTIME (a chatbot, a RAG service, an AI assistant), the base, left unguided,
//! tends to hardcode the BUILT APP's runtime engine to the same vendor it is
//! itself (Anthropic / Claude — `ANTHROPIC_API_KEY` + the Claude API). A user who
//! wants the delivered app to run on, say, Qwen Max then has to hand-edit the
//! generated backend. The dev base and the app's runtime model are two DIFFERENT
//! things; conflating them is the bug.
//!
//! This module is the deterministic, fail-open detection + guidance that makes the
//! generation AWARE of the distinction:
//!
//! - [`app_calls_llm_at_runtime`] — does the requirement describe an app that
//!   calls an LLM at runtime?
//! - [`stated_runtime_model`] — did the user name a runtime model / provider
//!   (e.g. "运行时用千问 Max", "用 DashScope", "OpenAI")?
//! - [`runtime_model_directive`] — the firmware / generation block that tells the
//!   base to treat the app's runtime model + API as a USER-CONFIGURABLE choice
//!   (env-driven provider layer: model id + base URL + key var), default it to
//!   what the user named, and NEVER silently hardcode the dev base's vendor.
//!
//! Pure string analysis — no I/O, no network, deterministic. The directive is
//! empty unless the app actually calls an LLM at runtime, so it spends tokens only
//! when relevant; everything is fail-open by construction (a non-match returns
//! `false` / `None` / an empty string, never an error).

/// Word-boundary match for a short ASCII token, so risky 3-letter signals (`rag`,
/// `gpt`, `llm`, `glm`) don't false-positive inside unrelated words ("storage"
/// contains "rag", "campaign" almost contains "ai"). Splits the (already
/// lowercased) haystack on every non-alphanumeric byte and compares whole tokens.
fn has_word(haystack_lower: &str, word: &str) -> bool {
    haystack_lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|tok| tok == word)
}

/// Multi-character substrings that unambiguously signal "this app calls an LLM at
/// runtime" — CJK phrases and multi-word English that are long enough to match by
/// plain `contains` without false positives.
const RUNTIME_LLM_SUBSTRINGS: &[&str] = &[
    // ── Chinese ──────────────────────────────────────────────────────────────
    "大模型",
    "大语言模型",
    "语言模型",
    "聊天机器人",
    "对话机器人",
    "智能问答",
    "智能客服",
    "知识库问答",
    "智能助手",
    "ai助手",
    "ai 助手",
    "写作助手",
    "智能体",
    "检索增强",
    "生成式",
    "提示词",
    "文本生成",
    "智能摘要",
    "对话系统",
    "智能对话",
    "ai应用",
    "ai 应用",
    "ai写作",
    "ai 写作",
    "ai生成",
    "ai 生成",
    "大模型应用",
    // ── English (multi-word / long enough for contains) ──────────────────────
    "language model",
    "generative ai",
    "gen ai",
    "ai assistant",
    "ai chatbot",
    "ai chat",
    "ai-powered",
    "ai powered",
    "retrieval augmented",
    "retrieval-augmented",
    "chat completion",
    "function calling",
];

/// Short ASCII tokens that signal a runtime LLM but need a word-boundary match
/// (so they don't fire inside unrelated words). Each also implies the app calls a
/// model at runtime. Provider names live in [`PROVIDER_PATTERNS`] and are checked
/// the same way.
const RUNTIME_LLM_WORDS: &[&str] = &["llm", "gpt", "chatgpt", "rag", "copilot"];

/// Tokens that signal a runtime LLM ONLY when an `ai` token co-occurs (MEDIUM M8).
/// Bare `agent` / `prompt` are common in NON-AI domains — "real estate agent CRM",
/// "travel agent booking", "prompt the user to confirm payment" — so on their own they
/// false-flagged ordinary apps. Requiring an accompanying `ai` keeps "AI agent" /
/// "AI prompt" while dropping the non-AI false positives.
const RUNTIME_LLM_WORDS_AI_QUALIFIED: &[&str] = &["agent", "prompt"];

/// The canonical label of an Anthropic Claude runtime (see [`stated_runtime_model`]).
const CLAUDE_RUNTIME_LABEL: &str = "Anthropic Claude";

/// The coding bases / dev CLIs by product name. Naming one says which tool WRITES
/// the app ("用 Claude Code 做一个记账本"), not a model the app calls at runtime.
const DEV_BASE_NAMES: &[&str] = &[
    "claude code",
    "claude-code",
    "claudecode",
    "kimi code",
    "kimi-code",
    "kimi cli",
    "gemini cli",
    "gemini-cli",
];

/// Phrases that, right after "用 / 使用 / 让 <model>", make the model the DEV tool
/// building the product ("用 kimi 帮我写个待办") rather than the app's runtime
/// engine ("运行时用 kimi", "用 kimi 来回答用户问题" keep the model).
const DEV_TOOL_VERBS: &[&str] = &[
    "帮我",
    "帮忙",
    "做一个",
    "做个",
    "做一款",
    "做一套",
    "写一个",
    "写个",
    "写一款",
    "来做",
    "来写",
    "来开发",
    "开发",
    "实现一个",
    "搭一个",
    "搭个",
    "搭建",
    "构建",
    "编写",
    "编程",
    "to build",
    "to make",
    "to create",
    "to develop",
    "to code",
    "to implement",
];

/// Lowercase `requirement` with every mention of the DEV tool blanked out — a
/// base product name, or a model named as the one doing the work — so provider
/// detection only sees what the BUILT APP calls at runtime.
fn without_dev_tool_mentions(requirement: &str) -> String {
    let mut lower = requirement.to_lowercase();
    for name in DEV_BASE_NAMES {
        lower = lower.replace(name, " ");
    }
    let leads = ["使用", "用", "让", "using", "use"];
    for matcher in PROVIDER_PATTERNS.iter().flat_map(|(matchers, _)| matchers.iter()) {
        let mut from = 0;
        while let Some(offset) = lower[from..].find(matcher) {
            let start = from + offset;
            let end = start + matcher.len();
            let before = lower[..start].trim_end();
            let after = lower[end..].trim_start();
            let led = leads.iter().any(|lead| before.ends_with(lead));
            if led && DEV_TOOL_VERBS.iter().any(|verb| after.starts_with(verb)) {
                lower.replace_range(start..end, &" ".repeat(matcher.len()));
            }
            from = end;
        }
    }
    lower
}

/// Provider / model recognisers, in priority order (more specific first). Each
/// entry is `(matchers, canonical_label)`: if ANY matcher hits (CJK matchers by
/// `contains`, ASCII matchers by [`has_word`]), the canonical label names the
/// provider the user asked for — including the note that DashScope/Qwen, DeepSeek,
/// Zhipu, Moonshot, etc. speak the OpenAI-compatible wire protocol.
const PROVIDER_PATTERNS: &[(&[&str], &str)] = &[
    (
        &["qwen", "千问", "通义", "dashscope", "灵积"],
        "Qwen / 通义千问 (DashScope, OpenAI-compatible API)",
    ),
    (
        &["openai", "gpt", "chatgpt"],
        "OpenAI (GPT, OpenAI-compatible API)",
    ),
    (
        &["deepseek", "深度求索"],
        "DeepSeek (OpenAI-compatible API)",
    ),
    (
        &["智谱", "glm", "chatglm", "zhipu"],
        "Zhipu GLM / 智谱 (OpenAI-compatible API)",
    ),
    (
        &["kimi", "moonshot", "月之暗面"],
        "Moonshot Kimi / 月之暗面 (OpenAI-compatible API)",
    ),
    (
        &["文心", "ernie", "wenxin", "千帆", "qianfan"],
        "Baidu ERNIE / 文心一言",
    ),
    (&["豆包", "doubao"], "Doubao / 豆包"),
    (&["混元", "hunyuan"], "Tencent Hunyuan / 混元"),
    (&["gemini"], "Google Gemini"),
    (&["claude", "anthropic"], CLAUDE_RUNTIME_LABEL),
    (
        &["ollama", "llama", "本地模型", "本地大模型", "私有化部署"],
        "a local / self-hosted model (Ollama / Llama, OpenAI-compatible API)",
    ),
];

/// Whether the requirement describes an app that calls an LLM at RUNTIME (a
/// chatbot / RAG service / AI assistant / agent / generative feature), as opposed
/// to an ordinary CRUD product that never touches a model. Naming a provider (see
/// [`stated_runtime_model`]) also counts — you only name Qwen/OpenAI when the app
/// will call it.
///
/// Deterministic, lowercase substring + word-boundary matching; fail-open (an
/// unrecognised requirement returns `false` → no directive, no tokens spent).
#[must_use]
pub fn app_calls_llm_at_runtime(requirement: &str) -> bool {
    let lower = without_dev_tool_mentions(requirement);
    if RUNTIME_LLM_SUBSTRINGS.iter().any(|s| lower.contains(s)) {
        return true;
    }
    // "ai app" / "ai apps" / "ai application" — matched at a WORD boundary (MEDIUM M8):
    // a plain `contains("ai app")` mis-fired on "Shang(hai app)" / "Du(bai app)", so it
    // is matched as the adjacent token pair `ai` + an `app…` token instead.
    if has_ai_app(&lower) {
        return true;
    }
    if RUNTIME_LLM_WORDS.iter().any(|w| has_word(&lower, w)) {
        return true;
    }
    // `agent` / `prompt` count only alongside an `ai` token (they are otherwise common
    // in non-AI domains) — see [`RUNTIME_LLM_WORDS_AI_QUALIFIED`].
    if has_word(&lower, "ai")
        && RUNTIME_LLM_WORDS_AI_QUALIFIED
            .iter()
            .any(|w| has_word(&lower, w))
    {
        return true;
    }
    // Naming a runtime provider/model is itself proof the app calls an LLM.
    stated_runtime_model(requirement).is_some()
}

/// Whether `lower` (already lowercased) contains the phrase "ai app" at a WORD
/// boundary — i.e. an `ai` token immediately followed by a token starting with `app`
/// (`app` / `apps` / `application`). This avoids the substring false-positive where
/// "shanghai app" / "dubai app" contain "ai app" mid-word. Splits on every
/// non-alphanumeric byte, mirroring [`has_word`].
fn has_ai_app(lower: &str) -> bool {
    let toks: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    toks.windows(2)
        .any(|w| w[0] == "ai" && w[1].starts_with("app"))
}

/// The runtime model / provider the user explicitly named for the BUILT APP, as a
/// canonical label (e.g. "运行时用千问 Max" → the Qwen/DashScope label), or `None`
/// when the requirement names no runtime model. The first matching provider in
/// the internal provider-pattern table wins (more specific patterns are listed first).
///
/// Deterministic + fail-open. ASCII matchers use whole-word matching so a bare "gpt"/
/// "glm" matches as a token but not inside an unrelated word.
#[must_use]
pub fn stated_runtime_model(requirement: &str) -> Option<&'static str> {
    let lower = without_dev_tool_mentions(requirement);
    for (matchers, label) in PROVIDER_PATTERNS {
        let hit = matchers.iter().any(|m| {
            if m.is_ascii() {
                has_word(&lower, m)
            } else {
                lower.contains(m)
            }
        });
        if hit {
            return Some(label);
        }
    }
    None
}

/// The one-line guard the work firmware prefixes onto [`runtime_model_directive`]:
/// keep the app's runtime model behind a provider layer and never silently
/// hardcode the dev base's vendor. Empty when the user NAMED Anthropic Claude as
/// the app's runtime model — then Claude is the requested default, not a leak.
#[must_use]
pub fn runtime_guard(requirement: &str) -> &'static str {
    if stated_runtime_model(requirement) == Some(CLAUDE_RUNTIME_LABEL) {
        return "";
    }
    "Runtime guard: use an OpenAI-compatible provider layer; NEVER silently hardcode \
     Anthropic / Claude or `ANTHROPIC_API_KEY`.\n"
}

/// The firmware / generation guidance block for a build whose app calls an LLM at
/// runtime: treat the app's runtime model + API as a USER-CONFIGURABLE choice
/// (env-driven provider layer — model id + base URL + API-key var), DEFAULT it to
/// what the user named, and NEVER silently hardcode the dev base's vendor
/// (Anthropic / Claude) as the app's runtime engine.
///
/// Returns an EMPTY string when the requirement does not describe a runtime-LLM
/// app — so a plain CRUD build spends no tokens on this. Pure + deterministic;
/// safe to call on every work turn.
#[must_use]
pub fn runtime_model_directive(requirement: &str) -> String {
    if !app_calls_llm_at_runtime(requirement) {
        return String::new();
    }
    let stated = stated_runtime_model(requirement);
    let default_clause = match stated {
        Some(model) => format!(
            "The user NAMED a runtime model — **{model}**: default the app's config to \
             THAT provider (its model id + base URL + the matching API-key env var). \
             Honor the user's choice; do not substitute a different vendor."
        ),
        None => String::from(
            "The user did NOT name a runtime model: do NOT default to the dev base's \
             vendor. Generate a clearly-labelled, swappable provider layer whose model \
             id + base URL + API-key var come from configuration (a documented \
             `.env` / config entry), and state in the delivered output that the runtime \
             model is configurable and how to switch it.",
        ),
    };
    // A user who chose Claude for the app's runtime gets Claude as the default;
    // everyone else gets the floor against silently inheriting the dev base's vendor.
    let vendor_floor = if stated == Some(CLAUDE_RUNTIME_LABEL) {
        ""
    } else {
        " NEVER silently hardcode Anthropic / Claude or `ANTHROPIC_API_KEY`."
    };
    format!(
        "## App runtime model — USER-CONFIGURABLE, not the dev base\n\
         {default_clause}\n\
         Minimum implementation contract: keep model id, base URL, and API-key \
         env-var name configurable; prefer an OpenAI-compatible client so Qwen, \
         DeepSeek, Zhipu, Moonshot, OpenAI, and local Ollama are config changes.{vendor_floor}\n\
         This build's app calls an LLM at RUNTIME. That runtime model is the USER'S \
         choice and is a SEPARATE concern from the base CLI this dev tool itself runs \
         on. Never silently substitute the dev base's provider as the app's runtime \
         engine.\n\
         - Put the LLM call behind a thin provider-abstraction layer: read the model \
         id, base URL, and API-key env-var NAME from configuration (env vars / a config \
         file) so the model can be swapped WITHOUT editing code.\n\
         - Keep provider-specific details behind that adapter instead of spreading \
         vendor branches through product code."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_runtime_llm_app_across_languages() {
        for req in [
            "做一个智能客服聊天机器人",
            "搭一个知识库问答(RAG)系统",
            "build an AI chatbot that answers support questions",
            "用大模型做一个写作助手",
            "an LLM-powered agent with function calling",
        ] {
            assert!(
                app_calls_llm_at_runtime(req),
                "should detect a runtime-LLM app: {req}"
            );
        }
    }

    #[test]
    fn plain_crud_builds_are_not_runtime_llm_apps() {
        // The whole point: an ordinary product must NOT trigger the directive, or
        // every build would carry it. Mirrors the requirements used in the firmware
        // tests so those stay clean.
        for req in [
            "做一个待办事项 SaaS 产品",
            "做一个带邮箱登录的 SaaS 落地页",
            "login oauth authentication",
            "在结算流程里修一个 bug",
            "用 JDK 编译并打包",
            "build something",
        ] {
            assert!(
                !app_calls_llm_at_runtime(req),
                "plain build must NOT be flagged as a runtime-LLM app: {req}"
            );
        }
    }

    #[test]
    fn short_ascii_signals_are_word_bounded() {
        // "rag"/"gpt"/"agent" must NOT match inside unrelated words.
        assert!(!app_calls_llm_at_runtime(
            "optimize image storage and caching"
        ));
        assert!(!app_calls_llm_at_runtime("a project management dashboard"));
        // …but a real token does match.
        assert!(app_calls_llm_at_runtime("add a RAG pipeline"));
        assert!(app_calls_llm_at_runtime("call the gpt model"));
    }

    #[test]
    fn bare_agent_prompt_and_ai_app_substring_do_not_false_flag_non_ai_apps() {
        // MEDIUM M8: bare `agent` / `prompt` are common in NON-AI domains, and a plain
        // "ai app" substring matches inside "Shanghai app" / "Dubai app". None of these
        // must be flagged as a runtime-LLM app (which would inject the ~1.2KB directive).
        for req in [
            "a real estate agent CRM",
            "travel agent booking platform",
            "prompt the user to confirm payment",
            "build a Shanghai app for restaurant reviews",
            "a Dubai app directory",
        ] {
            assert!(
                !app_calls_llm_at_runtime(req),
                "must NOT false-flag a non-AI app: {req}"
            );
        }
        // …but a genuine AI signal still fires: an `ai` token alongside agent/prompt, or
        // a real "ai app" word phrase.
        for req in [
            "build an AI agent for support",
            "an AI prompt playground",
            "ship an AI app for note-taking",
            "an AI application for triage",
        ] {
            assert!(
                app_calls_llm_at_runtime(req),
                "a real AI signal must still fire: {req}"
            );
        }
    }

    #[test]
    fn detects_explicit_runtime_provider() {
        assert_eq!(
            stated_runtime_model("运行时用千问 Max"),
            Some("Qwen / 通义千问 (DashScope, OpenAI-compatible API)")
        );
        assert!(stated_runtime_model("用 DashScope 接入")
            .unwrap()
            .contains("Qwen"));
        assert!(stated_runtime_model("the app should use OpenAI gpt-4o")
            .unwrap()
            .contains("OpenAI"));
        assert!(stated_runtime_model("运行时跑 DeepSeek")
            .unwrap()
            .contains("DeepSeek"));
        assert!(stated_runtime_model("用本地 Ollama 部署")
            .unwrap()
            .to_lowercase()
            .contains("local"));
        // No provider named → None.
        assert_eq!(stated_runtime_model("做一个聊天机器人"), None);
        assert_eq!(stated_runtime_model("做一个待办事项产品"), None);
    }

    #[test]
    fn directive_is_empty_for_a_non_ai_build() {
        assert!(runtime_model_directive("做一个待办事项 SaaS 产品").is_empty());
        assert!(runtime_model_directive("build a login page").is_empty());
    }

    #[test]
    fn directive_carries_the_configurable_contract() {
        let d = runtime_model_directive("做一个智能客服聊天机器人");
        assert!(!d.is_empty());
        // The core contract: separate concern, env-configurable, never hardcode Claude.
        assert!(d.contains("USER-CONFIGURABLE"));
        assert!(
            d.to_lowercase().contains("never silently hardcode")
                || d.contains("NEVER silently hardcode")
        );
        assert!(d.contains("ANTHROPIC_API_KEY") || d.contains("Anthropic / Claude"));
        assert!(d.to_lowercase().contains("openai-compatible"));
        // Unspecified model → the swappable-default + "note it's configurable" branch.
        assert!(d.contains("did NOT name a runtime model"));
    }

    #[test]
    fn directive_threads_the_explicit_model() {
        let d = runtime_model_directive("做一个聊天机器人,运行时用千问 Max");
        assert!(
            d.contains("NAMED a runtime model"),
            "honors the named model: {d}"
        );
        assert!(d.contains("Qwen"), "threads the Qwen/DashScope label: {d}");
        // Still carries the never-hardcode-Claude floor.
        assert!(d.contains("Anthropic / Claude"));
    }

    #[test]
    fn naming_the_dev_base_is_not_a_runtime_llm_app() {
        // S02-9: the dev base / the tool that WRITES the app is not a model the
        // app calls at runtime, and a plain Q&A forum is an ordinary CRUD app.
        for req in [
            "用 Claude Code 做一个记账本",
            "用 claude-code 帮我写一个博客",
            "用 kimi 帮我写个待办",
            "用 Kimi Code 做一个库存管理系统",
            "use claude to build a todo app",
            "做一个校园问答系统",
        ] {
            assert!(
                !app_calls_llm_at_runtime(req),
                "must not be flagged as a runtime-LLM app: {req}"
            );
            assert_eq!(stated_runtime_model(req), None, "{req}");
        }
        // A model the app really calls at runtime is still detected.
        assert!(stated_runtime_model("做一个聊天机器人,运行时用 kimi")
            .unwrap()
            .contains("Moonshot"));
        assert!(stated_runtime_model("做一个客服,用 kimi 来回答用户问题")
            .unwrap()
            .contains("Moonshot"));
        assert!(app_calls_llm_at_runtime("做一个智能问答系统"));
    }

    #[test]
    fn a_claude_backed_app_is_not_told_to_avoid_claude() {
        // S02-9: when the user names Claude as the app's RUNTIME model, the
        // directive must not also forbid hardcoding Anthropic / Claude.
        let d = runtime_model_directive("做一个客服聊天机器人,运行时用 Claude");
        assert!(d.contains("NAMED a runtime model") && d.contains("Anthropic Claude"));
        assert!(
            !d.contains("NEVER silently hardcode"),
            "no contradictory anti-Anthropic guard for a Claude-backed app: {d}"
        );
        assert!(runtime_guard("做一个客服聊天机器人,运行时用 Claude").is_empty());
        assert!(runtime_guard("做一个智能客服聊天机器人").contains("ANTHROPIC_API_KEY"));
    }
}
