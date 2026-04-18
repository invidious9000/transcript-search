//! Single source of truth for the agent-facing tool reference.
//!
//! Every `bbox_*` / `bro_*` MCP tool registered in `main.rs` must have
//! a matching stanza in `TOOL_DOCS`. A unit test enforces this.
//!
//! On daemon startup, `sync_into_knowledge` upserts a fixed-ID global
//! knowledge entry (`bb-tool-reference`) rendered from `TOOL_DOCS` +
//! `WORKFLOW_NOTES`. That entry lands in `~/.claude-shared/CLAUDE.md`
//! / `~/.codex/AGENTS.md` / `~/.gemini/GEMINI.md` on the next
//! `bbox_render` pass so every agent on every project sees a current
//! tool map.
//!
//! Adding or changing a tool = one edit here. No hand-curated drift.

use anyhow::Result;

use crate::knowledge::{
    Approval, Category, KnowledgeEntry, Priority, Scope, Status,
};

pub const TOOL_DOC_ENTRY_ID: &str = "bb-tool-reference";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Transcripts,
    Knowledge,
    Threads,
    Notes,
    Inbox,
    Orchestration,
}

impl ToolCategory {
    fn heading(&self) -> &'static str {
        match self {
            Self::Transcripts => "Transcripts",
            Self::Knowledge => "Knowledge",
            Self::Threads => "Threads",
            Self::Notes => "Side-channel notes",
            Self::Inbox => "Attention / inbox",
            Self::Orchestration => "Bro orchestration",
        }
    }

    fn intro(&self) -> &'static str {
        match self {
            Self::Transcripts => "Search and read across every Claude Code / Codex / Gemini session the host has recorded. Reach for these when the user asks about past conversations, when you need to cite the origin of a rule, or when you need context around a prior decision.",
            Self::Knowledge => "Durable memory with three verbs: `bbox_learn` for rendered rules/conventions, `bbox_remember` for indexed-only notes you can grep later, `bbox_decide` for commitments with required rationale. Render pipeline emits provider-specific markdown files (CLAUDE.md / AGENTS.md / GEMINI.md). Prefer `remember` when unsure — it can be promoted to `learn` later.",
            Self::Threads => "Track non-dispatchable work that spans sessions (investigations, QC walks, debugging, refinement loops). Lighter than the full dispatch pipeline, heavier than memory. Use `kind=work_item` for orchestrator-led propose→execute→review→refine loops.",
            Self::Notes => "Structured side channel for observations emitted during work. Executors call `bbox_note` throughout a dispatch; orchestrators query `bbox_notes` / `bbox_inbox` at round boundaries. Seven kinds: `dispute`, `assumption`, `surprise`, `followup`, `blocked`, `learned`, `done`. The *done* kind with a one-line acceptance summary is the single highest-leverage signal — always emit it on completion.",
            Self::Inbox => "Attention aggregator: a single read that surfaces unresolved notes, stale threads, unverified knowledge, and failed tasks. Run at round boundaries, morning-brief style, and whenever you're unsure what needs attention next.",
            Self::Orchestration => "Dispatch agents across providers (Claude, Codex, Copilot, Vibe, Gemini). Prefer named `bro` targeting (resolves provider + account + lens + session automatically) over raw provider. Core pattern: `bro_exec` to launch, `bro_wait` or `bro_when_all` to block, `bro_resume` for follow-ups (never `bro_exec` again — it starts fresh with no memory). For ensembles: `bro_broadcast` + `bro_when_all` (blind deliberation) or `bro_when_any` (race).",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ToolDoc {
    pub name: &'static str,
    pub category: ToolCategory,
    pub summary: &'static str,
    pub when_to_use: &'static str,
    pub example: Option<&'static str>,
}

pub const TOOL_DOCS: &[ToolDoc] = &[
    // ── Transcripts ──────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_search",
        category: ToolCategory::Transcripts,
        summary: "Full-text search across all indexed transcripts.",
        when_to_use: "User asks 'when did we discuss X' / 'find the session where Y' / 'what did codex say about Z'. Filterable by account, project, role.",
        example: Some(r#"bbox_search(query="redis locking", project="my-app", role="user")"#),
    },
    ToolDoc {
        name: "bbox_cite",
        category: ToolCategory::Transcripts,
        summary: "Trace a claim back to the turn that established it.",
        when_to_use: "You want provenance for a rule or preference. Defaults to role=user, returns citations oldest-first so the origin surfaces first.",
        example: Some(r#"bbox_cite(claim="never kill processes by port")"#),
    },
    ToolDoc {
        name: "bbox_context",
        category: ToolCategory::Transcripts,
        summary: "Conversation context around a specific byte offset.",
        when_to_use: "You got a hit from `bbox_search` and want the surrounding turns.",
        example: None,
    },
    ToolDoc {
        name: "bbox_session",
        category: ToolCategory::Transcripts,
        summary: "Summary metadata for a single session.",
        when_to_use: "You have a session ID (from search or dashboard) and want first prompt, project, duration, tool usage, counts.",
        example: None,
    },
    ToolDoc {
        name: "bbox_messages",
        category: ToolCategory::Transcripts,
        summary: "Chronological messages from a session.",
        when_to_use: "Read conversation flow. Supports pagination, role filter, tail mode.",
        example: None,
    },
    ToolDoc {
        name: "bbox_reindex",
        category: ToolCategory::Transcripts,
        summary: "Build or incrementally update the search index.",
        when_to_use: "Rarely — background reindexer runs every 120s. Use `full=true` after corpus corruption or schema changes.",
        example: None,
    },
    ToolDoc {
        name: "bbox_topics",
        category: ToolCategory::Transcripts,
        summary: "Top terms in a session by frequency.",
        when_to_use: "Quick 'what was this session about' without LLM summarization.",
        example: None,
    },
    ToolDoc {
        name: "bbox_sessions_list",
        category: ToolCategory::Transcripts,
        summary: "Browse sessions sorted by recency.",
        when_to_use: "Finding a session by project or name without a specific query.",
        example: None,
    },
    ToolDoc {
        name: "bbox_stats",
        category: ToolCategory::Transcripts,
        summary: "Corpus statistics (doc count, index size, file counts).",
        when_to_use: "Sanity-check the index; diagnose 'did my new sessions get indexed?'.",
        example: None,
    },

    // ── Knowledge ────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_learn",
        category: ToolCategory::Knowledge,
        summary: "Persist a rule or convention; rendered into provider markdown files.",
        when_to_use: "User says 'from now on / always / never X' in a way that should bind future sessions. Entry appears in every agent's CLAUDE.md / AGENTS.md / GEMINI.md after `bbox_render`.",
        example: Some(r#"bbox_learn(content="use rustls, not openssl", category="convention", scope="project", project="/repo/x")"#),
    },
    ToolDoc {
        name: "bbox_remember",
        category: ToolCategory::Knowledge,
        summary: "Persist a fact for later recall; indexed but NOT rendered.",
        when_to_use: "Observations, decisions, context worth grepping for later but not worth every session loading. Safer default than `learn` when unsure.",
        example: Some(r#"bbox_remember(content="port 7263 conflicts with helper-daemon on host bravo", title="port clash")"#),
    },
    ToolDoc {
        name: "bbox_decide",
        category: ToolCategory::Knowledge,
        summary: "Record a durable commitment with required rationale; supports supersession.",
        when_to_use: "You're locking in a design choice or reversing a prior decision. Rationale is required. Passing `supersedes` marks the prior entry superseded and links them.",
        example: Some(r#"bbox_decide(content="use RocksDB for cache", rationale="SQLite locking conflicted with concurrent writers", supersedes="8a3f...")"#),
    },
    ToolDoc {
        name: "bbox_knowledge",
        category: ToolCategory::Knowledge,
        summary: "Query stored entries by free-text or filters. First tool call on any substantive task per the CORE RULE above.",
        when_to_use: "The start of any task. Default to `query=<one distinctive word>`. Use scoped filters (`category`, `scope`, `project`) only for browsing audit trails, e.g. `bbox_knowledge(category=\"decision\", project=\"/repo/x\")`.",
        example: Some(r#"bbox_knowledge(query="retry")"#),
    },
    ToolDoc {
        name: "bbox_forget",
        category: ToolCategory::Knowledge,
        summary: "Retire or supersede an entry.",
        when_to_use: "Entry is stale or replaced. Prefer `bbox_decide` with `supersedes` if the replacement is itself a decision.",
        example: None,
    },
    ToolDoc {
        name: "bbox_render",
        category: ToolCategory::Knowledge,
        summary: "Render entries into CLAUDE.md / AGENTS.md / GEMINI.md.",
        when_to_use: "After adding or changing entries. Scope `global` patches host-wide memory files; `project` writes project-local files + PROJECT.md.",
        example: Some(r#"bbox_render(scope="project", project="/repo/x")"#),
    },
    ToolDoc {
        name: "bbox_absorb",
        category: ToolCategory::Knowledge,
        summary: "Import external edits to rendered files back as unverified entries.",
        when_to_use: "User hand-edited the rendered CLAUDE.md / AGENTS.md / GEMINI.md and you want to reconcile.",
        example: None,
    },
    ToolDoc {
        name: "bbox_lint",
        category: ToolCategory::Knowledge,
        summary: "Health check for contradictions, stale entries, duplicates.",
        when_to_use: "Periodic hygiene; before large refactors of the knowledge store.",
        example: None,
    },
    ToolDoc {
        name: "bbox_review",
        category: ToolCategory::Knowledge,
        summary: "Approve or reject entries awaiting review.",
        when_to_use: "Unverified entries accumulate from absorb and agent-inferred `learn` calls. Review them before they render into global memory.",
        example: None,
    },
    ToolDoc {
        name: "bbox_bootstrap",
        category: ToolCategory::Knowledge,
        summary: "Onboard a new repo into the blackbox knowledge system.",
        when_to_use: "First-time setup for a project — seeds PROJECT.md, scaffolds knowledge structure.",
        example: None,
    },

    // ── Threads ──────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_thread",
        category: ToolCategory::Threads,
        summary: "Open / continue / resolve / promote / rename / link a work thread.",
        when_to_use: "Investigation or QC walk that may span sessions; deferred work too informal for a finding. Check `bbox_thread_list` first — a thread may already exist. Use `kind=work_item` for orchestrator-led dispatch loops.",
        example: Some(r#"bbox_thread(action="open", topic="audit the dispatch path", project="/repo/x", kind="work_item")"#),
    },
    ToolDoc {
        name: "bbox_thread_list",
        category: ToolCategory::Threads,
        summary: "Scan open / active / stale threads.",
        when_to_use: "Before starting work on a topic (continuity check). Use `stale_days` to find abandoned work. Filter by `kind=work_item`.",
        example: None,
    },

    // ── Notes ────────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_note",
        category: ToolCategory::Notes,
        summary: "Record a structured side-channel note while working.",
        when_to_use: "As an executor: emit throughout a dispatch for the 7 kinds below. As an orchestrator: rarely — you're the reader. Genuine signal only, not stylistic preference. The `done` kind with a one-line acceptance summary is the most important: always emit before returning. Kinds: `dispute` (disagree with brief/premise), `assumption` (ambiguity-resolving judgment), `surprise` (expected X, found Y), `followup` (out-of-scope work deferred), `blocked` (subtask blocked, include reason), `learned` (project convention discovered in situ), `done` (completion + summary).",
        example: Some(r#"bbox_note(kind="dispute", body="brief assumes schema is additive — migration 0042 makes it subtractive")"#),
    },
    ToolDoc {
        name: "bbox_notes",
        category: ToolCategory::Notes,
        summary: "List / filter notes by kind, project, session, thread, resolution.",
        when_to_use: "Orchestrators reading what executors emitted this round, or auditing past dispatch for a work-item thread.",
        example: Some(r#"bbox_notes(kind="assumption", thread_id="thread-abc")"#),
    },
    ToolDoc {
        name: "bbox_note_resolve",
        category: ToolCategory::Notes,
        summary: "Mark a note acknowledged or addressed.",
        when_to_use: "Orchestrator's close-the-loop move. Addressed notes drop from the default inbox view.",
        example: None,
    },

    // ── Inbox ────────────────────────────────────────────────────────
    ToolDoc {
        name: "bbox_inbox",
        category: ToolCategory::Inbox,
        summary: "Aggregate attention layer across every store.",
        when_to_use: "Round boundaries, morning brief, any 'what needs my attention' moment. Surfaces unresolved disputes/blocked/surprises, deferred followups, stale threads, unverified knowledge, failed bro tasks. Single call, prioritized view.",
        example: Some(r#"bbox_inbox(project="/repo/x", stale_days=3)"#),
    },

    // ── Orchestration (bro) ──────────────────────────────────────────
    ToolDoc {
        name: "bro_exec",
        category: ToolCategory::Orchestration,
        summary: "Launch an agent task. Returns {taskId, sessionId} immediately.",
        when_to_use: "Dispatching work. Prefer `bro: \"name\"` over `provider: \"...\"` — named bros resolve provider/account/lens/sessionId automatically. Returns immediately; follow with `bro_wait` or `bro_when_all`.",
        example: Some(r#"bro_exec(bro="executor", prompt="refactor the tail module", project_dir="/repo/x")"#),
    },
    ToolDoc {
        name: "bro_resume",
        category: ToolCategory::Orchestration,
        summary: "Continue an existing session with a follow-up.",
        when_to_use: "Multi-turn conversations with a specific bro. NEVER use `bro_exec` again for follow-ups — it starts fresh. Named bro targeting auto-resolves the sessionId.",
        example: Some(r#"bro_resume(bro="executor", prompt="add tests for the edge case we discussed")"#),
    },
    ToolDoc {
        name: "bro_wait",
        category: ToolCategory::Orchestration,
        summary: "Block until a single task completes.",
        when_to_use: "After `bro_exec`. USE MAXIMUM TIMEOUT. Returns the final task state.",
        example: None,
    },
    ToolDoc {
        name: "bro_when_all",
        category: ToolCategory::Orchestration,
        summary: "Block until ALL tasks / team members complete.",
        when_to_use: "Fan-out/fan-in pattern. Pair with `bro_broadcast` for blind deliberation / provider comparison. USE MAXIMUM TIMEOUT.",
        example: None,
    },
    ToolDoc {
        name: "bro_when_any",
        category: ToolCategory::Orchestration,
        summary: "Block until the FIRST task completes.",
        when_to_use: "Racing providers / fast-path resolution. First result wins, others keep running unless cancelled.",
        example: None,
    },
    ToolDoc {
        name: "bro_broadcast",
        category: ToolCategory::Orchestration,
        summary: "Send the same prompt to every team member.",
        when_to_use: "Ensemble work. Follow with `bro_when_all` (deliberation) or `bro_when_any` (race). Interleave with individual `bro_resume` for cross-pollination between rounds.",
        example: None,
    },
    ToolDoc {
        name: "bro_status",
        category: ToolCategory::Orchestration,
        summary: "Non-blocking progress check on a task.",
        when_to_use: "Peek at a running task without blocking. Prefer `bro_wait` with a timeout when you actually need the result.",
        example: None,
    },
    ToolDoc {
        name: "bro_dashboard",
        category: ToolCategory::Orchestration,
        summary: "List recent tasks / sessions.",
        when_to_use: "Look up a taskId or sessionId when you don't already have it. Filter by provider, status, team.",
        example: None,
    },
    ToolDoc {
        name: "bro_cancel",
        category: ToolCategory::Orchestration,
        summary: "Cancel a running task (SIGTERM).",
        when_to_use: "Task is stuck, you raced another, or user asked to stop.",
        example: None,
    },
    ToolDoc {
        name: "bro_prune",
        category: ToolCategory::Orchestration,
        summary: "Drop terminal tasks from the store + persisted tasks.json.",
        when_to_use: "Stale failed/completed tasks are cluttering bro_dashboard or bbox_inbox. Defaults to status=failed. Filter by provider or older_than_hours; use dry_run=true to preview. Running tasks are never touched.",
        example: Some(r#"bro_prune(status="failed", provider="gemini")"#),
    },
    ToolDoc {
        name: "bro_providers",
        category: ToolCategory::Orchestration,
        summary: "List configured providers, binaries, models.",
        when_to_use: "Check what's available before composing a team or choosing a model.",
        example: None,
    },
    ToolDoc {
        name: "bro_brofile",
        category: ToolCategory::Orchestration,
        summary: "Manage brofile templates + accounts (provider+account+lens).",
        when_to_use: "Create / list / get / delete brofiles; set accounts. Brofiles are reusable team-member blueprints.",
        example: None,
    },
    ToolDoc {
        name: "bro_team",
        category: ToolCategory::Orchestration,
        summary: "Manage teamplates and instantiated teams.",
        when_to_use: "Save / list / delete teamplates; create / list / dissolve teams; show roster. A team = instantiated teamplate with named bro instances tracking their own sessionIds.",
        example: None,
    },
    ToolDoc {
        name: "bro_mcp",
        category: ToolCategory::Orchestration,
        summary: "Manage MCP servers + tool filters for dispatched bros.",
        when_to_use: "Add/remove MCP servers visible to dispatched bros (fans out to Claude/Copilot/Codex/Gemini CLIs on global-scope writes). Allow/disallow tool patterns for mechanical filtering — default disallow `mcp__blackbox__bro_*` replaces the text recursion guard on providers that support dispatch-time filtering (Claude, Copilot). Actions: list, get, add, remove, allow, disallow, clear_filters, sync.",
        example: Some(r#"bro_mcp(action="disallow", pattern="mcp__blackbox__bro_*", scope="global")"#),
    },
];

pub const WORKFLOW_NOTES: &str = "\
## Roles and the core loop

Blackbox supports a two-role workflow across multi-provider dispatch:

- **Orchestrator** (usually the main-session agent). Proposes work, \
deliberates with an ensemble, dispatches to executors, reviews output, \
iterates. Reads `bbox_inbox` at round boundaries. Writes `bbox_decide` \
when a commitment is made. Marks notes `acknowledged` or `addressed` \
via `bbox_note_resolve` as the loop progresses.

- **Executor** (dispatched via `bro_exec` / `bro_broadcast`, or the \
equivalent in a cosession flow). Does the work. Emits `bbox_note` \
records throughout for the 7 kinds. Always emits `kind=done` with a \
one-line acceptance summary before returning — this is the \
orchestrator's primary scan signal.

## Ambient scope block

Every dispatched agent receives a per-turn ambient prefix containing \
pre-bound IDs: `session`, `project`, `bro`, and (when applicable) \
`thread`, `work_item`. Use those IDs in `bbox_note` / `bbox_thread` \
calls rather than reaching back through the transcript to guess. The \
daemon may also auto-fill these fields when you omit them.

## Persistence hierarchy (when to reach for which)

- `bbox_remember` — default. Indexed, grep-able, never rendered. \
Safest choice when unsure.
- `bbox_learn` — rules / conventions that should bind every future \
session. Renders into CLAUDE.md / AGENTS.md / GEMINI.md.
- `bbox_decide` — durable commitments with required rationale. Pass \
`supersedes` to reverse a prior decision with an auditable chain.
- `bbox_note` — transient work artifacts during a dispatch. Auto-expires \
from attention views once addressed.

## Work-item threads

For orchestrator-led propose→execute→review→refine loops, open a \
thread with `kind=\"work_item\"`. Pass its ID as `thread_id` to \
`bbox_note` calls so the loop's notes collate automatically. The \
orchestrator can read the full refinement history with \
`bbox_notes(thread_id=...)` instead of re-reading transcripts.

## Dispatch etiquette

- Prefer named bro targeting (`bro: \"executor\"`) over raw provider — \
it resolves provider, account, lens, and sessionId automatically.
- Always use `bro_resume` (not `bro_exec`) for multi-turn conversations. \
`bro_exec` starts fresh with no memory.
- Use maximum timeouts on `bro_wait` / `bro_when_all` / `bro_when_any`.
- For ensembles: `bro_broadcast` then `bro_when_all` (blind deliberation) \
or `bro_when_any` (race).
";

// ── Filter translation helpers ───────────────────────────────────────

/// Bare names of every orchestration (`bro_*`) tool. Used by provider
/// filter translators that can't accept glob patterns (Codex,
/// Gemini's policy engine) to expand `mcp__blackbox__bro_*` into a
/// concrete list.
pub fn orchestration_tool_names() -> Vec<&'static str> {
    TOOL_DOCS
        .iter()
        .filter(|d| d.category == ToolCategory::Orchestration)
        .map(|d| d.name)
        .collect()
}

/// Prefix convention for blackbox-served tools in provider tool namespaces.
pub const BLACKBOX_MCP_PREFIX: &str = "mcp__blackbox__";

// ── Rendering ────────────────────────────────────────────────────────

/// Render the full tool reference as markdown. Shape: category intros
/// followed by per-tool stanzas, then workflow notes.
pub fn render_markdown() -> String {
    let mut out = String::new();
    out.push_str("Blackbox tool reference — the MCP tools this daemon exposes and when to reach for them. ");
    out.push_str("This entry is generated from `src/tool_docs.rs` and refreshed on every daemon restart. ");
    out.push_str("Do not hand-edit.\n\n");

    out.push_str("## CORE RULE: recall first\n\n");
    out.push_str("**On any substantive task, your FIRST tool call must be `bbox_knowledge(query=<one keyword>)` to check for stored project-specific context.** Not the second call after `ls`. Not after probing the live system. First.\n\n");
    out.push_str("The signature failure mode here: agents confidently produce training-prior answers to questions whose actual answer is stored in bbox. This is not a suggestion.\n\n");
    out.push_str("Use a single distinctive keyword from the task. If empty, try a different word. Do not fall back to filesystem exploration, process probing, or training-prior inference until at least 2 distinct queries have returned empty.\n\n");
    out.push_str("Cost of a wasted query: near zero. Cost of a confident wrong answer: the entire task.\n\n");

    let categories = [
        ToolCategory::Transcripts,
        ToolCategory::Knowledge,
        ToolCategory::Threads,
        ToolCategory::Notes,
        ToolCategory::Inbox,
        ToolCategory::Orchestration,
    ];

    for cat in categories {
        out.push_str(&format!("## {}\n\n", cat.heading()));
        out.push_str(cat.intro());
        out.push_str("\n\n");
        for doc in TOOL_DOCS.iter().filter(|d| d.category == cat) {
            out.push_str(&format!("- **`{}`** — {}\n", doc.name, doc.summary));
            out.push_str(&format!("  _When to use:_ {}\n", doc.when_to_use));
            if let Some(ex) = doc.example {
                out.push_str(&format!("  _Example:_ `{ex}`\n"));
            }
        }
        out.push('\n');
    }

    out.push_str(WORKFLOW_NOTES);
    out
}

// ── Sync into knowledge store ────────────────────────────────────────

pub struct SyncResult {
    /// true = upsert wrote to disk; false = content unchanged
    pub wrote: bool,
    pub bytes: usize,
}

/// Upsert the canonical tool reference as a fixed-ID global entry.
/// Idempotent: no-op if the content hasn't changed.
pub fn sync_into_knowledge(kb: &mut crate::knowledge::Knowledge) -> Result<SyncResult> {
    let content = render_markdown();
    let bytes = content.len();

    // Look for existing entry by stable ID
    let existing = kb
        .all_entries()
        .iter()
        .find(|e| e.id == TOOL_DOC_ENTRY_ID)
        .cloned();

    if let Some(ref e) = existing {
        if e.content == content {
            return Ok(SyncResult { wrote: false, bytes });
        }
    }

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let entry = KnowledgeEntry {
        id: TOOL_DOC_ENTRY_ID.to_string(),
        title: "Blackbox tool reference".to_string(),
        content,
        variants: Default::default(),
        category: Category::Tool,
        scope: Scope::Global,
        project: None,
        providers: Vec::new(),
        priority: Priority::Standard,
        weight: 100,
        render: true,
        decay: false, // generated; managed by code
        review_at: None,
        status: Status::Active,
        approval: Approval::UserConfirmed,
        supersedes: None,
        rationale: None,
        expires_at: None,
        source: "tool_docs".to_string(),
        created_at: existing.as_ref().map(|e| e.created_at.clone()).unwrap_or_else(|| now.clone()),
        updated_at: now,
        recall_count: 0,
        last_recalled: None,
    };

    kb.upsert_generated(entry)?;
    Ok(SyncResult { wrote: true, bytes })
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_every_tool_name() {
        let md = render_markdown();
        for doc in TOOL_DOCS {
            assert!(md.contains(doc.name), "rendered markdown missing {}", doc.name);
        }
    }

    #[test]
    fn render_includes_workflow_notes() {
        let md = render_markdown();
        assert!(md.contains("## Roles and the core loop"));
        assert!(md.contains("Ambient scope"));
        assert!(md.contains("Persistence hierarchy"));
    }

    #[test]
    fn every_registered_tool_has_a_doc() {
        // Greps main.rs for `#[tool(name = "bbox_..." / "bro_..."` and
        // asserts each name has a ToolDoc stanza. Source-of-truth check.
        let main_rs = include_str!("main.rs");
        let mut registered: Vec<String> = Vec::new();
        for line in main_rs.lines() {
            if let Some(rest) = line.trim_start().strip_prefix("#[tool(name = \"") {
                if let Some(end) = rest.find('"') {
                    let name = &rest[..end];
                    if name.starts_with("bbox_") || name.starts_with("bro_") {
                        registered.push(name.to_string());
                    }
                }
            }
        }
        assert!(!registered.is_empty(), "no tools found in main.rs — parse regressed");

        let documented: std::collections::HashSet<&str> =
            TOOL_DOCS.iter().map(|d| d.name).collect();

        let missing: Vec<&str> = registered
            .iter()
            .filter(|n| !documented.contains(n.as_str()))
            .map(|s| s.as_str())
            .collect();

        assert!(
            missing.is_empty(),
            "tools registered in main.rs without a ToolDoc stanza: {missing:?}"
        );

        let registered_set: std::collections::HashSet<&str> =
            registered.iter().map(|s| s.as_str()).collect();
        let extra: Vec<&str> = TOOL_DOCS
            .iter()
            .map(|d| d.name)
            .filter(|n| !registered_set.contains(n))
            .collect();
        assert!(
            extra.is_empty(),
            "ToolDoc stanzas without a matching #[tool] registration: {extra:?}"
        );
    }
}
