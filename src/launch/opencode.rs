//! `llm-gateway launch opencode`
//!
//! opencode merges configuration from several sources in a fixed order, and
//! `OPENCODE_CONFIG_CONTENT` is applied *after* a project's `opencode.json`.
//! `OPENCODE_CONFIG` is applied before it and therefore loses to any project
//! config — so the inline form is the only one that reliably wins.
//!
//! Two consequences worth knowing:
//!
//! - **`{env:VAR}` and `{file:...}` placeholders are not expanded inside
//!   `OPENCODE_CONFIG_CONTENT`** (anomalyco/opencode#13219). The token has to be
//!   embedded literally. Since this launcher generates the JSON, nobody has to
//!   hand-write a secret — but it does mean the value is in the child's
//!   environment, which is why it is redacted when printed.
//! - **`models` keys must match `GET /v1/models` exactly.** On a mismatch
//!   opencode offers no models and reports nothing. [`verify_models`] checks
//!   against the running gateway before the child starts, turning a silent
//!   failure into a message.
//!
//! `--isolate` adds `--pure`, which disables external plugins.

use crate::config::Config;
use crate::error::Result;
use crate::launch::Invocation;

pub fn build(
    config: &Config,
    model: &str,
    models: &[String],
    isolate: bool,
    args: &[String],
) -> Result<Invocation> {
    let _ = (config, model, models, isolate, args);
    todo!("src/launch/opencode.rs")
}

/// The inline config injected via `OPENCODE_CONFIG_CONTENT`.
pub fn config_content(base_url: &str, api_key: Option<&str>, models: &[String]) -> serde_json::Value {
    let _ = (base_url, api_key, models);
    todo!("src/launch/opencode.rs")
}

/// Ask the running gateway for its model list and confirm every name we are
/// about to hand opencode is present.
///
/// Returns the names that are missing.
pub async fn verify_models(
    http: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    wanted: &[String],
) -> Result<Vec<String>> {
    let _ = (http, base_url, api_key, wanted);
    todo!("src/launch/opencode.rs")
}
