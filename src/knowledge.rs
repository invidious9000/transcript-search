use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use rmcp::schemars;
use serde::{Deserialize, Serialize};

// ── MCP parameter structs ─────────────────────────────────────────
//
// Typed inputs for the bbox_* knowledge tools. Keeping them colocated
// with the domain methods that consume them means adding a field is a
// one-file change.

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct LearnParams {
    /// The instruction, fact, or preference
    pub content: String,
    /// Entry category
    pub category: String,
    /// Short title (auto-generated if omitted)
    #[serde(default)] pub title: Option<String>,
    /// global or project (default: global)
    #[serde(default)] pub scope: Option<String>,
    /// Project path for project-scoped entries
    #[serde(default)] pub project: Option<String>,
    /// Provider filter (empty = all)
    #[serde(default)] pub providers: Option<Vec<String>>,
    /// Priority: critical, standard, supplementary
    #[serde(default)] pub priority: Option<String>,
    /// Ordering within priority tier
    #[serde(default)] pub weight: Option<u32>,
    /// ISO 8601 expiry time
    #[serde(default)] pub expires_at: Option<String>,
    /// Update existing entry by ID
    #[serde(default)] pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RememberParams {
    /// The fact, observation, or note
    pub content: String,
    /// Category (default: memory)
    #[serde(default)] pub category: Option<String>,
    /// Short title
    #[serde(default)] pub title: Option<String>,
    /// global or project (default: global)
    #[serde(default)] pub scope: Option<String>,
    /// Project path
    #[serde(default)] pub project: Option<String>,
    /// Set false for invariants (default: true)
    #[serde(default)] pub decay: Option<bool>,
    /// ISO 8601 date to revisit
    #[serde(default)] pub review_at: Option<String>,
    /// ISO 8601 expiry
    #[serde(default)] pub expires_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct KnowledgeListParams {
    #[serde(default)] pub category: Option<String>,
    #[serde(default)] pub scope: Option<String>,
    #[serde(default)] pub project: Option<String>,
    #[serde(default)] pub provider: Option<String>,
    #[serde(default)] pub status: Option<String>,
    #[serde(default)] pub approval: Option<String>,
    #[serde(default)] pub query: Option<String>,
    #[serde(default)] pub limit: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ForgetParams {
    /// Entry ID to remove
    pub id: String,
    /// Mark as superseded instead of deleted
    #[serde(default)] pub superseded_by: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RenderParams {
    /// Render for specific provider or all
    #[serde(default)] pub provider: Option<String>,
    /// Project directory path. Required when scope includes "project".
    #[serde(default)] pub project: Option<String>,
    /// Which scope to render. "global" surgically patches each provider's
    /// global-memory file (~/.claude-shared/CLAUDE.md, ~/.codex/AGENTS.md,
    /// ~/.gemini/GEMINI.md) inside `<!-- bb:managed-* -->` markers and
    /// snapshots the original to ~/.local/state/blackbox/backups/ first.
    /// "project" writes <project>/{CLAUDE,AGENTS,GEMINI}.md from project-
    /// scope entries + PROJECT.md only (no global content). "both" runs
    /// both. Defaults to "both" if `project` is given, else "global".
    #[serde(default)] pub scope: Option<String>,
    /// Preview without writing (default: false)
    #[serde(default)] pub dry_run: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AbsorbParams {
    /// Project directory path
    pub project: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReviewParams {
    /// list, approve, or reject (default: list)
    #[serde(default)] pub action: Option<String>,
    /// Entry ID (required for approve/reject)
    #[serde(default)] pub id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct BootstrapParams {
    /// Absolute path to the repo root
    pub project: String,
}

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
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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

    pub fn learn(&mut self, p: &LearnParams, from_agent: bool) -> Result<String> {
        let category = Category::from_str(&p.category).context("invalid category")?;
        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = p.scope.clone().unwrap_or_else(|| "global".to_string());
        let providers = p.providers.clone().unwrap_or_default();
        let priority = match p.priority.as_deref().unwrap_or("standard") {
            "critical" => Priority::Critical,
            "supplementary" => Priority::Supplementary,
            _ => Priority::Standard,
        };
        let weight = p.weight.unwrap_or(100);

        let now = Self::now_iso();
        let approval = if from_agent {
            Approval::AgentInferred
        } else {
            Approval::UserConfirmed
        };

        // Update existing entry if id given and found
        if let Some(id) = p.id.as_deref() {
            if let Some(entry) = self.store.entries.iter_mut().find(|e| e.id == id) {
                entry.content = p.content.clone();
                entry.title = title;
                entry.category = category;
                entry.priority = priority;
                entry.weight = weight;
                entry.providers = providers;
                entry.updated_at = now;
                if let Some(exp) = p.expires_at.clone() {
                    entry.expires_at = Some(exp);
                }
                if let Some(s) = p.scope.clone() {
                    entry.scope = s;
                }
                if let Some(proj) = p.project.clone() {
                    entry.project = Some(proj);
                }
                self.save()?;
                return Ok(format!("Updated entry {id}"));
            }
        }

        let id = Self::gen_id();
        let entry = KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            variants: HashMap::new(),
            category,
            scope,
            project: p.project.clone(),
            providers,
            priority,
            weight,
            render: true,
            decay: true,
            review_at: None,
            status: Status::Active,
            approval,
            supersedes: None,
            expires_at: p.expires_at.clone(),
            source: if from_agent { "agent".to_string() } else { "user".to_string() },
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        };

        self.store.entries.push(entry);
        self.save()?;
        Ok(format!("Created entry {id}"))
    }

    /// Import a knowledge entry from external content. Used by absorb().
    /// Always creates an Imported-approval entry; never updates in place.
    fn import_entry(
        &mut self,
        content: String,
        category: Category,
        scope: String,
        project: Option<String>,
    ) -> Result<String> {
        let title = derive_title(&content);
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
        Ok(format!("Imported entry {id}"))
    }

    /// Remember — store for on-demand recall only, never rendered into markdown.
    pub fn remember(&mut self, p: &RememberParams, from_agent: bool) -> Result<String> {
        let category_str = p.category.as_deref().unwrap_or("memory");
        let category = Category::from_str(category_str).unwrap_or(Category::Memory);
        let title = p.title.clone().unwrap_or_else(|| derive_title(&p.content));
        let scope = p.scope.clone().unwrap_or_else(|| "global".to_string());

        let now = Self::now_iso();
        let id = Self::gen_id();

        self.store.entries.push(KnowledgeEntry {
            id: id.clone(),
            title,
            content: p.content.clone(),
            variants: HashMap::new(),
            category,
            scope,
            project: p.project.clone(),
            providers: Vec::new(),
            priority: Priority::Standard,
            weight: 100,
            render: false,
            decay: p.decay.unwrap_or(true),
            review_at: p.review_at.clone(),
            status: Status::Active,
            approval: if from_agent { Approval::AgentInferred } else { Approval::UserConfirmed },
            supersedes: None,
            expires_at: p.expires_at.clone(),
            source: if from_agent { "agent".to_string() } else { "user".to_string() },
            created_at: now.clone(),
            updated_at: now,
            recall_count: 0,
            last_recalled: None,
        });

        self.save()?;
        Ok(format!("Remembered entry {id} (indexed only, not rendered)"))
    }

    pub fn forget(&mut self, p: &ForgetParams) -> Result<String> {
        let id = &p.id;

        if let Some(entry) = self.store.entries.iter_mut().find(|e| &e.id == id) {
            if let Some(by) = p.superseded_by.as_deref() {
                entry.status = Status::Superseded;
                entry.supersedes = Some(by.to_string());
            } else {
                entry.status = Status::Deleted;
            }
            entry.updated_at = Self::now_iso();
            self.save()?;
            Ok(format!("Removed entry {id}"))
        } else {
            Ok(format!("Entry {id} not found"))
        }
    }

    pub fn list(&mut self, p: &KnowledgeListParams) -> Result<String> {
        let category_filter = p.category.as_deref();
        let scope_filter = p.scope.as_deref();
        let project_filter = p.project.as_deref();
        let provider_filter = p.provider.as_deref();
        let status_filter = p.status.as_deref().unwrap_or("active");
        let approval_filter = p.approval.as_deref();
        let query = p.query.as_deref();
        let limit = p.limit.unwrap_or(50) as usize;

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
                        let mut end = 120;
                        while end > 0 && !e.content.is_char_boundary(end) { end -= 1; }
                        format!("{}...", &e.content[..end])
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

    pub fn render(&self, p: &RenderParams) -> Result<String> {
        let provider = p.provider.as_deref();
        let project_dir = p.project.as_deref();
        let dry_run = p.dry_run.unwrap_or(false);
        let scope_arg = p.scope.as_deref()
            .unwrap_or(if project_dir.is_some() { "both" } else { "global" });

        let do_global = matches!(scope_arg, "global" | "both");
        let do_project = matches!(scope_arg, "project" | "both") && project_dir.is_some();

        if !do_global && !do_project {
            anyhow::bail!(
                "nothing to render: scope={} project_dir={}",
                scope_arg,
                project_dir.unwrap_or("<none>")
            );
        }

        let providers: Vec<&str> = if let Some(p) = provider {
            vec![p]
        } else {
            vec!["claude", "agents", "gemini"]
        };

        let mut results = Vec::new();

        // ── Global render: surgical patch into provider global-memory files ──
        if do_global {
            for prov in &providers {
                let Some(target_res) = crate::render::global_target_path(prov) else {
                    results.push(format!(
                        "Skipped {} global (no documented global-memory file)",
                        prov
                    ));
                    continue;
                };
                let target = target_res?;
                let body = self.render_global_body(prov)?;
                let plan = crate::render::plan_managed_patch(&target, &body)?;

                if dry_run {
                    use crate::render::PatchPlan;
                    let (before_label, after_label) = match &plan {
                        PatchPlan::Create { .. } => (
                            "--- no existing file ---",
                            "--- proposed managed region ---",
                        ),
                        PatchPlan::Append { .. } => (
                            "--- existing file (will be preserved, managed region appended) ---",
                            "--- managed region to append ---",
                        ),
                        PatchPlan::Replace { .. } => (
                            "--- existing managed region (will be replaced) ---",
                            "--- proposed managed region ---",
                        ),
                        PatchPlan::Unchanged { .. } => (
                            "--- existing managed region (identical, no change) ---",
                            "--- no change ---",
                        ),
                    };
                    results.push(format!(
                        "[DRY-RUN] {}\n{}\n{}\n{}\n{}",
                        plan.summary(),
                        before_label,
                        plan.before_text().unwrap_or("<none>"),
                        after_label,
                        plan.managed_block().unwrap_or("<no change>"),
                    ));
                } else {
                    let backup = crate::render::apply_managed_patch(&plan)?;
                    let backup_str = backup
                        .map(|p| format!(" (backup: {})", p.display()))
                        .unwrap_or_default();
                    results.push(format!("{}{}", plan.summary(), backup_str));
                }
            }
        }

        // ── Project render: project-scope entries + PROJECT.md only ──
        if do_project {
            let dir = project_dir.unwrap();
            for prov in &providers {
                let body = self.render_project_body(prov, dir)?;
                let path = Path::new(dir).join(target_file(prov, project_dir));

                if body.trim().is_empty() {
                    results.push(format!(
                        "Skipped {} (no project-scope entries and no PROJECT.md content)",
                        path.display()
                    ));
                    continue;
                }

                let mut full = String::new();
                full.push_str("<!-- Generated by blackbox. Do not edit directly. -->\n");
                full.push_str("<!-- Use bbox_learn / bbox_forget to modify. -->\n\n");
                full.push_str(&body);

                if dry_run {
                    results.push(format!(
                        "[DRY-RUN] PROJECT {} ({} chars)\n{}",
                        path.display(),
                        full.len(),
                        full
                    ));
                } else {
                    atomic_write(&path, &full)?;
                    results.push(format!(
                        "Wrote project {} ({} chars)",
                        path.display(),
                        full.len()
                    ));
                }
            }
        }

        Ok(results.join("\n\n"))
    }

    /// Body for a global-memory file: built-in preamble + global steerage +
    /// global shared memory. No PROJECT.md (that's project-scope).
    fn render_global_body(&self, provider: &str) -> Result<String> {
        let mut md = String::new();
        md.push_str(&crate::render::builtin_preamble(provider));
        md.push_str("\n\n");
        self.render_steerage(provider, ScopeFilter::Global, &mut md);
        self.render_memory(provider, ScopeFilter::Global, &mut md);
        Ok(md)
    }

    /// Body for a project file: project-scope steerage + project-scope memory
    /// + PROJECT.md. No global content (that lives in the global render).
    fn render_project_body(&self, provider: &str, project_dir: &str) -> Result<String> {
        let mut body = String::new();
        let filter = ScopeFilter::Project(project_dir);

        self.render_steerage(provider, filter, &mut body);

        // Gemini deprioritizes content at the bottom, so PROJECT.md goes
        // between steerage and memory instead of after both.
        if provider == "gemini" {
            self.render_project_md(Some(project_dir), &mut body);
            self.render_memory(provider, filter, &mut body);
        } else {
            self.render_memory(provider, filter, &mut body);
            self.render_project_md(Some(project_dir), &mut body);
        }

        Ok(body)
    }

    fn render_steerage(&self, provider: &str, filter: ScopeFilter, md: &mut String) {
        let heading = match provider {
            "claude" => "## Standing Orders",
            "gemini" => "## Foundational Mandates",
            _ => "## Critical Instructions",
        };

        let steerage: Vec<&KnowledgeEntry> = self
            .renderable_entries()
            .filter(|e| e.category == Category::Steering)
            .filter(|e| entry_visible_to(e, provider))
            .filter(|e| filter.matches(e))
            .collect();

        if !steerage.is_empty() {
            md.push_str(heading);
            md.push('\n');
            md.push('\n');
            render_entries(&steerage, provider, md);
            md.push('\n');
        }
    }

    fn render_memory(&self, provider: &str, filter: ScopeFilter, md: &mut String) {
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
            if !entry_visible_to(entry, provider) {
                continue;
            }
            if !filter.matches(entry) {
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
                    // Delimit PROJECT.md content with an HTML comment so absorb()
                    // can recognize and skip it. No visible wrapper heading —
                    // PROJECT.md's own top-level heading stands.
                    md.push_str("<!-- bb:project-md -->\n");
                    md.push_str(&content);
                    if !content.ends_with('\n') {
                        md.push('\n');
                    }
                }
            }
        }
    }

    // ── Absorb ─────────────────────────────────────────────────────

    pub fn absorb(&mut self, p: &AbsorbParams) -> Result<String> {
        let project_dir = p.project.as_str();

        let files = vec![
            ("CLAUDE.md", "claude"),
            ("AGENTS.md", "agents"),
            ("GEMINI.md", "gemini"),
        ];

        let mut absorbed = 0u32;
        let mut disabled = 0u32;

        // Track which provider files were actually scanned
        let mut scanned_providers: Vec<String> = Vec::new();

        // Collect all entry IDs found across ALL rendered files
        let mut all_found_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for (filename, provider) in &files {
            let path = Path::new(project_dir).join(filename);
            if !path.exists() {
                continue;
            }
            scanned_providers.push(provider.to_string());

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
                // Skip PROJECT.md content (it's included verbatim, not an entry).
                // Recognize both the new bb:project-md marker and the legacy
                // "## Project Details" wrapper that was in use pre-2026-04.
                if section.contains("<!-- bb:project-md -->")
                    || section.contains("## Project Details")
                {
                    continue;
                }
                // Skip render-emitted category headings (e.g. "## Standing Orders",
                // "## Workflow") that sit between marker-wrapped entries. These
                // are structural, not content — absorbing them creates junk
                // entries like "## Tools [imported]" with no body.
                if is_structural_only(section) {
                    continue;
                }

                self.import_entry(
                    section.trim().to_string(),
                    Category::Memory,
                    "project".to_string(),
                    Some(project_dir.to_string()),
                )?;
                absorbed += 1;
            }
        }

        // Disable entries missing from scanned files. Project-file absorption
        // only touches project-scope entries — global entries live in the
        // provider's global-memory file and are absorbed separately.
        for entry in &mut self.store.entries {
            if entry.status != Status::Active {
                continue;
            }
            if entry.scope != "project" || entry.project.as_deref() != Some(project_dir) {
                continue;
            }
            if !entry.render {
                continue;
            }
            let visible_to_scanned = scanned_providers.iter()
                .any(|p| entry_visible_to(entry, p));
            if !visible_to_scanned {
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

    pub fn review(&mut self, p: &ReviewParams) -> Result<String> {
        let action = p.action.as_deref().unwrap_or("list");
        let id = p.id.as_deref();

        match action {
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

/// Derive a short display title from entry content: first ~60 chars,
/// truncated at a UTF-8 boundary, with an ellipsis when we had to cut.
fn derive_title(content: &str) -> String {
    let t: String = content.chars().take(60).collect();
    if content.len() > t.len() {
        format!("{t}...")
    } else {
        t
    }
}

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

#[derive(Clone, Copy)]
enum ScopeFilter<'a> {
    Global,
    Project(&'a str),
}

impl<'a> ScopeFilter<'a> {
    fn matches(&self, entry: &KnowledgeEntry) -> bool {
        match (self, entry.scope.as_str()) {
            (ScopeFilter::Global, "global") => true,
            (ScopeFilter::Project(dir), "project") => entry.project.as_deref() == Some(*dir),
            _ => false,
        }
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

/// True if a candidate section contains only markdown headings and
/// whitespace — i.e. render-emitted category separators like
/// "## Standing Orders" sitting between marker-wrapped entries. These
/// are structure, not absorbable content.
fn is_structural_only(section: &str) -> bool {
    let mut saw_heading = false;
    for line in section.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            saw_heading = true;
            continue;
        }
        return false;
    }
    saw_heading
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
    pub fn bootstrap(&self, p: &BootstrapParams) -> Result<String> {
        let project_dir = p.project.as_str();
        let dir = Path::new(project_dir);
        if !dir.exists() {
            anyhow::bail!("project directory does not exist: {project_dir}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structural_only_skips_bare_category_headings() {
        assert!(is_structural_only("## Standing Orders\n"));
        assert!(is_structural_only("\n## Conventions\n\n"));
        assert!(is_structural_only("## A\n## B\n"));
    }

    #[test]
    fn structural_only_rejects_heading_with_body() {
        assert!(!is_structural_only("## New thing\n\nuser-written body text\n"));
    }

    #[test]
    fn structural_only_rejects_empty() {
        // A wholly-blank section isn't structural content either — it's
        // filtered separately by the empty-trim check in absorb(). Be
        // explicit about the contract: we require at least one heading.
        assert!(!is_structural_only(""));
        assert!(!is_structural_only("\n\n"));
    }

    #[test]
    fn extract_unmarked_returns_category_headings_between_entries() {
        // Regression: absorb used to ingest these as junk "## Tools" etc.
        // entries. The fix is is_structural_only skipping them; this test
        // just pins the shape extract_unmarked_sections produces so future
        // refactors don't silently change it.
        let content = "\
## Standing Orders

<!-- bb:entry=abc -->
body
<!-- /bb:entry=abc -->

## Conventions

<!-- bb:entry=def -->
body
<!-- /bb:entry=def -->
";
        let sections = extract_unmarked_sections(content);
        assert!(
            sections.iter().any(|s| s.contains("## Standing Orders")),
            "expected Standing Orders heading to surface as unmarked"
        );
        assert!(
            sections.iter().any(|s| s.contains("## Conventions")),
            "expected Conventions heading to surface as unmarked"
        );
        for s in &sections {
            assert!(
                is_structural_only(s),
                "all extracted sections should be structural-only in this shape: {s:?}"
            );
        }
    }
}

