//! Config validation.
//!
//! Every problem is collected into one [`ValidationReport`] rather than
//! returning on the first failure: fixing a config file one error per run is
//! miserable, and `config check` is supposed to be the single place you look.

use std::path::Path;

use crate::config::Config;
use crate::error::ValidationReport;

/// Check a parsed config and return everything that is wrong with it.
///
/// Errors (block startup):
/// - a route references a provider that is not defined
/// - a `model` string is not `"<provider>/<model>"`
/// - a route's fallbacks do not all speak the same `ApiKind` as its default —
///   crossing protocols needs translation, which does not exist yet, so a
///   config that would silently produce garbage is refused instead
/// - a route name contains `:` or `/`
/// - `server.host` is not loopback and `server.api_key` is unset
/// - a `description` path does not exist
///
/// Warnings (allowed but reported):
/// - the config file is group- or world-readable
/// - a route has no `description` (it will be invisible to semantic routing)
/// - a provider is defined but no route uses it
pub fn validate(config: &Config, config_path: &Path) -> ValidationReport {
    let _ = (config, config_path);
    todo!("src/config/validate.rs")
}
