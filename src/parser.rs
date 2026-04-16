use serde_json::Value;

/// The role associated with a single transcript event. All provider
/// formats (Claude Code, Codex CLI, history.jsonl) normalize into this
/// fixed set; parsers that encounter a role outside this set return no
/// event rather than inventing one.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash,
    strum::EnumString, strum::AsRefStr, strum::Display,
)]
#[strum(serialize_all = "snake_case")]
pub enum MessageRole {
    /// Human-typed input.
    User,
    /// Model output.
    Assistant,
    /// Model reasoning / thinking blocks.
    Thinking,
    /// Tool invocation by the model.
    ToolUse,
    /// Tool output returned to the model.
    ToolResult,
    /// System / developer context (Codex-specific).
    Developer,
}

/// A single searchable unit extracted from a JSONL transcript event.
pub struct ParsedEvent {
    pub role: MessageRole,
    pub content: String,
    pub session_id: String,
    pub timestamp: Option<String>,
    pub git_branch: Option<String>,
    pub is_subagent: bool,
    pub agent_slug: Option<String>,
    pub cwd: Option<String>,
}

const MAX_CONTENT_LEN: usize = 12_000;

// ═══════════════════════════════════════════════════════════════════════
// Rich transcript event model — consumed by the `bro tail` TUI.
// Indexing stays on ParsedEvent via TranscriptEvent::to_parsed().
// ═══════════════════════════════════════════════════════════════════════

/// Richer, structured transcript event. Preserves tool-call structure,
/// thinking/text separation, and out-of-band signals (compaction, hooks,
/// system-reminders, etc.) that inform *why* the agent is reasoning.
#[derive(Debug, Clone)]
pub struct TranscriptEvent {
    pub role: MessageRole,
    pub session_id: String,
    pub timestamp: Option<String>,
    pub git_branch: Option<String>,
    pub is_subagent: bool,
    pub agent_slug: Option<String>,
    pub cwd: Option<String>,
    pub parent_tool_use_id: Option<String>,
    pub detail: EventDetail,
}

