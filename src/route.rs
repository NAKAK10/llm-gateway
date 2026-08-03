//! Turning a route name into an ordered list of upstreams.
//!
//! Resolution itself stays boring: exact route name match, nothing else —
//! route names may not contain `*` (enforced by `crate::config::validate`),
//! so there is no prefix matching to speak of. What changed is *which* route
//! name gets resolved — `crate::server::proxy::classify_request` always
//! decides that first, by classifying the request's content against every
//! route's `description`; the model name the client sent plays no part in
//! it. `resolve` here just turns whatever name classification (or the
//! `default` fallback) picked into a concrete `Target`.

use crate::config::{ApiKind, Config, ModelConfig, ModelRef, ProviderConfig, SecretRef};
use crate::error::{Error, Result};

/// A resolved upstream: which provider, which model, and which protocol.
#[derive(Debug, Clone)]
pub struct Target {
    pub model_ref: ModelRef,
    pub api: ApiKind,
    /// How to reach it: HTTP, or a local agent CLI (`crate::agent`).
    pub transport: crate::config::Transport,
    /// Extra command-line arguments for an agent CLI transport.
    pub agent_args: Vec<String>,
    /// `base_url` with no trailing slash.
    pub base_url: String,
    /// Still unresolved — the secret is read per attempt, so a fixed Keychain
    /// entry or rotated environment variable is picked up without a reload.
    pub api_key: Option<SecretRef>,
    /// Provider-level extra headers (e.g. OpenRouter attribution).
    pub headers: Vec<(String, String)>,
    pub inject_usage: bool,
    /// How long to wait for response headers from this target. Defaults to
    /// `crate::upstream::FIRST_BYTE_TIMEOUT` when the provider config does not
    /// set its own `timeout_seconds`.
    pub timeout: std::time::Duration,
    /// How many concurrent child processes this target's provider may run
    /// when its transport is an agent CLI (`crate::agent`). Ignored by an
    /// HTTP transport. Defaults to `crate::agent::DEFAULT_MAX_CONCURRENT` when
    /// the provider config does not set its own `maxConcurrent` — see that
    /// constant's doc comment for why this exists.
    pub max_concurrent: u32,
    /// Set by `crate::server::proxy` for every target resolved inside the
    /// `<transcript>` bypass (`SemanticOutcome::UtilityBypass`, any of its
    /// three resolutions) — Claude Code's own internal auto-mode permission
    /// judgment, not a real user turn. An agent-CLI transport reads this to
    /// trim its own overhead for a call that expects a fast, short verdict
    /// (see `crate::agent::claude_cli`); an HTTP transport ignores it
    /// entirely. Always `false` from ordinary route resolution.
    pub is_utility_bypass: bool,
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.model_ref)
    }
}

/// The outcome of resolving one request.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// Route key from the config.
    pub route_name: String,
    /// `default` first, then `fallbacks`, each paired with its provider's
    /// protocol. Never empty.
    pub targets: Vec<Target>,
}

/// Resolve `requested` against the configured routes by exact name match.
pub fn resolve(config: &Config, requested: &str) -> Result<Resolution> {
    let (route_name, route) =
        find_route(config, requested).ok_or_else(|| Error::NoRoute(requested.to_string()))?;

    let targets = resolve_model(config, &format!("route `{route_name}`"), &route.model)?;

    Ok(Resolution {
        route_name: route_name.to_string(),
        targets,
    })
}

/// Resolve a bare `ModelConfig` (`default` + `fallbacks`) straight to
/// targets, without going through a route name.
///
/// Used both by [`resolve`] (via a route's `model`) and by
/// `crate::server::proxy` for `Config::auto_mode`, which has no route name to
/// look up — Claude Code's own internal `<transcript>`-prefixed auto-mode
/// judgment requests are pinned to whatever the operator configured there,
/// deliberately bypassing route-name resolution entirely.
///
/// `context` labels any error this produces — a pre-formatted phrase such as
/// `` route `role-writer` `` or `` the `autoMode` config ``, since this
/// function itself has no route name to blame a malformed entry on.
pub fn resolve_model(config: &Config, context: &str, model: &ModelConfig) -> Result<Vec<Target>> {
    let mut refs = Vec::with_capacity(1 + model.fallbacks.len());
    refs.push(model.default.as_str());
    refs.extend(model.fallbacks.iter().map(String::as_str));

    let mut targets = Vec::with_capacity(refs.len());
    for raw in refs {
        let parsed = ModelRef::parse(raw).ok_or_else(|| {
            Error::Other(format!(
                "{context} has malformed model `{raw}`; expected \"<provider>/<model>\""
            ))
        })?;
        let provider = config
            .provider(&parsed.provider)
            .ok_or_else(|| Error::UnknownProvider {
                provider: parsed.provider.clone(),
                context: context.to_string(),
            })?;
        targets.push(build_target(parsed, provider));
    }

    Ok(targets)
}

