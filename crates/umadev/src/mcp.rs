//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! Exposes UmaDev's governance + director layer as a `tools/call` target so ANY
//! MCP-compatible host (Claude Desktop, Cursor, Continue, etc.) can ask
//! "is this file content safe to write?" — and now also query the live plan, the
//! frontend↔backend contract, and the self-evolving pitfall memory — turning
//! UmaDev into a **portable governance gateway** for the whole MCP ecosystem,
//! not just Claude Code's PreToolUse hook.
//!
//! ## Protocol
//! JSON-RPC 2.0 over stdio, one request per line. UmaDev implements:
//! - `initialize` → server capabilities
//! - `tools/list` → the governance tools (`govern_file` / `govern_command`)
//!   plus the read-mostly director tools (`plan_status` / `contract_check` /
//!   `lessons_recall` / `governance_summary`)
//! - `tools/call` → run the named tool over a project root
//!
//! ## Safety contract
//! The two `govern_*` tools are content-scoped pure checks. The four director
//! tools are **read-mostly + fail-open by contract**: each calls an existing
//! pure agent/contract entry point over the project root and, on ANY failure
//! (missing/corrupt artifact, unreadable dir), returns an empty / "unavailable"
//! result — never a crash, never a workspace mutation. The heavy, mutating work
//! (running tests, writing files, driving a build) stays behind the TUI/CLI and
//! the trust floor; the MCP surface is a safe query window into UmaDev's state.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

use umadev_governance::{
    check_dangerous_bash, pre_write_floor_decision, scan_content_with_policy, Policy,
};

/// Tool name: govern a file's proposed content.
const TOOL_GOVERN_FILE: &str = "govern_file";
/// Tool name: govern a shell command before execution.
const TOOL_GOVERN_COMMAND: &str = "govern_command";
/// Tool name: read the owned, persisted build plan (`.umadev/plan.json`).
const TOOL_PLAN_STATUS: &str = "plan_status";
/// Tool name: cross-check frontend calls against the API contract (read-only).
const TOOL_CONTRACT_CHECK: &str = "contract_check";
/// Tool name: recall recorded pitfalls + corrective strategies from memory.
const TOOL_LESSONS_RECALL: &str = "lessons_recall";
/// Tool name: summarise the active rule policy + the audit-trail tail.
const TOOL_GOVERNANCE_SUMMARY: &str = "governance_summary";

/// Maximum bytes read for one JSON-RPC line. A stream that never sends a
/// newline (or a single pathologically large line) is capped here rather than
/// buffered without bound; the over-long chunk is answered with a parse error
/// and the loop resynchronises to the next newline. Generous — a real
/// JSON-RPC request is kilobytes, not megabytes.
const MAX_LINE_BYTES: u64 = 1 << 20;

/// Run the MCP server loop: read JSON-RPC requests from stdin, write
/// responses to stdout. Runs until stdin closes (EOF) or `shutdown` arrives.
///
/// # Errors
/// Returns an error only on a stdout write failure (a broken pipe); malformed
/// input lines are answered with a JSON-RPC error (the protocol's fail-open).
pub fn serve() -> io::Result<()> {
    let project_root = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let policy = Policy::load(&project_root);
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut out = stdout.lock();
    serve_io(&mut reader, &mut out, &policy)
}

/// The JSON-RPC framing + dispatch loop over an arbitrary reader/writer. Split
/// out from [`serve`] so the line framing is unit-testable without a real
/// stdin.
///
/// Reads one line at a time on the BYTE level (`read_until`), so:
/// - an **invalid-UTF-8** line no longer ends the session — it is lossily
///   decoded and answered with a parse error, then the loop continues (the old
///   `BufRead::lines()` yielded `Err` on bad UTF-8 → the whole session died);
/// - a line is **size-capped** at [`MAX_LINE_BYTES`], so a newline-less stream
///   can't grow memory without bound; an over-long line is answered as a parse
///   error and the loop drains to the next newline to resynchronise.
fn serve_io<R: BufRead, W: Write>(reader: &mut R, out: &mut W, policy: &Policy) -> io::Result<()> {
    let mut raw: Vec<u8> = Vec::new();
    loop {
        raw.clear();
        let n = match (&mut *reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut raw)
        {
            // EOF (stdin closed) or a genuine stdin I/O failure — end the loop.
            Ok(0) | Err(_) => break,
            Ok(n) => n, // a (possibly capped) line, including any '\n'.
        };
        // We stopped at the cap (not a newline) → the line was longer than
        // MAX_LINE_BYTES and is truncated. Answer + drain to the next newline.
        let hit_cap = n as u64 >= MAX_LINE_BYTES && raw.last() != Some(&b'\n');

        // Lossy decode: one bad byte answers a parse error, never a crash/exit.
        let decoded = String::from_utf8_lossy(&raw);
        let line = decoded.trim();
        if line.is_empty() && !hit_cap {
            continue;
        }
        if let Some(resp) = build_response(line, policy) {
            let serialized = serde_json::to_string(&resp).unwrap_or_default();
            writeln!(out, "{serialized}")?;
            out.flush()?;
        }
        // A `shutdown` request ends the loop AFTER its response is written, so a
        // client blocked on the reply gets it and the server then exits cleanly.
        // Before, the shutdown branch returned a response but the loop never broke,
        // so the server hung until stdin EOF and a waiting client stalled. Checked
        // on the raw line (independent of the response) so a shutdown NOTIFICATION
        // (no `id`, no response) still ends the loop.
        if is_shutdown_request(line) {
            break;
        }
        if hit_cap {
            drain_to_newline(reader);
        }
    }
    Ok(())
}

/// Is this input line a JSON-RPC `shutdown` request? [`serve_io`] uses it to end
/// the loop after the shutdown response is written. Fail-open: an unparseable /
/// non-shutdown line is not a shutdown, so the loop continues as before.
fn is_shutdown_request(line: &str) -> bool {
    serde_json::from_str::<JsonRpcRequest>(line).is_ok_and(|r| r.method == "shutdown")
}

