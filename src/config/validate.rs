//! Config validation.
//!
//! Every problem is collected into one [`ValidationReport`] rather than
//! returning on the first failure: fixing a config file one error per run is
//! miserable, and `config check` is supposed to be the single place you look.

use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{ApiKind, Config, ModelRef, RouteConfig, SemanticConfig};
use crate::error::ValidationReport;

/// Check a parsed config and return everything that is wrong with it.
///
/// Errors (block startup):
/// - a route references a provider that is not defined
/// - a `model` string is not `"<provider>/<model>"`
/// - a route's fallbacks do not all speak the same `ApiKind` as its default.
///   Cross-protocol translation exists for the *client*-to-provider direction
///   (`crate::translate`), but a route's own target list is still required to be
///   uniform: `proxy` picks one translation per route from its first target, and
///   a mixed list would make which translation ran depend on which upstream
///   happened to answer
/// - a route name contains `:` or `/`
/// - `server.host` is not loopback and `server.api_key` is unset
/// - a `description` path does not exist
/// - a route with `semantic` has a wildcard name — auto routes are selected by
///   an exact name, so they cannot double as a forwarding wildcard
/// - a `semantic.candidates` entry does not name a defined route
/// - a `semantic.candidates` entry names a wildcard route
/// - a `semantic.candidates` entry has no `description` (it cannot be
///   classified against)
/// - a `semantic.candidates` entry itself has `semantic` (nested auto routes
///   are not allowed)
/// - `semantic.candidates` is empty and no other route has a description
///   (there would be nothing to classify against)
/// - `semantic.threshold` is outside `0.0..=1.0` (this also catches `NaN`)
/// - a route lists itself in its own `semantic.candidates`
///
/// Warnings (allowed but reported):
/// - the config file is group- or world-readable
/// - a route has no `description` (it will be invisible to semantic routing);
///   not reported for routes with `semantic`, which host the classification
///   rather than being a target of it
/// - a provider is defined but no route uses it
/// - a `semantic` route has a candidate whose `ApiKind` the route's own
///   protocol can neither match nor be translated to — such a candidate is
///   excluded from selection at runtime, which can be surprising even though it
///   is sometimes intentional (several clients sharing one auto route). A
///   candidate reachable *through* translation is not reported: that is the
///   supported way to let a Claude Code request pick an `openai-chat` model
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
            // An auto route hosts the classification, it is never a target of
            // it (listing itself is an error), so a description would do
            // nothing for it. Warning here would tell the user to add
            // something that has no effect.
            None if route.semantic.is_none() => {
                report.warn(format!(
                    "route `{route_name}` has no description; it will be invisible to semantic routing"
                ));
            }
            None => {}
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

        validate_semantic(
            config,
            route_name,
            route,
            default_api,
            &mut used_providers,
            &mut report,
        );
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

