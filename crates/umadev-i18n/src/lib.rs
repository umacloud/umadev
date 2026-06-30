//! `umadev-i18n` — language detection + a tri-lingual string catalog for
//! UmaDev's own UI chrome (picker, menus, hints, status lines, gate prompts,
//! errors, help).
//!
//! Scope: this localizes the **deterministic shell**. The pipeline *content*
//! (PRD / architecture / code) is produced by the borrowed base model, which
//! already answers in whatever language the user writes — so it is not routed
//! through this catalog.
//!
//! Three languages, default Simplified Chinese:
//! - `zh-CN` 简体中文 (default / fallback)
//! - `zh-TW` 繁體中文
//! - `en` English
//!
//! Strings live in `catalog/<lang>.toml` (flat, quoted dotted keys), embedded
//! at compile time. Lookup falls back to `zh-CN` then the key itself, and a
//! parity test guarantees every key exists in all three catalogs so the
//! fallback never ships.

#![forbid(unsafe_code)]
#![warn(missing_docs, clippy::all, clippy::pedantic)]
#![allow(clippy::doc_markdown)]

use std::collections::HashMap;
use std::sync::OnceLock;

/// A supported UI language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Lang {
    /// Simplified Chinese — the default and fallback locale.
    #[default]
    ZhCn,
    /// Traditional Chinese.
    ZhTw,
    /// English.
    En,
}

impl Lang {
    /// All languages in display order (used to render the first-run picker).
    pub const ALL: [Lang; 3] = [Lang::ZhCn, Lang::ZhTw, Lang::En];

    /// Stable BCP-47-ish code persisted in config (`zh-CN` / `zh-TW` / `en`).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Lang::ZhCn => "zh-CN",
            Lang::ZhTw => "zh-TW",
            Lang::En => "en",
        }
    }

    /// Native display label (shown in the language picker, in its own script).
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Lang::ZhCn => "简体中文",
            Lang::ZhTw => "繁體中文",
            Lang::En => "English",
        }
    }

    /// Parse a persisted code back into a [`Lang`]. Tolerant of case and of a
    /// bare `zh` (→ Simplified). Returns `None` for anything unrecognised.
    #[must_use]
    pub fn from_code(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "zh-cn" | "zh-hans" | "zh" | "cn" => Some(Lang::ZhCn),
            "zh-tw" | "zh-hant" | "zh-hk" | "tw" => Some(Lang::ZhTw),
            "en" | "en-us" | "en-gb" | "c" | "posix" => Some(Lang::En),
            _ => None,
        }
    }

    /// Detect the UI language across macOS / Windows / Linux. An explicit
    /// Chinese locale env (`LC_ALL` / `LC_MESSAGES` / `LANG` / `LANGUAGE`)
    /// always wins. Otherwise we read the real system UI language via the OS
    /// API (`sys-locale`: CFLocale on macOS, `GetUserDefaultLocaleName` on
    /// Windows, `LANG`/`LC_*` on Linux) — crucial on macOS, where the terminal
    /// `LANG` is usually `en_US` regardless of the UI language, and on Windows,
    /// where it is unset. Falls back to the env locale, else default Simplified.
    #[must_use]
    pub fn detect() -> Self {
        let raw = ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE"]
            .iter()
            // Skip empty-but-set vars and fall through to the next.
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
            .unwrap_or_default()
            .to_ascii_lowercase();
        // An explicit Chinese locale wins immediately.
        if raw.starts_with("zh") {
            return if raw.contains("tw") || raw.contains("hk") || raw.contains("hant") {
                Lang::ZhTw
            } else {
                Lang::ZhCn
            };
        }
        // Native OS UI language — CFLocale on macOS, GetUserDefaultLocaleName on
        // Windows, LANG/LC_* on Linux (via sys-locale). Reliable cross-platform,
        // unlike a bare terminal LANG read (which lies on macOS and is empty on
        // Windows).
        if let Some(lang) =
            sys_locale::get_locale().and_then(|s| lang_from_locale(&s.to_ascii_lowercase()))
        {
            return lang;
        }
        if raw.starts_with("en") {
            Lang::En
        } else {
            Lang::ZhCn
        }
    }
}

