use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Schema ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Profile,
    Convention,
    Steering,
    Build,
    Tool,
    Memory,
    Workflow,
}

impl Category {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "profile" => Some(Self::Profile),
            "convention" => Some(Self::Convention),
            "steering" => Some(Self::Steering),
            "build" => Some(Self::Build),
            "tool" => Some(Self::Tool),
            "memory" => Some(Self::Memory),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }

    fn heading(&self) -> &str {
        match self {
            Self::Profile => "User Profile",
            Self::Convention => "Conventions",
            Self::Steering => "Provider Steering",
            Self::Build => "Build & Test",
            Self::Tool => "Tools",
            Self::Memory => "Memory",
            Self::Workflow => "Workflow",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Critical,
    Standard,
    Supplementary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Draft,
    Superseded,
    Disabled,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    UserConfirmed,
    AgentInferred,
    Imported,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeEntry {
    pub id: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub variants: HashMap<String, String>, // provider → alternative content
    pub category: Category,
    pub scope: String, // "global" or "project"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default)]
    pub providers: Vec<String>,
    pub priority: Priority,
    #[serde(default = "default_weight")]
    pub weight: u32,
    pub status: Status,
    pub approval: Approval,
    #[serde(default = "default_true")]
    pub render: bool,              // false = indexed only, never rendered into markdown
    #[serde(default = "default_true")]
    pub decay: bool,               // false = invariant, never ages out or gets staleness-reviewed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_at: Option<String>, // soft staleness checkpoint (ISO 8601)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub recall_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_recalled: Option<String>,
}

fn default_weight() -> u32 {
    100
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Serialize, Deserialize)]
pub struct KnowledgeStore {
    pub version: u32,
    pub entries: Vec<KnowledgeEntry>,
}

impl KnowledgeStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            entries: Vec::new(),
        }
    }
}

// ── Store operations ───────────────────────────────────────────────

pub struct Knowledge {
    store_path: PathBuf,
    store: KnowledgeStore,
}

impl Knowledge {
    pub fn open(store_path: &Path) -> Result<Self> {
        let store = if store_path.exists() {
            let raw = fs::read_to_string(store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?
        } else {
            KnowledgeStore::new()
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
        // Simple UTC timestamp
        let d = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = d.as_secs();
        let days = secs / 86400;
        let time_secs = secs % 86400;
        let hours = time_secs / 3600;
        let mins = (time_secs % 3600) / 60;
        let s = time_secs % 60;
        // Approximate date calculation (good enough for timestamps)
        let (year, month, day) = epoch_days_to_date(days);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
            year, month, day, hours, mins, s
        )
    }

    fn gen_id() -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .hash(&mut h);
        std::process::id().hash(&mut h);
        format!("{:08x}", h.finish() as u32)
    }

    fn is_expired(entry: &KnowledgeEntry) -> bool {
        if let Some(ref exp) = entry.expires_at {
            let now = Self::now_iso();
            exp.as_str() < now.as_str() // ISO 8601 string comparison works for ordering
        } else {
            false
        }
    }

    fn active_entries(&self) -> impl Iterator<Item = &KnowledgeEntry> {
        self.store.entries.iter().filter(|e| {
            e.status == Status::Active && !Self::is_expired(e)
        })
    }

    /// Active entries that should be rendered into markdown (excludes indexed-only).
    fn renderable_entries(&self) -> impl Iterator<Item = &KnowledgeEntry> {
        self.active_entries().filter(|e| e.render)
    }

    // ── CRUD ───────────────────────────────────────────────────────

