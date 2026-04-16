mod index;
mod knowledge;
mod orchestration;
mod parser;
mod render;
mod threads;

use std::io;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::extract::{Query, State as AxumState};
use axum::response::sse::{Event, Sse};
use futures::stream::Stream;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, IntoContents, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig,
    StreamableHttpService,
    session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use index::TranscriptIndex;
use knowledge::Knowledge;
use orchestration::providers::{ExecOpts, Provider};
use orchestration::tail::TailEvent;
use orchestration::{self as orch, TaskStore};
use threads::Threads;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct SharedState {
    idx: RwLock<TranscriptIndex>,
    kb: RwLock<Knowledge>,
    threads: RwLock<Threads>,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: broadcast::Sender<TailEvent>,
    store_dir: PathBuf, // ~/.bro
}

// ---------------------------------------------------------------------------
// MCP Server Handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BlackboxServer {
    state: Arc<SharedState>,
    tool_router: ToolRouter<Self>,
}

impl BlackboxServer {
    fn new(state: Arc<SharedState>) -> Self {
        Self {
            state,
            tool_router: Self::bbox_tools() + Self::bro_tools(),
        }
    }

    fn ok_text(text: &str) -> CallToolResult {
        CallToolResult::success(text.to_string().into_contents())
    }

    fn ok_json(value: &Value) -> CallToolResult {
        let text = serde_json::to_string_pretty(value).unwrap_or_default();
        CallToolResult::success(text.into_contents())
    }

    fn err_text(msg: &str) -> CallToolResult {
        let mut r = CallToolResult::success(msg.to_string().into_contents());
        r.is_error = Some(true);
        r
    }
}

// ---------------------------------------------------------------------------
// Bbox tools (search, knowledge, threads)
// ---------------------------------------------------------------------------

