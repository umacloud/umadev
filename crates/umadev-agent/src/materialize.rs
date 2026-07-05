//! Two-layer artifact materialization (item A of the team-interaction hardening).
//!
//! The blackboard docs (`output/*-{architecture,uiux,prd}.md`) carry DECISIONS in
//! prose. This module parses those decisions out into typed, machine-checkable
//! contracts emitted to `.umadev/contracts/` — the "what" layer (data model /
//! design tokens / acceptance criteria) sitting next to the "why" layer (the
//! prose). It mirrors how `umadev-contract` already derives the API contract from
//! the architecture doc's API table, extending the same pattern to the three
//! remaining derived contracts named by [`crate::critics::ArtifactKind`].
//!
//! Everything here is lightweight (markdown-section heuristics — no heavy parser
//! dep) and **fail-open**: an absent section yields an empty typed contract, never
//! an error, and a write failure is swallowed (materialization is an audit/precision
//! aid, never a gate).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// One entity in the materialized data model (a table / struct the app persists).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataModelEntity {
    /// The entity name (e.g. `User`, `Order`).
    pub name: String,
    /// Its declared fields, in source order.
    pub fields: Vec<String>,
}

/// The full set of typed contracts materialized from the blackboard docs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedContracts {
    /// Entities parsed from the architecture doc's data-model section.
    pub data_model: Vec<DataModelEntity>,
    /// `token -> value` pairs parsed from the UIUX doc's design-tokens section.
    pub design_tokens: Vec<(String, String)>,
    /// Acceptance-criteria lines parsed from the PRD's acceptance section.
    pub acceptance: Vec<String>,
}

/// Extract the body lines of the FIRST markdown section whose `#`-heading contains
/// any of `keys` (case-insensitive), up to the next heading of the same-or-higher
/// level. `None` when no matching section exists.
fn section_body<'a>(md: &'a str, keys: &[&str]) -> Option<Vec<&'a str>> {
    let lines: Vec<&str> = md.lines().collect();
    for (i, raw) in lines.iter().enumerate() {
        let l = raw.trim_start();
        if !l.starts_with('#') {
            continue;
        }
        let heading = l.trim_start_matches('#').trim().to_ascii_lowercase();
        if !keys
            .iter()
            .any(|k| heading.contains(&k.to_ascii_lowercase()))
        {
            continue;
        }
        let level = l.chars().take_while(|c| *c == '#').count();
        let mut body = Vec::new();
        for lj in &lines[i + 1..] {
            let t = lj.trim_start();
            if t.starts_with('#') {
                let lvl = t.chars().take_while(|c| *c == '#').count();
                if lvl <= level {
                    break;
                }
            }
            body.push(*lj);
        }
        return Some(body);
    }
    None
}

/// A bullet line's payload (`- x` / `* x` / `1. x`), or `None` if not a bullet.
fn bullet_payload(line: &str) -> Option<String> {
    let t = line.trim();
    let rest = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
        .or_else(|| {
            // ordered list "1. ..." / "1) ..."
            let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                return None;
            }
            let after = &t[digits.len()..];
            after
                .strip_prefix(". ")
                .or_else(|| after.strip_prefix(") "))
        })?;
    let cleaned = rest.replace("**", "").trim().to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Parse the data model from the architecture doc's data-model section. Each bullet
