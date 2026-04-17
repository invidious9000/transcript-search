# Examples

Reference configurations that demonstrate how to wire blackbox (`bbox_*` / `bro_*` MCP tools)
into the CLIs it orchestrates. Copy / adapt into your own project or agent directories — the
daemon doesn't read these files, they're just starting points.

## Agents

Drop-in subagent definitions for Claude Code. Install by copying into
`.claude/agents/` in your repo (project-scoped) or `~/.claude/agents/` (user-scoped).

| File | Purpose |
|---|---|
| [`agents/session-searcher.md`](agents/session-searcher.md) | Read-only subagent that searches indexed CLI transcripts across every provider / account on the host. Traces rules to their origin turn, summarizes sessions for takeover, audits what another agent did, samples topics across sessions. Scoped so it can only call `bbox_*` readers — never mutates. Use it to keep transcript digging out of your main context window. |

## Skills / Slash Commands

Workflow definitions for Claude Code, invocable as `/user:<name>` (user-scoped) or
`/project:<name>` (project-scoped). Install by copying into `~/.claude/commands/` or
`.claude/commands/` in your repo.

| File | Purpose |
|---|---|
| [`skills/crucible.md`](skills/crucible.md) | Orchestrator-led implementation workflow. Main-session Claude drives; a durable implementer bro (Opus 4.7 xhigh, held across rounds via `bro_resume`) carries mechanical code context the main session would otherwise lose to compaction; a continuous red-team ensemble (codex + gemini via `red_team` teamplate by default) reviews plan and work product with sustained per-member context. All coordinated through a `bbox_thread(kind="work_item")` and structured `bbox_note` signals (`dispute` / `surprise` / `blocked` / `followup` / `done`) so the orchestrator scans a signal trail instead of parsing prose. Use when context compartmentalization is the primary benefit and cross-provider consensus at bookends is worth the ceremony. |
| [`skills/takeover.md`](skills/takeover.md) | Take over driving an existing agent session. Composes **thread init** (find-or-open a `bbox_thread` with full scope context — handoff docs, source docs, prior takeover notes) and **thread run** (resume the target session via `bro_resume`, drive it iteratively against an authoritative scope checklist until halt conditions are met). Pairs well with the `session-searcher` agent for transcript recon. Use when an agent session stalled, got handed off, or was interrupted mid-work and you need to pick it up without losing scope. |

## Adding your own

PRs welcome. Keep examples self-contained — no references to private / project-specific
tooling, no references to corpora blackbox doesn't actually index. Agents should declare
their tool scope explicitly (`disallowedTools` or equivalent) and state up front which
`bbox_*` / `bro_*` surface they depend on.