fn build_target(model_ref: ModelRef, provider: &ProviderConfig) -> Target {
    Target {
        model_ref,
        api: provider.api,
        transport: provider.transport,
        agent_args: provider.agent_args.clone(),
        base_url: provider.base_url.trim_end_matches('/').to_string(),
        api_key: provider.api_key.clone(),
        headers: provider
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        inject_usage: provider.inject_usage,
        timeout: provider
            .timeout_seconds
            .map(std::time::Duration::from_secs)
            .unwrap_or(crate::upstream::FIRST_BYTE_TIMEOUT),
        max_concurrent: provider
            .max_concurrent
            .unwrap_or(crate::agent::DEFAULT_MAX_CONCURRENT),
        // Set by the caller (`crate::server::proxy`), never known here: this
        // function has no idea whether it's building an ordinary route's
        // targets or the `<transcript>` bypass's.
        is_utility_bypass: false,
    }
}

fn find_route<'c>(
    config: &'c Config,
    requested: &str,
) -> Option<(&'c str, &'c crate::config::RouteConfig)> {
    config
        .routes
        .get_key_value(requested)
        .map(|(name, route)| (name.as_str(), route))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ModelConfig, ProviderConfig, RouteConfig};
    use std::collections::BTreeMap;

    fn provider(api: ApiKind) -> ProviderConfig {
        ProviderConfig {
            base_url: "https://example.test/v1/".to_string(),
            api,
            api_key: None,
            headers: BTreeMap::new(),
            inject_usage: true,
            transport: Default::default(),
            agent_args: Vec::new(),
            timeout_seconds: None,
            max_concurrent: None,
        }
    }

    fn route(default: &str, fallbacks: &[&str]) -> RouteConfig {
        RouteConfig {
            model: ModelConfig {
                default: default.to_string(),
                fallbacks: fallbacks.iter().map(|s| s.to_string()).collect(),
            },
            ..Default::default()
        }
    }

    fn config() -> Config {
        let mut c = Config::default();
        c.providers
            .insert("anthropic".into(), provider(ApiKind::AnthropicMessages));
        c.providers
            .insert("openrouter".into(), provider(ApiKind::OpenaiChat));
        c.routes.insert(
            "role-writer".into(),
            route("openrouter/qwen/qwen3.5", &["openrouter/deepseek/v4"]),
        );
        c.routes
            .insert("role-claude".into(), route("anthropic/sonnet-pinned", &[]));
        c
    }

    #[test]
    fn exact_match_resolves_the_named_route() {
        let c = config();
        let r = resolve(&c, "role-writer").unwrap();
        assert_eq!(r.route_name, "role-writer");
        assert_eq!(r.targets.len(), 2);
        assert_eq!(r.targets[0].model_ref.model, "qwen/qwen3.5");
    }

    /// `is_utility_bypass` is set later, by `crate::server::proxy`, for the
    /// one case (Claude Code's `<transcript>` auto-mode calls) that needs it
    /// — ordinary resolution, whether by route name or by `resolve_model`
    /// directly, must never set it itself.
    #[test]
    fn build_target_defaults_is_utility_bypass_to_false() {
        let c = config();
        let r = resolve(&c, "role-writer").unwrap();
        assert!(r.targets.iter().all(|t| !t.is_utility_bypass));
    }

    #[test]
    fn trailing_slash_is_stripped_from_base_url() {
        let c = config();
        let r = resolve(&c, "role-claude").unwrap();
        assert_eq!(r.targets[0].base_url, "https://example.test/v1");
    }

    #[test]
    fn unmatched_model_is_an_error() {
        let c = config();
        assert!(matches!(resolve(&c, "gpt-9"), Err(Error::NoRoute(_))));
    }

    #[test]
    fn model_ref_splits_on_first_slash_only() {
        let r = ModelRef::parse("openrouter/anthropic/claude-sonnet-4.6").unwrap();
        assert_eq!(r.provider, "openrouter");
        assert_eq!(r.model, "anthropic/claude-sonnet-4.6");
    }

    #[test]
    fn model_ref_keeps_colons_in_model_names() {
        let r = ModelRef::parse("ollama-cloud/glm-5.2:cloud").unwrap();
        assert_eq!(r.provider, "ollama-cloud");
        assert_eq!(r.model, "glm-5.2:cloud");
    }
}
