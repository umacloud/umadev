//! Project-aware workspace initialization shared by the CLI and TUI.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use umadev_i18n::Lang;

use crate::adopt::DetectedCommand;
use crate::init_assets::{scaffold_init_knowledge, KnowledgeScaffoldReport};
use crate::manifest::{SpecManifest, MANIFEST_FILENAME};
use crate::verify::{detect_dev_server, detect_project, verify_steps, ProjectKind};

const MANAGED_BEGIN: &str = "<!-- umadev:project:begin -->";
const MANAGED_END: &str = "<!-- umadev:project:end -->";
const PROJECT_DISCOVERY_DEPTH: usize = 2;
const MAX_DISCOVERED_FILES: usize = 4_000;
const MAX_SOURCE_FILES: usize = 10_000;
const MAX_INIT_MANAGED_TEXT_BYTES: usize = 4 * 1024 * 1024;

/// Whether initialization found a pre-existing project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectShape {
    /// No repository, project manifest, source file, or user-owned root entry.
    Empty,
    /// A repository or existing project surface was found.
    Existing,
}

/// Read-only facts discovered before initialization writes anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAnalysis {
    /// Empty/greenfield or existing/brownfield classification.
    pub shape: ProjectShape,
    /// Detected languages, runtimes, and frameworks.
    pub stacks: Vec<String>,
    /// Existing project configuration files, relative to the root.
    pub configs: Vec<String>,
    /// Number of source files found by the bounded scan.
    pub source_files: usize,
    /// Whether a scan bound was reached.
    pub scan_truncated: bool,
    /// Maximum directory depth used for stack and configuration discovery.
    pub discovery_depth: usize,
    /// Whether `.git` exists at the project root.
    pub git_repository: bool,
    /// Build/test/lint commands derived from the detected root project.
    pub commands: Vec<DetectedCommand>,
    /// Detected development-server command, if any.
    pub dev_server: Option<String>,
}

/// Inputs to the shared initializer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInitOptions {
    /// Stable project slug written to `umadev.yaml`.
    pub slug: String,
    /// Replace a differing manifest, but never user-owned host guidance.
    pub force_manifest: bool,
}

impl ProjectInitOptions {
    /// Build safe default options for `slug`.
    #[must_use]
    pub fn new(slug: impl Into<String>) -> Self {
        Self {
            slug: slug.into(),
            force_manifest: false,
        }
    }
}

/// Structured initialization result rendered by both CLI and TUI.
#[derive(Debug, Clone)]
pub struct ProjectInitReport {
    /// Facts observed before writes.
    pub analysis: ProjectAnalysis,
    /// Effective on-disk manifest after initialization.
    pub manifest: SpecManifest,
    /// Manifest location.
    pub manifest_path: PathBuf,
    /// Bundled knowledge installation result.
    pub knowledge: KnowledgeScaffoldReport,
    /// Workspace-relative files newly created.
    pub created: Vec<String>,
    /// Files changed only through an UmaDev-managed block or safe append.
    pub updated: Vec<String>,
    /// Existing files intentionally left unchanged.
    pub preserved: Vec<String>,
    /// Non-fatal safety or I/O warnings.
    pub warnings: Vec<String>,
}

impl ProjectInitReport {
    /// Effective slug, respecting a preserved user manifest.
    #[must_use]
    pub fn effective_slug(&self) -> String {
        self.manifest
            .slug
            .clone()
            .filter(|slug| !slug.trim().is_empty())
            .unwrap_or_else(|| "project".to_string())
    }

