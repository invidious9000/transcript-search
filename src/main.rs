mod index;
mod parser;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use index::TranscriptIndex;

// ── JSON-RPC types ──────────────────────────────────────────────────

#[derive(Deserialize)]
struct JsonRpcMessage {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: Option<String>,
    params: Option<Value>,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl JsonRpcResponse {
    fn success(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: Option<Value>, code: i64, message: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.to_string(),
            }),
        }
    }
}

// ── MCP tool definitions ────────────────────────────────────────────

fn tool_definitions() -> Value {
    json!({
        "tools": [
            {
                "name": "transcript_search",
                "description": "Full-text search across Claude Code conversation transcripts from all accounts. Returns ranked results with file paths, session IDs, timestamps, and highlighted excerpts. Supports AND (default), OR, phrase queries (\"exact phrase\"), and field filtering.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Search query. Terms are ANDed by default. Use quotes for phrases, OR for disjunction."
                        },
                        "account": {
                            "type": "string",
                            "description": "Filter to account: 'claude', 'account2', 'account3', 'codex'"
                        },
                        "project": {
                            "type": "string",
                            "description": "Filter by project path keywords"
                        },
                        "role": {
                            "type": "string",
                            "enum": ["user", "assistant", "thinking", "tool_use", "tool_result"],
                            "description": "Filter by message role/type"
                        },
                        "include_subagents": {
                            "type": "boolean",
                            "description": "Include subagent transcripts (default: true)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max results (default: 20, max: 100)"
                        }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "transcript_context",
                "description": "Get conversation context around a specific point in a transcript file. Use after transcript_search to see surrounding messages.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Path to the JSONL transcript file"
                        },
                        "byte_offset": {
                            "type": "integer",
                            "description": "Byte offset of the target line (from search results)"
                        },
                        "context_lines": {
                            "type": "integer",
                            "description": "Number of JSONL events before/after to include (default: 5)"
                        }
                    },
                    "required": ["file_path", "byte_offset"]
                }
            },
            {
                "name": "transcript_session",
                "description": "Get summary info for a session: first prompt, project, duration, tool usage, message counts.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session UUID"
                        }
                    },
                    "required": ["session_id"]
                }
            },
            {
                "name": "transcript_messages",
                "description": "List messages from a session in chronological order. Returns the conversation flow with role labels and timestamps. Use session_id to find by UUID, or file_path for a known transcript. Large sessions are paginated via offset/limit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session UUID. Resolves to file path(s) via index or filesystem."
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Direct path to a JSONL transcript file (overrides session_id)."
                        },
                        "role": {
                            "type": "string",
                            "enum": ["user", "assistant", "thinking", "tool_use", "tool_result"],
                            "description": "Filter to a specific role (default: all)"
                        },
                        "include_subagents": {
                            "type": "boolean",
                            "description": "Include subagent transcripts (default: false)"
                        },
                        "max_content_length": {
                            "type": "integer",
                            "description": "Max characters per message (default: 500). Use 0 for full content."
                        },
                        "from_end": {
                            "type": "boolean",
                            "description": "Read from the end of the session (tail mode). offset=0 gives last N messages. (default: false)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Skip this many messages (for pagination, default: 0)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max messages to return (default: 50, max: 200)"
                        }
                    }
                }
            },
            {
                "name": "transcript_reindex",
                "description": "Build or incrementally update the transcript search index. First run indexes all transcripts; subsequent runs only process new/modified files.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "full": {
                            "type": "boolean",
                            "description": "Force full reindex, ignoring incremental metadata (default: false)"
                        }
                    }
                }
            },
            {
                "name": "transcript_topics",
                "description": "Extract top terms from a session by frequency analysis. No LLM — pure term counting with stop-word filtering. Shows what a session was about at a glance.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session UUID (uses index for lookup)"
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Direct path to transcript file (overrides session_id)"
                        },
                        "role": {
                            "type": "string",
                            "enum": ["user", "assistant", "thinking", "tool_use"],
                            "description": "Limit analysis to a specific role (default: all except tool_result)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Number of top terms to return (default: 25)"
                        }
                    }
                }
            },
            {
                "name": "transcript_sessions_list",
                "description": "Browse sessions across all accounts, sorted by most recent. Shows date, duration, account, project, session ID, and first prompt. Use to find sessions without knowing keywords.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "account": {
                            "type": "string",
                            "description": "Filter to account: 'claude', 'account2', 'account3', 'codex'"
                        },
                        "project": {
                            "type": "string",
                            "description": "Filter by project name substring (case-insensitive)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Skip this many sessions (default: 0)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max sessions to return (default: 30, max: 100)"
                        }
                    }
                }
            },
            {
                "name": "transcript_stats",
                "description": "Corpus statistics: indexed document count, index size on disk, source file counts per account.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }
        ]
    })
}

// ── Request handling ────────────────────────────────────────────────

fn handle_initialize(id: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "transcript-search",
                "version": "0.1.0"
            }
        }),
    )
}

fn handle_tools_list(id: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse::success(id, tool_definitions())
}

