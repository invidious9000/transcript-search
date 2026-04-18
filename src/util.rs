//! Shared utilities. Keep this module small — only utilities that
//! genuinely have multiple callers should live here. One-callers
//! belong with their owner.

/// ISO-8601 UTC timestamp with second precision and a trailing `Z`.
/// Canonical format for every timestamp written into bbox stores
/// (knowledge, threads, notes, tool_docs). Hoisted from per-store
/// `Self::now_iso()` duplicates so the format stays consistent.
pub fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