/// Parse one input line into the response to write (if any). A well-formed
/// request is dispatched via [`handle_request`] (which returns `None` for a
/// notification); a malformed line is answered with a JSON-RPC error, recovering
/// the `id` when the line is at least valid JSON.
fn build_response(line: &str, policy: &Policy) -> Option<JsonRpcResponse> {
    let Ok(req) = serde_json::from_str::<JsonRpcRequest>(line) else {
        // Don't silently drop a malformed request: a client that sent an `id`
        // would wait forever. Emit a JSON-RPC error, recovering the id when the
        // line is at least valid JSON.
        let id = serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|v| v.get("id").cloned());
        let (code, message) = if id.is_some() {
            (-32600, "Invalid Request")
        } else {
            (-32700, "Parse error")
        };
        return Some(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.to_string(),
            }),
        });
    };
    handle_request(&req, policy)
}

/// Discard bytes up to and including the next newline (or EOF), in bounded
/// chunks. Used to resynchronise after a single line exceeded
/// [`MAX_LINE_BYTES`], so the over-long line's tail isn't mis-read as a new
/// request.
fn drain_to_newline<R: BufRead>(reader: &mut R) {
    let mut scratch: Vec<u8> = Vec::new();
    loop {
        scratch.clear();
        match (&mut *reader)
            .take(MAX_LINE_BYTES)
            .read_until(b'\n', &mut scratch)
        {
            Ok(0) | Err(_) => return,                          // EOF / I/O error
            Ok(_) if scratch.last() == Some(&b'\n') => return, // consumed the newline
            Ok(_) => {}                                        // still draining — loop
        }
    }
}

/// One JSON-RPC 2.0 request.
///
/// `id` is `Option<Value>` so a request with NO `id` member (a notification)
/// is distinguishable from one carrying `"id": null`. Per JSON-RPC 2.0 a
/// notification gets no response; only requests (with an id) do.
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// One JSON-RPC 2.0 response (success or error).
#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

/// Dispatch a single JSON-RPC method. Returns `None` for notifications (no
/// response expected) — including any request that omits `id`.
fn handle_request(req: &JsonRpcRequest, policy: &Policy) -> Option<JsonRpcResponse> {
    // A request with no `id` member is a NOTIFICATION: per JSON-RPC 2.0 the
    // server MUST NOT reply (even on error). Drop it silently.
    let id = req.id.clone()?;

    // Validate the protocol version. A request that omits `jsonrpc` or sends a
    // value other than "2.0" (e.g. "1.0") is an Invalid Request — answer with
    // -32600 rather than treating it as a well-formed 2.0 call.
    if req.jsonrpc != "2.0" {
        return Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: None,
            error: Some(JsonRpcError {
                code: -32600,
                message: format!(
                    "Invalid Request: jsonrpc must be \"2.0\" (got {:?})",
                    req.jsonrpc
                ),
            }),
        });
    }

    match req.method.as_str() {
        "initialize" => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "umadev-governance",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            })),
            error: None,
        }),
        "initialized" | "notifications/initialized" => None, // notification
        "tools/list" => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: Some(json!({
                "tools": [
                    {
                        "name": TOOL_GOVERN_FILE,
                        "description": "Run UmaDev governance rules on a file's proposed content. Returns whether the content passes or is blocked, with the firing clause and a fix suggestion. Call BEFORE writing a file to a user's project.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "file_path": { "type": "string", "description": "The target file path (relative to project root)." },
                                "content": { "type": "string", "description": "The proposed file content to check." }
                            },
                            "required": ["file_path", "content"]
                        }
                    },
                    {
                        "name": TOOL_GOVERN_COMMAND,
                        "description": "Run UmaDev's dangerous-command guard (UD-SEC-002) on a shell command before executing it. Returns whether the command is safe to run.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "command": { "type": "string", "description": "The shell command to check." }
                            },
                            "required": ["command"]
                        }
                    },
                    {
                        "name": TOOL_PLAN_STATUS,
                        "description": "Read UmaDev's owned, persisted build plan (`.umadev/plan.json`) for a project: the dependency-DAG steps with their seat, kind, acceptance criterion and status, plus progress, risks and open questions. Read-only. Returns `has_plan:false` when no plan exists.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "project_root": { "type": "string", "description": "Project root directory. Defaults to the server's current directory." }
                            }
                        }
                    },
                    {
                        "name": TOOL_CONTRACT_CHECK,
                        "description": "Cross-check the project's real frontend fetch/axios calls against the API contract parsed from `output/<slug>-architecture.md` (UD-CODE-003). Returns the endpoint count, call count, alignment flag and any violations (undeclared_call / method_mismatch). Read-only analysis — never mutates the workspace.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "project_root": { "type": "string", "description": "Project root directory. Defaults to the server's current directory." },
                                "slug": { "type": "string", "description": "Project slug selecting `output/<slug>-architecture.md`. Optional — auto-detected from the output/ directory when omitted." }
                            }
                        }
                    },
                    {
                        "name": TOOL_LESSONS_RECALL,
                        "description": "Recall recorded pitfalls + corrective strategies from UmaDev's self-evolving memory for a project. Optionally filter by a free-text query; on-stack pitfalls (matching the project's dependency fingerprint) surface too. Returns each pitfall's signature, hit count, fix-lifecycle status, recorded fix and already-tried-but-failed fixes, plus validated success patterns. Read-only.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "project_root": { "type": "string", "description": "Project root directory. Defaults to the server's current directory." },
                                "query": { "type": "string", "description": "Free-text query to filter pitfalls by (title / signature / fix / tech-stack context). Optional — omit to get the worst-first top pitfalls." }
                            }
                        }
                    },
                    {
                        "name": TOOL_GOVERNANCE_SUMMARY,
                        "description": "Summarise the project's active governance posture: the total governance clause count, the clauses/paths/domains the project has opted out of (`.umadev/rules.toml`), and the tail of the tool-call audit trail (`.umadev/audit/tool-calls.jsonl`). Read-only.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "project_root": { "type": "string", "description": "Project root directory. Defaults to the server's current directory." }
                            }
                        }
                    }
                ]
            })),
            error: None,
        }),
        "tools/call" => Some(handle_tool_call(req, &id, policy)),
        "shutdown" => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: Some(json!({})),
            error: None,
        }),
        _ => Some(JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(id),
            result: None,
            error: Some(JsonRpcError {
                code: -32601,
                message: format!("method not found: {}", req.method),
            }),
        }),
    }
}

