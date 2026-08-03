//! Config validation.
//!
//! Every problem is collected into one [`ValidationReport`] rather than
//! returning on the first failure: fixing a config file one error per run is
//! miserable, and `config check` is supposed to be the single place you look.

use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{Config, ModelRef, Transport, DEFAULT_ROUTE};
use crate::error::ValidationReport;

/// Check a parsed config and return everything that is wrong with it.
///
/// Errors (block startup):
/// - a route references a provider that is not defined
/// - a `model` string is not `"<provider>/<model>"`
/// - a route name contains `*`, `:`, or `/` — wildcard route names are not
///   supported (see the 2026-08-01 entry in `docs/decisions.md`)
/// - a route has no `description` — every request is classified against
///   every route's description now, so a route without one can never win
///   and is effectively unreachable
/// - no route named `default` exists — this is the reserved catch-all used
///   when nothing clears the classification threshold (see
///   `crate::semantic::index`)
/// - `auto_mode.default` or an `auto_mode.fallbacks` entry is not
///   `"<provider>/<model>"`, references an undefined provider, or contains a
///   wildcard `*` — checked the same way as a route's `model`, even though
///   `auto_mode` is not a route (see the 2026-08-01 entry in
///   `docs/decisions.md`)
/// - `server.host` is not loopback and `server.api_key` is unset
/// - a `description` has no variants (an empty array), or one of its
///   variants is empty
/// - a `description` variant that is a path does not exist
///
/// Warnings (allowed but reported):
/// - the config file is group- or world-readable
/// - a provider is defined but no route uses it
/// - a route's `model` has no `fallbacks`, so a transient failure on its
///   `default` target fails the request outright
pub fn validate(config: &Config, config_path: &Path) -> ValidationReport {
    let mut report = ValidationReport::default();

    if let Some(warning) = readable_by_others(config_path) {
        report.warn(warning);
    }

    let mut used_providers: BTreeSet<&str> = BTreeSet::new();

    for (route_name, route) in &config.routes {
        if route_name.contains('*') {
            report.error(format!(
                "route `{route_name}`: route names may not contain `*`; wildcard route names are not supported"
            ));
        }
        if route_name.contains(':') || route_name.contains('/') {
            report.error(format!(
                "route `{route_name}`: name contains `:` or `/`, which is not allowed"
            ));
        }

        match &route.description {
            Some(description) => {
                if description.variants().is_empty() {
                    report.error(format!(
                        "route `{route_name}`: description is an empty array; it needs at least \
                         one variant to be a classification candidate"
                    ));
                }
                for variant in description.variants() {
                    if variant.trim().is_empty() {
                        report.error(format!(
                            "route `{route_name}`: description has an empty variant"
                        ));
                    }
                }
                for path in description.paths() {
                    if !path.exists() {
                        report.error(format!(
                            "route `{route_name}`: description path `{}` does not exist",
                            path.display()
                        ));
                    }
                }
            }
            // Every request is classified against every route's description
            // now (see `crate::semantic::index`) — a route without one can
            // never win and its `model` is unreachable.
            None => {
                report.error(format!(
                    "route `{route_name}` has no description; it can never be picked by \
                     classification and its model is unreachable — add one"
                ));
            }
        }

        resolve_target(
            config,
            route_name,
            "default",
            &route.model.default,
            &mut used_providers,
            &mut report,
        );

        for fallback in &route.model.fallbacks {
            resolve_target(
                config,
                route_name,
                "fallback",
                fallback,
                &mut used_providers,
                &mut report,
            );
        }

        // Not an error — plenty of setups accept the risk — but worth
        // surfacing: `upstream::send_with_fallback` only recovers from a
        // transient upstream failure (a dead subprocess, a 502, a timeout)
        // by moving to the next target, so a route with none fails outright
        // on every hiccup. Measured against a real gateway's trace log:
        // routes with a `default`-only model saw 40-80% of their requests
        // fail completely, while routes with a fallback configured
        // recovered from the same class of failure.
        if route.model.fallbacks.is_empty() {
            report.warn(format!(
                "route `{route_name}` has no fallback target; a transient failure on its \
                 `default` model (a dead subprocess, an upstream 502, a timeout) fails the \
                 request outright with nothing to fall back to — consider adding at least one \
                 fallback"
            ));
        }
    }

    if !config.routes.contains_key(DEFAULT_ROUTE) {
        report.error(format!(
            "no route named `{DEFAULT_ROUTE}` is defined; it is the reserved catch-all used \
             when no candidate clears the classification threshold — every config needs one"
        ));
    }

    // `auto_mode` is independent of `routes` — it never depends on a route
    // name — but its `default`/`fallbacks` entries are the same shape as a
    // route's `model` and are checked the same way, via the same
    // `resolve_target` helper, just under a fixed label since there is no
    // route name to report.
    if let Some(auto_mode) = &config.auto_mode {
        resolve_target(
            config,
            "autoMode",
            "default",
            &auto_mode.default,
            &mut used_providers,
            &mut report,
        );
        for fallback in &auto_mode.fallbacks {
            resolve_target(
                config,
                "autoMode",
                "fallback",
                fallback,
                &mut used_providers,
                &mut report,
            );
        }
    }

    if !config.server.is_loopback() && config.server.api_key.is_none() {
        report.error(format!(
            "server.host `{}` is not loopback and server.api_key is unset; set server.api_key or bind server.host to a loopback address (127.0.0.1, ::1, localhost)",
            config.server.host
        ));
    }

    for (provider_id, provider) in &config.providers {
        if !used_providers.contains(provider_id.as_str()) {
            report.warn(format!(
                "provider `{provider_id}` is defined but not used by any route"
            ));
        }

        match provider.transport {
            Transport::Http => {
                if provider.base_url.trim().is_empty() {
                    report.error(format!(
                        "provider `{provider_id}`: baseUrl is required for the `http` transport"
                    ));
                }
            }
            transport if transport.is_agent_cli() => {
                // An agent CLI's output shape is fixed by the CLI, not chosen:
                // declaring something else would make the gateway translate
                // away from the shape it already has, or refuse the request.
                if let Some(expected) = transport.fixed_api() {
                    if provider.api != expected {
                        report.error(format!(
                            "provider `{provider_id}`: transport `{transport}` speaks {expected} — set `api: \"{expected}\"` (found {})",
                            provider.api
                        ));
                    }
                }
                if !provider.base_url.trim().is_empty() {
                    report.warn(format!(
                        "provider `{provider_id}`: baseUrl is ignored by the `{transport}` transport (it runs a local process)"
                    ));
                }
                // Worth saying out loud: a user who set a key here believes it
                // is being used, and it is not — the child authenticates with
                // its own subscription login.
                if provider.api_key.is_some() {
                    report.warn(format!(
                        "provider `{provider_id}`: apiKey is ignored by the `{transport}` transport; the CLI uses its own login"
                    ));
                }
            }
            // Unreachable: `is_agent_cli` covers every non-HTTP transport.
            Transport::ClaudeCli | Transport::CodexCli => {}
        }
    }

    report
}

