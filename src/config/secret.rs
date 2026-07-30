//! API key references.
//!
//! Four forms are accepted, in decreasing order of convenience and increasing
//! order of safety:
//!
//! | form | example | resolved from |
//! |---|---|---|
//! | literal | `"sk-or-v1-abc…"` | the config file itself |
//! | env | `"${OPENROUTER_API_KEY}"` | the process environment |
//! | keychain | `"keychain:openrouter"` | macOS Keychain, service `llm-gateway/openrouter` |
//! | command | `"command:gh auth token"` | stdout of a command, run per request attempt |
//!
//! The literal form exists because it is what people actually want when setting
//! this up for themselves. That is also why `config.json` is created `chmod 600`
//! and why [`SecretRef::masked`] is used everywhere a config is printed.
//!
//! The command form exists for credentials that are *minted elsewhere and
//! rotate*: GitHub Copilot is reached with a GitHub token that `gh` refreshes on
//! its own schedule, so any copy of it goes stale. `${VAR}` cannot solve that —
//! a `serve` process's environment is fixed when it starts, so nothing outside
//! can update it — while a command is re-run on every attempt and therefore
//! always current.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// An unresolved secret, exactly as written in the config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SecretRef(pub String);

/// Which of the four forms a [`SecretRef`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    Literal,
    Env,
    Keychain,
    Command,
}

