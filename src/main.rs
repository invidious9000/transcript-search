mod index;
mod knowledge;
mod parser;
mod threads;

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
                "name": "blackbox_search",
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
                            "enum": ["user", "assistant", "thinking", "tool_use", "tool_result", "developer"],
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
                "name": "blackbox_context",
                "description": "Get conversation context around a specific point in a transcript file. Use after blackbox_search to see surrounding messages.",
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
                "name": "blackbox_session",
                "description": "Get summary info for a session: first prompt, project, duration, tool usage, message counts. Accepts UUID or friendly session name.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session UUID or friendly name (e.g. 'bbox', 'claude-test')"
                        }
                    },
                    "required": ["session_id"]
                }
            },
            {
                "name": "blackbox_messages",
                "description": "List messages from a session in chronological order. Returns the conversation flow with role labels and timestamps. Use session_id (UUID or friendly name) to find a session, or file_path for a known transcript. Large sessions are paginated via offset/limit.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "session_id": {
                            "type": "string",
                            "description": "Session UUID or friendly name (e.g. 'bbox', 'claude-test'). Resolves to file path(s) via index or filesystem."
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Direct path to a JSONL transcript file (overrides session_id)."
                        },
                        "role": {
                            "type": "string",
                            "enum": ["user", "assistant", "thinking", "tool_use", "tool_result", "developer"],
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
                "name": "blackbox_reindex",
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
                "name": "blackbox_topics",
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
                "name": "blackbox_sessions_list",
                "description": "Browse sessions across all accounts, sorted by most recent. Shows date, duration, account, project, session ID, friendly name, and first prompt. Use to find sessions without knowing keywords. Supports filtering by friendly session name.",
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
                        "name": {
                            "type": "string",
                            "description": "Filter by friendly session name substring (case-insensitive)"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "Skip this many sessions (default: 0)"
                        },
                        "exclude_session": {
                            "type": "string",
                            "description": "Session UUID to exclude (use to avoid matching your own session)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max sessions to return (default: 30, max: 100)"
                        }
                    }
                }
            },
            {
                "name": "blackbox_stats",
                "description": "Corpus statistics: indexed document count, index size on disk, source file counts per account.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "blackbox_learn",
                "description": "Add or update a knowledge entry. Entries are rendered into CLAUDE.md/AGENTS.md/GEMINI.md.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "The instruction, fact, or preference." },
                        "category": { "type": "string", "enum": ["profile", "convention", "steering", "build", "tool", "memory", "workflow"], "description": "Entry category." },
                        "title": { "type": "string", "description": "Short human-readable title (auto-generated if omitted)." },
                        "scope": { "type": "string", "enum": ["global", "project"], "description": "Global (all projects) or project-specific. Default: global." },
                        "project": { "type": "string", "description": "Project path for project-scoped entries." },
                        "providers": { "type": "array", "items": { "type": "string" }, "description": "Empty = all providers. Non-empty = only these (claude, codex, vibe, gemini)." },
                        "priority": { "type": "string", "enum": ["critical", "standard", "supplementary"], "description": "Default: standard." },
                        "weight": { "type": "integer", "description": "Ordering within priority tier. Lower = first. Default: 100." },
                        "expires_at": { "type": "string", "description": "ISO 8601 expiry time. Null = permanent." },
                        "id": { "type": "string", "description": "If provided, updates existing entry." }
                    },
                    "required": ["content", "category"]
                }
            },
            {
                "name": "blackbox_knowledge",
                "description": "List/search knowledge entries with filters.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "category": { "type": "string", "description": "Filter by category." },
                        "scope": { "type": "string", "description": "Filter: global or project." },
                        "project": { "type": "string", "description": "Filter by project path substring." },
                        "provider": { "type": "string", "description": "Show entries visible to this provider." },
                        "status": { "type": "string", "description": "Filter by status. Default: active." },
                        "approval": { "type": "string", "description": "Filter by approval state." },
                        "query": { "type": "string", "description": "Full-text search within content/title." },
                        "limit": { "type": "integer", "description": "Max results. Default: 50." }
                    }
                }
            },
            {
                "name": "blackbox_forget",
                "description": "Remove or supersede a knowledge entry.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "description": "Entry ID to remove." },
                        "superseded_by": { "type": "string", "description": "If provided, marks as superseded instead of deleted." }
                    },
                    "required": ["id"]
                }
            },
            {
                "name": "blackbox_render",
                "description": "Render knowledge entries into provider markdown files (CLAUDE.md, AGENTS.md, GEMINI.md).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "provider": { "type": "string", "description": "Render for specific provider (claude, agents, gemini) or all." },
                        "project": { "type": "string", "description": "Project directory path." },
                        "dry_run": { "type": "boolean", "description": "Preview without writing files. Default: false." }
                    }
                }
            },
            {
                "name": "blackbox_absorb",
                "description": "Absorb external changes from rendered files back into the knowledge store. Detects unmarked additions (new entries) and missing marked blocks (deletions).",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": { "type": "string", "description": "Project directory path." }
                    },
                    "required": ["project"]
                }
            },
            {
                "name": "blackbox_lint",
                "description": "Health check: find contradictions, stale entries, unverified entries, duplicates.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            },
            {
                "name": "blackbox_review",
                "description": "Review unverified entries (agent_inferred or imported). List, approve, or reject.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["list", "approve", "reject"], "description": "Default: list." },
                        "id": { "type": "string", "description": "Entry ID (required for approve/reject)." }
                    }
                }
            },
            {
                "name": "blackbox_bootstrap",
                "description": "Bootstrap a new repo into the blackbox knowledge system. Scans for existing instruction files (CLAUDE.md, AGENTS.md, .cursorrules, copilot-instructions.md) and returns their contents with classification guidance. The agent then decomposes them into PROJECT.md (project-specific docs) + blackbox_learn entries (cross-project knowledge). Run this once when onboarding a repo.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "project": { "type": "string", "description": "Absolute path to the repo root." }
                    },
                    "required": ["project"]
                }
            },
            {
                "name": "blackbox_remember",
                "description": "Store a fact for on-demand recall only — NOT rendered into CLAUDE.md/AGENTS.md/GEMINI.md. Use this for observations, context, decisions, and notes that should be searchable via blackbox_knowledge but don't need to be in every session's context window.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "The fact, observation, or note to remember." },
                        "category": { "type": "string", "enum": ["profile", "convention", "steering", "build", "tool", "memory", "workflow"], "description": "Default: memory." },
                        "title": { "type": "string", "description": "Short human-readable title." },
                        "scope": { "type": "string", "enum": ["global", "project"], "description": "Default: global." },
                        "project": { "type": "string", "description": "Project path for project-scoped entries." },
                        "decay": { "type": "boolean", "description": "Set false for invariants that should never age out. Default: true." },
                        "review_at": { "type": "string", "description": "ISO 8601 date to revisit this entry." },
                        "expires_at": { "type": "string", "description": "ISO 8601 expiry. Null = permanent." }
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "blackbox_thread",
                "description": "Manage work threads — lightweight continuity tracker for non-dispatchable work (debugging, QC walks, interactive investigations, sideband concerns). Threads connect sessions, handoff docs, and notes into trackable work items below the dispatch pipeline.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["open", "continue", "resolve", "promote"],
                            "description": "open: create new thread. continue: link session/add notes to existing. resolve: mark done. promote: graduated to graph entity."
                        },
                        "id": { "type": "string", "description": "Thread ID (required for continue/resolve/promote)." },
                        "topic": { "type": "string", "description": "Short description of the work (required for open)." },
                        "project": { "type": "string", "description": "Project path or name." },
                        "session_id": { "type": "string", "description": "Link a session to this thread." },
                        "provider": { "type": "string", "description": "Provider of the linked session (claude, codex, etc)." },
                        "session_name": { "type": "string", "description": "Friendly name of the linked session." },
                        "handoff_doc": { "type": "string", "description": "Path to handoff/context document." },
                        "note": { "type": "string", "description": "Add a note (status update, observation, decision)." },
                        "promoted_to": { "type": "string", "description": "Graph entity reference (required for promote)." }
                    },
                    "required": ["action"]
                }
            },
            {
                "name": "blackbox_thread_list",
                "description": "List and scan work threads. Shows open/active/stale threads by default. Use stale_days to find abandoned work.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "status": { "type": "string", "enum": ["open", "active", "stale", "resolved", "promoted"], "description": "Filter by status." },
                        "project": { "type": "string", "description": "Filter by project name substring." },
                        "stale_days": { "type": "integer", "description": "Only show threads with no activity in this many days." },
                        "include_resolved": { "type": "boolean", "description": "Include resolved/promoted threads (default: false)." }
                    }
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
                "name": "blackbox",
                "version": "0.1.0"
            }
        }),
    )
}

