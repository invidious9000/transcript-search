//! MCP server registry and filter layer.
//!
//! Users and the daemon coordinate a single view of which MCP servers
//! dispatched bros should see, and which tool calls are allowed or
//! disallowed. The registry lives at `~/.bro/mcp.json` with an optional
//! project overlay at `<project>/.bro/mcp.json`.
//!
//! At dispatch time, the effective set is (global entries) merged with
//! (project entries override), and translated into provider-specific CLI
//! args — each provider has native `mcp add/list/remove` and tool-filter
//! flags (see `providers.rs::build_mcp_*_args`). Gemini and the persistent-
//! only providers are handled through their own CLI at registration time;
//! transient-injection providers (Claude, Copilot, Codex) receive the
//! effective set per-invocation as well for determinism.
//!
//! The recursion guard is now mechanical: the default filter set includes
//! a disallow pattern matching `mcp__blackbox__bro_*` so dispatched agents
//! literally cannot see (let alone call) the orchestration tools.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use rmcp::schemars;

use super::providers::{MatchState, Provider};

// ── Types ──────────────────────────────────────────────────────────

/// Transport-discriminated MCP server config. Matches the shape every
/// provider CLI accepts, modulo translation at registration time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_tools: Vec<String>,
    },
    Sse {
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        exclude_tools: Vec<String>,
    },
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env: BTreeMap<String, String>,
    },
}

impl McpServerConfig {
    /// Per-server exclude list (Gemini-only at present, applied at
    /// registration time). Empty for Stdio (no add fan-out).
    pub fn exclude_tools(&self) -> &[String] {
        match self {
            Self::Http { exclude_tools, .. } | Self::Sse { exclude_tools, .. } => exclude_tools,
            Self::Stdio { .. } => &[],
        }
    }
}

impl McpServerConfig {
    /// True if this is the blackbox self-entry (used by self-registration
    /// to detect URL drift and decide add vs remove+add).
    pub fn blackbox_matches(&self, current_url: &str) -> bool {
        matches!(self, Self::Http { url, .. } if url == current_url)
    }
}

/// Filter rules — mirrors what each provider's `--disallowedTools` /
/// `--deny-tool` / `--exclude-tools` flag accepts, in a canonical form
/// translated at dispatch time.
///
/// Patterns support simple glob: `*` matches any suffix, e.g.
/// `mcp__blackbox__bro_*` matches every bro_* orchestration tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct McpFilters {
    /// Disallow rules — tools matching these patterns are filtered out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disallow: Vec<String>,

    /// Allow rules — if non-empty, ONLY matching tools pass. Applied
    /// AFTER disallow (disallow always wins). Most dispatches leave this
    /// empty and rely on disallow alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

impl McpFilters {
    pub fn is_empty(&self) -> bool {
        self.disallow.is_empty() && self.allow.is_empty()
    }

    /// Merge another filter set into this one. `other` disallow/allow
    /// entries are appended; duplicates are deduped.
    pub fn merge_from(&mut self, other: &McpFilters) {
        for p in &other.disallow {
            if !self.disallow.iter().any(|q| q == p) {
                self.disallow.push(p.clone());
            }
        }
        for p in &other.allow {
            if !self.allow.iter().any(|q| q == p) {
                self.allow.push(p.clone());
            }
        }
    }

