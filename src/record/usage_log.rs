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
    /// Route key that matched.
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
    /// Set when a `success` response yielded no usage at all — extraction
    /// failed (a parse error, a buffer overflow, a provider that never sent
    /// usage), not that the request genuinely cost zero tokens. Distinguishes
    /// "unknown" from "really zero" for `stats`; always `false` for a
    /// non-success status, since those have no usage to speak of either way.
    #[serde(default)]
    pub usage_missing: bool,
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
    use std::io::Write;

    std::fs::create_dir_all(dir)?;
    let now = time::OffsetDateTime::now_utc();
    let path = dir.join(file_name(now.year(), now.month() as u8));
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    let line = serde_json::to_string(record)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> UsageRecord {
        UsageRecord {
            ts: "2026-07-30T00:00:00Z".to_string(),
            client: "claude-code".to_string(),
            route: "claude-*".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            attempt: 1,
            in_tok: 12,
            out_tok: 34,
            cache_read_tok: 5,
            cache_write_tok: 0,
            usage_missing: false,
            dur_ms: 250,
            status: "success".to_string(),
            stream: true,
            error: None,
        }
    }

    #[test]
    fn appended_line_round_trips_as_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let record = sample();
        append(dir.path(), &record).unwrap();

        let now = time::OffsetDateTime::now_utc();
        let path = dir.path().join(file_name(now.year(), now.month() as u8));
        let contents = std::fs::read_to_string(&path).unwrap();
        let line = contents.lines().next().expect("one line was written");

        let read_back: UsageRecord = serde_json::from_str(line).unwrap();
        assert_eq!(read_back.model, record.model);
        assert_eq!(read_back.in_tok, record.in_tok);
        assert_eq!(read_back.out_tok, record.out_tok);
    }

    #[test]
    fn appending_twice_produces_two_lines() {
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), &sample()).unwrap();
        append(dir.path(), &sample()).unwrap();

        let now = time::OffsetDateTime::now_utc();
        let path = dir.path().join(file_name(now.year(), now.month() as u8));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 2);
        for line in contents.lines() {
            let _: UsageRecord = serde_json::from_str(line).unwrap();
        }
    }
}