    /// Render the same deterministic summary for CLI and TUI.
    #[must_use]
    pub fn render_summary(&self, lang: Lang) -> String {
        let labels = SummaryLabels::for_lang(lang);
        let mode = match self.analysis.shape {
            ProjectShape::Empty => labels.empty,
            ProjectShape::Existing => labels.existing,
        };
        let mut out = format!("{}: {mode}\n", labels.detected);
        out.push_str(&format!(
            "{}: {}\n",
            labels.stack,
            compact_list(&self.analysis.stacks, labels.none)
        ));
        out.push_str(&format!(
            "{}: {}{}\n",
            labels.source,
            self.analysis.source_files,
            if self.analysis.scan_truncated {
                "+"
            } else {
                ""
            }
        ));
        out.push_str(&format!(
            "{}: {}\n",
            labels.config,
            compact_list(&self.analysis.configs, labels.none)
        ));
        out.push_str(&format!(
            "{}: {} + {} {}\n",
            labels.scan_scope, labels.root, self.analysis.discovery_depth, labels.levels
        ));
        if !self.analysis.commands.is_empty() {
            let commands = self
                .analysis
                .commands
                .iter()
                .map(|command| format!("{}={}", command.name, command.command))
                .collect::<Vec<_>>();
            out.push_str(&format!(
                "{}: {}\n",
                labels.commands,
                compact_list(&commands, labels.none)
            ));
        }
        append_action_line(&mut out, labels.created, &self.created);
        append_action_line(&mut out, labels.updated, &self.updated);
        append_action_line(&mut out, labels.preserved, &self.preserved);
        out.push_str(&format!(
            "{}: {}/{} {}\n",
            labels.knowledge, self.knowledge.created, self.knowledge.total, labels.new_files
        ));
        append_action_line(&mut out, labels.warnings, &self.warnings);
        out.trim_end().to_string()
    }
}

/// Inspect a workspace without modifying it.
#[must_use]
pub fn analyze_project(project_root: &Path) -> ProjectAnalysis {
    let (files, discovered_truncated) =
        discover_files(project_root, PROJECT_DISCOVERY_DEPTH, MAX_DISCOVERED_FILES);
    let (source_files, source_truncated) = count_source_files(project_root);
    let stacks = detect_stacks(&files);
    let configs = detect_configs(project_root, &files);
    let git_repository = project_root.join(".git").exists();
    let kind = detect_project(project_root);
    let commands = detected_commands(kind, project_root);
    let dev_server = detect_dev_server(project_root).map(|server| server.command);
    let existing = git_repository
        || !stacks.is_empty()
        || source_files > 0
        || has_meaningful_root_entry(project_root);
    ProjectAnalysis {
        shape: if existing {
            ProjectShape::Existing
        } else {
            ProjectShape::Empty
        },
        stacks,
        configs,
        source_files,
        scan_truncated: discovered_truncated || source_truncated,
        discovery_depth: PROJECT_DISCOVERY_DEPTH,
        git_repository,
        commands,
        dev_server,
    }
}

/// Initialize a workspace without replacing user-owned content.
pub fn initialize_project(
    project_root: &Path,
    options: &ProjectInitOptions,
) -> io::Result<ProjectInitReport> {
    let analysis = analyze_project(project_root);
    fs::create_dir_all(project_root)?;
    let mut created = Vec::new();
    let mut updated = Vec::new();
    let mut preserved = Vec::new();
    let mut warnings = Vec::new();

    let (manifest, manifest_path) = ensure_manifest(
        project_root,
        options,
        &mut created,
        &mut updated,
        &mut preserved,
        &mut warnings,
    )?;
    let block = managed_project_block(&analysis);
    ensure_guidance(
        project_root,
        "CLAUDE.md",
        "# CLAUDE.md\n\nThis project uses UmaDev. The latest explicit user request is the only task authorization; prior plans and `.umadev/coach/CURRENT.md` are context unless that request explicitly continues the active run.\n",
        &block,
        &mut created,
        &mut updated,
        &mut preserved,
        &mut warnings,
    );
    ensure_guidance(
        project_root,
        "AGENTS.md",
        "# AGENTS.md\n\nThis project uses UmaDev. The latest explicit user request is the only task authorization; prior plans and `.umadev/coach/CURRENT.md` are context unless that request explicitly continues the active run.\n",
        &block,
        &mut created,
        &mut updated,
        &mut preserved,
        &mut warnings,
    );
    ensure_project_config(project_root, &mut created, &mut preserved, &mut warnings);
    ensure_rules(project_root, &mut created, &mut preserved, &mut warnings);
    ensure_gitignore(
        project_root,
        &mut created,
        &mut updated,
        &mut preserved,
        &mut warnings,
    );

    let knowledge = scaffold_init_knowledge(project_root);
    if knowledge.failed > 0 {
        warnings.push(format!(
            "knowledge/: {} of {} bundled files could not be created",
            knowledge.failed, knowledge.total
        ));
    }

    Ok(ProjectInitReport {
        analysis,
        manifest,
        manifest_path,
        knowledge,
        created,
        updated,
        preserved,
        warnings,
    })
}

