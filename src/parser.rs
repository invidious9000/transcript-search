use serde_json::Value;

/// A single searchable unit extracted from a JSONL transcript event.
pub struct ParsedEvent {
    pub role: String,
    pub content: String,
    pub session_id: String,
    pub timestamp: Option<String>,
    pub git_branch: Option<String>,
    pub is_subagent: bool,
    pub agent_slug: Option<String>,
    pub cwd: Option<String>,
}

const MAX_CONTENT_LEN: usize = 12_000;

/// Parse a single JSONL line into zero or more searchable events.
pub fn parse_transcript_line(line: &str) -> Vec<ParsedEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let event_type = match v["type"].as_str() {
        Some(t) => t,
        None => return vec![],
    };

    let session_id = v["sessionId"].as_str().unwrap_or("").to_string();
    let timestamp = v["timestamp"].as_str().map(String::from);
    let git_branch = v["gitBranch"].as_str().map(String::from);
    let is_subagent = v.get("isSidechain").and_then(|v| v.as_bool()).unwrap_or(false)
        || v.get("agentId").is_some();
    let agent_slug = v["slug"].as_str().map(String::from);
    let cwd = v["cwd"].as_str().map(String::from);

    let base = EventBase {
        session_id,
        timestamp,
        git_branch,
        is_subagent,
        agent_slug,
        cwd,
    };

    match event_type {
        "user" => parse_user_message(&v["message"], &base),
        "assistant" => parse_assistant_message(&v["message"], &base),
        _ => vec![],
    }
}

/// Parse a history.jsonl line (different schema: {display, project, sessionId, timestamp}).
pub fn parse_history_line(line: &str) -> Vec<ParsedEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let display = match v["display"].as_str() {
        Some(d) if !d.is_empty() => d,
        _ => return vec![],
    };

    let session_id = v["sessionId"].as_str().unwrap_or("").to_string();
    let project = v["project"].as_str().map(String::from);

    // Timestamp is epoch millis (number), convert to ISO-ish string
    let timestamp = v["timestamp"].as_u64().map(|ms| {
        let secs = ms / 1000;
        format!("{}", secs)
    });

    vec![ParsedEvent {
        role: "user".to_string(),
        content: truncate(display),
        session_id,
        timestamp,
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: project,
    }]
}

struct EventBase {
    session_id: String,
    timestamp: Option<String>,
    git_branch: Option<String>,
    is_subagent: bool,
    agent_slug: Option<String>,
    cwd: Option<String>,
}

fn parse_user_message(message: &Value, base: &EventBase) -> Vec<ParsedEvent> {
    match &message["content"] {
        // String content = human-typed message
        Value::String(s) if !s.is_empty() => {
            vec![make_event("user", &truncate(s), base)]
        }
        // Array content = tool results (sent as user role in Claude API)
        Value::Array(blocks) => {
            let mut events = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            if !text.is_empty() {
                                events.push(make_event("user", &truncate(text), base));
                            }
                        }
                    }
                    Some("tool_result") => {
                        if let Some(text) = extract_tool_result_text(block) {
                            if !text.is_empty() {
                                let tool_id = block["tool_use_id"]
                                    .as_str()
                                    .unwrap_or("?");
                                let content = format!("result:{} {}", tool_id, truncate(&text));
                                events.push(make_event("tool_result", &content, base));
                            }
                        }
                    }
                    _ => {}
                }
            }
            events
        }
        _ => vec![],
    }
}

fn parse_assistant_message(message: &Value, base: &EventBase) -> Vec<ParsedEvent> {
    let blocks = match message["content"].as_array() {
        Some(b) => b,
        None => return vec![],
    };

    let mut events = Vec::new();
    for block in blocks {
        let block_type = match block["type"].as_str() {
            Some(t) => t,
            None => continue,
        };

        match block_type {
            "text" => {
                if let Some(text) = block["text"].as_str() {
                    if !text.is_empty() {
                        events.push(make_event("assistant", &truncate(text), base));
                    }
                }
            }
            "thinking" => {
                if let Some(thinking) = block["thinking"].as_str() {
                    if !thinking.is_empty() {
                        events.push(make_event("thinking", &truncate(thinking), base));
                    }
                }
            }
            "tool_use" => {
                let tool_name = block["name"].as_str().unwrap_or("unknown");
                let input_str = serde_json::to_string(&block["input"]).unwrap_or_default();
                let content = format!("tool:{} {}", tool_name, truncate(&input_str));
                events.push(make_event("tool_use", &content, base));
            }
            _ => {}
        }
    }
    events
}

