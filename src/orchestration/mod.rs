pub mod providers;
pub mod brofile;
pub mod team;
pub mod tail;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::Notify;

use providers::{EventSink, Provider, Usage};

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled)
    }
}

/// Shared inner state of a task, updated by background readers.
pub struct TaskInner {
    pub id: String,
    pub provider: Provider,
    pub session_id: String,
    pub events: Vec<Value>,
    pub last_assistant_message: Option<String>,
    pub usage: Option<Usage>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub stderr: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub cwd: Option<String>,
}

pub struct Task {
    pub inner: Mutex<TaskInner>,
    pub notify: Arc<Notify>,
    /// Handle to the child process for cancellation. Only set while running.
    child_id: Mutex<Option<u32>>, // PID
}

impl Task {
    pub fn id(&self) -> String {
        self.inner.lock().id.clone()
    }
}

// ---------------------------------------------------------------------------
// Task Store
// ---------------------------------------------------------------------------

pub struct TaskStore {
    tasks: HashMap<String, Arc<Task>>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self { tasks: HashMap::new() }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.get(id).cloned()
    }

    pub fn insert(&mut self, id: String, task: Arc<Task>) {
        self.tasks.insert(id, task);
    }

    pub fn all_tasks(&self) -> Vec<Arc<Task>> {
        self.tasks.values().cloned().collect()
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

const MAX_PERSISTED_EVENTS: usize = 50;

#[derive(Serialize, Deserialize)]
struct PersistedTask {
    id: String,
    provider: Provider,
    session_id: String,
    events: Vec<Value>,
    last_assistant_message: Option<String>,
    usage: Option<Usage>,
    cost_usd: Option<f64>,
    num_turns: Option<u64>,
    stderr: String,
    status: TaskStatus,
    started_at: u64,
    completed_at: Option<u64>,
    exit_code: Option<i32>,
    cwd: Option<String>,
}

impl TaskStore {
    pub fn persist(&self, store_dir: &std::path::Path) {
        let records: Vec<PersistedTask> = self.tasks.values().map(|t| {
            let inner = t.inner.lock();
            PersistedTask {
                id: inner.id.clone(),
                provider: inner.provider,
                session_id: inner.session_id.clone(),
                events: inner.events.iter().rev().take(MAX_PERSISTED_EVENTS).rev().cloned().collect(),
                last_assistant_message: inner.last_assistant_message.clone(),
                usage: inner.usage.clone(),
                cost_usd: inner.cost_usd,
                num_turns: inner.num_turns,
                stderr: inner.stderr.chars().take(2000).collect(),
                status: inner.status,
                started_at: inner.started_at,
                completed_at: inner.completed_at,
                exit_code: inner.exit_code,
                cwd: inner.cwd.clone(),
            }
        }).collect();

        let file = store_dir.join("tasks.json");
        let tmp = store_dir.join("tasks.json.tmp");
        if let Ok(data) = serde_json::to_string(&records) {
            let _ = std::fs::create_dir_all(store_dir);
            if std::fs::write(&tmp, &data).is_ok() {
                let _ = std::fs::rename(&tmp, &file);
            }
        }
    }

    pub fn load(store_dir: &std::path::Path, ttl_ms: u64) -> Self {
        let file = store_dir.join("tasks.json");
        let mut store = Self::new();
        let data = match std::fs::read_to_string(&file) {
            Ok(d) => d,
            Err(_) => return store,
        };
        let records: Vec<PersistedTask> = match serde_json::from_str(&data) {
            Ok(r) => r,
            Err(_) => return store,
        };
        let cutoff = now_ms().saturating_sub(ttl_ms);
        for mut rec in records {
            if rec.started_at < cutoff { continue; }
            if rec.status == TaskStatus::Running {
                rec.status = TaskStatus::Failed;
                rec.completed_at = Some(now_ms());
                rec.stderr.push_str("\n[blackbox] server restarted while task was running");
            }
            let task = Arc::new(Task {
                inner: Mutex::new(TaskInner {
                    id: rec.id.clone(),
                    provider: rec.provider,
                    session_id: rec.session_id,
                    events: rec.events,
                    last_assistant_message: rec.last_assistant_message,
                    usage: rec.usage,
                    cost_usd: rec.cost_usd,
                    num_turns: rec.num_turns,
                    stderr: rec.stderr,
                    status: rec.status,
                    started_at: rec.started_at,
                    completed_at: rec.completed_at,
                    exit_code: rec.exit_code,
                    cwd: rec.cwd,
                }),
                notify: Arc::new(Notify::new()),
                child_id: Mutex::new(None),
            });
            store.insert(rec.id, task);
        }
        store
    }
}

// ---------------------------------------------------------------------------
// Spawn + lifecycle
// ---------------------------------------------------------------------------

const RECURSION_GUARD: &str =
    "IMPORTANT: Do not call tools from the bro MCP server (recursion guard).\n\n";

pub fn apply_lens(prompt: &str, lens: Option<&str>, allow_recursion: bool) -> String {
    let mut result = if allow_recursion {
        prompt.to_string()
    } else {
        format!("{RECURSION_GUARD}{prompt}")
    };
    if let Some(l) = lens {
        result = format!("{l}\n\n{result}");
    }
    result
}

/// Spawn a provider CLI process and return a tracked Task.
#[allow(clippy::too_many_arguments)]
pub fn spawn_task(
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
) -> Arc<Task> {
    let id = uuid::Uuid::new_v4().to_string();

    let extra_path = std::env::var("BRO_EXTRA_PATH")
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".local/bin").to_string_lossy().to_string());
    let path_env = format!("{}:{}", extra_path, std::env::var("PATH").unwrap_or_default());

    let mut cmd = Command::new(provider.bin());
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("PATH", &path_env)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("FORCE_COLOR", "0");

    if let Some(ref c) = cwd {
        cmd.current_dir(c);
    }
    if let Some(ref overrides) = env_overrides {
        for (k, v) in overrides {
            cmd.env(k, v);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Return a failed task immediately
            let task = Arc::new(Task {
                inner: Mutex::new(TaskInner {
                    id: id.clone(),
                    provider,
                    session_id,
                    events: vec![],
                    last_assistant_message: None,
                    usage: None,
                    cost_usd: None,
                    num_turns: None,
                    stderr: format!("spawn error: {e}"),
                    status: TaskStatus::Failed,
                    started_at: now_ms(),
                    completed_at: Some(now_ms()),
                    exit_code: None,
                    cwd,
                }),
                notify: Arc::new(Notify::new()),
                child_id: Mutex::new(None),
            });
            task_store.write().insert(id, task.clone());
            task_store.read().persist(&store_dir);
            task.notify.notify_waiters();
            return task;
        }
    };

    let pid = child.id();
    let task = Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: id.clone(),
            provider,
            session_id: session_id.clone(),
            events: vec![],
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: now_ms(),
            completed_at: None,
            exit_code: None,
            cwd: cwd.clone(),
        }),
        notify: Arc::new(Notify::new()),
        child_id: Mutex::new(pid),
    });

    task_store.write().insert(id.clone(), task.clone());

    // Emit tail event
    let _ = tail_tx.send(tail::TailEvent::TaskStarted {
        task_id: id.clone(),
        provider,
        bro_name: None,
    });

    // Spawn stdout reader — signals completion via oneshot so the process
    // waiter can ensure all output is consumed before marking the task done.
    let stdout = child.stdout.take().unwrap();
    let stderr_handle = child.stderr.take().unwrap();
    let task_ref = task.clone();
    let is_streaming = provider.is_streaming_json();
    let tail_tx_clone = tail_tx.clone();
    let task_id_clone = id.clone();

    let (stdout_done_tx, stdout_done_rx) = tokio::sync::oneshot::channel::<()>();

    if is_streaming {
        // Line-by-line JSON parsing
        tokio::spawn(async move {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut last_emitted_snippet: Option<String> = None;
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(evt) = serde_json::from_str::<Value>(&line) {
                    let snippet_to_emit = {
                        let mut inner = task_ref.inner.lock();
                        inner.events.push(evt.clone());
                        let mut sink = EventSink {
                            last_assistant_message: inner.last_assistant_message.clone(),
                            usage: inner.usage.clone(),
                            cost_usd: inner.cost_usd,
                            num_turns: inner.num_turns,
                            session_id: if inner.session_id != "pending" {
                                Some(inner.session_id.clone())
                            } else {
                                None
                            },
                        };
                        provider.parse_event(&evt, &mut sink);
                        inner.last_assistant_message = sink.last_assistant_message;
                        inner.usage = sink.usage;
                        inner.cost_usd = sink.cost_usd;
                        inner.num_turns = sink.num_turns;
                        if let Some(sid) = sink.session_id {
                            if inner.session_id == "pending" {
                                inner.session_id = sid;
                            }
                        }
                        inner.last_assistant_message.as_ref().map(|msg| {
                            if msg.len() > 80 { msg[..80].to_string() } else { msg.clone() }
                        })
                    };

                    if let Some(snippet) = snippet_to_emit {
                        if last_emitted_snippet.as_deref() != Some(snippet.as_str()) {
                            let _ = tail_tx_clone.send(tail::TailEvent::TaskProgress {
                                task_id: task_id_clone.clone(),
                                activity: snippet.clone(),
                            });
                            last_emitted_snippet = Some(snippet);
                        }
                    }
                }
            }
            let _ = stdout_done_tx.send(());
        });
    } else {
        // Bulk stdout collection
        let task_ref_bulk = task.clone();
        tokio::spawn(async move {
            let mut buf = String::new();
            let mut reader = tokio::io::BufReader::new(stdout);
            loop {
                let mut chunk = String::new();
                match reader.read_line(&mut chunk).await {
                    Ok(0) => break,
                    Ok(_) => buf.push_str(&chunk),
                    Err(_) => break,
                }
            }
            if !buf.trim().is_empty() {
                let mut inner = task_ref_bulk.inner.lock();
                let mut sink = EventSink {
                    last_assistant_message: inner.last_assistant_message.clone(),
                    usage: inner.usage.clone(),
                    cost_usd: inner.cost_usd,
                    num_turns: inner.num_turns,
                    session_id: None,
                };
                provider.parse_bulk_output(buf.trim(), &mut sink);
                inner.last_assistant_message = sink.last_assistant_message;
                inner.usage = sink.usage;
                inner.cost_usd = sink.cost_usd;
                inner.num_turns = sink.num_turns;
                if let Some(sid) = sink.session_id {
                    if inner.session_id == "pending" {
                        inner.session_id = sid;
                    }
                }
            }
            let _ = stdout_done_tx.send(());
        });
    }

    // Spawn stderr reader
    let task_ref_err = task.clone();
    tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr_handle);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut inner = task_ref_err.inner.lock();
            inner.stderr.push_str(&line);
            inner.stderr.push('\n');
        }
    });

    // Spawn process waiter — waits for BOTH the process exit AND stdout reader
    // to finish before marking the task terminal. This ensures results are
    // fully parsed before waiters are notified.
    let task_ref_wait = task.clone();
    let task_id_wait = id.clone();
    let tail_tx_wait = tail_tx;
    tokio::spawn(async move {
        let status = child.wait().await;
        // Wait for stdout reader to finish before processing results —
        // ensures all events/results are parsed before we mark terminal.
        let _ = stdout_done_rx.await;
        let code = status.ok().and_then(|s| s.code());

        // Post-hoc vibe session discovery
        if provider == Provider::Vibe {
            let inner = task_ref_wait.inner.lock();
            if inner.session_id == "pending" {
                if let Some(ref c) = inner.cwd {
                    let start = inner.started_at;
                    let cwd_clone = c.clone();
                    drop(inner); // release lock before blocking call
                    if let Some(sid) = providers::discover_vibe_session(start, &cwd_clone) {
                        task_ref_wait.inner.lock().session_id = sid;
                    }
                }
            }
        }

        {
            let mut inner = task_ref_wait.inner.lock();
            inner.exit_code = code;
            if inner.status != TaskStatus::Cancelled {
                inner.status = if code == Some(0) { TaskStatus::Completed } else { TaskStatus::Failed };
            }
            inner.completed_at = Some(now_ms());

            let elapsed = format_elapsed(inner.started_at, inner.completed_at);
            match inner.status {
                TaskStatus::Completed => {
                    let _ = tail_tx_wait.send(tail::TailEvent::TaskCompleted {
                        task_id: task_id_wait.clone(),
                        elapsed,
                        cost: inner.cost_usd,
                    });
                }
                TaskStatus::Failed => {
                    let _ = tail_tx_wait.send(tail::TailEvent::TaskFailed {
                        task_id: task_id_wait.clone(),
                        elapsed,
                        error: inner.stderr.chars().take(200).collect(),
                    });
                }
                _ => {}
            }
        }

        // Propagate session ID to team members
        {
            let inner = task_ref_wait.inner.lock();
            if inner.session_id != "pending" {
                let sid = inner.session_id.clone();
                let tid = inner.id.clone();
                drop(inner);
                team::propagate_session_id(&tid, &sid, &store_dir);
            }
        }

        // Persist and notify waiters
        task_store.read().persist(&store_dir);
        task_ref_wait.notify.notify_waiters();
    });

    task
}

