//! **UD-ARCH-009**: hardcoded UI text without an i18n layer — judged only for a
//! project that is multi-language.
//!
//! A Chinese-only UI ("做一个记账页面") is the product's audience writing its
//! own language, not a missing i18n layer, so the content scan runs the rule
//! only when the [`ProjectContext`] records multi-language intent: the
//! requirement asks for i18n / 多语言 / 国际化, or the workspace already has an
//! i18n library or a locale catalog with two languages.

use std::path::Path;

use super::{extension_of, Decision, ProjectContext};

/// Words in a requirement that ask for a multi-language UI.
const REQUIREMENT_MARKERS: &[&str] = &[
    "i18n",
    "l10n",
    "internationaliz",
    "internationalis",
    "localization",
    "localisation",
    "localize",
    "localise",
    "multilingual",
    "multi-language",
    "multiple languages",
    "bilingual",
    "language switch",
    "switch language",
    "多语言",
    "多語言",
    "国际化",
    "國際化",
    "本地化",
    "多语种",
    "多語種",
    "双语",
    "雙語",
    "中英文切换",
    "中英文切換",
    "中英切换",
    "中英切換",
    "语言切换",
    "語言切換",
    "切换语言",
    "切換語言",
    "英文版",
];

/// i18n libraries whose presence in a `package.json` means the project localizes.
const I18N_PACKAGES: &[&str] = &[
    "i18next",
    "react-i18next",
    "next-i18next",
    "next-intl",
    "react-intl",
    "@formatjs/intl",
    "vue-i18n",
    "@nuxtjs/i18n",
    "svelte-i18n",
    "@lingui/core",
    "@lingui/react",
    "typesafe-i18n",
    "@ngx-translate/core",
    "@angular/localize",
    "@inlang/paraglide-js",
    "rosetta",
    "i18n-js",
];

/// Where a project keeps its `package.json`: the root, or a frontend folder.
const PACKAGE_DIRS: &[&str] = &["", "frontend", "web", "client", "app", "ui"];
/// Folders that hold locale catalogs, and the folders they usually sit in.
const CATALOG_PARENTS: &[&str] = &["", "src", "public", "app", "assets", "src/assets", "static"];
const CATALOG_DIRS: &[&str] = &[
    "locales",
    "locale",
    "i18n",
    "lang",
    "langs",
    "translations",
    "messages",
];
const MAX_PACKAGE_JSON_BYTES: u64 = 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 256;

impl ProjectContext {
    /// The same context, recording whether the project is multi-language (see
    /// [`ProjectContext::i18n_intent`]).
    #[must_use]
    pub const fn with_i18n_intent(mut self, intent: bool) -> Self {
        self.i18n_intent = intent;
        self
    }
}

/// Whether the content scan stands `check` down for this project: UD-ARCH-009
/// only applies to a multi-language project.
pub(super) fn stands_down(check: fn(&str, &str) -> Decision, ctx: ProjectContext) -> bool {
    !ctx.i18n_intent
        && std::ptr::fn_addr_eq(check, check_i18n_required as fn(&str, &str) -> Decision)
}

/// Whether `requirement` asks for a multi-language UI (i18n, localization,
/// 多语言, 国际化, 双语, 中英文切换, an English version, …).
#[must_use]
pub fn requirement_asks_for_i18n(requirement: &str) -> bool {
    let lower = requirement.to_lowercase();
    REQUIREMENT_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Whether the workspace at `project_root` already localizes: an i18n library
/// in the dependencies of its (or its frontend folder's) `package.json`, or a
/// locale catalog folder (`locales/`, `src/i18n/`, `public/locales/`, …) with
/// entries for at least two languages (`en.json` + `zh-CN.json`, `en/` +
/// `zh/`). Bounded and fail-open: an unreadable file or folder counts as
/// absent.
#[must_use]
pub fn project_declares_i18n(project_root: &Path) -> bool {
    has_i18n_dependency(project_root) || has_locale_catalog(project_root)
}

fn has_i18n_dependency(project_root: &Path) -> bool {
    PACKAGE_DIRS.iter().any(|dir| {
        let Ok(bytes) = umadev_state::fs::read_bounded_beneath(
            project_root,
            &Path::new(dir).join("package.json"),
            MAX_PACKAGE_JSON_BYTES,
        ) else {
            return false;
        };
        let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return false;
        };
        ["dependencies", "devDependencies", "peerDependencies"]
            .iter()
            .filter_map(|section| manifest.get(section)?.as_object())
            .any(|deps| I18N_PACKAGES.iter().any(|name| deps.contains_key(*name)))
    })
}

