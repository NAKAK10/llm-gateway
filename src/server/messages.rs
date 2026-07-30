//! `POST /v1/messages` and `POST /v1/messages/count_tokens` — Anthropic Messages.
//!
//! Claude Code needs both. `count_tokens` is not optional: without it Claude
//! Code cannot size its context window and starts making bad compaction
//! decisions instead of failing loudly.

use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;

use crate::server::{proxy, AppState};

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    proxy(state, headers, body, "/v1/messages").await
}

/// Token counting never falls back — see `proxy` for why — and never records a
/// usage row, so a chatty client's counting does not inflate call statistics.
pub async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    proxy(state, headers, body, "/v1/messages/count_tokens").await
}