/// Handle a `tools/call` request. `id` is the request id (already known to be
/// present — `tools/call` is never a notification in this server).
///
/// Dispatches to the named tool. The two `govern_*` tools are content-scoped
/// pure checks; the four director tools are read-mostly + fail-open (any
/// failure degrades to an empty/"unavailable" result, never an error response).
/// Each tool yields `(text, is_error)`; only an UNKNOWN tool name is a protocol
/// error (`-32602`).
fn handle_tool_call(req: &JsonRpcRequest, id: &Value, policy: &Policy) -> JsonRpcResponse {
    let name = req
        .params
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let args = req.params.get("arguments").cloned().unwrap_or(json!({}));
    let outcome: Option<(String, bool)> = match name {
        TOOL_GOVERN_FILE => Some(govern_file_tool(&args, policy)),
        TOOL_GOVERN_COMMAND => Some(govern_command_tool(&args)),
        TOOL_PLAN_STATUS => Some(plan_status_tool(&args)),
        TOOL_CONTRACT_CHECK => Some(contract_check_tool(&args)),
        TOOL_LESSONS_RECALL => Some(lessons_recall_tool(&args)),
        TOOL_GOVERNANCE_SUMMARY => Some(governance_summary_tool(&args)),
        _ => None,
    };
    match outcome {
        Some((text, is_error)) => tool_text_result(id, &text, is_error),
        None => JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(id.clone()),
            result: None,
            error: Some(JsonRpcError {
                code: -32602,
                message: format!("unknown tool: {name}"),
            }),
        },
    }
}

/// Wrap a tool's `(text, is_error)` outcome in the standard MCP `tools/call`
/// result envelope (a single text content block + the `isError` flag).
fn tool_text_result(id: &Value, text: &str, is_error: bool) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: Some(id.clone()),
        result: Some(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": is_error,
        })),
        error: None,
    }
}

/// Pretty-print a JSON value as the tool's text payload. Fail-open: a serialize
/// error (unreachable for the values we build) degrades to `{}`.
fn json_text(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
}

/// Resolve the project root a director tool operates on: the `project_root`
/// argument when given + non-empty, else the server's current directory.
fn arg_root(args: &Value) -> PathBuf {
    args.get("project_root")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            PathBuf::from,
        )
}

/// `govern_file`: run the governance rules on a file's proposed content.
///
/// The bypass-immune irreversible floor runs FIRST ([`pre_write_floor_decision`])
/// and IGNORES the project policy — so a `.umadev/rules.toml` that disabled
/// UD-SEC-003/018/026 (or the UD-SEC-001 path guard) can NOT turn a leaked
/// `sk_live_…` secret / credential / sensitive-path write into a "PASS" here. Only
/// when the floor is clean does the policy-aware content scan run (which honours
/// disabled clauses for everything else).
fn govern_file_tool(args: &Value, policy: &Policy) -> (String, bool) {
    let path = args.get("file_path").and_then(Value::as_str).unwrap_or("");
    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
    let floor = pre_write_floor_decision(path, content);
    if floor.block {
        return govern_decision_text(true, &floor.clause, &floor.reason);
    }
    let d = scan_content_with_policy(path, content, policy);
    govern_decision_text(d.block, &d.clause, &d.reason)
}

/// `govern_command`: run the dangerous-command guard on a shell command.
fn govern_command_tool(args: &Value) -> (String, bool) {
    let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
    let d = check_dangerous_bash(cmd);
    govern_decision_text(d.block, &d.clause, &d.reason)
}

/// Render a governance decision into the tool's `(text, is_error)` outcome —
/// the historical wording, preserved so existing hosts/tests are unaffected.
fn govern_decision_text(blocked: bool, clause: &str, reason: &str) -> (String, bool) {
    let text = if blocked {
        format!("BLOCKED ({clause}): {reason}")
    } else {
        "PASS: no governance violations detected.".to_string()
    };
    (text, blocked)
}

/// `plan_status`: read the owned, persisted plan (`.umadev/plan.json`) for the
/// project. Fail-open: an absent / unreadable / corrupt plan yields
/// `has_plan:false` (handled inside [`umadev_agent::load_plan`]).
fn plan_status_tool(args: &Value) -> (String, bool) {
    let root = arg_root(args);
    let Some(plan) = umadev_agent::load_plan(&root) else {
        return (
            json_text(&json!({
                "has_plan": false,
                "message": "No plan found (.umadev/plan.json is absent or unreadable).",
            })),
            false,
        );
    };
    let (done, total) = plan.progress();
    let steps: Vec<Value> = plan
        .steps
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "title": s.title,
                "seat": s.seat.role_id(),
                "kind": serde_json::to_value(s.kind).unwrap_or(Value::Null),
                "depends_on": s.depends_on,
                "acceptance": serde_json::to_value(&s.acceptance).unwrap_or(Value::Null),
                "status": s.status.as_str(),
            })
        })
        .collect();
    let out = json!({
        "has_plan": true,
        "progress": { "done": done, "total": total },
        "steps": steps,
        "risks": plan.risks,
        "open_questions": plan.open_questions,
    });
    (json_text(&out), false)
}