fn has_locale_catalog(project_root: &Path) -> bool {
    CATALOG_PARENTS.iter().any(|parent| {
        CATALOG_DIRS.iter().any(|dir| {
            let Ok(entries) = std::fs::read_dir(project_root.join(parent).join(dir)) else {
                return false;
            };
            entries
                .take(MAX_CATALOG_ENTRIES)
                .filter_map(Result::ok)
                .filter(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    is_locale_code(name.split('.').next().unwrap_or(&name))
                })
                .take(2)
                .count()
                == 2
        })
    })
}

/// `en`, `zh`, `zh-CN`, `zh_TW`, `pt-BR`, `zh-Hans`: a language code with an
/// optional region / script.
fn is_locale_code(stem: &str) -> bool {
    let (language, region) = stem
        .split_once(['-', '_'])
        .map_or((stem, None), |(language, region)| (language, Some(region)));
    (2..=3).contains(&language.len())
        && language.bytes().all(|b| b.is_ascii_alphabetic())
        && region.is_none_or(|region| {
            (2..=4).contains(&region.len()) && region.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// **UD-ARCH-009**: require i18n for hardcoded user-facing strings.
///
/// A multi-language product must not hardcode UI text — it needs an i18n layer
/// (react-intl / i18next / formatjs) so strings can be localized. Flags JSX
/// files that contain CJK characters in JSX text nodes or string literals
/// passed to user-facing props (`placeholder`/`label`/`title`/`<button>` text),
/// when no i18n import is present. Conservative: only flags CJK (the clearest
/// "this is a hardcoded UI string" signal) and only when no i18n setup exists.
///
/// The content scan runs this rule only for a multi-language project
/// ([`ProjectContext::i18n_intent`]); a single-language Chinese UI passes.
#[must_use]
pub fn check_i18n_required(file_path: &str, content: &str) -> Decision {
    let ext = extension_of(file_path);
    if !matches!(ext.as_str(), "jsx" | "tsx" | "vue" | "svelte") {
        return Decision::pass();
    }
    // If the file already imports an i18n library, it's set up correctly.
    if content.contains("react-intl")
        || content.contains("i18next")
        || content.contains("useTranslation")
        || content.contains("FormattedMessage")
        || content.contains("@formatjs")
        || content.contains("vue-i18n")
        || content.contains("$t(")
        || content.contains("i18n[")
        || (content.contains("_zh") && content.contains("_en"))
    {
        return Decision::pass();
    }
    // Scan for CJK characters in user-facing contexts (JSX text / string props).
    let has_cjk_ui = content
        .lines()
        .filter(|l| {
            // Skip comment lines.
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with('*') && !t.starts_with("/*")
        })
        .any(|line| {
            // CJK between `>` and `<` (JSX text node) or in a UI prop string.
            (line.contains('>') && line.contains('<') && has_cjk(line)) || has_cjk_in_prop(line)
        });
    if has_cjk_ui {
        return Decision::block(
            "UD-ARCH-009",
            format!(
                "UmaDev: hardcoded UI string without i18n (UD-ARCH-009). \
                 `{file_path}` has CJK user-facing text but no i18n import, in a \
                 project that ships more than one language. Wrap text with \
                 `<FormattedMessage>` / `t(\"key\")` from react-intl or i18next, \
                 and move the string to a locale file. (If this file is a test \
                 or demo, disable this clause in .umadev/rules.toml.)",
            ),
        );
    }
    Decision::pass()
}

/// `true` when the line contains a CJK ideograph (Unicode CJK Unified block).
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// `true` when a UI-prop string literal contains CJK (placeholder/label/title).
fn has_cjk_in_prop(line: &str) -> bool {
    for prop in [
        "placeholder=\"",
        "placeholder='",
        "label=\"",
        "label='",
        "title=\"",
        "title='",
    ] {
        if let Some(start) = line.find(prop) {
            let after = &line[start + prop.len()..];
            let end_quote = after.find(if prop.ends_with('"') { '"' } else { '\'' });
            if let Some(end) = end_quote {
                if has_cjk(&after[..end]) {
                    return true;
                }
            }
        }
    }
    false
}
