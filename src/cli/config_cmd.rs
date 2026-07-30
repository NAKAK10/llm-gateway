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

use crate::error::Result;

pub fn check() -> Result<()> {
    todo!("src/cli/config_cmd.rs")
}

pub fn show() -> Result<()> {
    todo!("src/cli/config_cmd.rs")
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
