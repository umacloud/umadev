//! Error-pitfall recogniser — the "brain" that turns a raw development error
//! (a failed tool call, a non-zero build/test exit, a runtime stack trace) into
//! structured, reusable guidance.
//!
//! This is what lets UmaDev "get it right the first time next time": every
//! error hit during a run is classified into a known *family* with a stable
//! [`ErrorInsight::signature`], a root-cause explanation, and an actionable
//! fix. The lessons layer ([`crate::lessons`]) persists those insights and
//! recalls them into later worker prompts, so a pitfall seen once is pre-empted
//! forever after.
//!
//! Design notes:
//! - **Pure + dependency-free.** `classify_error` is a total function over a
//!   string; it never touches the filesystem or network. No `regex` dep — plain
//!   `str` scanning keeps this crate light (matching `umadev-spec` etc.).
//! - **Signature is the dedup key.** Volatile parts (file paths, line/column
//!   numbers, hex addresses) are stripped so the *same class* of error collapses
//!   to one lesson even when the offending file differs run to run.
//! - **Ordered cascade, specific first.** Detectors run most-specific →
//!   generic; the first match wins. A generic fallback still yields a usable
//!   signature so nothing is dropped.

/// Structured guidance distilled from one raw error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorInsight {
    /// Coarse family bucket — also the `domain` the lesson sediments under
    /// (`dependency`, `type`, `runtime`, `network`, `api`, `config`, `build`,
    /// `test`, `lint`, `general`).
    pub category: String,
    /// Stable, volatility-stripped dedup key, e.g.
    /// `dependency/module-not-found/react-router-dom`. The same class of error
    /// always produces the same signature, which is how recurrence is detected.
    pub signature: String,
    /// Short human-readable title (becomes the lesson H1).
    pub title: String,
    /// Why this happens.
    pub root_cause: String,
    /// What to do about it — concrete and actionable.
    pub fix: String,
    /// Search keywords for BM25 / recall discoverability.
    pub keywords: Vec<String>,
    /// `true` when a specific family matched; `false` for the generic fallback.
    /// Callers may choose to capture only recognised errors, or all of them.
    pub recognized: bool,
}