// Parameter structs for bbox tools
#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct SearchParams {
    /// Search query. Terms ANDed by default. Use quotes for phrases, OR for disjunction.
    query: String,
    /// Filter to account: 'claude', 'account2', 'account3', 'codex'
    #[serde(default)]
    account: Option<String>,
    /// Filter by project path keywords
    #[serde(default)]
    project: Option<String>,
    /// Filter by message role/type
    #[serde(default)]
    role: Option<String>,
    /// Include subagent transcripts (default: true)
    #[serde(default)]
    include_subagents: Option<bool>,
    /// Max results (default: 20, max: 100)
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ContextParams {
    /// Path to the JSONL transcript file
    file_path: String,
    /// Byte offset of the target line (from search results)
    byte_offset: u64,
    /// Number of JSONL events before/after to include (default: 5)
    #[serde(default)]
    context_lines: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct SessionParams {
    /// Session UUID or friendly name
    session_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct MessagesParams {
    /// Session UUID or friendly name
    #[serde(default)]
    session_id: Option<String>,
    /// Direct path to a JSONL transcript file
    #[serde(default)]
    file_path: Option<String>,
    /// Filter to a specific role
    #[serde(default)]
    role: Option<String>,
    /// Include subagent transcripts (default: false)
    #[serde(default)]
    include_subagents: Option<bool>,
    /// Max characters per message (default: 500, 0 = full)
    #[serde(default)]
    max_content_length: Option<u64>,
    /// Read from end of session (default: false)
    #[serde(default)]
    from_end: Option<bool>,
    /// Skip this many messages (default: 0)
    #[serde(default)]
    offset: Option<u64>,
    /// Max messages to return (default: 50, max: 200)
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ReindexParams {
    /// Force full reindex (default: false)
    #[serde(default)]
    full: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TopicsParams {
    /// Session UUID
    #[serde(default)]
    session_id: Option<String>,
    /// Direct path to transcript file
    #[serde(default)]
    file_path: Option<String>,
    /// Limit to specific role
    #[serde(default)]
    role: Option<String>,
    /// Number of top terms (default: 25)
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct SessionsListParams {
    /// Filter to account
    #[serde(default)]
    account: Option<String>,
    /// Filter by project name substring
    #[serde(default)]
    project: Option<String>,
    /// Filter by friendly session name
    #[serde(default)]
    name: Option<String>,
    /// Skip sessions (default: 0)
    #[serde(default)]
    offset: Option<u64>,
    /// Session UUID to exclude
    #[serde(default)]
    exclude_session: Option<String>,
    /// Max sessions (default: 30, max: 100)
    #[serde(default)]
    limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct LearnParams {
    /// The instruction, fact, or preference
    content: String,
    /// Entry category
    category: String,
    /// Short title (auto-generated if omitted)
    #[serde(default)]
    title: Option<String>,
    /// global or project (default: global)
    #[serde(default)]
    scope: Option<String>,
    /// Project path for project-scoped entries
    #[serde(default)]
    project: Option<String>,
    /// Provider filter (empty = all)
    #[serde(default)]
    providers: Option<Vec<String>>,
    /// Priority: critical, standard, supplementary
    #[serde(default)]
    priority: Option<String>,
    /// Ordering within priority tier
    #[serde(default)]
    weight: Option<u32>,
    /// ISO 8601 expiry time
    #[serde(default)]
    expires_at: Option<String>,
    /// Update existing entry by ID
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct KnowledgeListParams {
    #[serde(default)] category: Option<String>,
    #[serde(default)] scope: Option<String>,
    #[serde(default)] project: Option<String>,
    #[serde(default)] provider: Option<String>,
    #[serde(default)] status: Option<String>,
    #[serde(default)] approval: Option<String>,
    #[serde(default)] query: Option<String>,
    #[serde(default)] limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ForgetParams {
    /// Entry ID to remove
    id: String,
    /// Mark as superseded instead of deleted
    #[serde(default)]
    superseded_by: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct RenderParams {
    /// Render for specific provider or all
    #[serde(default)]
    provider: Option<String>,
    /// Project directory path
    #[serde(default)]
    project: Option<String>,
    /// Preview without writing (default: false)
    #[serde(default)]
    dry_run: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct AbsorbParams {
    /// Project directory path
    project: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ReviewParams {
    /// list, approve, or reject (default: list)
    #[serde(default)]
    action: Option<String>,
    /// Entry ID (required for approve/reject)
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct BootstrapParams {
    /// Absolute path to the repo root
    project: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct RememberParams {
    /// The fact, observation, or note
    content: String,
    /// Category (default: memory)
    #[serde(default)]
    category: Option<String>,
    /// Short title
    #[serde(default)]
    title: Option<String>,
    /// global or project (default: global)
    #[serde(default)]
    scope: Option<String>,
    /// Project path
    #[serde(default)]
    project: Option<String>,
    /// Set false for invariants (default: true)
    #[serde(default)]
    decay: Option<bool>,
    /// ISO 8601 date to revisit
    #[serde(default)]
    review_at: Option<String>,
    /// ISO 8601 expiry
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ThreadParams {
    /// get, open, continue, link, resolve, promote, rename
    action: String,
    #[serde(default)] name: Option<String>,
    #[serde(default)] id: Option<String>,
    #[serde(default)] topic: Option<String>,
    #[serde(default)] project: Option<String>,
    #[serde(default)] session_id: Option<String>,
    #[serde(default)] provider: Option<String>,
    #[serde(default)] session_name: Option<String>,
    #[serde(default)] handoff_doc: Option<String>,
    #[serde(default)] note: Option<String>,
    #[serde(default)] target: Option<String>,
    #[serde(default)] target_type: Option<String>,
    #[serde(default)] edge: Option<String>,
    #[serde(default)] promoted_to: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ThreadListParams {
    #[serde(default)] status: Option<String>,
    #[serde(default)] project: Option<String>,
    #[serde(default)] name: Option<String>,
    #[serde(default)] stale_days: Option<u64>,
    #[serde(default)] include_resolved: Option<bool>,
}

/// Helper: convert params struct to serde_json::Value for passing to existing handlers.
/// Strips null values to avoid confusing legacy handlers that check field presence.
fn to_value<T: serde::Serialize>(p: &T) -> Value {
    let mut v = serde_json::to_value(p).unwrap_or(json!({}));
    if let Value::Object(ref mut map) = v {
        map.retain(|_, val| !val.is_null());
    }
    v
}

#[tool_router(router = bbox_tools)]
impl BlackboxServer {
    #[tool(name = "bbox_search", description = "Full-text search across Claude Code conversation transcripts from all accounts. Returns ranked results with excerpts.")]
    fn bbox_search(&self, Parameters(p): Parameters<SearchParams>) -> CallToolResult {
        let args = to_value(&p);
        let mut idx = self.state.idx.write().unwrap();
        if idx.is_empty() {
            if let Err(e) = idx.build_index(false) {
                return Self::err_text(&format!("Auto-index failed: {e}"));
            }
        }
        drop(idx);
        match self.state.idx.read().unwrap().search(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_context", description = "Get conversation context around a specific point in a transcript. Use after bbox_search.")]
    fn bbox_context(&self, Parameters(p): Parameters<ContextParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.idx.read().unwrap().context(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_session", description = "Get summary info for a session: first prompt, project, duration, tool usage, message counts.")]
    fn bbox_session(&self, Parameters(p): Parameters<SessionParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.idx.read().unwrap().session(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_messages", description = "List messages from a session in chronological order. Supports pagination, role filter, tail mode.")]
    fn bbox_messages(&self, Parameters(p): Parameters<MessagesParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.idx.read().unwrap().messages(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_reindex", description = "Build or incrementally update the transcript search index.")]
    fn bbox_reindex(&self, Parameters(p): Parameters<ReindexParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.idx.write().unwrap().reindex(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_topics", description = "Extract top terms from a session by frequency analysis. No LLM — pure term counting.")]
    fn bbox_topics(&self, Parameters(p): Parameters<TopicsParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.idx.read().unwrap().topics(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_sessions_list", description = "Browse sessions across all accounts, sorted by most recent.")]
    fn bbox_sessions_list(&self, Parameters(p): Parameters<SessionsListParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.idx.read().unwrap().sessions_list(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_stats", description = "Corpus statistics: indexed document count, index size, source file counts.")]
    fn bbox_stats(&self) -> CallToolResult {
        match self.state.idx.read().unwrap().stats() {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_learn", description = "Add or update a knowledge entry. Entries are rendered into CLAUDE.md/AGENTS.md/GEMINI.md.")]
    fn bbox_learn(&self, Parameters(p): Parameters<LearnParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.kb.write().unwrap().learn(&args, false) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_remember", description = "Store a fact for on-demand recall only — NOT rendered into markdown files.")]
    fn bbox_remember(&self, Parameters(p): Parameters<RememberParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.kb.write().unwrap().remember(&args, false) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_knowledge", description = "List/search knowledge entries with filters.")]
    fn bbox_knowledge(&self, Parameters(p): Parameters<KnowledgeListParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.kb.write().unwrap().list(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_forget", description = "Remove or supersede a knowledge entry.")]
    fn bbox_forget(&self, Parameters(p): Parameters<ForgetParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.kb.write().unwrap().forget(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_render", description = "Render knowledge entries into provider markdown files.")]
    fn bbox_render(&self, Parameters(p): Parameters<RenderParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.kb.read().unwrap().render(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_absorb", description = "Absorb external changes from rendered files back into knowledge store.")]
    fn bbox_absorb(&self, Parameters(p): Parameters<AbsorbParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.kb.write().unwrap().absorb(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_lint", description = "Health check: find contradictions, stale entries, duplicates.")]
    fn bbox_lint(&self) -> CallToolResult {
        match self.state.kb.read().unwrap().lint() {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_review", description = "Review unverified entries. List, approve, or reject.")]
    fn bbox_review(&self, Parameters(p): Parameters<ReviewParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.kb.write().unwrap().review(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_bootstrap", description = "Bootstrap a new repo into the blackbox knowledge system.")]
    fn bbox_bootstrap(&self, Parameters(p): Parameters<BootstrapParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.kb.read().unwrap().bootstrap(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_thread", description = "Manage work threads — lightweight continuity tracker for non-dispatchable work.")]
    fn bbox_thread(&self, Parameters(p): Parameters<ThreadParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.threads.write().unwrap().thread(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }

    #[tool(name = "bbox_thread_list", description = "List and scan work threads. Shows open/active/stale threads by default.")]
    fn bbox_thread_list(&self, Parameters(p): Parameters<ThreadListParams>) -> CallToolResult {
        let args = to_value(&p);
        match self.state.threads.read().unwrap().thread_list(&args) {
            Ok(text) => Self::ok_text(&text),
            Err(e) => Self::err_text(&format!("Error: {e:#}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Bro tools (orchestration)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ExecParams {
    /// Task instruction for the agent
    prompt: String,
    /// Named bro instance to target (resolves provider/account/lens)
    #[serde(default)]
    bro: Option<String>,
    /// Raw provider for ad-hoc tasks
    #[serde(default)]
    provider: Option<String>,
    /// Working directory (absolute path)
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ResumeParams {
    /// Follow-up instruction
    prompt: String,
    /// Named bro instance to resume
    #[serde(default)]
    bro: Option<String>,
    /// Session ID from a prior task (requires provider)
    #[serde(default)]
    session_id: Option<String>,
    /// Provider (required with session_id)
    #[serde(default)]
    provider: Option<String>,
    /// Working directory
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WaitParams {
    /// Task ID from exec or resume
    task_id: String,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhenParams {
    /// Team name — waits on each member's most recent task
    #[serde(default)]
    team: Option<String>,
    /// Explicit list of task IDs
    #[serde(default)]
    task_ids: Option<Vec<String>>,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct BroadcastParams {
    /// Team name
    team: String,
    /// Prompt sent to every member
    prompt: String,
    /// Working directory override
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct StatusParams {
    /// Task ID to check
    task_id: String,
    /// Number of recent events to include (default: 0)
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct DashboardParams {
    #[serde(default)] provider: Option<String>,
    #[serde(default)] team: Option<String>,
    #[serde(default)] status: Option<String>,
    #[serde(default)] limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CancelParams {
    /// Task ID to cancel
    task_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct BrofileParams {
    /// Operation: create, list, get, delete, set_account, list_accounts
    action: String,
    #[serde(default)] name: Option<String>,
    #[serde(default)] provider: Option<String>,
    #[serde(default)] account: Option<String>,
    #[serde(default)] lens: Option<String>,
    #[serde(default)] model: Option<String>,
    #[serde(default)] effort: Option<String>,
    #[serde(default)] env: Option<std::collections::HashMap<String, String>>,
    #[serde(default)] scope: Option<String>,
    #[serde(default)] project_dir: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TeamParams {
    /// Operation: save_template, list_templates, delete_template, create, list, dissolve, roster
    action: String,
    #[serde(default)] name: Option<String>,
    #[serde(default)] members: Option<Vec<TeamMemberSlot>>,
    #[serde(default)] template: Option<String>,
    #[serde(default)] project_dir: Option<String>,
    #[serde(default)] scope: Option<String>,
    #[serde(default)] cancel_running: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TeamMemberSlot {
    brofile: String,
    #[serde(default)] alias: Option<String>,
    #[serde(default)] count: Option<u32>,
}

#[tool_router(router = bro_tools)]
impl BlackboxServer {
    #[tool(name = "bro_exec", description = "Launch an agent task. Returns {taskId, sessionId} immediately. Prefer named bro over raw provider.")]
    async fn bro_exec(&self, Parameters(p): Parameters<ExecParams>) -> CallToolResult {
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let store_dir = self.state.store_dir.clone();

        let (provider, lens, exec_opts, env_overrides, cwd) =
            match self.resolve_exec_target(p.bro.as_deref(), p.provider.as_deref(), p.project_dir.as_deref()) {
                Ok(r) => r,
                Err(e) => return Self::err_text(&e),
            };

        let final_prompt = orch::apply_lens(&p.prompt, lens.as_deref(), allow_recursion);
        let session_id = if provider == Provider::Claude {
            uuid::Uuid::new_v4().to_string()
        } else {
            "pending".to_string()
        };
        let args = provider.build_exec_args(&final_prompt, &session_id, cwd.as_deref(), exec_opts.as_ref());

        let task = orch::spawn_task(
            provider, args, session_id,
            cwd, env_overrides, store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
        );

        // If targeting a named bro in a team, record the task
        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let inner = task.inner.lock().unwrap();
        Self::ok_json(&json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        }))
    }

    #[tool(name = "bro_resume", description = "Resume a previous agent session with a follow-up prompt. Returns a new taskId on the same session.")]
    async fn bro_resume(&self, Parameters(p): Parameters<ResumeParams>) -> CallToolResult {
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let store_dir = self.state.store_dir.clone();

        let (provider, session_id, lens, exec_opts, env_overrides, cwd) =
            match self.resolve_resume_target(
                p.bro.as_deref(), p.session_id.as_deref(),
                p.provider.as_deref(), p.project_dir.as_deref(),
            ) {
                Ok(r) => r,
                Err(e) => return Self::err_text(&e),
            };

        if !provider.supports_resume() {
            return Self::err_text(&format!("{provider} does not support resume"));
        }

        let final_prompt = orch::apply_lens(&p.prompt, lens.as_deref(), allow_recursion);
        let args = provider.build_resume_args(&session_id, &final_prompt, exec_opts.as_ref());

        let task = orch::spawn_task(
            provider, args, session_id,
            cwd, env_overrides, store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
        );

        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let inner = task.inner.lock().unwrap();
        Self::ok_json(&json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        }))
    }

    #[tool(name = "bro_wait", description = "Block until a task completes. USE MAXIMUM TIMEOUT. With timeout_seconds, returns a progress snapshot if not finished yet.")]
    async fn bro_wait(
        &self,
        Parameters(p): Parameters<WaitParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task = match self.state.task_store.read().unwrap().get(&p.task_id) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };

        // Spawn progress notifier
        let task_progress = task.clone();
        let task_id = p.task_id.clone();
        let store_dir = self.state.store_dir.clone();
        let peer = context.peer.clone();
        let progress_handle = tokio::spawn(async move {
            let mut tick = 0u64;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                tick += 1;

                // Build the message under the lock, then drop before await
                let msg = {
                    let inner = task_progress.inner.lock().unwrap();
                    if inner.status.is_terminal() { break; }
                    let bro_name = orchestration::team::find_bro_name_for_task(&task_id, &store_dir);
                    let label = bro_name.unwrap_or_else(|| task_id[..task_id.len().min(8)].to_string());
                    let elapsed = orch::format_elapsed(inner.started_at, None);
                    let events = inner.events.len();
                    let activity = inner.last_assistant_message.as_deref()
                        .map(|m| { let c = m.replace('\n', " "); if c.len() > 80 { format!("{}…", &c[..80]) } else { c } })
                        .unwrap_or_else(|| if events == 0 { "starting…".into() } else { "working…".into() });
                    format!("[{label}] {elapsed} | {events} events | {activity}")
                }; // MutexGuard dropped here

                let _ = peer.send_notification(rmcp::model::ServerNotification::ProgressNotification(
                    rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                        progress_token: rmcp::model::ProgressToken(rmcp::model::NumberOrString::Number(tick as i64)),
                        progress: tick as f64,
                        total: None,
                        message: Some(msg),
                    }),
                )).await;
            }
        });

        let completed = orch::wait_for_task_with_timeout(&task, p.timeout_seconds).await;
        progress_handle.abort();
        if completed {
            Self::ok_json(&orch::task_result_json(&task))
        } else {
            Self::ok_json(&orch::timeout_snapshot_json(&task))
        }
    }

    #[tool(name = "bro_when_all", description = "Block until ALL tasks complete. Accepts team name or task_ids array. USE MAXIMUM TIMEOUT.")]
    async fn bro_when_all(&self, Parameters(p): Parameters<WhenParams>) -> CallToolResult {
        let task_ids = match self.resolve_when_targets(p.team.as_deref(), p.task_ids.as_deref()) {
            Ok(ids) => ids,
            Err(e) => return Self::err_text(&e),
        };

        let tasks: Vec<Arc<orch::Task>> = {
            let store = self.state.task_store.read().unwrap();
            task_ids.iter().filter_map(|id| store.get(id)).collect()
        };

        // Wait concurrently (like Promise.all), not sequentially
        let timeout = p.timeout_seconds;
        let store_dir = self.state.store_dir.clone();
        let futs: Vec<_> = tasks.iter().map(|task| {
            let task = task.clone();
            let sd = store_dir.clone();
            async move {
                let completed = orch::wait_for_task_with_timeout(&task, timeout).await;
                let bro_name = {
                    let inner = task.inner.lock().unwrap();
                    orchestration::team::find_bro_name_for_task(&inner.id, &sd)
                };
                let mut r = if completed {
                    orch::task_result_json(&task)
                } else {
                    orch::timeout_snapshot_json(&task)
                };
                if let Some(name) = bro_name {
                    r["bro"] = Value::String(name);
                }
                r
            }
        }).collect();

        let results: Vec<Value> = futures::future::join_all(futs).await;
        let all_done = results.iter().all(|r| r.get("timed_out").is_none());
        Self::ok_json(&json!({ "all_completed": all_done, "results": results }))
    }

    #[tool(name = "bro_when_any", description = "Block until the FIRST task completes. Returns all current states. USE MAXIMUM TIMEOUT.")]
    async fn bro_when_any(&self, Parameters(p): Parameters<WhenParams>) -> CallToolResult {
        let task_ids = match self.resolve_when_targets(p.team.as_deref(), p.task_ids.as_deref()) {
            Ok(ids) => ids,
            Err(e) => return Self::err_text(&e),
        };

        let tasks: Vec<Arc<orch::Task>> = {
            let store = self.state.task_store.read().unwrap();
            task_ids.iter().filter_map(|id| store.get(id)).collect()
        };

        // Check if any already done
        let any_done = tasks.iter().any(|t| t.inner.lock().unwrap().status.is_terminal());
        if !any_done && !tasks.is_empty() {
            // Race them
            let futs: Vec<_> = tasks.iter().map(|t| {
                let t = t.clone();
                Box::pin(async move { orch::wait_for_task(&t).await; })
            }).collect();

            match p.timeout_seconds {
                Some(secs) => {
                    let dur = std::time::Duration::from_secs_f64(secs);
                    let _ = tokio::time::timeout(dur, futures::future::select_all(futs)).await;
                }
                None => {
                    futures::future::select_all(futs).await;
                }
            }
        }

        let mut results = Vec::new();
        for task in &tasks {
            let inner = task.inner.lock().unwrap();
            let bro_name = orchestration::team::find_bro_name_for_task(&inner.id, &self.state.store_dir);
            drop(inner);

            let mut r = if task.inner.lock().unwrap().status.is_terminal() {
                orch::task_result_json(task)
            } else {
                orch::timeout_snapshot_json(task)
            };
            if let Some(name) = bro_name {
                r["bro"] = Value::String(name);
            }
            results.push(r);
        }

        let any_completed = results.iter().any(|r| r.get("timed_out").is_none());
        Self::ok_json(&json!({ "any_completed": any_completed, "results": results }))
    }

    #[tool(name = "bro_broadcast", description = "Send same prompt to every team member. Follow with bro_when_all or bro_when_any.")]
    async fn bro_broadcast(&self, Parameters(p): Parameters<BroadcastParams>) -> CallToolResult {
        let _team_lock = orchestration::team::lock_teams();
        let team = match orchestration::team::load_team(&p.team, &self.state.store_dir) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown team: {}", p.team)),
        };
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let cwd = p.project_dir.or(team.project_dir.clone());
        let store_dir = self.state.store_dir.clone();
        let mut launched = Vec::new();
        let mut updated_team = team.clone();

        for (i, member) in team.members.iter().enumerate() {
            let brofile = match orchestration::brofile::resolve_brofile(
                &member.brofile, &store_dir, team.project_dir.as_deref(),
            ) {
                Some(bf) => bf,
                None => {
                    launched.push(json!({"bro": member.name, "error": format!("Brofile not found: {}", member.brofile)}));
                    continue;
                }
            };

            let final_prompt = orch::apply_lens(&p.prompt, brofile.lens.as_deref(), allow_recursion);
            let mut env_overrides = None;
            if let Some(ref acct_name) = brofile.account {
                if let Some(acct) = orchestration::brofile::load_account(acct_name, &store_dir) {
                    env_overrides = acct.env;
                }
            }
            let exec_opts = if brofile.model.is_some() || brofile.effort.is_some() {
                Some(ExecOpts { model: brofile.model.clone(), effort: brofile.effort.clone() })
            } else {
                None
            };

            let task = if let Some(ref sid) = member.session_id {
                if sid != "pending" {
                    let args = brofile.provider.build_resume_args(sid, &final_prompt, exec_opts.as_ref());
                    orch::spawn_task(
                        brofile.provider, args, sid.clone(),
                        cwd.clone(), env_overrides, store_dir.clone(),
                        self.state.task_store.clone(), self.state.tail_tx.clone(),
                    )
                } else {
                    let session_id = if brofile.provider == Provider::Claude { uuid::Uuid::new_v4().to_string() } else { "pending".into() };
                    let args = brofile.provider.build_exec_args(&final_prompt, &session_id, cwd.as_deref(), exec_opts.as_ref());
                    let t = orch::spawn_task(
                        brofile.provider, args, session_id,
                        cwd.clone(), env_overrides, store_dir.clone(),
                        self.state.task_store.clone(), self.state.tail_tx.clone(),
                    );
                    updated_team.members[i].session_id = Some(t.inner.lock().unwrap().session_id.clone());
                    t
                }
            } else {
                let session_id = if brofile.provider == Provider::Claude { uuid::Uuid::new_v4().to_string() } else { "pending".into() };
                let args = brofile.provider.build_exec_args(&final_prompt, &session_id, cwd.as_deref(), exec_opts.as_ref());
                let t = orch::spawn_task(
                    brofile.provider, args, session_id,
                    cwd.clone(), env_overrides, store_dir.clone(),
                    self.state.task_store.clone(), self.state.tail_tx.clone(),
                );
                updated_team.members[i].session_id = Some(t.inner.lock().unwrap().session_id.clone());
                t
            };

            let tid = task.id();
            updated_team.members[i].task_history.push(tid.clone());
            let sid = task.inner.lock().unwrap().session_id.clone();
            launched.push(json!({"bro": member.name, "taskId": tid, "sessionId": sid}));
        }

        orchestration::team::save_team(&updated_team, &store_dir);
        Self::ok_json(&json!({"team": p.team, "tasks": launched}))
    }

    #[tool(name = "bro_status", description = "Non-blocking progress check. Returns current state without waiting.")]
    fn bro_status(&self, Parameters(p): Parameters<StatusParams>) -> CallToolResult {
        match self.state.task_store.read().unwrap().get(&p.task_id) {
            Some(task) => Self::ok_json(&orch::task_status_json(&task, p.tail.unwrap_or(0))),
            None => Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        }
    }

    #[tool(name = "bro_dashboard", description = "List recent tasks and sessions. Use to look up a taskId or sessionId.")]
    fn bro_dashboard(&self, Parameters(p): Parameters<DashboardParams>) -> CallToolResult {
        let store = self.state.task_store.read().unwrap();
        let limit = p.limit.unwrap_or(20);

        let filter_provider = p.provider.as_deref().and_then(Provider::from_str);
        let filter_status: Option<orch::TaskStatus> = p.status.as_deref().and_then(|s| {
            serde_json::from_str(&format!("\"{s}\"")).ok()
        });

        let team_task_ids: Option<std::collections::HashSet<String>> = p.team.as_ref().and_then(|name| {
            let team = orchestration::team::load_team(name, &self.state.store_dir)?;
            Some(team.members.iter().flat_map(|m| m.task_history.clone()).collect())
        });

        let mut with_ts: Vec<(u64, Value)> = store.all_tasks().iter()
            .filter(|t| {
                let inner = t.inner.lock().unwrap();
                if let Some(fp) = filter_provider { if inner.provider != fp { return false; } }
                if let Some(fs) = filter_status { if inner.status != fs { return false; } }
                if let Some(ref ids) = team_task_ids { if !ids.contains(&inner.id) { return false; } }
                true
            })
            .map(|t| {
                let inner = t.inner.lock().unwrap();
                let bro_name = orchestration::team::find_bro_name_for_task(&inner.id, &self.state.store_dir);
                let mut entry = json!({
                    "taskId": inner.id,
                    "provider": inner.provider,
                    "sessionId": inner.session_id,
                    "status": inner.status,
                    "elapsed": orch::format_elapsed(inner.started_at, inner.completed_at),
                    "hasResult": inner.last_assistant_message.is_some(),
                });
                if let Some(name) = bro_name {
                    entry["bro"] = Value::String(name);
                }
                (inner.started_at, entry)
            })
            .collect();
        with_ts.sort_by(|a, b| b.0.cmp(&a.0));
        let entries: Vec<Value> = with_ts.into_iter().take(limit).map(|(_, e)| e).collect();

        Self::ok_json(&json!({"count": entries.len(), "tasks": entries}))
    }

    #[tool(name = "bro_cancel", description = "Cancel a running task (sends SIGTERM).")]
    fn bro_cancel(&self, Parameters(p): Parameters<CancelParams>) -> CallToolResult {
        let task = match self.state.task_store.read().unwrap().get(&p.task_id) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };
        match orch::cancel_task(&task, &self.state.task_store, &self.state.store_dir) {
            Ok(()) => {
                let inner = task.inner.lock().unwrap();
                let _ = self.state.tail_tx.send(TailEvent::TaskCancelled {
                    task_id: inner.id.clone(),
                    elapsed: orch::format_elapsed(inner.started_at, inner.completed_at),
                });
                Self::ok_json(&json!({
                    "taskId": inner.id,
                    "sessionId": inner.session_id,
                    "status": "cancelled",
                }))
            }
            Err(e) => Self::err_text(&e),
        }
    }

    #[tool(name = "bro_providers", description = "List configured providers, binary paths, and available models.")]
    fn bro_providers(&self) -> CallToolResult {
        let extra_path = std::env::var("BRO_EXTRA_PATH")
            .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/bin").to_string_lossy().to_string());
        let augmented_path = format!("{}:{}", extra_path, std::env::var("PATH").unwrap_or_default());

        let mut info = serde_json::Map::new();
        for p in Provider::ALL {
            let bin = p.bin();
            let found = std::process::Command::new("bash")
                .args(["-lc", &format!("command -v '{bin}'")])
                .env("PATH", &augmented_path)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            let mut entry = json!({
                "bin": bin,
                "found": found,
                "supportsResume": p.supports_resume(),
            });
            if !p.models().is_empty() {
                entry["models"] = serde_json::to_value(p.models()).unwrap_or_default();
            }
            if !p.efforts().is_empty() {
                entry["efforts"] = serde_json::to_value(p.efforts()).unwrap_or_default();
            }
            info.insert(p.as_str().to_string(), entry);
        }
        Self::ok_json(&Value::Object(info))
    }

    #[tool(name = "bro_brofile", description = "Manage brofile templates and accounts. Actions: create, list, get, delete, set_account, list_accounts.")]
    fn bro_brofile(&self, Parameters(p): Parameters<BrofileParams>) -> CallToolResult {
        use orchestration::brofile;
        let store_dir = &self.state.store_dir;
        let scope = p.scope.as_deref().unwrap_or("global");

        match p.action.as_str() {
            "create" => {
                let name = match &p.name { Some(n) => n, None => return Self::err_text("name is required") };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                let provider = match p.provider.as_deref().and_then(Provider::from_str) {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let bf = brofile::Brofile {
                    name: name.clone(), provider,
                    account: p.account.clone(), lens: p.lens.clone(),
                    model: p.model.clone(), effort: p.effort.clone(),
                };
                brofile::save_brofile(&bf, scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&json!({"created": name, "scope": scope, "brofile": bf}))
            }
            "list" => {
                let list = brofile::list_brofiles(scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&serde_json::to_value(&list).unwrap_or_default())
            }
            "get" => {
                let name = match &p.name { Some(n) => n, None => return Self::err_text("name is required") };
                match brofile::resolve_brofile(name, store_dir, p.project_dir.as_deref()) {
                    Some(bf) => Self::ok_json(&serde_json::to_value(&bf).unwrap_or_default()),
                    None => Self::err_text(&format!("Brofile not found: {name}")),
                }
            }
            "delete" => {
                let name = match &p.name { Some(n) => n, None => return Self::err_text("name is required") };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                if brofile::delete_brofile(name, scope, store_dir, p.project_dir.as_deref()) {
                    Self::ok_json(&json!({"deleted": name}))
                } else {
                    Self::err_text(&format!("Brofile not found: {name}"))
                }
            }
            "set_account" => {
                let name = match &p.name { Some(n) => n, None => return Self::err_text("name is required") };
                let mut config = brofile::load_config(store_dir);
                config.accounts.insert(name.clone(), brofile::Account { env: p.env.clone() });
                brofile::save_config(&config, store_dir);
                Self::ok_json(&json!({"account": name, "env": p.env}))
            }
            "list_accounts" => {
                let config = brofile::load_config(store_dir);
                Self::ok_json(&serde_json::to_value(&config.accounts).unwrap_or_default())
            }
            _ => Self::err_text(&format!("Unknown brofile action: {}", p.action)),
        }
    }

    #[tool(name = "bro_team", description = "Manage teamplates and teams. Actions: save_template, list_templates, delete_template, create, list, dissolve, roster.")]
    fn bro_team(&self, Parameters(p): Parameters<TeamParams>) -> CallToolResult {
        use orchestration::team;
        let store_dir = &self.state.store_dir;
        let scope = p.scope.as_deref().unwrap_or("global");

        match p.action.as_str() {
            "save_template" => {
                let name = match &p.name { Some(n) => n, None => return Self::err_text("name is required") };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                let members = match &p.members {
                    Some(m) if !m.is_empty() => m,
                    _ => return Self::err_text("members is required"),
                };
                // Validate brofile names
                for m in members {
                    if orchestration::brofile::resolve_brofile(&m.brofile, store_dir, p.project_dir.as_deref()).is_none() {
                        return Self::err_text(&format!("Brofile not found: {}", m.brofile));
                    }
                }
                let tp = team::Teamplate {
                    name: name.clone(),
                    members: members.iter().map(|m| team::TeamplateMember {
                        brofile: m.brofile.clone(),
                        alias: m.alias.clone(),
                        count: m.count.unwrap_or(1),
                    }).collect(),
                };
                team::save_teamplate(&tp, scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&json!({"saved": name, "scope": scope}))
            }
            "list_templates" => {
                let list = team::list_teamplates(scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&serde_json::to_value(&list).unwrap_or_default())
            }
            "delete_template" => {
                let name = match &p.name { Some(n) => n, None => return Self::err_text("name is required") };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                if team::delete_teamplate(name, scope, store_dir, p.project_dir.as_deref()) {
                    Self::ok_json(&json!({"deleted": name}))
                } else {
                    Self::err_text(&format!("Teamplate not found: {name}"))
                }
            }
            "create" => {
                let template = match &p.template { Some(t) => t, None => return Self::err_text("template is required") };
                let tp = match team::resolve_teamplate(template, store_dir, p.project_dir.as_deref()) {
                    Some(tp) => tp,
                    None => return Self::err_text(&format!("Teamplate not found: {template}")),
                };
                // Validate all brofiles exist before instantiating
                for m in &tp.members {
                    if orchestration::brofile::resolve_brofile(&m.brofile, store_dir, p.project_dir.as_deref()).is_none() {
                        return Self::err_text(&format!("Brofile not found: {}", m.brofile));
                    }
                }
                let team_name = p.name.clone().unwrap_or_else(|| format!("{template}-{}", orch::now_ms()));
                let t = team::instantiate_team(&tp, &team_name, p.project_dir.as_deref(), store_dir);
                Self::ok_json(&json!({
                    "created": t.name,
                    "teamplate": tp.name,
                    "members": t.members.iter().map(|m| json!({"name": m.name, "brofile": m.brofile})).collect::<Vec<_>>(),
                }))
            }
            "list" => {
                let teams = team::load_all_teams(store_dir);
                let list: Vec<Value> = teams.iter().map(|t| json!({
                    "name": t.name,
                    "teamplate": t.teamplate,
                    "memberCount": t.members.len(),
                    "createdAt": t.created_at,
                    "projectDir": t.project_dir,
                })).collect();
                Self::ok_json(&json!(list))
            }
            "dissolve" => {
                let name = match &p.name { Some(n) => n, None => return Self::err_text("name is required") };
                let loaded_team = match team::load_team(name, store_dir) {
                    Some(t) => t,
                    None => return Self::err_text(&format!("Unknown team: {name}")),
                };
                if p.cancel_running.unwrap_or(false) {
                    let task_store = self.state.task_store.read().unwrap();
                    for member in &loaded_team.members {
                        for tid in &member.task_history {
                            if let Some(task) = task_store.get(tid) {
                                let _ = orch::cancel_task(&task, &self.state.task_store, &self.state.store_dir);
                            }
                        }
                    }
                }
                team::remove_team(name, store_dir);
                Self::ok_json(&json!({"dissolved": name}))
            }
            "roster" => {
                let name = match &p.name { Some(n) => n, None => return Self::err_text("name is required") };
                let loaded_team = match team::load_team(name, store_dir) {
                    Some(t) => t,
                    None => return Self::err_text(&format!("Unknown team: {name}")),
                };
                let task_store = self.state.task_store.read().unwrap();
                let roster: Vec<Value> = loaded_team.members.iter().map(|m| {
                    let latest_tid = m.task_history.last();
                    let latest = latest_tid.and_then(|id| task_store.get(id)).map(|t| {
                        let inner = t.inner.lock().unwrap();
                        json!({
                            "taskId": inner.id,
                            "status": inner.status,
                            "elapsed": orch::format_elapsed(inner.started_at, inner.completed_at),
                        })
                    });
                    json!({
                        "name": m.name,
                        "brofile": m.brofile,
                        "sessionId": m.session_id,
                        "taskCount": m.task_history.len(),
                        "latestTask": latest,
                    })
                }).collect();
                Self::ok_json(&json!({"team": name, "teamplate": loaded_team.teamplate, "members": roster}))
            }
            _ => Self::err_text(&format!("Unknown team action: {}", p.action)),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper methods on BlackboxServer
// ---------------------------------------------------------------------------

impl BlackboxServer {
    #[allow(clippy::type_complexity)]
    fn resolve_exec_target(
        &self,
        bro_name: Option<&str>,
        raw_provider: Option<&str>,
        project_dir: Option<&str>,
    ) -> Result<(Provider, Option<String>, Option<ExecOpts>, Option<std::collections::HashMap<String, String>>, Option<String>), String> {
        let store_dir = &self.state.store_dir;

        if let Some(name) = bro_name {
            let teams = orchestration::team::load_all_teams(store_dir);
            if let Some(bro_match) = orchestration::team::find_bro(name, &teams) {
                let member = &bro_match.team.members[bro_match.member_idx];
                let bf = orchestration::brofile::resolve_brofile(&member.brofile, store_dir, bro_match.team.project_dir.as_deref())
                    .ok_or(format!("Brofile not found: {}", member.brofile))?;
                let env = bf.account.as_ref()
                    .and_then(|a| orchestration::brofile::load_account(a, store_dir))
                    .and_then(|a| a.env);
                let opts = if bf.model.is_some() || bf.effort.is_some() {
                    Some(ExecOpts { model: bf.model, effort: bf.effort })
                } else { None };
                let cwd = project_dir.map(String::from).or(bro_match.team.project_dir.clone());
                return Ok((bf.provider, bf.lens, opts, env, cwd));
            }
            // Standalone brofile fallback
            let bf = orchestration::brofile::resolve_brofile(name, store_dir, project_dir)
                .ok_or(format!("Unknown bro or brofile: {name}"))?;
            let env = bf.account.as_ref()
                .and_then(|a| orchestration::brofile::load_account(a, store_dir))
                .and_then(|a| a.env);
            let opts = if bf.model.is_some() || bf.effort.is_some() {
                Some(ExecOpts { model: bf.model, effort: bf.effort })
            } else { None };
            return Ok((bf.provider, bf.lens, opts, env, project_dir.map(String::from)));
        }

        if let Some(p) = raw_provider {
            let provider = Provider::from_str(p).ok_or(format!("Unknown provider: {p}"))?;
            return Ok((provider, None, None, None, project_dir.map(String::from)));
        }

        Err("Provide either bro or provider".into())
    }

    #[allow(clippy::type_complexity)]
    fn resolve_resume_target(
        &self,
        bro_name: Option<&str>,
        session_id: Option<&str>,
        raw_provider: Option<&str>,
        project_dir: Option<&str>,
    ) -> Result<(Provider, String, Option<String>, Option<ExecOpts>, Option<std::collections::HashMap<String, String>>, Option<String>), String> {
        let store_dir = &self.state.store_dir;

        if let Some(name) = bro_name {
            let teams = orchestration::team::load_all_teams(store_dir);
            let bro_match = orchestration::team::find_bro(name, &teams)
                .ok_or_else(|| {
                    if orchestration::brofile::resolve_brofile(name, store_dir, project_dir).is_some() {
                        format!("Brofile \"{name}\" is not in a team — use exec first or provide session_id + provider")
                    } else {
                        format!("Unknown bro: {name}")
                    }
                })?;
            let member = &bro_match.team.members[bro_match.member_idx];
            let sid = member.session_id.as_deref()
                .filter(|s| *s != "pending")
                .ok_or(format!("Bro \"{name}\" has no active session — use exec first"))?;
            let bf = orchestration::brofile::resolve_brofile(&member.brofile, store_dir, bro_match.team.project_dir.as_deref())
                .ok_or(format!("Brofile not found: {}", member.brofile))?;
            let env = bf.account.as_ref()
                .and_then(|a| orchestration::brofile::load_account(a, store_dir))
                .and_then(|a| a.env);
            let opts = if bf.model.is_some() || bf.effort.is_some() {
                Some(ExecOpts { model: bf.model, effort: bf.effort })
            } else { None };
            let cwd = project_dir.map(String::from).or(bro_match.team.project_dir.clone());
            return Ok((bf.provider, sid.to_string(), bf.lens, opts, env, cwd));
        }

        if let (Some(sid), Some(p)) = (session_id, raw_provider) {
            let provider = Provider::from_str(p).ok_or(format!("Unknown provider: {p}"))?;
            return Ok((provider, sid.to_string(), None, None, None, project_dir.map(String::from)));
        }

        Err("Provide either bro or session_id + provider".into())
    }

    fn resolve_when_targets(&self, team_name: Option<&str>, task_ids: Option<&[String]>) -> Result<Vec<String>, String> {
        if let Some(name) = team_name {
            let team = orchestration::team::load_team(name, &self.state.store_dir)
                .ok_or(format!("Unknown team: {name}"))?;
            let ids: Vec<String> = team.members.iter()
                .filter_map(|m| m.task_history.last().cloned())
                .collect();
            if ids.is_empty() { return Err(format!("No tasks found for team {name}")); }
            return Ok(ids);
        }
        if let Some(ids) = task_ids {
            if ids.is_empty() { return Err("Empty task_ids array".into()); }
            return Ok(ids.to_vec());
        }
        Err("Provide either team or task_ids".into())
    }

    fn record_task_to_bro(&self, bro_name: &str, task: &Arc<orch::Task>) {
        let _lock = orchestration::team::lock_teams();
        let tid = task.id();
        let teams = orchestration::team::load_all_teams(&self.state.store_dir);
        for mut team in teams {
            let mut dirty = false;
            for member in &mut team.members {
                if member.name == bro_name {
                    member.task_history.push(tid.clone());
                    if member.session_id.as_deref().unwrap_or("pending") == "pending" {
                        member.session_id = Some(task.inner.lock().unwrap().session_id.clone());
                    }
                    dirty = true;
                }
            }
            if dirty {
                orchestration::team::save_team(&team, &self.state.store_dir);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler impl
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlackboxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Blackbox: unified transcript search, knowledge management, and multi-provider agent orchestration")
    }
}

// ---------------------------------------------------------------------------
// Tail SSE endpoint (outside MCP)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TailQuery {
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    bro: Option<String>,
    #[serde(default)]
    provider: Option<String>,
}

async fn tail_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<TailQuery>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = state.tail_tx.subscribe();

    // Resolve team filter to a set of task IDs (dynamic — checks on each event)
    let team_name = query.team.clone();
    let bro_filter = query.bro.clone();
    let provider_filter = query.provider.and_then(|p| Provider::from_str(&p));
    let store_dir = state.store_dir.clone();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    // Apply provider filter — resolve from task store
                    if let Some(pf) = provider_filter {
                        // Check if this event's task matches the provider
                        let task_provider = state.task_store.read().unwrap()
                            .get(event.task_id())
                            .map(|t| t.inner.lock().unwrap().provider);
                        if task_provider != Some(pf) {
                            continue;
                        }
                    }
                    if let Some(ref bf) = bro_filter {
                        let bro = orchestration::team::find_bro_name_for_task(event.task_id(), &store_dir);
                        if bro.as_deref() != Some(bf.as_str()) {
                            continue;
                        }
                    }
                    if let Some(ref tn) = team_name {
                        if let Some(team) = orchestration::team::load_team(tn, &store_dir) {
                            let team_tasks: std::collections::HashSet<String> = team.members.iter()
                                .flat_map(|m| m.task_history.clone())
                                .collect();
                            if !team_tasks.contains(event.task_id()) {
                                continue;
                            }
                        }
                    }

                    // Enrich event with bro name for display
                    let bro_name = orchestration::team::find_bro_name_for_task(event.task_id(), &store_dir);
                    let mut evt_json = serde_json::to_value(&event).unwrap_or_default();
                    if let Some(ref name) = bro_name {
                        evt_json["bro_name"] = Value::String(name.clone());
                    }
                    let data = serde_json::to_string(&evt_json).unwrap_or_default();
                    yield Ok(Event::default().data(data));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("tail subscriber lagged by {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let home = dirs::home_dir().expect("cannot determine home directory");

    // Logging
    let log_dir = home.join(".claude-shared");
    let file_appender = tracing_appender::rolling::Builder::new()
        .max_log_files(3)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("blackbox")
        .filename_suffix("log")
        .build(&log_dir)
        .expect("failed to create log appender");

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "blackbox=info".into());

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .with(tracing_subscriber::fmt::layer().with_writer(file_appender).with_ansi(false))
        .init();

    std::panic::set_hook(Box::new(|info| {
        tracing::error!("PANIC: {}", info);
    }));

    // Transcript index roots
    let roots: Vec<(String, PathBuf)> = if let Ok(val) = std::env::var("TRANSCRIPT_SEARCH_ROOTS") {
        val.split(',')
            .filter_map(|entry| {
                let (name, path) = entry.split_once('=')?;
                let expanded = if path.starts_with('~') {
                    home.join(&path[2..])
                } else { PathBuf::from(path) };
                Some((name.to_string(), expanded))
            })
            .collect()
    } else {
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

    let codex_root = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let default = home.join(".codex");
            if default.join("sessions").exists() { Some(default) } else { None }
        });

    let index_path = std::env::var("TRANSCRIPT_SEARCH_INDEX_PATH")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let shared = home.join(".claude-shared").join("transcript-index");
            if shared.parent().map(|p| p.exists()).unwrap_or(false) { shared }
            else { home.join(".local/share/transcript-search/index") }
        });

    tracing::info!("Roots: {:?}", roots.iter().map(|(n, p)| format!("{n}={}", p.display())).collect::<Vec<_>>());
    if let Some(ref cr) = codex_root { tracing::info!("Codex root: {}", cr.display()); }
    tracing::info!("Index path: {}", index_path.display());

    let idx = TranscriptIndex::open_or_create(&index_path, roots, codex_root)?;

    let kb_path = home.join(".claude-shared").join("blackbox-knowledge.json");
    let kb = Knowledge::open(&kb_path)?;
    tracing::info!("Knowledge store: {}", kb_path.display());

    let th_path = home.join(".claude-shared").join("blackbox-threads.json");
    let th = Threads::open(&th_path)?;
    tracing::info!("Thread store: {}", th_path.display());

    // Orchestration state
    let store_dir = PathBuf::from(
        std::env::var("BRO_STORE").unwrap_or_else(|_| home.join(".bro").to_string_lossy().to_string())
    );
    let task_ttl = std::env::var("BRO_TASK_TTL_MS")
        .ok().and_then(|v| v.parse().ok())
        .unwrap_or(86_400_000u64);
    let task_store = TaskStore::load(&store_dir, task_ttl);

    let (tail_tx, _) = broadcast::channel::<TailEvent>(1024);

    // Spawn background reindex thread
    let reindex_interval = std::env::var("BLACKBOX_REINDEX_INTERVAL_SECS")
        .ok().and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);
    index::spawn_reindex_thread(
        idx.index_handle(),
        idx.reindex_config(),
        idx.field_handles(),
        std::time::Duration::from_secs(reindex_interval),
    );

    let shared = Arc::new(SharedState {
        idx: RwLock::new(idx),
        kb: RwLock::new(kb),
        threads: RwLock::new(th),
        task_store: Arc::new(RwLock::new(task_store)),
        tail_tx: tail_tx.clone(),
        store_dir: store_dir.clone(),
    });

    // MCP service
    let port: u16 = std::env::var("BBOX_PORT")
        .or_else(|_| std::env::var("BRO_PORT"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7263);

    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.child_token())
        .with_stateful_mode(true);

    let shared_for_mcp = shared.clone();
    let mcp_service: StreamableHttpService<BlackboxServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BlackboxServer::new(shared_for_mcp.clone())),
            Default::default(),
            config,
        );

    let app = axum::Router::new()
        .route("/tail", axum::routing::get(tail_handler))
        .with_state(shared.clone())
        .nest_service("/mcp", mcp_service);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    tracing::info!("blackboxd listening on http://127.0.0.1:{port}/mcp");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            ct.cancel();
        })
        .await?;

    // Persist tasks on shutdown
    shared.task_store.read().unwrap().persist(&store_dir);
    tracing::info!("blackboxd shut down");
    Ok(())
}

#[cfg(test)]
mod main_tests {
    use super::*;

    #[test]
    fn test_to_value_strips_nulls() {
        #[derive(Serialize)]
        struct TestParams {
            required: String,
            optional: Option<String>,
            another: Option<u64>,
        }
        let p = TestParams {
            required: "hello".into(),
            optional: None,
            another: Some(42),
        };
        let v = to_value(&p);
        let map = v.as_object().unwrap();
        assert_eq!(map.get("required").unwrap(), "hello");
        assert!(map.get("optional").is_none(), "null fields should be stripped");
        assert_eq!(map.get("another").unwrap(), 42);
    }
}
