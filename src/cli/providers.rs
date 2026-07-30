//! `llm-gateway providers` — is each upstream actually reachable?
//!
//! One probe per configured provider, in parallel, with a per-provider verdict:
//! resolved key or not, connection or not, and the HTTP status of a cheap
//! request. Exists so "the gateway is broken" can be split into "your key is
//! wrong" versus "the provider is down" without reading any logs.

use crate::error::Result;

/// Probe result for one provider.
#[derive(Debug)]
pub struct Probe {
    pub id: String,
    pub base_url: String,
    /// Whether the API key reference resolved (not whether it is *valid* —
    /// that is what the HTTP status is for).
    pub key_resolved: bool,
    /// HTTP status of the probe request, if a response came back at all.
    pub status: Option<u16>,
    pub error: Option<String>,
    pub elapsed_ms: u64,
}

pub async fn run() -> Result<()> {
    todo!("src/cli/providers.rs")
}
