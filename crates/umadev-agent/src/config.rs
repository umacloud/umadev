//! Project-level configuration overrides via `.umadevrc`.
//!
//! Users can create a `.umadevrc` file (TOML) in the project root to
//! customize UmaDev behavior per-project without modifying the global
//! config.
//!
//! ```toml
//! # .umadevrc
//! [quality]
//! threshold = 85              # override quality gate pass threshold
//! skip_checks = ["dark_mode"] # skip specific quality checks
//!
//! [pipeline]
//! skip_phases = ["research"]  # skip research if you already did it
//! max_review_rounds = 2       # limit review→fix cycles
//!
//! [experts]
//! custom_knowledge = "team-standards/" # extra knowledge directory
//!
//! [knowledge]
//! enabled = true                       # use structured retrieval (default)
//! engine = "hybrid"                    # request BM25+vector; degrades to BM25
//! top_k = 6                            # chunks injected per phase
//!
//! [model]
//! # DEPRECATED / UNUSED — UmaDev does NOT route models. UmaDev owns no model
//! # endpoint; the selected one of five base CLIs (3 native + 2 vendor-isolated ACP) owns the
//! # login/config and decides which model runs. A non-empty `provider` here is
//! # IGNORED, and the run prints a one-time "ignored" warning so it fails loud
//! # rather than silently doing nothing. To use a different model, configure it
//! # in your base CLI, not here. Kept only so an older `.umadevrc` still parses.
//! provider = ""                        # ignored — configure the model in the base CLI
//!
//! [codex]
//! sandbox_mode = "danger-full-access"  # main coding session default: full
//!                                       # filesystem / process / network /
//!                                       # local-port access. Set read-only or
//!                                       # workspace-write to restrict Codex.
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Project-level overrides from `.umadevrc`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProjectConfig {
    /// Quality gate overrides.
    #[serde(default)]
    pub quality: QualityConfig,
    /// Pipeline behavior overrides.
    #[serde(default)]
    pub pipeline: PipelineConfig,
    /// Expert knowledge overrides.
    #[serde(default)]
    pub experts: ExpertsConfig,
    /// Knowledge-base / RAG retrieval overrides.
    #[serde(default)]
    pub knowledge: KnowledgeConfig,
    /// Deprecated model-provider field retained only for backward-compatible
    /// parsing; the run path ignores it (see [`ModelConfig`]).
    #[serde(default)]
    pub model: ModelConfig,
    /// Codex base launch-sandbox overrides.
    #[serde(default)]
    pub codex: CodexConfig,
}

/// Quality gate customization.
#[derive(Debug, Clone, Deserialize)]
pub struct QualityConfig {
    /// Minimum score to pass (default 90).
    #[serde(default = "default_threshold")]
    pub threshold: u32,
    /// Check names to skip (e.g. `dark_mode`).
    #[serde(default)]
    pub skip_checks: Vec<String>,
}

impl Default for QualityConfig {
    fn default() -> Self {
        Self {
            threshold: default_threshold(),
            skip_checks: Vec::new(),
        }
    }
}

fn default_threshold() -> u32 {
    90
}

/// Pipeline behavior customization.
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfig {
    /// Phases to skip (e.g. `research` if you already did it).
    #[serde(default)]
    pub skip_phases: Vec<String>,
    /// Max review→fix rounds per document (default 3).
    #[serde(default = "default_review_rounds")]
    pub max_review_rounds: usize,
    /// Strict spec-coverage enforcement (default `false`). When `true`, the
    /// spec phase's FR→task coverage check BLOCKS the pipeline (pausing at
    /// `spec`) if any PRD functional requirement has no covering task, instead
    /// of merely emitting an advisory note. Default `false` keeps the existing
    /// warn-only behaviour so a partial breakdown never silently halts a run.
    /// Overridable per run by the `UMADEV_STRICT_COVERAGE=1` environment flag.
    #[serde(default)]
    pub strict_coverage: bool,
    /// Auto-approve the pipeline's ordinary document/preview gates without
    /// waiting for input (default `true`). The gates (`docs_confirm`,
    /// `preview_confirm`) still appear as checkpoints in the event stream and
    /// status bar. This setting does not bypass irreversible-action
    /// confirmations, deterministic acceptance, or the trust-mode safety
    /// floor. Set it to `false` to require manual gate approval.
    #[serde(default = "default_auto_approve")]
    pub auto_approve_gates: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            skip_phases: Vec::new(),
            max_review_rounds: default_review_rounds(),
            strict_coverage: false,
            auto_approve_gates: default_auto_approve(),
        }
    }
}

