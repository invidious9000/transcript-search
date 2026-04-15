# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

**Blackbox** — an MCP server that indexes Claude Code and Codex CLI conversation transcripts into a tantivy full-text search index and manages a unified knowledge store across multiple AI providers. Binary name is `transcript-search`; all MCP tools are prefixed `blackbox_`.

## Build & Run

```bash
cargo build --release    # release binary at target/release/transcript-search
cargo build              # debug build
cargo test               # unit tests
cargo clippy             # lint
```

Installed as an MCP server — communicates via JSON-RPC 2.0 over stdin/stdout. Stderr is reserved for tracing logs. Never write to stdout except MCP responses.

## Architecture

Four source files in `src/`:

- **main.rs** — JSON-RPC message loop, MCP tool dispatcher (routes `tools/call` to handler functions), signal handling (SIGUSR1 triggers reindex). Reads stdin line-by-line, dispatches synchronously.

- **index.rs** — Tantivy index lifecycle. Defines the schema (account, project, role, session_id, content, timestamps, git_branch, agent_slug, is_subagent, cwd, etc.). Implements all search/browse tools: `blackbox_search`, `blackbox_context`, `blackbox_messages`, `blackbox_session`, `blackbox_sessions_list`, `blackbox_topics`, `blackbox_stats`. Runs a background reindex thread (Arc<Mutex<>>, configurable via `BLACKBOX_REINDEX_INTERVAL_SECS`, default 120s). Incremental reindexing tracks file mtime/size in `_meta.json`.

- **parser.rs** — Multi-format transcript parsing. Handles Claude Code format (`message.content` array), Codex CLI format (`payload.content`), and history.jsonl (`display` field). Extracts role, content blocks, tool use/results, thinking blocks. Content truncated at 12KB per document.

- **knowledge.rs** — Knowledge entry CRUD stored in `~/.claude-shared/blackbox-knowledge.json`. Entry schema: category, scope (global/project), providers, priority, status, approval, variants, expiry, decay. Rendering pipeline produces provider-specific markdown files (CLAUDE.md, AGENTS.md, GEMINI.md) with three layers: steerage → shared memory → PROJECT.md content. Git-based absorption detects external edits to rendered files and imports them as unverified entries.

## Key Design Decisions

- **No async** — synchronous I/O throughout; acceptable for MCP request/response model.
- **One tantivy doc per content block** — enables role-based filtering and precise excerpt generation rather than one doc per session.
- **Field-based filtering** — account, project, role, subagent, branch, cwd filters all happen at the tantivy query level, no post-filtering.
- **Response cap** — 80KB max MCP response. 12KB max per indexed document.
- **Multi-account auto-detection** — scans `~/` for `.claude-*` dirs with `projects/` subdirs; always includes `~/.claude` as `claude` account; includes `~/.codex` if sessions dir exists.

## Environment Variables

- `TRANSCRIPT_SEARCH_ROOTS` — override account roots (`name=/path,name2=/path2`)
- `TRANSCRIPT_SEARCH_CODEX_ROOT` — override Codex data dir
- `TRANSCRIPT_SEARCH_INDEX_PATH` — override tantivy index location
- `BLACKBOX_REINDEX_INTERVAL_SECS` — background reindex interval (default: 120)
- `RUST_LOG` — tracing filter (default: `transcript_search=info`)

## Design Docs

`design/knowledge-store.md` — comprehensive design doc for the knowledge store (v2): layer architecture, absorption mechanism, entry schema, rendering pipeline, migration path.