/// Check one route's `semantic` block, if it has one.
///
/// No-op for routes without `semantic`. Everything here is additional to the
/// checks already run against `route.model` by the caller, so `default_api` —
/// this route's own `ApiKind`, already resolved by the caller — is passed in
/// rather than recomputed.
fn validate_semantic<'a>(
    config: &'a Config,
    route_name: &str,
    route: &'a RouteConfig,
    default_api: Option<ApiKind>,
    used_providers: &mut BTreeSet<&'a str>,
    report: &mut ValidationReport,
) {
    let Some(semantic) = &route.semantic else {
        return;
    };

    if route_name.contains('*') {
        report.error(format!(
            "route `{route_name}`: has `semantic` but its name contains `*`; auto routes must be selected by an exact name, not a wildcard"
        ));
    }

    if !(0.0..=1.0).contains(&semantic.threshold) {
        report.error(format!(
            "route `{route_name}`: semantic.threshold {} is out of range; must be between 0.0 and 1.0",
            semantic.threshold
        ));
    }

    if semantic.candidates.iter().any(|c| c == route_name) {
        report.error(format!(
            "route `{route_name}`: semantic.candidates includes itself, which is not allowed"
        ));
    }

    // Names resolved well enough to be worth an ApiKind cross-check below —
    // errors already reported above (self-reference) or below (missing,
    // wildcard) are excluded so the warning doesn't pile on top of them.
    // `resolve_candidates` is the same resolution `crate::semantic::index`
    // uses to build classification vectors; it is kept in one place so the
    // two can never drift apart.
    let resolved_candidates = resolve_candidates(config, route_name, semantic);

    if semantic.candidates.is_empty() {
        // Empty means "every other route that has a description".
        if resolved_candidates.is_empty() {
            report.error(format!(
                "route `{route_name}`: semantic.candidates is empty and no other route has a description; there is nothing to classify against"
            ));
        }
    } else {
        for candidate_name in &semantic.candidates {
            if candidate_name == route_name {
                continue; // already reported above
            }

            match config.routes.get_key_value(candidate_name.as_str()) {
                Some((name, candidate)) => {
                    if name.contains('*') {
                        report.error(format!(
                            "route `{route_name}`: candidate `{candidate_name}` is a wildcard route, which cannot be a semantic routing target"
                        ));
                        continue;
                    }

                    if candidate.description.is_none() {
                        report.error(format!(
                            "route `{route_name}`: candidate `{candidate_name}` has no description; it cannot be classified"
                        ));
                    }

                    if candidate.semantic.is_some() {
                        report.error(format!(
                            "route `{route_name}`: candidate `{candidate_name}` itself has `semantic`; nested auto routes are not allowed"
                        ));
                    }
                }
                None => {
                    report.error(format!(
                        "route `{route_name}`: candidate `{candidate_name}` is not a defined route"
                    ));
                }
            }
        }
    }

    let Some(default_api) = default_api else {
        return;
    };

    let mismatched: Vec<&str> = resolved_candidates
        .iter()
        .filter_map(|candidate_name| {
            let candidate = config.routes.get(*candidate_name)?;
            let mut scratch = ValidationReport::default();
            let api = resolve_target(
                config,
                candidate_name,
                "default",
                &candidate.model.default,
                used_providers,
                &mut scratch,
            )?;
            // A candidate the request's protocol can be translated *to* is
            // reachable, so it is not a mismatch worth warning about — that
            // is exactly the "Claude Code picks a local openai-chat model"
            // case cross-protocol translation exists for.
            let unreachable = api != default_api
                && crate::translate::Translation::select(default_api, api).is_none();
            unreachable.then_some(*candidate_name)
        })
        .collect();

    if !mismatched.is_empty() {
        report.warn(format!(
            "route `{route_name}`: candidate(s) {} speak an ApiKind this route's own protocol ({default_api}) can neither match nor be translated to; a request whose endpoint/protocol cannot reach a candidate excludes it from selection at runtime",
            mismatched.join(", ")
        ));
    }
}