#[derive(Debug, Clone)]
pub enum EventDetail {
    Text { text: String },
    Thinking { text: String },
    ToolUse {
        name: String,
        target: String,
        tool_use_id: Option<String>,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        is_error: bool,
        exit_code: Option<i32>,
        size: usize,
        preview: String,
    },
    SystemSignal {
        kind: SystemSignalKind,
        summary: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemSignalKind {
    SessionInit,
    SessionResumed,
    Compaction,
    HookFired,
    SystemReminder,
    PermissionDenied,
    RateLimitHit,
    UserCommand,
    SubagentLaunched,
    Other,
}

impl TranscriptEvent {
    /// Project onto the flat ParsedEvent shape used by the tantivy indexer.
    /// SystemSignal events are not indexed by default.
    pub fn to_parsed(&self) -> Option<ParsedEvent> {
        let content = match &self.detail {
            EventDetail::Text { text } => truncate(text),
            EventDetail::Thinking { text } => truncate(text),
            EventDetail::ToolUse { name, input, .. } => {
                let input_str = serde_json::to_string(input).unwrap_or_default();
                format!("tool:{} {}", name, truncate(&input_str))
            }
            EventDetail::ToolResult { tool_use_id, preview, .. } => {
                format!("result:{} {}", tool_use_id, truncate(preview))
            }
            EventDetail::SystemSignal { .. } => return None,
        };
        Some(ParsedEvent {
            role: self.role,
            content,
            session_id: self.session_id.clone(),
            timestamp: self.timestamp.clone(),
            git_branch: self.git_branch.clone(),
            is_subagent: self.is_subagent,
            agent_slug: self.agent_slug.clone(),
            cwd: self.cwd.clone(),
        })
    }
}

/// Best-effort extraction of a human-readable "what is this tool doing"
/// string from a tool-use input payload. Per-tool rules fall back to the
/// first non-empty string-valued field.
pub fn extract_tool_target(tool_name: &str, input: &Value) -> String {
    let explicit: Option<&str> = match tool_name {
        "Bash" | "bash" | "shell" => input.get("command").and_then(|v| v.as_str()),
        "Read" | "Write" | "Edit" | "read" | "write" | "edit" => {
            input.get("file_path").and_then(|v| v.as_str())
        }
        "NotebookEdit" => input.get("notebook_path").and_then(|v| v.as_str()),
        "Grep" | "grep" => input.get("pattern").and_then(|v| v.as_str()),
        "Glob" | "glob" => input.get("pattern").and_then(|v| v.as_str()),
        "WebFetch" => input.get("url").and_then(|v| v.as_str()),
        "WebSearch" => input.get("query").and_then(|v| v.as_str()),
        "Task" | "TaskCreate" => input
            .get("description")
            .or_else(|| input.get("subject"))
            .and_then(|v| v.as_str()),
        "TaskUpdate" => input.get("taskId").and_then(|v| v.as_str()),
        "ScheduleWakeup" => input.get("reason").and_then(|v| v.as_str()),
        "Skill" => input.get("skill").and_then(|v| v.as_str()),
        "ToolSearch" => input.get("query").and_then(|v| v.as_str()),
        _ => None,
    };
    if let Some(s) = explicit {
        return oneline_snippet(s, 160);
    }
    if let Some(obj) = input.as_object() {
        for (_k, v) in obj {
            if let Some(s) = v.as_str() {
                if !s.is_empty() {
                    return oneline_snippet(s, 160);
                }
            }
        }
    }
    String::new()
}

fn oneline_snippet(s: &str, max_chars: usize) -> String {
    let one = s.replace('\n', " ");
    let count = one.chars().count();
    if count <= max_chars {
        one
    } else {
        let mut out: String = one.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// Best-effort exit code extraction from a Bash-style tool result.
/// Returns None if not present or not parseable.
fn extract_exit_code(text: &str) -> Option<i32> {
    // Match "exit code <N>", "exit_code=<N>", "(exit code <N>)"
    let lower = text.to_ascii_lowercase();
    let marker = lower.find("exit code").or_else(|| lower.find("exit_code"))?;
    let rest = &lower[marker..];
    let digits: String = rest
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '-')
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

struct RichBase {
    session_id: String,
    timestamp: Option<String>,
    git_branch: Option<String>,
    is_subagent: bool,
    agent_slug: Option<String>,
    cwd: Option<String>,
    parent_tool_use_id: Option<String>,
}

fn make_rich(role: MessageRole, detail: EventDetail, base: &RichBase) -> TranscriptEvent {
    TranscriptEvent {
        role,
        session_id: base.session_id.clone(),
        timestamp: base.timestamp.clone(),
        git_branch: base.git_branch.clone(),
        is_subagent: base.is_subagent,
        agent_slug: base.agent_slug.clone(),
        cwd: base.cwd.clone(),
        parent_tool_use_id: base.parent_tool_use_id.clone(),
        detail,
    }
}

/// Detect steering signals buried inside a "user" text message:
/// `<system-reminder>` injections and `<command-name>` slash commands.
fn detect_user_signal(text: &str) -> Option<SystemSignalKind> {
    if text.contains("<system-reminder>") {
        return Some(SystemSignalKind::SystemReminder);
    }
    if text.contains("<command-name>") || text.contains("<local-command-stdout>") {
        return Some(SystemSignalKind::UserCommand);
    }
    None
}

// ── Rich parser: Claude Code JSONL ──────────────────────────────────

pub fn parse_transcript_line_rich(line: &str) -> Vec<TranscriptEvent> {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let event_type = match v["type"].as_str() {
        Some(t) => t,
        None => return vec![],
    };
    let base = RichBase {
        session_id: v["sessionId"].as_str().unwrap_or("").to_string(),
        timestamp: v["timestamp"].as_str().map(String::from),
        git_branch: v["gitBranch"].as_str().map(String::from),
        is_subagent: v.get("isSidechain").and_then(|x| x.as_bool()).unwrap_or(false)
            || v.get("agentId").is_some(),
        agent_slug: v["slug"].as_str().map(String::from),
        cwd: v["cwd"].as_str().map(String::from),
        parent_tool_use_id: v["parentUuid"].as_str().map(String::from),
    };
    match event_type {
        "user" => parse_user_message_rich(&v["message"], &base),
        "assistant" => parse_assistant_message_rich(&v["message"], &base),
        "system" => parse_system_event_rich(&v, &base),
        "summary" => vec![make_rich(
            MessageRole::Developer,
            EventDetail::SystemSignal {
                kind: SystemSignalKind::Compaction,
                summary: v["summary"].as_str().unwrap_or("context compacted").to_string(),
            },
            &base,
        )],
        _ => vec![],
    }
}

fn parse_user_message_rich(message: &Value, base: &RichBase) -> Vec<TranscriptEvent> {
    match &message["content"] {
        Value::String(s) if !s.is_empty() => {
            if let Some(kind) = detect_user_signal(s) {
                return vec![make_rich(
                    MessageRole::User,
                    EventDetail::SystemSignal { kind, summary: s.clone() },
                    base,
                )];
            }
            vec![make_rich(MessageRole::User, EventDetail::Text { text: s.clone() }, base)]
        }
        Value::Array(blocks) => {
            let mut out = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            if !text.is_empty() {
                                if let Some(kind) = detect_user_signal(text) {
                                    out.push(make_rich(
                                        MessageRole::User,
                                        EventDetail::SystemSignal { kind, summary: text.into() },
                                        base,
                                    ));
                                } else {
                                    out.push(make_rich(
                                        MessageRole::User,
                                        EventDetail::Text { text: text.into() },
                                        base,
                                    ));
                                }
                            }
                        }
                    }
                    Some("tool_result") => {
                        let tool_use_id = block["tool_use_id"].as_str().unwrap_or("?").to_string();
                        let is_error = block["is_error"].as_bool().unwrap_or(false);
                        let text = extract_tool_result_text(block).unwrap_or_default();
                        let size = text.len();
                        let preview = oneline_snippet(&text, 200);
                        let exit_code = extract_exit_code(&text);
                        out.push(make_rich(
                            MessageRole::ToolResult,
                            EventDetail::ToolResult {
                                tool_use_id, is_error, exit_code, size, preview,
                            },
                            base,
                        ));
                    }
                    _ => {}
                }
            }
            out
        }
        _ => vec![],
    }
}

