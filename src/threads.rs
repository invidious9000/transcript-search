use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Schema ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Open,
    Active,
    Stale,
    Resolved,
    Promoted, // graduated to graph (finding/inquiry/task)
}

impl ThreadStatus {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "active" => Some(Self::Active),
            "stale" => Some(Self::Stale),
            "resolved" => Some(Self::Resolved),
            "promoted" => Some(Self::Promoted),
            _ => None,
        }
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Open => "open",
            Self::Active => "active",
            Self::Stale => "stale",
            Self::Resolved => "resolved",
            Self::Promoted => "promoted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionLink {
    pub session_id: String,
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub linked_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub topic: String,
    pub project: String,
    pub status: ThreadStatus,
    pub sessions: Vec<SessionLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handoff_doc: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promoted_to: Option<String>, // graph entity ref when promoted
    pub created_at: String,
    pub last_activity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ThreadStore {
    pub version: u32,
    pub threads: Vec<Thread>,
}

impl ThreadStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            threads: Vec::new(),
        }
    }
}

// ── Store operations ───────────────────────────────────────────────

pub struct Threads {
    store_path: PathBuf,
    store: ThreadStore,
}

impl Threads {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            ThreadStore::new()
        };
        Ok(Self {
            store_path: store_path.to_path_buf(),
            store,
        })
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(&self.store)?;
        let tmp = self.store_path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, &self.store_path)?;
        Ok(())
    }

    fn now_iso() -> String {
        use std::time::SystemTime;
        let d = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = d.as_secs();
        let (days_since_epoch, rem) = (secs / 86400, secs % 86400);
        let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

        // Simple date calculation from epoch days
        let mut y: i64 = 1970;
        let mut remaining_days = days_since_epoch as i64;
        loop {
            let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 366 } else { 365 };
            if remaining_days < days_in_year { break; }
            remaining_days -= days_in_year;
            y += 1;
        }
        let months = [31, if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 29 } else { 28 },
                       31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        let mut mo = 0;
        for days_in_month in &months {
            if remaining_days < *days_in_month { break; }
            remaining_days -= days_in_month;
            mo += 1;
        }
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, mo + 1, remaining_days + 1, h, m, s)
    }

    fn gen_id() -> String {
        use std::time::SystemTime;
        let d = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let hash = d.as_nanos() ^ 0x517cc1b727220a95;
        format!("thread-{:08x}", hash as u32)
    }

    // ── blackbox_thread (CRUD) ─────────────────────────────────────

    pub fn thread(&mut self, args: &Value) -> Result<String> {
        let action = args["action"]
            .as_str()
            .context("'action' is required (open, continue, resolve, promote)")?;

        match action {
            "open" => self.thread_open(args),
            "continue" => self.thread_continue(args),
            "resolve" => self.thread_resolve(args),
            "promote" => self.thread_promote(args),
            "rename" => self.thread_rename(args),
            _ => anyhow::bail!("Unknown action: {}. Use: open, continue, resolve, promote, rename", action),
        }
    }

    fn thread_open(&mut self, args: &Value) -> Result<String> {
        let topic = args["topic"]
            .as_str()
            .context("'topic' is required")?;
        let project = args["project"].as_str().unwrap_or("");
        let handoff_doc = args["handoff_doc"].as_str().map(String::from);
        let note = args["note"].as_str();

        let now = Self::now_iso();
        let id = Self::gen_id();

        let mut sessions = Vec::new();
        if let Some(sid) = args["session_id"].as_str() {
            sessions.push(SessionLink {
                session_id: sid.to_string(),
                provider: args["provider"].as_str().unwrap_or("unknown").to_string(),
                name: args["session_name"].as_str().map(String::from),
                linked_at: now.clone(),
            });
        }

        let mut notes = Vec::new();
        if let Some(n) = note {
            notes.push(n.to_string());
        }

        let name = args["name"].as_str().map(String::from);

        let thread = Thread {
            id: id.clone(),
            name,
            topic: topic.to_string(),
            project: project.to_string(),
            status: ThreadStatus::Open,
            sessions,
            handoff_doc,
            notes,
            promoted_to: None,
            created_at: now.clone(),
            last_activity: now,
            resolved_at: None,
        };

        self.store.threads.push(thread);
        self.save()?;

        Ok(format!("Thread created: {} — \"{}\"", id, topic))
    }

    fn thread_continue(&mut self, args: &Value) -> Result<String> {
        let id = args["id"]
            .as_str()
            .context("'id' is required")?;

        let thread = self.store.threads.iter_mut()
            .find(|t| t.id == id)
            .context("Thread not found")?;

        let now = Self::now_iso();

        // Link a session
        if let Some(sid) = args["session_id"].as_str() {
            thread.sessions.push(SessionLink {
                session_id: sid.to_string(),
                provider: args["provider"].as_str().unwrap_or("unknown").to_string(),
                name: args["session_name"].as_str().map(String::from),
                linked_at: now.clone(),
            });
        }

        // Add a note
        if let Some(note) = args["note"].as_str() {
            thread.notes.push(note.to_string());
        }

        // Update handoff doc
        if let Some(doc) = args["handoff_doc"].as_str() {
            thread.handoff_doc = Some(doc.to_string());
        }

        // Update name if provided
        if let Some(name) = args["name"].as_str() {
            thread.name = Some(name.to_string());
        }

        thread.status = ThreadStatus::Active;
        thread.last_activity = now;
        let topic = thread.topic.clone();

        self.save()?;

        Ok(format!("Thread {} continued — \"{}\"", id, topic))
    }

    fn thread_resolve(&mut self, args: &Value) -> Result<String> {
        let id = args["id"]
            .as_str()
            .context("'id' is required")?;

        let thread = self.store.threads.iter_mut()
            .find(|t| t.id == id)
            .context("Thread not found")?;

        let now = Self::now_iso();

        if let Some(note) = args["note"].as_str() {
            thread.notes.push(note.to_string());
        }

        thread.status = ThreadStatus::Resolved;
        thread.last_activity = now.clone();
        thread.resolved_at = Some(now);
        let topic = thread.topic.clone();

        self.save()?;

        Ok(format!("Thread {} resolved — \"{}\"", id, topic))
    }

    fn thread_promote(&mut self, args: &Value) -> Result<String> {
        let id = args["id"]
            .as_str()
            .context("'id' is required")?;
        let promoted_to = args["promoted_to"]
            .as_str()
            .context("'promoted_to' is required (graph entity reference)")?;

        let thread = self.store.threads.iter_mut()
            .find(|t| t.id == id)
            .context("Thread not found")?;

        let now = Self::now_iso();

        if let Some(note) = args["note"].as_str() {
            thread.notes.push(note.to_string());
        }

        thread.status = ThreadStatus::Promoted;
        thread.promoted_to = Some(promoted_to.to_string());
        thread.last_activity = now.clone();
        thread.resolved_at = Some(now);
        let topic = thread.topic.clone();

        self.save()?;

        Ok(format!(
            "Thread {} promoted to {} — \"{}\"",
            id, promoted_to, topic
        ))
    }

    fn thread_rename(&mut self, args: &Value) -> Result<String> {
        let id = args["id"]
            .as_str()
            .context("'id' is required")?;
        let new_name = args["name"]
            .as_str()
            .context("'name' is required for rename")?;

        let thread = self.store.threads.iter_mut()
            .find(|t| t.id == id)
            .context("Thread not found")?;

        thread.name = Some(new_name.to_string());
        thread.last_activity = Self::now_iso();
        let topic = thread.topic.clone();

        self.save()?;

        Ok(format!("Thread {} renamed to \"{}\" (topic: {})", id, new_name, topic))
    }

    // ── blackbox_thread_list (query) ───────────────────────────────

    pub fn thread_list(&self, args: &Value) -> Result<String> {
        let status_filter = args["status"].as_str();
        let project_filter = args["project"].as_str();
        let name_filter = args["name"].as_str();
        let stale_days = args["stale_days"].as_u64();
        let include_resolved = args["include_resolved"].as_bool().unwrap_or(false);

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut results: Vec<&Thread> = Vec::new();

        for thread in &self.store.threads {
            // Status filter
            if let Some(sf) = status_filter {
                if let Some(target) = ThreadStatus::from_str(sf) {
                    if thread.status != target {
                        continue;
                    }
                }
            } else if !include_resolved {
                // Default: exclude resolved and promoted
                if thread.status == ThreadStatus::Resolved || thread.status == ThreadStatus::Promoted {
                    continue;
                }
            }

            // Project filter
            if let Some(pf) = project_filter {
                if !thread.project.to_lowercase().contains(&pf.to_lowercase()) {
                    continue;
                }
            }

            // Name filter
            if let Some(nf) = name_filter {
                let nf_lower = nf.to_lowercase();
                let name_matches = thread.name.as_ref()
                    .map(|n| n.to_lowercase().contains(&nf_lower))
                    .unwrap_or(false);
                let topic_matches = thread.topic.to_lowercase().contains(&nf_lower);
                if !name_matches && !topic_matches {
                    continue;
                }
            }

            // Staleness filter
            if let Some(days) = stale_days {
                let age = self.thread_age_days(thread, now_secs);
                if age < days {
                    continue;
                }
            }

            results.push(thread);
        }

        if results.is_empty() {
            return Ok("No threads found.".to_string());
        }

        // Sort by last_activity descending
        results.sort_by(|a, b| b.last_activity.cmp(&a.last_activity));

        let mut lines = Vec::new();
        for t in &results {
            let age = self.thread_age_days(t, now_secs);
            let age_str = if age == 0 {
                "today".to_string()
            } else {
                format!("{}d ago", age)
            };

            let sessions_str = if t.sessions.is_empty() {
                "no sessions".to_string()
            } else {
                let names: Vec<String> = t.sessions.iter().map(|s| {
                    match s.name.as_deref() {
                        Some(n) => n.to_string(),
                        None => s.session_id.chars().take(8).collect::<String>(),
                    }
                }).collect();
                names.join(", ")
            };

            let handoff = t.handoff_doc.as_deref().unwrap_or("-");
            let project = if t.project.is_empty() { "-" } else {
                t.project.rsplit('/').next().unwrap_or(&t.project)
            };

            let display_name = t.name.as_deref().unwrap_or("-");

            lines.push(format!(
                "{} | {} | {} | {} | {} | {} | {} | {}",
                t.id,
                display_name,
                t.status.as_str(),
                age_str,
                project,
                t.topic,
                sessions_str,
                handoff,
            ));
        }

        let header = format!("{} thread(s)", results.len());
        Ok(format!("{}\n\n{}", header, lines.join("\n")))
    }

    fn thread_age_days(&self, thread: &Thread, now_secs: u64) -> u64 {
        // Parse ISO timestamp to approximate epoch seconds
        let ts = &thread.last_activity;
        if ts.len() < 10 {
            return 0;
        }
        let y: i64 = ts[0..4].parse().unwrap_or(2026);
        let m: u32 = ts[5..7].parse().unwrap_or(1);
        let d: u32 = ts[8..10].parse().unwrap_or(1);

        // Rough epoch calc
        let mut epoch_days: i64 = 0;
        for yr in 1970..y {
            epoch_days += if yr % 4 == 0 && (yr % 100 != 0 || yr % 400 == 0) { 366 } else { 365 };
        }
        let months = [31, if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) { 29 } else { 28 },
                       31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        for i in 0..(m as usize - 1).min(11) {
            epoch_days += months[i] as i64;
        }
        epoch_days += d as i64 - 1;

        let activity_secs = epoch_days as u64 * 86400;
        now_secs.saturating_sub(activity_secs) / 86400
    }
}
