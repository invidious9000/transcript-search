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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_user_string_message() {
        let line = json!({
            "type": "user",
            "sessionId": "session-123",
            "timestamp": "2026-04-15T12:00:00Z",
            "message": {
                "role": "user",
                "content": "Hello world"
            }
        }).to_string();

        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role, "user");
        assert_eq!(events[0].content, "Hello world");
        assert_eq!(events[0].session_id, "session-123");
    }

    #[test]
    fn test_parse_user_tool_result() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "Analyzing..." },
                    { "type": "tool_result", "tool_use_id": "call_1", "content": "Success" }
                ]
            }
        }).to_string();

        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].role, "user");
        assert_eq!(events[0].content, "Analyzing...");
        assert_eq!(events[1].role, "tool_result");
        assert!(events[1].content.contains("result:call_1 Success"));
    }

    #[test]
    fn test_parse_assistant_message() {
        let line = json!({
            "type": "assistant",
            "sessionId": "s1",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "I should say hello" },
                    { "type": "text", "text": "Hello!" },
                    { "type": "tool_use", "name": "read_file", "input": {"path": "foo.rs"}, "id": "t1" }
                ]
            }
        }).to_string();

        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].role, "thinking");
        assert_eq!(events[0].content, "I should say hello");
        assert_eq!(events[1].role, "assistant");
        assert_eq!(events[1].content, "Hello!");
        assert_eq!(events[2].role, "tool_use");
        assert!(events[2].content.contains("tool:read_file"));
        assert!(events[2].content.contains("foo.rs"));
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(parse_transcript_line("not json").is_empty());
        assert!(parse_transcript_line("{}").is_empty());
    }

    #[test]
    fn test_parse_metadata() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "timestamp": "ts",
            "gitBranch": "main",
            "cwd": "/repo",
            "message": { "content": "hi" }
        }).to_string();

        let events = parse_transcript_line(&line);
        assert_eq!(events[0].session_id, "s1");
        assert_eq!(events[0].timestamp, Some("ts".to_string()));
        assert_eq!(events[0].git_branch, Some("main".to_string()));
        assert_eq!(events[0].cwd, Some("/repo".to_string()));
        assert!(!events[0].is_subagent);
    }

    #[test]
    fn test_parse_subagent() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "isSidechain": true,
            "slug": "researcher",
            "message": { "content": "hi" }
        }).to_string();

        let events = parse_transcript_line(&line);
        assert!(events[0].is_subagent);
        assert_eq!(events[0].agent_slug, Some("researcher".to_string()));
    }

    #[test]
    fn test_parse_history_line() {
        let line = json!({
            "display": "ls command",
            "sessionId": "s1",
            "project": "/p",
            "timestamp": 1600000000000u64
        }).to_string();

        let events = parse_history_line(&line);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "ls command");
        assert_eq!(events[0].role, "user");
        assert_eq!(events[0].cwd, Some("/p".to_string()));
    }

    #[test]
    fn test_parse_codex_line() {
        let meta = json!({
            "type": "session_meta",
            "payload": { "cwd": "/repo", "base_instructions": "Be helpful" }
        }).to_string();
        let ev1 = parse_codex_line(&meta, "s1");
        assert_eq!(ev1.len(), 1);
        assert_eq!(ev1[0].role, "developer");
        assert_eq!(ev1[0].content, "Be helpful");

        let resp = json!({
            "type": "response_item",
            "payload": {
                "role": "assistant",
                "content": [
                    { "type": "output_text", "text": "Done" },
                    { "type": "function_call", "name": "ls", "arguments": "{\"path\": \".\"}" }
                ]
            }
        }).to_string();
        let ev2 = parse_codex_line(&resp, "s1");
        assert_eq!(ev2.len(), 2);
        assert_eq!(ev2[0].role, "assistant");
        assert_eq!(ev2[1].role, "tool_use");
        assert!(ev2[1].content.contains("tool:ls"));
    }

    #[test]
    fn test_extract_tool_result_text() {
        let b1 = json!({ "content": "direct string" });
        assert_eq!(extract_tool_result_text(&b1), Some("direct string".to_string()));

        let b2 = json!({ "content": [ { "text": "part1" }, { "text": "part2" } ] });
        assert_eq!(extract_tool_result_text(&b2), Some("part1\npart2".to_string()));

        let b3 = json!({ "content": [] });
        assert_eq!(extract_tool_result_text(&b3), None);
    }

    #[test]
    fn test_truncate() {
        let short = "abc";
        assert_eq!(truncate(short), "abc");

        let long = "a".repeat(13000);
        let tr = truncate(&long);
        assert!(tr.ends_with("...[truncated]"));
        assert!(tr.len() < 13000);

        // UTF-8 boundary test (Emoji is 4 bytes: 🦀 = \u{1F980})
        let emoji_repeated = "🦀".repeat(4000); 
        let tr_emoji = truncate(&emoji_repeated);
        assert!(tr_emoji.ends_with("...[truncated]"));
        // Ensure it doesn't panic and result is valid string
        assert!(!tr_emoji.is_empty());
    }

    #[test]
    fn test_parse_codex_history_line() {
        let line = json!({
            "session_id": "s1",
            "ts": 1600000000,
            "text": "Hello Codex"
        }).to_string();
        let events = parse_codex_history_line(&line);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "Hello Codex");
        assert_eq!(events[0].timestamp, Some("1600000000".to_string()));
    }

    #[test]
    fn test_parse_codex_direct_string_content() {
        let line = json!({
            "type": "response_item",
            "payload": {
                "role": "user",
                "content": "direct text"
            }
        }).to_string();
        let events = parse_codex_line(&line, "s1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].content, "direct text");
    }

    #[test]
    fn test_parse_codex_reasoning() {
        let line = json!({
            "type": "response_item",
            "payload": {
                "role": "assistant",
                "content": [
                    { "type": "reasoning", "text": "Thinking hard" }
                ]
            }
        }).to_string();
        let events = parse_codex_line(&line, "s1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role, "thinking");
        assert_eq!(events[0].content, "Thinking hard");
    }

    #[test]
    fn test_parse_codex_function_call_output() {
        let line = json!({
            "type": "response_item",
            "payload": {
                "role": "assistant",
                "content": [
                    { "type": "function_call_output", "output": "Tool result" }
                ]
            }
        }).to_string();
        let events = parse_codex_line(&line, "s1");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].role, "tool_result");
        assert!(events[0].content.contains("result: Tool result"));
    }

    #[test]
    fn test_parse_assistant_no_content() {
        let line = json!({
            "type": "assistant",
            "sessionId": "s1",
            "message": { "role": "assistant" }
        }).to_string();
        assert!(parse_transcript_line(&line).is_empty());
    }

    #[test]
    fn test_parse_user_array_text() {
        let line = json!({
            "type": "user",
            "sessionId": "s1",
            "message": {
                "role": "user",
                "content": [
                    { "type": "text", "text": "Part 1" },
                    { "type": "text", "text": "Part 2" }
                ]
            }
        }).to_string();
        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].content, "Part 1");
        assert_eq!(events[1].content, "Part 2");
    }

    #[test]
    fn test_parse_assistant_tool_use_truncation() {
        let long_input = "a".repeat(15000);
        let line = json!({
            "type": "assistant",
            "sessionId": "s1",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "name": "foo", "input": {"data": long_input} }
                ]
            }
        }).to_string();
        let events = parse_transcript_line(&line);
        assert_eq!(events.len(), 1);
        assert!(events[0].content.contains("...[truncated]"));
    }
}
