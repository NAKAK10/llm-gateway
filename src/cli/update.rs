//! `llm-gateway update` — is there a newer release, and how do I get it?
//!
//! This command deliberately **does not replace its own binary**. Every install
//! of this tool comes from somewhere that already tracks versions: Homebrew has
//! a Cellar path and a manifest, `cargo install` has a registry entry. Writing
//! over the binary in place would leave that bookkeeping wrong — brew would still
//! believe the old version is installed, and the next `brew upgrade` would
//! cheerfully "upgrade" back to it.
//!
//! So the job here is narrower and more honest: ask GitHub what the latest
//! release is, compare, and run the upgrade command that suits how *this* binary
//! got here. The command is printed before it runs, because a tool that mutates
//! an installation without saying how is a tool you cannot audit.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

/// Where the release list lives. The `/latest` endpoint skips drafts and
/// prereleases, which is exactly the set a user should be offered.
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/NAKAK10/llm-gateway/releases/latest";

const REPO_URL: &str = "https://github.com/NAKAK10/llm-gateway";
const BREW_FORMULA: &str = "NAKAK10/tap/llm-gateway";

/// GitHub rejects API requests without one.
const USER_AGENT: &str = concat!("llm-gateway/", env!("CARGO_PKG_VERSION"));

const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct Options {
    /// Report only; never run an upgrade.
    pub check_only: bool,
}

/// How this binary was installed, and therefore how to upgrade it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// A Cellar path: `brew upgrade` owns this.
    Homebrew,
    /// `~/.cargo/bin`: `cargo install --force` owns this.
    Cargo,
    /// A `target/{debug,release}` path — someone's working copy, where the right
    /// answer is `git pull`, not a package manager.
    Source,
    /// Anything else: a binary someone placed by hand, so the honest answer is
    /// a download link.
    Manual(PathBuf),
}

pub async fn run(options: Options) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .unwrap_or_else(|_| PathBuf::from("llm-gateway"));
    let install = detect_install(&exe);

    let latest = latest_release().await?;

    println!("installed: {current}");
    println!("latest:    {latest}");

    if !is_newer(&latest, current) {
        println!("\nalready up to date");
        return Ok(());
    }

    let Some(command) = upgrade_command(&install) else {
        println!("\na newer release is available. This binary was not installed by a");
        println!("package manager, so replace it with the build for your machine:");
        println!("  {REPO_URL}/releases/tag/v{latest}");
        if let Install::Manual(path) = &install {
            println!("\n(currently running {})", path.display());
        }
        if install == Install::Source {
            println!("\nOr, from your checkout: git pull && cargo install --path .");
        }
        return Ok(());
    };

    if options.check_only {
        println!("\na newer release is available. To install it:");
        println!("  {}", command.join(" "));
        return Ok(());
    }

    println!("\nrunning: {}", command.join(" "));
    let status = Command::new(&command[0])
        .args(&command[1..])
        .status()
        .map_err(|source| {
            Error::Other(format!("could not run `{}`: {source}", command.join(" ")))
        })?;

    if !status.success() {
        return Err(Error::Other(format!(
            "`{}` failed ({status}); nothing was changed",
            command.join(" ")
        )));
    }

    println!("\nupdated. `llm-gateway --version` to confirm.");
    Ok(())
}

/// Classify an executable path.
///
/// Pure, so the mapping is testable without installing four different ways.
pub fn detect_install(exe: &Path) -> Install {
    let path = exe.to_string_lossy();
    if path.contains("/Cellar/") || path.contains("/homebrew/") {
        Install::Homebrew
    } else if path.contains("/.cargo/bin/") {
        Install::Cargo
    } else if path.contains("/target/debug/") || path.contains("/target/release/") {
        Install::Source
    } else {
        Install::Manual(exe.to_path_buf())
    }
}

/// The upgrade command for an install a package manager owns.
///
/// `None` means "there is nothing safe to run" — see [`Install::Manual`].
pub fn upgrade_command(install: &Install) -> Option<Vec<String>> {
    match install {
        Install::Homebrew => Some(vec![
            "brew".to_string(),
            "upgrade".to_string(),
            BREW_FORMULA.to_string(),
        ]),
        Install::Cargo => {
            let mut command = vec![
                "cargo".to_string(),
                "install".to_string(),
                "--git".to_string(),
                REPO_URL.to_string(),
                "--force".to_string(),
            ];
            // A rebuild without this silently drops semantic routing from a
            // binary that had it, and the only symptom is a warning at startup
            // much later.
            if cfg!(feature = "semantic") {
                command.push("--features".to_string());
                command.push("semantic".to_string());
            }
            Some(command)
        }
        Install::Source | Install::Manual(_) => None,
    }
}

