//! `llm-gateway launch claude`
//!
//! Claude Code needs nothing but environment variables:
//!
//! ```text
//! ANTHROPIC_BASE_URL=http://127.0.0.1:4000
//! ANTHROPIC_AUTH_TOKEN=<server.apiKey>        # "Bearer " is added by the client
//! ANTHROPIC_MODEL=<route>
//! ANTHROPIC_CUSTOM_HEADERS=x-gw-client: claude-code
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

use std::path::PathBuf;

use crate::config::Config;
use crate::error::Result;
use crate::launch::Invocation;

/// Environment variables this launcher sets, in the order they are shown.
pub fn build(config: &Config, model: &str, isolate: bool, args: &[String]) -> Result<Invocation> {
    let _ = (config, model, isolate, args);
    todo!("src/launch/claude.rs")
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
    let _ = vars;
    todo!("src/launch/claude.rs")
}
