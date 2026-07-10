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

/// The semantic decision a structured-gate option maps onto. Each maps to the
/// EXISTING gate flow — there is **no new decision machinery**: `Approve` drives
/// the confirm/continue path, `Revise`/`AddMore` drop into the existing
/// free-text revise path (the picker is a nicer front-end to it), and `Cancel`
/// aborts the run. UD-FLOW-002 / UD-FLOW-003.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    /// Approve the gate — the existing confirm/continue path.
    Approve,
    /// Request revisions — drops into the existing free-text revise path.
    Revise,
    /// Supplement / add more — a revise-class follow-up with an "add more" framing.
    AddMore,
    /// Cancel the run.
    Cancel,
}

impl GateDecision {
    /// Stable id (persistence / tests).
    #[must_use]
    pub const fn id_str(self) -> &'static str {
        match self {
            Self::Approve => "approve",
            Self::Revise => "revise",
            Self::AddMore => "add_more",
            Self::Cancel => "cancel",
        }
    }

    /// The i18n key the UI localizes into this option's picker label. Carried as
    /// a key (not a localized string) so the runner can attach a structured
    /// choice without knowing the user's locale — the TUI resolves it at render
    /// time (a non-key string passed through `t()` is returned verbatim, so a
    /// caller may also supply a literal label).
    #[must_use]
    pub const fn label_key(self) -> &'static str {
        match self {
            Self::Approve => "gate.choice.confirm",
            Self::Revise => "gate.choice.revise",
            Self::AddMore => "gate.choice.add_more",
            Self::Cancel => "gate.choice.cancel",
        }
    }
}

/// One labeled option in a structured gate choice (2–4 per choice).
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct GateChoiceOption {
    /// The display label — an i18n KEY (localized by the UI via `t()`) or a
    /// literal string (passed through verbatim). See [`GateDecision::label_key`].
    pub label: String,
    /// Which existing gate decision picking this option drives.
    pub decision: GateDecision,
}

/// A structured choice surfaced when a gate opens: a question + 2–4 labeled
/// options the UI renders as a picker (↑↓ / number keys + Enter). Free-text
/// stays an always-available fallback — the picker never replaces it.
///
/// **Fail-open:** an empty `options` list (or a `None` choice on the gate event)
/// means "no structured choice" → the UI falls back to the existing free-form
/// gate exactly as before.
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub struct GateChoice {
    /// The question shown above the options — an i18n key or a literal (same
    /// resolution rule as [`GateChoiceOption::label`]).
    pub question: String,
    /// The 2–4 options.
    pub options: Vec<GateChoiceOption>,
}

impl GateChoice {
    /// The STANDARD structured choice for a gate, carried as i18n keys so it is
    /// locale-free (the UI localizes at render). Returns `None` for a gate that
    /// has no standard approve/revise choice (the clarify gate collects free-form
    /// answers, not a decision) so the caller falls back to the free-form gate.
    #[must_use]
    pub fn standard(gate: Gate) -> Option<Self> {
        let (question, decisions): (&str, &[GateDecision]) = match gate {
            Gate::DocsConfirm => (
                "gate.choice.docs.question",
                &[
                    GateDecision::Approve,
                    GateDecision::Revise,
                    GateDecision::AddMore,
                ],
            ),
            Gate::PreviewConfirm => (
                "gate.choice.preview.question",
                &[
                    GateDecision::Approve,
                    GateDecision::Revise,
                    GateDecision::AddMore,
                ],
            ),
            // The clarify gate is an answer-collection surface, not an
            // approve/revise decision → no standard picker (free-form, unchanged).
            Gate::ClarifyGate => return None,
        };
        Some(Self {
            question: question.to_string(),
            options: decisions
                .iter()
                .map(|d| GateChoiceOption {
                    label: d.label_key().to_string(),
                    decision: *d,
                })
                .collect(),
        })
    }