fn ensure_manifest(
    root: &Path,
    options: &ProjectInitOptions,
    created: &mut Vec<String>,
    updated: &mut Vec<String>,
    preserved: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> io::Result<(SpecManifest, PathBuf)> {
    let requested = SpecManifest::new(&options.slug);
    let requested_yaml = requested.to_yaml();
    let path = root.join(MANIFEST_FILENAME);
    if is_symlink(&path) {
        preserved.push(MANIFEST_FILENAME.to_string());
        warnings.push("umadev.yaml is a symlink; it was not followed or replaced".to_string());
        return Ok((SpecManifest::read_from(root).unwrap_or(requested), path));
    }
    match crate::bounded_fs::read_utf8_beneath(root, &path, MAX_INIT_MANAGED_TEXT_BYTES) {
        Ok(existing) => {
            if options.force_manifest {
                if existing == requested_yaml {
                    preserved.push(MANIFEST_FILENAME.to_string());
                } else {
                    umadev_state::fs::atomic_write(&path, requested_yaml.as_bytes())?;
                    updated.push(MANIFEST_FILENAME.to_string());
                }
            } else {
                preserved.push(MANIFEST_FILENAME.to_string());
                if existing != requested_yaml {
                    warnings.push(
                    "umadev.yaml differs from the generated manifest; preserved (use --force in the CLI to replace it)"
                        .to_string(),
                );
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match umadev_state::fs::write_new_private(&path, requested_yaml.as_bytes()) {
                Ok(()) => created.push(MANIFEST_FILENAME.to_string()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    preserved.push(MANIFEST_FILENAME.to_string());
                    warnings.push(
                        "umadev.yaml appeared during init; preserved without replacing it"
                            .to_string(),
                    );
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => {
            if fs::symlink_metadata(&path)
                .is_ok_and(|metadata| !umadev_state::fs::metadata_is_real_file(&metadata))
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists but is not a regular file", path.display()),
                ));
            }
            preserved.push(MANIFEST_FILENAME.to_string());
            warnings.push(format!(
                "umadev.yaml could not be read and was preserved: {error}"
            ));
        }
    }
    Ok((SpecManifest::read_from(root).unwrap_or(requested), path))
}

#[allow(clippy::too_many_arguments)]
fn ensure_guidance(
    root: &Path,
    name: &str,
    header: &str,
    block: &str,
    created: &mut Vec<String>,
    updated: &mut Vec<String>,
    preserved: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let path = root.join(name);
    if is_symlink(&path) {
        preserved.push(name.to_string());
        warnings.push(format!(
            "{name} is a symlink; preserved without following it"
        ));
        return;
    }
    let existing =
        match crate::bounded_fs::read_utf8_beneath(root, &path, MAX_INIT_MANAGED_TEXT_BYTES) {
            Ok(body) => body,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let body = format!("{}\n{}\n", header.trim_end(), block.trim_end());
                match umadev_state::fs::write_new_private(&path, body.as_bytes()) {
                    Ok(()) => created.push(name.to_string()),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        preserved.push(name.to_string());
                        warnings.push(format!(
                            "{name} appeared during init; preserved without replacing it"
                        ));
                    }
                    Err(error) => warnings.push(format!("could not create {name}: {error}")),
                }
                return;
            }
            Err(error) => {
                preserved.push(name.to_string());
                warnings.push(format!("could not read {name}; preserved: {error}"));
                return;
            }
        };
    let merged = match upsert_managed_block(&existing, block) {
        Ok(body) => body,
        Err(reason) => {
            preserved.push(name.to_string());
            warnings.push(format!(
                "{name} has malformed UmaDev markers; preserved: {reason}"
            ));
            return;
        }
    };
    if merged == existing {
        preserved.push(name.to_string());
    } else {
        match crate::phases::atomic_write(&path, &merged) {
            Ok(()) => updated.push(format!("{name} (managed block only)")),
            Err(error) => warnings.push(format!("could not update {name}: {error}")),
        }
    }
}

fn ensure_project_config(
    root: &Path,
    created: &mut Vec<String>,
    preserved: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let path = root.join(".umadevrc");
    let config = crate::config::ProjectConfig::default();
    let body = format!(
        "# UmaDev project configuration\n\
         [quality]\nthreshold = {}\nskip_checks = []\n\n\
         [pipeline]\nskip_phases = []\nmax_review_rounds = {}\nstrict_coverage = {}\nauto_approve_gates = {}\n\n\
         [knowledge]\nenabled = {}\nengine = {:?}\ntop_k = {}\n\n\
         [codex]\nsandbox_mode = {:?}\n",
        config.quality.threshold,
        config.pipeline.max_review_rounds,
        config.pipeline.strict_coverage,
        config.pipeline.auto_approve_gates,
        config.knowledge.enabled,
        config.knowledge.engine,
        config.knowledge.top_k,
        config.codex.resolved_sandbox().as_codex_arg(),
    );
    match umadev_state::fs::write_new_private(&path, body.as_bytes()) {
        Ok(()) => created.push(".umadevrc".to_string()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            preserved.push(".umadevrc".to_string());
        }
        Err(error) => warnings.push(format!("could not create .umadevrc: {error}")),
    }
}

fn ensure_rules(
    root: &Path,
    created: &mut Vec<String>,
    preserved: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    let dir = root.join(".umadev");
    let path = dir.join("rules.toml");
    if is_symlink(&dir) {
        warnings.push(".umadev is a symlink; rules.toml was not written".to_string());
        return;
    }
    if path.exists() {
        preserved.push(".umadev/rules.toml".to_string());
        return;
    }
    match umadev_governance::Policy::write_default_template(root) {
        Ok(_) => created.push(".umadev/rules.toml".to_string()),
        Err(error) => warnings.push(format!("could not create .umadev/rules.toml: {error}")),
    }
}

fn ensure_gitignore(
    root: &Path,
    created: &mut Vec<String>,
    updated: &mut Vec<String>,
    preserved: &mut Vec<String>,
    warnings: &mut Vec<String>,
) {
    const ENTRIES: [&str; 3] = [".umadev/", "output/", "opencode.json"];
    let path = root.join(".gitignore");
    if is_symlink(&path) {
        preserved.push(".gitignore".to_string());
        warnings.push(".gitignore is a symlink; preserved without following it".to_string());
        return;
    }
    let (existing, existed) =
        match crate::bounded_fs::read_utf8_beneath(root, &path, MAX_INIT_MANAGED_TEXT_BYTES) {
            Ok(body) => (body, true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (String::new(), false),
            Err(error) => {
                preserved.push(".gitignore".to_string());
                warnings.push(format!("could not read .gitignore; preserved: {error}"));
                return;
            }
        };
    let missing = ENTRIES
        .iter()
        .filter(|entry| !gitignore_covers(&existing, entry))
        .copied()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        preserved.push(".gitignore".to_string());
        return;
    }
    let mut body = existing.trim_end().to_string();
    if !body.is_empty() {
        body.push_str("\n\n");
    }
    body.push_str("# UmaDev runtime data\n");
    for entry in missing {
        body.push_str(entry);
        body.push('\n');
    }
    let result = if existed {
        crate::phases::atomic_write(&path, &body)
    } else {
        // A create-only open closes the check/create race: if another process
        // or the user creates .gitignore after our read, preserve their file
        // instead of replacing it with the generated template.
        umadev_state::fs::write_new_private(&path, body.as_bytes())
    };
    match result {
        Ok(()) if !existed => created.push(".gitignore".to_string()),
        Ok(()) => updated.push(".gitignore (missing entries appended)".to_string()),
        Err(error) if !existed && error.kind() == io::ErrorKind::AlreadyExists => {
            preserved.push(".gitignore".to_string());
            warnings.push(
                ".gitignore appeared during init; preserved without replacing it".to_string(),
            );
        }
        Err(error) => warnings.push(format!("could not update .gitignore: {error}")),
    }
}

fn managed_project_block(analysis: &ProjectAnalysis) -> String {
    let mode = match analysis.shape {
        ProjectShape::Empty => "greenfield / empty workspace",
        ProjectShape::Existing => {
            "existing repository; preserve conventions and change incrementally"
        }
    };
    let mut body = format!(
        "{MANAGED_BEGIN}\n## UmaDev project context\n\n- **Mode**: {mode}\n- **Stack**: {}\n- **Source files scanned**: {}{}\n- **Existing config**: {}\n",
        compact_list(&analysis.stacks, "not detected"),
        analysis.source_files,
        if analysis.scan_truncated { "+" } else { "" },
        compact_list(&analysis.configs, "none detected"),
    );
    body.push_str(
        "\n### Current-task authority\n\n\
         - The latest user message is the current objective. Existing plans, \
         `.umadev/coach/CURRENT.md`, run notes, output documents, and earlier conversation are \
         context only; their presence does not authorize continuing old work.\n\
         - Resume an earlier plan only when the user explicitly asks to continue or resume it. \
         Keep targeted work within the requested scope and do not fix unrelated issues.\n",
    );
    if let Some(server) = &analysis.dev_server {
        body.push_str(&format!("- **Dev server**: `{server}`\n"));
    }
    if !analysis.commands.is_empty() {
        body.push_str("\n### Build / test / lint\n\n");
        for command in &analysis.commands {
            body.push_str(&format!("- **{}**: `{}`\n", command.name, command.command));
        }
    }
    body.push_str(&format!("\n{MANAGED_END}"));
    body
}

fn upsert_managed_block(existing: &str, block: &str) -> Result<String, &'static str> {
    match (existing.find(MANAGED_BEGIN), existing.find(MANAGED_END)) {
        (None, None) => {
            let mut output = existing.trim_end().to_string();
            if !output.is_empty() {
                output.push_str("\n\n");
            }
            output.push_str(block.trim_end());
            output.push('\n');
            Ok(output)
        }
        (Some(begin), Some(end)) if end >= begin => {
            let end = end + MANAGED_END.len();
            let mut output = String::with_capacity(existing.len() + block.len());
            output.push_str(&existing[..begin]);
            output.push_str(block.trim_end());
            output.push_str(&existing[end..]);
            Ok(output)
        }
        _ => Err("only one marker exists or the end marker precedes the begin marker"),
    }
}

