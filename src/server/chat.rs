//! `POST /v1/chat/completions` — OpenAI Chat Completions.
//!
//! Used by opencode and by OpenClaw (whose `openai-completions` mode speaks this
//! protocol).
//!
//! One request rewrite happens here beyond `model`: when the request is streaming
//! and the provider has `injectUsage` on, `stream_options.include_usage` is set.
//! Without it the upstream reports no token counts for streamed responses at all,
//! and cost accounting quietly reads zero forever — which is worse than no
//! accounting, because it looks like it works. The cost is one extra usage-only
//! chunk at the end of the stream.

use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;

use crate::server::AppState;

pub async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let _ = (state, headers, body);
    todo!("src/server/chat.rs")
}