    pub fn learn(&mut self, args: &Value, from_agent: bool) -> Result<String> {
        let content = args["content"]
            .as_str()
            .context("'content' is required")?
            .to_string();
        let category_str = args["category"]
            .as_str()
            .context("'category' is required")?;
        let category =
            Category::from_str(category_str).context("invalid category")?;
        let title = args["title"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| {
                // Generate title from first ~60 chars of content
                let t = content.chars().take(60).collect::<String>();
                if content.len() > 60 {
                    format!("{}...", t)
                } else {
                    t
                }
            });
        let scope = args["scope"].as_str().unwrap_or("global").to_string();
        let project = args["project"].as_str().map(String::from);
        let providers: Vec<String> = args["providers"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let priority = match args["priority"].as_str().unwrap_or("standard") {
            "critical" => Priority::Critical,
            "supplementary" => Priority::Supplementary,
            _ => Priority::Standard,
        };
        let weight = args["weight"].as_u64().unwrap_or(100) as u32;
        let expires_at = args["expires_at"].as_str().map(String::from);
        let supersedes = args["supersedes"].as_str().map(String::from);

        let now = Self::now_iso();
        let approval = if from_agent {
            Approval::AgentInferred
        } else {
            Approval::UserConfirmed
        };

        // Update existing or create new
        if let Some(id) = args["id"].as_str() {
            if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                entry.content = content;
                entry.title = title;
                entry.category = category;
                entry.priority = priority;
                entry.weight = weight;
                entry.providers = providers;
                entry.updated_at = now;
                if let Some(exp) = expires_at {
                    entry.expires_at = Some(exp);
                }
                // Apply optional fields when explicitly provided
                if args.get("scope").is_some() {
                    entry.scope = args["scope"].as_str().unwrap_or("global").to_string();
                }
                if args.get("project").is_some() {
                    entry.project = args["project"].as_str().map(String::from);
                }
                if args.get("decay").is_some() {
                    entry.decay = args["decay"].as_bool().unwrap_or(true);
                }
                if args.get("render").is_some() {
                    entry.render = args["render"].as_bool().unwrap_or(true);
                }
                if args.get("review_at").is_some() {
                    entry.review_at = args["review_at"].as_str().map(String::from);
                }
                if let Some(ref sup_id) = supersedes {
                    entry.supersedes = Some(sup_id.clone());
                }
                let sup_target = supersedes.clone();
                self.save()?;
                // Supersede the old entry outside the mutable borrow
                if let Some(sup_id) = sup_target {
                    if let Some(old) = self.store.entries.iter_mut().find(|e| e.id == sup_id) {
                        old.status = Status::Superseded;
                        old.updated_at = Self::now_iso();
                    }
                    self.save()?;
                }
                return Ok(format!("Updated entry {}", id));
            }
        }

        // Mark superseded entry
        if let Some(ref sup_id) = supersedes {
            if let Some(old) = self.store.entries.iter_mut().find(|e| e.id == *sup_id) {
                old.status = Status::Superseded;
            }
        }

        let id = Self::gen_id();
        let variants: HashMap<String, String> = args["variants"]
            .as_object()
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let decay = args["decay"].as_bool().unwrap_or(true);
        let review_at = args["review_at"].as_str().map(String::from);

        let entry = KnowledgeEntry {
            id: id.clone(),
            title,
            content,
            variants,
            category,
            scope,
            project,
            providers,
            priority,
            weight,
            render: true,
            decay,
            review_at,
            status: Status::Active,
            approval,
            supersedes,
            expires_at,
            source: if from_agent {
                args["source"]
                    .as_str()
                    .unwrap_or("agent")
                    .to_string()
            } else {
                "user".to_string()
            },
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        };

        self.store.entries.push(entry);
        self.save()?;
        Ok(format!("Created entry {}", id))
    }

    /// Internal learn — used by absorption. No render trigger.
    pub fn learn_internal(&mut self, args: &Value) -> Result<String> {
        // Same as learn but always marks as imported
        let mut args = args.clone();
        if let Some(obj) = args.as_object_mut() {
            obj.insert(
                "source".to_string(),
                serde_json::json!("imported"),
            );
        }
        // Use the learn path but flag as imported
        let content = args["content"]
            .as_str()
            .context("'content' is required")?
            .to_string();
        let category_str = args["category"]
            .as_str()
            .unwrap_or("memory");
        let category =
            Category::from_str(category_str).unwrap_or(Category::Memory);
        let title = args["title"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| {
                let t = content.chars().take(60).collect::<String>();
                if content.len() > 60 {
                    format!("{}...", t)
                } else {
                    t
                }
            });
        let now = Self::now_iso();
        let id = Self::gen_id();

        self.store.entries.push(KnowledgeEntry {
            id: id.clone(),
            title,
            content,
            variants: HashMap::new(),
            category,
            scope: args["scope"].as_str().unwrap_or("project").to_string(),
            project: args["project"].as_str().map(String::from),
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval: Approval::Imported,
            supersedes: None,
            expires_at: None,
            source: "imported".to_string(),
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        });

        self.save()?;
        Ok(format!("Imported entry {}", id))
    }

