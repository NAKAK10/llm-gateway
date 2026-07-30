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
use crate::error::Result;
use crate::launch::Invocation;

pub fn build(config: &Config, model: &str, isolate: bool, args: &[String]) -> Result<Invocation> {
    let _ = (config, model, isolate, args);
    todo!("src/launch/codex.rs")
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
        assert_eq!(
            toml_string_override("k", r#"a"b\c"#),
            r#"k="a\"b\\c""#
        );
    }
}
