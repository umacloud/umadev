//! Read-only discovery of model, reasoning, and context settings owned by base CLIs.

use std::path::PathBuf;

use crate::config;

/// Base CLI ids exposed by the TUI. Only product-supported, end-to-end adapted
/// bases may appear in configuration, the picker, commands, or session startup.
pub(crate) const FIRST_CLASS_BACKEND_IDS: [&str; 5] = [
    "claude-code",
    "codex",
    "opencode",
    "grok-build",
    "kimi-code",
];

/// Read the model the BASE is configured to use, in the base's OWN resolution
/// order, purely to DISPLAY which model the Agent runs on — UmaDev owns no
/// model and never sets one; the base's model IS the engine. Returns `None` when
/// the base pins no explicit model in config (it then runs on its login / server
/// default, which UmaDev does not override). Read-only observation, never a
/// write. Fail-open throughout.
#[must_use]
pub fn detect_base_model(backend_id: &str, project_root: &std::path::Path) -> Option<String> {
    let home = config::home_dir();
    match backend_id {
        // claude: --model > ANTHROPIC_MODEL > project/user .claude/settings.json.
        "claude-code" => {
            if let Ok(m) = std::env::var("ANTHROPIC_MODEL") {
                let m = m.trim();
                if !m.is_empty() {
                    return Some(m.to_string());
                }
            }
            json_top_string(&project_root.join(".claude/settings.json"), "model").or_else(|| {
                home.as_ref()
                    .and_then(|h| json_top_string(&h.join(".claude/settings.json"), "model"))
            })
        }
        // codex: project/user .codex/config.toml `model` (then `default_model`).
        "codex" => {
            let proj = project_root.join(".codex/config.toml");
            let user = home.as_ref().map(|h| h.join(".codex/config.toml"));
            ["model", "default_model"].into_iter().find_map(|k| {
                toml_top_string(&proj, k)
                    .or_else(|| user.as_ref().and_then(|u| toml_top_string(u, k)))
            })
        }
        // opencode: project/user/env opencode config `model` (format provider/model).
        "opencode" => opencode_config_values(project_root, home.as_deref())
            .into_iter()
            .find_map(|v| opencode_model_from_config(&v)),
        // Kimi: an environment-defined temporary model wins over the
        // base-owned config, exactly matching Kimi Code's documented order.
        "kimi-code" => non_empty_env("KIMI_MODEL_NAME").or_else(|| {
            kimi_config(home.as_deref())?
                .get("default_model")?
                .as_str()
                .map(str::to_string)
        }),
        // Grok Build: `.grok/config.toml` `[models] default` — the model used for
        // new sessions (docs.x.ai/build/settings/reference). UmaDev may pin `--model`
        // at launch, but when it does not, THIS is the model the base runs on.
        "grok-build" => grok_config(project_root, home.as_deref())?
            .get("models")?
            .get("default")?
            .as_str()
            .map(str::to_string),
        _ => None,
    }
}

/// Read the active base's configured context window when the base config exposes
/// an exact value. Today this is mainly OpenCode's provider model catalog
/// (`provider.<id>.models.<model>.limit.context`). Fail-open: if the shape is
/// absent or unfamiliar, callers fall back to the model-name estimate.
#[must_use]
pub fn detect_base_context_window(backend_id: &str, project_root: &std::path::Path) -> Option<u64> {
    if backend_id == "kimi-code" {
        if non_empty_env("KIMI_MODEL_NAME").is_some() {
            return non_empty_env("KIMI_MODEL_MAX_CONTEXT_SIZE")?.parse().ok();
        }
        let home = config::home_dir();
        let value = kimi_config(home.as_deref())?;
        let model = value.get("default_model")?.as_str()?;
        return kimi_context_for_model(&value, model);
    }
    if backend_id == "grok-build" {
        let home = config::home_dir();
        let value = grok_config(project_root, home.as_deref())?;
        let model = value.get("models")?.get("default")?.as_str()?;
        return grok_context_for_model(&value, model);
    }
    if backend_id != "opencode" {
        return None;
    }
    let home = config::home_dir();
    let values = opencode_config_values(project_root, home.as_deref());
    let model = values.iter().find_map(opencode_model_from_config)?;
    values
        .iter()
        .find_map(|v| opencode_context_for_model(v, &model))
}

