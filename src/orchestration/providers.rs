use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Provider enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Copilot,
    Vibe,
    Gemini,
}

impl Provider {
    pub const ALL: &[Provider] = &[
        Provider::Claude,
        Provider::Codex,
        Provider::Copilot,
        Provider::Vibe,
        Provider::Gemini,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Copilot => "copilot",
            Provider::Vibe => "vibe",
            Provider::Gemini => "gemini",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Provider::Claude),
            "codex" => Some(Provider::Codex),
            "copilot" => Some(Provider::Copilot),
            "vibe" => Some(Provider::Vibe),
            "gemini" => Some(Provider::Gemini),
            _ => None,
        }
    }

    pub fn bin(&self) -> String {
        match self {
            Provider::Claude => std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into()),
            Provider::Codex => std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".into()),
            Provider::Copilot => std::env::var("COPILOT_BIN").unwrap_or_else(|_| "gh".into()),
            Provider::Vibe => std::env::var("VIBE_BIN").unwrap_or_else(|_| "vibe".into()),
            Provider::Gemini => std::env::var("GEMINI_BIN").unwrap_or_else(|_| "gemini".into()),
        }
    }

    pub fn supports_resume(&self) -> bool {
        matches!(self, Provider::Claude | Provider::Codex | Provider::Copilot | Provider::Vibe | Provider::Gemini)
    }

    pub fn is_streaming_json(&self) -> bool {
        matches!(self, Provider::Claude | Provider::Codex | Provider::Copilot)
    }

    pub fn models(&self) -> &'static [ModelInfo] {
        match self {
            Provider::Claude => &CLAUDE_MODELS,
            Provider::Codex => &CODEX_MODELS,
            Provider::Copilot => &COPILOT_MODELS,
            Provider::Vibe => &VIBE_MODELS,
            Provider::Gemini => &GEMINI_MODELS,
        }
    }

    pub fn efforts(&self) -> &'static [EffortInfo] {
        match self {
            Provider::Claude => &CLAUDE_EFFORTS,
            Provider::Codex => &CODEX_EFFORTS,
            Provider::Copilot => &COPILOT_EFFORTS,
            _ => &[],
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Exec options
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ExecOpts {
    pub model: Option<String>,
    pub effort: Option<String>,
}

// ---------------------------------------------------------------------------
// Arg builders
// ---------------------------------------------------------------------------

impl Provider {
    pub fn build_exec_args(
        &self,
        prompt: &str,
        session_id: &str,
        cwd: Option<&str>,
        opts: Option<&ExecOpts>,
    ) -> Vec<String> {
        let model = opts.and_then(|o| o.model.as_deref());
        let effort = opts.and_then(|o| o.effort.as_deref());

        match self {
            Provider::Claude => {
                let mut args = vec![
                    "-p".into(), prompt.into(),
                    "--output-format".into(), "stream-json".into(),
                    "--verbose".into(),
                    "--session-id".into(), session_id.into(),
                    "--dangerously-skip-permissions".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                if let Some(e) = effort { args.extend(["--effort".into(), e.into()]); }
                args
            }
            Provider::Codex => {
                let mut args = vec![
                    "exec".into(),
                    "--dangerously-bypass-approvals-and-sandbox".into(),
                    "--json".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                if let Some(e) = effort {
                    args.extend(["-c".into(), format!("model_reasoning_effort=\"{e}\"")]);
                }
                if let Some(c) = cwd { args.extend(["-C".into(), c.into()]); }
                args.push(prompt.into());
                args
            }
            Provider::Copilot => {
                let mut args = vec![
                    "copilot".into(), "--".into(),
                    "-p".into(), prompt.into(),
                    "--yolo".into(), "--autopilot".into(),
                    "--output-format".into(), "json".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                if let Some(e) = effort { args.extend(["--effort".into(), e.into()]); }
                if let Some(c) = cwd { args.extend(["--add-dir".into(), c.into()]); }
                args
            }
            Provider::Vibe => {
                let mut args = vec![
                    "-p".into(), prompt.into(),
                    "--output".into(), "json".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                args
            }
            Provider::Gemini => {
                let mut args = vec![
                    "-p".into(), prompt.into(),
                    "--yolo".into(),
                    "-o".into(), "json".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                args
            }
        }
    }

    pub fn build_resume_args(
        &self,
        session_id: &str,
        prompt: &str,
        opts: Option<&ExecOpts>,
    ) -> Vec<String> {
        let model = opts.and_then(|o| o.model.as_deref());
        let effort = opts.and_then(|o| o.effort.as_deref());

        match self {
            Provider::Claude => {
                let mut args = vec![
                    "--resume".into(), session_id.into(),
                    "-p".into(), prompt.into(),
                    "--output-format".into(), "stream-json".into(),
                    "--verbose".into(),
                    "--dangerously-skip-permissions".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                if let Some(e) = effort { args.extend(["--effort".into(), e.into()]); }
                args
            }
            Provider::Codex => {
                let mut args = vec![
                    "exec".into(), "resume".into(),
                    "--dangerously-bypass-approvals-and-sandbox".into(),
                    "--json".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                if let Some(e) = effort {
                    args.extend(["-c".into(), format!("model_reasoning_effort=\"{e}\"")]);
                }
                args.push(session_id.into());
                args.push(prompt.into());
                args
            }
            Provider::Copilot => {
                let mut args = vec![
                    "copilot".into(), "--".into(),
                    format!("--resume={session_id}"),
                    "-p".into(), prompt.into(),
                    "--yolo".into(), "--autopilot".into(),
                    "--output-format".into(), "json".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                if let Some(e) = effort { args.extend(["--effort".into(), e.into()]); }
                args
            }
            Provider::Vibe => {
                let mut args = vec![
                    "--resume".into(), session_id.into(),
                    "-p".into(), prompt.into(),
                    "--output".into(), "json".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                args
            }
            Provider::Gemini => {
                let mut args = vec![
                    "--resume".into(), session_id.into(),
                    "-p".into(), prompt.into(),
                    "--yolo".into(),
                    "-o".into(), "json".into(),
                ];
                if let Some(m) = model { args.extend(["--model".into(), m.into()]); }
                args
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Event parsing — extract structured data from provider-specific JSON events
// ---------------------------------------------------------------------------

/// Mutable state that event parsing updates on a Task.
pub struct EventSink {
    pub last_assistant_message: Option<String>,
    pub usage: Option<Usage>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub session_id: Option<String>, // discovered session id
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Provider {
    /// Parse a streaming JSON event and update the sink.
    pub fn parse_event(&self, evt: &Value, sink: &mut EventSink) {
        match self {
            Provider::Claude => parse_claude_event(evt, sink),
            Provider::Codex => parse_codex_event(evt, sink),
            Provider::Copilot => parse_copilot_event(evt, sink),
            Provider::Vibe => parse_vibe_event(evt, sink),
            Provider::Gemini => parse_gemini_event(evt, sink),
        }
    }

    /// For non-streaming providers, parse the full stdout after process exit.
    pub fn parse_bulk_output(&self, raw: &str, sink: &mut EventSink) {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            self.parse_event(&parsed, sink);
        } else {
            sink.last_assistant_message = Some(raw.trim().to_string());
        }
    }
}

fn parse_claude_event(evt: &Value, sink: &mut EventSink) {
    if evt["type"].as_str() == Some("assistant") {
        if let Some(content) = evt["message"]["content"].as_array() {
            for block in content {
                if block["type"].as_str() == Some("text") {
                    if let Some(text) = block["text"].as_str() {
                        sink.last_assistant_message = Some(text.to_string());
                    }
                }
            }
        }
    }
    if evt["type"].as_str() == Some("result") {
        if let Some(result) = evt["result"].as_str() {
            sink.last_assistant_message = Some(result.to_string());
        }
        if let Some(usage) = evt["usage"].as_object() {
            sink.usage = Some(Usage {
                input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
            });
        }
        sink.cost_usd = evt["total_cost_usd"].as_f64();
        sink.num_turns = evt["num_turns"].as_u64();
    }
}

fn parse_codex_event(evt: &Value, sink: &mut EventSink) {
    let msg_type = evt["type"].as_str().unwrap_or("");
    match msg_type {
        // item.completed — assistant message text
        "item.completed" => {
            if let Some(item) = evt.get("item") {
                if item["type"].as_str() == Some("agent_message") {
                    if let Some(text) = item["text"].as_str() {
                        sink.last_assistant_message = Some(text.to_string());
                    }
                }
            }
        }
        // turn.completed — usage stats
        "turn.completed" => {
            if let Some(usage) = evt["usage"].as_object() {
                sink.usage = Some(Usage {
                    input_tokens: usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                    output_tokens: usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                });
            }
        }
        // thread.started — session discovery
        "thread.started" => {
            if let Some(tid) = evt["thread_id"].as_str() {
                sink.session_id = Some(tid.to_string());
            }
        }
        _ => {}
    }
}

fn parse_copilot_event(evt: &Value, sink: &mut EventSink) {
    let msg_type = evt["type"].as_str().unwrap_or("");
    match msg_type {
        // assistant.message — direct text responses
        "assistant.message" => {
            if let Some(data) = evt.get("data") {
                if let Some(content) = data["content"].as_str() {
                    sink.last_assistant_message = Some(content.to_string());
                }
            }
        }
        // session.task_complete — autopilot mode completion
        "session.task_complete" => {
            if let Some(data) = evt.get("data") {
                if let Some(summary) = data["summary"].as_str() {
                    sink.last_assistant_message = Some(summary.to_string());
                }
            }
        }
        // result — sessionId, usage
        "result" => {
            if let Some(sid) = evt["sessionId"].as_str() {
                sink.session_id = Some(sid.to_string());
            }
            if let Some(usage) = evt["usage"].as_object() {
                sink.usage = Some(Usage { input_tokens: 0, output_tokens: 0 });
                sink.num_turns = usage.get("premiumRequests").and_then(|v| v.as_u64());
            }
        }
        _ => {}
    }
}

fn parse_vibe_event(evt: &Value, sink: &mut EventSink) {
    // Vibe returns bulk JSON on exit — an array of messages
    if let Some(arr) = evt.as_array() {
        // Find the last assistant message
        for msg in arr.iter().rev() {
            if msg["role"].as_str() == Some("assistant") {
                if let Some(content) = msg["content"].as_str() {
                    sink.last_assistant_message = Some(content.trim().to_string());
                    break;
                }
            }
        }
    }
}

fn parse_gemini_event(evt: &Value, sink: &mut EventSink) {
    // Gemini returns bulk JSON
    if let Some(response) = evt["response"].as_str() {
        sink.last_assistant_message = Some(response.to_string());
    }
    if let Some(session_id) = evt["session_id"].as_str() {
        sink.session_id = Some(session_id.to_string());
    }
    // Usage extraction from stats.models.*.tokens
    if let Some(stats) = evt.get("stats") {
        if let Some(models) = stats.get("models").and_then(|m| m.as_object()) {
            if let Some(first_model) = models.values().next() {
                if let Some(tokens) = first_model.get("tokens") {
                    sink.usage = Some(Usage {
                        input_tokens: tokens["input"].as_u64().unwrap_or(0),
                        output_tokens: tokens["candidates"].as_u64().unwrap_or(0),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Vibe session discovery (post-hoc)
// ---------------------------------------------------------------------------

pub fn discover_vibe_session(start_ms: u64, project_dir: &str) -> Option<String> {
    let session_dir = std::env::var("VIBE_SESSION_DIR")
        .unwrap_or_else(|_| {
            let home = dirs::home_dir().unwrap_or_default();
            home.join(".vibe/logs/session").to_string_lossy().to_string()
        });
    let session_path = Path::new(&session_dir);

    let entries: Vec<String> = match std::fs::read_dir(session_path) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("session_"))
            .collect(),
        Err(_) => return None,
    };

    let resolved_project = std::fs::canonicalize(project_dir)
        .unwrap_or_else(|_| Path::new(project_dir).to_path_buf());

    let mut scored: Vec<(String, u64, bool, bool)> = entries
        .iter()
        .filter_map(|name| {
            let meta_file = session_path.join(name).join("meta.json");
            let stat = std::fs::metadata(&meta_file).ok()?;
            let mtime_ms = stat.modified().ok()?
                .duration_since(std::time::UNIX_EPOCH).ok()?
                .as_millis() as u64;
            let data: Value = serde_json::from_str(&std::fs::read_to_string(&meta_file).ok()?).ok()?;
            let env = data.get("environment")?.as_object()?;
            let wd = env.get("working_directory")?.as_str()?;

            let matches_dir = std::fs::canonicalize(wd)
                .map(|c| c == resolved_project)
                .unwrap_or(wd == project_dir);
            let recent = mtime_ms >= start_ms.saturating_sub(2000);
            let session_id = data["session_id"].as_str()?.to_string();

            Some((session_id, mtime_ms, matches_dir, recent))
        })
        .collect();

    scored.sort_by(|a, b| b.1.cmp(&a.1)); // most recent first

    scored.iter()
        .find(|(_, _, dir, recent)| *dir && *recent)
        .or_else(|| scored.iter().find(|(_, _, dir, _)| *dir))
        .or_else(|| scored.iter().find(|(_, _, _, recent)| *recent))
        .map(|(sid, _, _, _)| sid.clone())
}

// ---------------------------------------------------------------------------
// Model/Effort catalogs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
    pub id: &'static str,
    pub description: &'static str,
    pub default: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EffortInfo {
    pub id: &'static str,
    pub description: &'static str,
    pub default: bool,
}

static CLAUDE_EFFORTS: &[EffortInfo] = &[
    EffortInfo { id: "low", description: "Light reasoning", default: false },
    EffortInfo { id: "medium", description: "Balanced speed and depth", default: false },
    EffortInfo { id: "high", description: "Greater depth for complex problems", default: false },
    EffortInfo { id: "xhigh", description: "Extended depth (Opus 4.7 only)", default: true },
    EffortInfo { id: "max", description: "Maximum reasoning depth", default: false },
];

static CLAUDE_MODELS: &[ModelInfo] = &[
    ModelInfo { id: "claude-opus-4-7", description: "Frontier model, 1M context built-in", default: true },
    ModelInfo { id: "claude-opus-4-6[1m]", description: "Previous frontier, 1M context window", default: false },
    ModelInfo { id: "claude-opus-4-6", description: "Previous frontier, 200K context", default: false },
    ModelInfo { id: "claude-sonnet-4-6", description: "Fast + capable, balanced cost", default: false },
    ModelInfo { id: "claude-haiku-4-5-20251001", description: "Fastest, lowest cost", default: false },
];

static CODEX_MODELS: &[ModelInfo] = &[
    ModelInfo { id: "gpt-5.4", description: "Latest frontier agentic coding model", default: true },
    ModelInfo { id: "gpt-5.4-mini", description: "Smaller frontier agentic coding model", default: false },
    ModelInfo { id: "gpt-5.3-codex", description: "Frontier Codex-optimized agentic coding model", default: false },
    ModelInfo { id: "gpt-5.3-codex-spark", description: "Ultra-fast coding model", default: false },
    ModelInfo { id: "gpt-5.2-codex", description: "Frontier agentic coding model", default: false },
    ModelInfo { id: "gpt-5.2", description: "Optimized for professional work and long-running agents", default: false },
    ModelInfo { id: "gpt-5.1-codex-max", description: "Deep and fast reasoning, xhigh effort", default: false },
    ModelInfo { id: "gpt-5.1-codex-mini", description: "Cheaper, faster, less capable", default: false },
];

static CODEX_EFFORTS: &[EffortInfo] = &[
    EffortInfo { id: "minimal", description: "Fastest, fewest reasoning tokens", default: false },
    EffortInfo { id: "low", description: "Light reasoning", default: false },
    EffortInfo { id: "medium", description: "Balanced speed and depth", default: true },
    EffortInfo { id: "high", description: "Greater depth for complex problems", default: false },
    EffortInfo { id: "xhigh", description: "Maximum depth (gpt-5.1-codex-max / gpt-5.2-codex only)", default: false },
];

static COPILOT_MODELS: &[ModelInfo] = &[
    ModelInfo { id: "claude-opus-4-7", description: "Anthropic Opus 4.7", default: true },
    ModelInfo { id: "claude-opus-4-6", description: "Anthropic Opus 4.6", default: false },
    ModelInfo { id: "claude-sonnet-4-6", description: "Anthropic Sonnet 4.6", default: false },
    ModelInfo { id: "gpt-5.3-codex", description: "OpenAI Codex-optimized", default: false },
    ModelInfo { id: "gpt-5.2-codex", description: "OpenAI Codex", default: false },
    ModelInfo { id: "gpt-5.1-codex-max", description: "OpenAI deep reasoning", default: false },
    ModelInfo { id: "gpt-5.2", description: "OpenAI general purpose", default: false },
];

static COPILOT_EFFORTS: &[EffortInfo] = &[
    EffortInfo { id: "low", description: "Fast responses with lighter reasoning", default: false },
    EffortInfo { id: "medium", description: "Balanced speed and depth", default: true },
    EffortInfo { id: "high", description: "Greater depth for complex problems", default: false },
    EffortInfo { id: "xhigh", description: "Maximum reasoning depth", default: false },
];

static VIBE_MODELS: &[ModelInfo] = &[
    ModelInfo { id: "devstral-2", description: "Devstral 2 123B, flagship coding model", default: true },
    ModelInfo { id: "devstral-small", description: "Devstral Small 2 24B, fast and compact", default: false },
];

static GEMINI_MODELS: &[ModelInfo] = &[
    ModelInfo { id: "gemini-2.5-flash", description: "Fast and capable", default: true },
    ModelInfo { id: "gemini-2.5-flash-lite", description: "Lightweight, lowest cost", default: false },
    ModelInfo { id: "gemini-2.5-pro", description: "Most capable Gemini 2.5", default: false },
    ModelInfo { id: "gemini-3-flash-preview", description: "Next-gen flash preview", default: false },
    ModelInfo { id: "gemini-3.1-flash-lite-preview", description: "Next-gen lite preview", default: false },
    ModelInfo { id: "gemini-3.1-pro-preview", description: "Next-gen pro preview", default: false },
];

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_roundtrip() {
        for p in Provider::ALL {
            assert_eq!(Provider::from_str(p.as_str()), Some(*p));
        }
        assert_eq!(Provider::from_str("unknown"), None);
    }

    #[test]
    fn test_claude_exec_args() {
        let args = Provider::Claude.build_exec_args("hello", "sid-1", None, None);
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"hello".to_string()));
        assert!(args.contains(&"--session-id".to_string()));
        assert!(args.contains(&"sid-1".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
    }

    #[test]
    fn test_claude_resume_args() {
        let args = Provider::Claude.build_resume_args("sid-1", "follow up", None);
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"sid-1".to_string()));
        assert!(args.contains(&"follow up".to_string()));
    }

    #[test]
    fn test_codex_exec_args_with_effort() {
        let opts = ExecOpts { model: Some("gpt-5.4".into()), effort: Some("high".into()) };
        let args = Provider::Codex.build_exec_args("do stuff", "", None, Some(&opts));
        assert!(args.contains(&"--model".to_string()));
        assert!(args.contains(&"gpt-5.4".to_string()));
        assert!(args.iter().any(|a| a.contains("model_reasoning_effort")));
    }

    #[test]
    fn test_codex_exec_args_with_cwd() {
        let args = Provider::Codex.build_exec_args("task", "", Some("/tmp/proj"), None);
        assert!(args.contains(&"-C".to_string()));
        assert!(args.contains(&"/tmp/proj".to_string()));
    }

    #[test]
    fn test_gemini_resume_args() {
        let args = Provider::Gemini.build_resume_args("gsid-1", "continue", None);
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"gsid-1".to_string()));
        assert!(args.contains(&"--yolo".to_string()));
    }

    #[test]
    fn test_copilot_exec_args() {
        let args = Provider::Copilot.build_exec_args("review this", "", None, None);
        assert_eq!(args[0], "copilot");
        assert_eq!(args[1], "--");
        assert!(args.contains(&"--autopilot".to_string()));
        assert!(args.contains(&"--output-format".to_string()));
    }

    #[test]
    fn test_vibe_resume_args() {
        let args = Provider::Vibe.build_resume_args("s1", "continue", None);
        assert!(args.contains(&"--resume".to_string()));
        assert!(args.contains(&"s1".to_string()));
        assert!(args.contains(&"--output".to_string()));
    }

    #[test]
    fn test_streaming_json_classification() {
        assert!(Provider::Claude.is_streaming_json());
        assert!(Provider::Codex.is_streaming_json());
        assert!(Provider::Copilot.is_streaming_json());
        assert!(!Provider::Vibe.is_streaming_json());
        assert!(!Provider::Gemini.is_streaming_json());
    }

    #[test]
    fn test_parse_claude_result_event() {
        let evt = serde_json::json!({
            "type": "result",
            "result": "The answer is 42",
            "usage": { "input_tokens": 100, "output_tokens": 50 },
            "total_cost_usd": 0.05,
            "num_turns": 3
        });
        let mut sink = EventSink {
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            session_id: None,
        };
        Provider::Claude.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("The answer is 42"));
        assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 100);
        assert_eq!(sink.cost_usd, Some(0.05));
        assert_eq!(sink.num_turns, Some(3));
    }

    #[test]
    fn test_parse_claude_assistant_event() {
        let evt = serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [
                    { "type": "text", "text": "Working on it..." }
                ]
            }
        });
        let mut sink = EventSink {
            last_assistant_message: None, usage: None,
            cost_usd: None, num_turns: None, session_id: None,
        };
        Provider::Claude.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("Working on it..."));
    }

    #[test]
    fn test_parse_codex_thread_started_event() {
        let evt = serde_json::json!({
            "type": "thread.started",
            "thread_id": "codex-thread-123"
        });
        let mut sink = EventSink {
            last_assistant_message: None, usage: None,
            cost_usd: None, num_turns: None, session_id: None,
        };
        Provider::Codex.parse_event(&evt, &mut sink);
        assert_eq!(sink.session_id.as_deref(), Some("codex-thread-123"));
    }

    #[test]
    fn test_parse_codex_item_completed_event() {
        let evt = serde_json::json!({
            "type": "item.completed",
            "item": { "type": "agent_message", "text": "Done!" }
        });
        let mut sink = EventSink {
            last_assistant_message: None, usage: None,
            cost_usd: None, num_turns: None, session_id: None,
        };
        Provider::Codex.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("Done!"));
    }

    #[test]
    fn test_parse_codex_turn_completed_event() {
        let evt = serde_json::json!({
            "type": "turn.completed",
            "usage": { "input_tokens": 200, "output_tokens": 80 }
        });
        let mut sink = EventSink {
            last_assistant_message: None, usage: None,
            cost_usd: None, num_turns: None, session_id: None,
        };
        Provider::Codex.parse_event(&evt, &mut sink);
        assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 200);
        assert_eq!(sink.usage.as_ref().unwrap().output_tokens, 80);
    }

    #[test]
    fn test_parse_copilot_assistant_message() {
        let evt = serde_json::json!({
            "type": "assistant.message",
            "data": { "content": "Here's the fix" }
        });
        let mut sink = EventSink {
            last_assistant_message: None, usage: None,
            cost_usd: None, num_turns: None, session_id: None,
        };
        Provider::Copilot.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("Here's the fix"));
    }

    #[test]
    fn test_parse_copilot_result_event() {
        let evt = serde_json::json!({
            "type": "result",
            "sessionId": "copilot-sid",
            "usage": { "premiumRequests": 5 }
        });
        let mut sink = EventSink {
            last_assistant_message: None, usage: None,
            cost_usd: None, num_turns: None, session_id: None,
        };
        Provider::Copilot.parse_event(&evt, &mut sink);
        assert_eq!(sink.session_id.as_deref(), Some("copilot-sid"));
        assert_eq!(sink.num_turns, Some(5));
    }

    #[test]
    fn test_parse_vibe_array_event() {
        let evt = serde_json::json!([
            {"role": "user", "content": "hello"},
            {"role": "assistant", "content": "  Hi there!  "},
            {"role": "assistant", "content": "  Final answer  "}
        ]);
        let mut sink = EventSink {
            last_assistant_message: None, usage: None,
            cost_usd: None, num_turns: None, session_id: None,
        };
        Provider::Vibe.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("Final answer"));
    }

    #[test]
    fn test_parse_gemini_with_stats() {
        let evt = serde_json::json!({
            "response": "The answer",
            "session_id": "gem-sid",
            "stats": {
                "models": {
                    "gemini-2.5-flash": {
                        "tokens": { "input": 150, "candidates": 60 }
                    }
                }
            }
        });
        let mut sink = EventSink {
            last_assistant_message: None, usage: None,
            cost_usd: None, num_turns: None, session_id: None,
        };
        Provider::Gemini.parse_event(&evt, &mut sink);
        assert_eq!(sink.last_assistant_message.as_deref(), Some("The answer"));
        assert_eq!(sink.session_id.as_deref(), Some("gem-sid"));
        assert_eq!(sink.usage.as_ref().unwrap().input_tokens, 150);
        assert_eq!(sink.usage.as_ref().unwrap().output_tokens, 60);
    }

    #[test]
    fn test_models_nonempty() {
        for p in Provider::ALL {
            assert!(!p.models().is_empty(), "{} should have at least one model", p);
        }
    }

    #[test]
    fn test_each_provider_has_default_model() {
        for p in Provider::ALL {
            let has_default = p.models().iter().any(|m| m.default);
            assert!(has_default, "{} should have a default model", p);
        }
    }
}
