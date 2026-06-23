//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! Exposes UmaDev's governance layer as a `tools/call` target so ANY
//! MCP-compatible host (Claude Desktop, Cursor, Continue, etc.) can ask
//! "is this file content safe to write?" and get a structured decision —
//! turning UmaDev into a governance gateway for the whole MCP ecosystem,
//! not just Claude Code's PreToolUse hook.
//!
//! ## Protocol
//! JSON-RPC 2.0 over stdio, one request per line. UmaDev implements:
//! - `initialize` → server capabilities
//! - `tools/list` → the `govern_file` + `govern_command` tools
//! - `tools/call` → run governance on a `{file_path, content}` or `{command}`
//!
//! Hosts register UmaDev as an MCP server (stdio transport) and call
//! `govern_file` before writing. This is the MCP equivalent of the
//! PreToolUse hook — but portable to every MCP client, not just Claude Code.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::Path;

use umadev_governance::{check_dangerous_bash, scan_content_with_policy, Policy};

/// Tool name: govern a file's proposed content.
const TOOL_GOVERN_FILE: &str = "govern_file";
/// Tool name: govern a shell command before execution.
const TOOL_GOVERN_COMMAND: &str = "govern_command";

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
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<JsonRpcRequest>(&line) else {
            // Don't silently drop a malformed request: a client that sent an
            // `id` would wait forever. Emit a JSON-RPC error, recovering the id
            // when the line is at least valid JSON.
            let id = serde_json::from_str::<Value>(&line)
                .ok()
                .and_then(|v| v.get("id").cloned());
            let (code, message) = if id.is_some() {
                (-32600, "Invalid Request")
            } else {
                (-32700, "Parse error")
            };
            let resp = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(JsonRpcError {
                    code,
                    message: message.to_string(),
                }),
            };
            let serialized = serde_json::to_string(&resp).unwrap_or_default();
            writeln!(out, "{serialized}")?;
            out.flush()?;
            continue;
        };
        let resp = handle_request(&req, &policy);
        if let Some(r) = resp {
            let serialized = serde_json::to_string(&r).unwrap_or_default();
            writeln!(out, "{serialized}")?;
            out.flush()?;
        }
    }
    Ok(())
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
fn handle_tool_call(req: &JsonRpcRequest, id: &Value, policy: &Policy) -> JsonRpcResponse {
    let name = req
        .params
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let args = req.params.get("arguments").cloned().unwrap_or(json!({}));
    let (blocked, clause, reason) = match name {
        TOOL_GOVERN_FILE => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let d = scan_content_with_policy(path, content, policy);
            (d.block, d.clause, d.reason)
        }
        TOOL_GOVERN_COMMAND => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let d = check_dangerous_bash(cmd);
            (d.block, d.clause, d.reason)
        }
        _ => {
            return JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: Some(id.clone()),
                result: None,
                error: Some(JsonRpcError {
                    code: -32602,
                    message: format!("unknown tool: {name}"),
                }),
            };
        }
    };
    let text = if blocked {
        format!("BLOCKED ({clause}): {reason}")
    } else {
        "PASS: no governance violations detected.".into()
    };
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id: Some(id.clone()),
        result: Some(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": blocked,
        })),
        error: None,
    }
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
}