    /// Remember — store for on-demand recall only, never rendered into markdown.
    pub fn remember(&mut self, args: &Value, from_agent: bool) -> Result<String> {
        let content = args["content"]
            .as_str()
            .context("'content' is required")?
            .to_string();
        let category_str = args["category"].as_str().unwrap_or("memory");
        let category = Category::from_str(category_str).unwrap_or(Category::Memory);
        let title = args["title"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| {
                let t = content.chars().take(60).collect::<String>();
                if content.len() > 60 { format!("{}...", t) } else { t }
            });
        let scope = args["scope"].as_str().unwrap_or("global").to_string();
        let project = args["project"].as_str().map(String::from);
        let decay = args["decay"].as_bool().unwrap_or(true);
        let review_at = args["review_at"].as_str().map(String::from);

        let now = Self::now_iso();
        let id = Self::gen_id();

        self.store.entries.push(KnowledgeEntry {
            id: id.clone(),
            title,
            content,
            variants: HashMap::new(),
            category,
            scope,
            project,
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            render: false,
            decay,
            review_at,
            status: Status::Active,
            approval: if from_agent { Approval::AgentInferred } else { Approval::UserConfirmed },
            supersedes: None,
            expires_at: args["expires_at"].as_str().map(String::from),
            source: if from_agent { "agent".to_string() } else { "user".to_string() },
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        });

        self.save()?;
        Ok(format!("Remembered entry {} (indexed only, not rendered)", id))
    }