/// Cheap pre-filter: does this text look like an error at all?
///
/// Used by capture sites to skip benign tool output before paying for
/// classification. Case-insensitive substring scan over common error markers
/// across JS / TS / Rust / Python / shells.
#[must_use]
pub fn looks_like_error(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "error",
        "failed",
        "fail",
        "panic",
        "exception",
        "traceback",
        "cannot find",
        "not found",
        "not defined",
        "undefined",
        "unexpected",
        "is not a function",
        "refused",
        "denied",
        "rejected",
        "err!",
        "err_",
        "eaddrinuse",
        "eacces",
        "econnrefused",
        "no such",
        "assertion",
        "missing",
        "✕",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

/// Classify a raw error string into a reusable [`ErrorInsight`].
///
/// Always returns a value (the generic fallback covers unrecognised text);
/// inspect [`ErrorInsight::recognized`] to tell a precise match from the
/// fallback.
#[must_use]
pub fn classify_error(text: &str) -> ErrorInsight {
    let lower = text.to_ascii_lowercase();
    let line = first_significant_line(text);

    // Ordered, most-specific first. The first detector to fire wins.
    for detector in [
        detect_missing_module,
        detect_package_manager,
        detect_permission,
        detect_type_mismatch,
        detect_undefined_access,
        detect_panic,
        detect_port_in_use,
        detect_cors,
        detect_connection,
        detect_http_status,
        detect_env_missing,
        detect_syntax,
        detect_test_failure,
        detect_build_tool,
    ] {
        if let Some(insight) = detector(&lower, &line) {
            return insight;
        }
    }

    generic_fallback(&lower, &line)
}

// ---------------------------------------------------------------------------
// Detectors — each returns Some(insight) when its family matches.
// ---------------------------------------------------------------------------

fn detect_missing_module(lower: &str, line: &str) -> Option<ErrorInsight> {
    const MARKERS: &[&str] = &[
        "cannot find module",
        "module not found",
        "can't resolve",
        "failed to resolve import",
        "could not resolve",
        "no module named",
        "modulenotfounderror",
        "unresolved import",
        "has no exported member",
        "is not exported",
        "error[e0432]",
        "error[e0433]",
    ];
    if !MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    let key = extract_quoted(line)
        .map(|s| slugify(&s))
        .unwrap_or_default();
    Some(build(
        "dependency",
        "module-not-found",
        &key,
        &short_title("缺少模块 / 导入无法解析", &key),
        "目标模块未安装,或导入路径(大小写/相对层级/别名)不正确。",
        "确认依赖已在 package.json / Cargo.toml / requirements 中并完成安装(npm/pnpm install、cargo add、pip install);\
         核对导入路径的大小写与相对层级;TS 项目检查 tsconfig 的 paths 别名是否配置。",
        line,
        &key,
    ))
}

fn detect_package_manager(lower: &str, line: &str) -> Option<ErrorInsight> {
    const MARKERS: &[&str] = &[
        "npm err!",
        "eresolve",
        "could not resolve dependency",
        "peer dep",
        "peerdependencies",
        "err_pnpm",
        "yarn error",
        "unmet peer",
    ];
    if !MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    Some(build(
        "dependency",
        "package-manager",
        "",
        "依赖解析冲突 (package manager)",
        "依赖版本范围互相冲突,或 peer dependency 未满足。",
        "对齐冲突包的版本范围,或在 package.json 加 overrides/resolutions 锁定;\
         ERESOLVE 可评估 `--legacy-peer-deps` 临时通过;优先修根因而非掩盖。",
        line,
        "",
    ))
}

fn detect_permission(lower: &str, line: &str) -> Option<ErrorInsight> {
    const MARKERS: &[&str] = &[
        "permission denied",
        "operation not permitted",
        "access is denied",
        "eacces",
    ];
    if !MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    Some(build(
        "runtime",
        "permission",
        "",
        "权限不足 (permission denied)",
        "进程对目标文件/目录/端口没有读写或执行权限。",
        "检查文件/目录的属主与权限;容器内以非 root 运行时确保挂载目录可写;\
         不要用 sudo 掩盖问题——修复 npm 全局目录权限或改用 nvm。",
        line,
        "",
    ))
}

fn detect_type_mismatch(lower: &str, line: &str) -> Option<ErrorInsight> {
    let ts_code = ["ts2322", "ts2339", "ts2345", "ts2531", "ts2769", "ts7006"]
        .iter()
        .any(|c| lower.contains(c));
    let hit = lower.contains("is not assignable to")
        || lower.contains("mismatched types")
        || lower.contains("error[e0308]")
        || lower.contains("does not exist on type")
        || lower.contains("argument of type")
        || (lower.contains("expected") && lower.contains("found"))
        || ts_code;
    if !hit {
        return None;
    }
    Some(build(
        "type",
        "type-mismatch",
        "",
        "类型不匹配 (type error)",
        "两端类型不一致:函数签名 / 接口定义与实际传参或返回值对不上。",
        "对齐类型:接口/泛型定义与调用处一致;TS 用准确的类型或类型守卫而非 any;\
         Rust 检查借用(&/owned)与 Option/Result 包装。先看报错指向的具体行。",
        line,
        "",
    ))
}

fn detect_undefined_access(lower: &str, line: &str) -> Option<ErrorInsight> {
    const MARKERS: &[&str] = &[
        "is not a function",
        "cannot read property",
        "cannot read properties of undefined",
        "cannot read properties of null",
        "is not defined",
        "'nonetype' object",
        "attributeerror",
        "nullpointerexception",
    ];
    if !MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    Some(build(
        "runtime",
        "undefined-access",
        "",
        "访问了 undefined / null / 未定义符号",
        "在值就绪前访问其属性/方法,或符号未导入、拼写错误、作用域不对。",
        "访问前判空(可选链 ?. 与默认值);确认异步数据加载完成再渲染;\
         核对 import 与变量名拼写、作用域。看报错栈顶定位首个出错点。",
        line,
        "",
    ))
}

fn detect_panic(lower: &str, line: &str) -> Option<ErrorInsight> {
    const MARKERS: &[&str] = &[
        "panicked at",
        "called `option::unwrap()` on a `none`",
        "called `result::unwrap()`",
        "index out of bounds",
        "unwrap() on",
    ];
    if !MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    Some(build(
        "runtime",
        "panic",
        "",
        "Rust panic (unwrap / 越界)",
        "对 None/Err 直接 unwrap/expect,或数组/切片越界访问。",
        "去掉裸 unwrap/expect,改用 `?` 传播或 match 处理 None/Err;\
         索引前校验长度或用 .get()。按 panic 指向的文件:行修复。",
        line,
        "",
    ))
}

fn detect_port_in_use(lower: &str, line: &str) -> Option<ErrorInsight> {
    let hit = lower.contains("eaddrinuse")
        || lower.contains("address already in use")
        || lower.contains("port is already")
        || lower.contains("already in use");
    if !hit {
        return None;
    }
    Some(build(
        "runtime",
        "port-in-use",
        "",
        "端口被占用 (EADDRINUSE)",
        "目标端口已被另一个进程监听。",
        "换端口或释放占用(`lsof -i:PORT` 后 kill;或在 vite/server 配置改 port)。\
         UmaDev 预览会自动探测占用端口并提示。",
        line,
        "",
    ))
}

fn detect_cors(lower: &str, line: &str) -> Option<ErrorInsight> {
    let hit = lower.contains("access-control-allow-origin")
        || lower.contains("blocked by cors")
        || lower.contains("cors policy")
        || lower.contains("preflight");
    if !hit {
        return None;
    }
    Some(build(
        "network",
        "cors",
        "",
        "跨域被拦截 (CORS)",
        "后端未对前端来源放行跨域请求,或预检(OPTIONS)未正确响应。",
        "后端开启 CORS(Access-Control-Allow-Origin/Methods/Headers,正确处理 OPTIONS);\
         开发期可用前端代理(vite server.proxy)绕过跨域。",
        line,
        "",
    ))
}

fn detect_connection(lower: &str, line: &str) -> Option<ErrorInsight> {
    let hit = lower.contains("failed to fetch")
        || lower.contains("err_connection_refused")
        || lower.contains("econnrefused")
        || lower.contains("fetch failed")
        || lower.contains("network error")
        || lower.contains("net::err");
    if !hit {
        return None;
    }
    Some(build(
        "network",
        "connection-refused",
        "",
        "请求失败 / 连接被拒",
        "后端服务未启动、端口不一致,或 baseURL / 代理配置错误。",
        "确认后端已启动且端口与前端请求一致;核对 baseURL 与代理;\
         先用 `curl` 验证接口可达,再排查前端。",
        line,
        "",
    ))
}

fn detect_http_status(lower: &str, line: &str) -> Option<ErrorInsight> {
    // Conservative: only fire on the parenthesised status phrases so a bare
    // "404" inside unrelated text doesn't false-positive.
    let hit = lower.contains("(not found)")
        || lower.contains("(internal server error)")
        || lower.contains("(unauthorized)")
        || lower.contains("(forbidden)")
        || lower.contains("status code 404")
        || lower.contains("status code 500");
    if !hit {
        return None;
    }
    Some(build(
        "api",
        "http-error",
        "",
        "接口返回错误状态码",
        "前后端路由对不齐(404)、鉴权缺失(401/403),或后端内部异常(500)。",
        "对齐前后端 method+path 与 OpenAPI 契约;401/403 检查鉴权头与登录态;\
         500 看后端日志栈定位根因。",
        line,
        "",
    ))
}

fn detect_env_missing(lower: &str, line: &str) -> Option<ErrorInsight> {
    let mentions_env = lower.contains("environment variable")
        || lower.contains("env var")
        || lower.contains("process.env");
    let hit = mentions_env
        && (lower.contains("not set")
            || lower.contains("missing")
            || lower.contains("undefined")
            || lower.contains("must be set")
            || lower.contains("required"));
    if !hit {
        return None;
    }
    Some(build(
        "config",
        "env-missing",
        "",
        "缺少环境变量",
        "运行所需的环境变量未配置。",
        "在 .env / 部署环境补齐缺失变量,并提供 .env.example 模板;\
         代码对必需 env 做启动期校验,缺失即抛清晰错误而非裸 undefined。",
        line,
        "",
    ))
}

fn detect_syntax(lower: &str, line: &str) -> Option<ErrorInsight> {
    const MARKERS: &[&str] = &[
        "syntaxerror",
        "unexpected token",
        "unexpected end of",
        "unterminated",
        "parsing error",
        "unexpected identifier",
    ];
    if !MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    Some(build(
        "build",
        "syntax",
        "",
        "语法错误 (syntax error)",
        "源码语法不合法:常见漏括号/逗号/引号、JSX 标签未闭合、import 写法错误。",
        "按报错的文件:行修正语法;让编辑器/格式化器先标红;\
         JSX 确认每个标签闭合、表达式包在 {} 内。",
        line,
        "",
    ))
}

fn detect_test_failure(lower: &str, line: &str) -> Option<ErrorInsight> {
    const MARKERS: &[&str] = &[
        "assertionerror",
        "test result: failed",
        "assertion failed",
        "assertion `left",
        "tests failed",
        "failures:",
        "✕",
    ];
    if !MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    Some(build(
        "test",
        "assertion",
        "",
        "测试断言失败",
        "实现与断言期望不一致,或断言已过期未更新。",
        "对比 left/right 差异定位;修正实现或更新过期断言;\
         先单独跑失败用例缩小范围,再回归全量。",
        line,
        "",
    ))
}

fn detect_build_tool(lower: &str, line: &str) -> Option<ErrorInsight> {
    const MARKERS: &[&str] = &[
        "[vite]",
        "failed to compile",
        "build failed",
        "module build failed",
        "webpack compiled with",
        "esbuild",
    ];
    if !MARKERS.iter().any(|m| lower.contains(m)) {
        return None;
    }
    Some(build(
        "build",
        "build-failed",
        "",
        "构建失败 (build failed)",
        "构建中断,通常由上游的导入/类型/语法问题引起。",
        "先看首条根因报错(往往在更上方);清缓存重装排除脏状态\
         (`rm -rf node_modules .vite && install`);逐条修复后重跑。",
        line,
        "",
    ))
}

fn generic_fallback(_lower: &str, line: &str) -> ErrorInsight {
    let key = slugify(&normalize_for_signature(line));
    let key = take_chars(&key, 48);
    ErrorInsight {
        category: "general".to_string(),
        signature: format!("general/error/{key}"),
        title: short_title("开发错误", &take_chars(line, 80)),
        root_cause: "未归入已知家族的错误——需按报错文本具体定位。".to_string(),
        fix: "读完整错误栈,从第一条根因错误开始修;构造最小可复现用例;\
              查官方文档确认 API 的正确用法与版本签名。"
            .to_string(),
        keywords: keywords_from(line, "general", "error", &key),
        recognized: false,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an [`ErrorInsight`] with a normalised signature + keyword set.
#[allow(clippy::too_many_arguments)]
fn build(
    category: &str,
    family: &str,
    key: &str,
    title: &str,
    root_cause: &str,
    fix: &str,
    line: &str,
    sig_key: &str,
) -> ErrorInsight {
    let signature = if sig_key.is_empty() {
        format!("{category}/{family}")
    } else {
        format!("{category}/{family}/{sig_key}")
    };
    ErrorInsight {
        category: category.to_string(),
        signature,
        title: title.to_string(),
        root_cause: root_cause.to_string(),
        fix: fix.to_string(),
        keywords: keywords_from(line, category, family, key),
        recognized: true,
    }
}

/// Pick the most error-relevant line: the first line containing an error
/// marker, else the first non-empty line. Trimmed.
fn first_significant_line(text: &str) -> String {
    const MARKERS: &[&str] = &[
        "error",
        "failed",
        "panic",
        "exception",
        "cannot",
        "not found",
        "undefined",
        "unexpected",
        "err!",
        "assertion",
        "refused",
        "denied",
    ];
    let mut first_nonempty: Option<&str> = None;
    for raw in text.lines() {
        let l = raw.trim();
        if l.is_empty() {
            continue;
        }
        if first_nonempty.is_none() {
            first_nonempty = Some(l);
        }
        let low = l.to_ascii_lowercase();
        if MARKERS.iter().any(|m| low.contains(m)) {
            return take_chars(l, 240);
        }
    }
    take_chars(first_nonempty.unwrap_or("").trim(), 240)
}

/// Extract the first single/double/back-quoted token from a line, if any.
///
/// A `'` is only treated as an opening quote when the preceding char is
/// non-alphanumeric, so a contraction like `Can't` is NOT mistaken for a quote
/// (the real `'lodash'` further along wins).
fn extract_quoted(line: &str) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c != '\'' && c != '"' && c != '`' {
            continue;
        }
        if i > 0 && chars[i - 1].is_alphanumeric() {
            continue; // apostrophe inside a word
        }
        for (j, &cj) in chars.iter().enumerate().skip(i + 1) {
            if cj == c {
                let inner: String = chars[i + 1..j].iter().collect();
                let inner = inner.trim().to_string();
                if !inner.is_empty() {
                    return Some(inner);
                }
                break;
            }
        }
    }
    None
}

/// Lowercase, replace non-alphanumeric runs with single `-`, trim, cap length.
fn slugify(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    take_chars(out.trim_matches('-'), 60)
}

/// Strip volatile tokens (file paths, line:col, numbers, hex) so the same
/// class of error normalises to one signature regardless of where it fired.
fn normalize_for_signature(line: &str) -> String {
    let mut out = String::new();
    for token in line.split_whitespace() {
        let looks_path = token.contains('/')
            || token.contains('\\')
            || token.contains(".rs")
            || token.contains(".ts")
            || token.contains(".tsx")
            || token.contains(".js")
            || token.contains(".jsx")
            || token.contains(".py");
        let looks_numeric = token
            .chars()
            .all(|c| c.is_ascii_digit() || c == ':' || c == '.')
            && token.chars().any(|c| c.is_ascii_digit());
        let looks_hex = token.starts_with("0x");
        if looks_path || looks_numeric || looks_hex {
            continue;
        }
        out.push_str(token);
        out.push(' ');
    }
    out.trim().to_string()
}

/// Build a keyword set for recall: category, family, an optional key, plus
/// salient alphanumeric tokens (len ≥ 3) from the line. Deduped, capped.
fn keywords_from(line: &str, category: &str, family: &str, key: &str) -> Vec<String> {
    let mut kws: Vec<String> = Vec::new();
    push_keyword(&mut kws, category);
    push_keyword(&mut kws, family);
    if !key.is_empty() {
        push_keyword(&mut kws, key);
    }
    for word in line.split(|c: char| !c.is_ascii_alphanumeric()) {
        if kws.len() >= 14 {
            break;
        }
        push_keyword(&mut kws, word);
    }
    kws
}

/// Append a lowercased keyword (len ≥ 3, deduped) to the set.
fn push_keyword(kws: &mut Vec<String>, word: &str) {
    let w = word.trim().to_ascii_lowercase();
    if w.len() >= 3 && !kws.contains(&w) {
        kws.push(w);
    }
}

/// `"<prefix>: <detail>"`, with `detail` clipped.
fn short_title(prefix: &str, detail: &str) -> String {
    let d = detail.trim();
    if d.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {}", take_chars(d, 80))
    }
}

