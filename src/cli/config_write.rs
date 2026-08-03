//! Shared read-modify-write path for CLI commands that mutate an *existing*
//! `config.json` in place — `providers add`, `route add`, `route edit`.
//!
//! Unlike `init`, which always regenerates the whole file from scratch, these
//! commands load what's there, apply one change, and write the result back.
//! `Config` round-trips through `serde_json` losslessly for the fields it
//! knows about, but a hand-written comment or unusual formatting does not
//! survive — the same tradeoff `init` already makes, not a new one.

use crate::config::{validate, Config};
use crate::error::{Error, Result};

/// Validate `config`, print every warning, and refuse to write if any error
/// is present — printing those first so the reason a write was refused is
/// visible before the command's own error return.
///
/// Called with the *whole* config, not just what one command changed: an
/// error already present before this command ran would also block the
/// write, but that only happens if the file was already broken (`serve`
/// would already be refusing to start on it) or a previous hand-edit left it
/// that way — either way, this is the moment to surface it rather than
/// layering a second problem on top of the first.
pub(crate) fn write_config(config: &Config, config_path: &std::path::Path) -> Result<()> {
    let report = validate::validate(config, config_path);
    for warning in &report.warnings {
        cliclack::log::warning(warning).ok();
    }
    if !report.errors.is_empty() {
        for error in &report.errors {
            cliclack::log::error(error).ok();
        }
        return Err(Error::Other(
            "not written — fix the error(s) above and try again".to_string(),
        ));
    }

    if config_path.exists() {
        let backup = crate::cli::init::backup_path_for(config_path);
        std::fs::copy(config_path, &backup)?;
    } else if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let json = serde_json::to_string_pretty(config)?;
    let contents = format!("// llm-gateway config — do not commit this file\n{json}\n");
    std::fs::write(config_path, contents)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(config_path)?.permissions();
        perms.set_mode(crate::cli::init::CONFIG_MODE);
        std::fs::set_permissions(config_path, perms)?;
    }

    Ok(())
}

/// Load the existing config, tolerantly (no validation — the caller
/// validates the *result* of its own edit, not whatever was already there),
/// or a default, empty `Config` if no file exists yet.
pub(crate) fn read_or_default(config_path: &std::path::Path) -> Result<Config> {
    if config_path.exists() {
        Config::read(config_path)
    } else {
        Ok(Config::default())
    }
}