/// `contract_check`: parse the architecture API table, extract the project's
/// real frontend calls, and cross-validate (UD-CODE-003). Read-only — it only
/// reads files and reports violations, never writes. Fail-open: no architecture
/// doc yields `has_contract:false`.
fn contract_check_tool(args: &Value) -> (String, bool) {
    let root = arg_root(args);
    let explicit_slug = args
        .get("slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some((slug, arch)) = find_architecture_doc(&root, explicit_slug) else {
        return (
            json_text(&json!({
                "has_contract": false,
                "message": "No architecture contract found (output/<slug>-architecture.md is absent or empty).",
                "violations": [],
            })),
            false,
        );
    };
    let spec = umadev_contract::parse_architecture(&arch, &slug);
    let calls = umadev_contract::extract_frontend_calls(&root);
    let violations = umadev_contract::validate_frontend_vs_contract(&calls, &spec);
    let vlist: Vec<Value> = violations
        .iter()
        .map(|v| {
            json!({
                "kind": violation_kind_str(v.kind),
                "detail": v.detail,
            })
        })
        .collect();
    let out = json!({
        "has_contract": true,
        "slug": slug,
        "endpoints": spec.len(),
        "frontend_calls": calls.len(),
        "aligned": violations.is_empty(),
        "violations": vlist,
    });
    (json_text(&out), false)
}

/// Locate the architecture contract doc under `<root>/output/`. With an explicit
/// slug, read `output/<slug>-architecture.md`; otherwise scan for the first
/// non-empty `*-architecture.md` and derive the slug from its name. Returns
/// `(slug, content)` or `None` when no usable doc exists (fail-open).
fn find_architecture_doc(root: &Path, explicit_slug: Option<&str>) -> Option<(String, String)> {
    let output = root.join("output");
    if let Some(slug) = explicit_slug {
        // A caller-supplied slug is interpolated straight into a filename, so a
        // value like `../../../../etc/foo` would escape `output/` and read an
        // arbitrary host file. Reject anything that isn't a single safe path
        // component before touching the filesystem (fail-open → None).
        if !is_safe_slug(slug) {
            return None;
        }
        let path = output.join(format!("{slug}-architecture.md"));
        let text = std::fs::read_to_string(path).ok()?;
        if text.trim().is_empty() {
            return None;
        }
        return Some((slug.to_string(), text));
    }
    let rd = std::fs::read_dir(&output).ok()?;
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(slug) = name.strip_suffix("-architecture.md") {
            if slug.is_empty() {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if !text.trim().is_empty() {
                    return Some((slug.to_string(), text));
                }
            }
        }
    }
    None
}

/// Is `slug` a single safe path component (no `..`, separators, root, or
/// prefix)? Anything else, interpolated into a filename and `join`ed under
/// `output/`, could traverse out of the workspace — so we reject it. Mirrors the
/// `safe_component` guard in `skill_manager` / `knowledge_manager`.
fn is_safe_slug(slug: &str) -> bool {
    use std::path::{Component, Path};
    if slug.is_empty() {
        return false;
    }
    let mut comps = Path::new(slug).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

/// Stable wire string for a contract-violation kind.
fn violation_kind_str(kind: umadev_contract::validate::ViolationKind) -> &'static str {
    use umadev_contract::validate::ViolationKind;
    match kind {
        ViolationKind::UndeclaredCall => "undeclared_call",
        ViolationKind::MethodMismatch => "method_mismatch",
        ViolationKind::UnmatchedRoute => "unmatched_route",
    }
}

/// `lessons_recall`: recall recorded pitfalls + corrective strategies from the
/// self-evolving memory. Uses the PURE-READ [`umadev_agent::lessons_report`]
/// (never the prompt-assembly recall, which mutates efficacy bookkeeping), then
/// filters by the optional query + the project's tech-stack fingerprint.
/// Fail-open: an empty KB yields `has_lessons:false`.
fn lessons_recall_tool(args: &Value) -> (String, bool) {
    let root = arg_root(args);
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let report = umadev_agent::lessons_report(&root);
    if report.is_empty() {
        return (
            json_text(&json!({
                "has_lessons": false,
                "message": "No lessons recorded yet for this project.",
                "pitfalls": [],
                "validated_patterns": [],
            })),
            false,
        );
    }
    // Build the relevance filter from the query words PLUS the project's real
    // dependency fingerprint (so an on-stack pitfall surfaces without a textual
    // query). With NO query and NO detectable stack there is nothing to filter
    // on → return the worst-first top pitfalls as-is.
    let q_tokens: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(str::to_string)
        .collect();
    let stack: std::collections::HashSet<String> =
        umadev_agent::lessons::project_context_tokens(&root)
            .into_iter()
            .collect();
    let has_filter = !q_tokens.is_empty() || !stack.is_empty();
    let relevant = |e: &umadev_agent::PitfallEntry| -> bool {
        if !has_filter {
            return true;
        }
        let hay = format!(
            "{} {} {} {}",
            e.title,
            e.signature,
            e.fix,
            e.context.join(" ")
        )
        .to_ascii_lowercase();
        let by_query = q_tokens.iter().any(|t| hay.contains(t.as_str()));
        let by_stack = e
            .context
            .iter()
            .any(|c| stack.contains(&c.to_ascii_lowercase()));
        by_query || by_stack
    };
    let pitfalls: Vec<Value> = report
        .top_pitfalls
        .iter()
        .filter(|e| relevant(e))
        .map(|e| {
            json!({
                "title": e.title,
                "signature": e.signature,
                "hits": e.hits,
                "status": pitfall_status_str(e.status),
                "fix": e.fix,
                "context": e.context,
                "failed_fixes": e.failed_fixes,
            })
        })
        .collect();
    let validated: Vec<Value> = report
        .validated_patterns
        .iter()
        .map(|v| json!({ "title": v.title, "summary": v.summary }))
        .collect();
    let out = json!({
        "has_lessons": true,
        "efficacy": {
            "total": report.efficacy.total,
            "validated": report.efficacy.validated,
            "recurring": report.efficacy.recurring,
            "active": report.efficacy.active,
        },
        "pitfalls": pitfalls,
        "validated_patterns": validated,
    });
    (json_text(&out), false)
}

/// Stable wire string for a pitfall's fix-lifecycle status.
fn pitfall_status_str(status: umadev_agent::PitfallStatus) -> &'static str {
    use umadev_agent::PitfallStatus;
    match status {
        PitfallStatus::Active => "active",
        PitfallStatus::Validated => "validated",
        PitfallStatus::Recurring => "recurring",
    }
}

