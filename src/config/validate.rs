//! Config validation.
//!
//! Every problem is collected into one [`ValidationReport`] rather than
//! returning on the first failure: fixing a config file one error per run is
//! miserable, and `config check` is supposed to be the single place you look.

use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{ApiKind, Config, ModelRef};
use crate::error::ValidationReport;

/// Check a parsed config and return everything that is wrong with it.
///
/// Errors (block startup):
/// - a route references a provider that is not defined
/// - a `model` string is not `"<provider>/<model>"`
/// - a route's fallbacks do not all speak the same `ApiKind` as its default —
///   crossing protocols needs translation, which does not exist yet, so a
///   config that would silently produce garbage is refused instead
/// - a route name contains `:` or `/`
/// - `server.host` is not loopback and `server.api_key` is unset
/// - a `description` path does not exist
///
/// Warnings (allowed but reported):
/// - the config file is group- or world-readable
/// - a route has no `description` (it will be invisible to semantic routing)
/// - a provider is defined but no route uses it
pub fn validate(config: &Config, config_path: &Path) -> ValidationReport {
    let mut report = ValidationReport::default();

    if let Some(warning) = readable_by_others(config_path) {
        report.warn(warning);
    }

    let mut used_providers: BTreeSet<&str> = BTreeSet::new();

    for (route_name, route) in &config.routes {
        let name_without_wildcard: String = route_name.chars().filter(|&c| c != '*').collect();
        if name_without_wildcard.contains(':') || name_without_wildcard.contains('/') {
            report.error(format!(
                "route `{route_name}`: name contains `:` or `/` outside the wildcard `*`, which is not allowed"
            ));
        }

        match &route.description {
            Some(description) => {
                if let Some(path) = description.path() {
                    if !path.exists() {
                        report.error(format!(
                            "route `{route_name}`: description path `{}` does not exist",
                            path.display()
                        ));
                    }
                }
            }
            None => {
                report.warn(format!(
                    "route `{route_name}` has no description; it will be invisible to semantic routing"
                ));
            }
        }

        let default_api = resolve_target(
            config,
            route_name,
            "default",
            &route.model.default,
            &mut used_providers,
            &mut report,
        );

        for fallback in &route.model.fallbacks {
            let fallback_api = resolve_target(
                config,
                route_name,
                "fallback",
                fallback,
                &mut used_providers,
                &mut report,
            );

            if let (Some(default_api), Some(fallback_api)) = (default_api, fallback_api) {
                if default_api != fallback_api {
                    report.error(format!(
                        "route `{route_name}`: fallback `{fallback}` speaks {fallback_api} but default speaks {default_api}; cross-protocol fallback is not supported"
                    ));
                }
            }
        }
    }

    if !config.server.is_loopback() && config.server.api_key.is_none() {
        report.error(format!(
            "server.host `{}` is not loopback and server.api_key is unset; set server.api_key or bind server.host to a loopback address (127.0.0.1, ::1, localhost)",
            config.server.host
        ));
    }

    for provider_id in config.providers.keys() {
        if !used_providers.contains(provider_id.as_str()) {
            report.warn(format!(
                "provider `{provider_id}` is defined but not used by any route"
            ));
        }
    }

    report
}

/// Parse and resolve one model reference (`default` or a `fallback`).
///
/// Records an error for a malformed string or an undefined provider; on
/// success, marks the provider as used and returns its `ApiKind` so the caller
/// can cross-check protocols between a route's default and its fallbacks.
fn resolve_target<'a>(
    config: &'a Config,
    route_name: &str,
    role: &str,
    raw: &str,
    used_providers: &mut BTreeSet<&'a str>,
    report: &mut ValidationReport,
) -> Option<ApiKind> {
    let model_ref = match ModelRef::parse(raw) {
        Some(m) => m,
        None => {
            report.error(format!(
                "route `{route_name}`: {role} model `{raw}` is not \"<provider>/<model>\""
            ));
            return None;
        }
    };

    match config.providers.get_key_value(model_ref.provider.as_str()) {
        Some((id, provider)) => {
            used_providers.insert(id.as_str());
            Some(provider.api)
        }
        None => {
            report.error(format!(
                "route `{route_name}`: {role} `{raw}` references undefined provider `{}`",
                model_ref.provider
            ));
            None
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
        Description, ModelConfig, ProviderConfig, RouteConfig, SecretRef, ServerConfig,
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
        }
    }

    fn route(default: &str, fallbacks: &[&str]) -> RouteConfig {
        RouteConfig {
            title: None,
            description: Some(Description("a test route".to_string())),
            model: ModelConfig {
                default: default.to_string(),
                fallbacks: fallbacks.iter().map(|s| s.to_string()).collect(),
            },
        }
    }

    fn minimal_config() -> Config {
        let mut c = Config::default();
        c.providers
            .insert("anthropic".into(), provider(ApiKind::AnthropicMessages));
        c.routes
            .insert("role-writer".into(), route("anthropic/opus-pinned", &[]));
        c
    }

    fn nonexistent_path() -> PathBuf {
        PathBuf::from("/nonexistent/llm-gateway-config.json")
    }

    #[test]
    fn valid_config_has_no_errors() {
        let report = validate(&minimal_config(), &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
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

    #[test]
    fn cross_protocol_fallback_is_rejected() {
        let mut c = minimal_config();
        c.providers
            .insert("openai".into(), provider(ApiKind::OpenaiResponses));
        c.routes.insert(
            "role-writer".into(),
            route("openai/gpt-5.6", &["anthropic/opus-pinned"]),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("cross-protocol fallback is not supported")),
            "{:?}",
            report.errors
        );
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

    #[test]
    fn wildcard_route_name_is_allowed() {
        let mut c = minimal_config();
        c.routes
            .insert("claude-*".into(), route("anthropic/*", &[]));

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
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

    #[test]
    fn missing_route_description_is_a_warning_not_an_error() {
        let mut c = minimal_config();
        c.routes.get_mut("role-writer").unwrap().description = None;

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("role-writer") && w.contains("no description")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn missing_description_path_is_an_error() {
        let mut c = minimal_config();
        c.routes.get_mut("role-writer").unwrap().description = Some(Description(
            "./__llm_gateway_test_missing_description_file__.md".to_string(),
        ));

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
}
