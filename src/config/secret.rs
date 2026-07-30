//! API key references.
//!
//! Three forms are accepted, in decreasing order of convenience and increasing
//! order of safety:
//!
//! | form | example | resolved from |
//! |---|---|---|
//! | literal | `"sk-or-v1-abc…"` | the config file itself |
//! | env | `"${OPENROUTER_API_KEY}"` | the process environment |
//! | keychain | `"keychain:openrouter"` | macOS Keychain, service `llm-gateway/openrouter` |
//!
//! The literal form exists because it is what people actually want when setting
//! this up for themselves. That is also why `config.json` is created `chmod 600`
//! and why [`SecretRef::masked`] is used everywhere a config is printed.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// An unresolved secret, exactly as written in the config file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct SecretRef(pub String);

/// Which of the three forms a [`SecretRef`] uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKind {
    Literal,
    Env,
    Keychain,
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
        }
    }

    /// A form safe to print. Never reveals more than the shape of the value.
    pub fn masked(&self) -> String {
        let s = self.0.trim();
        match self.kind() {
            // Non-secret by construction: they name a location, not a value.
            SecretKind::Env | SecretKind::Keychain => s.to_string(),
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
    }
}
