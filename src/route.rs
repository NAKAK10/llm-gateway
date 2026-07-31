//! Turning the `model` a client asked for into an ordered list of upstreams.
//!
//! Resolution is intentionally boring: exact route name first, then the
//! longest-prefix wildcard. Nothing inspects the request body — that is what
//! semantic routing will add later, and keeping it out for now means an
//! explicit route name always wins and is always predictable.

use crate::config::{ApiKind, Config, ModelRef, ProviderConfig, SecretRef};
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
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.model_ref)
    }
}

/// Why a particular route was chosen. Recorded verbatim in the trace log so a
/// surprising choice can be explained after the fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// The client named the route exactly.
    Exact,
    /// A `*`-suffixed route matched by prefix.
    Wildcard,
}

impl MatchKind {
    pub fn reason(self) -> &'static str {
        match self {
            Self::Exact => "exact route name match",
            Self::Wildcard => "wildcard route prefix match",
        }
    }
}

/// The outcome of resolving one request.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// Route key from the config (may contain `*`).
    pub route_name: String,
    pub kind: MatchKind,
    /// `default` first, then `fallbacks`, each already `*`-expanded and paired
    /// with its provider's protocol. Never empty.
    pub targets: Vec<Target>,
}

/// Resolve `requested` against the configured routes.
///
/// Wildcards match by prefix and the **longest** pattern wins, so a specific
/// `claude-opus-*` can coexist with a catch-all `claude-*` without ordering
/// tricks in the config file.
pub fn resolve(config: &Config, requested: &str) -> Result<Resolution> {
    let (route_name, route, kind) =
        find_route(config, requested).ok_or_else(|| Error::NoRoute(requested.to_string()))?;

    let mut refs = Vec::with_capacity(1 + route.model.fallbacks.len());
    refs.push(route.model.default.as_str());
    refs.extend(route.model.fallbacks.iter().map(String::as_str));

    let mut targets = Vec::with_capacity(refs.len());
    for raw in refs {
        let parsed = ModelRef::parse(raw).ok_or_else(|| {
            Error::Other(format!(
                "route `{route_name}` has malformed model `{raw}`; expected \"<provider>/<model>\""
            ))
        })?;
        let provider = config
            .provider(&parsed.provider)
            .ok_or_else(|| Error::UnknownProvider {
                provider: parsed.provider.clone(),
                route: route_name.to_string(),
            })?;
        targets.push(build_target(parsed.expand(requested), provider));
    }

    Ok(Resolution {
        route_name: route_name.to_string(),
        kind,
        targets,
    })
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
    }
}

fn find_route<'c>(
    config: &'c Config,
    requested: &str,
) -> Option<(&'c str, &'c crate::config::RouteConfig, MatchKind)> {
    if let Some((name, route)) = config.routes.get_key_value(requested) {
        return Some((name.as_str(), route, MatchKind::Exact));
    }

    // Longest prefix wins, so `claude-opus-*` beats `claude-*`.
    config
        .routes
        .iter()
        .filter_map(|(name, route)| {
            let prefix = name.strip_suffix('*')?;
            requested
                .starts_with(prefix)
                .then_some((prefix.len(), name.as_str(), route))
        })
        .max_by_key(|(len, _, _)| *len)
        .map(|(_, name, route)| (name, route, MatchKind::Wildcard))
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
            .insert("claude-*".into(), route("anthropic/*", &[]));
        c.routes
            .insert("claude-opus-*".into(), route("anthropic/opus-pinned", &[]));
        c
    }

    #[test]
    fn exact_match_beats_wildcard() {
        let c = config();
        let r = resolve(&c, "role-writer").unwrap();
        assert_eq!(r.kind, MatchKind::Exact);
        assert_eq!(r.route_name, "role-writer");
        assert_eq!(r.targets.len(), 2);
        assert_eq!(r.targets[0].model_ref.model, "qwen/qwen3.5");
    }

    #[test]
    fn wildcard_expands_to_the_requested_model() {
        let c = config();
        let r = resolve(&c, "claude-sonnet-4-6").unwrap();
        assert_eq!(r.kind, MatchKind::Wildcard);
        assert_eq!(r.targets[0].model_ref.provider, "anthropic");
        assert_eq!(r.targets[0].model_ref.model, "claude-sonnet-4-6");
    }

    #[test]
    fn longest_wildcard_prefix_wins() {
        let c = config();
        let r = resolve(&c, "claude-opus-4-6").unwrap();
        assert_eq!(r.route_name, "claude-opus-*");
        assert_eq!(r.targets[0].model_ref.model, "opus-pinned");
    }

    #[test]
    fn trailing_slash_is_stripped_from_base_url() {
        let c = config();
        let r = resolve(&c, "claude-sonnet-4-6").unwrap();
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