/// Wait for a task to complete. Returns immediately if already terminal.
/// Uses `enable()` on the Notify future before checking status to avoid
/// lost-wakeup races (TOCTOU between status check and await).
pub async fn wait_for_task(task: &Task) {
    loop {
        // Register interest BEFORE checking status — avoids lost wakeup if
        // the task completes between our check and our await.
        let notified = task.notify.notified();
        tokio::pin!(notified);
        // Enable the future so it will capture a notify even if we haven't
        // .await'd yet (this is the critical fix for the race).
        notified.as_mut().enable();

        {
            let inner = task.inner.lock();
            if inner.status.is_terminal() {
                return;
            }
        }
        notified.await;
    }
}

/// Wait with timeout. Returns true if completed, false if timed out.
pub async fn wait_for_task_with_timeout(task: &Task, timeout_secs: Option<f64>) -> bool {
    match timeout_secs {
        None => {
            wait_for_task(task).await;
            true
        }
        Some(secs) => {
            let duration = std::time::Duration::from_secs_f64(secs);
            match tokio::time::timeout(duration, wait_for_task(task)).await {
                Ok(()) => true,
                Err(_) => false,
            }
        }
    }
}

/// Cancel a running task.
pub fn cancel_task(task: &Task, task_store: &RwLock<TaskStore>, store_dir: &std::path::Path) -> Result<(), String> {
    let mut inner = task.inner.lock();
    if inner.status != TaskStatus::Running {
        return Err(format!("Task already {}", serde_json::to_string(&inner.status).unwrap_or_default()));
    }
    inner.status = TaskStatus::Cancelled;
    inner.completed_at = Some(now_ms());
    drop(inner);

    // Kill the child process
    if let Some(pid) = task.child_id.lock().take() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    task_store.read().persist(store_dir);
    task.notify.notify_waiters();
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn format_elapsed(started_at: u64, completed_at: Option<u64>) -> String {
    let end = completed_at.unwrap_or_else(now_ms);
    let ms = end.saturating_sub(started_at);
    let s = ms / 1000 ;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

pub fn task_result_json(task: &Task) -> Value {
    let inner = task.inner.lock();
    let mut obj = serde_json::json!({
        "taskId": inner.id,
        "provider": inner.provider,
        "sessionId": inner.session_id,
        "status": inner.status,
        "elapsed": format_elapsed(inner.started_at, inner.completed_at),
    });

    if let Some(ref msg) = inner.last_assistant_message {
        obj["result"] = Value::String(msg.clone());
    }
    if inner.status == TaskStatus::Completed || inner.status == TaskStatus::Failed {
        if let Some(ref u) = inner.usage {
            obj["usage"] = serde_json::json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
            });
        }
        if let Some(cost) = inner.cost_usd {
            obj["costUsd"] = Value::from(cost);
        }
        if let Some(turns) = inner.num_turns {
            obj["numTurns"] = Value::from(turns);
        }
    }
    if inner.status == TaskStatus::Failed {
        if let Some(code) = inner.exit_code {
            obj["exitCode"] = Value::from(code);
        }
        if !inner.stderr.is_empty() {
            let truncated: String = inner.stderr.chars().take(2000).collect();
            obj["stderr"] = Value::String(truncated);
        }
    }
    obj
}