    pub fn forget(&mut self, args: &Value) -> Result<String> {
        let id = args["id"].as_str().context("'id' is required")?;
        let superseded_by = args["superseded_by"].as_str();

        if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
            if let Some(by) = superseded_by {
                entry.status = Status::Superseded;
                entry.supersedes = Some(by.to_string());
            } else {
                entry.status = Status::Deleted;
            }
            entry.updated_at = Self::now_iso();
            self.save()?;
            Ok(format!("Removed entry {}", id))
        } else {
            Ok(format!("Entry {} not found", id))
        }
    }

    pub fn list(&mut self, args: &Value) -> Result<String> {
        let category_filter = args["category"].as_str();
        let scope_filter = args["scope"].as_str();
        let project_filter = args["project"].as_str();
        let provider_filter = args["provider"].as_str();
        let status_filter = args["status"].as_str().unwrap_or("active");
        let approval_filter = args["approval"].as_str();
        let query = args["query"].as_str();
        let limit = args["limit"].as_u64().unwrap_or(50) as usize;

        let mut results: Vec<&KnowledgeEntry> = self
            .store
            .entries
            .iter()
            .filter(|e| {
                // Status filter
                let status_ok = match status_filter {
                    "active" => e.status == Status::Active && !Self::is_expired(e),
                    "all" => true,
                    "draft" => e.status == Status::Draft,
                    "superseded" => e.status == Status::Superseded,
                    "disabled" => e.status == Status::Disabled,
                    "deleted" => e.status == Status::Deleted,
                    _ => e.status == Status::Active,
                };
                if !status_ok {
                    return false;
                }

                if let Some(cat) = category_filter {
                    if let Some(c) = Category::from_str(cat) {
                        if e.category != c {
                            return false;
                        }
                    }
                }
                if let Some(s) = scope_filter {
                    if e.scope != s {
                        return false;
                    }
                }
                if let Some(p) = project_filter {
                    match &e.project {
                        Some(ep) => {
                            if !ep.contains(p) {
                                return false;
                            }
                        }
                        None => return false,
                    }
                }
                if let Some(prov) = provider_filter {
                    if !e.providers.is_empty() && !e.providers.iter().any(|p| p == prov) {
                        return false;
                    }
                }
                if let Some(ap) = approval_filter {
                    let matches = match ap {
                        "user_confirmed" => e.approval == Approval::UserConfirmed,
                        "agent_inferred" => e.approval == Approval::AgentInferred,
                        "imported" => e.approval == Approval::Imported,
                        _ => true,
                    };
                    if !matches {
                        return false;
                    }
                }
                if let Some(q) = query {
                    let q_lower = q.to_lowercase();
                    if !e.content.to_lowercase().contains(&q_lower)
                        && !e.title.to_lowercase().contains(&q_lower)
                    {
                        return false;
                    }
                }
                true
            })
            .collect();

        results.sort_by(|a, b| a.weight.cmp(&b.weight));
        results.truncate(limit);

        if results.is_empty() {
            return Ok("No entries found.".to_string());
        }

        let returned_ids: Vec<String> = results.iter().map(|e| e.id.clone()).collect();

        let lines: Vec<String> = results
            .iter()
            .map(|e| {
                let prov = if e.providers.is_empty() {
                    "all".to_string()
                } else {
                    e.providers.join(",")
                };
                let approval_mark = match e.approval {
                    Approval::UserConfirmed => "",
                    Approval::AgentInferred => " [unverified]",
                    Approval::Imported => " [imported]",
                };
                let render_mark = if !e.render { " [indexed-only]" } else { "" };
                let decay_mark = if !e.decay { " [invariant]" } else { "" };
                format!(
                    "[{}] {:?}/{} | {} | {}{}{}{}\n  {}",
                    e.id,
                    e.category,
                    e.scope,
                    prov,
                    e.title,
                    approval_mark,
                    render_mark,
                    decay_mark,
                    if e.content.len() > 120 {
                        format!("{}...", &e.content[..120])
                    } else {
                        e.content.clone()
                    }
                )
            })
            .collect();

        let output = format!("{} entries:\n\n{}", results.len(), lines.join("\n\n"));
        drop(results); // release immutable borrow

        // Update recall stats (best-effort, don't fail the query)
        let now = Self::now_iso();
        for entry in &mut self.store.entries {
            if returned_ids.contains(&entry.id) {
                entry.recall_count += 1;
                entry.last_recalled = Some(now.clone());
            }
        }
        let _ = self.save();

        Ok(output)
    }

    // ── Render ─────────────────────────────────────────────────────

    pub fn render(&self, args: &Value) -> Result<String> {
        let provider = args["provider"].as_str();
        let project_dir = args["project"].as_str();
        let dry_run = args["dry_run"].as_bool().unwrap_or(false);

        let providers: Vec<&str> = if let Some(p) = provider {
            vec![p]
        } else {
            vec!["claude", "agents", "gemini"]
        };

        let mut results = Vec::new();
        for prov in &providers {
            let md = self.render_for_provider(prov, project_dir)?;
            let target = target_file(prov, project_dir);
            if dry_run {
                results.push(format!("=== {} ({}) ===\n{}", prov, target, md));
            } else if let Some(ref dir) = project_dir {
                let path = Path::new(dir).join(&target);
                atomic_write(&path, &md)?;
                results.push(format!("Wrote {} ({} chars)", path.display(), md.len()));
            } else {
                results.push(format!(
                    "=== {} ===\nNo project_dir specified, dry_run only.\n{}",
                    prov, md
                ));
            }
        }

        Ok(results.join("\n\n"))
    }

    fn render_for_provider(&self, provider: &str, project_dir: Option<&str>) -> Result<String> {
        let mut md = String::new();

        // Header
        md.push_str("<!-- Generated by blackbox. Do not edit directly. -->\n");
        md.push_str("<!-- Use blackbox_learn / blackbox_forget to modify. -->\n\n");

        // ── Layer 1: Provider Steerage ──
        let steerage_heading = match provider {
            "claude" => "## Standing Orders",
            "gemini" => "## Foundational Mandates",
            _ => "## Critical Instructions",
        };

        let steerage: Vec<&KnowledgeEntry> = self
            .renderable_entries()
            .filter(|e| e.category == Category::Steering)
            .filter(|e| entry_visible_to(e, provider))
            .filter(|e| entry_in_scope(e, project_dir))
            .collect();

        if !steerage.is_empty() {
            md.push_str(steerage_heading);
            md.push('\n');
            md.push('\n');
            render_entries(&steerage, provider, &mut md);
            md.push('\n');
        }

        // ── Gemini: steerage → PROJECT.md → memory ──
        // Gemini deprioritizes content at the bottom, so PROJECT.md goes
        // between steerage and memory instead of after both.
        if provider == "gemini" {
            self.render_project_md(project_dir, &mut md);
            self.render_shared_memory(provider, project_dir, &mut md);
        } else {
            // ── Everyone else: steerage → memory → PROJECT.md ──
            self.render_shared_memory(provider, project_dir, &mut md);
            self.render_project_md(project_dir, &mut md);
        }

        Ok(md)
    }

    fn render_shared_memory(&self, provider: &str, project_dir: Option<&str>, md: &mut String) {
        let memory_categories = [
            Category::Profile,
            Category::Convention,
            Category::Build,
            Category::Tool,
            Category::Memory,
            Category::Workflow,
        ];

        let mut by_category: HashMap<&str, Vec<&KnowledgeEntry>> = HashMap::new();
        for entry in self.renderable_entries() {
            if entry.category == Category::Steering {
                continue;
            }
            if !entry_in_scope(entry, project_dir) {
                continue;
            }
            let heading = entry.category.heading();
            by_category.entry(heading).or_default().push(entry);
        }

        for cat in &memory_categories {
            let heading = cat.heading();
            if let Some(entries) = by_category.get(heading) {
                let mut sorted = entries.clone();
                sorted.sort_by_key(|e| e.weight);
                md.push_str(&format!("## {}\n\n", heading));
                render_entries(&sorted, provider, md);
                md.push('\n');
            }
        }
    }

    fn render_project_md(&self, project_dir: Option<&str>, md: &mut String) {
        if let Some(dir) = project_dir {
            let project_md = Path::new(dir).join("PROJECT.md");
            if project_md.exists() {
                let content = fs::read_to_string(&project_md).unwrap_or_default();
                if !content.is_empty() {
                    md.push_str("## Project Details\n\n");
                    md.push_str(&content);
                    md.push('\n');
                }
            }
        }
    }

    // ── Absorb ─────────────────────────────────────────────────────

    pub fn absorb(&mut self, args: &Value) -> Result<String> {
        let project_dir = args["project"]
            .as_str()
            .context("'project' is required for absorption")?;

        let files = vec![
            ("CLAUDE.md", "claude"),
            ("AGENTS.md", "agents"),
            ("GEMINI.md", "gemini"),
        ];

        let mut absorbed = 0u32;
        let mut disabled = 0u32;

        // Collect all entry IDs found across ALL rendered files
        let mut all_found_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for (filename, _provider) in &files {
            let path = Path::new(project_dir).join(filename);
            if !path.exists() {
                continue;
            }

            let content = fs::read_to_string(&path)?;

            for line in content.lines() {
                if let Some(id) = extract_marker_id(line) {
                    all_found_ids.insert(id);
                }
            }

            // Find unmarked content blocks (external additions)
            let unmarked = extract_unmarked_sections(&content);
            for section in &unmarked {
                if section.trim().is_empty() {
                    continue;
                }
                // Skip the generated header
                if section.contains("Generated by blackbox") {
                    continue;
                }
                // Skip PROJECT.md content (it's included verbatim, not an entry)
                if section.contains("## Project Details") {
                    continue;
                }

                let entry_args = serde_json::json!({
                    "content": section.trim(),
                    "category": "memory",
                    "scope": "project",
                    "project": project_dir,
                });
                self.learn_internal(&entry_args)?;
                absorbed += 1;
            }
        }

        // Disable entries that are missing from ALL files they should appear in
        // (only after scanning all files to avoid the per-file disability race)
        for entry in &mut self.store.entries {
            if entry.status != Status::Active {
                continue;
            }
            if !entry_in_scope(entry, Some(project_dir)) {
                continue;
            }
            // Never disable render=false entries — they are intentionally never in markdown
            if !entry.render {
                continue;
            }
            if !all_found_ids.contains(&entry.id) {
                entry.status = Status::Disabled;
                entry.updated_at = Self::now_iso();
                disabled += 1;
            }
        }

        self.save()?;
        Ok(format!(
            "Absorbed {} new entries, disabled {} removed entries",
            absorbed, disabled
        ))
    }

    // ── Lint ───────────────────────────────────────────────────────

    pub fn lint(&self) -> Result<String> {
        let mut issues = Vec::new();

        let mut unverified = 0u32;
        let mut expired = 0u32;
        let mut disabled = 0u32;

        for entry in &self.store.entries {
            if entry.approval == Approval::AgentInferred || entry.approval == Approval::Imported {
                if entry.status == Status::Active {
                    unverified += 1;
                }
            }
            if Self::is_expired(entry) && entry.status == Status::Active {
                expired += 1;
                issues.push(format!("[{}] expired: {}", entry.id, entry.title));
            }
            if entry.status == Status::Disabled {
                disabled += 1;
            }
        }

        if unverified > 0 {
            issues.push(format!("{} unverified entries (use blackbox_review)", unverified));
        }
        if expired > 0 {
            issues.push(format!("{} expired entries", expired));
        }
        if disabled > 0 {
            issues.push(format!("{} disabled entries", disabled));
        }

        // Check for entries past review_at
        let now = Self::now_iso();
        let mut needs_review = 0u32;
        for entry in self.active_entries() {
            if let Some(ref review) = entry.review_at {
                if review.as_str() < now.as_str() && entry.decay {
                    needs_review += 1;
                    issues.push(format!("[{}] past review date: {}", entry.id, entry.title));
                }
            }
        }
        if needs_review > 0 {
            issues.push(format!("{} entries past review date", needs_review));
        }

        // Check for never-recalled entries (potential dead weight)
        let mut never_recalled = 0u32;
        for entry in self.active_entries() {
            if entry.recall_count == 0 && entry.decay {
                never_recalled += 1;
            }
        }
        if never_recalled > 0 {
            issues.push(format!("{} entries never recalled (may be dead weight)", never_recalled));
        }

        // Check for potential duplicates (same title)
        let mut titles: HashMap<String, Vec<String>> = HashMap::new();
        for entry in self.active_entries() {
            titles
                .entry(entry.title.to_lowercase())
                .or_default()
                .push(entry.id.clone());
        }
        for (title, ids) in &titles {
            if ids.len() > 1 {
                issues.push(format!(
                    "Possible duplicates for '{}': {}",
                    title,
                    ids.join(", ")
                ));
            }
        }

        if issues.is_empty() {
            Ok("No issues found.".to_string())
        } else {
            Ok(format!("{} issues:\n\n{}", issues.len(), issues.join("\n")))
        }
    }

    // ── Review ─────────────────────────────────────────────────────

    pub fn review(&mut self, args: &Value) -> Result<String> {
        let action = args["action"].as_str(); // "list", "approve", "reject"
        let id = args["id"].as_str();

        match action.unwrap_or("list") {
            "list" => {
                let unverified: Vec<&KnowledgeEntry> = self
                    .store
                    .entries
                    .iter()
                    .filter(|e| {
                        e.status == Status::Active
                            && (e.approval == Approval::AgentInferred
                                || e.approval == Approval::Imported)
                    })
                    .collect();

                if unverified.is_empty() {
                    return Ok("No entries pending review.".to_string());
                }

                let lines: Vec<String> = unverified
                    .iter()
                    .map(|e| {
                        format!(
                            "[{}] {:?} | {:?} | {}\n  {}",
                            e.id, e.approval, e.category, e.title, e.content
                        )
                    })
                    .collect();

                Ok(format!(
                    "{} entries pending review:\n\n{}",
                    unverified.len(),
                    lines.join("\n\n")
                ))
            }
            "approve" => {
                let id = id.context("'id' required for approve")?;
                if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                    entry.approval = Approval::UserConfirmed;
                    entry.updated_at = Self::now_iso();
                    self.save()?;
                    Ok(format!("Approved entry {}", id))
                } else {
                    Ok(format!("Entry {} not found", id))
                }
            }
            "reject" => {
                let id = id.context("'id' required for reject")?;
                if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                    entry.status = Status::Deleted;
                    entry.updated_at = Self::now_iso();
                    self.save()?;
                    Ok(format!("Rejected entry {}", id))
                } else {
                    Ok(format!("Entry {} not found", id))
                }
            }
            other => Ok(format!("Unknown action: {}. Use list, approve, or reject.", other)),
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn entry_visible_to(entry: &KnowledgeEntry, provider: &str) -> bool {
    if entry.providers.is_empty() {
        return true; // visible to all
    }
    if provider == "agents" {
        // AGENTS.md serves codex + vibe
        return entry.providers.iter().any(|p| p == "codex" || p == "vibe");
    }
    entry.providers.iter().any(|p| p == provider)
}