    /// Whether this choice has at least one option — the fail-open guard the UI
    /// checks before rendering a picker (an empty choice → free-form gate).
    #[must_use]
    pub fn is_renderable(&self) -> bool {
        !self.options.is_empty()
    }
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
    // English change verbs. Matched as substrings (`t.contains(k)`), so a root
    // covers its inflections: `build` → building/built (kept explicit for clarity),
    // `wrote` → rewrote, `set up` → "set up the route". The build-loop directive
    // literally says "build it", so a base answering "I built …/wrote …/scaffolded
    // …/wired up …" MUST register as a code claim — otherwise the honesty QC + the
    // source-present hard-gate are skipped over a possibly-hallucinated "done".
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
        // The most common "I did the work" verbs — aligned with the /run
        // directive's own "build it" wording (P1-3).
        "build", // building / rebuilt / "I'll build" → also "built" (substring)
        "built",
        "wrote",
        "wired",
        "scaffolded",
        "generated",
        "coded",
        "developed",
        "set up",
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
        // L4: the most common "I did the work" Chinese phrasings were missing, so
        // "已完成登录功能" / "开发了支付模块" was NOT read as a code claim — the
        // single-turn loop then skipped the honesty/QC read over a possibly-
        // hallucinated "done". ("已完成" is a substring of "完成" so "完成" covers it.)
        "完成",
        "已完成",
        "开发",
    ];
    let t = text.to_lowercase();
    if EN.iter().any(|k| t.contains(k)) {
        return true;
    }
    // ZH: a change verb is a code CLAIM only when NOT negated. "未写入真实代码" / "没有修改" /
    // "不新增" is the OPPOSITE of a claim - a bare substring match wrongly read the negated verb
    // as a claim (the plan-mode read-only reply "未写入真实代码" then false-tripped the
    // source-present hard-gate -> a spurious "0 source files" abort in zh-CN). A verb counts only
    // if the character immediately before it is not a negation (未 / 没 / 不 / 无).
    const ZH_NEG: &[char] = &['未', '没', '不', '无'];
    ZH.iter().any(|k| {
        text.match_indices(k).any(|(i, _)| {
            text[..i]
                .chars()
                .next_back()
                .is_none_or(|prev| !ZH_NEG.contains(&prev))
        })
    })
}