fn parse_assistant_message_rich(message: &Value, base: &RichBase) -> Vec<TranscriptEvent> {
    let blocks = match message["content"].as_array() {
        Some(b) => b,
        None => return vec![],
    };
    let mut out = Vec::new();
    for block in blocks {
        let block_type = match block["type"].as_str() {
            Some(t) => t,
            None => continue,
        };
        match block_type {
            "text" => {
                if let Some(text) = block["text"].as_str() {
                    if !text.is_empty() {
                        out.push(make_rich(
                            MessageRole::Assistant,
                            EventDetail::Text { text: text.into() },
                            base,
                        ));
                    }
                }
            }
            "thinking" => {
                if let Some(t) = block["thinking"].as_str() {
                    if !t.is_empty() {
                        out.push(make_rich(
                            MessageRole::Thinking,
                            EventDetail::Thinking { text: t.into() },
                            base,
                        ));
                    }
                }
            }
            "tool_use" => {
                let name = block["name"].as_str().unwrap_or("unknown").to_string();
                let tool_use_id = block["id"].as_str().map(String::from);
                let input = block["input"].clone();
                let target = extract_tool_target(&name, &input);
                out.push(make_rich(
                    MessageRole::ToolUse,
                    EventDetail::ToolUse { name, target, tool_use_id, input },
                    base,
                ));
            }
            _ => {}
        }
    }
    out
}

fn parse_system_event_rich(v: &Value, base: &RichBase) -> Vec<TranscriptEvent> {
    let subtype = v["subtype"].as_str().unwrap_or("");
    match subtype {
        "init" => {
            let model = v["model"].as_str().unwrap_or("?");
            let cwd = v["cwd"].as_str().unwrap_or("?");
            let tools = v["tools"].as_array().map(|a| a.len()).unwrap_or(0);
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal {
                    kind: SystemSignalKind::SessionInit,
                    summary: format!("session init: {model} in {cwd} ({tools} tools)"),
                },
                base,
            )]
        }
        "hook_started" | "hook_response" => {
            let hook = v["hook_name"].as_str().unwrap_or("?");
            let outcome = v["outcome"].as_str().unwrap_or("");
            let summary = if outcome.is_empty() {
                format!("hook {subtype}: {hook}")
            } else {
                format!("hook {hook}: {outcome}")
            };
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal { kind: SystemSignalKind::HookFired, summary },
                base,
            )]
        }
        _ => vec![],
    }
}

// ── Rich parser: Codex CLI JSONL ────────────────────────────────────

pub fn parse_codex_line_rich(line: &str, session_id: &str) -> Vec<TranscriptEvent> {
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
            let cwd = v["payload"]["cwd"].as_str().map(String::from);
            let model = v["payload"]["model"].as_str().unwrap_or("?");
            let base = RichBase {
                session_id: session_id.into(), timestamp,
                git_branch: None, is_subagent: false, agent_slug: None, cwd,
                parent_tool_use_id: None,
            };
            vec![make_rich(
                MessageRole::Developer,
                EventDetail::SystemSignal {
                    kind: SystemSignalKind::SessionInit,
                    summary: format!("codex session: {model}"),
                },
                &base,
            )]
        }
        "response_item" => {
            let payload = &v["payload"];
            let role = payload["role"].as_str().unwrap_or("");
            let base = RichBase {
                session_id: session_id.into(), timestamp,
                git_branch: None, is_subagent: false, agent_slug: None, cwd: None,
                parent_tool_use_id: None,
            };
            match role {
                "user" => parse_codex_content_rich(payload, MessageRole::User, &base),
                "assistant" => parse_codex_content_rich(payload, MessageRole::Assistant, &base),
                "developer" => parse_codex_content_rich(payload, MessageRole::Developer, &base),
                _ => vec![],
            }
        }
        _ => vec![],
    }
}