fn detected_commands(kind: ProjectKind, root: &Path) -> Vec<DetectedCommand> {
    verify_steps(kind, root)
        .unwrap_or_default()
        .into_iter()
        .map(|step| DetectedCommand {
            name: step.name.to_string(),
            command: if step.args.is_empty() {
                step.program
            } else {
                format!("{} {}", step.program, step.args.join(" "))
            },
        })
        .collect()
}

fn detect_stacks(files: &[PathBuf]) -> Vec<String> {
    let mut stacks = BTreeSet::new();
    for path in files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match name.as_str() {
            "cargo.toml" => {
                stacks.insert("Rust".to_string());
            }
            "package.json" => {
                stacks.insert("Node.js".to_string());
                detect_package_stacks(path, &mut stacks);
            }
            "tsconfig.json" => {
                stacks.insert("TypeScript".to_string());
            }
            "pyproject.toml" | "requirements.txt" | "setup.py" => {
                stacks.insert("Python".to_string());
            }
            "go.mod" => {
                stacks.insert("Go".to_string());
            }
            "deno.json" | "deno.jsonc" => {
                stacks.insert("Deno".to_string());
            }
            "pom.xml" => {
                stacks.insert("Java / Maven".to_string());
            }
            "build.gradle" | "build.gradle.kts" => {
                stacks.insert("Java / Gradle".to_string());
            }
            "composer.json" => {
                stacks.insert("PHP".to_string());
            }
            "gemfile" => {
                stacks.insert("Ruby".to_string());
            }
            "package.swift" => {
                stacks.insert("Swift".to_string());
            }
            _ if has_dotnet_project_extension(&name) => {
                stacks.insert(".NET".to_string());
            }
            _ => {}
        }
    }
    stacks.into_iter().collect()
}