/// Take at most `max` chars (char-boundary safe), appending `…` on truncation.
fn take_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn looks_like_error_discriminates() {
        assert!(looks_like_error("error[E0308]: mismatched types"));
        assert!(looks_like_error("npm ERR! code ERESOLVE"));
        assert!(!looks_like_error("Compiled successfully in 1.2s"));
        assert!(!looks_like_error("Listening on http://localhost:3000"));
    }

    #[test]
    fn missing_module_keeps_module_in_signature() {
        let i = classify_error("Error: Cannot find module 'react-router-dom'");
        assert_eq!(i.category, "dependency");
        assert!(i.recognized);
        assert_eq!(i.signature, "dependency/module-not-found/react-router-dom");
        assert!(i.fix.contains("install"));
    }

    #[test]
    fn rust_unresolved_import_is_dependency() {
        let i = classify_error("error[E0432]: unresolved import `serde::Deserializ`");
        assert_eq!(i.category, "dependency");
        assert_eq!(i.signature, "dependency/module-not-found/serde-deserializ");
    }

    #[test]
    fn type_mismatch_detected_for_ts_and_rust() {
        let ts = classify_error("Type 'string' is not assignable to type 'number'.");
        assert_eq!(ts.category, "type");
        assert_eq!(ts.signature, "type/type-mismatch");
        let rust = classify_error("error[E0308]: mismatched types: expected u32, found String");
        assert_eq!(rust.category, "type");
    }

    #[test]
    fn undefined_access_runtime() {
        let i = classify_error("TypeError: Cannot read properties of undefined (reading 'map')");
        assert_eq!(i.category, "runtime");
        assert_eq!(i.signature, "runtime/undefined-access");
    }

    #[test]
    fn port_in_use_drops_the_port_number() {
        let a = classify_error("Error: listen EADDRINUSE: address already in use :::3000");
        let b = classify_error("Error: listen EADDRINUSE: address already in use :::5173");
        // Same class → same signature regardless of the specific port.
        assert_eq!(a.signature, "runtime/port-in-use");
        assert_eq!(a.signature, b.signature);
    }

    #[test]
    fn cors_and_connection_are_network() {
        assert_eq!(
            classify_error("Access to fetch blocked by CORS policy").category,
            "network"
        );
        assert_eq!(
            classify_error("TypeError: Failed to fetch").signature,
            "network/connection-refused"
        );
    }

    #[test]
    fn package_manager_conflict() {
        let i = classify_error("npm ERR! ERESOLVE unable to resolve dependency tree");
        assert_eq!(i.signature, "dependency/package-manager");
    }

    #[test]
    fn panic_detected() {
        let i = classify_error(
            "thread 'main' panicked at src/main.rs:42: called `Option::unwrap()` on a `None` value",
        );
        assert_eq!(i.signature, "runtime/panic");
    }

    #[test]
    fn generic_fallback_strips_paths_and_numbers() {
        let a = classify_error("Boom at /app/src/foo.ts:10:5 weird breakage");
        let b = classify_error("Boom at /app/src/bar.ts:99:1 weird breakage");
        assert!(!a.recognized);
        assert_eq!(a.category, "general");
        // Volatile file path + line:col stripped → same signature.
        assert_eq!(a.signature, b.signature);
        assert!(a.signature.starts_with("general/error/"));
    }

    #[test]
    fn signature_is_stable_for_same_class() {
        // Same family, different offending module → signatures differ by key,
        // but each is itself stable.
        let a1 = classify_error("Cannot find module 'lodash'");
        let a2 = classify_error("Module not found: Error: Can't resolve 'lodash'");
        assert_eq!(a1.signature, "dependency/module-not-found/lodash");
        assert_eq!(a2.signature, "dependency/module-not-found/lodash");
    }
}
