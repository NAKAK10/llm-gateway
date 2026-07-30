//! `llm-gateway launch codex`
//!
//! Codex has no environment variable that redirects its upstream, so the
//! redirect goes through `-c` overrides. Its `--help` documents these as dotted
//! paths whose value is **parsed as TOML** — which is why every string below is
//! double-quoted inside a single-quoted shell word:
//!
//! ```text
//! -c 'model_provider="gateway"'
//! -c 'model_providers.gateway.base_url="http://127.0.0.1:4000/v1"'
//! -c 'model_providers.gateway.env_key="CODEX_API_KEY"'
//! -c 'model_providers.gateway.wire_api="responses"'
//! -c 'disable_response_storage=true'
//! ```
//!
//! `env_key` names an environment variable rather than holding the key, so the
//! token goes in `CODEX_API_KEY` and never onto a command line where `ps` can
//! see it.
//!
//! Two deliberate choices:
//!
//! - **No profile.** `-p` requires a real `$CODEX_HOME/<name>.config.toml` file
//!   to exist; `-c` cannot create one. Writing that file would break the promise
//!   not to touch client config.
//! - **`disable_response_storage=true` by default.** Codex threads conversations
//!   with `previous_response_id`, and OpenRouter's Responses endpoint is
//!   stateless — it 400s on a non-null one. Without this, a fallback to
//!   OpenRouter breaks on the second turn of every conversation.
//!
//! `--isolate` adds `--ignore-user-config`, which exists **only on `codex exec`**
//! — the TUI has no equivalent. So isolation is asymmetric here, and that is
//! called out rather than papered over.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::launch::Invocation;

pub fn build(config: &Config, model: &str, isolate: bool, args: &[String]) -> Result<Invocation> {
    let (wire_api, extra_args) = match &config.launch.codex {
        Some(cfg) => (cfg.wire_api.clone(), cfg.extra_args.clone()),
        None => ("responses".to_string(), Vec::new()),
    };
    if wire_api != "responses" && wire_api != "chat" {
        return Err(Error::Other(format!(
            "launch.codex.wireApi must be \"responses\" or \"chat\", got \"{wire_api}\""
        )));
    }

    let base_url = config.server.base_url();

    let mut all_args = vec![
        "-c".to_string(),
        toml_string_override("model_provider", "gateway"),
        "-c".to_string(),
        toml_string_override("model", model),
        "-c".to_string(),
        toml_string_override("model_providers.gateway.name", "LLM Gateway"),
        "-c".to_string(),
        toml_string_override(
            "model_providers.gateway.base_url",
            &format!("{base_url}/v1"),
        ),
        "-c".to_string(),
        toml_string_override("model_providers.gateway.env_key", "CODEX_API_KEY"),
        "-c".to_string(),
        toml_string_override("model_providers.gateway.wire_api", &wire_api),
        "-c".to_string(),
        "model_providers.gateway.http_headers={ \"x-gw-client\" = \"codex\" }".to_string(),
        "-c".to_string(),
        "disable_response_storage=true".to_string(),
    ];

    all_args.extend(extra_args);

    let mut forwarded = args.to_vec();
    let mut warnings = Vec::new();
    if isolate {
        warnings
            .push("--ignore-user-config は codex exec 専用。TUI起動では隔離されない".to_string());
        if forwarded.first().map(String::as_str) == Some("exec") {
            forwarded.insert(1, "--ignore-user-config".to_string());
        }
    }
    all_args.extend(forwarded);

    // `env_key` names an environment variable to read rather than holding the
    // key itself, so it must always resolve to something even without a
    // configured `server.api_key` — otherwise codex fails to find the env var
    // it was told to read.
    let api_key = match &config.server.api_key {
        Some(key) => key.resolve()?,
        None => "unused".to_string(),
    };

    Ok(Invocation {
        program: "codex".to_string(),
        args: all_args,
        env: vec![("CODEX_API_KEY".to_string(), api_key)],
        warnings,
    })
}

/// Render a `key=value` override with the value as a TOML string literal.
///
/// Quotes and backslashes are escaped, because a model name or header value
/// containing either would otherwise produce a config Codex silently
/// misinterprets.
pub fn toml_string_override(key: &str, value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{key}=\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_rendered_as_toml_strings() {
        assert_eq!(
            toml_string_override("model", "gpt-5.6-sol"),
            r#"model="gpt-5.6-sol""#
        );
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        assert_eq!(toml_string_override("k", r#"a"b\c"#), r#"k="a\"b\\c""#);
    }

    fn config_with_codex(wire_api: &str) -> Config {
        let mut config = Config::default();
        config.launch.codex = Some(crate::config::LaunchCodex {
            model: "unused".to_string(),
            wire_api: wire_api.to_string(),
            extra_args: Vec::new(),
        });
        config
    }

    #[test]
    fn build_produces_expected_c_overrides_and_escapes_the_model_name() {
        let config = config_with_codex("responses");
        let model = r#"gpt-5.6-"sol""#;

        let invocation = build(&config, model, false, &[]).unwrap();

        assert_eq!(invocation.program, "codex");
        assert_eq!(
            invocation.args,
            vec![
                "-c".to_string(),
                r#"model_provider="gateway""#.to_string(),
                "-c".to_string(),
                r#"model="gpt-5.6-\"sol\"""#.to_string(),
                "-c".to_string(),
                r#"model_providers.gateway.name="LLM Gateway""#.to_string(),
                "-c".to_string(),
                r#"model_providers.gateway.base_url="http://127.0.0.1:4000/v1""#.to_string(),
                "-c".to_string(),
                r#"model_providers.gateway.env_key="CODEX_API_KEY""#.to_string(),
                "-c".to_string(),
                r#"model_providers.gateway.wire_api="responses""#.to_string(),
                "-c".to_string(),
                r#"model_providers.gateway.http_headers={ "x-gw-client" = "codex" }"#.to_string(),
                "-c".to_string(),
                "disable_response_storage=true".to_string(),
            ]
        );
        assert_eq!(
            invocation.env,
            vec![("CODEX_API_KEY".to_string(), "unused".to_string())]
        );
        assert!(invocation.warnings.is_empty());
    }

    #[test]
    fn isolate_inserts_ignore_user_config_right_after_exec_and_warns() {
        let config = config_with_codex("responses");

        let invocation = build(
            &config,
            "m",
            true,
            &["exec".to_string(), "hello".to_string()],
        )
        .unwrap();

        let exec_pos = invocation.args.iter().position(|a| a == "exec").unwrap();
        assert_eq!(invocation.args[exec_pos + 1], "--ignore-user-config");
        assert!(invocation.warnings.iter().any(|w| w.contains("codex exec")));
    }

    #[test]
    fn isolate_does_not_touch_args_when_not_running_exec() {
        let config = config_with_codex("responses");

        let invocation = build(&config, "m", true, &["--foo".to_string()]).unwrap();

        assert!(!invocation
            .args
            .contains(&"--ignore-user-config".to_string()));
        assert!(!invocation.warnings.is_empty());
    }

    #[test]
    fn invalid_wire_api_is_rejected() {
        let config = config_with_codex("bogus");
        assert!(build(&config, "m", false, &[]).is_err());
    }
}