pub fn task_status_json(task: &Task, tail: usize) -> Value {
    let mut obj = task_result_json(task);
    let inner = task.inner.lock();
    obj["eventCount"] = Value::from(inner.events.len());
    if tail > 0 && !inner.events.is_empty() {
        let start = inner.events.len().saturating_sub(tail);
        obj["recentEvents"] = Value::Array(inner.events[start..].to_vec());
    }
    obj
}

pub fn timeout_snapshot_json(task: &Task) -> Value {
    let inner = task.inner.lock();
    let elapsed = format_elapsed(inner.started_at, None);
    let event_count = inner.events.len();
    let last_activity = inner.last_assistant_message.as_deref().map(|msg| {
        let clean = msg.replace('\n', " ");
        if clean.len() > 80 { format!("{}…", &clean[..80]) } else { clean }
    });

    let keep_going = if inner.status.is_terminal() {
        "no"
    } else if event_count > 0 {
        "yes"
    } else {
        "check_status"
    };

    serde_json::json!({
        "taskId": inner.id,
        "provider": inner.provider,
        "sessionId": inner.session_id,
        "status": inner.status,
        "timed_out": true,
        "elapsed": elapsed,
        "eventCount": event_count,
        "keep_going": keep_going,
        "lastAssistantSnippet": last_activity,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_status_is_terminal() {
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(1000, Some(6000)), "5s");
        assert_eq!(format_elapsed(1000, Some(91000)), "1m 30s");
    }

    #[test]
    fn test_apply_lens() {
        let result = apply_lens("do stuff", None, false);
        assert!(result.starts_with("IMPORTANT:"));
        assert!(result.contains("do stuff"));

        let result = apply_lens("do stuff", Some("You are a reviewer"), false);
        assert!(result.starts_with("You are a reviewer"));
        assert!(result.contains("IMPORTANT:"));
        assert!(result.contains("do stuff"));

        let result = apply_lens("do stuff", None, true);
        assert_eq!(result, "do stuff");
    }

    #[test]
    fn test_task_result_json_completed() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t1".into(),
                provider: Provider::Claude,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: Some("Done!".into()),
                usage: Some(Usage { input_tokens: 100, output_tokens: 50 }),
                cost_usd: Some(0.05),
                num_turns: Some(3),
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: 1000,
                completed_at: Some(5000),
                exit_code: Some(0),
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let json = task_result_json(&task);
        assert_eq!(json["taskId"], "t1");
        assert_eq!(json["result"], "Done!");
        assert_eq!(json["costUsd"], 0.05);
        assert_eq!(json["usage"]["input_tokens"], 100);
    }

    #[test]
    fn test_task_result_json_failed() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t2".into(),
                provider: Provider::Codex,
                session_id: "s2".into(),
                events: vec![],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: "something went wrong".into(),
                status: TaskStatus::Failed,
                started_at: 1000,
                completed_at: Some(2000),
                exit_code: Some(1),
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let json = task_result_json(&task);
        assert_eq!(json["exitCode"], 1);
        assert!(json["stderr"].as_str().unwrap().contains("something went wrong"));
    }
}

#[cfg(test)]
mod async_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn test_wait_for_task_already_terminal() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t1".into(),
                provider: Provider::Claude,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: now_ms(),
                completed_at: Some(now_ms()),
                exit_code: Some(0),
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });
        // Should return immediately without blocking
        wait_for_task(&task).await;
    }

    #[tokio::test]
    async fn test_wait_for_task_notify_race() {
        // Simulate the race: task completes between status check and await
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t2".into(),
                provider: Provider::Claude,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let task_clone = task.clone();
        // Complete the task after a brief delay
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            {
                let mut inner = task_clone.inner.lock();
                inner.status = TaskStatus::Completed;
                inner.completed_at = Some(now_ms());
            }
            task_clone.notify.notify_waiters();
        });

        // This should not hang even if the notify fires during the gap
        let completed = wait_for_task_with_timeout(&task, Some(5.0)).await;
        assert!(completed, "wait_for_task should have completed");
    }

    #[tokio::test]
    async fn test_wait_for_task_timeout() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t3".into(),
                provider: Provider::Claude,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        // Should timeout after 0.1s
        let completed = wait_for_task_with_timeout(&task, Some(0.1)).await;
        assert!(!completed, "should have timed out");
    }
}