fn handle_tools_call(id: Option<Value>, params: &Value, idx: &mut TranscriptIndex) -> JsonRpcResponse {
    let tool_name = params["name"].as_str().unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match tool_name {
        "transcript_search" => {
            // Auto-index on first search if empty
            if idx.is_empty() {
                tracing::info!("Index is empty — building before first search");
                if let Err(e) = idx.build_index(false) {
                    return tool_response(id, &format!("Auto-index failed: {}", e), true);
                }
            }
            idx.search(&arguments)
        }
        "transcript_context" => idx.context(&arguments),
        "transcript_messages" => idx.messages(&arguments),
        "transcript_session" => idx.session(&arguments),
        "transcript_topics" => idx.topics(&arguments),
        "transcript_sessions_list" => idx.sessions_list(&arguments),
        "transcript_reindex" => idx.reindex(&arguments),
        "transcript_stats" => idx.stats(),
        _ => {
            return tool_response(id, &format!("Unknown tool: {}", tool_name), true);
        }
    };

    match result {
        Ok(text) => tool_response(id, &text, false),
        Err(e) => tool_response(id, &format!("Error: {:#}", e), true),
    }
}

fn tool_response(id: Option<Value>, text: &str, is_error: bool) -> JsonRpcResponse {
    let mut result = json!({
        "content": [{"type": "text", "text": text}]
    });
    if is_error {
        result["isError"] = json!(true);
    }
    JsonRpcResponse::success(id, result)
}

// ── Main loop ───────────────────────────────────────────────────────

fn main() -> Result<()> {
    // Log to stderr (stdout is MCP transport)
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "transcript_search=info".into()),
        )
        .init();

    let home = dirs::home_dir().expect("cannot determine home directory");

    // Discover Claude Code account roots.
    // TRANSCRIPT_SEARCH_ROOTS env var overrides auto-detection.
    // Format: "name=/path,name2=/path2" e.g. "claude=~/.claude,work=~/.claude-work"
    let roots: Vec<(String, PathBuf)> = if let Ok(val) = std::env::var("TRANSCRIPT_SEARCH_ROOTS") {
        val.split(',')
            .filter_map(|entry| {
                let (name, path) = entry.split_once('=')?;
                let expanded = if path.starts_with('~') {
                    home.join(&path[2..])
                } else {
                    PathBuf::from(path)
                };
                Some((name.to_string(), expanded))
            })
            .collect()
    } else {
        // Auto-detect: ~/.claude is always included, plus any ~/.claude-* dirs with projects/
        let mut found = vec![("claude".to_string(), home.join(".claude"))];
        if let Ok(entries) = std::fs::read_dir(&home) {
            let mut extras: Vec<(String, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.starts_with(".claude-")
                        && !name.contains("shared")
                        && e.path().join("projects").exists()
                })
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let label = name.trim_start_matches(".claude-").to_string();
                    (label, e.path())
                })
                .collect();
            extras.sort_by(|a, b| a.0.cmp(&b.0));
            found.extend(extras);
        }
        found
    };

    // Codex CLI root — auto-detect or override via TRANSCRIPT_SEARCH_CODEX_ROOT
    let codex_root = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let default = home.join(".codex");
            if default.join("sessions").exists() { Some(default) } else { None }
        });

    // Index location — override via TRANSCRIPT_SEARCH_INDEX_PATH
    let index_path = std::env::var("TRANSCRIPT_SEARCH_INDEX_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Prefer ~/.claude-shared/transcript-index if it exists, else ~/.local/share/transcript-search
            let shared = home.join(".claude-shared").join("transcript-index");
            if shared.parent().map(|p| p.exists()).unwrap_or(false) {
                shared
            } else {
                home.join(".local").join("share").join("transcript-search").join("index")
            }
        });

    tracing::info!("Roots: {:?}", roots.iter().map(|(n, p)| format!("{}={}", n, p.display())).collect::<Vec<_>>());
    if let Some(ref cr) = codex_root {
        tracing::info!("Codex root: {}", cr.display());
    }
    tracing::info!("Index path: {}", index_path.display());

    let mut idx = TranscriptIndex::open_or_create(&index_path, roots, codex_root)?;

    tracing::info!("transcript-search MCP server ready");

    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("stdin read error: {}", e);
                break;
            }
        };

        if line.is_empty() {
            continue;
        }

        let msg: JsonRpcMessage = match serde_json::from_str(&line) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("invalid JSON-RPC message: {}", e);
                // Send parse error for messages with an id
                let resp = JsonRpcResponse::error(None, -32700, "Parse error");
                let _ = writeln!(out, "{}", serde_json::to_string(&resp)?);
                let _ = out.flush();
                continue;
            }
        };

        let method = match msg.method.as_deref() {
            Some(m) => m,
            None => continue,
        };

        // Notifications (no id) don't get responses
        let response = match method {
            "initialize" => Some(handle_initialize(msg.id)),
            "notifications/initialized" | "notifications/cancelled" => None,
            "tools/list" => Some(handle_tools_list(msg.id)),
            "tools/call" => {
                let params = msg.params.as_ref().cloned().unwrap_or(json!({}));
                Some(handle_tools_call(msg.id, &params, &mut idx))
            }
            "ping" => Some(JsonRpcResponse::success(msg.id, json!({}))),
            _ => {
                tracing::debug!("unhandled method: {}", method);
                if msg.id.is_some() {
                    Some(JsonRpcResponse::error(
                        msg.id,
                        -32601,
                        &format!("Method not found: {}", method),
                    ))
                } else {
                    None
                }
            }
        };

        if let Some(resp) = response {
            writeln!(out, "{}", serde_json::to_string(&resp)?)?;
            out.flush()?;
        }
    }

    tracing::info!("stdin closed, shutting down");
    Ok(())
}