pub fn extract_tool_result_text(block: &Value) -> Option<String> {
    match &block["content"] {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => {
            let texts: Vec<&str> = parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect();
            if texts.is_empty() {
                None
            } else {
                Some(texts.join("\n"))
            }
        }
        _ => None,
    }
}

// ── Codex CLI parser ────────────────────────────────────────────────

/// Parse a Codex CLI JSONL line: {timestamp, type, payload}.
/// Session ID is NOT per-line in Codex — caller must supply it from session_meta or filename.
pub fn parse_codex_line(line: &str, session_id: &str) -> Vec<ParsedEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let event_type = match v["type"].as_str() {
        Some(t) => t,
        None => return vec![],
    };

    let timestamp = v["timestamp"].as_str().map(String::from);

    match event_type {
        "session_meta" => {
            // Extract cwd from session metadata — useful for project association
            let cwd = v["payload"]["cwd"].as_str().map(String::from);
            let base_instructions = v["payload"]["base_instructions"].as_str().unwrap_or("");
            if base_instructions.is_empty() {
                return vec![];
            }
            let base = EventBase {
                session_id: session_id.to_string(),
                timestamp,
                git_branch: None,
                is_subagent: false,
                agent_slug: None,
                cwd,
            };
            vec![make_event("developer", &truncate(base_instructions), &base)]
        }
        "response_item" => {
            let payload = &v["payload"];
            let role = payload["role"].as_str().unwrap_or("");
            let cwd = None; // Not per-message in Codex

            let base = EventBase {
                session_id: session_id.to_string(),
                timestamp,
                git_branch: None,
                is_subagent: false,
                agent_slug: None,
                cwd,
            };

            match role {
                "user" => parse_codex_content_blocks(payload, "user", &base),
                "assistant" => parse_codex_content_blocks(payload, "assistant", &base),
                "developer" => parse_codex_content_blocks(payload, "developer", &base),
                _ => vec![],
            }
        }
        _ => vec![], // Skip event_msg, turn_context, etc.
    }
}

/// Parse Codex history.jsonl: {session_id, ts, text}
pub fn parse_codex_history_line(line: &str) -> Vec<ParsedEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let text = match v["text"].as_str() {
        Some(t) if !t.is_empty() => t,
        _ => return vec![],
    };

    let session_id = v["session_id"].as_str().unwrap_or("").to_string();
    let timestamp = v["ts"].as_u64().map(|s| format!("{}", s));

    vec![ParsedEvent {
        role: "user".to_string(),
        content: truncate(text),
        session_id,
        timestamp,
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: None,
    }]
}

fn parse_codex_content_blocks(payload: &Value, role: &str, base: &EventBase) -> Vec<ParsedEvent> {
    let content = match payload["content"].as_array() {
        Some(c) => c,
        None => {
            // Sometimes content is a string directly
            if let Some(s) = payload["content"].as_str() {
                if !s.is_empty() {
                    return vec![make_event(role, &truncate(s), base)];
                }
            }
            return vec![];
        }
    };

    let mut events = Vec::new();
    for block in content {
        let block_type = block["type"].as_str().unwrap_or("");
        match block_type {
            "input_text" | "output_text" => {
                if let Some(text) = block["text"].as_str() {
                    if !text.is_empty() {
                        events.push(make_event(role, &truncate(text), base));
                    }
                }
            }
            "function_call" => {
                let name = block["name"].as_str().unwrap_or("unknown");
                let args = block["arguments"].as_str().unwrap_or("{}");
                let content = format!("tool:{} {}", name, truncate(args));
                events.push(make_event("tool_use", &content, base));
            }
            "function_call_output" => {
                let output = block["output"].as_str().unwrap_or("");
                if !output.is_empty() {
                    let content = format!("result: {}", truncate(output));
                    events.push(make_event("tool_result", &content, base));
                }
            }
            "reasoning" => {
                // Codex reasoning/thinking — map to "thinking"
                if let Some(text) = block["text"].as_str() {
                    if !text.is_empty() {
                        events.push(make_event("thinking", &truncate(text), base));
                    }
                }
            }
            _ => {}
        }
    }
    events
}

fn make_event(role: &str, content: &str, base: &EventBase) -> ParsedEvent {
    ParsedEvent {
        role: role.to_string(),
        content: content.to_string(),
        session_id: base.session_id.clone(),
        timestamp: base.timestamp.clone(),
        git_branch: base.git_branch.clone(),
        is_subagent: base.is_subagent,
        agent_slug: base.agent_slug.clone(),
        cwd: base.cwd.clone(),
    }
}

fn truncate(s: &str) -> String {
    if s.len() <= MAX_CONTENT_LEN {
        s.to_string()
    } else {
        let mut end = MAX_CONTENT_LEN;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...[truncated]", &s[..end])
    }
}