/// Map a BCP-47 / POSIX locale string (`zh-hans-cn`, `zh-tw`, `en-us`, `fr-fr`)
/// to a [`Lang`]. Traditional for Hant / TW / HK / MO; Simplified for any other
/// Chinese; English for `en*`; `None` for anything else (caller defaults).
/// Expects lowercase input.
fn lang_from_locale(s: &str) -> Option<Lang> {
    if s.starts_with("zh") {
        if s.contains("hant") || s.contains("-tw") || s.contains("-hk") || s.contains("-mo") {
            Some(Lang::ZhTw)
        } else {
            Some(Lang::ZhCn)
        }
    } else if s.starts_with("en") {
        Some(Lang::En)
    } else {
        None
    }
}

use std::sync::atomic::{AtomicU8, Ordering};

/// Process-wide current UI language, so free functions (gate cards, status
/// lines, slash replies) can localize without threading `Lang` through every
/// signature. Set once at launch from config and again on `/lang` / picker.
static CURRENT: AtomicU8 = AtomicU8::new(0); // 0 = ZhCn (default)

/// Set the process-wide current UI language. Call on launch and whenever the
/// user switches language.
pub fn set_lang(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// The process-wide current UI language (defaults to Simplified Chinese).
#[must_use]
pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::ZhTw,
        2 => Lang::En,
        _ => Lang::ZhCn,
    }
}

/// Look up `key` in the CURRENT language (see [`set_lang`]). Falls back to
/// `zh-CN` then empty (the parity test guarantees no key is ever missing).
#[must_use]
pub fn tl(key: &str) -> &'static str {
    let lang = current();
    catalogs()[lang as usize]
        .get(key)
        .or_else(|| catalogs()[Lang::ZhCn as usize].get(key))
        .map_or("", String::as_str)
}

/// [`tl`] with positional `{}` substitution (see [`tf`]).
#[must_use]
pub fn tlf(key: &str, args: &[&str]) -> String {
    let mut out = tl(key).to_string();
    for a in args {
        if let Some(pos) = out.find("{}") {
            out.replace_range(pos..pos + 2, a);
        } else {
            break;
        }
    }
    out
}

fn parse_catalog(src: &str) -> HashMap<String, String> {
    toml::from_str(src).unwrap_or_default()
}

fn catalogs() -> &'static [HashMap<String, String>; 3] {
    static C: OnceLock<[HashMap<String, String>; 3]> = OnceLock::new();
    C.get_or_init(|| {
        [
            parse_catalog(include_str!("../catalog/zh-CN.toml")),
            parse_catalog(include_str!("../catalog/zh-TW.toml")),
            parse_catalog(include_str!("../catalog/en.toml")),
        ]
    })
}

/// Look up `key` in `lang`'s catalog. Falls back to `zh-CN`, then to the key
/// itself (so a missing string is visible, not blank). The catalog-parity test
/// guarantees the fallback never triggers in a shipped build.
#[must_use]
pub fn t(lang: Lang, key: &str) -> &str {
    if let Some(v) = catalogs()[lang as usize].get(key) {
        return v;
    }
    if let Some(v) = catalogs()[Lang::ZhCn as usize].get(key) {
        return v;
    }
    key
}

