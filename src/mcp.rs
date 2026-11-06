//! A read-only MCP server over stdio (JSON-RPC 2.0, one JSON object per line).
//!
//! Every tool reads persisted state the way `status` does: no network, no writer lock, no
//! state writes, no source URLs or secrets. Diagnostics go to stderr only.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::constants::VERSION;
use crate::tempo::tool_definitions;

pub const MAX_MESSAGE_BYTES: usize = 1 << 20;
const SUPPORTED: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const DEFAULT_VERSION: &str = "2025-06-18";

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let structured = if value.is_object() { value.clone() } else { json!({"result": value}) };
    json!({
        "content": [{"type": "text", "text": serde_json::to_string(&value).unwrap_or_default()}],
        "structuredContent": structured,
        "isError": is_error,
    })
}

fn annotated_tools() -> Vec<Value> {
    tool_definitions()
        .into_iter()
        .map(|mut tool| {
            tool["annotations"] = json!({
                "title": tool["title"], "readOnlyHint": true, "destructiveHint": false,
                "idempotentHint": true, "openWorldHint": false,
            });
            tool
        })
        .collect()
}

pub struct Server {
    config_path: PathBuf,
}

fn optional_limit(arguments: &serde_json::Map<String, Value>) -> Result<i64, String> {
    match arguments.get("limit") {
        None => Ok(20),
        Some(value) => value
            .as_i64()
            .filter(|n| (1..=100).contains(n))
            .ok_or_else(|| "limit must be an integer from 1 to 100".into()),
    }
}

fn check_keys(arguments: &serde_json::Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    match arguments.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(format!("unknown argument {key}")),
        None => Ok(()),
    }
}

impl Server {
    pub fn new(config_path: &Path) -> Self {
        Server { config_path: crate::config::absolute(config_path) }
    }

    fn call_tool(&self, name: &str, arguments: &serde_json::Map<String, Value>) -> Result<Value, String> {
        let config = crate::config::load_config(&self.config_path).map_err(|e| format!("config: {}", e.0))?;
        match name {
            "deadair_status" => {
                check_keys(arguments, &[])?;
                crate::status::read_status(&config, None).map_err(|e| e.0)
            }
            "deadair_incidents" => {
                check_keys(arguments, &["state", "limit"])?;
                let open_only = match arguments.get("state") {
                    None => true,
                    Some(Value::String(s)) if s == "open" => true,
                    Some(Value::String(s)) if s == "all" => false,
                    Some(_) => return Err("state must be open or all".into()),
                };
                crate::status::list_incidents(&config, open_only, optional_limit(arguments)?).map_err(|e| e.0)
            }
            "deadair_incident" => {
                check_keys(arguments, &["incident_id"])?;
                let id = arguments.get("incident_id").and_then(Value::as_str).ok_or("incident_id is required")?;
                match crate::status::incident_detail(&config, id).map_err(|e| e.0)? {
                    Some(detail) => Ok(detail),
                    None => Err(format!("incident {} was not found", id.chars().take(64).collect::<String>())),
                }
            }
            "deadair_deliveries" => {
                check_keys(arguments, &["status", "limit"])?;
                let status = match arguments.get("status") {
                    None => None,
                    Some(Value::String(s)) if s == "all" => None,
                    Some(Value::String(s)) if ["pending", "sent", "failed"].contains(&s.as_str()) => Some(s.as_str()),
                    Some(_) => return Err("status must be pending, sent, failed or all".into()),
                };
                crate::status::list_deliveries(&config, status, optional_limit(arguments)?).map_err(|e| e.0)
            }
            "deadair_sources" => {
                check_keys(arguments, &[])?;
                Ok(crate::status::sources(&config))
            }
            _ => Err("unknown tool".into()),
        }
    }