fn entry_in_scope(entry: &KnowledgeEntry, project_dir: Option<&str>) -> bool {
    match entry.scope.as_str() {
        "global" => true,
        "project" => {
            if let (Some(ep), Some(pd)) = (&entry.project, project_dir) {
                ep == pd
            } else {
                false
            }
        }
        _ => true,
    }
}

fn render_entries(entries: &[&KnowledgeEntry], provider: &str, out: &mut String) {
    for entry in entries {
        out.push_str(&format!("<!-- bb:entry={} -->\n", entry.id));
        let mark = match entry.approval {
            Approval::AgentInferred => " *(unverified)*",
            Approval::Imported => " *(imported)*",
            _ => "",
        };
        // Always render title if non-empty — not just for unverified entries
        if !entry.title.is_empty() {
            out.push_str(&format!("**{}**{}\n\n", entry.title, mark));
        }
        // Use provider-specific variant if available, else default content
        let content = entry
            .variants
            .get(provider)
            .unwrap_or(&entry.content);
        out.push_str(content);
        out.push_str("\n\n");
        out.push_str(&format!("<!-- /bb:entry={} -->\n", entry.id));
    }
}

fn target_file(provider: &str, _project_dir: Option<&str>) -> String {
    match provider {
        "claude" => "CLAUDE.md".to_string(),
        "agents" | "codex" | "vibe" => "AGENTS.md".to_string(),
        "gemini" => "GEMINI.md".to_string(),
        other => format!("{}.md", other.to_uppercase()),
    }
}

