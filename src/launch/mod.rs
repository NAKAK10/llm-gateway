//! Starting a client against the gateway without editing its config.
//!
//! Every client here can be redirected at launch time, so nothing in
//! `~/.claude/`, `~/.codex/` or `~/.config/opencode/` is ever written. Each one
//! needs a different mechanism:
//!
//! | client | mechanism | why not the others |
//! |---|---|---|
//! | Claude Code | environment variables | `ANTHROPIC_BASE_URL` is enough on its own |
//! | Codex CLI | `-c` dotted overrides | no environment variable redirects the upstream |
//! | opencode | `OPENCODE_CONFIG_CONTENT` | `OPENCODE_CONFIG` loses to a project config |
//!
//! OpenClaw is absent on purpose: it runs as a daemon with its own scheduler,
//! usually on another machine, so there is no process for us to start. Its setup
//! is documented in `docs/clients/openclaw.md` instead.

pub mod claude;
pub mod codex;
pub mod opencode;

use clap::ValueEnum;

use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Client {
    Claude,
    Codex,
    Opencode,
}

impl Client {
    /// The executable name looked up on `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }

    /// Value sent in `x-gw-client`, and the key `stats --by client` groups on.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Claude => "claude-code",
            Self::Codex => "codex",
            Self::Opencode => "opencode",
        }
    }
}

pub struct Options {
    pub client: Client,
    pub model_override: Option<String>,
    pub isolate: bool,
    pub print_only: bool,
    pub forwarded_args: Vec<String>,
}

/// A fully-built child invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    /// Added to the child's environment. Values may contain secrets, so
    /// [`Invocation::redacted`] is what gets printed.
    pub env: Vec<(String, String)>,
    /// Non-fatal problems found while preparing, shown before starting.
    pub warnings: Vec<String>,
}

impl Invocation {
    /// A shell-pasteable rendering with secrets replaced.
    pub fn redacted(&self) -> String {
        todo!("src/launch/mod.rs")
    }
}

/// Prepare and run (or just print) a client invocation.
///
/// Checks that the gateway is actually up first. Starting a client that will
/// fail on its first request wastes more time than a clear message here.
pub async fn run(options: Options) -> Result<()> {
    let _ = options;
    todo!("src/launch/mod.rs")
}

/// Environment variable names whose values must be redacted when printed.
pub const SECRET_ENV: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "CODEX_API_KEY",
    "OPENCODE_CONFIG_CONTENT",
];
