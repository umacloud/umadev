use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use toml::Value;

const HOTSPOT_LINES: &[(&str, usize)] = &[
    ("crates/umadev-tui/src/app.rs", 18_681),
    ("crates/umadev-tui/src/lib.rs", 11_985),
    ("crates/umadev-agent/src/director_loop.rs", 7_243),
    ("crates/umadev-governance/src/rules.rs", 8_953),
];

const CONTROL_FLOW_RATCHET_FILES: &[&str] = &[
    "crates/umadev-agent/src/design_system.rs",
    "crates/umadev-agent/src/plan_state.rs",
    "crates/umadev-governance/src/tokenizer.rs",
    "crates/umadev-host/src/claude.rs",
    "crates/umadev-host/src/codex.rs",
    "crates/umadev-tui/src/app.rs",
    "crates/umadev-tui/src/lib.rs",
    "crates/umadev/src/skill_manager.rs",
];

// This is a maximum-edge allowlist: removing an edge is allowed; adding one
// requires an explicit architecture review. Keep every workspace package here
// so a new member cannot silently bypass the graph contract.
const NORMAL_ALLOWLIST: &[(&str, &[&str])] = &[
    (
        "umadev",
        &[
            "umadev-agent",
            "umadev-contract",
            "umadev-governance",
            "umadev-host",
            "umadev-i18n",
            "umadev-runtime",
            "umadev-spec",
            "umadev-state",
            "umadev-tui",
        ],
    ),
    (
        "umadev-agent",
        &[
            "umadev-contract",
            "umadev-governance",
            "umadev-i18n",
            "umadev-knowledge",
            "umadev-runtime",
            "umadev-spec",
            "umadev-state",
        ],
    ),
    ("umadev-contract", &["umadev-spec"]),
    ("umadev-governance", &["umadev-spec"]),
    (
        "umadev-host",
        &[
            "umadev-governance",
            "umadev-process",
            "umadev-runtime",
            "umadev-spec",
        ],
    ),
    ("umadev-i18n", &[]),
    ("umadev-knowledge", &["umadev-spec", "umadev-state"]),
    ("umadev-process", &[]),
    ("umadev-runtime", &["umadev-spec"]),
    ("umadev-spec", &[]),
    ("umadev-state", &[]),
    (
        "umadev-tui",
        &[
            "umadev-agent",
            "umadev-host",
            "umadev-i18n",
            "umadev-runtime",
            "umadev-spec",
            "umadev-state",
        ],
    ),
];

const DEV_ALLOWLIST: &[(&str, &[&str])] = &[
    ("umadev", &[]),
    ("umadev-agent", &[]),
    ("umadev-contract", &[]),
    ("umadev-governance", &["umadev-contract"]),
    ("umadev-host", &[]),
    ("umadev-i18n", &[]),
    ("umadev-knowledge", &[]),
    ("umadev-process", &[]),
    ("umadev-runtime", &[]),
    ("umadev-spec", &[]),
    ("umadev-state", &[]),
    ("umadev-tui", &[]),
];

const FOUNDATION_PACKAGES: &[&str] = &[
    "umadev-spec",
    "umadev-i18n",
    "umadev-runtime",
    "umadev-contract",
    "umadev-governance",
    "umadev-knowledge",
    "umadev-host",
    "umadev-process",
    "umadev-state",
];
const UPPER_PACKAGES: &[&str] = &["umadev-agent", "umadev-tui", "umadev"];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DependencyScope {
    Normal,
    Build,
    Dev,
    TargetNormal(String),
    TargetBuild(String),
    TargetDev(String),
}

impl DependencyScope {
    fn is_dev(&self) -> bool {
        matches!(self, Self::Dev | Self::TargetDev(_))
    }

    fn is_production(&self) -> bool {
        !self.is_dev()
    }