fn extract_marker_id(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("<!-- bb:entry=") && trimmed.ends_with(" -->") {
        let inner = &trimmed[14..trimmed.len() - 4];
        Some(inner.to_string())
    } else {
        None
    }
}

/// Extract sections of content that are NOT wrapped in bb:entry markers.
fn extract_unmarked_sections(content: &str) -> Vec<String> {
    let mut sections = Vec::new();
    let mut current = String::new();
    let mut in_entry = false;

    for line in content.lines() {
        if line.trim().starts_with("<!-- bb:entry=") && !line.trim().starts_with("<!-- /bb:entry=")
        {
            // Starting a marked entry — flush any unmarked content
            if !current.trim().is_empty() {
                sections.push(current.clone());
            }
            current.clear();
            in_entry = true;
        } else if line.trim().starts_with("<!-- /bb:entry=") {
            in_entry = false;
        } else if !in_entry {
            current.push_str(line);
            current.push('\n');
        }
    }

    // Flush remaining
    if !current.trim().is_empty() {
        sections.push(current);
    }

    sections
}

fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("md.tmp");
    let mut file = fs::File::create(&tmp)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Convert days since Unix epoch to (year, month, day).
fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
    // Simplified — handles 2000-2099 correctly
    let mut y = 1970;
    let mut remaining = days as i64;
    loop {
        let days_in_year = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if remaining < days_in_year {
            break;
        }
        remaining -= days_in_year;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let month_days: [i64; 12] = [
        31,
        if leap { 29 } else { 28 },
        31, 30, 31, 30, 31, 31, 30, 31, 30, 31,
    ];
    let mut m = 0;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining < md {
            m = i;
            break;
        }
        remaining -= md;
    }
    (y as u64, (m + 1) as u64, (remaining + 1) as u64)
}