    /// Default filter set: the mechanical recursion guard. Blocks every
    /// `bro_*` orchestration tool so dispatched agents can't spawn sub-
    /// bros. Callers that pass allow_recursion=true skip this layer.
    pub fn default_recursion_guard() -> Self {
        Self {
            disallow: vec!["mcp__blackbox__bro_*".to_string()],
            allow: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpStore {
    pub version: u32,
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
    #[serde(default)]
    pub filters: McpFilters,
}

impl McpStore {
    pub fn new() -> Self {
        Self {
            version: 1,
            servers: BTreeMap::new(),
            filters: McpFilters::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let raw = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        file.write_all(raw.as_bytes())?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

// ── Path helpers ───────────────────────────────────────────────────

pub fn global_store_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".bro").join("mcp.json"))
}

pub fn project_store_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".bro").join("mcp.json")
}

// ── Overlay resolution ─────────────────────────────────────────────

/// Effective view after applying project overlay on top of global.
#[derive(Debug, Clone)]
pub struct EffectiveMcp {
    pub servers: BTreeMap<String, McpServerConfig>,
    pub filters: McpFilters,
}

/// Resolve the effective MCP set by merging global + project overlay.
/// Project entries fully replace same-named global entries. Filter
/// lists are concatenated (project additions layered on top of global).
pub fn resolve_effective(
    global: &McpStore,
    project: Option<&McpStore>,
    include_default_guard: bool,
) -> EffectiveMcp {
    let mut servers = global.servers.clone();
    let mut filters = global.filters.clone();

    if let Some(p) = project {
        for (name, cfg) in &p.servers {
            servers.insert(name.clone(), cfg.clone());
        }
        filters.merge_from(&p.filters);
    }

    if include_default_guard {
        filters.merge_from(&McpFilters::default_recursion_guard());
    }

    EffectiveMcp { servers, filters }
}

// ── Pattern matching ───────────────────────────────────────────────

/// Expand a glob-style pattern (e.g. `mcp__blackbox__bro_*`, `*_exec`,
/// `bro_?xec`) against a known tool universe. Used by providers that
/// accept exact tool names (Gemini, Codex) rather than patterns.
///
/// Supports `*` (any sequence) and `?` (single char) anywhere in the
/// pattern. Character classes (`[abc]`) are not supported — they fall
/// back to literal match against the bracketed string.
pub fn expand_pattern(pattern: &str, universe: &[&str]) -> Vec<String> {
    universe
        .iter()
        .filter(|t| glob_match(pattern, t))
        .map(|t| t.to_string())
        .collect()
}

/// Simple recursive glob matcher: `*` = any sequence (incl. empty),
/// `?` = exactly one char, everything else literal. No character
/// classes or escapes — adequate for tool-name patterns we ship.
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_match_inner(&p, 0, &t, 0)
}

fn glob_match_inner(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => (ti..=t.len()).any(|k| glob_match_inner(p, pi + 1, t, k)),
        '?' => ti < t.len() && glob_match_inner(p, pi + 1, t, ti + 1),
        c => ti < t.len() && t[ti] == c && glob_match_inner(p, pi + 1, t, ti + 1),
    }
}

// ── Self-registration ──────────────────────────────────────────────

/// Per-provider outcome for `self_register_blackbox`.
#[derive(Debug, Clone)]
pub enum SelfRegisterOutcome {
    /// Provider registered blackbox with the expected URL — nothing to do.
    Unchanged,
    /// Provider had no blackbox entry; `mcp add` succeeded.
    Added,
    /// Provider had a stale URL; removed then added.
    Updated,
    /// Provider binary isn't on PATH (spawn failed). Distinguished from
    /// ListFailed: the CLI is genuinely absent, not just misbehaving.
    NotInstalled { detail: String },
    /// Provider binary spawned but `mcp list` exited non-zero. CLI is
    /// installed but something is wrong (auth, schema mismatch, etc.).
    /// Worth surfacing differently — the user can fix this; NotInstalled
    /// requires actually installing the CLI.
    ListFailed { detail: String },
    /// Provider has no MCP CRUD (Vibe).
    Unsupported,
    /// The subsequent add/remove CLI call errored out.
    Error { detail: String },
}

#[derive(Debug, Default)]
pub struct SelfRegisterReport {
    pub per_provider: Vec<(Provider, SelfRegisterOutcome)>,
}

impl SelfRegisterReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (p, o) in &self.per_provider {
            let label = match o {
                SelfRegisterOutcome::Unchanged => "unchanged",
                SelfRegisterOutcome::Added => "added",
                SelfRegisterOutcome::Updated => "updated",
                SelfRegisterOutcome::NotInstalled { .. } => "not-installed",
                SelfRegisterOutcome::ListFailed { .. } => "list-failed",
                SelfRegisterOutcome::Unsupported => "unsupported",
                SelfRegisterOutcome::Error { .. } => "error",
            };
            parts.push(format!("{p}={label}"));
        }
        parts.join(", ")
    }
}