    fn label(&self) -> String {
        match self {
            Self::Normal => "dependencies".to_string(),
            Self::Build => "build-dependencies".to_string(),
            Self::Dev => "dev-dependencies".to_string(),
            Self::TargetNormal(target) => format!("target.{target}.dependencies"),
            Self::TargetBuild(target) => format!("target.{target}.build-dependencies"),
            Self::TargetDev(target) => format!("target.{target}.dev-dependencies"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct InternalDependency {
    from: String,
    to: String,
    scope: DependencyScope,
}

#[derive(Debug)]
struct WorkspaceModel {
    packages: BTreeSet<String>,
    dependencies: Vec<InternalDependency>,
}

impl WorkspaceModel {
    fn load(root: &Path) -> Self {
        let root_manifest = parse_manifest(&root.join("Cargo.toml"));
        let workspace_dependencies = workspace_dependency_packages(&root_manifest);
        let members = root_manifest
            .get("workspace")
            .and_then(|v| v.get("members"))
            .and_then(Value::as_array)
            .expect("workspace.members must be an array");

        let mut manifests = Vec::new();
        let mut packages = BTreeSet::new();
        for member in members {
            let relative = member
                .as_str()
                .expect("workspace member paths must be strings");
            assert!(
                !relative.contains(['*', '?', '[']),
                "workspace architecture guard requires explicit member paths, got {relative}"
            );
            let manifest = parse_manifest(&root.join(relative).join("Cargo.toml"));
            let name = manifest
                .get("package")
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{relative}/Cargo.toml has no package.name"))
                .to_string();
            assert!(
                packages.insert(name.clone()),
                "duplicate package name {name}"
            );
            manifests.push((name, manifest));
        }

        let mut dependencies = Vec::new();
        for (name, manifest) in &manifests {
            dependencies.extend(dependencies_from_manifest(
                name,
                manifest,
                &packages,
                &workspace_dependencies,
            ));
        }
        dependencies.sort();
        dependencies.dedup();

        Self {
            packages,
            dependencies,
        }
    }
}

fn workspace_dependency_packages(manifest: &Value) -> BTreeMap<String, String> {
    manifest
        .get("workspace")
        .and_then(|v| v.get("dependencies"))
        .and_then(Value::as_table)
        .map(|dependencies| {
            dependencies
                .iter()
                .map(|(alias, specification)| {
                    let package = specification
                        .as_table()
                        .and_then(|table| table.get("package"))
                        .and_then(Value::as_str)
                        .unwrap_or(alias);
                    (alias.clone(), package.to_string())
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_manifest(path: &Path) -> Value {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    toml::from_str(&text).unwrap_or_else(|error| panic!("cannot parse {}: {error}", path.display()))
}

fn dependencies_from_manifest(
    from: &str,
    manifest: &Value,
    internal_packages: &BTreeSet<String>,
    workspace_dependencies: &BTreeMap<String, String>,
) -> Vec<InternalDependency> {
    let mut out = Vec::new();
    collect_dependency_table(
        from,
        manifest.get("dependencies"),
        DependencyScope::Normal,
        internal_packages,
        workspace_dependencies,
        &mut out,
    );
    collect_dependency_table(
        from,
        manifest.get("build-dependencies"),
        DependencyScope::Build,
        internal_packages,
        workspace_dependencies,
        &mut out,
    );
    collect_dependency_table(
        from,
        manifest.get("dev-dependencies"),
        DependencyScope::Dev,
        internal_packages,
        workspace_dependencies,
        &mut out,
    );

    if let Some(targets) = manifest.get("target").and_then(Value::as_table) {
        for (target, tables) in targets {
            collect_dependency_table(
                from,
                tables.get("dependencies"),
                DependencyScope::TargetNormal(target.clone()),
                internal_packages,
                workspace_dependencies,
                &mut out,
            );
            collect_dependency_table(
                from,
                tables.get("build-dependencies"),
                DependencyScope::TargetBuild(target.clone()),
                internal_packages,
                workspace_dependencies,
                &mut out,
            );
            collect_dependency_table(
                from,
                tables.get("dev-dependencies"),
                DependencyScope::TargetDev(target.clone()),
                internal_packages,
                workspace_dependencies,
                &mut out,
            );
        }
    }
    out
}

fn collect_dependency_table(
    from: &str,
    value: Option<&Value>,
    scope: DependencyScope,
    internal_packages: &BTreeSet<String>,
    workspace_dependencies: &BTreeMap<String, String>,
    out: &mut Vec<InternalDependency>,
) {
    let Some(table) = value.and_then(Value::as_table) else {
        return;
    };
    for (alias, specification) in table {
        let specification = specification.as_table();
        let package = specification
            .and_then(|table| table.get("package"))
            .and_then(Value::as_str)
            .or_else(|| {
                specification
                    .and_then(|table| table.get("workspace"))
                    .and_then(Value::as_bool)
                    .filter(|enabled| *enabled)
                    .and_then(|_| workspace_dependencies.get(alias).map(String::as_str))
            })
            .unwrap_or(alias);
        if internal_packages.contains(package) {
            out.push(InternalDependency {
                from: from.to_string(),
                to: package.to_string(),
                scope: scope.clone(),
            });
        }
    }
}

fn locate_workspace_root() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|candidate| {
            candidate.join("Cargo.toml").is_file()
                && candidate.join("crates/umadev-agent/Cargo.toml").is_file()
                && candidate.join("crates/umadev-tui/src/app.rs").is_file()
        })
        .map(Path::to_path_buf)
}

fn allowlisted_packages(allowlist: &[(&str, &[&str])]) -> BTreeSet<String> {
    allowlist
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect()
}

fn assert_allowlisted(
    model: &WorkspaceModel,
    allowlist: &[(&str, &[&str])],
    select: impl Fn(&DependencyScope) -> bool,
) {
    assert_eq!(
        model.packages,
        allowlisted_packages(allowlist),
        "workspace members changed; add the new package to the architecture allowlist only after reviewing its layer"
    );

    let mut violations = Vec::new();
    for dependency in model.dependencies.iter().filter(|d| select(&d.scope)) {
        let allowed = allowlist
            .iter()
            .find_map(|(name, targets)| (*name == dependency.from).then_some(*targets))
            .expect("workspace package missing from dependency allowlist");
        if !allowed.contains(&dependency.to.as_str()) {
            violations.push(format!(
                "{} -> {} [{}]",
                dependency.from,
                dependency.to,
                dependency.scope.label()
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "unreviewed internal dependency edge(s):\n{}",
        violations.join("\n")
    );
}

#[test]
fn legacy_hotspots_never_grow() {
    let Some(root) = locate_workspace_root() else {
        eprintln!("SKIP: UmaDev workspace files are absent (standalone packaged crate)");
        return;
    };

    for (relative, locked_lines) in HOTSPOT_LINES {
        let path = root.join(relative);
        let actual = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
            .lines()
            .count();
        match actual.cmp(locked_lines) {
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Less => panic!(
                "{relative} shrank from {locked_lines} to {actual} lines; lower its locked baseline to {actual} so it cannot regrow"
            ),
            std::cmp::Ordering::Greater => panic!(
                "{relative} grew from the locked {locked_lines} to {actual} lines; split/move code instead of raising the baseline"
            ),
        }
    }
}

#[test]
fn remediated_control_flow_hotspots_never_regress() {
    let Some(root) = locate_workspace_root() else {
        eprintln!("SKIP: UmaDev workspace files are absent (standalone packaged crate)");
        return;
    };
    let mut violations = Vec::new();
    for relative in CONTROL_FLOW_RATCHET_FILES {
        let path = root.join(relative);
        let content = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let decision = umadev_governance::rules::check_deep_nesting(relative, &content);
        if decision.block {
            violations.push(decision.reason);
        }
    }
    assert!(
        violations.is_empty(),
        "remediated control-flow hotspot(s) regressed:\n{}",
        violations.join("\n")
    );
}

#[test]
fn workspace_internal_production_dependencies_follow_allowlist() {
    let Some(root) = locate_workspace_root() else {
        eprintln!("SKIP: UmaDev workspace files are absent (standalone packaged crate)");
        return;
    };
    let model = WorkspaceModel::load(&root);
    assert_allowlisted(&model, NORMAL_ALLOWLIST, DependencyScope::is_production);
}

#[test]
fn workspace_internal_dev_dependencies_follow_separate_allowlist() {
    let Some(root) = locate_workspace_root() else {
        eprintln!("SKIP: UmaDev workspace files are absent (standalone packaged crate)");
        return;
    };
    let model = WorkspaceModel::load(&root);
    assert_allowlisted(&model, DEV_ALLOWLIST, DependencyScope::is_dev);
}

#[test]
fn foundation_crates_never_depend_on_application_layers() {
    let Some(root) = locate_workspace_root() else {
        eprintln!("SKIP: UmaDev workspace files are absent (standalone packaged crate)");
        return;
    };
    let model = WorkspaceModel::load(&root);
    let violations: Vec<String> = model
        .dependencies
        .iter()
        .filter(|d| {
            FOUNDATION_PACKAGES.contains(&d.from.as_str())
                && UPPER_PACKAGES.contains(&d.to.as_str())
        })
        .map(|d| format!("{} -> {} [{}]", d.from, d.to, d.scope.label()))
        .collect();
    assert!(
        violations.is_empty(),
        "lower-layer crate(s) depend on agent/TUI/binary:\n{}",
        violations.join("\n")
    );
}

#[test]
fn workspace_internal_production_dependency_graph_is_acyclic() {
    let Some(root) = locate_workspace_root() else {
        eprintln!("SKIP: UmaDev workspace files are absent (standalone packaged crate)");
        return;
    };
    let model = WorkspaceModel::load(&root);
    let edges: BTreeSet<(String, String)> = model
        .dependencies
        .iter()
        .filter(|d| d.scope.is_production())
        .map(|d| (d.from.clone(), d.to.clone()))
        .collect();

    let mut indegree: BTreeMap<String, usize> = model
        .packages
        .iter()
        .map(|package| (package.clone(), 0))
        .collect();
    let mut outgoing: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (from, to) in edges {
        assert_ne!(from, to, "workspace package {from} depends on itself");
        outgoing.entry(from).or_default().insert(to.clone());
        *indegree
            .get_mut(&to)
            .expect("dependency is a workspace member") += 1;
    }

    let mut ready: VecDeque<String> = indegree
        .iter()
        .filter_map(|(package, degree)| (*degree == 0).then_some(package.clone()))
        .collect();
    let mut visited = 0usize;
    while let Some(package) = ready.pop_front() {
        visited += 1;
        if let Some(targets) = outgoing.get(&package) {
            for target in targets {
                let degree = indegree
                    .get_mut(target)
                    .expect("dependency is a workspace member");
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(target.clone());
                }
            }
        }
    }

    let cyclic: Vec<&str> = indegree
        .iter()
        .filter_map(|(package, degree)| (*degree > 0).then_some(package.as_str()))
        .collect();
    assert_eq!(
        visited,
        model.packages.len(),
        "internal production dependency cycle involving: {}",
        cyclic.join(", ")
    );
}

#[test]
fn dependency_parser_distinguishes_dev_build_and_target_scopes() {
    let manifest: Value = toml::from_str(
        r#"
[package]
name = "scope-probe"

[dependencies]
umadev-spec = "1"

[build-dependencies]
runtime-alias = { workspace = true }

[dev-dependencies]
umadev-agent = "1"

[target.'cfg(unix)'.dependencies]
umadev-host = "1"

[target.'cfg(windows)'.build-dependencies]
umadev-i18n = "1"

[target.'cfg(target_os = "macos")'.dev-dependencies]
umadev-tui = "1"
"#,
    )
    .expect("scope fixture must parse");
    let internal: BTreeSet<String> = [
        "umadev-agent",
        "umadev-host",
        "umadev-i18n",
        "umadev-runtime",
        "umadev-spec",
        "umadev-tui",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let workspace_dependencies =
        BTreeMap::from([("runtime-alias".to_string(), "umadev-runtime".to_string())]);
    let got: BTreeSet<(String, String)> =
        dependencies_from_manifest("scope-probe", &manifest, &internal, &workspace_dependencies)
            .into_iter()
            .map(|d| (d.to, d.scope.label()))
            .collect();
    let expected: BTreeSet<(String, String)> = [
        ("umadev-agent", "dev-dependencies"),
        ("umadev-host", "target.cfg(unix).dependencies"),
        ("umadev-i18n", "target.cfg(windows).build-dependencies"),
        ("umadev-runtime", "build-dependencies"),
        ("umadev-spec", "dependencies"),
        (
            "umadev-tui",
            "target.cfg(target_os = \"macos\").dev-dependencies",
        ),
    ]
    .into_iter()
    .map(|(package, scope)| (package.to_string(), scope.to_string()))
    .collect();
    assert_eq!(got, expected);
}
