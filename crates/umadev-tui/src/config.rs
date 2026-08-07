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
//! # The base runs on ITS OWN configured model — UmaDev never sets one.
//! backend = "claude-code"
//! ```
//!
//! All read/write is fail-soft: a corrupt or missing file just means
//! "no preference yet — show the picker." Never panics.

use std::fs;
use std::io::Read as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "config.toml";
const DIR_NAME: &str = ".umadev";
/// A user/base CLI config is human-authored metadata, never a document store.
/// Keeping one shared ceiling prevents a damaged file or special filesystem
/// node from wedging TUI startup while still leaving ample room for provider
/// model catalogs embedded by OpenCode.
pub(crate) const MAX_USER_CONFIG_BYTES: u64 = 4 * 1024 * 1024;

#[cfg(test)]
pub(crate) static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read a small user-owned config through one already-open handle.
///
/// Dotfile managers commonly expose these files through symlinks, so unlike
/// workspace-managed state this intentionally follows a link to a regular file.
/// The opened handle is then type-checked and byte-bounded. `O_NONBLOCK` keeps a
/// mistakenly-created FIFO from hanging startup before that check can reject it.
pub(crate) fn read_user_config(path: &std::path::Path) -> std::io::Result<String> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "config input is not a regular file",
        ));
    }
    if metadata.len() > MAX_USER_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("config exceeds {MAX_USER_CONFIG_BYTES} bytes"),
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0).min(64 * 1024));
    file.take(MAX_USER_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_USER_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("config exceeds {MAX_USER_CONFIG_BYTES} bytes while being read"),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// The on-disk shape of the user config.
#[derive(Debug, Clone, Eq, PartialEq, Default, Serialize, Deserialize)]
pub struct UserConfig {
    /// Stable backend id (`claude-code` / `codex` / `opencode` / `grok-build` /
    /// `kimi-code`;
    /// `offline` is an explicit internal fallback).
    /// `None` triggers the first-launch picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,

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

    /// Show the base's long-running command (build / install / `spring-boot:run`)
    /// output in the transcript as it runs, instead of the tight 200-char clip on
    /// completion. Off by default. Toggled live via `/logs`; published into the
    /// host drivers' thread-safe shared flag (see [`Self::publish_process_logs`]).
    /// Only serialized when `true` so a default config stays clean.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_process_logs: bool,

    /// How approval questions (both UmaDev's own gate checkpoints and the base's
    /// `AskUserQuestion`) are presented: `"picker"` (default) renders a numbered
    /// multiple-choice picker; `"text"` frames the question + its options as prose
    /// the user answers in natural language. The free-text reply path already works
    /// either way — only the presentation changes. Toggled live via `/questions`;
    /// published into the agent's shared flag (see [`Self::apply_question_form`]).
    /// `None`/unset means the default picker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question_form: Option<String>,

    /// Schema/migration version of this persisted config. Bumped by the ordered
    /// startup migration runner ([`run_migrations`]), which applies idempotent,
    /// fail-soft upgrade steps **once** per upgrade and then saves the new
    /// version. Defaults to `0` (via `#[serde(default)]`) so a config written by
    /// a pre-versioning build loads cleanly and gets every migration applied on
    /// the first launch after the upgrade. Always serialized so the gate
    /// persists.
    #[serde(default)]
    pub migration_version: u32,
}

/// One ordered config migration: an idempotent, fail-soft function that upgrades
/// the persisted config by exactly one version step. Append new steps to the END
/// of [`MIGRATIONS`] only — never reorder or remove existing ones, since a
/// migration's INDEX is its version number.
type Migration = fn(&mut UserConfig);

/// The ordered list of config migrations. The runner ([`run_migrations`]) applies
/// every migration whose index is `>=` the config's current
/// [`UserConfig::migration_version`], in order, then records the new version. Each
/// fn MUST be idempotent (safe to re-run) and is executed inside a panic guard so
/// one bad step is logged-and-skipped, never breaking startup.
const MIGRATIONS: &[Migration] = &[
    // v0 -> v1: collapse any empty-string optional field back to `None`. A legacy
    // build (or a hand-edit) could persist `backend = ""` / `lang = ""` where
    // `None` is meant, which would wrongly skip the first-launch picker or pin an
    // unknown language. Idempotent: a `None` or non-empty value is untouched.
    migrate_empty_strings_to_none,
    // v1 -> v2: retire base CLIs that no longer meet the product's first-class
    // integration bar. Clearing ONLY the backend reopens the picker while keeping
    // every other user preference; it never silently selects offline or a
    // different base. Aliases persisted by earlier command versions are included.
    migrate_retired_backends_to_picker,
];

/// Target version of the retired-backend migration (v1 -> v2). Used by the
/// startup reporter to distinguish the one upgrade launch from later launches.
const RETIRED_BACKEND_MIGRATION_VERSION: u32 = 2;

/// The version a freshly-migrated config carries — exactly the number of
/// migrations. A config already at this version is left untouched by the runner.
// The migration list is statically tiny; the count can never overflow a u32.
#[allow(clippy::cast_possible_truncation)]
pub const CURRENT_MIGRATION_VERSION: u32 = MIGRATIONS.len() as u32;

/// v0 -> v1: collapse empty-string optionals to `None` (see [`MIGRATIONS`]).
fn migrate_empty_strings_to_none(cfg: &mut UserConfig) {
    for field in [
        &mut cfg.backend,
        &mut cfg.design_system,
        &mut cfg.seed_template,
        &mut cfg.lang,
        &mut cfg.question_form,
    ] {
        if field.as_deref().is_some_and(str::is_empty) {
            *field = None;
        }
    }
}

/// Whether a persisted id belongs to a backend retired by the v1 -> v2
/// migration. Matching is tolerant of casing and surrounding whitespace because
/// old configs may have been hand-edited.
fn is_retired_backend(id: &str) -> bool {
    matches!(
        id.trim().to_ascii_lowercase().as_str(),
        "cursor" | "codebuddy" | "cbc" | "droid" | "qwen" | "qwen-code"
    )
}

/// v1 -> v2: clear retired backend ids so startup must ask the user to choose
/// one of the five supported bases. No fallback is selected on their behalf.
fn migrate_retired_backends_to_picker(cfg: &mut UserConfig) {
    if cfg.backend.as_deref().is_some_and(is_retired_backend) {
        cfg.backend = None;
    }
}

/// Run every PENDING migration once, in order, advancing
/// [`UserConfig::migration_version`] to [`CURRENT_MIGRATION_VERSION`]. Returns
/// `true` when the config changed (so the caller should persist it). A config
/// already at (or beyond) the current version is a no-op returning `false` — a
/// config from a *newer* build is never downgraded.
///
/// Fail-soft by contract: each step runs inside [`std::panic::catch_unwind`], so
/// a panicking migration is logged and skipped while the version still advances —
/// startup never breaks, and a persistently-bad step can't wedge every launch.
pub fn run_migrations(cfg: &mut UserConfig) -> bool {
    run_migrations_with(cfg, MIGRATIONS)
}

/// Inner runner over an explicit migration slice — lets tests exercise the
/// version-gate, idempotency, and fail-soft (panicking-step) behaviour with a
/// custom list without touching the production [`MIGRATIONS`].
fn run_migrations_with(cfg: &mut UserConfig, migrations: &[Migration]) -> bool {
    // Work in usize internally (a u32->usize widen is lossless); only the stored
    // version is u32, written via a saturating try_from at the boundary.
    let target = migrations.len();
    let start = cfg.migration_version as usize;
    if start >= target {
        // Already current (or a newer build's config) — never re-run or downgrade.
        return false;
    }
    for (idx, migration) in migrations.iter().enumerate().skip(start) {
        // Guard each step: a panic must never break startup. `AssertUnwindSafe`
        // is sound — a panicked step may leave `cfg` partially mutated, but we
        // only ever ADVANCE the version, so the next launch resumes forward.
        let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| migration(cfg)));
        if guarded.is_err() {
            // Fail-soft: the default panic hook already logged it; we swallow and
            // keep going so one bad migration doesn't abort the rest.
        }
        cfg.migration_version = u32::try_from(idx + 1).unwrap_or(u32::MAX);
    }
    true
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

    /// Publish the saved process-log preference into the host drivers' **thread-safe
    /// shared flag** (`umadev_host::process_logs::set_show_process_logs`), so the
    /// choice takes effect on the next streamed event. Call at startup (from
    /// [`Self::apply_process_logs`]) and after a live `/logs` toggle. Associated
    /// (not `&self`) so the live toggle can publish the new value without
    /// re-borrowing the whole config.
    ///
    /// Uses shared state, NOT the process env: the drivers read the flag from
    /// background tasks while a turn streams, so a runtime `set_var`/`remove_var`
    /// would be a `setenv`/`getenv` data race (UB). The env is only read once at
    /// startup to seed the flag, never mutated at runtime.
    pub fn publish_process_logs(on: bool) {
        umadev_host::process_logs::set_show_process_logs(on);
    }

    /// Publish THIS config's saved process-log preference at startup (see
    /// [`Self::publish_process_logs`]). An env set externally at launch (advanced /
    /// CI) wins and is never overridden here: the host flag seeds itself from that
    /// same env on first read, so realizing that read pins the override. Otherwise
    /// the saved preference is honored only to TURN IT ON (a `false` config leaves
    /// the default off). The live `/logs` toggle still sets the flag explicitly.
    pub fn apply_process_logs(&self) {
        if std::env::var(umadev_host::process_logs::SHOW_PROCESS_LOGS_ENV).is_ok() {
            // One-time startup read: realize the host flag's lazy env-seed so an
            // external override is live, then leave it untouched.
            let _ = umadev_host::process_logs::show_process_logs();
            return;
        }
        if self.show_process_logs {
            Self::publish_process_logs(true);
        }
    }

    /// `claude-code` / `codex` / `offline` (default when unset).
    #[must_use]
    pub fn backend_or_default(&self) -> String {
        self.backend
            .clone()
            .unwrap_or_else(|| "offline".to_string())
    }

    /// `true` when the user prefers free-text (prose) approval questions over the
    /// numbered multiple-choice picker — i.e. `question_form = "text"`. Any other
    /// value (or unset) is the default picker. Case-insensitive.
    #[must_use]
    pub fn prefers_text_questions(&self) -> bool {
        self.question_form
            .as_deref()
            .is_some_and(|v| v.eq_ignore_ascii_case("text"))
    }

    /// Publish THIS config's approval-question presentation preference into the
    /// agent crate's process-global flag (see
    /// [`umadev_agent::set_prefer_text_questions`]), so the base `AskUserQuestion`
    /// notes — emitted deep in the run pumps with no config in hand — honor it.
    /// Call at startup and after a live `/questions` toggle. Deterministic; the
    /// TUI-side gate picker reads [`Self::prefers_text_questions`] directly.
    pub fn apply_question_form(&self) {
        umadev_agent::set_prefer_text_questions(self.prefers_text_questions());
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
    let Ok(body) = read_user_config(path) else {
        return UserConfig::default();
    };
    toml::from_str(&body).unwrap_or_default()
}

