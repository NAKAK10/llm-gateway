//! `llm-gateway` — one local endpoint in front of every agent CLI.
//!
//! The gateway speaks all three wire protocols its clients need
//! (`/v1/messages`, `/v1/chat/completions`, `/v1/responses`), rewrites only the
//! `model` field of the request, and streams the response back byte-for-byte.
//! Not touching the response body is what keeps it correct: for same-protocol
//! traffic there is no translation layer to get subtly wrong.
//!
//! The exception is a client/provider pair whose protocols differ, which used
//! to be refused outright — a `/v1/messages` client such as Claude Code could
//! never reach the many `openai-chat` providers. Those requests, and only
//! those, go through [`translate`], which rebuilds the request and the
//! response. Which path a request took is recorded per request in the trace
//! log.
//!
//! This is a binary-first crate; the library exists so integration tests can
//! drive the real router and config machinery over real TCP.

pub mod agent;
pub mod cli;
pub mod config;
pub mod error;
pub mod launch;
pub mod paths;
pub mod record;
pub mod route;
#[cfg(feature = "semantic")]
pub mod semantic;
pub mod server;
pub mod translate;
pub mod upstream;
pub mod usage;