fn detect_package_stacks(path: &Path, stacks: &mut BTreeSet<String>) {
    let Ok(body) = crate::bounded_fs::read_utf8(path, 1024 * 1024) else {
        return;
    };
    let lower = body.to_ascii_lowercase();
    for (needle, label) in [
        ("\"typescript\"", "TypeScript"),
        ("\"next\"", "Next.js"),
        ("\"react\"", "React"),
        ("\"vue\"", "Vue"),
        ("\"vite\"", "Vite"),
        ("\"svelte\"", "Svelte"),
        ("\"@angular/core\"", "Angular"),
        ("\"electron\"", "Electron"),
    ] {
        if lower.contains(needle) {
            stacks.insert(label.to_string());
        }
    }
}

fn detect_configs(root: &Path, files: &[PathBuf]) -> Vec<String> {
    let mut configs = BTreeSet::new();
    for path in files {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if is_config_name(name) {
            if let Ok(relative) = path.strip_prefix(root) {
                configs.insert(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    for dir in [".github", ".gitlab", ".circleci"] {
        if root.join(dir).is_dir() {
            configs.insert(format!("{dir}/"));
        }
    }
    configs.into_iter().collect()
}

fn is_config_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "cargo.toml"
            | "package.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
            | "package-lock.json"
            | "tsconfig.json"
            | "pyproject.toml"
            | "requirements.txt"
            | "go.mod"
            | "deno.json"
            | "deno.jsonc"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "composer.json"
            | "gemfile"
            | "package.swift"
            | "dockerfile"
            | "docker-compose.yml"
            | "compose.yml"
            | "makefile"
            | "justfile"
            | ".editorconfig"
            | ".env.example"
            | "rust-toolchain.toml"
            | "rustfmt.toml"
            | "clippy.toml"
    ) || lower.starts_with("eslint.config.")
        || lower.starts_with(".eslintrc")
        || lower.starts_with(".prettierrc")
        || has_dotnet_project_extension(&lower)
}

fn has_dotnet_project_extension(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("csproj") || extension.eq_ignore_ascii_case("sln")
        })
}