/// Startup entry point: strictly load the config, run any PENDING migrations
/// once ([`run_migrations`]), and persist the bumped version back when something
/// changed. A damaged existing file is never rewritten from a fail-soft default;
/// a save error is swallowed so idempotent migrations can retry next launch.
#[must_use]
pub fn load_and_migrate(path: &std::path::Path) -> UserConfig {
    load_and_migrate_for_startup(path).0
}

/// Startup variant that also reports which retired backend was cleared during
/// this launch. The report is deliberately transient: it is never serialized,
/// so the TUI can show one clear migration notice exactly on the upgrade launch.
#[must_use]
pub(crate) fn load_and_migrate_for_startup(path: &std::path::Path) -> (UserConfig, Option<String>) {
    let Ok(mut cfg) = load_strict(path) else {
        return (UserConfig::default(), None);
    };
    // The retired-backend step is migration index 1 (target version 2). Capture
    // the old id only while that step is pending; once v2 is persisted, a later
    // startup cannot repeat the notice.
    let retired_backend = (cfg.migration_version < RETIRED_BACKEND_MIGRATION_VERSION)
        .then(|| cfg.backend.clone())
        .flatten()
        .filter(|id| is_retired_backend(id));
    if run_migrations(&mut cfg) {
        // Best-effort persist of the new version + any normalized fields.
        let _ = save_to(&cfg, path);
    }
    let retired_backend = retired_backend.filter(|_| cfg.backend.is_none());
    (cfg, retired_backend)
}