/// Read an exact context window for a specific live model report, but only from
/// base-owned provider metadata. This is deliberately narrower than a model-name
/// table: if the selected OpenCode model cannot be matched to a configured
/// provider catalog entry, callers must hide the denominator.
#[must_use]
pub fn detect_base_context_window_for_model(
    backend_id: &str,
    project_root: &std::path::Path,
    model: &str,
) -> Option<u64> {
    if backend_id == "kimi-code" {
        if non_empty_env("KIMI_MODEL_NAME").is_some() {
            return non_empty_env("KIMI_MODEL_MAX_CONTEXT_SIZE")?.parse().ok();
        }
        let home = config::home_dir();
        return kimi_context_for_model(&kimi_config(home.as_deref())?, model);
    }
    if backend_id == "grok-build" {
        let home = config::home_dir();
        return grok_context_for_model(&grok_config(project_root, home.as_deref())?, model.trim());
    }
    if backend_id != "opencode" {
        return None;
    }
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    let home = config::home_dir();
    opencode_config_values(project_root, home.as_deref())
        .into_iter()
        .find_map(|v| opencode_context_for_model(&v, model))
}

/// Read the reasoning / thinking effort the BASE is configured with, so UmaDev
/// can SHOW it next to the driving model. UmaDev never overrides it — the base
/// runs at its own effort, just like its own model. `None` when the base pins no
/// explicit effort (opencode encodes effort in the model variant, so it has no
/// separate field). Fail-open throughout.
#[must_use]
pub fn detect_base_reasoning(backend_id: &str, project_root: &std::path::Path) -> Option<String> {
    let home = config::home_dir();
    match backend_id {
        // claude: settings.json `effortLevel` (project wins over user).
        "claude-code" => json_top_string(
            &project_root.join(".claude/settings.json"),
            "effortLevel",
        )
        .or_else(|| {
            home.as_ref()
                .and_then(|h| json_top_string(&h.join(".claude/settings.json"), "effortLevel"))
        }),
        // codex: config.toml `model_reasoning_effort`.
        "codex" => {
            let proj = project_root.join(".codex/config.toml");
            let user = home.as_ref().map(|h| h.join(".codex/config.toml"));
            toml_top_string(&proj, "model_reasoning_effort").or_else(|| {
                user.as_ref()
                    .and_then(|u| toml_top_string(u, "model_reasoning_effort"))
            })
        }
        "kimi-code" => non_empty_env("KIMI_MODEL_THINKING_EFFORT").or_else(|| {
            let value = kimi_config(home.as_deref())?;
            if let Some(effort) = value
                .get("thinking")
                .and_then(|v| v.get("effort"))
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                return Some(effort.to_string());
            }
            value
                .get("thinking")
                .and_then(|v| v.get("enabled"))
                .and_then(toml::Value::as_bool)
                .map(|enabled| if enabled { "on" } else { "off" }.to_string())
        }),
        // Grok Build: `.grok/config.toml` `[models] default_reasoning_effort` — the
        // default effort for the default model (docs.x.ai/build/settings/reference).
        "grok-build" => grok_config(project_root, home.as_deref())?
            .get("models")?
            .get("default_reasoning_effort")?
            .as_str()
            .map(str::to_string),
        // opencode: effort is baked into the model variant — no separate field.
        _ => None,
    }
}

/// The base config a FULL session re-establish RE-DETECTS before the bounded
/// auto-recovery retry re-opens the session — the equivalent of what a manual
/// `/codex` re-select does, driven automatically when a live session goes stale.
///
/// The reported failure mode: the user changes the base's UPSTREAM config (a proxy
/// tier / reasoning-effort / model switch in their Codex/ChatGPT proxy) mid-session,
/// so the running session's START-time model + negotiated capabilities fall out of
/// sync and every turn errors. Reopening while REUSING the cached model / RESUMING
/// the same session just re-enters the stale state. Re-detecting here mirrors the
/// `/codex` path: the caller reopens COLD (no resume) so `initialize` re-negotiates
/// capabilities from scratch, and this supplies the base's freshly-read model +
/// reasoning effort for that reopen and for the refreshed status display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReestablishConfig {
    /// The base's freshly RE-DETECTED model, or the caller's fallback when the base
    /// pins none (its own login default still drives it — UmaDev owns no model).
    pub(crate) model: String,
    /// The base's freshly RE-DETECTED reasoning effort to surface, or `None` when the
    /// base pins none — so a stale effort is never shown.
    pub(crate) reasoning: Option<String>,
}

