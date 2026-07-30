//! `trace-YYYY-MM-DD.jsonl` — the routing decision, in full.
//!
//! This exists to answer one question: *given what the client said, was the
//! right model picked?* So the record keeps the inputs a routing decision could
//! plausibly depend on (message count, estimated tokens, tool names, whether an
//! image was attached, the last user message) alongside the decision itself and
//! every upstream attempt.
//!
//! The `routing` field is deliberately an enum-shaped object rather than a
//! string: once semantic routing lands it gains per-candidate similarity scores,
//! and existing readers keep working because `mode` tells them which shape to
//! expect.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    pub ts: String,
    /// UUIDv7, so lines sort chronologically by id alone.
    pub req_id: String,
    pub client: String,
    /// The path that was called, e.g. `/v1/chat/completions`.
    pub endpoint: String,
    /// Exactly what the client put in `model`.
    pub requested_model: String,
    pub input: TraceInput,
    pub routing: TraceRouting,
    pub resolved: TraceResolved,
    pub attempts: Vec<TraceAttempt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TraceUsage>,
}

/// What the gateway could see about the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceInput {
    pub messages_n: usize,
    /// Truncated to 200 characters unless `--debug-full`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_text: Option<String>,
    /// Rough estimate — this is for spotting long-context requests, not billing.
    pub tokens_est: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    pub has_image: bool,
    pub stream: bool,
}

/// How the route was chosen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRouting {
    /// `explicit` today; `semantic` once embedding-based routing lands.
    pub mode: String,
    pub matched_route: String,
    pub reason: String,
    /// Populated only in `semantic` mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<TraceCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceCandidate {
    pub route: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceResolved {
    pub provider: String,
    pub model: String,
    pub api: String,
}

/// One upstream attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceAttempt {
    pub n: u32,
    /// `provider/model`.
    pub target: String,
    /// `ok_first_byte`, `http_429`, `connect_error`, `timeout`, …
    pub result: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TraceUsage {
    pub in_tok: u64,
    pub out_tok: u64,
}

/// Path for a given day, e.g. `trace-2026-08-01.jsonl`.
pub fn file_name(year: i32, month: u8, day: u8) -> String {
    format!("trace-{year:04}-{month:02}-{day:02}.jsonl")
}

/// Append one record as a single JSON line.
pub fn append(dir: &std::path::Path, record: &TraceRecord) -> crate::error::Result<()> {
    use std::io::Write;

    std::fs::create_dir_all(dir)?;
    let now = time::OffsetDateTime::now_utc();
    let path = dir.join(file_name(now.year(), now.month() as u8, now.day()));
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

    fn sample() -> TraceRecord {
        TraceRecord {
            ts: "2026-07-30T00:00:00Z".to_string(),
            req_id: "0190f0a0-0000-7000-8000-000000000000".to_string(),
            client: "claude-code".to_string(),
            endpoint: "/v1/messages".to_string(),
            requested_model: "claude-sonnet-4-6".to_string(),
            input: TraceInput {
                messages_n: 3,
                last_user_text: Some("hello".to_string()),
                tokens_est: 42,
                tools: vec![],
                has_image: false,
                stream: true,
            },
            routing: TraceRouting {
                mode: "explicit".to_string(),
                matched_route: "claude-*".to_string(),
                reason: "exact match".to_string(),
                candidates: vec![],
                score: None,
                threshold: None,
                embed_ms: None,
            },
            resolved: TraceResolved {
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4-6".to_string(),
                api: "anthropic-messages".to_string(),
            },
            attempts: vec![TraceAttempt {
                n: 1,
                target: "anthropic/claude-sonnet-4-6".to_string(),
                result: "ok_first_byte".to_string(),
                ms: 120,
            }],
            usage: Some(TraceUsage {
                in_tok: 12,
                out_tok: 34,
            }),
        }
    }

    #[test]
    fn appended_line_round_trips_as_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let record = sample();
        append(dir.path(), &record).unwrap();

        let now = time::OffsetDateTime::now_utc();
        let path = dir
            .path()
            .join(file_name(now.year(), now.month() as u8, now.day()));
        let contents = std::fs::read_to_string(&path).unwrap();
        let line = contents.lines().next().expect("one line was written");

        let read_back: TraceRecord = serde_json::from_str(line).unwrap();
        assert_eq!(read_back.req_id, record.req_id);
        assert_eq!(read_back.attempts.len(), 1);
        assert_eq!(read_back.usage.unwrap().in_tok, 12);
    }
}
