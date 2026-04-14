# transcript-search

MCP server for full-text search across Claude Code and Codex CLI conversation transcripts. Backed by [tantivy](https://github.com/quickwit-oss/tantivy) (Rust, BM25 ranking). Sub-50ms queries over hundreds of thousands of indexed documents.

## What it does

Claude Code and Codex CLI store every conversation as JSONL files on disk. This tool indexes them into a tantivy full-text search index and exposes search, browsing, and analysis via MCP tools — so any Claude Code session can search its own (and other sessions') history.

## Install

```bash
# Build
git clone https://github.com/invidious9000/transcript-search.git
cd transcript-search
cargo build --release

# Add to Claude Code MCP config (~/.claude.json or $CLAUDE_CONFIG_DIR/.claude.json)
```

Add to your Claude Code MCP configuration:

```json
{
  "mcpServers": {
    "transcript-search": {
      "command": "/path/to/transcript-search/target/release/transcript-search",
      "args": []
    }
  }
}
```

For Codex CLI, add to `~/.codex/config.toml`:

```toml
[mcp_servers.transcript-search]
command = "/path/to/transcript-search/target/release/transcript-search"
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

| Tool | Description |
|---|---|
| `transcript_search` | Full-text query with filters (account, project, role, include_subagents, limit). Terms are ANDed by default; supports `OR`, `"phrase queries"`. Returns ranked results with highlighted excerpts. |
| `transcript_messages` | Read a session's conversation flow. Accepts `session_id` or `file_path`. Supports `role` filter, `from_end` (tail mode), `offset`/`limit` pagination, `max_content_length`. |
| `transcript_context` | Surrounding messages around a search hit (given file path + byte offset). |
| `transcript_session` | Session metadata: first prompt, project, duration, tool usage, message counts. |
| `transcript_topics` | Top terms from a session by frequency analysis (no LLM). Stop-word filtered. Quick "what was this session about?" |
| `transcript_sessions_list` | Browse all sessions across accounts, sorted by recency. Filter by account, project. Shows date, duration, first prompt. |
| `transcript_reindex` | Incremental (default) or full rebuild. Only re-processes new/modified files. |
| `transcript_stats` | Corpus statistics: document count, index size, per-account file counts. |

## Configuration

Auto-detection works out of the box for most setups. Override via environment variables for custom configurations.

| Env var | Default | Description |
|---|---|---|
| `TRANSCRIPT_SEARCH_ROOTS` | auto-detect `~/.claude` + `~/.claude-*` | Claude Code account roots. Format: `name=/path,name2=/path2` |
| `TRANSCRIPT_SEARCH_CODEX_ROOT` | `~/.codex` if it exists | Codex CLI data directory |
| `TRANSCRIPT_SEARCH_INDEX_PATH` | `~/.claude-shared/transcript-index` or `~/.local/share/transcript-search/index` | Where the tantivy index is stored |

### Auto-detection

By default, the server:
1. Always includes `~/.claude` as the `claude` account
2. Scans `~/` for any `~/.claude-*` directories that contain a `projects/` subdirectory (multi-account setups)
3. Includes `~/.codex` if `~/.codex/sessions/` exists

### Multi-account example

```json
{
  "mcpServers": {
    "transcript-search": {
      "command": "/path/to/transcript-search",
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
- **Incremental reindex** is fast — skips unchanged files by mtime/size. Call `transcript_reindex` to pick up new sessions.
- **Logs go to stderr** (stdout is MCP transport). Set `RUST_LOG=transcript_search=debug` for verbose output.
- **Add a CLAUDE.md cue** so Claude knows to use these tools:
  ```markdown
  ### Transcript search
  When the user asks about past conversations, use the `transcript-search` MCP tools.
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
- **No LLM calls** — pure local indexing and retrieval. `transcript_topics` uses term frequency, not embeddings.

## License

MIT
