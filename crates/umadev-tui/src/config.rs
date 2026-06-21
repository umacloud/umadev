//! User-scope configuration at `~/.umadev/config.toml`.
//!
//! Stores the user's chosen runtime — a base CLI backend, an external
//! model/API provider, or offline templates — plus a few small UI preferences.
//! First-launch picker writes this file; later launches read it and skip
//! the picker.
//!
//! Format (all fields optional, future-additive):
//!
//! ```toml
//! # Drive a logged-in base CLI (umadev needs no API key of its own).
//! backend = "claude-code"
//! model = "claude-sonnet-4-6"
//! ```
//!
//! All read/write is fail-soft: a corrupt or missing file just means
//! "no preference yet — show the picker." Never panics.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "config.toml";
const DIR_NAME: &str = ".umadev";

/// The on-disk shape of the user config.
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct UserConfig {
    /// Stable backend id (`claude-code` / `codex` / `opencode` / `offline`).
    /// `None` triggers the first-launch picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,

    /// Model identifier passed to the worker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Optional per-phase model TIERS — plan with a cheaper/faster model, write
    /// code with a stronger one (the per-phase model assignment top agents use).
    /// `model_plan` drives research/docs/spec/quality; `model_build` drives the
    /// frontend/backend code phases. Unset → every phase uses [`Self::model`].
    /// Applied via the `UMADEV_MODEL_PLAN` / `UMADEV_MODEL_BUILD` env the runner
    /// reads, so the worker path needs no extra plumbing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_plan: Option<String>,
    /// Stronger model for the code phases — see [`Self::model_plan`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_build: Option<String>,

    /// Active design system name (e.g. `modern-minimal`, `tech-utility`).
    /// Saved to config so subsequent runs reuse the same visual direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design_system: Option<String>,

    /// Active seed template (e.g. `saas-landing`, `dashboard`, `blog-content`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_template: Option<String>,

    /// UI language code (`zh-CN` / `zh-TW` / `en`). `None` triggers system
    /// detection on first launch; the user can change it anytime via `/lang`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
}

impl UserConfig {
    /// Resolve the effective UI language: the saved code if valid, else detect
    /// from the system locale (default Simplified Chinese).
    #[must_use]
    pub fn resolved_lang(&self) -> umadev_i18n::Lang {
        self.lang
            .as_deref()
            .and_then(umadev_i18n::Lang::from_code)
            .unwrap_or_else(umadev_i18n::Lang::detect)
    }
}

impl UserConfig {
    /// `true` when the user has already picked a backend.
    #[must_use]
    pub fn has_backend(&self) -> bool {
        self.backend.is_some()
    }

    /// Export the per-phase model tiers ([`Self::model_plan`] /
    /// [`Self::model_build`]) to the `UMADEV_MODEL_PLAN` / `UMADEV_MODEL_BUILD`
    /// env the runner reads. Call once at startup and after any `/model
    /// plan|build` change so the in-process worker loop picks them up. An unset
    /// tier clears the env so it cleanly falls back to the single model.
    pub fn apply_model_tiers(&self) {
        for (var, val) in [
            ("UMADEV_MODEL_PLAN", self.model_plan.as_deref()),
            ("UMADEV_MODEL_BUILD", self.model_build.as_deref()),
        ] {
            match val {
                Some(m) if !m.is_empty() => std::env::set_var(var, m),
                _ => std::env::remove_var(var),
            }
        }
    }

    /// `claude-code` / `codex` / `offline` (default when unset).
    #[must_use]
    pub fn backend_or_default(&self) -> String {
        self.backend
            .clone()
            .unwrap_or_else(|| "offline".to_string())
    }
}

/// Default location: `$XDG_CONFIG_HOME/umadev/config.toml` if set,
/// else `$HOME/.umadev/config.toml`.
#[must_use]
pub fn default_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("umadev").join(FILE_NAME);
        }
    }
    // Cross-platform home: HOME on Unix, USERPROFILE on Windows.
    if let Some(home) = home_dir() {
        return home.join(DIR_NAME).join(FILE_NAME);
    }
    // Last-resort fallback so tests / CI never panic when HOME is unset.
    PathBuf::from(DIR_NAME).join(FILE_NAME)
}