/// `governance_summary`: the active rule policy (opt-outs from
/// `.umadev/rules.toml`) plus the tail of the tool-call audit trail. Read-only +
/// fail-open: missing policy/audit files yield defaults / an empty tail.
fn governance_summary_tool(args: &Value) -> (String, bool) {
    let root = arg_root(args);
    let policy = Policy::load(&root);
    let out = json!({
        "total_clauses": umadev_spec::CLAUSES.len(),
        "disabled_clauses": policy.disabled.clauses,
        "excluded_paths": policy.exclusions.paths,
        "extra_blocked_domains": policy.extra.blocked_domains,
        "audit_tail": read_audit_tail(&root, AUDIT_TAIL_LEN),
    });
    (json_text(&out), false)
}

/// How many trailing audit rows `governance_summary` surfaces — a recent-activity
/// peek, not a full dump.
const AUDIT_TAIL_LEN: usize = 10;

/// Read the last `n` rows of the tool-call audit log
/// (`.umadev/audit/tool-calls.jsonl`) as parsed JSON, oldest-first. Fail-open: a
/// missing/unreadable log yields an empty vec; unparseable rows are skipped.
fn read_audit_tail(root: &Path, n: usize) -> Vec<Value> {
    let path = root.join(".umadev").join("audit").join("tool-calls.jsonl");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut tail: Vec<Value> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .rev()
        .take(n)
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();
    tail.reverse();
    tail
}

#[cfg(test)]
mod tests {
    use super::*;
    use umadev_governance::Policy;