/// Like [`t`], but substitutes positional `{}` placeholders left-to-right with
/// `args` (UI strings rarely need more than two). Extra `{}` are left as-is;
/// extra args are ignored.
#[must_use]
pub fn tf(lang: Lang, key: &str, args: &[&str]) -> String {
    let mut out = t(lang, key).to_string();
    for a in args {
        if let Some(pos) = out.find("{}") {
            out.replace_range(pos..pos + 2, a);
        } else {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip() {
        for l in Lang::ALL {
            assert_eq!(Lang::from_code(l.code()), Some(l));
        }
    }

    #[test]
    fn detect_maps_locales() {
        // Pure mapping check via from_code (detect() reads the live env).
        assert_eq!(
            Lang::from_code("zh_CN.UTF-8".split('.').next().unwrap()),
            Some(Lang::ZhCn)
        );
        assert_eq!(Lang::from_code("zh-TW"), Some(Lang::ZhTw));
        assert_eq!(Lang::from_code("en_US"), None.or(Lang::from_code("en")));
        assert_eq!(Lang::from_code("fr_FR"), None); // unknown → caller defaults
    }

    #[test]
    fn catalog_parity_no_missing_keys() {
        // Every key present in ANY catalog must exist in ALL three — otherwise
        // a user on one language silently sees the fallback. This is the gate
        // that lets `t()`'s key-fallback never ship.
        let cats = catalogs();
        let mut all_keys: Vec<&String> = cats.iter().flat_map(HashMap::keys).collect();
        all_keys.sort();
        all_keys.dedup();
        for (i, lang) in Lang::ALL.iter().enumerate() {
            for k in &all_keys {
                assert!(
                    cats[i].contains_key(*k),
                    "catalog {} is missing key `{}`",
                    lang.code(),
                    k
                );
            }
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)] // a flat registry of guarded keys; length is data, not logic.
    fn migrated_tui_keys_present_in_all_langs() {
        // Guard the keys migrated out of hard-coded TUI strings (overlay / help
        // hints, /model tiers, checkpoint+rewind labels, /deploy, and the
        // background worker / preview / deploy spawn errors). A developer who
        // adds one of these surfaces by writing literal English (or Chinese)
        // again — instead of a catalog key — would let the parity gate pass
        // while still shipping a wrong-language string; this test fails loudly
        // and tells them to add the key in all three catalogs.
        const MIGRATED: &[&str] = &[
            "run.classified_build_memo",
            "checkpoint.phase_label",
            "checkpoint.manual_label",
            "checkpoint.created",
            "checkpoint.git_required",
            "rewind.empty",
            "rewind.list_header",
            "rewind.restored",
            "rewind.failed",
            "model.tiers_current",
            "model.tiers_hint",
            "model.tiers_busy",
            "model.tiers_default",
            "model.tiers_default_paren",
            "model.tiers_updated",
            "deploy.confirm_preflight",
            "worker.init_failed",
            "pipeline.start_failed",
            "worker.timeout",
            "worker.not_on_path",
            "worker.exited",
            "pipeline.generic_error",
            "pipeline.error_note",
            "base.init_failed",
            // Idle-watchdog diagnosis (run + chat paths): the base went silent with no
            // tool running (looks hung) OR the run budget was reached — while a tool
            // runs UmaDev keeps waiting as long as the base is alive (liveness-based, no
            // fixed cap). Phrased for that reality, not a misleading auth/login hint.
            "base.fail.idle",
            // Visible retry: a transient base hiccup (429 / overloaded / network) is
            // backed off + retried with a COUNTDOWN Note (never a silent wait); a
            // non-tool silent hang on a live base is re-driven ONCE before failing.
            "tui.retry.countdown",
            "tui.retry.silent_redrive",
            "route.resume_retry",
            "base.empty_reply",
            "route.failed",
            "preview.dev_starting",
            "preview.dev_ready",
            "preview.dev_not_ready",
            "preview.dev_spawn_failed",
            "preview.port_busy",
            // Build-complete card + Delivery preview line (every build path:
            // chat / Fast / Delivery) — the "✅ done + what changed + preview URL".
            "delivery.preview_line",
            "build.complete.title",
            "build.complete.files",
            "build.complete.files_more",
            "build.complete.no_files",
            "build.complete.entry",
            "build.complete.run",
            "build.complete.preview_starting",
            "build.complete.preview_line",
            "deploy.running",
            "deploy.login_hint",
            "deploy.done",
            "deploy.done_no_url",
            "deploy.failed",
            "deploy.exec_failed",
            "deploy.timeout",
            "deploy.detected",
            "deploy.proof_written",
            "deploy.no_target",
            // Wave 4-b: TUI prose swept out of hard-coded ui.rs / app.rs strings.
            "tui.too_small.title",
            "tui.too_small.resize",
            "tui.too_small.now",
            "tui.title.workspace_placeholder",
            "tui.gate_block.title",
            "tui.gate_block.hint",
            "tui.scroll.both",
            "tui.scroll.above",
            "tui.hint.palette",
            "tui.hint.gate_tag",
            "tui.hint.gate_action",
            "tui.hint.multiline",
            "tui.hint.typed",
            "tui.hint.finished",
            "tui.hint.running",
            "tui.hint.idle",
            // Phase-2-C-P1: the persistent token/cost gauge in the meta row, and
            // the idle double-Esc rewind hint.
            "tui.gauge.tokens",
            "tui.gauge.cost",
            "tui.rewind.hint",
            "tui.palette.title",
            // Phase-2-C-P1: in-transcript search (Ctrl+F) — prompt label, live
            // match counter, empty-result note, and the open-search prompt hint.
            "tui.search.prompt",
            "tui.search.count",
            "tui.search.none",
            "tui.hint.search",
            // Reverse prompt-history search (Ctrl+R): the prompt label + the
            // operation hint shown while incremental history search owns the input.
            "tui.histsearch.prompt",
            "tui.hint.histsearch",
            "tui.status.complete",
            "tui.overlay.empty",
            "tui.overlay.progress",
            "tui.help.header_picker",
            "tui.help.header_chat",
            "tui.help.scroll_hint",
            "tui.help.group.navigation",
            "tui.help.group.worker",
            "tui.help.group.pipeline",
            "tui.help.group.ship",
            "tui.help.group.inspect",
            "tui.help.group.editing",
            "tui.help.nav.move",
            "tui.help.worker.offline",
            "tui.help.pipe.enter",
            "tui.help.ship.preview",
            "tui.help.inspect.design",
            "tui.help.edit.newline",
            // P2 polish: the keyboard-shortcut cheatsheet rows added to the
            // /help overlay's "Keys" group (real bindings only), plus the
            // `!`-prefixed local convenience-shell runtime strings.
            "tui.help.key.mention",
            "tui.help.key.shell",
            "tui.help.key.trust",
            "tui.help.key.search",
            "tui.help.key.redraw",
            "tui.help.key.scroll",
            "tui.help.key.jump",
            "tui.help.key.wheel",
            "tui.bang.exit",
            "tui.bang.failed",
            "tui.bang.spawn_failed",
            "tui.bang.timeout",
            "tui.bang.no_output",
            "event.phase_done",
            "event.verify_started",
            "event.verify_skipped",
            "event.verify_passed",
            "event.verify_failed",
            "event.subtask_started",
            "event.subtask_completed",
            "event.subtask_done",
            "event.subtask_failed",
            "agentic.inspecting",
            "agentic.working_on",
            "agentic.done",
            "slash.history_cleared",
            "slash.gate_approved",
            "slash.mcp_header",
            "slash.skill_header",
            "slash.mouse_on",
            "slash.mouse_off",
            "spec.overlay_title",
            "doctor.heading",
            "doctor.binary",
            "doctor.worker_availability",
            "doctor.overlay_title",
            // Wave 4 (tui): honest aborted-state + silent-failure surfaces.
            "input.aborted",
            "status.aborted",
            "tui.hint.aborted",
            "config.save_failed_note",
            "chat.claims_unverified",
            "chat.director_build_with_history",
            "gate.clarify_write_failed",
            // P2-D: gate-card artifact health labels (were hard-coded English).
            "gate.detail.missing",
            "gate.detail.scaffold",
            "gate.detail.short",
            "gate.detail.ok",
            "gate.detail.dark_ok",
            "gate.detail.dark_missing",
            // Continuous long-session path: CLI prints + TUI block notes + the
            // phase-progress and role-team review notes swept out of hard-coded
            // (mostly zh-only) literals in main.rs / tui lib.rs / continuous.rs.
            "continuous.session_active",
            "continuous.session_unavailable",
            "continuous.auto_gate_resumed",
            "continuous.lean_complete",
            "continuous.hardstop_report",
            "continuous.tui_session_unavailable",
            "continuous.block_aborted_busy",
            "continuous.block_aborted_locked",
            "continuous.block_aborted_io",
            "continuous.plan_mode_skip",
            "continuous.phase_failed",
            "continuous.no_source_hardstop",
            "continuous.dangerous_action_denied",
            "continuous.tool_call_blocked",
            "continuous.phase_truncated",
            "continuous.phase_truncated_degraded",
            "continuous.node.docs",
            "continuous.node.preview",
            "continuous.node.quality",
            "continuous.team.passed_after_rework",
            "continuous.team.unresolved_advisory",
            "continuous.team.inject_rework",
            "continuous.team.cross_review_header",
            "continuous.team.seat_passed",
            "continuous.team.seat_blocking",
            "continuous.verify_failed",
            "continuous.quality_gate_result",
            "continuous.quality_gate_advisory",
            "continuous.quality_gate_findings",
            "continuous.quality_gate_blocked",
            "continuous.governance_catchup",
            "continuous.governance_clean",
            "continuous.governance_remaining",
            "continuous.governance_rework_intro",
            // Wave 1 (director-driven `/run`): the director path's terminal report
            // + its objective source-present hard-stop.
            "director.run_done",
            "director.no_source_hardstop",
            // Wave 5 (memory + conversation): persistent chat restore, the
            // /sessions //resume //compact surfaces, cross-session goal continuity,
            // and the new help rows.
            "chat.restored",
            "sessions.empty",
            "sessions.header",
            "resume.usage",
            "resume.not_found",
            "resume.done",
            "compact.too_short",
            "compact.summary",
            "compact.done",
            "session.resume_goal",
            "tui.help.inspect.sessions",
            "tui.help.inspect.resume",
            "tui.help.edit.compact",
            // Tool-call beautification + long-output folding (TUI transcript
            // restructure): the merged read/grep batch headline, the grep metric,
            // and the collapse/expand hints.
            "tui.tool.batch",
            "tui.tool.matches",
            "tui.tool.aborted",
            "tui.fold.collapsed",
            "tui.fold.expand_hint",
            "tui.fold.hard_capped",
            "tui.thinking.expand_hint",
            "tui.diff.collapsed",
            "tui.diff.truncated",
            "tui.help.edit.expand",
            // `/sandbox` — in-app view/change of the Codex base launch sandbox
            // (current tier + the three options with a one-line WHY each, the
            // set/danger-set confirmations, and the fail-open persist warning).
            "sandbox.current",
            "sandbox.why.read_only",
            "sandbox.why.workspace_write",
            "sandbox.why.danger",
            "sandbox.usage",
            "sandbox.codex_only",
            "sandbox.set",
            "sandbox.danger_set",
            "sandbox.persist_failed",
            // Wave C: the team constitution — the user-readable/editable charter of
            // the team's non-negotiable operating principles, rendered by
            // `umadev_agent::render_constitution` and surfaced by the TUI
            // `/constitution` command.
            "constitution.title",
            "constitution.intro",
            "constitution.section.craft",
            "constitution.section.security",
            "constitution.section.governance",
            "constitution.article.icons",
            "constitution.article.tokens",
            "constitution.article.antislop",
            "constitution.article.contract",
            "constitution.article.craft",
            "constitution.article.evidence",
            "constitution.article.secrets",
            "constitution.article.sensitive_paths",
            "constitution.article.floor",
            "constitution.article.irreversible",
            "constitution.footer",
            "constitution.overlay_title",
            "constitution.edit_hint",
            "constitution.generated",
            "tui.cmd.constitution",
            // Background-run task registry + `/tasks` management surface: the
            // second-run guard, the list/stop/resume prose, the per-row status
            // labels, the command desc, and the compact `[run X/Y]` meta chip.
            "run.already_active",
            "tasks.empty",
            "tasks.header",
            "tasks.untitled",
            "tasks.actions_hint",
            "tasks.none_active",
            "tasks.already_running",
            "tasks.nothing_to_resume",
            "tasks.usage",
            "tasks.status.running",
            "tasks.status.done",
            "tasks.status.failed",
            "tasks.status.stopped",
            "tui.cmd.tasks",
            "tui.chip.run",
            "tui.chip.run_indeterminate",
            // Large-paste collapse: a bulky bracketed paste folds into this
            // `[粘贴 N 行]` chip (and re-expands on submit) instead of flooding
            // the input box — trilingual so the chip never ships a wrong-language
            // label.
            "attach.paste",
            // Wave C: the live team roster panel + handoff timeline — the convened
            // seats as named teammates with their live status (idle/working/
            // reviewing/blocked/done), and the seat→deliverable handoff entries.
            "team.roster.panel.title",
            "team.seat.pm",
            "team.seat.architect",
            "team.seat.designer",
            "team.seat.frontend",
            "team.seat.backend",
            "team.seat.qa",
            "team.seat.security",
            "team.seat.devops",
            "team.status.idle",
            "team.status.working",
            "team.status.reviewing",
            "team.status.blocked",
            "team.status.done",
            "team.handoff.header",
            "team.handoff.entry",
            // Trust ledger: the one-time note shown when the user approves a
            // guarded confirmation and the action class is remembered.
            "trust.approval_remembered",
            // I9: the first-run rotating example tip layered above the idle input
            // placeholder (three templates + the generic-file fallback token).
            "input.example.refactor",
            "input.example.tests",
            "input.example.explain",
            "input.example.file_generic",
            // Structured-choice gate picker: the per-gate question, the
            // approve/revise/add-more/cancel option labels, the driver hint, and
            // the revise/add-more free-text follow-up prompts.
            "gate.choice.docs.question",
            "gate.choice.preview.question",
            "gate.choice.confirm",
            "gate.choice.revise",
            "gate.choice.add_more",
            "gate.choice.cancel",
            "gate.choice.hint",
            "gate.choice.revise.prompt",
            "gate.choice.add_more.prompt",
            // Base AskUserQuestion bridge: the base asked a structured multiple-choice
            // question while driven non-interactively — the question + options are
            // surfaced (not a bare stub) and the user's reply is relayed back.
            "ask.prompt.header",
            "ask.prompt.relay_hint",
        ];
        let cats = catalogs();
        for lang in Lang::ALL {
            for key in MIGRATED {
                assert!(
                    cats[lang as usize].contains_key(*key),
                    "catalog {} is missing migrated key `{}` — add it (in all three \
                     catalogs) instead of hard-coding the string in the TUI",
                    lang.code(),
                    key
                );
            }
        }
    }

    #[test]
    fn lookup_and_format() {
        // Seeded key exists in all langs.
        assert!(!t(Lang::ZhCn, "picker.title").is_empty());
        assert_ne!(t(Lang::En, "picker.title"), t(Lang::ZhCn, "picker.title"));
        // Unknown key falls back to itself.
        assert_eq!(t(Lang::En, "no.such.key"), "no.such.key");
        // Positional format.
        let s = tf(Lang::En, "lang.changed", &["English"]);
        assert!(s.contains("English"));
    }

    #[test]
    fn locale_mapping_covers_all_variants() {
        assert_eq!(lang_from_locale("zh-hans-cn"), Some(Lang::ZhCn));
        assert_eq!(lang_from_locale("zh-cn"), Some(Lang::ZhCn));
        assert_eq!(lang_from_locale("zh-sg"), Some(Lang::ZhCn));
        assert_eq!(lang_from_locale("zh-hant-tw"), Some(Lang::ZhTw));
        assert_eq!(lang_from_locale("zh-tw"), Some(Lang::ZhTw));
        assert_eq!(lang_from_locale("zh-hk"), Some(Lang::ZhTw));
        assert_eq!(lang_from_locale("zh-mo"), Some(Lang::ZhTw));
        assert_eq!(lang_from_locale("en-us"), Some(Lang::En));
        assert_eq!(lang_from_locale("en-gb"), Some(Lang::En));
        assert_eq!(lang_from_locale("fr-fr"), None);
    }
}
