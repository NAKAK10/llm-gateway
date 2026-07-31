//! `llm-gateway launch claude`
//!
//! Claude Code needs nothing but environment variables:
//!
//! ```text
//! ANTHROPIC_BASE_URL=http://127.0.0.1:4000
//! ANTHROPIC_AUTH_TOKEN=<server.apiKey>        # "Bearer " is added by the client
//! ANTHROPIC_MODEL=<route>
//! ANTHROPIC_CUSTOM_HEADERS=x-gw-client: claude-code
//! x-gw-auto-route: 1
//! CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
//! ```
//!
//! There is one trap. `~/.claude/settings.json` has an `env` block, and values
//! there are written into the process environment at startup — **overriding what
//! the shell exported**. So if that file ever gains `ANTHROPIC_BASE_URL`, this
//! launcher stops working silently. [`detect_conflicts`] reads the file (read
//! only, never written) and says so up front.
//!
//! `--isolate` adds `--setting-sources project`, which stops user settings being
//! read at all. It is not the default because that also discards permissions,
//! hooks and model preferences — a big hammer for changing an endpoint.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::Result;
use crate::launch::Invocation;

/// Environment variables this launcher sets, in the order they are shown.
///
/// `model` is always the reserved `default` route name now (see
/// `crate::config::DEFAULT_ROUTE`) — the gateway classifies every request by
/// content regardless of what the client sent, so this exists only to give
/// Claude Code *something* syntactically valid to put in its own UI and
/// `ANTHROPIC_MODEL` env var.
pub fn build(
    config: &Config,
    model: &str,
    isolate: bool,
    auto_route: bool,
    args: &[String],
) -> Result<Invocation> {
    let mut env = vec![("ANTHROPIC_BASE_URL".to_string(), config.server.base_url())];
    if let Some(api_key) = &config.server.api_key {
        env.push(("ANTHROPIC_AUTH_TOKEN".to_string(), api_key.resolve()?));
    }
    env.push(("ANTHROPIC_MODEL".to_string(), model.to_string()));
    env.push((
        "ANTHROPIC_CUSTOM_HEADERS".to_string(),
        // Newline-separated `Key: Value` pairs — the only format Claude Code
        // accepts for more than one custom header.
        format!(
            "x-gw-client: claude-code\nx-gw-auto-route: {}",
            if auto_route { "1" } else { "0" }
        ),
    ));
    env.push((
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
        "1".to_string(),
    ));

    let mut all_args = Vec::new();
    if let Some(cfg) = &config.launch.claude {
        all_args.extend(cfg.extra_args.iter().cloned());
    }
    if isolate {
        all_args.push("--setting-sources".to_string());
        all_args.push("project".to_string());
    }
    all_args.extend(args.iter().cloned());

    let env_names: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
    let warnings = detect_conflicts(&env_names);

    Ok(Invocation {
        program: "claude".to_string(),
        args: all_args,
        env,
        warnings,
    })
}

/// Path of the settings file that could shadow our environment.
pub fn settings_path() -> Option<PathBuf> {
    use etcetera::BaseStrategy;
    etcetera::choose_base_strategy()
        .ok()
        .map(|s| s.home_dir().join(".claude").join("settings.json"))
}

/// Read `~/.claude/settings.json` and report any `env` key that would override
/// what this launcher sets.
///
/// Strictly read-only. Returns an empty vector when the file is missing,
/// unreadable or has no `env` block.
pub fn detect_conflicts(vars: &[String]) -> Vec<String> {
    match settings_path() {
        Some(path) => detect_conflicts_in(&path, vars),
        None => Vec::new(),
    }
}

/// The actual conflict scan, taking the file path explicitly so it can be
/// exercised against a temp file in tests without touching the real
/// `~/.claude/settings.json`.
fn detect_conflicts_in(path: &Path, vars: &[String]) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => {
            return vec!["~/.claude/settings.json がパースできません（不正なJSON）。\
                 env の競合チェックをスキップしました"
                .to_string()];
        }
    };

    let Some(env_obj) = parsed.get("env").and_then(|v| v.as_object()) else {
        return Vec::new();
    };

    env_obj
        .keys()
        .filter(|key| vars.iter().any(|v| v == *key))
        .map(|key| {
            format!(
                "~/.claude/settings.json の env.{key} がこの起動の環境変数を上書きします\
                 （settings の env は shell env より優先）"
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(contents: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f
    }

    #[test]
    fn missing_file_reports_no_conflicts() {
        let path = Path::new("/definitely/does/not/exist/llm-gateway-test/settings.json");
        let warnings = detect_conflicts_in(path, &["ANTHROPIC_BASE_URL".to_string()]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn no_env_block_reports_no_conflicts() {
        let f = write_temp(r#"{"foo": "bar"}"#);
        let warnings = detect_conflicts_in(f.path(), &["ANTHROPIC_BASE_URL".to_string()]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn conflicting_env_keys_are_reported() {
        let f = write_temp(r#"{"env": {"ANTHROPIC_BASE_URL": "https://example.com"}}"#);
        let warnings = detect_conflicts_in(
            f.path(),
            &[
                "ANTHROPIC_BASE_URL".to_string(),
                "ANTHROPIC_MODEL".to_string(),
            ],
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn non_conflicting_env_keys_are_not_reported() {
        let f = write_temp(r#"{"env": {"SOME_OTHER_VAR": "x"}}"#);
        let warnings = detect_conflicts_in(f.path(), &["ANTHROPIC_BASE_URL".to_string()]);
        assert!(warnings.is_empty());
    }

    #[test]
    fn broken_json_reports_a_single_warning() {
        let f = write_temp("{ not json");
        let warnings = detect_conflicts_in(f.path(), &["ANTHROPIC_BASE_URL".to_string()]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("settings.json"));
    }
}