/// On daemon startup, ensure every provider with MCP CRUD has a
/// `blackbox` entry pointing at `url`. Idempotent: no-op when the
/// entry is already correct; updates on drift. Per-provider failures
/// are captured and returned in the report — one missing CLI doesn't
/// block the others.
pub fn self_register_blackbox(url: &str) -> SelfRegisterReport {
    let mut report = SelfRegisterReport::default();
    for provider in [
        Provider::Claude,
        Provider::Copilot,
        Provider::Codex,
        Provider::Gemini,
        Provider::Vibe,
    ] {
        let outcome = register_one(provider, url);
        report.per_provider.push((provider, outcome));
    }
    report
}

fn register_one(provider: Provider, url: &str) -> SelfRegisterOutcome {
    let Some(list_args) = provider.build_mcp_list_args() else {
        return SelfRegisterOutcome::Unsupported;
    };

    let raw_bin = provider.bin();
    let bin = super::providers::resolve_bin(&raw_bin).unwrap_or(raw_bin);
    let list_out = match Command::new(&bin).args(&list_args).output() {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            // CLI installed but `mcp list` errored — distinct from
            // not-installed. User can fix this without installing.
            return SelfRegisterOutcome::ListFailed {
                detail: format!(
                    "{bin} mcp list exited {:?}: {}",
                    o.status.code(),
                    String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or(""),
                ),
            };
        }
        Err(e) => {
            // Spawn failed — binary genuinely not on PATH (or unreadable).
            return SelfRegisterOutcome::NotInstalled {
                detail: format!("{bin}: {e}"),
            };
        }
    };

    let stdout = String::from_utf8_lossy(&list_out.stdout).to_string();
    match provider.mcp_list_has(&stdout, "blackbox", Some(url)) {
        MatchState::MatchesName => SelfRegisterOutcome::Unchanged,
        MatchState::Drift => {
            if let Err(e) = run_cli(&provider, &provider.build_mcp_remove_args("blackbox").unwrap_or_default()) {
                return SelfRegisterOutcome::Error { detail: format!("remove: {e}") };
            }
            match run_cli(
                &provider,
                &provider.build_mcp_add_http_args("blackbox", url, &[]).unwrap_or_default(),
            ) {
                Ok(()) => SelfRegisterOutcome::Updated,
                Err(e) => SelfRegisterOutcome::Error { detail: format!("re-add: {e}") },
            }
        }
        MatchState::Missing => match run_cli(
            &provider,
            &provider.build_mcp_add_http_args("blackbox", url, &[]).unwrap_or_default(),
        ) {
            Ok(()) => SelfRegisterOutcome::Added,
            Err(e) => SelfRegisterOutcome::Error { detail: format!("add: {e}") },
        },
    }
}

fn run_cli(provider: &Provider, args: &[String]) -> Result<()> {
    let raw_bin = provider.bin();
    let bin = super::providers::resolve_bin(&raw_bin).unwrap_or(raw_bin);
    let out = Command::new(&bin)
        .args(args)
        .output()
        .with_context(|| format!("spawning {bin}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "{bin} {} exited {:?}: {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or(""),
        );
    }
    Ok(())
}

// ── Gemini policy file generation ──────────────────────────────────

/// Directory where per-dispatch Gemini policy files are written.
/// Daemon-owned — never touches `~/.gemini/policies/` (user territory).
pub fn gemini_policy_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".bro").join("gemini-policies"))
}

/// Translate `McpFilters` into a Gemini-compatible policy TOML string.
///
/// Disallow patterns targeting the blackbox MCP namespace are expanded
/// against the orchestration tool universe (same treatment Codex gets).
/// Non-blackbox patterns are skipped — the Gemini policy engine keys
/// on `mcpName` + `toolName`, so filtering outside an MCP server is
/// out of scope for this translator.
pub fn render_gemini_policy_toml(filters: &McpFilters) -> String {
    let universe: Vec<&str> = crate::tool_docs::orchestration_tool_names();
    let prefix = crate::tool_docs::BLACKBOX_MCP_PREFIX;

    let mut disabled: Vec<String> = Vec::new();
    for p in &filters.disallow {
        let Some(stripped) = p.strip_prefix(prefix) else {
            continue;
        };
        for t in expand_pattern(stripped, &universe) {
            if !disabled.contains(&t) {
                disabled.push(t);
            }
        }
    }

    let mut out = String::from(
        "# Generated per-dispatch by blackboxd. Do not hand-edit — this\n\
         # file is deleted when the dispatched task exits.\n",
    );

    if !disabled.is_empty() {
        let quoted: Vec<String> = disabled.iter().map(|t| format!("\"{t}\"")).collect();
        out.push_str(&format!(
            "\n[[rule]]\n\
             mcpName = \"blackbox\"\n\
             toolName = [{}]\n\
             decision = \"deny\"\n\
             priority = 500\n\
             denyMessage = \"Blocked by blackbox dispatch policy\"\n",
            quoted.join(",")
        ));
    }

    out
}

