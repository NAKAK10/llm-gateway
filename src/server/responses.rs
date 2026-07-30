//! `POST /v1/responses` — OpenAI Responses API.
//!
//! This is the endpoint Codex CLI uses, and Codex is the client with the least
//! flexibility: if this endpoint does not work, Codex cannot be routed through
//! the gateway at all.
//!
//! Note for fallback targets: OpenRouter's Responses implementation is
//! stateless and rejects `store: true` or a non-null `previous_response_id`
//! with a 400. That is why `launch codex` sets `disable_response_storage=true`
//! by default — otherwise a fallback to OpenRouter fails on the second turn of
//! every conversation.

use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;

use crate::server::{proxy, AppState};

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    proxy(state, headers, body, "/v1/responses").await
}