/// RE-DETECT the base's model + reasoning effort for a full session re-establish.
///
/// Fail-open: a base that pins no explicit model keeps `fallback_model`; a base with
/// no explicit effort yields `reasoning == None`. Read-only config reads only; never
/// panics.
#[must_use]
pub(crate) fn redetect_base_config(
    backend_id: &str,
    project_root: &std::path::Path,
    fallback_model: &str,
) -> ReestablishConfig {
    let model = detect_base_model(backend_id, project_root)
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| fallback_model.to_string());
    let reasoning = detect_base_reasoning(backend_id, project_root)
        .map(|effort| effort.trim().to_string())
        .filter(|effort| !effort.is_empty());
    ReestablishConfig { model, reasoning }
}

/// Read a top-level string field from a JSON config file (fail-open `None`).
fn json_top_string(path: &std::path::Path, key: &str) -> Option<String> {
    let v = json_value(path)?;
    v.get(key)?.as_str().map(str::to_string)
}

fn opencode_config_paths(
    project_root: &std::path::Path,
    home: Option<&std::path::Path>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // OpenCode merges OPENCODE_CONFIG_CONTENT last; handled separately by
    // `opencode_config_values`. OPENCODE_CONFIG_DIR is the highest-priority file
    // directory and still works when project config is disabled.
    if let Ok(dir) = std::env::var("OPENCODE_CONFIG_DIR") {
        let dir = dir.trim();
        if !dir.is_empty() {
            paths.push(PathBuf::from(dir).join("opencode.jsonc"));
            paths.push(PathBuf::from(dir).join("opencode.json"));
        }
    }
    let project_disabled = std::env::var("OPENCODE_DISABLE_PROJECT_CONFIG").is_ok_and(|v| {
        let v = v.trim();
        v == "1" || v.eq_ignore_ascii_case("true")
    });
    if !project_disabled {
        paths.extend(opencode_project_config_paths(project_root));
    }
    if let Ok(file) = std::env::var("OPENCODE_CONFIG") {
        let file = file.trim();
        if !file.is_empty() {
            paths.push(PathBuf::from(file));
        }
    }
    if let Some(home) = home {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            let xdg = xdg.trim();
            if !xdg.is_empty() {
                paths.push(PathBuf::from(xdg).join("opencode/opencode.jsonc"));
                paths.push(PathBuf::from(xdg).join("opencode/opencode.json"));
                paths.push(PathBuf::from(xdg).join("opencode/config.json"));
            }
        }
        paths.extend([
            home.join(".config/opencode/opencode.jsonc"),
            home.join(".config/opencode/opencode.json"),
            home.join(".config/opencode/config.json"),
            home.join(".opencode/opencode.jsonc"),
            home.join(".opencode/opencode.json"),
        ]);
    }
    paths
}

fn opencode_project_config_paths(project_root: &std::path::Path) -> Vec<PathBuf> {
    let dirs = opencode_project_config_dirs(project_root);
    let mut paths = Vec::new();
    for dir in &dirs {
        paths.push(dir.join(".opencode/opencode.jsonc"));
        paths.push(dir.join(".opencode/opencode.json"));
    }
    for dir in dirs {
        paths.push(dir.join("opencode.jsonc"));
        paths.push(dir.join("opencode.json"));
    }
    paths
}

fn opencode_project_config_dirs(project_root: &std::path::Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut current = Some(project_root);
    while let Some(dir) = current {
        dirs.push(dir.to_path_buf());
        if dir.join(".git").exists() || dir.join(".umadev").exists() {
            break;
        }
        current = dir.parent().filter(|p| *p != dir);
    }
    dirs
}