fn default_auto_approve() -> bool {
    true
}

fn default_review_rounds() -> usize {
    3
}

/// Expert knowledge customization.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ExpertsConfig {
    /// Additional knowledge directory (relative to project root).
    /// Files in this dir are injected alongside built-in expert knowledge.
    #[serde(default)]
    pub custom_knowledge: Option<String>,
}

/// **DEPRECATED / UNUSED** custom-model (API provider) override.
///
/// UmaDev deliberately owns NO model endpoint and does NOT route models: the
/// selected one of five base CLIs (three native plus Grok Build/Kimi Code ACP) owns the
/// login/config and decides which model runs. This section is therefore **not
/// consumed by the run path** — the run always drives the base on the base's own
/// model. It
/// is retained ONLY so an older `.umadevrc` that sets `[model] provider` still
/// parses; a non-empty value is IGNORED, and the run surfaces a one-time
/// "ignored" warning (see [`ModelConfig::ignored_provider`]) so a mis-set
/// provider fails LOUD rather than silently doing nothing. To change the model,
/// configure it in the base CLI, not here.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelConfig {
    /// **Unused / deprecated.** The named provider a project once pinned. UmaDev
    /// does not route models, so this is never consumed — the base CLI decides.
    /// `Some(non-empty)` triggers the one-time "ignored" run warning; `Some("")`
    /// / `None` (field absent) stay silent (the common case). Kept for
    /// backward-compatible parsing only.
    #[serde(default)]
    pub provider: Option<String>,
}

impl ModelConfig {
    /// The configured provider name IF the user set a non-empty one — a value
    /// UmaDev deliberately **IGNORES** (it owns no model endpoint; the base CLI's
    /// own login/config is authoritative). `None` when unset or explicitly empty
    /// (incl. whitespace-only), so the common case stays silent. The run path
    /// surfaces a one-time "ignored" warning when this is `Some`, so a mis-set
    /// provider fails LOUD instead of silently doing nothing.
    #[must_use]
    pub fn ignored_provider(&self) -> Option<&str> {
        self.provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
    }
}

/// Codex base launch-sandbox customization (`.umadevrc` `[codex]` section).
///
/// Codex launches its workspace in one of three sandbox tiers. UmaDev is a
/// development-team host, so its main execution session defaults to
/// `danger-full-access`: package managers, local dev servers, git, subprocesses,
/// and network calls must reach the real development environment. Users can
/// explicitly downgrade a project to `workspace-write` or `read-only`; the
/// independent critic session is always read-only regardless of this setting.
#[derive(Debug, Clone, Deserialize)]
pub struct CodexConfig {
    /// One of `read-only` / `workspace-write` / `danger-full-access` (default).
    /// An explicitly invalid value resolves conservatively to `workspace-write`,
    /// so a typo in a restriction never silently widens access.
    #[serde(default = "default_codex_sandbox")]
    pub sandbox_mode: String,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            sandbox_mode: default_codex_sandbox(),
        }
    }
}

impl CodexConfig {
    /// Resolve the configured string to the typed [`CodexSandbox`]. Missing
    /// config is already populated with [`CodexSandbox::DangerFullAccess`]; an
    /// explicitly unrecognised value restricts to [`CodexSandbox::WorkspaceWrite`].
    #[must_use]
    pub fn resolved_sandbox(&self) -> CodexSandbox {
        CodexSandbox::parse_fail_open(&self.sandbox_mode)
    }
}