/// Write a Gemini policy file for a single dispatch. Returns the path
/// to the tempfile, which the caller appends to the CLI invocation via
/// `--policy <path>` and is responsible for cleaning up after the
/// child exits. Returns None if no filters apply (no file needed).
pub fn write_gemini_policy_file(task_id: &str, filters: &McpFilters) -> Result<Option<PathBuf>> {
    if filters.disallow.is_empty() && filters.allow.is_empty() {
        return Ok(None);
    }
    let dir = gemini_policy_dir().context("resolving gemini policy dir")?;
    fs::create_dir_all(&dir)?;
    let content = render_gemini_policy_toml(filters);
    // Skip the file entirely if the translator produced no rules —
    // filters targeted patterns outside blackbox namespace.
    if !content.contains("[[rule]]") {
        return Ok(None);
    }
    let path = dir.join(format!("dispatch-{task_id}.toml"));
    let mut f = fs::File::create(&path)?;
    f.write_all(content.as_bytes())?;
    f.sync_all()?;
    Ok(Some(path))
}

/// Sweep stale Gemini policy files at daemon startup. Anything older
/// than `max_age_hours` is deleted (leftover from crashes or force-
/// kills where the normal cleanup path didn't run).
pub fn sweep_stale_gemini_policies(max_age_hours: u64) -> Result<usize> {
    let Some(dir) = gemini_policy_dir() else {
        return Ok(0);
    };
    if !dir.exists() {
        return Ok(0);
    }
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(max_age_hours * 3600))
        .unwrap_or(std::time::UNIX_EPOCH);
    let mut removed = 0;
    for entry in fs::read_dir(&dir)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if mtime < cutoff && path.extension().map(|e| e == "toml").unwrap_or(false) {
            if fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

// ── MCP tool dispatch ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpAction {
    List,
    Get,
    Add,
    Remove,
    Allow,
    Disallow,
    ClearFilters,
    Sync,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpToolParams {
    pub action: McpAction,
    /// Server name (required for add/remove/get).
    #[serde(default)]
    pub name: Option<String>,
    /// URL for HTTP/SSE servers (required on add).
    #[serde(default)]
    pub url: Option<String>,
    /// Transport: http, sse, stdio. Defaults to http.
    #[serde(default)]
    pub transport: Option<String>,
    /// global or project (default: global).
    #[serde(default)]
    pub scope: Option<String>,
    /// Project path — required when scope=project.
    #[serde(default)]
    pub project: Option<String>,
    /// Filter pattern for allow/disallow (e.g. `mcp__blackbox__bro_*`).
    #[serde(default)]
    pub pattern: Option<String>,
    /// Persistent per-server exclude list (Gemini only; applied at
    /// registration time).
    #[serde(default)]
    pub exclude_tools: Option<Vec<String>>,
    /// Optional HTTP/SSE headers (e.g. auth tokens) to pass at
    /// registration time. Persisted into McpServerConfig and replayed
    /// by `action=sync`.
    #[serde(default)]
    pub headers: Option<BTreeMap<String, String>>,
}

/// Dispatch a bro_mcp tool call. Returns a human-readable result string.
pub fn handle(p: &McpToolParams) -> Result<String> {
    use McpAction::*;
    match p.action {
        List => action_list(p),
        Get => action_get(p),
        Add => action_add(p),
        Remove => action_remove(p),
        Allow => action_filter(p, /* disallow */ false),
        Disallow => action_filter(p, /* disallow */ true),
        ClearFilters => action_clear_filters(p),
        Sync => action_sync(p),
    }
}

fn resolve_scope_path(p: &McpToolParams) -> Result<PathBuf> {
    let scope = p.scope.as_deref().unwrap_or("global");
    match scope {
        "global" => global_store_path().context("resolving home dir"),
        "project" => {
            let pd = p
                .project
                .as_deref()
                .context("'project' is required when scope=project")?;
            Ok(project_store_path(Path::new(pd)))
        }
        other => anyhow::bail!("Unknown scope: {other}. Use: global, project"),
    }
}

fn action_list(p: &McpToolParams) -> Result<String> {
    let global_path = global_store_path().context("home dir")?;
    let global = McpStore::load(&global_path)?;

    let project = p
        .project
        .as_deref()
        .map(|pd| McpStore::load(&project_store_path(Path::new(pd))))
        .transpose()?;

    let eff = resolve_effective(&global, project.as_ref(), false);

    let mut out = String::new();
    if eff.servers.is_empty() {
        out.push_str("No MCP servers registered.\n");
    } else {
        out.push_str(&format!("{} server(s):\n", eff.servers.len()));
        for (name, cfg) in &eff.servers {
            match cfg {
                McpServerConfig::Http { url, .. } => {
                    out.push_str(&format!("  {name} — http {url}\n"));
                }
                McpServerConfig::Sse { url, .. } => {
                    out.push_str(&format!("  {name} — sse {url}\n"));
                }
                McpServerConfig::Stdio { command, args, .. } => {
                    out.push_str(&format!("  {name} — stdio {command} {}\n", args.join(" ")));
                }
            }
        }
    }

    if !eff.filters.disallow.is_empty() {
        out.push_str(&format!("\nDisallow ({}):\n", eff.filters.disallow.len()));
        for p in &eff.filters.disallow {
            out.push_str(&format!("  {p}\n"));
        }
    }
    if !eff.filters.allow.is_empty() {
        out.push_str(&format!("\nAllow ({}):\n", eff.filters.allow.len()));
        for p in &eff.filters.allow {
            out.push_str(&format!("  {p}\n"));
        }
    }

    Ok(out)
}

fn action_get(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let path = resolve_scope_path(p)?;
    let store = McpStore::load(&path)?;
    match store.servers.get(name) {
        Some(cfg) => Ok(format!("{name}: {}", serde_json::to_string_pretty(cfg)?)),
        None => Ok(format!("{name}: not registered")),
    }
}

fn action_add(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let url = p.url.as_deref().context("'url' is required")?;
    let transport = p.transport.as_deref().unwrap_or("http");
    let scope = p.scope.as_deref().unwrap_or("global");
    let headers = p.headers.clone().unwrap_or_default();
    let exclude = p.exclude_tools.clone().unwrap_or_default();

    let config = match transport {
        "http" => McpServerConfig::Http {
            url: url.to_string(),
            headers,
            exclude_tools: exclude.clone(),
        },
        "sse" => McpServerConfig::Sse {
            url: url.to_string(),
            headers,
            exclude_tools: exclude.clone(),
        },
        other => anyhow::bail!("Transport {other} not supported via bro_mcp add (use provider CLI for stdio)"),
    };

    let path = resolve_scope_path(p)?;

    // Fan out FIRST so we know whether the providers accepted the
    // add before we persist intent locally. Project-scope adds skip
    // fan-out (overlay only — user runs `sync` to push later).
    let mut fanout_lines: Vec<String> = Vec::new();
    if scope == "global" {
        for provider in [
            Provider::Claude,
            Provider::Copilot,
            Provider::Codex,
            Provider::Gemini,
        ] {
            let Some(args) = provider.build_mcp_add_http_args(name, url, &exclude) else {
                continue;
            };
            // Idempotent: best-effort remove (no-op if absent), then add.
            // The remove error is logged but not surfaced — it's expected
            // to fail when the server isn't already registered. Genuine
            // failures (CLI crash, permissions) still surface via the
            // subsequent add error.
            if let Some(rm) = provider.build_mcp_remove_args(name) {
                if let Err(e) = run_cli(&provider, &rm) {
                    tracing::debug!(target: "blackbox::mcp",
                        "{provider} idempotent pre-add remove of {name} failed (ok if not registered): {e}");
                }
            }
            match run_cli(&provider, &args) {
                Ok(()) => fanout_lines.push(format!("  {provider}: added")),
                Err(e) => fanout_lines.push(format!("  {provider}: error — {e}")),
            }
        }
    }

    // Persist intent regardless of fan-out outcome — `sync` can replay
    // failed providers later, but only if we recorded the config.
    let mut store = McpStore::load(&path)?;
    store.servers.insert(name.to_string(), config);
    store.save(&path)?;

    let mut lines = vec![format!("Saved {name} to {}", path.display())];
    lines.extend(fanout_lines);
    Ok(lines.join("\n"))
}

fn action_remove(p: &McpToolParams) -> Result<String> {
    let name = p.name.as_deref().context("'name' is required")?;
    let scope = p.scope.as_deref().unwrap_or("global");

    let path = resolve_scope_path(p)?;
    let mut store = McpStore::load(&path)?;
    let had = store.servers.remove(name).is_some();
    store.save(&path)?;

    let mut lines = vec![if had {
        format!("Removed {name} from {}", path.display())
    } else {
        format!("{name} not in {}", path.display())
    }];

    if scope == "global" {
        for provider in [
            Provider::Claude,
            Provider::Copilot,
            Provider::Codex,
            Provider::Gemini,
        ] {
            if let Some(args) = provider.build_mcp_remove_args(name) {
                match run_cli(&provider, &args) {
                    Ok(()) => lines.push(format!("  {provider}: removed")),
                    Err(e) => lines.push(format!("  {provider}: {e}")),
                }
            }
        }
    }

    Ok(lines.join("\n"))
}

fn action_filter(p: &McpToolParams, disallow: bool) -> Result<String> {
    let pattern = p.pattern.as_deref().context("'pattern' is required")?;
    let path = resolve_scope_path(p)?;
    let mut store = McpStore::load(&path)?;

    let list = if disallow {
        &mut store.filters.disallow
    } else {
        &mut store.filters.allow
    };
    if list.iter().any(|p| p == pattern) {
        return Ok(format!(
            "{} pattern {pattern} already present",
            if disallow { "disallow" } else { "allow" }
        ));
    }
    list.push(pattern.to_string());
    store.save(&path)?;

    Ok(format!(
        "Added {} pattern {pattern} to {}",
        if disallow { "disallow" } else { "allow" },
        path.display()
    ))
}

fn action_clear_filters(p: &McpToolParams) -> Result<String> {
    let path = resolve_scope_path(p)?;
    let mut store = McpStore::load(&path)?;
    let had = !store.filters.is_empty();
    store.filters = McpFilters::default();
    store.save(&path)?;
    Ok(if had {
        format!("Cleared filters in {}", path.display())
    } else {
        format!("{} already had no filters", path.display())
    })
}

fn action_sync(p: &McpToolParams) -> Result<String> {
    let path = resolve_scope_path(p)?;
    let store = McpStore::load(&path)?;

    let mut lines = vec![format!("Syncing {} server(s)…", store.servers.len())];
    for (name, cfg) in &store.servers {
        let url = match cfg {
            McpServerConfig::Http { url, .. } | McpServerConfig::Sse { url, .. } => url.clone(),
            McpServerConfig::Stdio { .. } => {
                lines.push(format!("  {name}: stdio not yet supported via sync"));
                continue;
            }
        };
        let exclude = cfg.exclude_tools();
        for provider in [
            Provider::Claude,
            Provider::Copilot,
            Provider::Codex,
            Provider::Gemini,
        ] {
            let Some(add_args) = provider.build_mcp_add_http_args(name, &url, exclude) else {
                continue;
            };
            if let Some(rm) = provider.build_mcp_remove_args(name) {
                if let Err(e) = run_cli(&provider, &rm) {
                    tracing::debug!(target: "blackbox::mcp",
                        "{provider} idempotent pre-sync remove of {name} failed (ok if not registered): {e}");
                }
            }
            match run_cli(&provider, &add_args) {
                Ok(()) => lines.push(format!("  {name} → {provider}: synced")),
                Err(e) => lines.push(format!("  {name} → {provider}: {e}")),
            }
        }
    }
    Ok(lines.join("\n"))
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_http_server() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let mut store = McpStore::new();
        store.servers.insert(
            "blackbox".into(),
            McpServerConfig::Http {
                url: "http://127.0.0.1:7264/mcp".into(),
                headers: BTreeMap::new(),
                exclude_tools: Vec::new(),
            },
        );
        store.save(&path).unwrap();
        let loaded = McpStore::load(&path).unwrap();
        assert_eq!(loaded.servers.len(), 1);
        assert!(matches!(
            loaded.servers.get("blackbox"),
            Some(McpServerConfig::Http { url, .. }) if url == "http://127.0.0.1:7264/mcp"
        ));
    }

    #[test]
    fn blackbox_matches_detects_drift() {
        let cfg = McpServerConfig::Http {
            url: "http://127.0.0.1:7264/mcp".into(),
            headers: BTreeMap::new(),
            exclude_tools: Vec::new(),
        };
        assert!(cfg.blackbox_matches("http://127.0.0.1:7264/mcp"));
        assert!(!cfg.blackbox_matches("http://127.0.0.1:7263/mcp"));
    }

    #[test]
    fn roundtrip_persists_headers_and_exclude_tools() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        let mut store = McpStore::new();
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".into(), "Bearer token".into());
        store.servers.insert(
            "blackbox".into(),
            McpServerConfig::Http {
                url: "http://127.0.0.1:7264/mcp".into(),
                headers,
                exclude_tools: vec!["bro_exec".into(), "bro_resume".into()],
            },
        );
        store.save(&path).unwrap();
        let loaded = McpStore::load(&path).unwrap();
        let cfg = loaded.servers.get("blackbox").unwrap();
        assert_eq!(
            cfg.exclude_tools(),
            &["bro_exec".to_string(), "bro_resume".to_string()]
        );
        match cfg {
            McpServerConfig::Http { headers, .. } => {
                assert_eq!(headers.get("Authorization"), Some(&"Bearer token".to_string()));
            }
            _ => panic!("expected Http variant"),
        }
    }

    #[test]
    fn exclude_tools_empty_for_stdio() {
        let cfg = McpServerConfig::Stdio {
            command: "node".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        assert!(cfg.exclude_tools().is_empty());
    }

    #[test]
    fn self_register_outcome_distinguishes_not_installed_from_list_failed() {
        let r = SelfRegisterReport {
            per_provider: vec![
                (Provider::Claude, SelfRegisterOutcome::NotInstalled { detail: "spawn err".into() }),
                (Provider::Codex, SelfRegisterOutcome::ListFailed { detail: "exit 1".into() }),
            ],
        };
        let s = r.summary();
        assert!(s.contains("not-installed"));
        assert!(s.contains("list-failed"));
    }

    #[test]
    fn filters_merge_dedupes() {
        let mut a = McpFilters {
            disallow: vec!["mcp__blackbox__bro_*".into()],
            allow: vec![],
        };
        let b = McpFilters {
            disallow: vec!["mcp__blackbox__bro_*".into(), "Bash(rm -rf *)".into()],
            allow: vec!["Read".into()],
        };
        a.merge_from(&b);
        assert_eq!(a.disallow.len(), 2);
        assert_eq!(a.allow, vec!["Read"]);
    }

    #[test]
    fn overlay_project_overrides_global() {
        let mut global = McpStore::new();
        global.servers.insert(
            "shared".into(),
            McpServerConfig::Http {
                url: "http://old/mcp".into(),
                headers: BTreeMap::new(),
                exclude_tools: Vec::new(),
            },
        );
        global.filters.disallow.push("Bash(git push *)".into());

        let mut project = McpStore::new();
        project.servers.insert(
            "shared".into(),
            McpServerConfig::Http {
                url: "http://new/mcp".into(),
                headers: BTreeMap::new(),
                exclude_tools: Vec::new(),
            },
        );
        project.filters.disallow.push("Edit(*)".into());

        let eff = resolve_effective(&global, Some(&project), false);
        assert!(matches!(
            eff.servers.get("shared"),
            Some(McpServerConfig::Http { url, .. }) if url == "http://new/mcp"
        ));
        assert_eq!(eff.filters.disallow.len(), 2);
        assert!(eff.filters.disallow.contains(&"Bash(git push *)".to_string()));
        assert!(eff.filters.disallow.contains(&"Edit(*)".to_string()));
    }

    #[test]
    fn default_guard_blocks_bro_tools() {
        let global = McpStore::new();
        let eff = resolve_effective(&global, None, true);
        assert_eq!(eff.filters.disallow, vec!["mcp__blackbox__bro_*".to_string()]);
    }

    #[test]
    fn default_guard_skipped_when_disabled() {
        let global = McpStore::new();
        let eff = resolve_effective(&global, None, false);
        assert!(eff.filters.is_empty());
    }

    #[test]
    fn expand_pattern_glob_prefix() {
        let universe = [
            "mcp__blackbox__bro_exec",
            "mcp__blackbox__bro_resume",
            "mcp__blackbox__bbox_note",
            "Bash",
        ];
        let out = expand_pattern("mcp__blackbox__bro_*", &universe);
        assert_eq!(out, vec!["mcp__blackbox__bro_exec", "mcp__blackbox__bro_resume"]);
    }

    #[test]
    fn expand_pattern_exact_match() {
        let universe = ["Bash", "Read", "Edit"];
        let out = expand_pattern("Bash", &universe);
        assert_eq!(out, vec!["Bash"]);
    }

    #[test]
    fn expand_pattern_supports_full_globs() {
        let universe = [
            "bro_exec", "bro_resume", "bro_status",
            "bbox_note", "bbox_notes",
        ];
        // Trailing `*`
        assert_eq!(expand_pattern("bro_*", &universe).len(), 3);
        // Leading `*`
        let leading = expand_pattern("*_exec", &universe);
        assert_eq!(leading, vec!["bro_exec"]);
        // Mid-string `*`
        let mid = expand_pattern("b*_note*", &universe);
        assert_eq!(mid, vec!["bbox_note", "bbox_notes"]);
        // `?` single-char wildcard
        let single = expand_pattern("bbox_note?", &universe);
        assert_eq!(single, vec!["bbox_notes"]);
        // Pure literal still works
        assert_eq!(expand_pattern("bro_exec", &universe), vec!["bro_exec"]);
        // No match returns empty (not panic)
        assert!(expand_pattern("nonexistent_*", &universe).is_empty());
    }

    #[test]
    fn gemini_policy_toml_renders_deny_rule() {
        let filters = McpFilters {
            disallow: vec!["mcp__blackbox__bro_*".to_string()],
            allow: vec![],
        };
        let toml = render_gemini_policy_toml(&filters);
        assert!(toml.contains("[[rule]]"));
        assert!(toml.contains("mcpName = \"blackbox\""));
        assert!(toml.contains("decision = \"deny\""));
        assert!(toml.contains("bro_exec"));
        assert!(toml.contains("bro_mcp"));
    }

    #[test]
    fn gemini_policy_toml_empty_when_no_blackbox_patterns() {
        let filters = McpFilters {
            disallow: vec!["Bash(rm *)".to_string()],
            allow: vec![],
        };
        let toml = render_gemini_policy_toml(&filters);
        // Only header comment, no rule block — non-blackbox patterns
        // have no Gemini policy-engine equivalent.
        assert!(!toml.contains("[[rule]]"));
    }

    #[test]
    fn write_gemini_policy_file_roundtrip() {
        // Override HOME so the write doesn't touch the user's real dir.
        let tmp = tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let filters = McpFilters {
            disallow: vec!["mcp__blackbox__bro_*".into()],
            allow: vec![],
        };
        let path = write_gemini_policy_file("test-task-123", &filters)
            .unwrap()
            .expect("expected a file");
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("[[rule]]"));
        assert!(content.contains("bro_exec"));
    }

    #[test]
    fn write_gemini_policy_file_none_when_no_filters() {
        let tmp = tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let filters = McpFilters::default();
        assert!(write_gemini_policy_file("t", &filters).unwrap().is_none());
    }

    #[test]
    fn write_gemini_policy_file_none_when_only_non_blackbox_patterns() {
        let tmp = tempdir().unwrap();
        std::env::set_var("HOME", tmp.path());
        let filters = McpFilters {
            disallow: vec!["Edit".into()],
            allow: vec![],
        };
        // Non-blackbox patterns → no rule block → no file written.
        assert!(write_gemini_policy_file("t", &filters).unwrap().is_none());
    }
}