fn opencode_config_values(
    project_root: &std::path::Path,
    home: Option<&std::path::Path>,
) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    if let Ok(content) = std::env::var("OPENCODE_CONFIG_CONTENT") {
        if let Some(v) = json_text_value(&content) {
            values.push(v);
        }
    }
    values.extend(
        opencode_config_paths(project_root, home)
            .into_iter()
            .filter_map(|p| json_value(&p)),
    );
    values
}

fn json_value(path: &std::path::Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    json_text_value(&text)
}

fn json_text_value(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(text).ok().or_else(|| {
        let stripped = strip_jsonc_comments(text);
        serde_json::from_str(&stripped)
            .ok()
            .or_else(|| serde_json::from_str(&remove_json_trailing_commas(&stripped)).ok())
    })
}

fn opencode_model_from_config(v: &serde_json::Value) -> Option<String> {
    if let Some(model) = v.get("model").and_then(serde_json::Value::as_str) {
        let model = model.trim();
        if !model.is_empty() {
            return Some(model.to_string());
        }
    }
    if let Some(model) = v.get("model").and_then(opencode_model_ref) {
        return Some(model);
    }
    opencode_model_ref(v)
}

fn opencode_model_ref(v: &serde_json::Value) -> Option<String> {
    let model_id = v
        .get("modelID")
        .or_else(|| v.get("model_id"))
        .or_else(|| v.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let provider_id = v
        .get("providerID")
        .or_else(|| v.get("provider_id"))
        .or_else(|| v.get("provider"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let base = match provider_id {
        Some(provider) if model_id.starts_with(&format!("{provider}/")) => model_id.to_string(),
        Some(provider) => format!("{provider}/{model_id}"),
        None => model_id.to_string(),
    };
    let variant = v
        .get("variant")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "default");
    Some(match variant {
        Some(variant) => format!("{base}/{variant}"),
        None => base,
    })
}

fn opencode_context_for_model(v: &serde_json::Value, model: &str) -> Option<u64> {
    let (provider_id, model_id) = model
        .split_once('/')
        .map_or((None, model), |(provider, id)| (Some(provider), id));
    let providers = v
        .get("provider")
        .or_else(|| v.get("providers"))?
        .as_object()?;
    if let Some(provider_id) = provider_id {
        if let Some(limit) = providers
            .get(provider_id)
            .and_then(|provider| provider_model_context(provider, model_id))
        {
            return Some(limit);
        }
    }
    providers
        .values()
        .find_map(|provider| provider_model_context(provider, model_id))
}

fn provider_model_context(provider: &serde_json::Value, model_id: &str) -> Option<u64> {
    let models = provider.get("models")?.as_object()?;
    models
        .get(model_id)
        .and_then(model_context_limit)
        .or_else(|| {
            models.iter().find_map(|(key, entry)| {
                key.eq_ignore_ascii_case(model_id)
                    .then(|| model_context_limit(entry))
                    .flatten()
            })
        })
        .or_else(|| {
            let (base_id, variant) = model_id.rsplit_once('/')?;
            model_context_for_variant(models, base_id, variant)
        })
}

fn model_context_for_variant(
    models: &serde_json::Map<String, serde_json::Value>,
    base_id: &str,
    variant: &str,
) -> Option<u64> {
    models
        .get(base_id)
        .and_then(|entry| {
            model_entry_has_variant(entry, variant)
                .then(|| model_context_limit(entry))
                .flatten()
        })
        .or_else(|| {
            models.iter().find_map(|(key, entry)| {
                (key.eq_ignore_ascii_case(base_id) && model_entry_has_variant(entry, variant))
                    .then(|| model_context_limit(entry))
                    .flatten()
            })
        })
}

fn model_entry_has_variant(entry: &serde_json::Value, variant: &str) -> bool {
    let Some(variants) = entry.get("variants") else {
        return false;
    };
    variants
        .as_object()
        .is_some_and(|map| map.contains_key(variant))
        || variants.as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item.as_str() == Some(variant)
                    || item.get("id").and_then(serde_json::Value::as_str) == Some(variant)
            })
        })
}