    #[test]
    fn initialize_returns_capabilities() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "initialize".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        assert!(resp.result.is_some());
        let r = resp.result.unwrap();
        assert_eq!(r["serverInfo"]["name"], "umadev-governance");
        assert!(r["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_exposes_govern_file_and_command() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "tools/list".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"govern_file"));
        assert!(names.contains(&"govern_command"));
    }

    #[test]
    fn govern_file_blocks_emoji() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(3)),
            method: "tools/call".into(),
            params: json!({
                "name": "govern_file",
                "arguments": {
                    "file_path": "src/B.tsx",
                    "content": "<b>🔍</b>"
                }
            }),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("BLOCKED"));
        assert!(text.contains("UD-CODE-001"));
    }

    #[test]
    fn govern_file_passes_clean_code() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(4)),
            method: "tools/call".into(),
            params: json!({
                "name": "govern_file",
                "arguments": {
                    "file_path": "src/clean.ts",
                    "content": "export const add = (a: number, b: number): number => a + b;"
                }
            }),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("PASS"));
    }

    #[test]
    fn govern_command_blocks_rm_rf() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(5)),
            method: "tools/call".into(),
            params: json!({
                "name": "govern_command",
                "arguments": { "command": "rm -rf /" }
            }),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("UD-SEC-002"));
    }

    #[test]
    fn unknown_method_returns_error() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(6)),
            method: "nonexistent".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, -32601);
    }

    #[test]
    fn govern_file_respects_policy_disabled() {
        // When UD-CODE-001 is disabled, emoji should pass.
        let policy = Policy {
            disabled: umadev_governance::DisabledSection {
                clauses: vec!["UD-CODE-001".into()],
            },
            ..Default::default()
        };
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(7)),
            method: "tools/call".into(),
            params: json!({
                "name": "govern_file",
                "arguments": {
                    "file_path": "src/B.tsx",
                    "content": "<b>🔍</b>"
                }
            }),
        };
        let resp = handle_request(&req, &policy).unwrap();
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], false);
    }

    #[test]
    fn govern_file_floor_blocks_secret_despite_disabled_clauses() {
        // REPRODUCTION: even with the secret clauses (UD-SEC-003/018/026) disabled
        // in the project policy, `govern_file` must NOT return PASS for a hardcoded
        // live secret — the bypass-immune floor blocks it FIRST, before the
        // policy-aware scan (which would otherwise honour the disable and pass).
        let policy = Policy {
            disabled: umadev_governance::DisabledSection {
                clauses: vec![
                    "UD-SEC-001".into(),
                    "UD-SEC-003".into(),
                    "UD-SEC-018".into(),
                    "UD-SEC-026".into(),
                ],
            },
            ..Default::default()
        };
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(31)),
            method: "tools/call".into(),
            params: json!({
                "name": "govern_file",
                "arguments": {
                    "file_path": "src/cfg.ts",
                    "content": "const k = \"aB3xK9pQ7mNr2WvT5sZ8dF1gH4jL6cE0\";"
                }
            }),
        };
        let resp = handle_request(&req, &policy).unwrap();
        let result = resp.result.unwrap();
        assert_eq!(
            result["isError"], true,
            "the floor must block a leaked secret even when the clauses are disabled"
        );
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("BLOCKED"), "{text}");
    }

    #[test]
    fn govern_file_floor_blocks_sensitive_path_despite_disabled_clause() {
        // Disabling UD-SEC-001 must not let `govern_file` PASS a write to a
        // sensitive path — the floor's path guard still blocks it.
        let policy = Policy {
            disabled: umadev_governance::DisabledSection {
                clauses: vec!["UD-SEC-001".into()],
            },
            ..Default::default()
        };
        let (text, is_error) = govern_file_tool(
            &json!({ "file_path": ".env", "content": "PORT=3000" }),
            &policy,
        );
        assert!(
            is_error,
            "a write to .env must block despite disabled UD-SEC-001"
        );
        assert!(text.contains("UD-SEC-001"), "{text}");
    }

    #[test]
    fn serve_io_breaks_the_loop_on_shutdown() {
        // A `shutdown` request must (a) get its response, then (b) END the loop so a
        // client blocked on exit doesn't hang. A request AFTER shutdown must NOT be
        // answered — the loop already broke.
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":1,"method":"shutdown"}"#);
        input.push(b'\n');
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#);
        input.push(b'\n');
        let mut out: Vec<u8> = Vec::new();
        let mut reader = std::io::Cursor::new(input);
        serve_io(&mut reader, &mut out, &Policy::default()).unwrap();
        let text = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "only the shutdown reply is written, then the loop ends: {text}"
        );
        let resp: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(
            resp["id"], 1,
            "the shutdown request is answered before exit"
        );
        // The post-shutdown `initialize` was never reached.
        assert!(
            !text.contains("serverInfo"),
            "no request after shutdown may be answered: {text}"
        );
    }

    #[test]
    fn shutdown_notification_also_breaks_the_loop() {
        // A shutdown with NO id is a notification (no response), but it must STILL
        // end the loop — the raw-line check is independent of the response.
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(br#"{"jsonrpc":"2.0","method":"shutdown"}"#);
        input.push(b'\n');
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":2,"method":"initialize"}"#);
        input.push(b'\n');
        let mut out: Vec<u8> = Vec::new();
        let mut reader = std::io::Cursor::new(input);
        serve_io(&mut reader, &mut out, &Policy::default()).unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(
            !text.contains("serverInfo"),
            "the loop must break on a shutdown notification too: {text}"
        );
    }

    #[test]
    fn initialized_notification_returns_none() {
        // A genuine notification carries NO id member.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: None,
            method: "notifications/initialized".into(),
            params: json!({}),
        };
        assert!(handle_request(&req, &Policy::default()).is_none());
    }

    #[test]
    fn request_without_id_is_a_notification_and_gets_no_reply() {
        // Even a NORMALLY-answered method (initialize) must be dropped when the
        // caller omitted `id` — per JSON-RPC, that's a notification.
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: None,
            method: "initialize".into(),
            params: json!({}),
        };
        assert!(
            handle_request(&req, &Policy::default()).is_none(),
            "a request with no id must get no response"
        );
    }

    #[test]
    fn missing_or_null_id_both_parse_as_notification() {
        // No `id` key → `id: None` → notification.
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"initialize"}"#).unwrap();
        assert!(req.id.is_none());
        // serde maps an explicit `"id": null` to None too (Option treats JSON
        // null as absent). The JSON-RPC spec discourages a null id anyway, so
        // folding it into "notification → no reply" is a safe interpretation.
        let req2: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":null,"method":"initialize"}"#).unwrap();
        assert!(req2.id.is_none());
        // A real id round-trips.
        let req3: JsonRpcRequest =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":7,"method":"initialize"}"#).unwrap();
        assert_eq!(req3.id, Some(json!(7)));
    }

    #[test]
    fn wrong_jsonrpc_version_is_invalid_request() {
        // "1.0" (or anything != "2.0") on a request WITH an id → -32600.
        let req = JsonRpcRequest {
            jsonrpc: "1.0".into(),
            id: Some(json!(9)),
            method: "initialize".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        let err = resp.error.expect("wrong version must error");
        assert_eq!(err.code, -32600);
        assert_eq!(resp.id, Some(json!(9)));
    }

    #[test]
    fn missing_jsonrpc_version_is_invalid_request() {
        // serde default leaves jsonrpc empty when the key is absent → -32600.
        let req: JsonRpcRequest =
            serde_json::from_str(r#"{"id":3,"method":"initialize"}"#).unwrap();
        let resp = handle_request(&req, &Policy::default()).unwrap();
        assert_eq!(resp.error.expect("must error").code, -32600);
    }

    #[test]
    fn wrong_version_notification_still_gets_no_reply() {
        // No id → notification → dropped BEFORE the version check fires.
        let req = JsonRpcRequest {
            jsonrpc: "1.0".into(),
            id: None,
            method: "initialize".into(),
            params: json!({}),
        };
        assert!(handle_request(&req, &Policy::default()).is_none());
    }

    // ──────────────────────────────────────────────────────────────────────
    // Director-layer read-mostly tools (plan_status / contract_check /
    // lessons_recall / governance_summary).
    // ──────────────────────────────────────────────────────────────────────

    /// Invoke a director tool over `root` with optional extra `arguments`
    /// (an object). Asserts the tool succeeded (`isError == false` — these are
    /// read-mostly, never an error) and returns its JSON-decoded text payload.
    fn director_tool(name: &str, root: &std::path::Path, extra: Value) -> Value {
        let mut map = extra.as_object().cloned().unwrap_or_default();
        map.insert(
            "project_root".to_string(),
            json!(root.to_string_lossy().to_string()),
        );
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: "tools/call".into(),
            params: json!({ "name": name, "arguments": Value::Object(map) }),
        };
        let resp =
            handle_request(&req, &Policy::default()).expect("a request with id gets a reply");
        let result = resp.result.expect("a read tool always yields a result");
        assert_eq!(result["isError"], false, "read-mostly tools never error");
        let text = result["content"][0]["text"]
            .as_str()
            .expect("text content block");
        serde_json::from_str(text).expect("the tool's text payload is JSON")
    }

    #[test]
    fn tools_list_exposes_all_six_tools() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(2)),
            method: "tools/list".into(),
            params: json!({}),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // The two original governance tools stay intact …
        assert!(names.contains(&"govern_file"));
        assert!(names.contains(&"govern_command"));
        // … plus the four new director tools.
        assert!(names.contains(&"plan_status"));
        assert!(names.contains(&"contract_check"));
        assert!(names.contains(&"lessons_recall"));
        assert!(names.contains(&"governance_summary"));
        assert_eq!(names.len(), 6, "exactly six tools are registered");
        // Every tool advertises a valid object input schema.
        for t in &tools {
            assert_eq!(t["inputSchema"]["type"], "object", "tool {t:?}");
        }
    }

    #[test]
    fn plan_status_returns_the_persisted_plan() {
        use umadev_agent::{save_plan, AcceptanceSpec, Plan, PlanStep, Seat, StepKind, StepStatus};
        let tmp = tempfile::tempdir().unwrap();
        let plan = Plan {
            steps: vec![
                PlanStep {
                    id: "scaffold".into(),
                    title: "Scaffold the app".into(),
                    seat: Seat::FrontendEngineer,
                    kind: StepKind::Build,
                    depends_on: vec![],
                    acceptance: AcceptanceSpec::SourcePresent,
                    evidence: Vec::new(),
                    status: StepStatus::Done,
                },
                PlanStep {
                    id: "auth".into(),
                    title: "Auth route".into(),
                    seat: Seat::BackendEngineer,
                    kind: StepKind::Build,
                    depends_on: vec!["scaffold".into()],
                    acceptance: AcceptanceSpec::Contract,
                    evidence: Vec::new(),
                    status: StepStatus::Pending,
                },
            ],
            risks: vec!["tight timeline".into()],
            open_questions: vec![],
        };
        save_plan(&plan, tmp.path()).unwrap();

        let out = director_tool("plan_status", tmp.path(), json!({}));
        assert_eq!(out["has_plan"], true);
        assert_eq!(out["progress"]["done"], 1);
        assert_eq!(out["progress"]["total"], 2);
        let steps = out["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0]["id"], "scaffold");
        assert_eq!(steps[0]["seat"], "frontend-engineer");
        assert_eq!(steps[0]["kind"], "build");
        assert_eq!(steps[0]["acceptance"], "source-present");
        assert_eq!(steps[0]["status"], "done");
        assert_eq!(steps[1]["seat"], "backend-engineer");
        assert_eq!(steps[1]["depends_on"][0], "scaffold");
        assert_eq!(out["risks"][0], "tight timeline");
    }

    #[test]
    fn plan_status_reports_no_plan_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let out = director_tool("plan_status", tmp.path(), json!({}));
        assert_eq!(out["has_plan"], false);
        assert!(out["message"].as_str().unwrap().contains("No plan"));
    }

    /// A minimal architecture API table the contract parser understands.
    const ARCH_DOC: &str = "| Method | Path | Request | Response | Auth | Description |
