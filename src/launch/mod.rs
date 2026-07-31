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

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::ValueEnum;

use crate::config::Config;
use crate::error::{Error, Result};

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
}

pub struct Options {
    pub client: Client,
    pub isolate: bool,
    pub print_only: bool,
    /// `Some(true/false)` when `--auto`/`--no-auto` answered the
    /// auto-classify question up front; `None` to ask interactively.
    pub auto_route: Option<bool>,
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
        let mut lines: Vec<String> = self
            .env
            .iter()
            .map(|(key, value)| {
                let shown = if SECRET_ENV.contains(&key.as_str()) {
                    "<redacted>"
                } else {
                    value.as_str()
                };
                format!("{key}={shown} \\")
            })
            .collect();

        let mut command = vec![self.program.clone()];
        command.extend(self.args.iter().map(|a| quote_if_needed(a)));
        lines.push(command.join(" "));

        lines.join("\n")
    }
}

/// Wrap in single quotes when the argument contains a space, so the printed
/// command can be pasted back into a shell unchanged.
fn quote_if_needed(arg: &str) -> String {
    if arg.contains(' ') {
        format!("'{arg}'")
    } else {
        arg.to_string()
    }
}

/// Prepare and run (or just print) a client invocation.
///
/// Checks that the gateway is actually up first. Starting a client that will
/// fail on its first request wastes more time than a clear message here.
pub async fn run(options: Options) -> Result<()> {
    let config = Config::load()?;

    // "Auto" means the gateway classifies every request by content and
    // ignores the model name the client sent (the historical, always-on
    // behaviour). "No" means the model each agent asked for is routed as
    // sent, unclassified. Asked once per `launch` invocation — i.e. once per
    // session — rather than read from config, so switching is a keystroke,
    // not an edit.
    let auto_route = match options.auto_route {
        Some(answer) => answer,
        None => prompt_auto_route()?,
    };

    let invocation = match options.client {
        Client::Claude => claude::build(
            &config,
            crate::config::DEFAULT_ROUTE,
            options.isolate,
            auto_route,
            &options.forwarded_args,
        )?,
        Client::Codex => codex::build(
            &config,
            crate::config::DEFAULT_ROUTE,
            options.isolate,
            auto_route,
            &options.forwarded_args,
        )?,
        Client::Opencode => {
            let models = config
                .launch
                .opencode
                .as_ref()
                .map(|cfg| cfg.models.clone())
                .unwrap_or_default();
            opencode::build(
                &config,
                crate::config::DEFAULT_ROUTE,
                &models,
                options.isolate,
                auto_route,
                &options.forwarded_args,
            )?
        }
    };

    if options.print_only {
        for warning in &invocation.warnings {
            println!("warning: {warning}");
        }
        println!("{}", invocation.redacted());
        return Ok(());
    }

    let base_url = config.server.base_url();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()?;

    let mut liveness = http.get(format!("{base_url}/v1/models"));
    if let Some(api_key) = &config.server.api_key {
        liveness = liveness.bearer_auth(api_key.resolve()?);
    }
    let reachable = liveness
        .send()
        .await
        .ok()
        .filter(|r| r.status().is_success())
        .is_some();
    if !reachable {
        return Err(Error::GatewayUnreachable { url: base_url });
    }

    if let Client::Opencode = options.client {
        let models = config
            .launch
            .opencode
            .as_ref()
            .map(|cfg| cfg.models.clone())
            .unwrap_or_default();
        let wanted = opencode::resolved_models(&config, &models);
        let api_key = match &config.server.api_key {
            Some(key) => Some(key.resolve()?),
            None => None,
        };
        let missing =
            opencode::verify_models(&http, &base_url, api_key.as_deref(), &wanted).await?;
        if !missing.is_empty() {
            return Err(Error::Other(format!(
                "opencode config lists model(s) the gateway does not serve: {}\n\
                 add them to a route in config.json, or remove them from launch.opencode.models",
                missing.join(", ")
            )));
        }
    }

    for warning in &invocation.warnings {
        eprintln!("warning: {warning}");
    }

    let program_path = find_on_path(&invocation.program)
        .ok_or_else(|| Error::ClientNotFound(invocation.program.clone()))?;

    let mut cmd = std::process::Command::new(&program_path);
    cmd.args(&invocation.args);
    cmd.envs(invocation.env.iter().map(|(k, v)| (k.as_str(), v.as_str())));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` only returns on failure; success replaces this process.
        Err(Error::Io(cmd.exec()))
    }

    #[cfg(not(unix))]
    {
        let status = cmd.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Ask, once per `launch` invocation, whether the gateway should classify
/// requests automatically for this session ("yes") or route by the model
/// name each agent actually sent ("no").
///
/// Skipped — defaulting to "yes", the historical always-classify behaviour —
/// when stdin is not a terminal (e.g. piped into a script), since there is
/// nobody to answer it. `--auto`/`--no-auto` bypass the prompt entirely; see
/// [`Options::auto_route`].
fn prompt_auto_route() -> Result<bool> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return Ok(true);
    }

    print!("このセッションではモデルを自動分類しますか？ auto-classify models for this session? [Y/n] ");
    std::io::stdout().flush().ok();

    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(!matches!(input.trim().to_lowercase().as_str(), "n" | "no"))
}

/// An actionable error for a missing `launch.<client>` block.
/// Look up an executable on `PATH`, the same way a shell would.
fn find_on_path(program: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(program);
        is_executable(&candidate).then_some(candidate)
    })
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = path.metadata() else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Environment variable names whose values must be redacted when printed.
pub const SECRET_ENV: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "CODEX_API_KEY",
    "OPENCODE_CONFIG_CONTENT",
];