fn discover_files(root: &Path, max_depth: usize, cap: usize) -> (Vec<PathBuf>, bool) {
    let mut found = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0_usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_file() {
                found.push(path);
                if found.len() >= cap {
                    return (found, true);
                }
            } else if file_type.is_dir()
                && depth < max_depth
                && !skip_directory(entry.file_name().to_string_lossy().as_ref())
            {
                stack.push((path, depth + 1));
            }
        }
    }
    found.sort();
    (found, false)
}

fn count_source_files(root: &Path) -> (usize, bool) {
    let mut count = 0;
    let mut stack = vec![(root.to_path_buf(), 0_usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                if depth < 8 && !skip_directory(entry.file_name().to_string_lossy().as_ref()) {
                    stack.push((entry.path(), depth + 1));
                }
                continue;
            }
            if file_type.is_file() && is_source_file(&entry.path()) {
                count += 1;
                if count >= MAX_SOURCE_FILES {
                    return (count, true);
                }
            }
        }
    }
    (count, false)
}

fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "rs" | "js"
            | "jsx"
            | "ts"
            | "tsx"
            | "vue"
            | "svelte"
            | "py"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "cs"
            | "cpp"
            | "cc"
            | "c"
            | "h"
            | "hpp"
            | "swift"
            | "php"
            | "rb"
            | "dart"
            | "ex"
            | "exs"
    )
}

fn skip_directory(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | ".umadev"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "vendor"
            | ".venv"
            | "venv"
            | "__pycache__"
            | "knowledge"
            | "output"
            | "release"
    )
}

fn has_meaningful_root_entry(root: &Path) -> bool {
    const GENERATED: [&str; 11] = [
        ".DS_Store",
        ".umadev",
        ".umadevrc",
        "umadev.yaml",
        "CLAUDE.md",
        "AGENTS.md",
        ".gitignore",
        "knowledge",
        "output",
        "release",
        "opencode.json",
    ];
    fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            !GENERATED.contains(&name.as_ref())
        })
    })
}

fn gitignore_covers(body: &str, expected: &str) -> bool {
    let expected = expected.trim_start_matches('/').trim_end_matches('/');
    body.lines().any(|line| {
        let line = line.trim();
        !line.starts_with('#')
            && line
                .trim_start_matches('/')
                .trim_end_matches('/')
                .eq(expected)
    })
}

fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

fn compact_list(values: &[String], none: &str) -> String {
    if values.is_empty() {
        return none.to_string();
    }
    const SHOWN: usize = 12;
    let mut output = values
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    if values.len() > SHOWN {
        output.push_str(&format!(" · +{}", values.len() - SHOWN));
    }
    output
}

fn append_action_line(output: &mut String, label: &str, values: &[String]) {
    if !values.is_empty() {
        output.push_str(&format!("{label}: {}\n", values.join(" · ")));
    }
}

