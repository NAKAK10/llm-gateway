//! Crate-wide error type.
//!
//! Errors that reach the user must be actionable: say which file, which key,
//! and what to do about it. Wrapping without context is worse than no wrapping.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config file not found: {0}\nrun `llm-gateway init` to create it")]
    ConfigMissing(PathBuf),

    #[error("failed to read {path}: {source}")]
    ConfigRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse {path}: {source}\nthe file is parsed as JSON5, so comments and trailing commas are allowed")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: json5::Error,
    },

    /// One or more validation failures. Reported together so the user can fix
    /// everything in one pass instead of playing whack-a-mole.
    #[error("{0}")]
    ConfigInvalid(ValidationReport),

    #[error("secret `{reference}` could not be resolved: {reason}")]
    SecretUnresolved { reference: String, reason: String },

    /// `context` is a pre-formatted phrase naming what referenced the
    /// provider — `` route `role-writer` `` for an ordinary route, or
    /// `` the `autoMode` config `` for `crate::route::resolve_model` called
    /// on `Config::auto_mode`, which has no route name to attribute to.
    #[error("unknown provider `{provider}` referenced by {context}")]
    UnknownProvider { provider: String, context: String },

    #[error("no route matches model `{0}`")]
    NoRoute(String),

    #[error("all upstream attempts failed for route `{route}`: {detail}")]
    AllUpstreamsFailed { route: String, detail: String },

    #[error("gateway is not running at {url}\nstart it with `llm-gateway serve`")]
    GatewayUnreachable { url: String },

    #[error("`{0}` was not found on PATH")]
    ClientNotFound(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Http(#[from] reqwest::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

/// Collected validation failures and warnings for a single config load.
#[derive(Debug, Default)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }

    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

impl std::fmt::Display for ValidationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.errors.is_empty() {
            writeln!(f, "{} configuration error(s):", self.errors.len())?;
            for e in &self.errors {
                writeln!(f, "  error: {e}")?;
            }
        }
        for w in &self.warnings {
            writeln!(f, "  warning: {w}")?;
        }
        Ok(())
    }
}