fn model_context_limit(entry: &serde_json::Value) -> Option<u64> {
    entry
        .pointer("/limit/context")
        .and_then(json_u64)
        .or_else(|| entry.pointer("/limits/context").and_then(json_u64))
        .or_else(|| entry.get("context").and_then(json_u64))
        .or_else(|| entry.get("context_window").and_then(json_u64))
        .or_else(|| entry.get("contextWindow").and_then(json_u64))
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64().or_else(|| {
        v.as_str()
            .map(|s| s.replace(['_', ','], ""))
            .and_then(|s| s.parse::<u64>().ok())
    })
}

fn strip_jsonc_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == '/' {
            match chars.peek().copied() {
                Some('/') => {
                    let _ = chars.next();
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push('\n');
                            break;
                        }
                    }
                    continue;
                }
                Some('*') => {
                    let _ = chars.next();
                    let mut prev = '\0';
                    for next in chars.by_ref() {
                        if next == '\n' {
                            out.push('\n');
                        }
                        if prev == '*' && next == '/' {
                            break;
                        }
                        prev = next;
                    }
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

fn remove_json_trailing_commas(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut in_string = false;
    let mut escaped = false;
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            continue;
        }
        if c == ',' {
            let next = chars[i + 1..].iter().find(|ch| !ch.is_whitespace());
            if matches!(next, Some('}' | ']')) {
                continue;
            }
        }
        out.push(c);
    }
    out
}

/// Read a root string field from a TOML config file (fail-open `None`).
fn toml_top_string(path: &std::path::Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: toml::Value = toml::from_str(&text).ok()?;
    v.get(key)?.as_str().map(str::to_string)
}

fn non_empty_env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn kimi_config(home: Option<&std::path::Path>) -> Option<toml::Value> {
    let path = non_empty_env("KIMI_CODE_HOME")
        .map(PathBuf::from)
        .or_else(|| home.map(|home| home.join(".kimi-code")))?
        .join("config.toml");
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str(&text).ok()
}

/// Parse Grok Build's `.grok/config.toml` (a project-root override first, then the
/// user's `~/.grok/config.toml`) into a TOML value, purely to DISPLAY the model /
/// effort / context the base runs on. Fail-open: an absent / unreadable / malformed
/// file at either location yields `None`, never a panic.
fn grok_config(project_root: &std::path::Path, home: Option<&std::path::Path>) -> Option<toml::Value> {
    let candidates = [
        Some(project_root.join(".grok/config.toml")),
        home.map(|h| h.join(".grok/config.toml")),
    ];
    candidates
        .into_iter()
        .flatten()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .find_map(|text| toml::from_str::<toml::Value>(&text).ok())
}

/// Read Grok Build's exact context window for `model` from its config: the
/// `[model."<id>"] context_window` field (docs.x.ai/build/settings/reference).
/// Fail-open: an absent / non-integer / oversized value yields `None`.
fn grok_context_for_model(value: &toml::Value, model: &str) -> Option<u64> {
    value
        .get("model")?
        .get(model)?
        .get("context_window")
        .and_then(toml::Value::as_integer)
        .and_then(|n| u64::try_from(n).ok())
}

fn kimi_context_for_model(value: &toml::Value, model: &str) -> Option<u64> {
    let entry = value.get("models")?.get(model)?;
    entry
        .get("overrides")
        .and_then(|overrides| overrides.get("max_context_size"))
        .and_then(toml::Value::as_integer)
        .or_else(|| {
            entry
                .get("max_context_size")
                .and_then(toml::Value::as_integer)
        })
        .and_then(|size| u64::try_from(size).ok())
        .filter(|size| *size > 0)
}

#[cfg(test)]
mod reestablish_tests {
    use super::redetect_base_config;

