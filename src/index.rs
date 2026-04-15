use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::*;
use tantivy::snippet::SnippetGenerator;
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use walkdir::WalkDir;

use crate::parser;

/// Metadata about an indexed file, for incremental updates.
#[derive(Serialize, Deserialize)]
struct FileMeta {
    mtime: u64,
    size: u64,
}

/// Field handles extracted for sharing with the background reindex thread.
/// All fields are `Copy` — they're just integer indices into the schema.
#[derive(Clone, Copy)]
pub struct FieldHandles {
    pub content: Field,
    pub session_id: Field,
    pub account: Field,
    pub project: Field,
    pub role: Field,
    pub timestamp: Field,
    pub file_path: Field,
    pub byte_offset: Field,
    pub git_branch: Field,
    pub is_subagent: Field,
    pub agent_slug: Field,
}

/// Config needed by the background reindex thread.
#[derive(Clone)]
pub struct ReindexConfig {
    pub roots: Vec<(String, PathBuf)>,
    pub codex_root: Option<PathBuf>,
    pub meta_path: PathBuf,
}

pub struct TranscriptIndex {
    index: Index,
    reader: IndexReader,
    #[allow(dead_code)]
    schema: Schema,
    fields: FieldHandles,
    config: ReindexConfig,
}

impl TranscriptIndex {
    pub fn open_or_create(index_path: &Path, roots: Vec<(String, PathBuf)>, codex_root: Option<PathBuf>) -> Result<Self> {
        let meta_path = index_path.join("_meta.json");

        // Build schema
        let mut builder = Schema::builder();
        let fields = FieldHandles {
            content: builder.add_text_field("content", TEXT | STORED),
            session_id: builder.add_text_field("session_id", STRING | STORED),
            account: builder.add_text_field("account", STRING | STORED),
            project: builder.add_text_field("project", TEXT | STORED),
            role: builder.add_text_field("role", STRING | STORED),
            timestamp: builder.add_text_field("timestamp", STRING | STORED),
            file_path: builder.add_text_field("file_path", STRING | STORED),
            byte_offset: builder.add_u64_field("byte_offset", STORED),
            git_branch: builder.add_text_field("git_branch", STRING | STORED),
            is_subagent: builder.add_u64_field("is_subagent", INDEXED | STORED),
            agent_slug: builder.add_text_field("agent_slug", STRING | STORED),
        };
        let schema = builder.build();

        fs::create_dir_all(index_path)?;

        // Try opening existing index, fall back to creating new
        let index = match Index::open_in_dir(index_path) {
            Ok(idx) => {
                tracing::info!("Opened existing index at {}", index_path.display());
                idx
            }
            Err(_) => {
                tracing::info!("Creating new index at {}", index_path.display());
                Index::create_in_dir(index_path, schema.clone())?
            }
        };

        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;

        let config = ReindexConfig {
            roots,
            codex_root,
            meta_path,
        };

        Ok(Self { index, reader, schema, fields, config })
    }

    /// Get a clone of the Index handle for the background thread.
    pub fn index_handle(&self) -> Index {
        self.index.clone()
    }

    /// Get the field handles for the background thread.
    pub fn field_handles(&self) -> FieldHandles {
        self.fields
    }

    /// Get the reindex config for the background thread.
    pub fn reindex_config(&self) -> ReindexConfig {
        self.config.clone()
    }

    pub fn is_empty(&self) -> bool {
        let searcher = self.reader.searcher();
        searcher.num_docs() == 0
    }

    // ── Search ──────────────────────────────────────────────────────

    pub fn search(&self, args: &Value) -> Result<String> {
        let query_str = args["query"]
            .as_str()
            .context("'query' is required")?;
        let limit = args["limit"].as_u64().unwrap_or(20).min(100) as usize;
        let include_subagents = args["include_subagents"].as_bool().unwrap_or(true);

        if self.is_empty() {
            return Ok("Index is empty. Run blackbox_reindex first.".to_string());
        }

        let searcher = self.reader.searcher();

        // Parse the user's text query against content + project fields
        let mut qp = QueryParser::for_index(&self.index, vec![self.fields.content, self.fields.project]);
        qp.set_conjunction_by_default();
        let text_query = qp.parse_query(query_str)?;

        // Build filter clauses
        let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![
            (Occur::Must, text_query.box_clone()),
        ];

        if !include_subagents {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_u64(self.fields.is_subagent, 0),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(account) = args["account"].as_str() {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.account, account),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(role) = args["role"].as_str() {
            clauses.push((
                Occur::Must,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.role, role),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        if let Some(project) = args["project"].as_str() {
            // Project filter: parse as a query against the project field only
            let mut pqp = QueryParser::for_index(&self.index, vec![self.fields.project]);
            pqp.set_conjunction_by_default();
            if let Ok(pq) = pqp.parse_query(project) {
                clauses.push((Occur::Must, pq));
            }
        }

        // Auto-exclude the caller's own session by detecting which active session
        // contains the search query as a recent user message (self-reference).
        if let Some(caller_sid) = detect_caller_session(&self.config, query_str) {
            clauses.push((
                Occur::MustNot,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.session_id, &caller_sid),
                    IndexRecordOption::Basic,
                )),
            ));
        }

        let query = BooleanQuery::new(clauses);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(limit))?;

        if top_docs.is_empty() {
            return Ok("No results found.".to_string());
        }

        // Snippet generator for excerpt highlighting
        let snippet_gen = SnippetGenerator::create(&searcher, &*text_query, self.fields.content)?;

        let mut results = Vec::new();
        for (score, addr) in &top_docs {
            let doc: TantivyDocument = searcher.doc(*addr)?;
            let snippet = snippet_gen.snippet_from_doc(&doc);

            let file_path = self.doc_text(&doc, self.fields.file_path);
            let session_id = self.doc_text(&doc, self.fields.session_id);
            let role = self.doc_text(&doc, self.fields.role);
            let ts = self.doc_text(&doc, self.fields.timestamp);
            let project = self.doc_text(&doc, self.fields.project);
            let account = self.doc_text(&doc, self.fields.account);

            let excerpt = snippet
                .to_html()
                .replace("<b>", "**")
                .replace("</b>", "**");

            results.push(format!(
                "Score: {score:.2} | {account} | {role}\n\
                 Session: {session_id}\n\
                 Project: {project}\n\
                 Time: {ts}\n\
                 File: {file_path}\n\
                 Excerpt: {excerpt}"
            ));
        }