/// Resolve which routes are eligible candidates for `route_name`'s
/// `semantic` block: the explicit list (dropping self-reference and
/// wildcard names), or — when the list is empty — every other non-wildcard
/// route that has a `description`.
///
/// This is the one place that rule lives. `validate_semantic` uses it to
/// decide what to warn/error about (`nothing to classify against`, ApiKind
/// mismatches); `crate::semantic::index` uses it to decide which routes to
/// embed as classification candidates. Keeping it in one function means a
/// change to the rule cannot accidentally apply to only one of the two.
///
/// Silently drops a candidate name that does not resolve to a route or
/// resolves to a wildcard, rather than reporting anything — reporting is
/// `validate_semantic`'s job. A candidate with no `description` or with its
/// own `semantic` block is still included here (those are errors that block
/// startup elsewhere), since by the time anything else calls this on a live
/// `Config`, `validate` has already guaranteed they cannot occur.
pub(crate) fn resolve_candidates<'a>(
    config: &'a Config,
    route_name: &str,
    semantic: &SemanticConfig,
) -> Vec<&'a str> {
    if semantic.candidates.is_empty() {
        return config
            .routes
            .iter()
            .filter(|(name, r)| {
                name.as_str() != route_name && !name.contains('*') && r.description.is_some()
            })
            .map(|(name, _)| name.as_str())
            .collect();
    }

    semantic
        .candidates
        .iter()
        .filter(|candidate_name| candidate_name.as_str() != route_name)
        .filter_map(|candidate_name| {
            let (name, _) = config.routes.get_key_value(candidate_name.as_str())?;
            (!name.contains('*')).then_some(name.as_str())
        })
        .collect()
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
        Description, ModelConfig, ProviderConfig, RouteConfig, SecretRef, SemanticConfig,
        ServerConfig,
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
            description: Some(Description("a test route".to_string())),
            model: ModelConfig {
                default: default.to_string(),
                fallbacks: fallbacks.iter().map(|s| s.to_string()).collect(),
            },
            ..Default::default()
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

    fn semantic_route(default: &str, candidates: &[&str], threshold: f32) -> RouteConfig {
        RouteConfig {
            semantic: Some(SemanticConfig {
                candidates: candidates.iter().map(|s| s.to_string()).collect(),
                threshold,
            }),
            ..route(default, &[])
        }
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

    #[test]
    fn semantic_route_with_wildcard_name_is_rejected() {
        let mut c = minimal_config();
        c.routes.insert(
            "auto-*".into(),
            semantic_route("anthropic/opus-pinned", &["role-writer"], 0.45),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("auto-*") && e.contains("wildcard")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn semantic_candidate_not_a_route_is_rejected() {
        let mut c = minimal_config();
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["ghost"], 0.45),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report.errors.iter().any(|e| e.contains("auto")
                && e.contains("ghost")
                && e.contains("not a defined route")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn semantic_candidate_that_is_wildcard_is_rejected() {
        let mut c = minimal_config();
        c.routes
            .insert("claude-*".into(), route("anthropic/*", &[]));
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["claude-*"], 0.45),
        );

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
    fn semantic_candidate_without_description_is_rejected() {
        let mut c = minimal_config();
        let mut nodesc = route("anthropic/opus-pinned", &[]);
        nodesc.description = None;
        c.routes.insert("role-nodesc".into(), nodesc);
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["role-nodesc"], 0.45),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-nodesc") && e.contains("no description")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn semantic_candidate_with_its_own_semantic_is_rejected() {
        let mut c = minimal_config();
        c.routes.insert(
            "role-nested".into(),
            semantic_route("anthropic/opus-pinned", &["role-writer"], 0.45),
        );
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["role-nested"], 0.45),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("role-nested") && e.contains("nested auto routes")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn semantic_with_no_candidates_and_none_available_is_rejected() {
        let mut c = Config::default();
        c.providers
            .insert("anthropic".into(), provider(ApiKind::AnthropicMessages));
        let mut auto = route("anthropic/opus-pinned", &[]);
        auto.description = None;
        auto.semantic = Some(SemanticConfig {
            candidates: Vec::new(),
            threshold: 0.45,
        });
        c.routes.insert("auto".into(), auto);

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("auto") && e.contains("nothing to classify against")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn semantic_threshold_out_of_range_is_rejected() {
        let mut c = minimal_config();
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["role-writer"], 1.5),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("auto") && e.contains("threshold")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn semantic_threshold_nan_is_rejected() {
        let mut c = minimal_config();
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["role-writer"], f32::NAN),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("auto") && e.contains("threshold")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn semantic_candidates_including_self_is_rejected() {
        let mut c = minimal_config();
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["auto", "role-writer"], 0.45),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("auto") && e.contains("includes itself")),
            "{:?}",
            report.errors
        );
    }

    #[test]
    fn valid_semantic_route_has_no_errors() {
        let mut c = minimal_config();
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["role-writer"], 0.45),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
    }

    /// An `anthropic-messages` auto route with an `openai-chat` candidate is
    /// the shape cross-protocol translation exists for — "let Claude Code send
    /// the cheap requests to local Ollama" — so it must not be warned about.
    #[test]
    fn a_semantic_candidate_reachable_through_translation_is_not_warned_about() {
        let mut c = minimal_config();
        c.providers
            .insert("ollama".into(), provider(ApiKind::OpenaiChat));
        c.routes
            .insert("role-cheap".into(), route("ollama/qwen3.5", &[]));
        c.routes.insert(
            "auto".into(),
            semantic_route(
                "anthropic/opus-pinned",
                &["role-writer", "role-cheap"],
                0.45,
            ),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(
            !report.warnings.iter().any(|w| w.contains("role-cheap")),
            "{:?}",
            report.warnings
        );
    }

    #[test]
    fn a_semantic_candidate_no_protocol_can_reach_is_a_warning_not_an_error() {
        // Reverse direction: an `openai-chat` auto route with an
        // `anthropic-messages` candidate. Nothing translates that way, so the
        // candidate can never be selected at runtime.
        let mut c = minimal_config();
        c.providers
            .insert("ollama".into(), provider(ApiKind::OpenaiChat));
        c.routes
            .insert("role-cheap".into(), route("ollama/qwen3.5", &[]));
        c.routes.insert(
            "auto".into(),
            semantic_route("ollama/qwen3.5", &["role-writer", "role-cheap"], 0.45),
        );

        let report = validate(&c, &nonexistent_path());
        assert!(report.is_ok(), "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("auto") && w.contains("role-writer") && w.contains("ApiKind")),
            "{:?}",
            report.warnings
        );
    }

    // `resolve_candidates` is the rule `crate::semantic::index` reuses to
    // build the classification vectors, so it is worth pinning down on its
    // own, independent of the errors/warnings `validate_semantic` derives
    // from it.

    #[test]
    fn resolve_candidates_with_empty_list_means_every_other_described_route() {
        let mut c = minimal_config(); // has "role-writer", described
        c.routes
            .insert("claude-*".into(), route("anthropic/*", &[])); // wildcard, excluded
        let mut nodesc = route("anthropic/opus-pinned", &[]);
        nodesc.description = None;
        c.routes.insert("role-nodesc".into(), nodesc); // no description, excluded
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &[], 0.45),
        );

        let semantic = c.routes["auto"].semantic.clone().unwrap();
        let resolved = resolve_candidates(&c, "auto", &semantic);

        assert_eq!(resolved, vec!["role-writer"]);
    }

    #[test]
    fn resolve_candidates_with_explicit_list_drops_self_reference_and_wildcards() {
        let mut c = minimal_config();
        c.routes
            .insert("claude-*".into(), route("anthropic/*", &[]));
        c.routes.insert(
            "auto".into(),
            semantic_route(
                "anthropic/opus-pinned",
                &["role-writer", "auto", "claude-*", "ghost"],
                0.45,
            ),
        );

        let semantic = c.routes["auto"].semantic.clone().unwrap();
        let resolved = resolve_candidates(&c, "auto", &semantic);

        // "auto" (self) and "claude-*" (wildcard) are dropped; "ghost" does
        // not name a route and is dropped too. Only "role-writer" survives.
        assert_eq!(resolved, vec!["role-writer"]);
    }

    #[test]
    fn resolve_candidates_explicit_list_preserves_order() {
        let mut c = minimal_config();
        c.routes
            .insert("role-a".into(), route("anthropic/opus-pinned", &[]));
        c.routes.insert(
            "auto".into(),
            semantic_route("anthropic/opus-pinned", &["role-a", "role-writer"], 0.45),
        );

        let semantic = c.routes["auto"].semantic.clone().unwrap();
        let resolved = resolve_candidates(&c, "auto", &semantic);

        assert_eq!(resolved, vec!["role-a", "role-writer"]);
    }
}
