# blackbox

MCP server for AI dev tooling: full-text search across Claude Code / Codex / Copilot / Vibe / Gemini transcripts, a unified knowledge store rendered into provider markdown files, work-thread tracking, and multi-provider agent orchestration. Backed by [tantivy](https://github.com/quickwit-oss/tantivy) (Rust, BM25 ranking). Sub-50ms queries over hundreds of thousands of indexed documents.

The crate is `blackbox`. It produces two binaries:
- **`blackboxd`** — the MCP server daemon
- **`bro`** — terminal client for live orchestration tail

## Install

```bash
git clone https://github.com/invidious9000/transcript-search.git
cd transcript-search
cargo build --release
# binaries land at target/release/{blackboxd,bro}
```

Add to your Claude Code MCP configuration (`~/.claude.json`):

```json
{
  "mcpServers": {
    "blackbox": {
      "command": "/path/to/transcript-search/target/release/blackboxd",
      "args": []
    }
  }
}
```

For Codex CLI, add to `~/.codex/config.toml`:

```toml
[mcp_servers.blackbox]
command = "/path/to/transcript-search/target/release/blackboxd"
args = []
```

Restart your session. The first search auto-builds the index.

## What gets indexed

| Source | Event type | Index role |
|---|---|---|
| Claude Code | User messages | `user` |
| Claude Code | Assistant text | `assistant` |
| Claude Code | Thinking blocks | `thinking` |
| Claude Code | Tool use (name + input) | `tool_use` |
| Claude Code | Tool results | `tool_result` |
| Claude Code | Subagent transcripts | all roles, `is_subagent=1` |
| Codex CLI | User/assistant/developer messages | `user`, `assistant`, `developer` |
| Codex CLI | Function calls | `tool_use` |
| Codex CLI | Function results | `tool_result` |
| Codex CLI | Reasoning blocks | `thinking` |
| Both | Command history | `user` |

Content is capped at 12KB per document. Responses are capped at 80KB to avoid blowing MCP result limits.

## MCP Tools

### Transcript search (`bbox_*`)

| Tool | Description |
|---|---|
| `bbox_search` | Full-text query with filters (account, project, role, include_subagents, limit). Terms are ANDed by default; supports `OR`, `"phrase queries"`. Returns ranked results with highlighted excerpts. |
| `bbox_messages` | Read a session's conversation flow. Accepts `session_id` or `file_path`. Supports `role` filter, `from_end` (tail mode), `offset`/`limit` pagination, `max_content_length`. |
| `bbox_context` | Surrounding messages around a search hit (given file path + byte offset). |
| `bbox_session` | Session metadata: first prompt, project, duration, tool usage, message counts. |
| `bbox_topics` | Top terms from a session by frequency analysis (no LLM). Stop-word filtered. |
| `bbox_sessions_list` | Browse sessions across accounts, sorted by recency. Filter by account, project. |
| `bbox_reindex` | Incremental (default) or full rebuild. Only re-processes new/modified files. |
| `bbox_stats` | Corpus statistics: document count, index size, per-account file counts. |

### Knowledge store (`bbox_*`)

| Tool | Description |
|---|---|
| `bbox_learn` | Add or update a knowledge entry. Entries render into provider markdown. |
| `bbox_remember` | Store a fact for on-demand recall only — NOT rendered into markdown. |
| `bbox_knowledge` | List/search knowledge entries with category/scope/provider filters. |
| `bbox_forget` | Remove or supersede an entry. |
| `bbox_render` | Render entries → CLAUDE.md / AGENTS.md / GEMINI.md (steerage → memory → PROJECT.md). |
| `bbox_absorb` | Detect external edits to rendered files and import them as unverified entries. |
| `bbox_lint` | Health check: contradictions, stale entries, duplicates. |
| `bbox_review` | Review unverified entries — list, approve, reject. |
| `bbox_bootstrap` | Scan a new repo's existing instruction files for migration into the knowledge store. |

### Work threads (`bbox_*`)

| Tool | Description |
|---|---|
| `bbox_thread` | Manage long-running work threads — friendly names, edges to other threads/sessions, notes. |
| `bbox_thread_list` | List/scan threads (open/active/stale by default). |

### Multi-provider orchestration (`bro_*`)

Dispatch agent tasks to Claude, Codex, Copilot, Vibe, or Gemini and coordinate them as teams.

| Tool | Description |
|---|---|
| `bro_exec` | Launch an agent task. Returns `{taskId, sessionId}` immediately. |
| `bro_resume` | Resume a previous agent session with a follow-up prompt. |
| `bro_wait` / `bro_when_all` / `bro_when_any` | Block until one / all / first task(s) complete. |
| `bro_broadcast` | Send the same prompt to every team member. |
| `bro_status` | Non-blocking progress check. |
| `bro_cancel` | Send SIGTERM to a running task. |
| `bro_dashboard` | List recent tasks and sessions. |
| `bro_providers` | Show configured providers, binaries, and model/effort catalogs. |
| `bro_brofile` | Manage brofile templates and named accounts. |
| `bro_team` | Manage team templates and live teams. |

Live tail from a terminal:
```bash
bro tail [--team NAME] [--bro NAME] [--provider NAME]
```

## Provider catalog

Maintained in `src/orchestration/providers.rs`:

- **Claude**: Opus 4.7 (default, 1M context built-in), Opus 4.6, Sonnet 4.6, Haiku 4.5. Effort tiers `low`/`medium`/`high`/`xhigh`/`max` (default `xhigh`; `xhigh` is Opus-4.7-only, `max` unsupported on Haiku).
- **Codex**: gpt-5.4 family. Efforts `minimal`/`low`/`medium`/`high`/`xhigh`.
- **Copilot**: Anthropic + OpenAI models. Efforts `low`/`medium`/`high`/`xhigh`.
- **Vibe**, **Gemini**: model lists only.

## Configuration

Auto-detection works out of the box for most setups. Override via environment variables.

| Env var | Default | Description |
|---|---|---|
| `TRANSCRIPT_SEARCH_ROOTS` | auto-detect `~/.claude` + `~/.claude-*` | Account roots. Format: `name=/path,name2=/path2` |
| `TRANSCRIPT_SEARCH_CODEX_ROOT` | `~/.codex` if it exists | Codex CLI data directory |
| `TRANSCRIPT_SEARCH_INDEX_PATH` | `~/.claude-shared/transcript-index` or `~/.local/share/blackbox/index` | Tantivy index location |
| `BLACKBOX_REINDEX_INTERVAL_SECS` | `120` | Background reindex interval (seconds) |
| `BBOX_PORT` / `BRO_PORT` | `7263` | HTTP port for `/tail` endpoint |
| `CLAUDE_BIN` / `CODEX_BIN` / `COPILOT_BIN` / `GEMINI_BIN` | from `$PATH` | Override provider binary paths |
| `RUST_LOG` | `transcript_search=info` | Tracing filter |

### Auto-detection

By default the server:
1. Always includes `~/.claude` as the `claude` account
2. Scans `~/` for any `~/.claude-*` directories that contain a `projects/` subdirectory
3. Includes `~/.codex` if `~/.codex/sessions/` exists

### Multi-account example

```json
{
  "mcpServers": {
    "blackbox": {
      "command": "/path/to/blackboxd",
      "args": [],
      "env": {
        "TRANSCRIPT_SEARCH_ROOTS": "personal=~/.claude,work=~/.claude-work"
      }
    }
  }
}
```

## Usage tips

- **First search auto-indexes** if the index is empty. Takes 1-3 minutes depending on corpus size.
- **Incremental reindex** is fast — skips unchanged files by mtime/size. Call `bbox_reindex` to pick up new sessions.
- **Logs go to stderr** (stdout is MCP transport). Set `RUST_LOG=blackbox=debug` for verbose output.
- **Add a CLAUDE.md cue** so Claude knows to use these tools:
  ```markdown
  ### Past conversations
  When the user asks about past conversations, use the `bbox_*` MCP tools.
  Do NOT write ad-hoc Python to parse JSONL transcript files.
  ```

## JSONL transcript schemas

**Claude Code** stores transcripts at `~/.claude/projects/<encoded-path>/<session-uuid>.jsonl`:
```jsonc
{"type": "user|assistant|progress", "message": {...}, "sessionId": "uuid", "timestamp": "ISO-8601", ...}
```

**Codex CLI** stores transcripts at `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`:
```jsonc
{"timestamp": "ISO-8601", "type": "session_meta|response_item|event_msg", "payload": {...}}
```

## Architecture

- **Tantivy** for full-text indexing with BM25 ranking, phrase queries, and positional indexing
- **Separate documents per content block** — each text/thinking/tool_use block is its own document, enabling role-based filtering and precise excerpts
- **Incremental indexing** via file mtime/size tracking
- **MCP over stdio** — standard JSON-RPC 2.0, compatible with Claude Code and Codex CLI
- **Knowledge render pipeline** — three-layer composition (steerage → shared memory → per-project PROJECT.md) into provider-specific markdown
- **Multi-provider orchestration** — spawns provider CLIs as child processes, streams JSON events, manages task lifecycle and team coordination
- **No LLM calls** — pure local indexing and retrieval. `bbox_topics` uses term frequency, not embeddings.

## License

MIT