/// Heuristic: does this base reply show it ALREADY ran the project's build/test
/// THIS turn and it PASSED? Used by the director's auto-QC to skip UmaDev's own
/// *duplicate* full build/test read (an `npm install` + build can be minutes) when
/// the base's body — which holds the tools — already ran it green inside its turn.
///
/// **Conservative by contract (no correctness regression):** this returns `true`
/// ONLY when the reply both (a) names a build/test/lint run AND (b) reports it
/// passed, AND (c) shows NO failure signal. Anything ambiguous — no mention, a
/// vague "done", or any whiff of a failure/error — returns `false`, so UmaDev
/// falls back to running its OWN objective read (the prior behaviour). A false
/// negative just re-runs a check we could have trusted (slower, still correct); we
/// never skip on a false positive that hides a real failure. Bilingual; matched as
/// lowercased substrings.
#[must_use]
pub fn base_ran_build_test_clean(text: &str) -> bool {
    let t = text.to_lowercase();

    // (c) Any failure signal vetoes the skip — if the base mentions a failing
    // build/test anywhere in its reply, UmaDev must run its own read to see it.
    const FAILURE: &[&str] = &[
        "fail",
        "failing",
        "failed",
        "error",
        "errored",
        "broke",
        "broken",
        "did not pass",
        "didn't pass",
        "does not pass",
        "doesn't pass",
        "not passing",
        "exit code 1",
        "exit 1",
        "panic",
        "测试失败",
        "构建失败",
        "编译失败",
        "报错",
        "未通过",
        "没通过",
        "不通过",
    ];
    // L3: "red" must be matched as a WHOLE word — a plain `contains("red")` hits
    // benign substrings ("requi**red**", "rende**red**", "cove**red**"), wrongly
    // vetoing a clean self-run. Word-boundary it; the other failure tokens are
    // distinctive enough as substrings.
    if FAILURE.iter().any(|k| t.contains(k)) || contains_word(&t, "red") {
        return false;
    }

    // (a) names a build/test/lint run AND (b) reports it passed/green. Require a
    // PASS phrase that co-locates the run with a success word so a bare "looks good"
    // (no actual run) does NOT qualify.
    const PASS_EN: &[&str] = &[
        "tests pass",
        "tests passing",
        "all tests pass",
        "all tests passing",
        "test suite passes",
        "tests are passing",
        "tests green",
        "build passes",
        "build passed",
        "build succeeded",
        "build succeeds",
        "builds successfully",
        "built successfully",
        "compiles cleanly",
        "compiled successfully",
        "lint passes",
        "lint passed",
        "lint clean",
        "checks pass",
        "all checks pass",
        "ci passes",
        "test and build pass",
        "build and test pass",
    ];
    const PASS_ZH: &[&str] = &[
        "测试通过",
        "测试全部通过",
        "测试全绿",
        "构建通过",
        "构建成功",
        "编译通过",
        "编译成功",
        "检查通过",
        "全部通过",
        "校验通过",
    ];
    let claims_pass =
        PASS_EN.iter().any(|k| t.contains(k)) || PASS_ZH.iter().any(|k| text.contains(k));
    // M3: a PASS phrase ALONE ("tests pass" / "构建成功") is just prose — trusting it
    // to SKIP UmaDev's own objective build/test read lets a hallucinated "it passes"
    // bypass the floor. Require, in addition, MACHINE EVIDENCE that a real run
    // happened (an exit-code-0 signal or a named build/test command/output). Absent
    // that, fall back to running our own read (slower, still correct) — we only ever
    // skip on a corroborated green, never on prose.
    claims_pass && reply_shows_machine_run_evidence(&t)
}

/// `true` when `t` (already lowercased) shows MACHINE evidence that a build/test
/// actually RAN — an exit-code-0 signal, or a named build/test runner command /
/// recognised runner output. This is the corroboration [`base_ran_build_test_clean`]
/// requires before trusting a prose "it passes" enough to SKIP UmaDev's own
/// objective read. Conservative: a reply that merely claims success in words (no
/// command, no exit code, no runner output) returns `false`, so UmaDev runs its own
/// check. Deterministic, fail-open (a false negative just re-runs a check).
#[must_use]
fn reply_shows_machine_run_evidence(t: &str) -> bool {
    // (a) An exit-code-0 / clean-return signal — the unambiguous "a process ran and
    //     returned success" tell.
    const EXIT_OK: &[&str] = &[
        "exit code 0",
        "exit 0",
        "exit status 0",
        "exited 0",
        "exited with 0",
        "exited with code 0",
        "status code 0",
        "returned 0",
        "return code 0",
        "$? = 0",
        "$?=0",
        "退出码 0",
        "退出码为 0",
        "退出码:0",
        "返回码 0",
        "返回 0",
    ];
    if EXIT_OK.iter().any(|k| t.contains(k)) {
        return true;
    }
    // (b) A named build/test/lint RUNNER command or its recognised result line — proof
    //     a real tool was invoked, not just a prose claim. Substring-matched (the
    //     command names are distinctive enough).
    const RUNNER: &[&str] = &[
        "cargo test",
        "cargo build",
        "cargo check",
        "cargo clippy",
        "npm test",
        "npm run test",
        "npm run build",
        "npm ci",
        "yarn test",
        "yarn build",
        "pnpm test",
        "pnpm build",
        "pnpm run",
        "npx tsc",
        "npx jest",
        "npx vitest",
        "tsc --",
        "jest ",
        "vitest ",
        "pytest",
        "python -m pytest",
        "unittest",
        "go test",
        "go build",
        "mvn ",
        "gradle ",
        "./gradlew",
        "make test",
        "make build",
        "phpunit",
        "rspec",
        "dotnet test",
        "dotnet build",
        // Distinctive runner RESULT lines (a machine emitted these, not the author) —
        // kept strict so prose alone never qualifies.
        "test result: ok",
        "0 failed",
        "0 failures",
        "0 errors",
        "passing (",
    ];
    RUNNER.iter().any(|k| t.contains(k))
}