// ── Bootstrap ─────────────────────────────────────────────────────

/// Candidate instruction files to scan during bootstrap, in priority order.
const BOOTSTRAP_CANDIDATES: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    "GEMINI.md",
    ".cursorrules",
    ".cursor/rules/rules.md",
    ".github/copilot-instructions.md",
];

impl Knowledge {
    /// Bootstrap: scan a project for existing instruction files and return their
    /// contents for the agent to decompose into PROJECT.md + knowledge entries.
    pub fn bootstrap(&self, args: &Value) -> Result<String> {
        let project_dir = args["project"]
            .as_str()
            .context("'project' is required — absolute path to the repo root")?;
        let dir = Path::new(project_dir);
        if !dir.exists() {
            anyhow::bail!("project directory does not exist: {}", project_dir);
        }

        let mut out = String::new();

        // ── Check for existing blackbox entries for this project ──
        let existing_count = self
            .store
            .entries
            .iter()
            .filter(|e| {
                e.status == Status::Active
                    && e.scope == "project"
                    && e.project.as_deref() == Some(project_dir)
            })
            .count();

        if existing_count > 0 {
            out.push_str(&format!(
                "⚠ {} active project-scoped entries already exist for this project.\n\
                 Use blackbox_knowledge with project=\"{}\" to review them.\n\
                 Re-bootstrapping will create duplicates unless you blackbox_forget the old entries first.\n\n",
                existing_count, project_dir
            ));
        }

        // ── Check for PROJECT.md ──
        let project_md = dir.join("PROJECT.md");
        if project_md.exists() {
            out.push_str("⚠ PROJECT.md already exists. Bootstrap will not overwrite it.\n\n");
        }

        // ── Scan instruction files ──
        let mut found_files: Vec<(String, String)> = Vec::new();
        for candidate in BOOTSTRAP_CANDIDATES {
            let path = dir.join(candidate);
            if path.exists() {
                match fs::read_to_string(&path) {
                    Ok(content) if !content.trim().is_empty() => {
                        found_files.push((candidate.to_string(), content));
                    }
                    _ => {}
                }
            }
        }

        // Also check .cursor/rules/ for any .md files beyond rules.md
        let cursor_rules_dir = dir.join(".cursor").join("rules");
        if cursor_rules_dir.is_dir() {
            if let Ok(entries) = fs::read_dir(&cursor_rules_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.ends_with(".md") && name != "rules.md" {
                        let rel = format!(".cursor/rules/{}", name);
                        if let Ok(content) = fs::read_to_string(entry.path()) {
                            if !content.trim().is_empty() {
                                found_files.push((rel, content));
                            }
                        }
                    }
                }
            }
        }

