//! `POST /v1/messages` and `POST /v1/messages/count_tokens` — Anthropic Messages.
//!
//! Claude Code needs both. `count_tokens` is not optional: without it Claude
//! Code cannot size its context window and starts making bad compaction
//! decisions instead of failing loudly.

use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;

use crate::server::AppState;

/// Proxy a Messages request.
pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let _ = (state, headers, body);
    todo!("src/server/messages.rs")
}

/// Proxy a token-counting request.
///
/// Never falls back: the answer is model-specific, so a count from a different
/// provider would be confidently wrong rather than usefully absent.
pub async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let _ = (state, headers, body);
    todo!("src/server/messages.rs")
}