struct SummaryLabels {
    detected: &'static str,
    empty: &'static str,
    existing: &'static str,
    stack: &'static str,
    source: &'static str,
    config: &'static str,
    scan_scope: &'static str,
    root: &'static str,
    levels: &'static str,
    commands: &'static str,
    created: &'static str,
    updated: &'static str,
    preserved: &'static str,
    knowledge: &'static str,
    new_files: &'static str,
    warnings: &'static str,
    none: &'static str,
}

impl SummaryLabels {
    const fn for_lang(lang: Lang) -> Self {
        match lang {
            Lang::ZhCn => Self {
                detected: "项目识别",
                empty: "空目录（新项目）",
                existing: "已有仓库（增量初始化）",
                stack: "技术栈",
                source: "源码文件",
                config: "已有配置",
                scan_scope: "识别范围（有界）",
                root: "项目根目录",
                levels: "层目录",
                commands: "可用命令",
                created: "已生成",
                updated: "已安全补齐",
                preserved: "已保留",
                knowledge: "知识文件",
                new_files: "个新增",
                warnings: "注意",
                none: "未发现",
            },
            Lang::ZhTw => Self {
                detected: "專案識別",
                empty: "空目錄（新專案）",
                existing: "既有儲存庫（增量初始化）",
                stack: "技術棧",
                source: "原始碼檔案",
                config: "既有設定",
                scan_scope: "識別範圍（有界）",
                root: "專案根目錄",
                levels: "層目錄",
                commands: "可用命令",
                created: "已建立",
                updated: "已安全補齊",
                preserved: "已保留",
                knowledge: "知識檔案",
                new_files: "個新增",
                warnings: "注意",
                none: "未發現",
            },
            Lang::En => Self {
                detected: "Project detected",
                empty: "empty workspace (new project)",
                existing: "existing repository (incremental initialization)",
                stack: "Stack",
                source: "Source files",
                config: "Existing config",
                scan_scope: "Detection scope (bounded)",
                root: "project root",
                levels: "directory levels",
                commands: "Commands",
                created: "Created",
                updated: "Safely supplemented",
                preserved: "Preserved",
                knowledge: "Knowledge files",
                new_files: "new",
                warnings: "Warnings",
                none: "none detected",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_workspace_uses_greenfield_path_and_creates_both_host_guides() {
        let temp = tempfile::TempDir::new().unwrap();
        let report = initialize_project(temp.path(), &ProjectInitOptions::new("demo")).unwrap();

        assert_eq!(report.analysis.shape, ProjectShape::Empty);
        for path in [
            "umadev.yaml",
            ".umadevrc",
            "CLAUDE.md",
            "AGENTS.md",
            ".gitignore",
            ".umadev/rules.toml",
        ] {
            assert!(temp.path().join(path).is_file(), "missing {path}");
        }
        for guide in ["CLAUDE.md", "AGENTS.md"] {
            let body = fs::read_to_string(temp.path().join(guide)).unwrap();
            assert!(body.contains("latest explicit user request is the only task authorization"));
            assert!(!body.contains("read it before continuing"));
        }
        assert_eq!(report.knowledge.created, report.knowledge.total);
        assert!(!temp.path().join(".umadev/adopt.json").exists());
        assert!(!temp.path().join("UMADEV.md").exists());
        assert!(report.render_summary(Lang::ZhCn).contains("空目录"));
    }

    #[test]
    fn existing_repo_is_detected_and_user_content_is_preserved() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .unwrap();
        fs::write(temp.path().join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
        fs::write(
            temp.path().join("AGENTS.md"),
            "# Team rules\n\nKeep this.\n",
        )
        .unwrap();
        fs::write(
            temp.path().join("CLAUDE.md"),
            "# Claude\n\nKeep this too.\n",
        )
        .unwrap();
        fs::write(temp.path().join(".umadevrc"), "[quality]\nthreshold=77\n").unwrap();
        fs::write(temp.path().join(".gitignore"), "vendor/\n").unwrap();

        let report = initialize_project(temp.path(), &ProjectInitOptions::new("demo")).unwrap();

        assert_eq!(report.analysis.shape, ProjectShape::Existing);
        assert!(report.analysis.stacks.contains(&"Rust".to_string()));
        assert!(report.analysis.configs.contains(&"Cargo.toml".to_string()));
        assert_eq!(
            fs::read_to_string(temp.path().join(".umadevrc")).unwrap(),
            "[quality]\nthreshold=77\n"
        );
        let agents = fs::read_to_string(temp.path().join("AGENTS.md")).unwrap();
        let claude = fs::read_to_string(temp.path().join("CLAUDE.md")).unwrap();
        assert!(agents.contains("Keep this.") && agents.contains(MANAGED_BEGIN));
        assert!(claude.contains("Keep this too.") && claude.contains(MANAGED_BEGIN));
        for guide in [&agents, &claude] {
            assert!(guide.contains("Current-task authority"));
            assert!(guide.contains("context only"));
            assert!(guide.contains("explicitly asks to continue or resume"));
        }
        let gitignore = fs::read_to_string(temp.path().join(".gitignore")).unwrap();
        assert!(gitignore.contains("vendor/") && gitignore.contains(".umadev/"));
        assert!(!temp.path().join(".umadev/adopt.json").exists());
    }

    #[test]
    fn rerun_is_idempotent_and_refreshes_only_managed_blocks() {
        let temp = tempfile::TempDir::new().unwrap();
        fs::write(
            temp.path().join("package.json"),
            r#"{"dependencies":{"vite":"1"}}"#,
        )
        .unwrap();
        let first = initialize_project(temp.path(), &ProjectInitOptions::new("web")).unwrap();
        assert_eq!(first.analysis.shape, ProjectShape::Existing);
        let agents = fs::read_to_string(temp.path().join("AGENTS.md")).unwrap();
        let second = initialize_project(temp.path(), &ProjectInitOptions::new("web")).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("AGENTS.md")).unwrap(),
            agents
        );
        assert!(second.updated.is_empty());
        assert_eq!(second.knowledge.created, 0);
        assert_eq!(second.knowledge.preserved, second.knowledge.total);
    }

    #[test]
    fn malformed_managed_markers_are_never_overwritten() {
        let temp = tempfile::TempDir::new().unwrap();
        let original = format!("# User rules\n{MANAGED_BEGIN}\nunfinished\n");
        fs::write(temp.path().join("AGENTS.md"), &original).unwrap();
        let report = initialize_project(temp.path(), &ProjectInitOptions::new("demo")).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("AGENTS.md")).unwrap(),
            original
        );
        assert!(report
            .warnings
            .iter()
            .any(|warning| warning.contains("AGENTS.md") && warning.contains("markers")));
    }

    #[test]
    fn differing_manifest_is_preserved_unless_force_is_explicit() {
        let temp = tempfile::TempDir::new().unwrap();
        let custom = "# user manifest\nproject:\n  slug: custom\n";
        fs::write(temp.path().join("umadev.yaml"), custom).unwrap();
        let report =
            initialize_project(temp.path(), &ProjectInitOptions::new("generated")).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("umadev.yaml")).unwrap(),
            custom
        );
        assert_eq!(report.effective_slug(), "custom");
        assert!(!report.warnings.is_empty());

        let forced = ProjectInitOptions {
            slug: "generated".to_string(),
            force_manifest: true,
        };
        let report = initialize_project(temp.path(), &forced).unwrap();
        assert_eq!(report.effective_slug(), "generated");
        assert!(report.updated.contains(&"umadev.yaml".to_string()));
    }

    #[test]
    fn oversized_user_owned_init_files_are_preserved_without_being_loaded() {
        let temp = tempfile::TempDir::new().unwrap();
        for name in ["AGENTS.md", ".gitignore", "umadev.yaml"] {
            std::fs::File::create(temp.path().join(name))
                .unwrap()
                .set_len((MAX_INIT_MANAGED_TEXT_BYTES + 1) as u64)
                .unwrap();
        }

        let report = initialize_project(temp.path(), &ProjectInitOptions::new("demo")).unwrap();

        for name in ["AGENTS.md", ".gitignore", "umadev.yaml"] {
            assert_eq!(
                std::fs::metadata(temp.path().join(name)).unwrap().len(),
                (MAX_INIT_MANAGED_TEXT_BYTES + 1) as u64,
                "init must preserve oversized user-owned {name}"
            );
            assert!(
                report.warnings.iter().any(|warning| warning.contains(name)),
                "init should explain why {name} was preserved"
            );
        }
    }
}