        Ok(format!(
            "{} results:\n\n{}",
            results.len(),
            results.join("\n\n---\n\n")
        ))
    }

    // ── Context ─────────────────────────────────────────────────────

    pub fn context(&self, args: &Value) -> Result<String> {
        let file_path = args["file_path"]
            .as_str()
            .context("'file_path' is required")?;
        let target_offset = args["byte_offset"]
            .as_u64()
            .context("'byte_offset' is required")?;
        let ctx_lines = args["context_lines"].as_u64().unwrap_or(5) as usize;

        let content = fs::read_to_string(file_path)
            .with_context(|| format!("Failed to read {}", file_path))?;

        let lines: Vec<&str> = content.split('\n').collect();

        // Find the line containing target_offset
        let mut offset = 0u64;
        let mut target_idx = 0usize;
        for (i, line) in lines.iter().enumerate() {
            if offset >= target_offset {
                target_idx = i;
                break;
            }
            offset += line.len() as u64 + 1;
        }

        let start = target_idx.saturating_sub(ctx_lines);
        let end = (target_idx + ctx_lines + 1).min(lines.len());

        let is_codex = file_path.contains("/.codex/");
        let codex_sid = if is_codex {
            extract_codex_session_id(Path::new(file_path))
        } else {
            String::new()
        };

        let mut output = Vec::new();
        for i in start..end {
            let events = if is_codex {
                parser::parse_codex_line(lines[i], &codex_sid)
            } else {
                parser::parse_transcript_line(lines[i])
            };
            if events.is_empty() {
                continue;
            }
            for ev in &events {
                let marker = if i == target_idx { ">>>" } else { "   " };
                let preview = if ev.content.len() > 400 {
                    format!("{}...", &ev.content[..400])
                } else {
                    ev.content.clone()
                };
                output.push(format!("{} [{}] {}", marker, ev.role, preview));
            }
        }

        if output.is_empty() {
            Ok("No parseable events in the requested range.".to_string())
        } else {
            Ok(output.join("\n\n"))
        }
    }

    // ── Session ─────────────────────────────────────────────────────

    pub fn session(&self, args: &Value) -> Result<String> {
        let raw_id = args["session_id"]
            .as_str()
            .context("'session_id' is required")?;

        // If it's a friendly name, resolve to UUID
        let resolved_id = resolve_session_name(
            raw_id,
            &self.config.roots,
            self.config.codex_root.as_ref(),
        );
        let session_id = resolved_id.as_deref().unwrap_or(raw_id);

        // Load name maps for display
        let claude_names = load_claude_session_names(&self.config.roots);
        let codex_names = load_codex_session_names(self.config.codex_root.as_ref());
        let name = claude_names
            .get(session_id)
            .or_else(|| codex_names.get(session_id))
            .cloned()
            .unwrap_or_default();
        let name_line = if name.is_empty() {
            String::new()
        } else {
            format!("Name: {name}\n")
        };

        // Try session-meta JSON files first
        for (account_name, root) in &self.config.roots {
            let meta_file = root
                .join("usage-data")
                .join("session-meta")
                .join(format!("{}.json", session_id));
            if meta_file.exists() {
                let raw = fs::read_to_string(&meta_file)?;
                let v: Value = serde_json::from_str(&raw)?;
                let project = v["project_path"].as_str().unwrap_or("?");
                let duration = v["duration_minutes"].as_u64().unwrap_or(0);
                let user_msgs = v["user_message_count"].as_u64().unwrap_or(0);
                let asst_msgs = v["assistant_message_count"].as_u64().unwrap_or(0);
                let first_prompt = v["first_prompt"].as_str().unwrap_or("?");
                let tools = &v["tool_counts"];

                return Ok(format!(
                    "Session: {session_id}\n\
                     {name_line}\
                     Account: {account_name}\n\
                     Project: {project}\n\
                     Duration: {duration} min\n\
                     Messages: {user_msgs} user, {asst_msgs} assistant\n\
                     Tools: {tools}\n\
                     First prompt: {first_prompt}"
                ));
            }
        }

        // Fallback: search index for this session
        if self.is_empty() {
            return Ok("Index empty and no session-meta found.".to_string());
        }

        let searcher = self.reader.searcher();
        let query = TermQuery::new(
            Term::from_field_text(self.fields.session_id, session_id),
            IndexRecordOption::Basic,
        );
        let top = searcher.search(&query, &TopDocs::with_limit(1))?;
        if let Some((_score, addr)) = top.first() {
            let doc: TantivyDocument = searcher.doc(*addr)?;
            let project = self.doc_text(&doc, self.fields.project);
            let account = self.doc_text(&doc, self.fields.account);
            let file_path = self.doc_text(&doc, self.fields.file_path);
            Ok(format!(
                "Session: {session_id}\n\
                 {name_line}\
                 Account: {account}\n\
                 Project: {project}\n\
                 File: {file_path}\n\
                 (No session-meta available — limited info from index)"
            ))
        } else {
            Ok(format!("Session {} not found.", session_id))
        }
    }

    // ── Messages ────────────────────────────────────────────────────

    pub fn messages(&self, args: &Value) -> Result<String> {
        let role_filter = args["role"].as_str();
        let include_subagents = args["include_subagents"].as_bool().unwrap_or(false);
        let max_length = args["max_content_length"].as_u64().unwrap_or(500) as usize;
        let from_end = args["from_end"].as_bool().unwrap_or(false);
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(50).min(200) as usize;

        // Resolve to file path(s) — accept either file_path or session_id
        let files: Vec<String> = if let Some(fp) = args["file_path"].as_str() {
            vec![fp.to_string()]
        } else if let Some(sid) = args["session_id"].as_str() {
            self.resolve_session_files(sid)?
        } else {
            anyhow::bail!("Either 'session_id' or 'file_path' is required");
        };

        if files.is_empty() {
            return Ok("Session not found.".to_string());
        }

        // Collect all matching messages first (for accurate count + pagination)
        let mut all_messages: Vec<String> = Vec::new();
        let mut file_labels: Vec<(usize, String)> = Vec::new(); // (insert_before_index, label)

        for file_path in &files {
            let is_subagent_file = file_path.contains("/subagents/");
            if is_subagent_file && !include_subagents {
                continue;
            }

            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(e) => {
                    all_messages.push(format!("[Error reading {}: {}]", file_path, e));
                    continue;
                }
            };

            if files.len() > 1 && include_subagents {
                let label = if is_subagent_file {
                    let name = Path::new(file_path)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    format!("=== Subagent: {} ===", name)
                } else {
                    format!("=== Main transcript ===")
                };
                file_labels.push((all_messages.len(), label));
            }

            let is_codex = file_path.contains("/.codex/");
            let codex_sid = if is_codex {
                extract_codex_session_id(Path::new(file_path))
            } else {
                String::new()
            };

            for line in content.lines() {
                let events = if is_codex {
                    parser::parse_codex_line(line, &codex_sid)
                } else {
                    parser::parse_transcript_line(line)
                };
                for ev in &events {
                    if let Some(rf) = role_filter {
                        if ev.role != rf {
                            continue;
                        }
                    }

                    let preview = if max_length == 0 {
                        ev.content.clone()
                    } else if ev.content.len() > max_length {
                        let mut end = max_length;
                        while end > 0 && !ev.content.is_char_boundary(end) {
                            end -= 1;
                        }
                        format!("{}...", &ev.content[..end])
                    } else {
                        ev.content.clone()
                    };

                    let ts = ev.timestamp.as_deref().unwrap_or("");
                    all_messages.push(format!("[{}] [{}] {}", ts, ev.role, preview));
                }
            }
        }

        let total = all_messages.len();
        if total == 0 {
            return Ok("No messages found matching filters.".to_string());
        }

        // Apply pagination — from_end reverses so offset 0 = last messages
        let (page, showing_start, showing_end): (Vec<&String>, usize, usize) = if from_end {
            let tail_start = total.saturating_sub(offset + limit);
            let tail_end = total.saturating_sub(offset);
            let p: Vec<&String> = all_messages[tail_start..tail_end].iter().collect();
            (p, tail_start, tail_end)
        } else {
            let p: Vec<&String> = all_messages.iter().skip(offset).take(limit).collect();
            let end = (offset + limit).min(total);
            (p, offset, end)
        };

        let mut header = format!(
            "Messages {}-{} of {} total",
            showing_start + 1,
            showing_end,
            total
        );
        if !from_end && showing_end < total {
            header.push_str(&format!(" (next page: offset={})", showing_end));
        }
        if from_end && showing_start > 0 {
            header.push_str(&format!(
                " (earlier: from_end=true, offset={})",
                offset + limit
            ));
        }

        // Assemble body with size cap (80KB) to avoid blowing MCP result limits
        const MAX_RESPONSE_BYTES: usize = 80_000;
        let mut body = String::new();
        let mut included = 0usize;
        for msg in &page {
            let entry = format!("{}\n\n", msg);
            if body.len() + entry.len() > MAX_RESPONSE_BYTES {
                body.push_str(&format!(
                    "[Response truncated at {} messages — narrow with role filter, smaller limit, or higher max_content_length]\n",
                    included
                ));
                break;
            }
            body.push_str(&entry);
            included += 1;
        }

        Ok(format!("{}\n\n{}", header, body.trim_end()))
    }

    /// Resolve a session ID to its JSONL file path(s) — main transcript + subagents.
    fn resolve_session_files(&self, session_id: &str) -> Result<Vec<String>> {
        // If session_id is a friendly name, resolve to UUID first
        let resolved_id = resolve_session_name(
            session_id,
            &self.config.roots,
            self.config.codex_root.as_ref(),
        );
        let session_id = resolved_id.as_deref().unwrap_or(session_id);

        let mut main_file: Option<String> = None;

        // Strategy 1: index lookup — may return a subagent file, so derive main from it
        if !self.is_empty() {
            let searcher = self.reader.searcher();
            let query = TermQuery::new(
                Term::from_field_text(self.fields.session_id, session_id),
                IndexRecordOption::Basic,
            );
            let top = searcher.search(&query, &TopDocs::with_limit(1))?;
            if let Some((_score, addr)) = top.first() {
                let doc: TantivyDocument = searcher.doc(*addr)?;
                let fp = self.doc_text(&doc, self.fields.file_path);
                if !fp.is_empty() {
                    let derived = Self::derive_main_transcript(&fp, session_id);
                    // Skip monolithic files (e.g. history.jsonl) that contain mixed sessions.
                    // A valid per-session file either has the session_id in its name (Claude)
                    // or follows the codex rollout-*-UUID.jsonl pattern.
                    let stem = Path::new(&derived)
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    if stem.contains(session_id) || stem == session_id {
                        main_file = Some(derived);
                    }
                }
            }
        }

        // Strategy 2: filesystem scan — look for <session-id>.jsonl
        if main_file.is_none() || !Path::new(main_file.as_ref().unwrap()).exists() {
            main_file = None;
            for (_name, root) in &self.config.roots {
                let projects_dir = root.join("projects");
                if !projects_dir.exists() {
                    continue;
                }
                for entry in WalkDir::new(&projects_dir)
                    .follow_links(true)
                    .max_depth(3) // projects/<encoded>/<uuid>.jsonl
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    let p = entry.path();
                    if p.extension().map(|e| e == "jsonl").unwrap_or(false)
                        && p.file_stem().map(|s| s.to_string_lossy()) == Some(session_id.into())
                        && !p.to_string_lossy().contains("/subagents/")
                    {
                        main_file = Some(p.to_string_lossy().to_string());
                        break;
                    }
                }
                if main_file.is_some() {
                    break;
                }
            }
        }

        // Strategy 3: codex sessions — look for rollout-*-<session-id>.jsonl
        if main_file.is_none() || !Path::new(main_file.as_ref().unwrap()).exists() {
            main_file = None;
            if let Some(ref codex_root) = self.config.codex_root {
                let sessions_dir = codex_root.join("sessions");
                if sessions_dir.exists() {
                    for entry in WalkDir::new(&sessions_dir)
                        .follow_links(true)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        let p = entry.path();
                        if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                            let extracted = extract_codex_session_id(p);
                            if extracted == session_id {
                                main_file = Some(p.to_string_lossy().to_string());
                                break;
                            }
                        }
                    }
                }
            }
        }

        let main = match main_file {
            Some(ref f) if Path::new(f).exists() => f.clone(),
            _ => return Ok(vec![]),
        };

        let mut files = vec![main.clone()];

        // Check for subagent directory alongside the main file
        // Main: .../projects/<proj>/<session-id>.jsonl
        // Subs: .../projects/<proj>/<session-id>/subagents/agent-*.jsonl
        let main_path = Path::new(&main);
        if let Some(stem) = main_path.file_stem() {
            if let Some(parent) = main_path.parent() {
                let subagent_dir = parent.join(stem.to_string_lossy().as_ref()).join("subagents");
                if subagent_dir.exists() {
                    for entry in WalkDir::new(&subagent_dir)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        let p = entry.path();
                        if p.extension().map(|e| e == "jsonl").unwrap_or(false) {
                            files.push(p.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }

        Ok(files)
    }

    /// Given a file path (possibly a subagent file) and a session ID, derive the main transcript path.
    /// Subagent: .../projects/<proj>/<session-id>/subagents/agent-xxx.jsonl
    /// Main:     .../projects/<proj>/<session-id>.jsonl
    fn derive_main_transcript(file_path: &str, session_id: &str) -> String {
        if !file_path.contains("/subagents/") {
            return file_path.to_string();
        }
        // Walk up from subagent path to find the session dir, then look for <session-id>.jsonl beside it
        let p = Path::new(file_path);
        let mut current = p.parent(); // agent file's dir (subagents/)
        while let Some(dir) = current {
            if dir.file_name().map(|n| n.to_string_lossy()) == Some(session_id.into()) {
                // Found .../projects/<proj>/<session-id>/
                // Main transcript is .../projects/<proj>/<session-id>.jsonl
                let main = dir.with_extension("jsonl");
                return main.to_string_lossy().to_string();
            }
            current = dir.parent();
        }
        file_path.to_string()
    }

    // ── Topics ──────────────────────────────────────────────────────

    pub fn topics(&self, args: &Value) -> Result<String> {
        let top_n = args["limit"].as_u64().unwrap_or(25) as usize;
        let role_filter = args["role"].as_str();

        // Resolve session docs via session_id or file_path
        let session_id: Option<&str> = args["session_id"].as_str();
        let file_path: Option<&str> = args["file_path"].as_str();

        if session_id.is_none() && file_path.is_none() {
            anyhow::bail!("Either 'session_id' or 'file_path' is required");
        }

        // Collect content from the session
        let mut all_content = String::new();

        if let Some(fp) = file_path {
            // Read directly from file
            let content = fs::read_to_string(fp)?;
            let is_codex = fp.contains("/.codex/");
            let codex_sid = if is_codex {
                extract_codex_session_id(Path::new(fp))
            } else {
                String::new()
            };
            for line in content.lines() {
                let events = if is_codex {
                    parser::parse_codex_line(line, &codex_sid)
                } else {
                    parser::parse_transcript_line(line)
                };
                for ev in &events {
                    if let Some(rf) = role_filter {
                        if ev.role != rf { continue; }
                    }
                    // Skip tool_result — too noisy for topic extraction
                    if ev.role == "tool_result" { continue; }
                    all_content.push(' ');
                    all_content.push_str(&ev.content);
                }
            }
        } else if let Some(sid) = session_id {
            // Use index to find all docs for this session
            if self.is_empty() {
                return Ok("Index is empty. Run blackbox_reindex first.".to_string());
            }
            let searcher = self.reader.searcher();
            let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![
                (Occur::Must, Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.session_id, sid),
                    IndexRecordOption::Basic,
                ))),
            ];
            if let Some(rf) = role_filter {
                clauses.push((Occur::Must, Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.role, rf),
                    IndexRecordOption::Basic,
                ))));
            }
            // Exclude tool_result by default
            if role_filter.is_none() {
                clauses.push((Occur::MustNot, Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.role, "tool_result"),
                    IndexRecordOption::Basic,
                ))));
            }
            let query = BooleanQuery::new(clauses);
            let top_docs = searcher.search(&query, &TopDocs::with_limit(5000))?;
            for (_score, addr) in &top_docs {
                let doc: TantivyDocument = searcher.doc(*addr)?;
                let content = self.doc_text(&doc, self.fields.content);
                all_content.push(' ');
                all_content.push_str(&content);
            }
        }

        if all_content.is_empty() {
            return Ok("No content found for this session.".to_string());
        }

        // Tokenize and count
        let mut counts: HashMap<String, u32> = HashMap::new();
        for word in all_content.split(|c: char| !c.is_alphanumeric() && c != '_') {
            let w = word.to_lowercase();
            if w.len() < 3 || is_stop_word(&w) {
                continue;
            }
            *counts.entry(w).or_insert(0) += 1;
        }

        let mut sorted: Vec<(String, u32)> = counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        sorted.truncate(top_n);

        let lines: Vec<String> = sorted
            .iter()
            .map(|(word, count)| format!("{:>4}  {}", count, word))
            .collect();

        Ok(format!("Top {} terms:\n{}", sorted.len(), lines.join("\n")))
    }

    // ── Sessions List ───────────────────────────────────────────────

    pub fn sessions_list(&self, args: &Value) -> Result<String> {
        let account_filter = args["account"].as_str();
        let project_filter = args["project"].as_str();
        let name_filter = args["name"].as_str();
        let limit = args["limit"].as_u64().unwrap_or(30).min(100) as usize;
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;

        // Load session name maps
        let claude_names = load_claude_session_names(&self.config.roots);
        let codex_names = load_codex_session_names(self.config.codex_root.as_ref());

        let mut entries: Vec<SessionEntry> = Vec::new();

        // Claude Code sessions — from session-meta JSON files
        for (account_name, root) in &self.config.roots {
            if let Some(af) = account_filter {
                if af != account_name { continue; }
            }
            let meta_dir = root.join("usage-data").join("session-meta");
            if !meta_dir.exists() { continue; }

            let dir_entries = match fs::read_dir(&meta_dir) {
                Ok(d) => d,
                Err(_) => continue,
            };

            for entry in dir_entries {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let path = entry.path();
                if path.extension().map(|e| e != "json").unwrap_or(true) {
                    continue;
                }

                let raw = match fs::read_to_string(&path) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let v: Value = match serde_json::from_str(&raw) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let project = v["project_path"].as_str().unwrap_or("").to_string();
                if let Some(pf) = project_filter {
                    if !project.to_lowercase().contains(&pf.to_lowercase()) {
                        continue;
                    }
                }

                let start = v["start_time"].as_str().unwrap_or("").to_string();
                let first_prompt = v["first_prompt"].as_str().unwrap_or("").to_string();
                // Truncate first_prompt for display
                let prompt_preview = if first_prompt.len() > 120 {
                    let mut end = 120;
                    while end > 0 && !first_prompt.is_char_boundary(end) { end -= 1; }
                    format!("{}...", &first_prompt[..end])
                } else {
                    first_prompt
                };

                let sid = v["session_id"].as_str().unwrap_or("").to_string();

                let name = claude_names.get(&sid).cloned().unwrap_or_default();

                if let Some(nf) = name_filter {
                    if !name.to_lowercase().contains(&nf.to_lowercase()) {
                        continue;
                    }
                }

                entries.push(SessionEntry {
                    session_id: sid,
                    account: account_name.clone(),
                    project: shorten_project(&project),
                    start_time: start,
                    duration_minutes: v["duration_minutes"].as_u64().unwrap_or(0),
                    user_messages: v["user_message_count"].as_u64().unwrap_or(0),
                    first_prompt: prompt_preview,
                    name,
                });
            }
        }

        // Codex sessions — from session files
        if account_filter.is_none() || account_filter == Some("codex") {
            if let Some(ref codex_root) = self.config.codex_root {
                let sessions_dir = codex_root.join("sessions");
                if sessions_dir.exists() {
                    for entry in WalkDir::new(&sessions_dir)
                        .follow_links(true)
                        .into_iter()
                        .filter_map(|e| e.ok())
                    {
                        let path = entry.path();
                        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
                            continue;
                        }

                        let session_id = extract_codex_session_id(path);

                        let cwd = extract_codex_cwd(path);
                        let project = cwd.as_deref().unwrap_or("");

                        if let Some(pf) = project_filter {
                            if !project.to_lowercase().contains(&pf.to_lowercase()) {
                                continue;
                            }
                        }

                        // Extract timestamp from filename: rollout-YYYY-MM-DDTHH-MM-SS-...
                        let stem = path.file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_default();
                        let start_time = if stem.starts_with("rollout-") && stem.len() > 27 {
                            // rollout-2026-04-12T13-09-35-...
                            let date_part = &stem[8..27]; // 2026-04-12T13-09-35
                            date_part.replace('T', " ").replacen('-', ":", 2)
                        } else {
                            String::new()
                        };

                        // Get first user prompt (read only first ~20 lines)
                        let first_prompt = extract_codex_first_prompt(path);
                        let name = codex_names.get(&session_id).cloned().unwrap_or_default();

                        if let Some(nf) = name_filter {
                            if !name.to_lowercase().contains(&nf.to_lowercase()) {
                                continue;
                            }
                        }

                        entries.push(SessionEntry {
                            session_id,
                            account: "codex".to_string(),
                            project: shorten_project(project),
                            start_time,
                            duration_minutes: 0,
                            user_messages: 0,
                            first_prompt,
                            name,
                        });
                    }
                }
            }
        }

        if entries.is_empty() {
            return Ok("No sessions found.".to_string());
        }

        // Sort by start_time descending (most recent first)
        entries.sort_by(|a, b| b.start_time.cmp(&a.start_time));

        let total = entries.len();
        let page: Vec<&SessionEntry> = entries.iter().skip(offset).take(limit).collect();
        let showing_end = (offset + limit).min(total);

        let mut header = format!("Sessions {}-{} of {} (most recent first)", offset + 1, showing_end, total);
        if showing_end < total {
            header.push_str(&format!(" — next: offset={}", showing_end));
        }

        let mut lines = Vec::new();
        for e in &page {
            let date = if e.start_time.len() >= 16 { &e.start_time[..16] } else { &e.start_time };
            let dur = if e.duration_minutes > 0 {
                format!("{}m", e.duration_minutes)
            } else {
                "-".to_string()
            };
            let name_col = if e.name.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.name)
            };
            lines.push(format!(
                "{} | {:>4} | {:<8} | {:<30} | {}{} | {}",
                date, dur, e.account, e.project, e.session_id, name_col, e.first_prompt
            ));
        }

        Ok(format!("{}\n\n{}", header, lines.join("\n")))
    }

    // ── Stats ───────────────────────────────────────────────────────

    pub fn stats(&self) -> Result<String> {
        let searcher = self.reader.searcher();
        let total_docs = searcher.num_docs();

        // Count JSONL files per root
        let mut per_account: Vec<String> = Vec::new();
        for (name, root) in &self.config.roots {
            let projects_dir = root.join("projects");
            if !projects_dir.exists() {
                per_account.push(format!("  {}: (no projects dir)", name));
                continue;
            }
            let count = WalkDir::new(&projects_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .map(|ext| ext == "jsonl")
                        .unwrap_or(false)
                })
                .count();
            per_account.push(format!("  {}: {} files", name, count));
        }

        if let Some(ref codex_root) = self.config.codex_root {
            let sessions_dir = codex_root.join("sessions");
            if sessions_dir.exists() {
                let count = WalkDir::new(&sessions_dir)
                    .into_iter()
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .map(|ext| ext == "jsonl")
                            .unwrap_or(false)
                    })
                    .count();
                per_account.push(format!("  codex: {} files", count));
            }
        }

        // Index size on disk
        let index_size = dir_size(&self.config.meta_path.parent().unwrap_or(Path::new(".")));

        Ok(format!(
            "Index documents: {}\n\
             Index size: {}\n\
             Source files:\n\
             {}",
            total_docs,
            human_bytes(index_size),
            per_account.join("\n")
        ))
    }

    // ── Reindex ─────────────────────────────────────────────────────

    pub fn reindex(&mut self, args: &Value) -> Result<String> {
        let full = args["full"].as_bool().unwrap_or(false);
        self.build_index(full)
    }

    pub fn build_index(&mut self, full: bool) -> Result<String> {
        let mut writer: IndexWriter = self.index.writer(100_000_000)?;

        let mut meta: HashMap<String, FileMeta> = if !full {
            load_meta(&self.config.meta_path).unwrap_or_default()
        } else {
            HashMap::new()
        };

        if full {
            tracing::info!("Full reindex — clearing existing index");
            writer.delete_all_documents()?;
            writer.commit()?;
        }

        let mut indexed_files = 0u64;
        let mut indexed_docs = 0u64;
        let mut skipped = 0u64;
        let f = self.fields;

        for (account_name, root) in &self.config.roots.clone() {
            let projects_dir = root.join("projects");
            if projects_dir.exists() {
                index_directory_standalone(
                    &projects_dir, account_name, f,
                    &mut writer, &mut meta, &mut indexed_files, &mut indexed_docs, &mut skipped,
                )?;
            }
            let history = root.join("history.jsonl");
            if history.exists() {
                index_history_standalone(
                    &history, account_name, f,
                    &mut writer, &mut meta, &mut indexed_files, &mut indexed_docs, &mut skipped,
                )?;
            }
        }

        if let Some(ref codex_root) = self.config.codex_root.clone() {
            let sessions_dir = codex_root.join("sessions");
            if sessions_dir.exists() {
                index_codex_directory_standalone(
                    &sessions_dir, f,
                    &mut writer, &mut meta, &mut indexed_files, &mut indexed_docs, &mut skipped,
                )?;
            }
            let history = codex_root.join("history.jsonl");
            if history.exists() {
                index_codex_history_standalone(
                    &history, f,
                    &mut writer, &mut meta, &mut indexed_files, &mut indexed_docs, &mut skipped,
                )?;
            }
        }

        // Purge documents for deleted source files
        let current_files = scan_source_files(&self.config);
        let current_paths: std::collections::HashSet<String> =
            current_files.iter().map(|(p, _, _)| p.clone()).collect();
        let mut purged = 0u64;
        let stale_paths: Vec<String> = meta.keys()
            .filter(|p| !current_paths.contains(p.as_str()))
            .cloned()
            .collect();
        for path in &stale_paths {
            let term = Term::from_field_text(f.file_path, path);
            writer.delete_term(term);
            meta.remove(path.as_str());
            purged += 1;
        }

        writer.commit()?;
        self.reader.reload()?;
        save_meta(&self.config.meta_path, &meta)?;

        let msg = if purged > 0 {
            format!(
                "Indexed {} files ({} docs), skipped {} unchanged, purged {} deleted",
                indexed_files, indexed_docs, skipped, purged
            )
        } else {
            format!(
                "Indexed {} files ({} docs), skipped {} unchanged",
                indexed_files, indexed_docs, skipped
            )
        };
        tracing::info!("{}", msg);
        Ok(msg)
    }

    fn doc_text(&self, doc: &TantivyDocument, field: Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|v| match v {
                tantivy::schema::OwnedValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// Extract a human-readable project name from the file path.
/// Claude Code encodes project paths as directory names: `/home/user/repos/foo` → `-home-user-repos-foo`
fn extract_project_from_path(file_path: &Path, projects_root: &Path) -> String {
    let relative = file_path
        .strip_prefix(projects_root)
        .unwrap_or(file_path);

    // First path component is the encoded project dir
    if let Some(first) = relative.components().next() {
        let encoded = first.as_os_str().to_string_lossy();
        // Decode: leading `-` → `/`, internal `-` could be path sep or literal hyphen.
        // Best-effort: replace leading `-` sequences that look like path segments.
        // The encoded form uses `-` for `/` — we can't perfectly disambiguate, but
        // the raw encoded string is still useful for display and filtering.
        return encoded.to_string();
    }

    "unknown".to_string()
}

/// Extract session ID from Codex filename: rollout-YYYY-MM-DDTHH-MM-SS-UUID.jsonl
fn extract_codex_session_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    // Format: rollout-2026-04-12T13-09-35-019d8319-6ffe-78b0-904b-4bfdb2a9cdb5
    // The UUID is the last 5 hyphen-separated groups (36 chars including hyphens)
    // Find the UUID portion by looking for the pattern after the datetime
    if let Some(idx) = stem.find('T') {
        // After T we have HH-MM-SS-UUID, skip past HH-MM-SS- (9 chars)
        let after_t = &stem[idx + 1..];
        if after_t.len() > 9 {
            return after_t[9..].to_string();
        }
    }
    stem
}

/// Extract cwd from the session_meta line in a Codex JSONL file (first line).
fn extract_codex_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().take(5) {
        let line = line.ok()?;
        let v: serde_json::Value = serde_json::from_str(&line).ok()?;
        if v["type"].as_str() == Some("session_meta") {
            return v["payload"]["cwd"].as_str().map(String::from);
        }
    }
    None
}