/// `true` when `word` occurs in `haystack` as a WHOLE token — the char on each side
/// (if any) is NOT ASCII-alphanumeric. Used so a short failure token like `red` is
/// not matched inside `requi**red**` / `rende**red**` / `cove**red**`. `haystack` is
/// expected lowercased by the caller; `word` is ASCII.
#[must_use]
fn contains_word(haystack: &str, word: &str) -> bool {
    if word.is_empty() {
        return false;
    }
    let bytes = haystack.as_bytes();
    let mut from = 0;
    while let Some(idx) = haystack[from..].find(word) {
        let abs = from + idx;
        let before_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let after = abs + word.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = abs + 1;
    }
    false
}

/// Does this SHELL COMMAND actually invoke a build / test / lint RUNNER? This is the
/// OBSERVED-tool corroboration the director requires before a base's prose "it's green"
/// is trusted enough to SKIP UmaDev's own objective build/test read: a green CLAIM in
/// the reply text ([`base_ran_build_test_clean`]) is honest ONLY when a real runner was
/// seen running on the tool-call stream THIS turn (`SessionEvent::ToolCall`'s command).
/// Narration alone — a "cargo test passed, exit 0" written into the reply with no runner
/// ever invoked — no longer skips the floor, because it produces no matching tool call.
///
/// **Bounded + comment/quote-tolerant.** The command is split on shell separators
/// (`;` `|` `&`, newlines — so `&&`/`||` fall out and a runner in a LATER segment of a
/// chain like `cd app && npm test` still counts); each segment has its leading noise
/// peeled (wrapping quotes/parens, `NAME=value` env-assignments, `sudo`/`time`/`env`/…
/// wrappers) and a `#` comment or empty segment is not a run; the FIRST real command
/// word is then matched (anchored, path-basename-normalized) against a bounded runner
/// set — so a runner NAME buried inside a quoted argument to a non-runner (`echo "npm
/// test"`, `git commit -m "run cargo test"`) does NOT falsely corroborate.
/// Deterministic, fail-open: an empty / unparseable command → `false` (UmaDev runs its
/// own read — never a false skip).
#[must_use]
pub fn command_is_build_test_runner(command: &str) -> bool {
    let lowered = command.to_lowercase();
    lowered
        .split([';', '\n', '|', '&'])
        .any(segment_invokes_runner)
}