/// Strictly load the config, surfacing a parse error instead of the fail-soft
/// reset-to-`Default`. `doctor` uses this so a corrupt `config.toml` (which
/// would otherwise silently wipe the user's backend/model/provider on the next
/// launch) is reported, not hidden. A missing file is `Ok(Default)`.
///
/// # Errors
/// Returns the read or TOML-parse error as a string.
pub fn load_strict(path: &std::path::Path) -> Result<UserConfig, String> {
    let body = match read_user_config(path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(UserConfig::default());
        }
        Err(error) => return Err(error.to_string()),
    };
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
    // Resolve the parent first so dotfile-manager symlinked directories keep
    // working, then use the shared crash-safe/no-follow writer on the real
    // target. If the config file itself is a symlink, preserve it by updating
    // its canonical regular-file target rather than replacing the link.
    let target = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::canonicalize(path)?
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = fs::canonicalize(path)?;
            if !target.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "config symlink does not resolve to a regular file",
                ));
            }
            target
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "config output is not a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "config has no parent")
            })?;
            let parent = fs::canonicalize(parent)?;
            parent.join(path.file_name().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidInput, "config has no filename")
            })?)
        }
        Err(error) => return Err(error),
    };
    umadev_state::fs::atomic_write(&target, body.as_bytes())?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    struct RestoreXdg(Option<String>);

    impl Drop for RestoreXdg {
        fn drop(&mut self) {
            match self.0.as_ref() {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

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
            lang: Some("en".into()),
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
    fn oversized_config_is_rejected_without_allocation_growth() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("oversized.toml");
        fs::write(
            &path,
            vec![b'x'; usize::try_from(MAX_USER_CONFIG_BYTES).unwrap() + 1],
        )
        .unwrap();
        assert_eq!(load_from(&path), UserConfig::default());
        assert!(load_strict(&path).unwrap_err().contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_config_is_rejected_without_blocking() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(load_from(&path), UserConfig::default());
        assert!(load_strict(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn config_symlink_is_supported_and_preserved_on_save() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("dotfiles/umadev.toml");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, "backend = \"codex\"\n").unwrap();
        let path = tmp.path().join("config.toml");
        symlink(&target, &path).unwrap();

        assert_eq!(load_from(&path).backend.as_deref(), Some("codex"));
        let config = UserConfig {
            backend: Some("opencode".into()),
            ..Default::default()
        };
        save_to(&config, &path).unwrap();
        assert!(fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(load_from(&path), config);
    }

    #[test]
    fn save_to_ignores_legacy_fixed_temp_name() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        // Occupy the OLD fixed temp path with a directory so a write to it would
        // fail. If save_to still staged through `config.toml.tmp` it would error;
        // the PID-qualified temp (`config.toml.tmp-<pid>`) sidesteps the obstacle.
        let fixed_tmp = path.with_extension("toml.tmp");
        fs::create_dir(&fixed_tmp).unwrap();

        let cfg = UserConfig {
            backend: Some("claude-code".into()),
            ..Default::default()
        };
        // Succeeds despite the occupied fixed temp path → the shared atomic
        // writer uses an unguessable create-new sibling instead.
        save_to(&cfg, &path).expect("atomic temp must avoid the occupied fixed name");
        assert_eq!(load_from(&path), cfg);
        // The fixed-name obstacle is untouched — confirming it was never used.
        assert!(fixed_tmp.is_dir());
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("a/b/c/config.toml");
        let cfg = UserConfig {
            backend: Some("codex".into()),
            ..Default::default()
        };
        save_to(&cfg, &deep).unwrap();
        assert!(deep.is_file());
    }

    #[test]
    fn question_form_defaults_to_picker_and_opts_into_text() {
        // Unset → the default numbered picker (existing users unaffected).
        let picker = UserConfig::default();
        assert!(!picker.prefers_text_questions());
        // `"text"` (case-insensitive) → free-text prose questions.
        let text = UserConfig {
            question_form: Some("text".into()),
            ..Default::default()
        };
        assert!(text.prefers_text_questions());
        let text_caps = UserConfig {
            question_form: Some("TEXT".into()),
            ..Default::default()
        };
        assert!(text_caps.prefers_text_questions());
        // Any other value is the picker (fail-safe to the default).
        let other = UserConfig {
            question_form: Some("picker".into()),
            ..Default::default()
        };
        assert!(!other.prefers_text_questions());
    }

    #[test]
    fn question_form_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = UserConfig {
            backend: Some("claude-code".into()),
            question_form: Some("text".into()),
            ..Default::default()
        };
        save_to(&original, &path).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.question_form.as_deref(), Some("text"));
        assert!(loaded.prefers_text_questions());
    }

    #[test]
    fn backend_or_default_falls_back_to_offline() {
        let cfg = UserConfig::default();
        assert_eq!(cfg.backend_or_default(), "offline");
        let cfg = UserConfig {
            backend: Some("claude-code".into()),
            ..Default::default()
        };
        assert_eq!(cfg.backend_or_default(), "claude-code");
    }

    #[test]
    fn migration_runs_once_bumps_version_and_normalizes() {
        // A pre-versioning config (version 0) with a legacy empty-string backend.
        let mut cfg = UserConfig {
            backend: Some(String::new()),
            lang: Some("en".into()),
            ..Default::default()
        };
        assert_eq!(cfg.migration_version, 0);
        // First run applies every pending step and bumps the version.
        assert!(
            run_migrations(&mut cfg),
            "a pending migration must report change"
        );
        assert_eq!(cfg.migration_version, CURRENT_MIGRATION_VERSION);
        assert_eq!(cfg.backend, None, "empty backend must collapse to None");
        assert_eq!(cfg.lang.as_deref(), Some("en"), "a real value is untouched");
        // Second run is a no-op (idempotent): already at the current version.
        assert!(
            !run_migrations(&mut cfg),
            "an already-current config must not re-run"
        );
        assert_eq!(cfg.migration_version, CURRENT_MIGRATION_VERSION);
    }

    #[test]
    fn retired_backends_and_aliases_migrate_to_picker_without_fallback() {
        assert_eq!(
            CURRENT_MIGRATION_VERSION, 2,
            "retirement is the v2 migration"
        );
        for retired in [
            "cursor",
            "codebuddy",
            "cbc",
            "droid",
            "qwen",
            "qwen-code",
            "  CBC  ",
        ] {
            let mut cfg = UserConfig {
                backend: Some(retired.to_string()),
                lang: Some("en".into()),
                design_system: Some("kept".into()),
                migration_version: 1,
                ..Default::default()
            };

            assert!(
                run_migrations(&mut cfg),
                "{retired} has a pending migration"
            );
            assert_eq!(cfg.backend, None, "{retired} must reopen the picker");
            assert_ne!(cfg.backend.as_deref(), Some("offline"));
            assert_eq!(cfg.lang.as_deref(), Some("en"));
            assert_eq!(cfg.design_system.as_deref(), Some("kept"));
            assert_eq!(cfg.migration_version, CURRENT_MIGRATION_VERSION);
            assert!(
                !run_migrations(&mut cfg),
                "{retired} migration is idempotent"
            );
            assert_eq!(cfg.backend, None);
        }
    }

    #[test]
    fn four_supported_backends_are_not_changed_by_retirement_migration() {
        for supported in crate::FIRST_CLASS_BACKEND_IDS {
            let mut cfg = UserConfig {
                backend: Some(supported.to_string()),
                migration_version: 1,
                ..Default::default()
            };
            assert!(run_migrations(&mut cfg));
            assert_eq!(cfg.backend.as_deref(), Some(supported));
        }
    }

    #[test]
    fn retired_backend_notice_is_reported_on_exactly_one_startup() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        save_to(
            &UserConfig {
                backend: Some("qwen-code".into()),
                lang: Some("zh-CN".into()),
                migration_version: 1,
                ..Default::default()
            },
            &path,
        )
        .unwrap();

        let (first, first_notice) = load_and_migrate_for_startup(&path);
        assert_eq!(first.backend, None);
        assert_eq!(first_notice.as_deref(), Some("qwen-code"));
        assert_eq!(first.migration_version, CURRENT_MIGRATION_VERSION);

        let (second, second_notice) = load_and_migrate_for_startup(&path);
        assert_eq!(second.backend, None);
        assert_eq!(second.lang.as_deref(), Some("zh-CN"));
        assert_eq!(
            second_notice, None,
            "persisted v2 must not repeat the notice"
        );
    }

    #[test]
    fn migration_never_downgrades_a_newer_config() {
        // A config from a hypothetical newer build (version far ahead) is left
        // untouched — never re-run, never reset backward.
        let mut cfg = UserConfig {
            migration_version: CURRENT_MIGRATION_VERSION + 9,
            ..Default::default()
        };
        assert!(!run_migrations(&mut cfg));
        assert_eq!(cfg.migration_version, CURRENT_MIGRATION_VERSION + 9);
    }

    #[test]
    fn migration_runner_is_fail_soft_on_a_panicking_step() {
        fn boom(_: &mut UserConfig) {
            panic!("simulated bad migration");
        }
        fn set_lang(cfg: &mut UserConfig) {
            cfg.lang = Some("recovered".into());
        }
        let migrations: &[Migration] = &[boom, set_lang];

        // Silence the default panic hook so the deliberate panic doesn't spam the
        // test output; restore it right after.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut cfg = UserConfig::default();
        let changed = run_migrations_with(&mut cfg, migrations);
        std::panic::set_hook(prev);

        assert!(
            changed,
            "the runner still reports progress despite a bad step"
        );
        // The version advanced past BOTH steps — a bad migration can't wedge the
        // launch — and the later, good step still ran.
        assert_eq!(cfg.migration_version, 2);
        assert_eq!(cfg.lang.as_deref(), Some("recovered"));
    }

    #[test]
    fn load_and_migrate_persists_the_bumped_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        // A legacy config file with NO migration_version field at all.
        fs::write(&path, "backend = \"claude-code\"\n").unwrap();
        let cfg = load_and_migrate(&path);
        assert_eq!(cfg.migration_version, CURRENT_MIGRATION_VERSION);
        // The bumped version was persisted: a fresh load sees it (so the next
        // launch is a no-op rather than re-running every migration).
        let reloaded = load_from(&path);
        assert_eq!(reloaded.migration_version, CURRENT_MIGRATION_VERSION);
        assert_eq!(reloaded.backend.as_deref(), Some("claude-code"));
    }

    #[test]
    fn load_and_migrate_never_overwrites_a_corrupt_config() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("config.toml");
        let original = b"backend = [broken";
        fs::write(&path, original).unwrap();

        assert_eq!(load_and_migrate(&path), UserConfig::default());
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn default_path_honours_xdg_config_home() {
        let _guard = CONFIG_ENV_LOCK.lock().unwrap();
        let prev = std::env::var("XDG_CONFIG_HOME").ok();
        let _restore = RestoreXdg(prev);
        let tmp = TempDir::new().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());
        let p = default_path();
        assert!(p.starts_with(tmp.path().join("umadev")));
        assert!(p.ends_with(FILE_NAME));
    }
}