fn handle_tools_list(id: Option<Value>) -> JsonRpcResponse {
    JsonRpcResponse::success(id, tool_definitions())
}

fn handle_tools_call(
    id: Option<Value>,
    params: &Value,
    idx: &mut TranscriptIndex,
    kb: &mut knowledge::Knowledge,
    th: &mut threads::Threads,
) -> JsonRpcResponse {
    let tool_name = params["name"].as_str().unwrap_or("");
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match tool_name {
        // Transcript search tools
        "blackbox_search" => {
            if idx.is_empty() {
                tracing::info!("Index is empty — building before first search");
                if let Err(e) = idx.build_index(false) {
                    return tool_response(id, &format!("Auto-index failed: {}", e), true);
                }
            }
            idx.search(&arguments)
        }
        "blackbox_context" => idx.context(&arguments),
        "blackbox_messages" => idx.messages(&arguments),
        "blackbox_session" => idx.session(&arguments),
        "blackbox_topics" => idx.topics(&arguments),
        "blackbox_sessions_list" => idx.sessions_list(&arguments),
        "blackbox_reindex" => idx.reindex(&arguments),
        "blackbox_stats" => idx.stats(),

        // Knowledge store tools
        "blackbox_learn" => kb.learn(&arguments, false),
        "blackbox_bootstrap" => kb.bootstrap(&arguments),
        "blackbox_remember" => kb.remember(&arguments, false),
        "blackbox_knowledge" => kb.list(&arguments),
        "blackbox_forget" => kb.forget(&arguments),
        "blackbox_render" => kb.render(&arguments),
        "blackbox_absorb" => kb.absorb(&arguments),
        "blackbox_lint" => kb.lint(),
        "blackbox_review" => kb.review(&arguments),

        // Thread tools
        "blackbox_thread" => th.thread(&arguments),
        "blackbox_thread_list" => th.thread_list(&arguments),

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

    // Open knowledge store
    let kb_path = home.join(".claude-shared").join("blackbox-knowledge.json");
    let mut kb = knowledge::Knowledge::open(&kb_path)?;
    tracing::info!("Knowledge store: {}", kb_path.display());

    // Open thread store
    let th_path = home.join(".claude-shared").join("blackbox-threads.json");
    let mut th = threads::Threads::open(&th_path)?;
    tracing::info!("Thread store: {}", th_path.display());

    // Spawn background reindex thread (every 2 minutes)
    let reindex_interval = std::env::var("BLACKBOX_REINDEX_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);
    index::spawn_reindex_thread(
        idx.index_handle(),
        idx.reindex_config(),
        idx.field_handles(),
        std::time::Duration::from_secs(reindex_interval),
    );

    tracing::info!("blackbox MCP server ready (auto-reindex every {}s)", reindex_interval);

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
                Some(handle_tools_call(msg.id, &params, &mut idx, &mut kb, &mut th))
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
