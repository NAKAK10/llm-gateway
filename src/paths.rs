//! Filesystem layout.
//!
//! Everything lives under one directory so a user can back up, inspect or wipe
//! the whole gateway state in a single move:
//!
//! ```text
//! ~/.config/llm-gateway/
//! ├── config.json      # chmod 600 — contains API keys
//! ├── llm/             # long `description` bodies referenced from config.json
//! └── logs/
//!     ├── usage-YYYY-MM.jsonl
//!     └── trace-YYYY-MM-DD.jsonl
//! ```
//!
//! `logs/` under `~/.config` is a deliberate departure from strict XDG (which
//! would put it in `~/.local/state`). Keeping one directory was the explicit
//! requirement; `LLM_GATEWAY_CONFIG_DIR` is the escape hatch for anyone who
//! disagrees.

use std::path::PathBuf;

use etcetera::BaseStrategy;

/// Environment variable that overrides the whole config directory.
pub const CONFIG_DIR_ENV: &str = "LLM_GATEWAY_CONFIG_DIR";

const APP_DIR: &str = "llm-gateway";

/// Root of all gateway state.
///
/// Uses `etcetera`'s base strategy, which resolves to XDG on every platform
/// except Windows — so this is `~/.config/llm-gateway` on macOS, *not*
/// `~/Library/Application Support`. `dirs`/`directories` would have given the
/// latter; that is why they are not used here.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    match etcetera::choose_base_strategy() {
        Ok(strategy) => strategy.config_dir().join(APP_DIR),
        // No home directory (unusual, but possible in containers and CI).
        // Fall back to a relative path rather than panicking at startup.
        Err(_) => PathBuf::from(".").join(APP_DIR),
    }
}

/// `~/.config/llm-gateway/config.json`
pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// `~/.config/llm-gateway/llm/` — bodies for long `description` fields.
pub fn llm_dir() -> PathBuf {
    config_dir().join("llm")
}

/// `~/.config/llm-gateway/models/potion-multilingual-128M/` — the static
/// embedding model used for semantic routing (`semantic` feature only).
pub fn semantic_model_dir() -> PathBuf {
    config_dir().join("models").join("potion-multilingual-128M")
}

/// Resolve a `logging.dir` value, which may be relative to the config dir.
pub fn logs_dir(configured: &str) -> PathBuf {
    let p = PathBuf::from(configured);
    if p.is_absolute() {
        p
    } else {
        config_dir().join(p)
    }
}

/// Resolve a path that appeared inside config.json (e.g. `./llm/role-writer.md`)
/// relative to the config directory, so the file is portable regardless of the
/// shell's working directory.
pub fn resolve_relative(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        config_dir().join(p)
    }
}