/// of the form `Entity: field, field, …` becomes a [`DataModelEntity`].
#[must_use]
pub fn parse_data_model(architecture_md: &str) -> Vec<DataModelEntity> {
    let Some(body) = section_body(
        architecture_md,
        &[
            "data model",
            "数据模型",
            "数据模型设计",
            "data schema",
            "schema",
        ],
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in body {
        let Some(payload) = bullet_payload(line) else {
            continue;
        };
        let Some((name, fields)) = payload.split_once(':') else {
            continue;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let fields = fields
            .split(',')
            .map(|f| f.trim().to_string())
            .filter(|f| !f.is_empty())
            .collect();
        out.push(DataModelEntity { name, fields });
    }
    out
}

/// Parse design tokens from the UIUX doc's design-tokens section. Each bullet of the
/// form `token: value` (or `token = value`) becomes a `(token, value)` pair.
#[must_use]
pub fn parse_design_tokens(uiux_md: &str) -> Vec<(String, String)> {
    let Some(body) = section_body(
        uiux_md,
        &[
            "design token",
            "design-token",
            "设计令牌",
            "design system tokens",
            "tokens",
        ],
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in body {
        let Some(payload) = bullet_payload(line) else {
            continue;
        };
        let split = payload.split_once(':').or_else(|| payload.split_once('='));
        if let Some((k, val)) = split {
            let k = k.trim().to_string();
            let val = val.trim().to_string();
            if !k.is_empty() && !val.is_empty() {
                out.push((k, val));
            }
        }
    }
    out
}

/// Parse acceptance criteria from the PRD's acceptance section — each bullet is one
/// criterion.
#[must_use]
pub fn parse_acceptance(prd_md: &str) -> Vec<String> {
    let Some(body) = section_body(
        prd_md,
        &[
            "acceptance",
            "验收",
            "验收标准",
            "acceptance criteria",
            "验收条件",
        ],
    ) else {
        return Vec::new();
    };
    body.iter().filter_map(|l| bullet_payload(l)).collect()
}

/// Read the blackboard docs for `slug` and materialize the derived contracts,
/// emitting `.umadev/contracts/derived-contracts.json`. Returns the parsed
/// contracts. Fail-open: unreadable docs → empty contracts; a write error is
/// swallowed.
#[must_use]
pub fn materialize(project_root: &Path, slug: &str) -> DerivedContracts {
    let read = |name: &str| {
        std::fs::read_to_string(project_root.join(format!("output/{slug}-{name}.md")))
            .unwrap_or_default()
    };
    let contracts = DerivedContracts {
        data_model: parse_data_model(&read("architecture")),
        design_tokens: parse_design_tokens(&read("uiux")),
        acceptance: parse_acceptance(&read("prd")),
    };
    let dir = project_root.join(".umadev").join("contracts");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(&contracts) {
        let _ = std::fs::write(dir.join("derived-contracts.json"), json);
    }
    contracts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_data_model_entities() {
        let arch = "\
# Architecture

## Data Model
- User: id, email, name
- **Order**: id, user_id, total, status

## API
- GET /users
";
        let dm = parse_data_model(arch);
        assert_eq!(dm.len(), 2);
        assert_eq!(dm[0].name, "User");
        assert_eq!(dm[0].fields, vec!["id", "email", "name"]);
        assert_eq!(dm[1].name, "Order");
        assert!(dm[1].fields.contains(&"status".to_string()));
        // The API section (a same-level heading) bounds the data-model section.
        assert!(dm.iter().all(|e| e.name != "GET /users"));
    }

    #[test]
    fn parses_design_tokens_and_acceptance() {
        let uiux = "\
## Design Tokens
- color.primary: #0B5FFF
- spacing.md = 16px
";
        let tokens = parse_design_tokens(uiux);
        assert_eq!(tokens.len(), 2);
        assert_eq!(
            tokens[0],
            ("color.primary".to_string(), "#0B5FFF".to_string())
        );
        assert_eq!(tokens[1], ("spacing.md".to_string(), "16px".to_string()));

        let prd = "\
## 验收标准
- 用户能用邮箱注册并收到验证邮件
- 登录失败三次后锁定十分钟
";
        let ac = parse_acceptance(prd);
        assert_eq!(ac.len(), 2);
        assert!(ac[0].contains("邮箱注册"));
    }

    #[test]
    fn absent_section_yields_empty_contract() {
        assert!(parse_data_model("# Arch\n\nno data model here").is_empty());
        assert!(parse_design_tokens("").is_empty());
        assert!(parse_acceptance("## Other\n- x").is_empty());
    }

    #[test]
    fn materialize_writes_a_typed_contract_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        std::fs::write(
            root.join("output").join("app-architecture.md"),
            "## Data Model\n- User: id, email\n",
        )
        .unwrap();
        let c = materialize(root, "app");
        assert_eq!(c.data_model.len(), 1);
        // The typed contract is emitted to disk next to the prose.
        let emitted = root
            .join(".umadev")
            .join("contracts")
            .join("derived-contracts.json");
        assert!(emitted.exists());
        let round: DerivedContracts =
            serde_json::from_str(&std::fs::read_to_string(emitted).unwrap()).unwrap();
        assert_eq!(round.data_model[0].name, "User");
    }
}