/// Parse and resolve one model reference (`default` or a `fallback`).
///
/// Records an error for a malformed string or an undefined provider; on
/// success, marks the provider as used. A route's default and fallbacks may
/// speak different protocols now — `crate::server::proxy` decides per target
/// whether a translation exists (see `crate::translate::Translation`),
/// filtering out unreachable targets at request time rather than rejecting
/// the route here.
fn resolve_target<'a>(
    config: &'a Config,
    route_name: &str,
    role: &str,
    raw: &str,
    used_providers: &mut BTreeSet<&'a str>,
    report: &mut ValidationReport,
) {
    let model_ref = match ModelRef::parse(raw) {
        Some(m) => m,
        None => {
            report.error(format!(
                "route `{route_name}`: {role} model `{raw}` is not \"<provider>/<model>\""
            ));
            return;
        }
    };

    if model_ref.model.contains('*') {
        report.error(format!(
            "route `{route_name}`: {role} model `{raw}` contains a wildcard `*`; routing is \
             decided purely by content classification now, so a wildcard model can no longer be \
             resolved to anything meaningful — use an explicit \"<provider>/<model>\" instead"
        ));
        return;
    }

    match config.providers.get_key_value(model_ref.provider.as_str()) {
        Some((id, _provider)) => {
            used_providers.insert(id.as_str());
        }
        None => {
            report.error(format!(
                "route `{route_name}`: {role} `{raw}` references undefined provider `{}`",
                model_ref.provider
            ));
        }
    }
}