/// The latest published release's version, without the `v`.
async fn latest_release() -> Result<String> {
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(USER_AGENT)
        .build()?;

    let response = client
        .get(LATEST_RELEASE_API)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|source| {
            Error::Other(format!(
                "could not reach GitHub to check for updates: {source}"
            ))
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(Error::Other(format!(
            "GitHub answered {status} when asked for the latest release"
        )));
    }

    let body: serde_json::Value = response.json().await?;
    let tag = body
        .get("tag_name")
        .and_then(|t| t.as_str())
        .ok_or_else(|| Error::Other("GitHub's release response had no tag_name".to_string()))?;

    Ok(tag.trim_start_matches('v').to_string())
}

/// `(major, minor, patch)` from an `x.y.z` string, ignoring any suffix.
pub fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `latest` is a higher version than `current`.
///
/// Unparseable input answers `false`: refusing to act on a version string we do
/// not understand beats offering an "upgrade" that might be a downgrade.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn homebrew_installs_are_recognised_by_their_cellar_path() {
        assert_eq!(
            detect_install(Path::new(
                "/usr/local/Cellar/llm-gateway/0.4.0/bin/llm-gateway"
            )),
            Install::Homebrew
        );
        // Apple silicon's prefix.
        assert_eq!(
            detect_install(Path::new(
                "/opt/homebrew/Cellar/llm-gateway/0.4.0/bin/llm-gateway"
            )),
            Install::Homebrew
        );
    }

    #[test]
    fn cargo_and_source_builds_are_told_apart() {
        assert_eq!(
            detect_install(Path::new("/Users/x/.cargo/bin/llm-gateway")),
            Install::Cargo
        );
        assert_eq!(
            detect_install(Path::new(
                "/Users/x/code/llm-gateway/target/debug/llm-gateway"
            )),
            Install::Source
        );
        assert_eq!(
            detect_install(Path::new(
                "/Users/x/code/llm-gateway/target/release/llm-gateway"
            )),
            Install::Source
        );
    }

    #[test]
    fn anything_else_is_manual_and_keeps_the_path_for_the_message() {
        assert_eq!(
            detect_install(Path::new("/usr/local/bin/llm-gateway")),
            Install::Manual(PathBuf::from("/usr/local/bin/llm-gateway"))
        );
    }

    #[test]
    fn homebrew_upgrades_through_the_tap_formula() {
        let command = upgrade_command(&Install::Homebrew).unwrap();
        assert_eq!(command, vec!["brew", "upgrade", "NAKAK10/tap/llm-gateway"]);
    }

    #[test]
    fn a_cargo_upgrade_keeps_the_features_this_build_has() {
        let command = upgrade_command(&Install::Cargo).unwrap();
        assert!(command.starts_with(&["cargo".to_string(), "install".to_string()]));
        assert!(command.iter().any(|a| a == "--force"));
        // Otherwise a rebuild would silently drop semantic routing.
        let asks_for_semantic = command.windows(2).any(|w| w == ["--features", "semantic"]);
        assert_eq!(asks_for_semantic, cfg!(feature = "semantic"));
    }

    #[test]
    fn nothing_is_run_for_an_install_no_package_manager_owns() {
        assert!(upgrade_command(&Install::Source).is_none());
        assert!(upgrade_command(&Install::Manual(PathBuf::from("/tmp/x"))).is_none());
    }

    #[test]
    fn versions_parse_with_or_without_a_leading_v() {
        assert_eq!(parse_version("0.4.0"), Some((0, 4, 0)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version(" 0.10.2 "), Some((0, 10, 2)));
        // Suffixes are ignored rather than rejected.
        assert_eq!(parse_version("1.0.0-rc.1"), Some((1, 0, 0)));
        assert_eq!(parse_version("nonsense"), None);
    }

    #[test]
    fn comparison_is_numeric_not_lexical() {
        // The bug this test exists for: "0.10.0" sorts before "0.9.0" as text.
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("0.9.0", "0.10.0"));
        assert!(is_newer("0.4.1", "0.4.0"));
        assert!(!is_newer("0.4.0", "0.4.0"));
        assert!(is_newer("1.0.0", "0.99.99"));
    }

    #[test]
    fn an_unparseable_version_never_offers_an_update() {
        // Better to say nothing than to offer a possible downgrade.
        assert!(!is_newer("garbage", "0.4.0"));
        assert!(!is_newer("0.5.0", "garbage"));
    }
}