/// One shell segment → does its FIRST real command word invoke a build/test/lint runner?
/// Leading noise (wrapping quotes/parens, env-assignments, `sudo`/`time`/`env`/…) is
/// peeled so the real command is examined; a `#` comment or empty segment is not a run.
fn segment_invokes_runner(seg: &str) -> bool {
    let mut words = seg
        .split_whitespace()
        .skip_while(|w| is_leading_command_noise(w));
    let Some(w0) = words.next() else {
        return false;
    };
    // Strip wrapping quotes/parens/backticks off the command word.
    let w0 =
        w0.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '(' | ')' | '{' | '}' | '\\'));
    if w0.is_empty() || w0.starts_with('#') {
        return false;
    }
    // A leading path (`./gradlew`, `/usr/bin/cargo`, `node_modules/.bin/eslint`) → basename.
    let cmd = w0.rsplit('/').next().unwrap_or(w0);
    let sub = words.next().unwrap_or("");
    match cmd {
        // JS/TS package managers: a build/test/lint verb, `run|exec|dlx <such a script>`,
        // or the manager's own test / clean-install shorthands.
        "npm" | "pnpm" | "yarn" | "bun" => {
            is_js_build_test_verb(sub)
                || (matches!(sub, "run" | "exec" | "dlx") && {
                    let next = words.next().unwrap_or("");
                    is_js_build_test_verb(next) || is_js_test_tool(next)
                })
                || (cmd == "npm" && matches!(sub, "ci" | "t"))
                || (cmd == "bun" && sub == "test")
        }
        // JS/TS tool runners invoked directly via npx.
        "npx" => is_js_test_tool(sub),
        "cargo" => matches!(sub, "test" | "build" | "check" | "clippy" | "nextest"),
        "go" => matches!(sub, "test" | "build" | "vet"),
        "python" | "python3" => {
            sub == "-m"
                && matches!(
                    words.next().unwrap_or(""),
                    "pytest" | "unittest" | "tox" | "nox"
                )
        }
        "deno" => matches!(sub, "test" | "lint" | "check"),
        "dotnet" => matches!(sub, "test" | "build"),
        "cmake" => sub == "--build",
        // Standalone test / lint / build binaries — the command name alone is the run.
        "pytest" | "tox" | "nox" | "ruff" | "mypy" | "flake8" | "pylint" | "jest" | "vitest"
        | "mocha" | "ava" | "tsc" | "eslint" | "phpunit" | "rspec" | "ctest" | "make"
        | "gradle" | "gradlew" | "mvn" | "rustc" => true,
        _ => false,
    }
}

/// A JS/TS package-manager script name that means build/test/lint (so `npm run <it>` is a
/// real runner). Prefix-matched so scoped scripts (`test:unit`, `lint:fix`, `build:prod`)
/// still count; a non-build script (`dev`, `start`, `serve`) is intentionally excluded.
fn is_js_build_test_verb(w: &str) -> bool {
    const ROOTS: &[&str] = &[
        "test",
        "build",
        "lint",
        "typecheck",
        "type-check",
        "check",
        "tsc",
        "e2e",
        "unit",
        "vitest",
        "jest",
        "coverage",
        "verify",
        "ci",
    ];
    ROOTS.iter().any(|r| {
        w.strip_prefix(r)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(':') || rest.starts_with('-'))
    })
}

/// A JS/TS tool a `npx` / `<pm> exec|dlx` invocation runs that is a build/test/lint runner.
fn is_js_test_tool(w: &str) -> bool {
    matches!(
        w,
        "jest" | "vitest" | "mocha" | "ava" | "tsc" | "eslint" | "playwright"
    )
}