fn default_codex_sandbox() -> String {
    CodexSandbox::DangerFullAccess.as_codex_arg().to_string()
}

/// The three Codex launch sandbox tiers, matching codex's own `--sandbox` enum
/// (and the JSON-RPC `sandbox` param).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexSandbox {
    /// Read-only: the base can read the workspace but write nothing.
    ReadOnly,
    /// Write the workspace only, without general host/network access.
    WorkspaceWrite,
    /// No sandbox: full filesystem + network + process access. UmaDev's main
    /// execution default; required for normal full-stack development work.
    #[default]
    DangerFullAccess,
}

impl CodexSandbox {
    /// Parse a config / env string fail-open. Recognises codex's canonical
    /// kebab-case ids (and is lenient about case / `_`↔`-`); anything else —
    /// including an empty or garbage explicit value — resolves to
    /// [`Self::WorkspaceWrite`] so a typo in a restriction never silently widens
    /// access. The absent-value default is supplied separately by
    /// the default sandbox resolver as [`Self::DangerFullAccess`].
    #[must_use]
    pub fn parse_fail_open(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "read-only" | "readonly" => Self::ReadOnly,
            "danger-full-access" | "danger-full" | "full-access" => Self::DangerFullAccess,
            // "workspace-write" + every unrecognised explicit value → the
            // conservative restricted tier.
            _ => Self::WorkspaceWrite,
        }
    }

    /// The canonical codex `--sandbox` / JSON-RPC `sandbox` value.
    #[must_use]
    pub fn as_codex_arg(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// `true` for the high-risk full-access tier — drives the loud startup
    /// warning and the forced non-interactive approval pairing.
    #[must_use]
    pub fn is_high_risk(self) -> bool {
        matches!(self, Self::DangerFullAccess)
    }
}

/// Knowledge-base (RAG) retrieval customization.
///
/// Controls the [`umadev_knowledge`] retrieval engine that replaces the
/// legacy "keyword-sort the folder" approach. `hybrid` is the configured
/// default, but it is a request rather than proof that a vector channel ran.
/// Local vectors require a binary built with `vector-local` plus a compatible,
/// verified model on disk (official release launchers provision that model;
/// plain source builds do not). Remote vectors require both the dedicated key
/// and the explicit upload opt-in documented by `umadev-knowledge`. If neither
/// vector backend is usable, retrieval degrades to BM25.
#[derive(Debug, Clone, Deserialize)]
pub struct KnowledgeConfig {
    /// Whether all knowledge consumption is enabled (default `true`). `false`
    /// short-circuits lexical retrieval, vectors, agentic/phase digests,
    /// previews, and expert knowledge; no legacy fallback may recall content.
    #[serde(default = "default_knowledge_enabled")]
    pub enabled: bool,
    /// Retrieval engine: `"hybrid"` (default) or lexical-only `"bm25"`.
    #[serde(default = "default_knowledge_engine")]
    pub engine: String,
    /// How many knowledge chunks to inject per phase (default 6).
    #[serde(default = "default_knowledge_top_k")]
    pub top_k: usize,
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        Self {
            enabled: default_knowledge_enabled(),
            engine: default_knowledge_engine(),
            top_k: default_knowledge_top_k(),
        }
    }
}

fn default_knowledge_enabled() -> bool {
    true
}

fn default_knowledge_engine() -> String {
    // `hybrid` requests BM25 + vector RRF fusion. It degrades to pure BM25 when
    // no enabled embedding backend can produce a usable vector. Set
    // `engine = "bm25"` to force lexical-only retrieval.
    "hybrid".to_string()
}

fn default_knowledge_top_k() -> usize {
    6
}

const PROJECT_CONFIG_FILE: &str = ".umadevrc";
const MAX_PROJECT_CONFIG_BYTES: u64 = 1024 * 1024;

