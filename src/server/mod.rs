//! The HTTP surface.
//!
//! Five endpoints, chosen because between them they cover every client:
//!
//! | endpoint | protocol | who needs it |
//! |---|---|---|
//! | `POST /v1/messages` | Anthropic Messages | Claude Code |
//! | `POST /v1/messages/count_tokens` | Anthropic Messages | Claude Code (context accounting) |
//! | `POST /v1/chat/completions` | OpenAI Chat | opencode, OpenClaw |
//! | `POST /v1/responses` | OpenAI Responses | Codex CLI |
//! | `GET /v1/models` | — | opencode (it fails silently on a name mismatch) |
//!
//! Handlers are deliberately thin: read the body, rewrite `model`, ask
//! [`crate::upstream`] to find a target, forward. All the interesting decisions
//! live in `route`, `upstream` and `passthrough`.

pub mod chat;
pub mod messages;
pub mod models;
pub mod passthrough;
pub mod responses;

use std::sync::Arc;

use crate::config::watch::SharedConfig;
use crate::error::Result;
use crate::record::Recorder;

/// Options from the `serve` subcommand.
pub struct ServeOptions {
    pub debug: bool,
    pub debug_full: bool,
    pub port_override: Option<u16>,
}

/// Everything a handler needs.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<SharedConfig>,
    pub http: reqwest::Client,
    pub recorder: Arc<Recorder>,
}

/// Load config, bind, and serve until interrupted.
///
/// Refuses to start when `server.host` is not loopback and `server.api_key` is
/// unset: a single key stands between the port and every provider credential in
/// the config, so binding it to the network anonymously is never what someone
/// meant to do.
pub async fn serve(options: ServeOptions) -> Result<()> {
    let _ = options;
    todo!("src/server/mod.rs")
}

/// Build the router. Split out so tests can drive it without binding a port.
pub fn router(state: AppState) -> axum::Router {
    let _ = state;
    todo!("src/server/mod.rs")
}

/// Identify the caller for logging.
///
/// `launch` always injects `x-gw-client`, so a request without it either came
/// from a manually configured client or from something unexpected — both worth
/// being able to tell apart in the logs.
pub fn client_name(headers: &http::HeaderMap) -> String {
    headers
        .get("x-gw-client")
        .or_else(|| headers.get(http::header::USER_AGENT))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}
