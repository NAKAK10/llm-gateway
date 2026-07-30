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
    let _ = (dir, record);
    todo!("src/record/trace_log.rs")
}