fn managed_project_config_path(project_root: &Path) -> std::io::Result<PathBuf> {
    let root = std::fs::canonicalize(project_root)?;
    if !umadev_state::fs::real_dir(&root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "project root is not a real directory",
        ));
    }
    Ok(root.join(PROJECT_CONFIG_FILE))
}

fn read_project_config(project_root: &Path) -> std::io::Result<String> {
    let path = managed_project_config_path(project_root)?;
    let root = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed config has no project directory",
        )
    })?;
    let bytes = umadev_state::fs::read_bounded_beneath(
        root,
        Path::new(PROJECT_CONFIG_FILE),
        MAX_PROJECT_CONFIG_BYTES,
    )?;
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Read `.umadevrc` from the project root. Returns `Default` if missing
/// or malformed (fail-soft, same as UserConfig).
#[must_use]
pub fn load_project_config(project_root: &Path) -> ProjectConfig {
    let Ok(body) = read_project_config(project_root) else {
        return ProjectConfig::default();
    };
    let mut cfg: ProjectConfig = toml::from_str(&body).unwrap_or_default();
    // Validate the knowledge engine: only "bm25" and "hybrid" are legal.
    // Unknown values (e.g. "quantum") silently fall back to "bm25" so a
    // typo never breaks retrieval.
    if cfg.knowledge.engine != "bm25" && cfg.knowledge.engine != "hybrid" {
        if !cfg.knowledge.engine.is_empty() {
            tracing::warn!(
                "knowledge.engine = {:?} is not bm25 or hybrid — falling back to bm25",
                cfg.knowledge.engine
            );
        }
        cfg.knowledge.engine = "bm25".to_string();
    }
    // Clamp quality threshold and top_k to sensible bounds.
    cfg.quality.threshold = cfg.quality.threshold.min(100);
    cfg.knowledge.top_k = cfg.knowledge.top_k.clamp(1, 50);
    // Normalise the codex sandbox to a canonical kebab id; an unrecognised
    // explicitly invalid value falls back to the restricted `workspace-write`
    // tier (never widens access). Missing config already carries the full-access
    // product default from `CodexConfig::default`.
    let raw_sandbox = cfg.codex.sandbox_mode.clone();
    let resolved_sandbox = CodexSandbox::parse_fail_open(&raw_sandbox);
    let normalized_sandbox = raw_sandbox.trim().to_ascii_lowercase().replace('_', "-");
    if resolved_sandbox == CodexSandbox::WorkspaceWrite
        && !normalized_sandbox.is_empty()
        && normalized_sandbox != "workspace-write"
    {
        tracing::warn!(
            "codex.sandbox_mode = {:?} is not read-only/workspace-write/danger-full-access \
             — restricting to workspace-write",
            raw_sandbox
        );
    }
    cfg.codex.sandbox_mode = resolved_sandbox.as_codex_arg().to_string();
    cfg
}

const LEGACY_GENERATED_CODEX_BLOCK: &str = "# Codex launch sandbox: read-only | workspace-write (default, safe) | danger-full-access.\n# The default blocks local dev servers (npm start for React/Electron) and git commits;\n# set danger-full-access to allow them (high-risk -- you accept the system-environment risk).\nsandbox_mode = \"workspace-write\"";

/// Upgrade the exact `[codex]` block generated by UmaDev 1.0.52–1.0.55.
/// Hand-written `workspace-write` settings are left untouched.
pub fn migrate_legacy_generated_codex_sandbox(project_root: &Path) -> std::io::Result<bool> {
    let body = match read_project_config(project_root) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !body.contains(LEGACY_GENERATED_CODEX_BLOCK) || body.contains("sandbox_explicit = true") {
        return Ok(false);
    }
    persist_codex_sandbox_inner(project_root, CodexSandbox::DangerFullAccess, false)?;
    tracing::warn!(
        "migrated UmaDev's legacy generated Codex sandbox default to danger-full-access"
    );
    Ok(true)
}

