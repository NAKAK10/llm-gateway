//! `POST /v1/chat/completions` — OpenAI Chat Completions.
//!
//! Used by opencode and by OpenClaw (whose `openai-completions` mode speaks this
//! protocol). The `stream_options.include_usage` injection lives in `proxy` —
//! without it, streamed responses report no token counts and cost accounting
//! silently reads zero forever.

use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;

use crate::server::{proxy, AppState};

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    proxy(state, headers, body, "/v1/chat/completions").await
}