    #[test]
    fn redetect_reads_the_base_config_live_not_a_cached_value() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".codex")).unwrap();
        std::fs::write(
            root.join(".codex/config.toml"),
            "model = \"gpt-5.6\"\nmodel_reasoning_effort = \"high\"\n",
        )
        .unwrap();

        // The cached (start-time) model is deliberately different: a re-establish must
        // RE-DETECT from the base's own config, not echo the stale cached value.
        let re = redetect_base_config("codex", root, "stale-cached-model");
        assert_eq!(re.model, "gpt-5.6");
        assert_eq!(re.reasoning.as_deref(), Some("high"));

        // The user raises the upstream effort (the reported flow). A fresh re-detect
        // must pick up the CHANGED value — proving the config is read live, not cached.
        std::fs::write(
            root.join(".codex/config.toml"),
            "model = \"gpt-5.6\"\nmodel_reasoning_effort = \"xhigh\"\n",
        )
        .unwrap();
        let re = redetect_base_config("codex", root, "stale-cached-model");
        assert_eq!(
            re.reasoning.as_deref(),
            Some("xhigh"),
            "the CHANGED effort is surfaced after re-establish, not the start-time value"
        );
    }

    #[test]
    fn redetect_is_fail_open_when_the_base_pins_no_model_or_effort() {
        // An unknown/offline base pins nothing and reads no home config: the fallback
        // model is kept and no effort is shown (never a fabricated/stale one).
        let tmp = tempfile::TempDir::new().unwrap();
        let re = redetect_base_config("offline", tmp.path(), "fallback-model");
        assert_eq!(re.model, "fallback-model");
        assert_eq!(re.reasoning, None);
    }
}

#[cfg(test)]
mod grok_tests {
    use super::{detect_base_context_window, detect_base_model, detect_base_reasoning};

    #[test]
    fn reads_model_effort_and_context_from_grok_config_toml() {
        // Grok Build's `.grok/config.toml` schema (docs.x.ai/build/settings/reference):
        // `[models] default` + `default_reasoning_effort`, and a per-model
        // `[model."<id>"] context_window`. A project-root override is read first, so
        // this is hermetic regardless of the dev machine's own `~/.grok/config.toml`.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".grok")).unwrap();
        std::fs::write(
            root.join(".grok/config.toml"),
            "[models]\ndefault = \"grok-4.5\"\ndefault_reasoning_effort = \"high\"\n\n\
             [model.\"grok-4.5\"]\ncontext_window = 1000000\n",
        )
        .unwrap();

        assert_eq!(
            detect_base_model("grok-build", root).as_deref(),
            Some("grok-4.5"),
            "the [models] default model is surfaced for Grok, like every other base"
        );
        assert_eq!(
            detect_base_reasoning("grok-build", root).as_deref(),
            Some("high"),
            "the [models] default_reasoning_effort is surfaced for Grok"
        );
        assert_eq!(
            detect_base_context_window("grok-build", root),
            Some(1_000_000),
            "the [model.<id>] context_window is surfaced for Grok"
        );
    }

    #[test]
    fn grok_config_with_only_mcp_servers_is_fail_open_none() {
        // A real `.grok/config.toml` may hold ONLY `[mcp_servers.*]` (that is all
        // UmaDev writes into it) with no `[models]` — that must yield None (no
        // fabricated model / effort / context), never a panic.
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".grok")).unwrap();
        std::fs::write(
            root.join(".grok/config.toml"),
            "[mcp_servers.example]\ncommand = \"x\"\n",
        )
        .unwrap();
        assert_eq!(detect_base_model("grok-build", root), None);
        assert_eq!(detect_base_reasoning("grok-build", root), None);
        assert_eq!(detect_base_context_window("grok-build", root), None);
    }
}

#[cfg(test)]
mod kimi_tests {
    use super::kimi_context_for_model;

    #[test]
    fn context_uses_the_selected_alias_and_persistent_override() {
        let value: toml::Value = toml::from_str(
            r#"
default_model = "kimi-code/kimi-for-coding"

[models."kimi-code/kimi-for-coding"]
max_context_size = 262144

[models."kimi-code/kimi-for-coding".overrides]
max_context_size = 131072
"#,
        )
        .unwrap();
        assert_eq!(
            kimi_context_for_model(&value, "kimi-code/kimi-for-coding"),
            Some(131_072)
        );
        assert_eq!(kimi_context_for_model(&value, "missing"), None);
    }
}