/// Persist the chosen Codex launch sandbox tier into the project's `.umadevrc`
/// `[codex] sandbox_mode`, **merging** into any existing file so the user's
/// other keys, comments, and formatting are preserved (via `toml_edit`). Lets
/// the in-app `/sandbox <mode>` command save the change without the user
/// hand-editing `.umadevrc` (or hacking `UMADEV_CODEX_SANDBOX` into a shell rc).
///
/// Always writes the canonical kebab id ([`CodexSandbox::as_codex_arg`]) so a
/// later [`load_project_config`] reads back exactly the same tier. The write is
/// atomic (temp file in the same dir + rename) so a crash mid-write can never
/// truncate the config into something the TOML parser would later choke on.
///
/// **Fail-open by contract:** returns the I/O / parse error to the caller (the
/// TUI still sets the session env + tells the user the persist failed) — it
/// never panics, never blocks, and never widens the sandbox on failure. If
/// `.umadevrc` exists but is not valid TOML it is *refused* (returns an error)
/// rather than clobbered, so a transient parse bug can't wipe the user's config.
///
/// # Errors
/// Propagates a filesystem write error, or an `InvalidData` error if an existing
/// `.umadevrc` cannot be parsed as TOML (refusing to overwrite it).
pub fn persist_codex_sandbox(project_root: &Path, mode: CodexSandbox) -> std::io::Result<()> {
    persist_codex_sandbox_inner(project_root, mode, true)
}