impl SecretRef {
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The reference exactly as written — for tests and config round-trips,
    /// never for display (that is what [`SecretRef::masked`] is for).
    #[allow(dead_code)] // used from test code only, for now
    pub fn raw(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> SecretKind {
        let s = self.0.trim();
        if s.starts_with("${") && s.ends_with('}') {
            SecretKind::Env
        } else if s.starts_with("keychain:") {
            SecretKind::Keychain
        } else if s.starts_with("command:") {
            SecretKind::Command
        } else {
            SecretKind::Literal
        }
    }

    /// Read the actual secret value.
    ///
    /// Resolution happens on demand rather than at load time so that a hot
    /// reload never has to re-prompt the Keychain, and so `config check` can
    /// report *which* reference failed instead of dying on the first one.
    pub fn resolve(&self) -> Result<String> {
        let s = self.0.trim();
        match self.kind() {
            SecretKind::Literal => Ok(s.to_string()),

            SecretKind::Env => {
                let name = s.trim_start_matches("${").trim_end_matches('}').trim();
                if name.is_empty() {
                    return Err(Error::SecretUnresolved {
                        reference: s.to_string(),
                        reason: "empty environment variable name".to_string(),
                    });
                }
                std::env::var(name).map_err(|_| Error::SecretUnresolved {
                    reference: s.to_string(),
                    reason: format!("environment variable `{name}` is not set"),
                })
            }

            SecretKind::Keychain => {
                let name = s.trim_start_matches("keychain:").trim();
                if name.is_empty() {
                    return Err(Error::SecretUnresolved {
                        reference: s.to_string(),
                        reason: "empty keychain entry name".to_string(),
                    });
                }
                keychain_lookup(name)
            }

            SecretKind::Command => {
                let command = s.trim_start_matches("command:").trim();
                if command.is_empty() {
                    return Err(Error::SecretUnresolved {
                        reference: s.to_string(),
                        reason: "empty command".to_string(),
                    });
                }
                run_command(command)
            }
        }
    }

    /// A form safe to print. Never reveals more than the shape of the value.
    pub fn masked(&self) -> String {
        let s = self.0.trim();
        match self.kind() {
            // Non-secret by construction: they name a location (or how to
            // fetch one), not a value.
            SecretKind::Env | SecretKind::Keychain | SecretKind::Command => s.to_string(),
            SecretKind::Literal => mask_literal(s),
        }
    }
}

/// Keep enough of a literal key to recognise which one it is, and not enough to
/// use it. Long keys keep a prefix (`sk-or-v1-`-style vendor markers are
/// useful when debugging) plus the last four characters.
fn mask_literal(value: &str) -> String {
    let n = value.chars().count();
    if n <= 8 {
        return "*".repeat(n.max(1));
    }
    let head: String = value.chars().take(6).collect();
    let tail: String = value.chars().skip(n.saturating_sub(4)).collect();
    format!("{head}…{tail} ({n} chars)")
}

/// Run `command` and take its stdout as the secret.
///
/// Executed through `sh -c` so the value in the config file can be an ordinary
/// command line (`gh auth token --user someone`) rather than an argv array —
/// this is a field a human types, and the file is already `0600` and already
/// allowed to hold a literal key, so a command in it is no new exposure.
///
/// Runs on **every request attempt**, like `keychain:` does. That is the point
/// (a rotated token is picked up without a restart) and also the cost: keep the
/// command fast, because its runtime is added to every request. A helper that
/// needs to talk to the network on each call is the wrong thing to put here.
///
/// stderr is deliberately included in the error message: a credential helper
/// that fails usually explains itself there ("not logged in", "no such
/// account"), and losing that turns a one-line fix into a guessing game.
fn run_command(command: &str) -> Result<String> {
    use std::process::Command;

    let reference = format!("command:{command}");
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .map_err(|e| Error::SecretUnresolved {
            reference: reference.clone(),
            reason: format!("could not run the command: {e}"),
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::SecretUnresolved {
            reference,
            reason: match (output.status.code(), stderr.is_empty()) {
                (Some(code), false) => format!("exited with status {code}: {stderr}"),
                (Some(code), true) => format!("exited with status {code} and said nothing"),
                (None, false) => format!("was killed by a signal: {stderr}"),
                (None, true) => "was killed by a signal".to_string(),
            },
        });
    }

    // Trailing newline is what any well-behaved CLI prints; an entirely empty
    // result means the command "succeeded" without producing a credential,
    // which would otherwise turn into a confusing 401 from the provider.
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if value.is_empty() {
        return Err(Error::SecretUnresolved {
            reference,
            reason: "succeeded but printed nothing".to_string(),
        });
    }
    Ok(value)
}

/// Look up `llm-gateway/<name>` in the login keychain.
///
/// Shells out to `security` rather than linking a Security.framework binding:
/// it is a single call at startup, and the CLI is the interface Apple keeps
/// stable.
#[cfg(target_os = "macos")]
fn keychain_lookup(name: &str) -> Result<String> {
    use std::process::Command;

    let service = format!("llm-gateway/{name}");
    let output = Command::new("security")
        .args(["find-generic-password", "-s", &service, "-w"])
        .output()
        .map_err(|e| Error::SecretUnresolved {
            reference: format!("keychain:{name}"),
            reason: format!("could not run `security`: {e}"),
        })?;

    if !output.status.success() {
        return Err(Error::SecretUnresolved {
            reference: format!("keychain:{name}"),
            reason: format!(
                "no keychain entry for service `{service}`\n  \
                 add one with: security add-generic-password -a \"$USER\" -s \"{service}\" -w '<key>' -U"
            ),
        });
    }

    // `security -w` emits the password followed by a newline.
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

#[cfg(not(target_os = "macos"))]
fn keychain_lookup(name: &str) -> Result<String> {
    Err(Error::SecretUnresolved {
        reference: format!("keychain:{name}"),
        reason: "keychain: references are only supported on macOS; use \"${VAR}\" instead"
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_detected_from_shape() {
        assert_eq!(SecretRef::new("sk-abc").kind(), SecretKind::Literal);
        assert_eq!(SecretRef::new("${FOO}").kind(), SecretKind::Env);
        assert_eq!(SecretRef::new("keychain:foo").kind(), SecretKind::Keychain);
        assert_eq!(
            SecretRef::new("command:gh auth token").kind(),
            SecretKind::Command
        );
    }

    #[test]
    fn command_references_resolve_to_trimmed_stdout() {
        let got = SecretRef::new("command:printf 'tok-123\\n'")
            .resolve()
            .unwrap();
        assert_eq!(got, "tok-123");
    }

    #[test]
    fn a_failing_command_reports_its_status_and_stderr() {
        let err = SecretRef::new("command:echo 'not logged in' >&2; exit 3")
            .resolve()
            .unwrap_err()
            .to_string();
        assert!(err.contains("status 3"), "{err}");
        assert!(err.contains("not logged in"), "{err}");
    }

    #[test]
    fn a_command_that_prints_nothing_is_an_error_not_an_empty_key() {
        // Otherwise this surfaces later as an unexplained 401 from the provider.
        let err = SecretRef::new("command:true")
            .resolve()
            .unwrap_err()
            .to_string();
        assert!(err.contains("printed nothing"), "{err}");
    }

    #[test]
    fn an_empty_command_is_rejected() {
        let err = SecretRef::new("command:   ")
            .resolve()
            .unwrap_err()
            .to_string();
        assert!(err.contains("empty command"), "{err}");
    }

    #[test]
    fn env_references_resolve_from_the_environment() {
        // A name unlikely to collide with anything else in the test binary.
        std::env::set_var("LLM_GATEWAY_TEST_SECRET_XYZ", "value-123");
        let got = SecretRef::new("${LLM_GATEWAY_TEST_SECRET_XYZ}")
            .resolve()
            .unwrap();
        assert_eq!(got, "value-123");
    }

    #[test]
    fn missing_env_reference_names_the_variable() {
        let err = SecretRef::new("${LLM_GATEWAY_DEFINITELY_UNSET}")
            .resolve()
            .unwrap_err()
            .to_string();
        assert!(err.contains("LLM_GATEWAY_DEFINITELY_UNSET"), "{err}");
    }

    #[test]
    fn literal_secrets_are_masked_but_identifiable() {
        let masked = SecretRef::new("sk-or-v1-0123456789abcdef").masked();
        assert!(masked.starts_with("sk-or-"), "{masked}");
        assert!(masked.ends_with("cdef (25 chars)"), "{masked}");
        assert!(!masked.contains("0123456789"), "{masked}");
    }

    #[test]
    fn short_literals_reveal_nothing() {
        assert_eq!(SecretRef::new("abcd").masked(), "****");
    }

    #[test]
    fn location_references_are_shown_verbatim() {
        assert_eq!(
            SecretRef::new("${OPENAI_API_KEY}").masked(),
            "${OPENAI_API_KEY}"
        );
        assert_eq!(
            SecretRef::new("keychain:openai").masked(),
            "keychain:openai"
        );
        assert_eq!(
            SecretRef::new("command:gh auth token").masked(),
            "command:gh auth token"
        );
    }
}