/// A leading shell token that is NOISE before the real command — a wrapper command
/// (`sudo` / `time` / `env` / …) or a `NAME=value` env-assignment prefix — so the first
/// REAL command word is examined, not `sudo` / `FOO=bar`.
fn is_leading_command_noise(w: &str) -> bool {
    matches!(
        w,
        "sudo" | "time" | "command" | "exec" | "env" | "nice" | "then" | "do" | "!" | "\\"
    ) || w.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
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
    fn command_is_build_test_runner_recognises_common_runners() {
        // The OBSERVED-tool corroboration must fire for the common build/test/lint
        // runners across ecosystems — anchored at the real command word, tolerant of
        // env-assignments, wrapper commands, path prefixes, and shell chains.
        for cmd in [
            "npm test",
            "npm run build",
            "npm run test:unit",
            "npm ci",
            "pnpm run lint",
            "yarn build",
            "bun test",
            "npx jest --ci",
            "pnpm exec eslint .",
            "cargo test --workspace",
            "cargo clippy -- -D warnings",
            "cargo build --release",
            "go test ./...",
            "pytest -q",
            "python -m pytest tests/",
            "tox",
            "eslint src/",
            "npx tsc --noEmit",
            "./gradlew test",
            "mvn -q test",
            "make build",
            "deno test",
            "dotnet test",
            "FORCE_COLOR=1 CI=true npm test", // env-assignment prefix peeled
            "sudo make install",              // wrapper peeled
            "cd app && npm run build",        // runner in a later chain segment
            "cargo build 2>&1 | tee log",     // runner before a pipe
        ] {
            assert!(
                command_is_build_test_runner(cmd),
                "should read as a build/test/lint run: {cmd}"
            );
        }
    }

    #[test]
    fn command_is_build_test_runner_ignores_non_runners() {
        // A non-runner command must NOT corroborate — including a runner NAME that only
        // appears as a quoted argument to a non-runner (the narration-in-a-command hole),
        // a `#` comment, a dev-server / start script, and plain file ops.
        for cmd in [
            "echo \"npm test\"",
            "git commit -m \"run cargo test\"",
            "cat package.json",
            "ls -la src",
            "npm run dev",
            "npm run start",
            "node server.js",
            "rm -rf build",
            "# cargo test",
            "grep -r 'pytest' .",
            "mkdir -p dist",
            "",
        ] {
            assert!(
                !command_is_build_test_runner(cmd),
                "must NOT read as a build/test/lint run: {cmd:?}"
            );
        }
    }

    #[test]
    fn claims_code_changes_recognises_build_verbs() {
        // P1-3: the /run directive says "build it", so the base's most common "done"
        // phrasings ("I built …", "wrote …", "scaffolded …", "wired up …", "set up …")
        // MUST count as a code claim, or the honesty QC + source-present hard-gate are
        // skipped over a possibly-hallucinated build.
        for claim in [
            "I built the login page and wrote the tests. All done.",
            "Built the app end to end.",
            "Scaffolded the project and wired up the routes.",
            "Generated the API client and coded the form handler.",
            "Developed the dashboard and set up the auth flow.",
            "I'll build it now and report back.",
        ] {
            assert!(claims_code_changes(claim), "should claim a build: {claim}");
        }
        // Still no false positive on a pure plan / discussion (no build verb).
        assert!(!claims_code_changes(
            "Let me first discuss the trade-offs of each option before touching anything."
        ));
    }

    #[test]
    fn claims_code_changes_recognises_completion_phrasings_zh() {
        // L4 regression: the most common Chinese "I did the work" phrasings — 完成 /
        // 已完成 / 开发 — must register as a code claim, or the honesty/QC read is
        // skipped over a possibly-hallucinated "done".
        for claim in [
            "已完成登录功能,接口都接好了。",
            "开发了支付模块并联调通过。",
            "这部分已经完成。",
        ] {
            assert!(
                claims_code_changes(claim),
                "should claim work done: {claim}"
            );
        }
    }

    #[test]
    fn base_ran_build_test_clean_detects_a_passed_run_bilingually() {
        // M3: each positive case now carries a PASS phrase AND machine evidence (a named
        // runner command, an exit-code-0 signal, or a runner result line) — only then is
        // it safe to SKIP UmaDev's own objective read.
        for claim in [
            "I ran `npm test` and all tests pass.",
            "Built the app; the build succeeded (exit code 0) and lint passes.",
            "Ran cargo test, the test suite passes cleanly — test result: ok.",
            "构建成功,测试全部通过 (cargo test 退出码 0),可以交付了。",
            "我跑了一遍 pytest,编译通过、测试通过。",
        ] {
            assert!(
                base_ran_build_test_clean(claim),
                "should read as a clean self-run: {claim}"
            );
        }
    }

    #[test]
    fn base_ran_build_test_clean_requires_machine_evidence_not_prose_alone() {
        // M3 regression: a PASS phrase with NO machine evidence (no command, no exit
        // code, no runner output) is prose only — it must NOT trigger the skip, so a
        // hallucinated "it passes" can never bypass UmaDev's objective build/test read.
        for prose_only in [
            "I implemented it and all tests pass.",
            "The build succeeded and lint passes.",
            "构建成功,测试全部通过,可以交付了。",
            "编译通过、测试通过。",
        ] {
            assert!(
                !base_ran_build_test_clean(prose_only),
                "a prose pass-claim with no run evidence must NOT skip the floor: {prose_only}"
            );
        }
        // The SAME claims, now corroborated by machine evidence, DO qualify.
        assert!(base_ran_build_test_clean(
            "Ran `cargo test` — all tests pass."
        ));
        assert!(base_ran_build_test_clean(
            "构建成功,测试全部通过(退出码 0)。"
        ));
    }

    #[test]
    fn base_ran_build_test_clean_red_is_word_boundaried() {
        // L3 regression: a benign reply whose only "red" is inside a longer word
        // ("required" / "rendered" / "covered") must NOT be vetoed as a failure. With
        // machine evidence present, such a clean self-run still qualifies for the skip.
        assert!(
            base_ran_build_test_clean(
                "Ran `npm test`; every required route is covered and the page rendered — all tests pass."
            ),
            "'required'/'covered'/'rendered' must not trip the 'red' failure veto"
        );
        // A real standalone "red" (tests are red) still vetoes.
        assert!(!base_ran_build_test_clean(
            "Ran `npm test` but the suite is red — all tests pass once I fix it."
        ));
    }

    #[test]
    fn base_ran_build_test_clean_is_false_on_failure_or_ambiguity() {
        // A failure signal ANYWHERE vetoes the skip — UmaDev must run its own read.
        for txt in [
            "Tests pass for the model layer but the integration test failed.",
            "Build succeeded but lint is failing on two files.",
            "构建成功,但有一个测试失败了。",
            "编译通过,不过跑测试时报错了。",
        ] {
            assert!(
                !base_ran_build_test_clean(txt),
                "a failure signal must veto the skip: {txt}"
            );
        }
        // Ambiguous "done" with no explicit passed-run → no skip (conservative).
        for txt in [
            "Done — implemented the login form and the route.",
            "Looks good, the page renders.",
            "实现完了,你看一下。",
            "",
        ] {
            assert!(
                !base_ran_build_test_clean(txt),
                "ambiguous reply must NOT trigger the skip: {txt}"
            );
        }
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
    fn standard_choice_is_present_for_confirm_gates_and_absent_for_clarify() {
        // docs/preview confirm gates carry a 3-option approve/revise/add-more
        // choice (locale-free i18n keys); the clarify gate has no standard picker.
        for gate in [Gate::DocsConfirm, Gate::PreviewConfirm] {
            let c = GateChoice::standard(gate).expect("confirm gate has a choice");
            assert!(c.is_renderable());
            assert_eq!(c.options.len(), 3);
            assert_eq!(c.options[0].decision, GateDecision::Approve);
            assert_eq!(c.options[1].decision, GateDecision::Revise);
            assert_eq!(c.options[2].decision, GateDecision::AddMore);
            // Labels are carried as i18n KEYS, not localized strings.
            assert_eq!(c.options[0].label, "gate.choice.confirm");
        }
        assert!(GateChoice::standard(Gate::ClarifyGate).is_none());
    }

    #[test]
    fn empty_choice_is_not_renderable_fail_open() {
        let empty = GateChoice {
            question: "q".to_string(),
            options: vec![],
        };
        assert!(!empty.is_renderable());
    }

    #[test]
    fn gate_decision_ids_and_label_keys_are_stable() {
        for d in [
            GateDecision::Approve,
            GateDecision::Revise,
            GateDecision::AddMore,
            GateDecision::Cancel,
        ] {
            assert!(!d.id_str().is_empty());
            assert!(d.label_key().starts_with("gate.choice."));
        }
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