struct SessionEntry {
    session_id: String,
    account: String,
    project: String,
    start_time: String,
    duration_minutes: u64,
    #[allow(dead_code)]
    user_messages: u64,
    first_prompt: String,
    name: String,
}

/// Extract the first user prompt from a Codex session file (reads first ~30 lines).
fn extract_codex_first_prompt(path: &Path) -> String {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let reader = BufReader::new(file);
    for line in reader.lines().take(30) {
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"].as_str() == Some("response_item")
            && v["payload"]["role"].as_str() == Some("user")
        {
            // Extract text from content blocks
            if let Some(blocks) = v["payload"]["content"].as_array() {
                for block in blocks {
                    if block["type"].as_str() == Some("input_text") {
                        if let Some(text) = block["text"].as_str() {
                            // Skip system/env context blocks
                            if text.starts_with('<') || text.starts_with('#') {
                                continue;
                            }
                            let t = text.trim();
                            if t.len() > 120 {
                                let mut end = 120;
                                while end > 0 && !t.is_char_boundary(end) { end -= 1; }
                                return format!("{}...", &t[..end]);
                            }
                            return t.to_string();
                        }
                    }
                }
            }
        }
    }
    String::new()
}



/// Detect the caller's session by finding the most recently modified transcript
/// whose tail contains the search query in a user message. Provider-agnostic.
fn detect_caller_session(config: &ReindexConfig, query: &str) -> Option<String> {
    let query_lower = query.to_lowercase();
    let now = std::time::SystemTime::now();
    let max_age_secs = 300; // 5-minute window

    // Collect recently-modified JSONL transcripts (max_depth=4 to bound WalkDir)
    let mut recent: Vec<(PathBuf, u64)> = Vec::new();
    let scan = |dir: &Path, out: &mut Vec<(PathBuf, u64)>| {
        for entry in WalkDir::new(dir)
            .follow_links(true)
            .max_depth(4)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if !p.extension().map(|e| e == "jsonl").unwrap_or(false) { continue; }
            if p.to_string_lossy().contains("/subagents/") { continue; }
            if let Ok(meta) = entry.metadata() {
                if let Ok(mtime) = meta.modified() {
                    if let Ok(age) = now.duration_since(mtime) {
                        if age.as_secs() < max_age_secs {
                            out.push((p.to_path_buf(), age.as_secs()));
                        }
                    }
                }
            }
        }
    };

    for (_name, root) in &config.roots {
        let projects_dir = root.join("projects");
        if projects_dir.exists() { scan(&projects_dir, &mut recent); }
    }
    if let Some(ref codex_root) = config.codex_root {
        let sessions_dir = codex_root.join("sessions");
        if sessions_dir.exists() { scan(&sessions_dir, &mut recent); }
    }

    // Most recently modified first
    recent.sort_by_key(|(_p, age)| *age);

    for (path, _age) in &recent {
        // Read tail safely — use read_to_end + lossy conversion to handle mid-char seeks
        use std::io::{Read, Seek, SeekFrom};
        let mut file = match fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        let _ = file.seek(SeekFrom::Start(size.saturating_sub(65536)));
        let mut raw = Vec::new();
        let _ = file.read_to_end(&mut raw);
        let tail = String::from_utf8_lossy(&raw);

        let is_codex = path.to_string_lossy().contains("/.codex/");
        let lines: Vec<&str> = tail.lines().collect();
        let start = lines.len().saturating_sub(50);

        for line in &lines[start..] {
            let v: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Extract user message text — handle both String and Array content
            let user_text: Option<String> = if is_codex {
                if v["type"].as_str() == Some("response_item")
                    && v["payload"]["role"].as_str() == Some("user")
                {
                    parser::extract_tool_result_text(&v["payload"])
                } else {
                    None
                }
            } else {
                if v["type"].as_str() == Some("user") {
                    parser::extract_tool_result_text(&v["message"])
                } else {
                    None
                }
            };

            if let Some(text) = user_text {
                if text.to_lowercase().contains(&query_lower) {
                    if is_codex {
                        return Some(extract_codex_session_id(path));
                    } else if let Some(stem) = path.file_stem() {
                        let s = stem.to_string_lossy().to_string();
                        if looks_like_uuid(&s) {
                            return Some(s);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Build a map of session UUID -> friendly name from Claude session files.
/// Claude stores sessions in ~/.claude/sessions/{pid}.json with { sessionId, name? }.
fn load_claude_session_names(roots: &[(String, PathBuf)]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for (_account, root) in roots {
        let sessions_dir = root.join("sessions");
        if !sessions_dir.exists() {
            continue;
        }
        let entries = match fs::read_dir(&sessions_dir) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e != "json").unwrap_or(true) {
                continue;
            }
            let raw = match fs::read_to_string(&path) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let v: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let (Some(sid), Some(name)) = (v["sessionId"].as_str(), v["name"].as_str()) {
                if !name.is_empty() {
                    map.insert(sid.to_string(), name.to_string());
                }
            }
        }
    }
    map
}

/// Build a map of session UUID -> friendly name from Codex session_index.jsonl.
/// Format: one JSON object per line: { "id": "UUID", "thread_name": "friendly-name" }
/// Append-only — last entry per ID wins (supports renames).
fn load_codex_session_names(codex_root: Option<&PathBuf>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let root = match codex_root {
        Some(r) => r,
        None => return map,
    };
    let index_path = root.join("session_index.jsonl");
    let file = match fs::File::open(&index_path) {
        Ok(f) => f,
        Err(_) => return map,
    };
    let reader = BufReader::new(file);
    for line in reader.lines().flatten() {
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let (Some(id), Some(name)) = (v["id"].as_str(), v["thread_name"].as_str()) {
            if !name.is_empty() {
                map.insert(id.to_string(), name.to_string());
            }
        }
    }
    map
}

/// Resolve a friendly session name to a UUID. Checks both Claude and Codex sources.
/// Returns None if the input already looks like a UUID or no match is found.
fn resolve_session_name(
    name: &str,
    roots: &[(String, PathBuf)],
    codex_root: Option<&PathBuf>,
) -> Option<String> {
    // If it already looks like a UUID, don't resolve
    if looks_like_uuid(name) {
        return None;
    }
    let name_lower = name.to_lowercase();

    // Check Claude sessions
    let claude_names = load_claude_session_names(roots);
    for (sid, n) in &claude_names {
        if n.to_lowercase() == name_lower {
            return Some(sid.clone());
        }
    }

    // Check Codex sessions
    let codex_names = load_codex_session_names(codex_root);
    for (sid, n) in &codex_names {
        if n.to_lowercase() == name_lower {
            return Some(sid.clone());
        }
    }

    None
}

/// Quick check: does this string look like a UUID (8-4-4-4-12 hex)?
fn looks_like_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    s.chars().enumerate().all(|(i, c)| {
        if i == 8 || i == 13 || i == 18 || i == 23 {
            c == '-'
        } else {
            c.is_ascii_hexdigit()
        }
    })
}

/// Shorten a project path for display: /home/user/repos/foo -> foo
fn shorten_project(path: &str) -> String {
    if path.is_empty() { return String::new(); }
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

fn is_stop_word(w: &str) -> bool {
    matches!(w,
        // Determiners, pronouns, prepositions, conjunctions
        "the" | "a" | "an" | "and" | "or" | "but" | "in" | "on" | "at" | "to" | "for"
        | "of" | "with" | "by" | "from" | "as" | "into" | "about" | "between" | "through"
        | "after" | "before" | "above" | "below" | "over" | "under" | "again" | "further"
        | "then" | "once" | "than" | "that" | "this" | "these" | "those"
        | "is" | "are" | "was" | "were" | "be" | "been" | "being"
        | "have" | "has" | "had" | "having" | "do" | "does" | "did" | "doing"
        | "will" | "would" | "shall" | "should" | "may" | "might" | "must" | "can" | "could"
        | "not" | "no" | "nor" | "if" | "so" | "just" | "also" | "very" | "too"
        | "he" | "she" | "it" | "we" | "they" | "you" | "me" | "him" | "her" | "us" | "them"
        | "my" | "your" | "his" | "its" | "our" | "their" | "who" | "whom" | "which" | "what"
        | "all" | "each" | "every" | "both" | "few" | "more" | "most" | "other" | "some" | "any"
        | "such" | "only" | "own" | "same" | "here" | "there" | "when" | "where" | "how" | "why"
        | "out" | "up" | "off" | "down" | "now"
        // Common verbs
        | "get" | "got" | "make" | "made" | "let" | "see" | "look" | "use" | "used" | "using"
        | "need" | "want" | "know" | "think" | "say" | "said" | "come" | "take" | "give"
        | "try" | "run" | "set" | "put" | "keep" | "find" | "way"
        // Code noise
        | "true" | "false" | "null" | "none" | "var" | "const" | "new" | "return" | "import"
        | "export" | "default" | "void" | "string" | "class" | "type" | "self"
        | "src" | "else" | "case" | "break" | "enum" | "struct"
        // Transcript noise
        | "tool" | "result" | "content" | "text" | "message" | "user" | "assistant"
        | "file" | "line" | "error" | "output" | "input" | "command"
    )
}

fn load_meta(path: &Path) -> Result<HashMap<String, FileMeta>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

fn save_meta(path: &Path, meta: &HashMap<String, FileMeta>) -> Result<()> {
    let raw = serde_json::to_string(meta)?;
    let tmp_path = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp_path)?;
    file.write_all(raw.as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn dir_size(path: &Path) -> u64 {
    WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

// ── Background auto-reindex ────────────────────────────────────────

/// Collect (path, mtime, size) for all JSONL files in a directory tree.
fn scan_jsonl_dir(dir: &Path, out: &mut Vec<(String, u64, u64)>) {
    for entry in WalkDir::new(dir).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = match meta.modified() {
            Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            Err(_) => continue,
        };
        out.push((path.to_string_lossy().to_string(), mtime, meta.len()));
    }
}

/// Stat a single file and push if not too recent.
fn scan_single_file(path: &Path, out: &mut Vec<(String, u64, u64)>) {
    if let Ok(meta) = fs::metadata(path) {
        let mtime = meta.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push((path.to_string_lossy().to_string(), mtime, meta.len()));
    }
}

/// Collect (path, mtime, size) for all JSONL files across all roots.
fn scan_source_files(config: &ReindexConfig) -> Vec<(String, u64, u64)> {
    let mut files = Vec::new();
    for (_name, root) in &config.roots {
        let projects_dir = root.join("projects");
        if projects_dir.exists() {
            scan_jsonl_dir(&projects_dir, &mut files);
        }
        let history = root.join("history.jsonl");
        if history.exists() {
            scan_single_file(&history, &mut files);
        }
    }

    if let Some(ref codex_root) = config.codex_root {
        let sessions_dir = codex_root.join("sessions");
        if sessions_dir.exists() {
            scan_jsonl_dir(&sessions_dir, &mut files);
        }
        let history = codex_root.join("history.jsonl");
        if history.exists() {
            scan_single_file(&history, &mut files);
        }
    }

    files
}

/// Check if any source files have changed since last index.
/// Returns true if reindexing is needed (cheap — stat only, no I/O on file contents).
fn needs_reindex(config: &ReindexConfig) -> bool {
    let meta = load_meta(&config.meta_path).unwrap_or_default();
    let files = scan_source_files(config);
    let current_paths: std::collections::HashSet<&str> =
        files.iter().map(|(p, _, _)| p.as_str()).collect();
    // Check for new or changed files
    for (path, mtime, size) in &files {
        match meta.get(path.as_str()) {
            Some(prev) if prev.mtime == *mtime && prev.size == *size => continue,
            _ => return true,
        }
    }
    // Check for deleted files (in meta but not on disk)
    for path in meta.keys() {
        if !current_paths.contains(path.as_str()) {
            return true;
        }
    }
    false
}

/// Background reindex: speculative scan → try-lock → reload meta → index → commit.
/// Returns Ok(()) even when skipped (lock busy, no changes). Errors only on real failures.
pub fn try_background_reindex(
    index: &Index,
    config: &ReindexConfig,
    fields: FieldHandles,
) -> Result<()> {
    // 1. Speculative scan — cheap, no writer allocation
    if !needs_reindex(config) {
        tracing::debug!("auto-reindex: no changes detected");
        return Ok(());
    }

    // 2. Acquire writer — returns LockBusy immediately if another process holds it
    let mut writer: IndexWriter = match index.writer(100_000_000) {
        Ok(w) => w,
        Err(tantivy::TantivyError::LockFailure(_, _)) => {
            tracing::debug!("auto-reindex: writer lock busy, skipping");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    // 3. Reload meta AFTER acquiring lock (another process may have committed)
    let mut meta = load_meta(&config.meta_path).unwrap_or_default();

    // 4. Index changed files
    let mut indexed_files = 0u64;
    let mut indexed_docs = 0u64;
    let mut skipped = 0u64;

    for (account_name, root) in &config.roots {
        let projects_dir = root.join("projects");
        if projects_dir.exists() {
            index_directory_standalone(
                &projects_dir, account_name, fields,
                &mut writer, &mut meta, &mut indexed_files, &mut indexed_docs, &mut skipped,
            )?;
        }
        let history = root.join("history.jsonl");
        if history.exists() {
            index_history_standalone(
                &history, account_name, fields,
                &mut writer, &mut meta, &mut indexed_files, &mut indexed_docs, &mut skipped,
            )?;
        }
    }

    if let Some(ref codex_root) = config.codex_root {
        let sessions_dir = codex_root.join("sessions");
        if sessions_dir.exists() {
            index_codex_directory_standalone(
                &sessions_dir, fields,
                &mut writer, &mut meta, &mut indexed_files, &mut indexed_docs, &mut skipped,
            )?;
        }
        let history = codex_root.join("history.jsonl");
        if history.exists() {
            index_codex_history_standalone(
                &history, fields,
                &mut writer, &mut meta, &mut indexed_files, &mut indexed_docs, &mut skipped,
            )?;
        }
    }

    // 4b. Purge documents for deleted source files
    let current_files = scan_source_files(config);
    let current_paths: std::collections::HashSet<String> =
        current_files.iter().map(|(p, _, _)| p.clone()).collect();
    let mut purged = 0u64;
    let stale_paths: Vec<String> = meta.keys()
        .filter(|p| !current_paths.contains(p.as_str()))
        .cloned()
        .collect();
    for path in &stale_paths {
        let term = Term::from_field_text(fields.file_path, path);
        writer.delete_term(term);
        meta.remove(path.as_str());
        purged += 1;
    }

    if indexed_files == 0 && purged == 0 {
        tracing::debug!("auto-reindex: no changes after post-lock re-check");
        return Ok(());
    }

    // 5. Commit + atomic meta save (while still holding writer lock)
    writer.commit()?;
    save_meta(&config.meta_path, &meta)?;

    tracing::info!(
        "auto-reindex: indexed {} files ({} docs), skipped {} unchanged, purged {} deleted",
        indexed_files, indexed_docs, skipped, purged
    );
    Ok(())
}

/// Spawn the background reindex thread. Runs every `interval` seconds.
pub fn spawn_reindex_thread(
    index: Index,
    config: ReindexConfig,
    fields: FieldHandles,
    interval: Duration,
) {
    std::thread::Builder::new()
        .name("blackbox-reindex".into())
        .spawn(move || {
            tracing::info!("background reindex thread started (interval: {:?})", interval);
            // First tick fires after a short delay to let the MCP handshake complete
            std::thread::sleep(Duration::from_secs(5));
            loop {
                if let Err(e) = try_background_reindex(&index, &config, fields) {
                    tracing::error!("background reindex failed: {:#}", e);
                }
                std::thread::sleep(interval);
            }
        })
        .expect("failed to spawn reindex thread");
}

// ── Standalone indexing functions (no &self — usable from background thread) ──

fn event_to_doc_standalone(
    event: &parser::ParsedEvent,
    account: &str,
    file_path: &str,
    byte_offset: u64,
    is_subagent: bool,
    f: FieldHandles,
) -> TantivyDocument {
    let mut doc = TantivyDocument::new();
    doc.add_text(f.content, &event.content);
    doc.add_text(f.session_id, &event.session_id);
    doc.add_text(f.account, account);
    doc.add_text(f.project, event.cwd.as_deref().unwrap_or(""));
    doc.add_text(f.role, &event.role);
    doc.add_text(f.file_path, file_path);
    doc.add_u64(f.byte_offset, byte_offset);
    doc.add_u64(f.is_subagent, if event.is_subagent || is_subagent { 1 } else { 0 });
    if let Some(ref ts) = event.timestamp {
        doc.add_text(f.timestamp, ts);
    }
    if let Some(ref branch) = event.git_branch {
        doc.add_text(f.git_branch, branch);
    }
    if let Some(ref slug) = event.agent_slug {
        doc.add_text(f.agent_slug, slug);
    }
    doc
}

fn should_skip_file(
    path_str: &str, mtime: u64, size: u64,
    meta: &HashMap<String, FileMeta>,
) -> bool {
    if let Some(prev) = meta.get(path_str) {
        prev.mtime == mtime && prev.size == size
    } else {
        false
    }
}

fn index_directory_standalone(
    dir: &Path,
    account_name: &str,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    for entry in WalkDir::new(dir).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) { continue; }

        let path_str = path.to_string_lossy().to_string();
        let file_meta = match entry.metadata() { Ok(m) => m, Err(_) => continue };
        let mtime = match file_meta.modified() {
            Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            Err(_) => continue,
        };

        if should_skip_file(&path_str, mtime, file_meta.len(), meta) {
            *skipped += 1;
            continue;
        }
        if meta.contains_key(&path_str) {
            writer.delete_term(Term::from_field_text(f.file_path, &path_str));
        }

        let is_subagent = path_str.contains("/subagents/");
        let project = extract_project_from_path(path, dir);

        let file = match fs::File::open(path) { Ok(f) => f, Err(_) => continue };
        let reader = BufReader::new(file);
        let mut offset = 0u64;

        for line in reader.lines() {
            let line = match line { Ok(l) => l, Err(_) => continue };
            let line_offset = offset;
            offset += line.len() as u64 + 1;

            for event in parser::parse_transcript_line(&line) {
                let is_sub = event.is_subagent || is_subagent;
                let proj = event.cwd.as_deref().unwrap_or(&project);

                let mut doc = TantivyDocument::new();
                doc.add_text(f.content, &event.content);
                doc.add_text(f.session_id, &event.session_id);
                doc.add_text(f.account, account_name);
                doc.add_text(f.project, proj);
                doc.add_text(f.role, &event.role);
                doc.add_text(f.file_path, &path_str);
                doc.add_u64(f.byte_offset, line_offset);
                doc.add_u64(f.is_subagent, if is_sub { 1 } else { 0 });
                if let Some(ref ts) = event.timestamp { doc.add_text(f.timestamp, ts); }
                if let Some(ref branch) = event.git_branch { doc.add_text(f.git_branch, branch); }
                if let Some(ref slug) = event.agent_slug { doc.add_text(f.agent_slug, slug); }
                writer.add_document(doc)?;
                *indexed_docs += 1;
            }
        }

        meta.insert(path_str, FileMeta { mtime, size: file_meta.len() });
        *indexed_files += 1;
        if *indexed_files % 500 == 0 {
            tracing::info!("Indexed {} files ({} docs)...", indexed_files, indexed_docs);
            writer.commit()?;
        }
    }
    Ok(())
}

fn index_history_standalone(
    history: &Path,
    account_name: &str,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    let path_str = history.to_string_lossy().to_string();
    let file_meta = fs::metadata(history)?;
    let mtime = file_meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs();

    if should_skip_file(&path_str, mtime, file_meta.len(), meta) {
        *skipped += 1;
        return Ok(());
    }
    if meta.contains_key(&path_str) {
        writer.delete_term(Term::from_field_text(f.file_path, &path_str));
    }

    let file = fs::File::open(history)?;
    let reader = BufReader::new(file);
    let mut offset = 0u64;
    for line in reader.lines() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        let line_offset = offset;
        offset += line.len() as u64 + 1;
        for event in parser::parse_history_line(&line) {
            let doc = event_to_doc_standalone(&event, account_name, &path_str, line_offset, false, f);
            writer.add_document(doc)?;
            *indexed_docs += 1;
        }
    }
    meta.insert(path_str, FileMeta { mtime, size: file_meta.len() });
    *indexed_files += 1;
    Ok(())
}

fn index_codex_directory_standalone(
    sessions_dir: &Path,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    for entry in WalkDir::new(sessions_dir).follow_links(true).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().map(|e| e != "jsonl").unwrap_or(true) { continue; }

        let path_str = path.to_string_lossy().to_string();
        let file_meta = match entry.metadata() { Ok(m) => m, Err(_) => continue };
        let mtime = match file_meta.modified() {
            Ok(t) => t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
            Err(_) => continue,
        };

        if should_skip_file(&path_str, mtime, file_meta.len(), meta) {
            *skipped += 1;
            continue;
        }
        if meta.contains_key(&path_str) {
            writer.delete_term(Term::from_field_text(f.file_path, &path_str));
        }

        let session_id = extract_codex_session_id(path);
        let cwd = extract_codex_cwd(path);

        let file = match fs::File::open(path) { Ok(fl) => fl, Err(_) => continue };
        let reader = BufReader::new(file);
        let mut offset = 0u64;

        for line in reader.lines() {
            let line = match line { Ok(l) => l, Err(_) => continue };
            let line_offset = offset;
            offset += line.len() as u64 + 1;
            for mut event in parser::parse_codex_line(&line, &session_id) {
                if event.cwd.is_none() { event.cwd = cwd.clone(); }
                let doc = event_to_doc_standalone(&event, "codex", &path_str, line_offset, false, f);
                writer.add_document(doc)?;
                *indexed_docs += 1;
            }
        }

        meta.insert(path_str, FileMeta { mtime, size: file_meta.len() });
        *indexed_files += 1;
        if *indexed_files % 500 == 0 {
            tracing::info!("Indexed {} files ({} docs)...", indexed_files, indexed_docs);
            writer.commit()?;
        }
    }
    Ok(())
}

fn index_codex_history_standalone(
    history: &Path,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    indexed_files: &mut u64,
    indexed_docs: &mut u64,
    skipped: &mut u64,
) -> Result<()> {
    let path_str = history.to_string_lossy().to_string();
    let file_meta = fs::metadata(history)?;
    let mtime = file_meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs();

    if should_skip_file(&path_str, mtime, file_meta.len(), meta) {
        *skipped += 1;
        return Ok(());
    }
    if meta.contains_key(&path_str) {
        writer.delete_term(Term::from_field_text(f.file_path, &path_str));
    }

    let file = fs::File::open(history)?;
    let reader = BufReader::new(file);
    let mut offset = 0u64;
    for line in reader.lines() {
        let line = match line { Ok(l) => l, Err(_) => continue };
        let line_offset = offset;
        offset += line.len() as u64 + 1;
        for event in parser::parse_codex_history_line(&line) {
            let doc = event_to_doc_standalone(&event, "codex", &path_str, line_offset, false, f);
            writer.add_document(doc)?;
            *indexed_docs += 1;
        }
    }
    meta.insert(path_str, FileMeta { mtime, size: file_meta.len() });
    *indexed_files += 1;
    Ok(())
}

fn human_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    const GB: u64 = 1024 * 1024 * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}
