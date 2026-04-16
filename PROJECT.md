## Project

**Blackbox** — MCP server that indexes Claude Code, Codex CLI, Copilot, Vibe, and Gemini transcripts into a tantivy full-text search index, manages a unified knowledge store across providers, tracks long-running work threads, and orchestrates multi-provider agent execution.

The crate is `blackbox` (`Cargo.toml`); it produces two binaries:
- `blackboxd` — the MCP server daemon (`src/main.rs`)
- `bro` — terminal client for tailing live orchestration events (`src/cli.rs`)

MCP tools are prefixed `bbox_*` (transcript/knowledge/threads) and `bro_*` (orchestration).

## Build & Run

```bash
cargo build --release    # release binaries at target/release/{blackboxd,bro}
cargo build              # debug build
cargo test               # unit tests (72 tests, ~0.1s)
cargo clippy             # lint
```

`blackboxd` communicates via JSON-RPC 2.0 over stdin/stdout. Stderr is reserved for tracing logs — never write to stdout except MCP responses.

## Architecture

Source layout (`src/`):

- **main.rs** — JSON-RPC message loop, MCP tool dispatcher (`#[tool]`-annotated handlers), signal handling (SIGUSR1 triggers reindex), HTTP `/tail` SSE endpoint for `bro tail`.
- **cli.rs** — `bro` binary. Connects to `blackboxd`'s `/tail` endpoint and renders colorized live task events.
- **index/** — Tantivy index lifecycle, schema (account, project, role, session_id, content, timestamps, git_branch, agent_slug, is_subagent, cwd), search/browse handlers, incremental reindex thread.
- **parser.rs** — Multi-format transcript parsing: Claude Code (`message.content` array), Codex CLI (`payload.content`), history.jsonl (`display`). Extracts roles, tool use/results, thinking blocks. Caps content at 12KB per document.
- **knowledge.rs** — Knowledge entry CRUD (`~/.claude-shared/blackbox-knowledge.json`). Render pipeline emits provider-specific markdown (CLAUDE.md, AGENTS.md, GEMINI.md) with three layers: steerage → shared memory → PROJECT.md. Git-based absorption imports external edits to rendered files as unverified entries.
- **threads.rs** — Work thread tracker for non-dispatchable, multi-session efforts. Friendly names with rename support; typed graph edges between threads/sessions.
- **orchestration/** — Multi-provider agent dispatch (Claude, Codex, Copilot, Vibe, Gemini). Provider catalogs (models, effort tiers), exec/resume arg builders, brofile/team management, task lifecycle, tail event stream.
- **render.rs** — Markdown emitter shared by knowledge render and other tooling.

## Provider Catalog

Maintained in `src/orchestration/providers.rs`:

- **Claude**: Opus 4.7 (default, 1M context built-in), Opus 4.6 [1m]/200K, Sonnet 4.6, Haiku 4.5. Effort tiers `low`/`medium`/`high`/`xhigh`/`max` (xhigh default, Opus-4.7-only; max unsupported on Haiku).
- **Codex**: gpt-5.4 family. Effort tiers `minimal`/`low`/`medium`/`high`/`xhigh`.
- **Copilot**: tracks Anthropic + OpenAI models. Effort tiers `low`/`medium`/`high`/`xhigh`.
- **Vibe**, **Gemini**: model lists only, no effort tier.

## Key Design Decisions

- **One tantivy doc per content block** — enables role-based filtering and precise excerpt generation rather than one doc per session.
- **Field-based filtering** — account, project, role, subagent, branch, cwd filters all happen at the tantivy query level, no post-filtering.
- **Response cap** — 80KB max MCP response. 12KB max per indexed document.
- **Multi-account auto-detection** — scans `~/` for `.claude-*` dirs with `projects/` subdirs; always includes `~/.claude` as `claude` account; includes `~/.codex` if sessions dir exists.
- **Tokio runtime** — async only where needed (MCP transport, HTTP `/tail`, orchestration child processes); synchronous I/O for tantivy and JSON storage.

## Environment Variables

- `TRANSCRIPT_SEARCH_ROOTS` — override account roots (`name=/path,name2=/path2`)
- `TRANSCRIPT_SEARCH_CODEX_ROOT` — override Codex data dir
- `TRANSCRIPT_SEARCH_INDEX_PATH` — override tantivy index location
- `BLACKBOX_REINDEX_INTERVAL_SECS` — background reindex interval (default: 120)
- `BBOX_PORT` / `BRO_PORT` — HTTP port for `/tail` endpoint (default: 7263)
- `CLAUDE_BIN` / `CODEX_BIN` / `COPILOT_BIN` / `GEMINI_BIN` — override provider binary paths
- `RUST_LOG` — tracing filter (default: `transcript_search=info`)

## Design Docs

- `design/knowledge-store.md` — knowledge store v2: layer architecture, absorption, entry schema, rendering pipeline, migration path.
