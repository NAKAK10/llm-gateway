//! `usage-YYYY-MM.jsonl` — one line per request.
//!
//! Field names are short and stable because `stats` and any future ad-hoc `jq`
//! query depend on them.

use serde::{Deserialize, Serialize};

/// One request, as filed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// RFC 3339, UTC.
    pub ts: String,
    /// From the `x-gw-client` header, falling back to `user-agent`.
    pub client: String,
    /// Route key that matched (may be a wildcard pattern).
    pub route: String,
    pub provider: String,
    /// Model actually sent upstream, after `*` expansion.
    pub model: String,
    /// 1 for the first target, 2 for the first fallback, and so on.
    pub attempt: u32,
    #[serde(default)]
    pub in_tok: u64,
    #[serde(default)]
    pub out_tok: u64,
    #[serde(default)]
    pub cache_read_tok: u64,
    #[serde(default)]
    pub cache_write_tok: u64,
    pub dur_ms: u64,
    /// `success`, `aborted`, or `error`.
    pub status: String,
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Path for a given month, e.g. `usage-2026-08.jsonl`.
pub fn file_name(year: i32, month: u8) -> String {
    format!("usage-{year:04}-{month:02}.jsonl")
}

/// Append one record as a single JSON line.
pub fn append(dir: &std::path::Path, record: &UsageRecord) -> crate::error::Result<()> {
    let _ = (dir, record);
    todo!("src/record/usage_log.rs")
}
