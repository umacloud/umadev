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
    /// Applied via the runner's thread-safe shared tier state (see
    /// [`Self::apply_model_tiers`]), so the worker path needs no extra plumbing.
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

    /// Show the base's long-running command (build / install / `spring-boot:run`)
    /// output in the transcript as it runs, instead of the tight 200-char clip on
    /// completion. Off by default. Toggled live via `/logs`; published into the
    /// host drivers' thread-safe shared flag (see [`Self::publish_process_logs`]).
    /// Only serialized when `true` so a default config stays clean.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub show_process_logs: bool,

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
];

/// The version a freshly-migrated config carries — exactly the number of
/// migrations. A config already at this version is left untouched by the runner.
// The migration list is statically tiny; the count can never overflow a u32.
#[allow(clippy::cast_possible_truncation)]
pub const CURRENT_MIGRATION_VERSION: u32 = MIGRATIONS.len() as u32;

/// v0 -> v1: collapse empty-string optionals to `None` (see [`MIGRATIONS`]).
fn migrate_empty_strings_to_none(cfg: &mut UserConfig) {
    for field in [
        &mut cfg.backend,
        &mut cfg.model,
        &mut cfg.model_plan,
        &mut cfg.model_build,
        &mut cfg.design_system,
        &mut cfg.seed_template,
        &mut cfg.lang,
    ] {
        if field.as_deref().is_some_and(str::is_empty) {
            *field = None;
        }
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

    /// Publish the per-phase model tiers ([`Self::model_plan`] /
    /// [`Self::model_build`]) into the runner's **thread-safe shared tier state**
    /// (`umadev_agent::runner::set_model_tiers`). Call once at startup and after
    /// any `/model plan|build` change so the in-process worker loop picks them up.
    /// An unset tier clears that tier so it cleanly falls back to the single model.
    ///
    /// Uses shared state, NOT the process env: the runner reads the tiers from
    /// background tasks while a turn streams, so a runtime `set_var` would be a
    /// `setenv`/`getenv` data race (UB). The env is only read once at startup to
    /// seed the shared state, never mutated at runtime.
    pub fn apply_model_tiers(&self) {
        umadev_agent::runner::set_model_tiers(
            self.model_plan.as_deref(),
            self.model_build.as_deref(),
        );
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

/// Startup entry point: [`load_from`] the config, run any PENDING migrations
/// once ([`run_migrations`]), and persist the bumped version back when something
/// changed. Fail-open: a save error is swallowed (the idempotent migrations just
/// re-run next launch), so config drift across npm releases is repaired exactly
/// once per upgrade without ever risking a broken startup.
#[must_use]
pub fn load_and_migrate(path: &std::path::Path) -> UserConfig {
    let mut cfg = load_from(path);
    if run_migrations(&mut cfg) {
        // Best-effort persist of the new version + any normalized fields.
        let _ = save_to(&cfg, path);
    }
    cfg
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
    //
    // PID-qualify the temp name (`config.toml.tmp-<pid>`): the GLOBAL
    // `~/.umadev/config.toml` is shared across every umadev process, so a FIXED
    // temp path lets two processes saving config at once write the same temp file
    // and clobber each other's partial bytes before the rename. A per-PID temp
    // gives each process its own staging file (matching `persist_chat`'s
    // `{id}.json.tmp-{pid}`); the rename onto the shared target stays atomic.
    let tmp = path.with_extension(format!("toml.tmp-{}", std::process::id()));
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
    fn save_to_uses_pid_qualified_temp_name() {
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
        // Succeeds despite the occupied fixed temp path → the temp name is
        // PID-qualified, not the fixed `config.toml.tmp`.
        save_to(&cfg, &path).expect("PID-qualified temp must avoid the occupied fixed name");
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
    fn migration_runs_once_bumps_version_and_normalizes() {
        // A pre-versioning config (version 0) with a legacy empty-string backend.
        let mut cfg = UserConfig {
            backend: Some(String::new()),
            lang: Some("en".into()),
            ..Default::default()
        };
        assert_eq!(cfg.migration_version, 0);
        // First run applies the v0->v1 normalization + bumps the version.
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
        fn set_model(cfg: &mut UserConfig) {
            cfg.model = Some("recovered".into());
        }
        let migrations: &[Migration] = &[boom, set_model];

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
        assert_eq!(cfg.model.as_deref(), Some("recovered"));
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
