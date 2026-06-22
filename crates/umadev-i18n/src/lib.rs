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
            "route.resume_retry",
            "base.empty_reply",
            "route.failed",
            "preview.dev_starting",
            "preview.dev_ready",
            "preview.dev_not_ready",
            "preview.dev_spawn_failed",
            "preview.port_busy",
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
            "tui.palette.title",
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
            "tui.hint.aborted",
            "config.save_failed_note",
            "chat.claims_unverified",
            "gate.clarify_write_failed",
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