fn persist_codex_sandbox_inner(
    project_root: &Path,
    mode: CodexSandbox,
    explicit: bool,
) -> std::io::Result<()> {
    use toml_edit::{value, DocumentMut, Item, Table};

    let path = managed_project_config_path(project_root)?;
    let mut doc = match read_project_config(project_root) {
        // Empty / whitespace-only file → start a fresh document.
        Ok(text) if text.trim().is_empty() => DocumentMut::new(),
        // Existing config → parse-merge, preserving comments + sibling keys. A
        // file that exists but isn't valid TOML is REFUSED (never clobbered).
        Ok(text) => text.parse::<DocumentMut>().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(".umadevrc exists but isn't valid TOML ({e}); refusing to overwrite it"),
            )
        })?,
        // No file yet → a fresh document. Unsafe/unreadable existing entries
        // are refused so a failed read can never become a destructive rewrite.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(error) => return Err(error),
    };

    // Ensure a `[codex]` table exists, then set just `sandbox_mode`.
    if !doc.get("codex").is_some_and(Item::is_table) {
        doc["codex"] = Item::Table(Table::new());
    }
    doc["codex"]["sandbox_mode"] = value(mode.as_codex_arg());
    doc["codex"]["sandbox_explicit"] = value(explicit);

    let body = doc.to_string();
    // Shared crash-safe writer uses a unique private sibling and refuses link,
    // directory, and special-file targets. Never fall back to a direct truncate:
    // doing so would break the atomicity promised by this API on rename failure.
    umadev_state::fs::atomic_write(&path, body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn invalid_engine_falls_back_to_bm25() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[knowledge]\nengine = \"quantum\"\ntop_k = 999\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.knowledge.engine, "bm25", "quantum must fall back");
        assert_eq!(cfg.knowledge.top_k, 50, "top_k must clamp to 50");
    }

    #[test]
    fn valid_hybrid_engine_preserved() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[knowledge]\nengine = \"hybrid\"\ntop_k = 8\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.knowledge.engine, "hybrid");
        assert_eq!(cfg.knowledge.top_k, 8);
    }

    #[test]
    fn threshold_clamped_to_100() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".umadevrc"), "[quality]\nthreshold = 999\n").unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.quality.threshold, 100);
    }

    #[test]
    fn codex_sandbox_unset_defaults_to_full_access() {
        let tmp = TempDir::new().unwrap();
        // No `.umadevrc` at all → a main coding session can use the complete
        // development environment.
        let cfg = load_project_config(tmp.path());
        assert_eq!(
            cfg.codex.resolved_sandbox(),
            CodexSandbox::DangerFullAccess,
            "missing config must use the full-access execution default"
        );
        // A `.umadevrc` that omits the [codex] section → still the default.
        std::fs::write(tmp.path().join(".umadevrc"), "[quality]\nthreshold = 80\n").unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.codex.resolved_sandbox(), CodexSandbox::DangerFullAccess);
        assert_eq!(cfg.codex.sandbox_mode, "danger-full-access");
    }

    #[test]
    fn codex_sandbox_danger_full_access_parses() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[codex]\nsandbox_mode = \"danger-full-access\"\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.codex.resolved_sandbox(), CodexSandbox::DangerFullAccess);
        assert!(cfg.codex.resolved_sandbox().is_high_risk());
        assert_eq!(cfg.codex.sandbox_mode, "danger-full-access");
    }

    #[test]
    fn codex_sandbox_read_only_parses() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[codex]\nsandbox_mode = \"read-only\"\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.codex.resolved_sandbox(), CodexSandbox::ReadOnly);
        assert!(!cfg.codex.resolved_sandbox().is_high_risk());
    }

    #[test]
    fn codex_sandbox_garbage_falls_back_to_workspace_write() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[codex]\nsandbox_mode = \"yolo-root\"\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(
            cfg.codex.resolved_sandbox(),
            CodexSandbox::WorkspaceWrite,
            "a garbage explicit value must restrict access, never error"
        );
        // The stored string is normalised so downstream consumers see the canonical id.
        assert_eq!(cfg.codex.sandbox_mode, "workspace-write");
    }

    #[test]
    fn codex_sandbox_parse_fail_open_is_lenient() {
        assert_eq!(
            CodexSandbox::parse_fail_open("DANGER_FULL_ACCESS"),
            CodexSandbox::DangerFullAccess
        );
        assert_eq!(
            CodexSandbox::parse_fail_open("  Read-Only  "),
            CodexSandbox::ReadOnly
        );
        assert_eq!(
            CodexSandbox::parse_fail_open(""),
            CodexSandbox::WorkspaceWrite
        );
        // Round-trip: every tier maps to codex's canonical kebab arg.
        assert_eq!(CodexSandbox::ReadOnly.as_codex_arg(), "read-only");
        assert_eq!(
            CodexSandbox::WorkspaceWrite.as_codex_arg(),
            "workspace-write"
        );
        assert_eq!(
            CodexSandbox::DangerFullAccess.as_codex_arg(),
            "danger-full-access"
        );
    }

    #[test]
    fn default_config_has_sane_values() {
        let cfg = ProjectConfig::default();
        assert_eq!(cfg.quality.threshold, 90);
        assert_eq!(cfg.pipeline.max_review_rounds, 3);
        assert!(cfg.pipeline.skip_phases.is_empty());
        // Knowledge defaults: enabled, hybrid engine (degrades to BM25 with no
        // embedding key), top_k 6.
        assert!(cfg.knowledge.enabled);
        assert_eq!(cfg.knowledge.engine, "hybrid");
        assert_eq!(cfg.knowledge.top_k, 6);
        assert_eq!(cfg.codex.resolved_sandbox(), CodexSandbox::DangerFullAccess);
    }

    #[test]
    fn load_from_missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.quality.threshold, 90);
    }

    #[test]
    fn load_parses_toml() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[quality]\nthreshold = 80\nskip_checks = [\"dark_mode\"]\n\n[pipeline]\nmax_review_rounds = 2\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.quality.threshold, 80);
        assert_eq!(cfg.quality.skip_checks, vec!["dark_mode"]);
        assert_eq!(cfg.pipeline.max_review_rounds, 2);
    }

    #[test]
    fn knowledge_section_parses() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[knowledge]\nengine = \"hybrid\"\ntop_k = 10\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.knowledge.engine, "hybrid");
        assert_eq!(cfg.knowledge.top_k, 10);
        assert!(cfg.knowledge.enabled); // defaults to true when omitted
    }

    #[test]
    fn knowledge_disabled_parses() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[knowledge]\nenabled = false\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert!(!cfg.knowledge.enabled);
    }

    #[test]
    fn model_section_parses() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[model]\nprovider = \"deepseek\"\n",
        )
        .unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.model.provider.as_deref(), Some("deepseek"));
    }

    #[test]
    fn model_section_disabled_via_empty_string() {
        let tmp = TempDir::new().unwrap();
        // Empty string = "explicitly use no provider for this project".
        std::fs::write(tmp.path().join(".umadevrc"), "[model]\nprovider = \"\"\n").unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.model.provider.as_deref(), Some(""));
    }

    #[test]
    fn model_section_absent_is_none() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".umadevrc"), "[quality]\nthreshold = 80\n").unwrap();
        let cfg = load_project_config(tmp.path());
        assert!(cfg.model.provider.is_none());
    }

    #[test]
    fn model_provider_non_empty_is_reported_ignored() {
        // A user who set `[model] provider` gets it surfaced as IGNORED so the
        // run can warn loudly (UmaDev does not route models — the base decides).
        let cfg = ModelConfig {
            provider: Some("deepseek".to_string()),
        };
        assert_eq!(cfg.ignored_provider(), Some("deepseek"));
    }

    #[test]
    fn model_provider_empty_or_absent_stays_silent() {
        // Empty / whitespace-only / absent are the common cases and MUST stay
        // silent (no spurious warning).
        assert_eq!(
            ModelConfig {
                provider: Some(String::new())
            }
            .ignored_provider(),
            None
        );
        assert_eq!(
            ModelConfig {
                provider: Some("   ".to_string())
            }
            .ignored_provider(),
            None
        );
        assert_eq!(ModelConfig::default().ignored_provider(), None);
    }

    #[test]
    fn persist_codex_sandbox_creates_file_when_missing() {
        let tmp = TempDir::new().unwrap();
        // No `.umadevrc` yet — persisting must create one with the [codex] table.
        persist_codex_sandbox(tmp.path(), CodexSandbox::DangerFullAccess).unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.codex.resolved_sandbox(), CodexSandbox::DangerFullAccess);
        let body = std::fs::read_to_string(tmp.path().join(".umadevrc")).unwrap();
        assert!(body.contains("[codex]"));
        assert!(body.contains("danger-full-access"));
    }

    #[test]
    fn persist_codex_sandbox_merges_and_preserves_siblings() {
        let tmp = TempDir::new().unwrap();
        // An existing config with an unrelated section + a comment. The persist
        // must keep both and only touch [codex] sandbox_mode.
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "# my notes\n[pipeline]\nauto_approve_gates = false\n",
        )
        .unwrap();
        persist_codex_sandbox(tmp.path(), CodexSandbox::DangerFullAccess).unwrap();
        let body = std::fs::read_to_string(tmp.path().join(".umadevrc")).unwrap();
        assert!(body.contains("# my notes"), "comment preserved");
        assert!(
            body.contains("auto_approve_gates = false"),
            "sibling preserved"
        );
        assert!(body.contains("danger-full-access"));
        // And it round-trips through the loader as the chosen tier.
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.codex.resolved_sandbox(), CodexSandbox::DangerFullAccess);
        assert!(!cfg.pipeline.auto_approve_gates);
    }

    #[test]
    fn persist_codex_sandbox_overwrites_existing_codex_value() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(".umadevrc"),
            "[codex]\nsandbox_mode = \"danger-full-access\"\n",
        )
        .unwrap();
        // Explicitly restrict it to the workspace.
        persist_codex_sandbox(tmp.path(), CodexSandbox::WorkspaceWrite).unwrap();
        let cfg = load_project_config(tmp.path());
        assert_eq!(cfg.codex.resolved_sandbox(), CodexSandbox::WorkspaceWrite);
        let body = std::fs::read_to_string(tmp.path().join(".umadevrc")).unwrap();
        assert!(!body.contains("danger-full-access"));
        assert!(body.contains("workspace-write"));
    }

    #[test]
    fn persist_codex_sandbox_refuses_unparseable_config() {
        let tmp = TempDir::new().unwrap();
        // A garbage (non-TOML) `.umadevrc` must be REFUSED, not clobbered, so a
        // transient bug can't wipe the user's file (fail-open: error, not panic).
        let garbage = "this is not [ valid toml = = =";
        std::fs::write(tmp.path().join(".umadevrc"), garbage).unwrap();
        let err = persist_codex_sandbox(tmp.path(), CodexSandbox::ReadOnly).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        // The original file is untouched.
        let body = std::fs::read_to_string(tmp.path().join(".umadevrc")).unwrap();
        assert_eq!(body, garbage);
    }

    #[test]
    fn oversized_project_config_is_not_loaded_or_rewritten() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join(PROJECT_CONFIG_FILE);
        let mut body = "[quality]\nthreshold = 1\n#".to_string();
        body.push_str(&"x".repeat(usize::try_from(MAX_PROJECT_CONFIG_BYTES).unwrap()));
        std::fs::write(&path, &body).unwrap();

        assert_eq!(
            load_project_config(tmp.path()).quality.threshold,
            default_threshold(),
            "an unbounded config must fail soft instead of being allocated and parsed"
        );
        let error = persist_codex_sandbox(tmp.path(), CodexSandbox::ReadOnly).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read_to_string(path).unwrap(), body);
    }

    #[cfg(unix)]
    #[test]
    fn linked_project_config_is_not_followed_or_rewritten() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let target = external.path().join("outside.toml");
        let original = b"[codex]\nsandbox_mode = \"read-only\"\n";
        std::fs::write(&target, original).unwrap();
        symlink(&target, tmp.path().join(PROJECT_CONFIG_FILE)).unwrap();

        assert_eq!(
            load_project_config(tmp.path()).codex.resolved_sandbox(),
            CodexSandbox::DangerFullAccess,
            "an external link must not supply project policy"
        );
        let error = persist_codex_sandbox(tmp.path(), CodexSandbox::WorkspaceWrite).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read(&target).unwrap(), original);
    }

    #[test]
    fn migrates_only_the_legacy_generated_workspace_write_default() {
        let generated = TempDir::new().unwrap();
        std::fs::write(
            generated.path().join(".umadevrc"),
            format!("[codex]\n{LEGACY_GENERATED_CODEX_BLOCK}\n"),
        )
        .unwrap();
        assert!(migrate_legacy_generated_codex_sandbox(generated.path()).unwrap());
        assert_eq!(
            load_project_config(generated.path())
                .codex
                .resolved_sandbox(),
            CodexSandbox::DangerFullAccess
        );

        let custom = TempDir::new().unwrap();
        std::fs::write(
            custom.path().join(".umadevrc"),
            "[codex]\nsandbox_mode = \"workspace-write\"\n",
        )
        .unwrap();
        assert!(!migrate_legacy_generated_codex_sandbox(custom.path()).unwrap());
        assert_eq!(
            load_project_config(custom.path()).codex.resolved_sandbox(),
            CodexSandbox::WorkspaceWrite
        );

        let explicitly_saved = TempDir::new().unwrap();
        std::fs::write(
            explicitly_saved.path().join(".umadevrc"),
            format!("[codex]\n{LEGACY_GENERATED_CODEX_BLOCK}\n"),
        )
        .unwrap();
        persist_codex_sandbox(explicitly_saved.path(), CodexSandbox::WorkspaceWrite).unwrap();
        assert!(!migrate_legacy_generated_codex_sandbox(explicitly_saved.path()).unwrap());
    }
}