/// Cross-platform home directory: `HOME` then `USERPROFILE` (Windows).
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .ok()
        .map(PathBuf::from)
}

/// Read the config from disk. Returns `Default::default()` on any
/// failure (missing file, parse error, IO error). Never panics.
#[must_use]
pub fn load() -> UserConfig {
    load_from(&default_path())
}

/// Read from a specific path. Same fail-soft behaviour.
#[must_use]
pub fn load_from(path: &std::path::Path) -> UserConfig {
    let Ok(body) = fs::read_to_string(path) else {
        return UserConfig::default();
    };
    toml::from_str(&body).unwrap_or_default()
}

/// Strictly load the config, surfacing a parse error instead of the fail-soft
/// reset-to-`Default`. `doctor` uses this so a corrupt `config.toml` (which
/// would otherwise silently wipe the user's backend/model/provider on the next
/// launch) is reported, not hidden. A missing file is `Ok(Default)`.
///
/// # Errors
/// Returns the read or TOML-parse error as a string.
pub fn load_strict(path: &std::path::Path) -> Result<UserConfig, String> {
    if !path.is_file() {
        return Ok(UserConfig::default());
    }
    let body = fs::read_to_string(path).map_err(|e| e.to_string())?;
    toml::from_str(&body).map_err(|e| e.to_string())
}

/// Write the config to disk at the default location, creating parent
/// directories as needed. Returns an `io::Error` so callers can surface
/// it to the user — but a write failure should never crash the TUI.
pub fn save(config: &UserConfig) -> std::io::Result<PathBuf> {
    save_to(config, &default_path())
}

/// Write to a specific path. Same semantics.
pub fn save_to(config: &UserConfig, path: &std::path::Path) -> std::io::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let body = toml::to_string_pretty(config).map_err(|e| std::io::Error::other(e.to_string()))?;
    // Atomic write (write-temp-then-rename): a crash mid-write must never corrupt
    // config.toml — it holds the backend, model, lang AND the provider api_key,
    // and load_from silently falls back to Default on a parse error (so a partial
    // write would silently wipe every setting). Rename within the same dir is
    // atomic on POSIX/Windows.
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, body)?;
    fs::rename(&tmp, path)?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_config_has_no_backend() {
        let cfg = UserConfig::default();
        assert!(cfg.backend.is_none());
        assert!(!cfg.has_backend());
    }

    #[test]
    fn round_trip_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = UserConfig {
            backend: Some("claude-code".into()),
            model: Some("claude-sonnet-4-6".into()),
            ..Default::default()
        };
        let written = save_to(&original, &path).unwrap();
        assert_eq!(written, path);
        let loaded = load_from(&path);
        assert_eq!(loaded, original);
    }

    #[test]
    fn load_from_missing_path_returns_default() {
        let tmp = TempDir::new().unwrap();
        let cfg = load_from(&tmp.path().join("nonexistent.toml"));
        assert_eq!(cfg, UserConfig::default());
    }

    #[test]
    fn load_from_corrupt_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.toml");
        fs::write(&path, "definitely not toml ===== broken ::: nope").unwrap();
        let cfg = load_from(&path);
        // Fail-soft: corrupt config doesn't crash; the picker just shows up again.
        assert!(!cfg.has_backend());
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a/b/c/config.toml");
        let cfg = UserConfig {
            backend: Some("codex".into()),
            model: None,
            ..Default::default()
        };
        save_to(&cfg, &deep).unwrap();
        assert!(deep.is_file());
    }

    #[test]
    fn backend_or_default_falls_back_to_offline() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.backend_or_default(), "offline");
        let cfg = UserConfig {
            backend: Some("claude-code".into()),
            model: None,
            ..Default::default()
        };
        assert_eq!(cfg.backend_or_default(), "claude-code");
    }

    #[test]
    fn default_path_honours_xdg_config_home() {
        // SAFETY: we mutate environment then restore — single-threaded test
        // runner (default). The env var is process-wide so we must reset
        // it before returning.
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-test");
        let p = default_path();
        // Restore before any potential panic from assertions.
        if let Some(v) = prev {
            std::env::set_var("XDG_CONFIG_HOME", v);
        } else {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        assert!(p.starts_with("/tmp/xdg-test/umadev"));
        assert!(p.ends_with(FILE_NAME));
    }
}
