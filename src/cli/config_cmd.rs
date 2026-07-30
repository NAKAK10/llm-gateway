//! `llm-gateway config check|show|gitignore`.
//!
//! `check` is the pre-flight: it parses without stopping at the first error and
//! prints the whole [`ValidationReport`], plus the launch-time conflict scan
//! (does `~/.claude/settings.json` shadow our environment?). Exit code is
//! non-zero when any error — not warning — is present, so it can gate a script.
//!
//! `show` prints the effective config with every secret masked. It exists so a
//! config can be shared in a bug report without leaking a key.
//!
//! [`ValidationReport`]: crate::error::ValidationReport

use crate::config::{validate, Config, SecretRef};
use crate::error::{Error, Result};
use crate::launch::claude;
use crate::paths;

pub fn check() -> Result<()> {
    let path = paths::config_file();
    let config = Config::read(&path)?;
    let report = validate::validate(&config, &path);

    println!("{report}");

    // Read-only scan of `~/.claude/settings.json` — see `launch::claude` for
    // why an `env` block there can silently break Claude Code's redirect.
    let conflicts = claude::detect_conflicts(&[
        "ANTHROPIC_BASE_URL".to_string(),
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        "ANTHROPIC_API_KEY".to_string(),
        "ANTHROPIC_MODEL".to_string(),
    ]);
    for conflict in &conflicts {
        println!("  warning: {conflict}");
    }

    if report.errors.is_empty() {
        println!(
            "ok: {} providers, {} routes",
            config.providers.len(),
            config.routes.len()
        );
        Ok(())
    } else {
        Err(Error::ConfigInvalid(report))
    }
}

pub fn show() -> Result<()> {
    let mut config = Config::read(&paths::config_file())?;

    for provider in config.providers.values_mut() {
        provider.api_key = provider
            .api_key
            .as_ref()
            .map(|key| SecretRef::new(key.masked()));
    }
    config.server.api_key = config
        .server
        .api_key
        .as_ref()
        .map(|key| SecretRef::new(key.masked()));

    println!("{}", serde_json::to_string_pretty(&config)?);
    Ok(())
}

/// Print a `.gitignore` for a directory that contains `config.json`.
pub fn gitignore() -> Result<()> {
    println!("{}", GITIGNORE);
    Ok(())
}

/// The template `gitignore` prints.
///
/// `config.json` holds API keys when the literal form is used; the logs hold
/// prompt text when `--debug` is on. Neither belongs in a repository.
pub const GITIGNORE: &str = "\
# llm-gateway: config.json can contain literal API keys
config.json
# llm-gateway: logs contain usage data, and prompt text when --debug is on
logs/
";