fn parse_codex_content_rich(
    payload: &Value,
    role: MessageRole,
    base: &RichBase,
) -> Vec<TranscriptEvent> {
    let content = match payload["content"].as_array() {
        Some(c) => c,
        None => {
            if let Some(s) = payload["content"].as_str() {
                if !s.is_empty() {
                    return vec![make_rich(role, EventDetail::Text { text: s.into() }, base)];
                }
            }
            return vec![];
        }
    };
    let mut out = Vec::new();
    for block in content {
        let block_type = block["type"].as_str().unwrap_or("");
        match block_type {
            "input_text" | "output_text" => {
                if let Some(text) = block["text"].as_str() {
                    if !text.is_empty() {
                        if let Some(kind) = detect_user_signal(text) {
                            out.push(make_rich(
                                role,
                                EventDetail::SystemSignal { kind, summary: text.into() },
                                base,
                            ));
                        } else {
                            out.push(make_rich(role, EventDetail::Text { text: text.into() }, base));
                        }
                    }
                }
            }
            "function_call" => {
                let name = block["name"].as_str().unwrap_or("unknown").to_string();
                let tool_use_id = block["call_id"].as_str().map(String::from);
                let args_str = block["arguments"].as_str().unwrap_or("{}");
                let input: Value = serde_json::from_str(args_str).unwrap_or(Value::Null);
                let target = extract_tool_target(&name, &input);
                out.push(make_rich(
                    MessageRole::ToolUse,
                    EventDetail::ToolUse { name, target, tool_use_id, input },
                    base,
                ));
            }
            "function_call_output" => {
                let tool_use_id = block["call_id"].as_str().unwrap_or("?").to_string();
                let output = block["output"].as_str().unwrap_or("");
                let size = output.len();
                let preview = oneline_snippet(output, 200);
                let exit_code = extract_exit_code(output);
                out.push(make_rich(
                    MessageRole::ToolResult,
                    EventDetail::ToolResult {
                        tool_use_id, is_error: false, exit_code, size, preview,
                    },
                    base,
                ));
            }
            "reasoning" => {
                if let Some(text) = block["text"].as_str() {
                    if !text.is_empty() {
                        out.push(make_rich(
                            MessageRole::Thinking,
                            EventDetail::Thinking { text: text.into() },
                            base,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

// ── Rich parser: Gemini chat JSON (single-object, not JSONL) ────────

/// Parse a full Gemini chat-session JSON file. Gemini stores sessions as
/// pretty-printed JSON objects under `~/.gemini/tmp/<project>/chats/`
/// rather than JSONL; callers re-invoke this on file mtime change and
/// filter out already-rendered messages by `id`.
pub fn parse_gemini_file_rich(raw: &str) -> Vec<TranscriptEvent> {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let session_id = v["sessionId"].as_str().unwrap_or("").to_string();
    let messages = match v["messages"].as_array() {
        Some(m) => m,
        None => return vec![],
    };
    let mut out = Vec::new();
    for msg in messages {
        let timestamp = msg["timestamp"].as_str().map(String::from);
        let msg_id = msg["id"].as_str().unwrap_or("").to_string();
        let base = RichBase {
            session_id: session_id.clone(),
            timestamp,
            git_branch: None,
            is_subagent: false,
            agent_slug: None,
            cwd: None,
            parent_tool_use_id: Some(msg_id),
        };
        let msg_type = msg["type"].as_str().unwrap_or("");
        match msg_type {
            "user" => {
                // content is either a string or an array of { text }
                if let Some(s) = msg["content"].as_str() {
                    if !s.is_empty() {
                        out.push(make_rich(
                            MessageRole::User,
                            EventDetail::Text { text: s.into() },
                            &base,
                        ));
                    }
                } else if let Some(arr) = msg["content"].as_array() {
                    for block in arr {
                        if let Some(text) = block["text"].as_str() {
                            if !text.is_empty() {
                                out.push(make_rich(
                                    MessageRole::User,
                                    EventDetail::Text { text: text.into() },
                                    &base,
                                ));
                            }
                        }
                    }
                }
            }
            "gemini" => {
                // Thoughts (reasoning) first, then content.
                if let Some(thoughts) = msg["thoughts"].as_array() {
                    for t in thoughts {
                        let subject = t["subject"].as_str().unwrap_or("");
                        let description = t["description"].as_str().unwrap_or("");
                        let text = if subject.is_empty() {
                            description.to_string()
                        } else {
                            format!("{subject}\n{description}")
                        };
                        if !text.is_empty() {
                            out.push(make_rich(
                                MessageRole::Thinking,
                                EventDetail::Thinking { text },
                                &base,
                            ));
                        }
                    }
                }
                if let Some(s) = msg["content"].as_str() {
                    if !s.is_empty() {
                        out.push(make_rich(
                            MessageRole::Assistant,
                            EventDetail::Text { text: s.into() },
                            &base,
                        ));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

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
        role: MessageRole::User,
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
            vec![make_event(MessageRole::User, &truncate(s), base)]
        }
        // Array content = tool results (sent as user role in Claude API)
        Value::Array(blocks) => {
            let mut events = Vec::new();
            for block in blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            if !text.is_empty() {
                                events.push(make_event(MessageRole::User, &truncate(text), base));
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
                                events.push(make_event(MessageRole::ToolResult, &content, base));
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
                        events.push(make_event(MessageRole::Assistant, &truncate(text), base));
                    }
                }
            }
            "thinking" => {
                if let Some(thinking) = block["thinking"].as_str() {
                    if !thinking.is_empty() {
                        events.push(make_event(MessageRole::Thinking, &truncate(thinking), base));
                    }
                }
            }
            "tool_use" => {
                let tool_name = block["name"].as_str().unwrap_or("unknown");
                let input_str = serde_json::to_string(&block["input"]).unwrap_or_default();
                let content = format!("tool:{} {}", tool_name, truncate(&input_str));
                events.push(make_event(MessageRole::ToolUse, &content, base));
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
            vec![make_event(MessageRole::Developer, &truncate(base_instructions), &base)]
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
                "user" => parse_codex_content_blocks(payload, MessageRole::User, &base),
                "assistant" => parse_codex_content_blocks(payload, MessageRole::Assistant, &base),
                "developer" => parse_codex_content_blocks(payload, MessageRole::Developer, &base),
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
        role: MessageRole::User,
        content: truncate(text),
        session_id,
        timestamp,
        git_branch: None,
        is_subagent: false,
        agent_slug: None,
        cwd: None,
    }]
}

fn parse_codex_content_blocks(payload: &Value, role: MessageRole, base: &EventBase) -> Vec<ParsedEvent> {
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
                events.push(make_event(MessageRole::ToolUse, &content, base));
            }
            "function_call_output" => {
                let output = block["output"].as_str().unwrap_or("");
                if !output.is_empty() {
                    let content = format!("result: {}", truncate(output));
                    events.push(make_event(MessageRole::ToolResult, &content, base));
                }
            }
            "reasoning" => {
                // Codex reasoning/thinking — map to "thinking"
                if let Some(text) = block["text"].as_str() {
                    if !text.is_empty() {
                        events.push(make_event(MessageRole::Thinking, &truncate(text), base));
                    }
                }
            }
            _ => {}
        }
    }
    events
}

fn make_event(role: MessageRole, content: &str, base: &EventBase) -> ParsedEvent {
    ParsedEvent {
        role,
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
        assert_eq!(events[0].role, MessageRole::User);
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
        assert_eq!(events[0].role, MessageRole::User);
        assert_eq!(events[0].content, "Analyzing...");
        assert_eq!(events[1].role, MessageRole::ToolResult);
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
        assert_eq!(events[0].role, MessageRole::Thinking);
        assert_eq!(events[0].content, "I should say hello");
        assert_eq!(events[1].role, MessageRole::Assistant);
        assert_eq!(events[1].content, "Hello!");
        assert_eq!(events[2].role, MessageRole::ToolUse);
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
        assert_eq!(events[0].role, MessageRole::User);
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
        assert_eq!(ev1[0].role, MessageRole::Developer);
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
        assert_eq!(ev2[0].role, MessageRole::Assistant);
        assert_eq!(ev2[1].role, MessageRole::ToolUse);
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
        assert_eq!(events[0].role, MessageRole::Thinking);
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
        assert_eq!(events[0].role, MessageRole::ToolResult);
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
