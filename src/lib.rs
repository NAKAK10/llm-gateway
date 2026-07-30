//! `llm-gateway` — one local endpoint in front of every agent CLI.
//!
//! The gateway speaks all three wire protocols its clients need
//! (`/v1/messages`, `/v1/chat/completions`, `/v1/responses`), rewrites only the
//! `model` field of the request, and streams the response back byte-for-byte.
//! Not touching the response body is what keeps it correct: there is no
//! translation layer to get subtly wrong.
//!
//! This is a binary-first crate; the library exists so integration tests can
//! drive the real router and config machinery over real TCP.

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
pub mod upstream;
pub mod usage;
