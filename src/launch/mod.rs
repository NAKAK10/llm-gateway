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

    let invocation = match options.client {
        Client::Claude => {
            let cfg = config
                .launch
                .claude
                .as_ref()
                .ok_or_else(|| missing_launch_config(Client::Claude))?;
            let model = options
                .model_override
                .clone()
                .unwrap_or_else(|| cfg.model.clone());
            claude::build(&config, &model, options.isolate, &options.forwarded_args)?
        }
        Client::Codex => {
            let cfg = config
                .launch
                .codex
                .as_ref()
                .ok_or_else(|| missing_launch_config(Client::Codex))?;
            let model = options
                .model_override
                .clone()
                .unwrap_or_else(|| cfg.model.clone());
            codex::build(&config, &model, options.isolate, &options.forwarded_args)?
        }
        Client::Opencode => {
            let cfg = config
                .launch
                .opencode
                .as_ref()
                .ok_or_else(|| missing_launch_config(Client::Opencode))?;
            let model = options
                .model_override
                .clone()
                .unwrap_or_else(|| cfg.model.clone());
            let models = cfg.models.clone();
            opencode::build(
                &config,
                &model,
                &models,
                options.isolate,
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
        // Presence was already checked above when building the invocation.
        let cfg = config
            .launch
            .opencode
            .as_ref()
            .expect("launch.opencode presence already checked");
        let wanted = opencode::resolved_models(&config, &cfg.models);
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

/// An actionable error for a missing `launch.<client>` block.
fn missing_launch_config(client: Client) -> Error {
    let key = client.program();
    Error::Other(format!(
        "launch.{key} is not configured\n\
         add launch.{key}.model to config.json (the route to use for `llm-gateway launch {key}`)"
    ))
}

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