        if found_files.is_empty() {
            out.push_str("No instruction files found. Nothing to bootstrap.\n");
            out.push_str("Create PROJECT.md with your project's build commands, architecture, and conventions,\n");
            out.push_str("then use blackbox_learn for cross-project knowledge.\n");
            return Ok(out);
        }

        // ── Check if any files are already blackbox-generated ──
        let mut generated_files: Vec<&str> = Vec::new();
        let mut authored_files: Vec<&str> = Vec::new();
        for (name, content) in &found_files {
            if content.contains("<!-- Generated by blackbox") {
                generated_files.push(name);
            } else {
                authored_files.push(name);
            }
        }

        if !generated_files.is_empty() {
            out.push_str(&format!(
                "Already managed by blackbox: {}\n",
                generated_files.join(", ")
            ));
            if authored_files.is_empty() {
                out.push_str("All instruction files are already blackbox-generated. Nothing to bootstrap.\n");
                return Ok(out);
            }
            out.push_str("Bootstrapping only the hand-authored files.\n\n");
        }

        // ── Emit file contents with classification guidance ──
        out.push_str(&format!(
            "Found {} hand-authored instruction file(s). Decompose each into:\n\n",
            authored_files.len()
        ));
        out.push_str(
            "**PROJECT.md** — project-specific, provider-neutral documentation:\n\
             - Build/test/lint commands\n\
             - Architecture overview, module descriptions\n\
             - Code conventions specific to THIS repo\n\
             - API/schema details, data models\n\
             - Anything a new contributor needs to know about the project itself\n\n",
        );
        out.push_str(
            "**blackbox_learn entries** — cross-project or provider-specific knowledge:\n\
             - User profile, preferences, communication style → category=profile, scope=global\n\
             - Universal conventions (naming, error handling, testing) → category=convention, scope=global\n\
             - Provider-specific behavioral instructions → category=steering, providers=[\"claude\"/etc]\n\
             - Tool configuration/awareness → category=tool\n\
             - Workflow patterns → category=workflow\n\
             - Project-specific conventions that ALSO apply to other repos → category=convention, scope=global\n\
             - Project-specific conventions that ONLY apply here → put in PROJECT.md instead\n\n",
        );
        out.push_str("──────────────────────────────────────\n\n");

        for (name, content) in &found_files {
            if generated_files.contains(&name.as_str()) {
                continue;
            }
            out.push_str(&format!("### {}\n\n```\n{}\n```\n\n", name, content));
        }

        // ── Emit action plan ──
        out.push_str("──────────────────────────────────────\n\n");
        out.push_str("## Action plan\n\n");
        out.push_str("1. Read each file above and classify every section/instruction.\n");
        out.push_str("2. Write PROJECT.md with the project-specific documentation.\n");
        out.push_str(&format!(
            "3. Call blackbox_learn for each cross-project entry (scope=global or scope=project, project=\"{}\").\n",
            project_dir
        ));
        out.push_str("4. Call blackbox_render with project=\"");
        out.push_str(project_dir);
        out.push_str("\" to generate the new CLAUDE.md/AGENTS.md/GEMINI.md.\n");
        out.push_str("5. Verify the rendered output includes everything from the originals.\n");
        out.push_str("6. Delete or git-rm the original hand-authored files that are now generated.\n");

        Ok(out)
    }
}

