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

use std::collections::HashSet;
use std::time::Duration;

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
    let base_url = config.server.base_url();
    let api_key = match &config.server.api_key {
        Some(key) => Some(key.resolve()?),
        None => None,
    };

    let wanted_models = resolved_models(config, models);
    let content = config_content(&base_url, api_key.as_deref(), &wanted_models);

    let env = vec![(
        "OPENCODE_CONFIG_CONTENT".to_string(),
        serde_json::to_string(&content)?,
    )];

    let mut all_args = vec!["-m".to_string(), format!("gateway/{model}")];
    if isolate {
        all_args.push("--pure".to_string());
    }
    all_args.extend(args.iter().cloned());

    Ok(Invocation {
        program: "opencode".to_string(),
        args: all_args,
        env,
        warnings: Vec::new(),
    })
}

/// `models`, or every non-wildcard route when it is empty.
///
/// Shared between [`build`] and the caller's [`verify_models`] check so both
/// agree on the same list.
pub(crate) fn resolved_models(config: &Config, models: &[String]) -> Vec<String> {
    if models.is_empty() {
        config
            .listable_routes()
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        models.to_vec()
    }
}

/// The inline config injected via `OPENCODE_CONFIG_CONTENT`.
pub fn config_content(
    base_url: &str,
    api_key: Option<&str>,
    models: &[String],
) -> serde_json::Value {
    let mut options = serde_json::Map::new();
    options.insert(
        "baseURL".to_string(),
        serde_json::Value::String(format!("{base_url}/v1")),
    );
    if let Some(key) = api_key {
        options.insert(
            "apiKey".to_string(),
            serde_json::Value::String(key.to_string()),
        );
    }
    let mut headers = serde_json::Map::new();
    headers.insert(
        "x-gw-client".to_string(),
        serde_json::Value::String("opencode".to_string()),
    );
    options.insert("headers".to_string(), serde_json::Value::Object(headers));

    let mut model_entries = serde_json::Map::new();
    for m in models {
        model_entries.insert(m.clone(), serde_json::json!({}));
    }

    serde_json::json!({
        "provider": {
            "gateway": {
                "npm": "@ai-sdk/openai-compatible",
                "options": options,
                "models": model_entries,
            }
        }
    })
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
    let mut req = http
        .get(format!("{base_url}/v1/models"))
        .timeout(Duration::from_secs(5));
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }

    let body: serde_json::Value = req.send().await?.json().await?;
    let present: HashSet<&str> = body
        .get("data")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
        .collect();

    Ok(wanted
        .iter()
        .filter(|m| !present.contains(m.as_str()))
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_field_is_present_only_when_configured() {
        let with_key = config_content("http://127.0.0.1:4000", Some("secret"), &[]);
        assert_eq!(
            with_key["provider"]["gateway"]["options"]["apiKey"],
            serde_json::json!("secret")
        );

        let without_key = config_content("http://127.0.0.1:4000", None, &[]);
        assert!(without_key["provider"]["gateway"]["options"]
            .get("apiKey")
            .is_none());
    }

    #[test]
    fn config_content_shape_matches_expected_fields() {
        let content = config_content(
            "http://127.0.0.1:4000",
            None,
            &["route-a".to_string(), "route-b".to_string()],
        );

        assert_eq!(
            content["provider"]["gateway"]["npm"],
            "@ai-sdk/openai-compatible"
        );
        assert_eq!(
            content["provider"]["gateway"]["options"]["baseURL"],
            "http://127.0.0.1:4000/v1"
        );
        assert_eq!(
            content["provider"]["gateway"]["options"]["headers"]["x-gw-client"],
            "opencode"
        );
        assert_eq!(
            content["provider"]["gateway"]["models"]["route-a"],
            serde_json::json!({})
        );
        assert_eq!(
            content["provider"]["gateway"]["models"]["route-b"],
            serde_json::json!({})
        );
    }

    #[test]
    fn build_produces_expected_args() {
        let config = Config::default();

        let invocation = build(
            &config,
            "route-a",
            &["route-a".to_string()],
            false,
            &["--foo".to_string()],
        )
        .unwrap();

        assert_eq!(invocation.program, "opencode");
        assert_eq!(
            invocation.args,
            vec![
                "-m".to_string(),
                "gateway/route-a".to_string(),
                "--foo".to_string()
            ]
        );
    }

    #[test]
    fn isolate_adds_pure_flag() {
        let config = Config::default();

        let invocation = build(&config, "route-a", &["route-a".to_string()], true, &[]).unwrap();

        assert!(invocation.args.contains(&"--pure".to_string()));
    }

    #[test]
    fn empty_models_falls_back_to_listable_routes() {
        let mut config = Config::default();
        config.routes.insert(
            "route-a".to_string(),
            crate::config::RouteConfig {
                title: None,
                description: None,
                model: crate::config::ModelConfig {
                    default: "p/m".to_string(),
                    fallbacks: Vec::new(),
                },
            },
        );

        let resolved = resolved_models(&config, &[]);
        assert_eq!(resolved, vec!["route-a".to_string()]);
    }
}
