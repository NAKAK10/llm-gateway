//! `llm-gateway providers` — is each upstream actually reachable?
//!
//! One probe per configured provider, in parallel, with a per-provider verdict:
//! resolved key or not, connection or not, and the HTTP status of a cheap
//! request. Exists so "the gateway is broken" can be split into "your key is
//! wrong" versus "the provider is down" without reading any logs.

use std::time::{Duration, Instant};

use futures_util::future::join_all;

use crate::config::{ApiKind, Config, ProviderConfig};
use crate::error::{Error, Result};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

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
    let config = Config::load()?;
    let client = reqwest::Client::builder().timeout(PROBE_TIMEOUT).build()?;

    let probes = join_all(
        config
            .providers
            .iter()
            .map(|(id, provider)| probe(&client, id.clone(), provider.clone())),
    )
    .await;

    let mut table = comfy_table::Table::new();
    table.set_header(vec!["provider", "api", "key", "status", "time"]);

    let mut unreachable = 0usize;
    for (probe, api) in &probes {
        let ok = probe
            .status
            .map(|code| (200..300).contains(&code))
            .unwrap_or(false);
        if !(probe.key_resolved && ok) {
            unreachable += 1;
        }

        let status = match (probe.status, &probe.error) {
            (Some(code), _) => code.to_string(),
            (None, Some(err)) => format!("{} ({err})", probe.base_url),
            (None, None) => format!("{} (unreachable)", probe.base_url),
        };

        table.add_row(vec![
            probe.id.clone(),
            api.as_str().to_string(),
            if probe.key_resolved {
                "resolved".to_string()
            } else {
                "unresolved".to_string()
            },
            status,
            format!("{}ms", probe.elapsed_ms),
        ]);
    }

    println!("{table}");

    if unreachable > 0 {
        return Err(Error::Other(format!(
            "{unreachable} provider(s) unreachable"
        )));
    }

    Ok(())
}

/// Resolve the key, then make one cheap request to see if the provider
/// answers at all. Errors at either step are recorded on the [`Probe`] rather
/// than aborting — one bad provider should not hide the rest of the table.
async fn probe(client: &reqwest::Client, id: String, provider: ProviderConfig) -> (Probe, ApiKind) {
    let start = Instant::now();

    let (key, key_resolved) = match &provider.api_key {
        Some(secret) => match secret.resolve() {
            Ok(value) => (Some(value), true),
            Err(_) => (None, false),
        },
        // Nothing to resolve — e.g. a local endpoint's literal `"local"`
        // placeholder never needs a header at all.
        None => (None, true),
    };

    let request = if provider.api == ApiKind::AnthropicMessages {
        let mut req = client.get(format!("{}/v1/models", provider.base_url));
        if let Some(key) = &key {
            req = req.header("x-api-key", key);
        }
        req.header("anthropic-version", "2023-06-01")
    } else {
        let mut req = client.get(format!("{}/models", provider.base_url));
        if let Some(key) = &key {
            req = req.bearer_auth(key);
        }
        req
    };

    // The provider's own headers go on the probe too, or a provider that needs
    // one would be reported as broken when it is merely configured — and this
    // command exists precisely to tell those two apart.
    let mut request = request;
    for (name, value) in &provider.headers {
        request = request.header(name, value);
    }

    let (status, error) = match request.send().await {
        Ok(response) => (Some(response.status().as_u16()), None),
        Err(err) => (None, Some(err.to_string())),
    };

    let probe = Probe {
        id,
        base_url: provider.base_url.clone(),
        key_resolved,
        status,
        error,
        elapsed_ms: start.elapsed().as_millis() as u64,
    };

    (probe, provider.api)
}