|---|---|---|---|---|---|
| GET | /api/users | - | - | none | List users |
| POST | /api/users | - | - | none | Create a user |
";

    #[test]
    fn contract_check_flags_an_undeclared_call() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(tmp.path().join("output/demo-architecture.md"), ARCH_DOC).unwrap();
        // A frontend call to an endpoint the contract never declares.
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/api.ts"),
            "export const load = () => fetch('/api/ghost');",
        )
        .unwrap();

        // Slug auto-detected from output/demo-architecture.md.
        let out = director_tool("contract_check", tmp.path(), json!({}));
        assert_eq!(out["has_contract"], true);
        assert_eq!(out["slug"], "demo");
        assert_eq!(out["aligned"], false);
        let violations = out["violations"].as_array().unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0]["kind"], "undeclared_call");
        assert!(violations[0]["detail"]
            .as_str()
            .unwrap()
            .contains("/api/ghost"));
    }

    #[test]
    fn contract_check_aligned_has_no_violations() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("output")).unwrap();
        std::fs::write(tmp.path().join("output/shop-architecture.md"), ARCH_DOC).unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        // A declared GET call (method unknown → no false MethodMismatch).
        std::fs::write(
            tmp.path().join("src/api.ts"),
            "export const load = () => fetch('/api/users');",
        )
        .unwrap();

        // Explicit slug also resolves the doc.
        let out = director_tool("contract_check", tmp.path(), json!({ "slug": "shop" }));
        assert_eq!(out["has_contract"], true);
        assert_eq!(out["aligned"], true);
        assert_eq!(out["endpoints"], 2);
        assert!(out["violations"].as_array().unwrap().is_empty());
    }

    #[test]
    fn contract_check_no_doc_is_fail_open() {
        let tmp = tempfile::tempdir().unwrap();
        let out = director_tool("contract_check", tmp.path(), json!({}));
        assert_eq!(out["has_contract"], false);
        assert!(out["violations"].as_array().unwrap().is_empty());
    }

    /// Write a single dev-error pitfall into the raw lessons ledger the report
    /// reader consumes. Only the no-default `Lesson` fields plus the few the
    /// report surfaces are set; the rest ride their serde defaults.
    fn seed_pitfall(root: &std::path::Path) {
        let raw = root.join(".umadev/learned/_raw");
        std::fs::create_dir_all(&raw).unwrap();
        let lesson = json!({
            "kind": "dev_error",
            "domain": "dependency",
            "title": "踩坑 [dependency/module-not-found/react-router-dom]: Cannot find module",
            "body": "During the demo run this error was hit.",
            "fix": "Run npm i react-router-dom, then restart the dev server.",
            "root_cause": "react-router-dom was imported but never installed.",
            "keywords": ["react", "router", "dependency"],
            "source_requirement": "build a dashboard",
            "first_seen": "2026-01-01T00:00:00Z",
            "signature": "dependency/module-not-found/react-router-dom",
            "occurrences": 3,
            "context": ["react", "vite", "typescript"],
        });
        std::fs::write(
            raw.join("dev-errors.jsonl"),
            format!("{}\n", serde_json::to_string(&lesson).unwrap()),
        )
        .unwrap();
    }

    #[test]
    fn lessons_recall_returns_the_recorded_pitfall() {
        let tmp = tempfile::tempdir().unwrap();
        seed_pitfall(tmp.path());

        // Query matches the signature/fix/title.
        let out = director_tool("lessons_recall", tmp.path(), json!({ "query": "router" }));
        assert_eq!(out["has_lessons"], true);
        assert_eq!(out["efficacy"]["total"], 1);
        let pits = out["pitfalls"].as_array().unwrap();
        assert_eq!(pits.len(), 1);
        assert!(pits[0]["title"]
            .as_str()
            .unwrap()
            .contains("react-router-dom"));
        assert_eq!(pits[0]["hits"], 3);
        assert_eq!(pits[0]["status"], "active");
        assert!(pits[0]["fix"].as_str().unwrap().contains("npm i"));

        // No query → the worst-first top pitfalls still surface.
        let all = director_tool("lessons_recall", tmp.path(), json!({}));
        assert_eq!(all["pitfalls"].as_array().unwrap().len(), 1);

        // A query that matches nothing on this (manifest-less → no stack) project
        // filters the pitfall out — honest empty, not a crash.
        let none = director_tool(
            "lessons_recall",
            tmp.path(),
            json!({ "query": "kubernetes" }),
        );
        assert!(none["pitfalls"].as_array().unwrap().is_empty());
    }

    #[test]
    fn lessons_recall_empty_kb_is_fail_open() {
        let tmp = tempfile::tempdir().unwrap();
        let out = director_tool("lessons_recall", tmp.path(), json!({ "query": "anything" }));
        assert_eq!(out["has_lessons"], false);
        assert!(out["pitfalls"].as_array().unwrap().is_empty());
    }

    #[test]
    fn governance_summary_reports_policy_and_audit_tail() {
        let tmp = tempfile::tempdir().unwrap();
        let udir = tmp.path().join(".umadev");
        std::fs::create_dir_all(udir.join("audit")).unwrap();
        std::fs::write(
            udir.join("rules.toml"),
            "[disabled]\nclauses = [\"UD-ARCH-002\"]\n",
        )
        .unwrap();
        std::fs::write(
            udir.join("audit/tool-calls.jsonl"),
            "{\"tool\":\"Write\",\"decision\":\"allow\"}\n{\"tool\":\"Bash\",\"decision\":\"block\"}\n",
        )
        .unwrap();

        let out = director_tool("governance_summary", tmp.path(), json!({}));
        assert_eq!(out["total_clauses"], umadev_spec::CLAUSES.len());
        assert!(out["total_clauses"].as_u64().unwrap() > 0);
        assert_eq!(out["disabled_clauses"][0], "UD-ARCH-002");
        let tail = out["audit_tail"].as_array().unwrap();
        assert_eq!(tail.len(), 2);
        // Oldest-first ordering is preserved.
        assert_eq!(tail[0]["tool"], "Write");
        assert_eq!(tail[1]["tool"], "Bash");
    }

    #[test]
    fn director_tools_fail_open_on_a_missing_root() {
        // A non-existent project root must NEVER panic — every director tool
        // degrades to its empty/"unavailable" shape with isError == false.
        let missing = std::path::Path::new("/nonexistent/umadev-mcp-xyz");
        let plan = director_tool("plan_status", missing, json!({}));
        assert_eq!(plan["has_plan"], false);
        let contract = director_tool("contract_check", missing, json!({}));
        assert_eq!(contract["has_contract"], false);
        let lessons = director_tool("lessons_recall", missing, json!({}));
        assert_eq!(lessons["has_lessons"], false);
        // governance_summary still answers (default policy + empty tail).
        let gov = director_tool("governance_summary", missing, json!({}));
        assert!(gov["total_clauses"].as_u64().unwrap() > 0);
        assert!(gov["audit_tail"].as_array().unwrap().is_empty());
        assert!(gov["disabled_clauses"].as_array().unwrap().is_empty());
    }

    #[test]
    fn serve_io_survives_invalid_utf8_line() {
        // Regression: the old `BufRead::lines()` yielded Err on an invalid-UTF-8
        // line and the loop `break`d — killing the whole session. Now a bad-UTF-8
        // line is answered with a parse error and a FOLLOWING valid request is
        // still served.
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(&[0xff, 0xfe, 0x00, b'\n']); // invalid UTF-8 line
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        input.push(b'\n');
        let mut out: Vec<u8> = Vec::new();
        let mut reader = std::io::Cursor::new(input);
        serve_io(&mut reader, &mut out, &Policy::default()).unwrap();
        let text = String::from_utf8_lossy(&out);
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "both input lines must be answered: {text}");
        // First answer: a parse error (the garbage has no recoverable id).
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["error"]["code"], -32700);
        // Second answer: the real `initialize` reply survived the bad line.
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["id"], 1);
        assert_eq!(second["result"]["serverInfo"]["name"], "umadev-governance");
    }

    #[test]
    fn serve_io_caps_an_overlong_line_and_resyncs() {
        // Regression: `lines()` read unbounded — a stream with no newline grew
        // memory without bound. A line larger than the cap is now answered (as a
        // parse error) and the loop resynchronises to the next newline so the
        // following request is still served.
        let cap = usize::try_from(MAX_LINE_BYTES).expect("cap fits usize");
        let mut input = "x".repeat(cap + 1024).into_bytes();
        input.push(b'\n');
        input.extend_from_slice(br#"{"jsonrpc":"2.0","id":7,"method":"initialize"}"#);
        input.push(b'\n');
        let mut out: Vec<u8> = Vec::new();
        let mut reader = std::io::Cursor::new(input);
        serve_io(&mut reader, &mut out, &Policy::default()).unwrap();
        let text = String::from_utf8_lossy(&out);
        let answered_seven = text.lines().any(|l| {
            serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| v.get("id").and_then(Value::as_i64))
                == Some(7)
        });
        assert!(
            answered_seven,
            "the request AFTER an over-long line must be answered: {text}"
        );
    }

    #[test]
    fn unknown_tool_is_a_param_error() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: Some(json!(9)),
            method: "tools/call".into(),
            params: json!({ "name": "does_not_exist", "arguments": {} }),
        };
        let resp = handle_request(&req, &Policy::default()).unwrap();
        assert_eq!(resp.error.expect("unknown tool errors").code, -32602);
    }

    #[test]
    fn is_safe_slug_rejects_traversal_and_separators() {
        assert!(is_safe_slug("my-app"));
        assert!(is_safe_slug("checkout_v2"));
        assert!(!is_safe_slug(""));
        assert!(!is_safe_slug(".."));
        assert!(!is_safe_slug("../../../../etc/foo"));
        assert!(!is_safe_slug("a/b"));
        assert!(!is_safe_slug("/etc/passwd"));
    }

    #[test]
    fn find_architecture_doc_rejects_slug_traversal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("output")).unwrap();
        // A real architecture doc for a SAFE slug is found...
        std::fs::write(
            root.join("output/app-architecture.md"),
            "# API\n| Method | Path |\n",
        )
        .unwrap();
        assert!(find_architecture_doc(root, Some("app")).is_some());

        // ...but a traversal slug that would escape `output/` is refused (None),
        // even when a target file is placed where the `..` would resolve.
        std::fs::write(root.join("secret-architecture.md"), "# escaped\nx\n").unwrap();
        assert!(
            find_architecture_doc(root, Some("../secret")).is_none(),
            "a `..` slug must not escape output/"
        );
    }
}