    /// Handle one parsed message; `None` for notifications.
    pub fn dispatch(&self, request: &Value) -> Option<Value> {
        let Some(object) = request.as_object() else {
            return Some(error(Value::Null, -32600, "Invalid Request"));
        };
        let method = object.get("method").and_then(Value::as_str);
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_none() {
            return Some(error(
                object.get("id").cloned().filter(|i| i.is_string() || i.is_i64()).unwrap_or(Value::Null),
                -32600,
                "Invalid Request",
            ));
        }
        let method = method.unwrap_or_default();
        let Some(id) = object.get("id") else {
            return None; // a notification never gets a reply
        };
        if !(id.is_string() || id.is_i64() || id.is_u64() || id.is_null()) {
            return Some(error(Value::Null, -32600, "Invalid Request"));
        }
        let id = id.clone();
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        let reply = |result: Value| json!({"jsonrpc": "2.0", "id": id.clone(), "result": result});
        match method {
            "initialize" => {
                let Some(params) = params.as_object() else {
                    return Some(error(id, -32602, "Invalid params"));
                };
                let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(DEFAULT_VERSION);
                let version = if SUPPORTED.contains(&requested) { requested } else { DEFAULT_VERSION };
                Some(reply(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "dead-air", "title": "Perennos DEAD AIR", "version": VERSION},
                    "instructions": "Read-only views of DEAD AIR's persisted stream health, incidents and alert deliveries. UNKNOWN means missing or inconclusive evidence, never live.",
                })))
            }
            "ping" => Some(reply(json!({}))),
            "tools/list" => {
                if !(params.is_null()
                    || params.as_object().is_some_and(|p| p.keys().all(|k| k == "cursor" || k == "_meta")))
                {
                    return Some(error(id, -32602, "Invalid params"));
                }
                Some(reply(json!({"tools": annotated_tools()})))
            }
            "tools/call" => {
                let Some(params) = params.as_object() else {
                    return Some(error(id, -32602, "Invalid params"));
                };
                if params.keys().any(|k| k != "name" && k != "arguments" && k != "_meta") {
                    return Some(error(id, -32602, "Invalid params"));
                }
                let Some(name) = params.get("name").and_then(Value::as_str) else {
                    return Some(error(id, -32602, "Invalid params"));
                };
                if !tool_definitions().iter().any(|t| t["name"] == name) {
                    return Some(error(id, -32602, "Unknown tool"));
                }
                let empty = serde_json::Map::new();
                let arguments = match params.get("arguments") {
                    None | Some(Value::Null) => &empty,
                    Some(Value::Object(map)) => map,
                    Some(_) => return Some(error(id, -32602, "Invalid params")),
                };
                Some(reply(match self.call_tool(name, arguments) {
                    Ok(value) => tool_result(value, false),
                    Err(message) => tool_result(json!({"error": message}), true),
                }))
            }
            _ => Some(error(id, -32601, "Method not found")),
        }
    }

    fn handle_line(&self, raw: &[u8]) -> Option<Value> {
        let text = std::str::from_utf8(raw).ok()?;
        if text.trim().is_empty() {
            return None;
        }
        match serde_json::from_str::<Value>(text) {
            Ok(request) => self.dispatch(&request),
            Err(_) => Some(error(Value::Null, -32700, "Parse error")),
        }
    }

    /// Serve until stdin closes.
    pub fn serve(&self, input: &mut dyn BufRead, output: &mut dyn Write) -> std::io::Result<()> {
        loop {
            let mut line = Vec::new();
            let mut overflow = false;
            loop {
                let buffer = input.fill_buf()?;
                if buffer.is_empty() {
                    break;
                }
                let (chunk, done) = match buffer.iter().position(|b| *b == b'\n') {
                    Some(i) => (&buffer[..=i], true),
                    None => (buffer, false),
                };
                let used = chunk.len();
                if !overflow {
                    if line.len() + used > MAX_MESSAGE_BYTES + 1 {
                        overflow = true;
                        line.clear();
                    } else {
                        line.extend_from_slice(chunk);
                    }
                }
                input.consume(used);
                if done {
                    break;
                }
            }
            if line.is_empty() && !overflow {
                return Ok(());
            }
            let response = if overflow {
                Some(error(Value::Null, -32700, "Parse error: message exceeds 1 MiB"))
            } else {
                let trimmed: &[u8] =
                    line.strip_suffix(b"\n").map(|l| l.strip_suffix(b"\r").unwrap_or(l)).unwrap_or(&line);
                if std::str::from_utf8(trimmed).is_err() {
                    Some(error(Value::Null, -32700, "Parse error"))
                } else {
                    self.handle_line(trimmed)
                }
            };
            if let Some(response) = response {
                let mut encoded = serde_json::to_vec(&response).unwrap_or_default();
                if encoded.len() > MAX_MESSAGE_BYTES {
                    encoded =
                        serde_json::to_vec(&error(response["id"].clone(), -32603, "Response exceeded its size limit"))
                            .unwrap_or_default();
                }
                encoded.push(b'\n');
                output.write_all(&encoded)?;
                output.flush()?;
            }
        }
    }
}

pub fn run(config_path: &Path) -> i32 {
    let server = Server::new(config_path);
    eprintln!("dead-air mcp: serving read-only tools for {}", server.config_path.display());
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    match server.serve(&mut input, &mut output) {
        Ok(()) => 0,
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => 0,
        Err(_) => 3,
    }
}