/// Warn when the config file (which contains API keys) is readable by group
/// or other. No-op if the file does not exist yet or the platform is not
/// Unix, where this concept does not apply.
#[cfg(unix)]
fn readable_by_others(path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).ok()?;
    let mode = metadata.permissions().mode();
    if mode & 0o044 != 0 {
        Some(format!(
            "config file `{}` is readable by group or other (mode {:o}); it contains API keys, tighten it with `chmod 600 {}`",
            path.display(),
            mode & 0o777,
            path.display()
        ))
    } else {
        None
    }
}

#[cfg(not(unix))]
fn readable_by_others(_path: &Path) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ApiKind, Description, ModelConfig, ProviderConfig, RouteConfig, SecretRef, ServerConfig,
    };
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn provider(api: ApiKind) -> ProviderConfig {
        ProviderConfig {
            base_url: "https://example.test/v1".to_string(),
            api,
            api_key: None,
            headers: BTreeMap::new(),
            inject_usage: true,
            transport: Default::default(),
            agent_args: Vec::new(),
            timeout_seconds: None,
        }
    }

    fn route(default: &str, fallbacks: &[&str]) -> RouteConfig {
        RouteConfig {
            description: Some(Description(vec!["a test route".to_string()])),
            model: ModelConfig {
                default: default.to_string(),
                fallbacks: fallbacks.iter().map(|s| s.to_string()).collect(),
            },
            ..Default::default()
        }
    }

    fn nonexistent_path() -> PathBuf {
        PathBuf::from("/nonexistent/llm-gateway-config.json")
    }

    /// Every test config needs the reserved `default` route to be valid on
    /// its own — most tests only care about one other property, so this is
    /// the shared baseline they build on.
    fn minimal_config() -> Config {
        let mut c = Config::default();
        c.providers
            .insert("anthropic".into(), provider(ApiKind::AnthropicMessages));
        c.routes
            .insert(DEFAULT_ROUTE.into(), route("anthropic/opus-pinned", &[]));
        c.routes
            .insert("role-writer".into(), route("anthropic/opus-pinned", &[]));
        c
    }

    #[test]
    fn valid_config_has_no_errors() {
        let report = validate(&minimal_config(), &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
    }

    #[test]
    fn missing_default_route_is_rejected() {
        let mut c = minimal_config();
        c.routes.remove(DEFAULT_ROUTE);

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains(DEFAULT_ROUTE) && e.contains("reserved catch-all")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn warnings_are_not_mixed_into_errors() {
        let mut c = minimal_config();
        // A second, unused provider triggers a warning but not an error.
        c.providers
            .insert("openrouter".into(), provider(ApiKind::OpenaiChat));

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("openrouter") && w.contains("not used")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn route_with_no_fallback_gets_a_warning_not_an_error() {
        let report = validate(&minimal_config(), &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("role-writer") && w.contains("no fallback target")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn route_with_a_fallback_gets_no_such_warning() {
        let mut c = minimal_config();
        c.routes.insert(
            "role-writer".into(),
            route("anthropic/opus-pinned", &["anthropic/haiku-pinned"]),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("role-writer") && w.contains("no fallback target")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn route_referencing_undefined_provider_is_an_error() {
        let mut c = minimal_config();
        c.routes
            .insert("role-ghost".into(), route("does-not-exist/some-model", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-ghost") && e.contains("does-not-exist")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn malformed_model_string_is_an_error() {
        let mut c = minimal_config();
        c.routes
            .insert("role-broken".into(), route("no-slash-here", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-broken") && e.contains("no-slash-here")),
            "{:?}",
            report.errors
        );
    }

    /// The point of the cross-protocol fallback change: a route's default and
    /// its fallbacks no longer have to speak the same protocol.
    /// `crate::server::proxy` is what decides per target whether a request
    /// can actually reach it.
    #[test]
    fn cross_protocol_fallback_is_accepted() {
        let mut c = minimal_config();
        c.providers
            .insert("openai".into(), provider(ApiKind::OpenaiResponses));
        c.routes.insert(
            "role-writer".into(),
            route("openai/gpt-5.6", &["anthropic/opus-pinned"]),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
    }

    #[test]
    fn route_name_with_colon_is_rejected() {
        let mut c = minimal_config();
        c.routes
            .insert("role:writer".into(), route("anthropic/opus-pinned", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report.errors.iter().any(|e| e.contains("role:writer")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn route_name_with_slash_is_rejected() {
        let mut c = minimal_config();
        c.routes
            .insert("role/writer".into(), route("anthropic/opus-pinned", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report.errors.iter().any(|e| e.contains("role/writer")),
            "{:?}",
            report.errors
        );
    }

    /// Wildcard route names are banned outright: a hand-written `claude-*`
    /// risks silently intercepting requests it was never meant to catch, so
    /// the mechanism that made it work is gone, not just discouraged.
    #[test]
    fn wildcard_route_name_is_rejected() {
        let mut c = minimal_config();
        c.routes
            .insert("claude-*".into(), route("anthropic/sonnet-pinned", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("claude-*") && e.contains("wildcard")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn wildcard_model_is_rejected() {
        let mut c = minimal_config();
        c.routes
            .insert("role-claude".into(), route("anthropic/*", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report.errors.iter().any(|e| e.contains("wildcard")),
            "{:?}",
            report.errors
        );
    }

    /// A `claude-cli` provider needs neither of the two fields every other
    /// provider requires, and saying so in tests keeps a future "surely baseUrl
    /// is mandatory" refactor honest.
    #[test]
    fn a_claude_cli_provider_needs_no_base_url_or_key() {
        let mut c = minimal_config();
        c.providers.insert(
            "claude-subscription".into(),
            ProviderConfig {
                base_url: String::new(),
                api: ApiKind::AnthropicMessages,
                api_key: None,
                headers: BTreeMap::new(),
                inject_usage: true,
                transport: Transport::ClaudeCli,
                agent_args: Vec::new(),
                timeout_seconds: None,
            },
        );
        c.routes
            .insert("role-sub".into(), route("claude-subscription/sonnet", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(
            !report
                .warnings
                .iter()
                .any(|w| w.contains("claude-subscription")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_claude_cli_provider_must_declare_the_protocol_the_cli_speaks() {
        let mut c = minimal_config();
        c.providers.insert(
            "wrong-api".into(),
            ProviderConfig {
                base_url: String::new(),
                api: ApiKind::OpenaiChat,
                api_key: None,
                headers: BTreeMap::new(),
                inject_usage: true,
                transport: Transport::ClaudeCli,
                agent_args: Vec::new(),
                timeout_seconds: None,
            },
        );
        c.routes
            .insert("role-x".into(), route("wrong-api/sonnet", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("wrong-api") && e.contains("anthropic-messages")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn fields_a_claude_cli_provider_ignores_are_warned_about() {
        // Silently ignoring an apiKey someone deliberately set is worse than
        // saying it does nothing.
        let mut c = minimal_config();
        c.providers.insert(
            "noisy".into(),
            ProviderConfig {
                base_url: "https://example.test".into(),
                api: ApiKind::AnthropicMessages,
                api_key: Some(SecretRef::new("sk-unused")),
                headers: BTreeMap::new(),
                inject_usage: true,
                transport: Transport::ClaudeCli,
                agent_args: Vec::new(),
                timeout_seconds: None,
            },
        );
        c.routes.insert("role-y".into(), route("noisy/sonnet", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("noisy") && w.contains("baseUrl")),
            "{:?}",
            report.warnings
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("noisy") && w.contains("apiKey")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn an_http_provider_without_a_base_url_is_rejected() {
        let mut c = minimal_config();
        c.providers.get_mut("anthropic").unwrap().base_url = String::new();

        let report = validate(&c, &nonexistent_path());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("anthropic") && e.contains("baseUrl")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn non_loopback_host_without_api_key_is_rejected() {
        let mut c = minimal_config();
        c.server = ServerConfig {
            host: "0.0.0.0".to_string(),
            port: 4000,
            api_key: None,
        };

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("0.0.0.0") && e.contains("api_key")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn non_loopback_host_with_api_key_is_allowed() {
        let mut c = minimal_config();
        c.server = ServerConfig {
            host: "0.0.0.0".to_string(),
            port: 4000,
            api_key: Some(SecretRef::new("sk-test")),
        };

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
    }

    /// Every request is now classified against every non-wildcard route's
    /// description, so one missing is a hard error, not a warning — the
    /// route's `model` would be silently unreachable otherwise.
    #[test]
    fn missing_route_description_is_an_error() {
        let mut c = minimal_config();
        c.routes.get_mut("role-writer").unwrap().description = None;

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-writer") && e.contains("no description")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn missing_description_path_is_an_error() {
        let mut c = minimal_config();
        c.routes.get_mut("role-writer").unwrap().description = Some(Description(vec![
            "./__llm_gateway_test_missing_description_file__.md".to_string(),
        ]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-writer") && e.contains("does not exist")),
            "{:?}",
            report.errors
        );
    }

    /// A route whose traffic mixes languages describes itself with several
    /// variants (see the module doc on `crate::config::Description`) — a
    /// plain array of two or more entries must be accepted without error.
    #[test]
    fn a_description_with_multiple_variants_is_accepted() {
        let mut c = minimal_config();
        c.routes.get_mut("role-writer").unwrap().description = Some(Description(vec![
            "日本語の説明".to_string(),
            "English description".to_string(),
        ]));

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
    }

    #[test]
    fn an_empty_description_array_is_an_error() {
        let mut c = minimal_config();
        c.routes.get_mut("role-writer").unwrap().description = Some(Description(Vec::new()));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-writer") && e.contains("empty array")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn a_description_with_an_empty_variant_is_an_error() {
        let mut c = minimal_config();
        c.routes.get_mut("role-writer").unwrap().description =
            Some(Description(vec!["ok".to_string(), "".to_string()]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-writer") && e.contains("empty variant")),
            "{:?}",
            report.errors
        );
    }

    /// Each variant in a multi-entry description is checked for a path
    /// independently — one missing path among several variants must still be
    /// caught.
    #[test]
    fn a_missing_path_among_several_variants_is_an_error() {
        let mut c = minimal_config();
        c.routes.get_mut("role-writer").unwrap().description = Some(Description(vec![
            "inline text".to_string(),
            "./__llm_gateway_test_missing_description_file__.md".to_string(),
        ]));

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-writer") && e.contains("does not exist")),
            "{:?}",
            report.errors
        );
    }

    /// `auto_mode` is entirely optional — an unset one must not affect an
    /// otherwise-valid config.
    #[test]
    fn unset_auto_mode_does_not_affect_validation() {
        let c = minimal_config();
        assert!(c.auto_mode.is_none());

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
    }

    /// A well-formed `auto_mode` pointing at a real provider is accepted.
    #[test]
    fn a_valid_auto_mode_is_accepted() {
        let mut c = minimal_config();
        c.auto_mode = Some(ModelConfig {
            default: "anthropic/opus-pinned".to_string(),
            fallbacks: Vec::new(),
        });

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
    }

    /// `auto_mode.default` is checked the same way a route's `model.default`
    /// is: a malformed string is an error.
    #[test]
    fn malformed_auto_mode_default_is_an_error() {
        let mut c = minimal_config();
        c.auto_mode = Some(ModelConfig {
            default: "no-slash-here".to_string(),
            fallbacks: Vec::new(),
        });

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("autoMode") && e.contains("no-slash-here")),
            "{:?}",
            report.errors
        );
    }

    /// An `auto_mode.default` referencing an undefined provider is an error,
    /// same as a route's.
    #[test]
    fn auto_mode_default_with_undefined_provider_is_an_error() {
        let mut c = minimal_config();
        c.auto_mode = Some(ModelConfig {
            default: "does-not-exist/some-model".to_string(),
            fallbacks: Vec::new(),
        });

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("autoMode") && e.contains("does-not-exist")),
            "{:?}",
            report.errors
        );
    }

    /// A malformed `auto_mode.fallbacks` entry is checked too, not just
    /// `default`.
    #[test]
    fn malformed_auto_mode_fallback_is_an_error() {
        let mut c = minimal_config();
        c.auto_mode = Some(ModelConfig {
            default: "anthropic/opus-pinned".to_string(),
            fallbacks: vec!["no-slash-here".to_string()],
        });

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("autoMode") && e.contains("no-slash-here")),
            "{:?}",
            report.errors
        );
    }

    /// `auto_mode.default` follows the same model-string wildcard ban as a
    /// route's `model` — routing there is pinned by the operator, not
    /// resolved from anything a wildcard could stand in for.
    #[test]
    fn wildcard_in_auto_mode_model_is_rejected() {
        let mut c = minimal_config();
        c.auto_mode = Some(ModelConfig {
            default: "anthropic/*".to_string(),
            fallbacks: Vec::new(),
        });

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report.errors.iter().any(|e| e.contains("wildcard")),
            "{:?}",
            report.errors
        );
    }
}
