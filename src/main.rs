//! `llm-gateway` — one local endpoint in front of every agent CLI.
//!
//! The gateway speaks all three wire protocols its clients need
//! (`/v1/messages`, `/v1/chat/completions`, `/v1/responses`), rewrites only the
//! `model` field of the request, and streams the response back byte-for-byte.
//! Not touching the response body is what keeps it correct: there is no
//! translation layer to get subtly wrong.
//!
//! `launch` starts a client with the right environment injected, so no existing
//! client config file is ever modified.
//!
//! All logic lives in the library crate (`src/lib.rs`); this file is only the
//! clap surface, so integration tests can drive the same code over real TCP.

use clap::{Args, Parser, Subcommand};

use llm_gateway::error::Result;
use llm_gateway::{cli, launch, server};

#[derive(Parser)]
#[command(
    name = "llm-gateway",
    version,
    about = "One local endpoint for Claude Code, Codex CLI, opencode and OpenClaw",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the gateway.
    Serve(ServeArgs),

    /// Create `~/.config/llm-gateway/config.json` interactively.
    Init,

    /// Start an agent CLI pointed at the gateway, without editing its config.
    Launch(LaunchArgs),

    /// Inspect and check configuration.
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Summarise recorded usage.
    Stats(StatsArgs),

    /// Show routing decisions recorded with `--debug`.
    Trace(TraceArgs),

    /// Check that every configured provider is reachable.
    Providers,

    /// Check for a newer release and install it.
    Update(UpdateArgs),
}

#[derive(Args)]
struct UpdateArgs {
    /// Report what is available without installing anything.
    #[arg(long)]
    check: bool,
}

#[derive(Args)]
struct ServeArgs {
    /// Record full routing decisions to `logs/trace-YYYY-MM-DD.jsonl`.
    ///
    /// This writes prompt text. User messages are truncated to 200 characters
    /// unless `--debug-full` is also given.
    #[arg(long)]
    debug: bool,

    /// Record untruncated prompt text. Implies `--debug`.
    ///
    /// Business conversations end up on disk in the clear — use deliberately.
    #[arg(long)]
    debug_full: bool,

    /// Override `server.port`.
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Args)]
struct LaunchArgs {
    /// Which client to start.
    #[arg(value_enum)]
    client: launch::Client,

    /// Override the route from `launch.<client>.model`.
    #[arg(long)]
    model: Option<String>,

    /// Also stop the client from reading its own user-level settings.
    ///
    /// Off by default: for Claude Code this would also discard permissions and
    /// hooks, which is rarely what someone wants just to change an endpoint.
    #[arg(long)]
    isolate: bool,

    /// Print the command and environment instead of running anything.
    #[arg(long)]
    print: bool,

    /// Arguments forwarded verbatim to the client.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Validate the config file and report every problem at once.
    Check,

    /// Print the effective config with secrets masked.
    Show,

    /// Print a `.gitignore` suitable for a directory holding this config.
    Gitignore,
}

#[derive(Args)]
struct StatsArgs {
    /// Grouping key.
    #[arg(long = "by", value_enum, default_value_t = cli::stats::GroupBy::Route)]
    by: cli::stats::GroupBy,

    /// Inclusive lower bound, `YYYY-MM-DD`.
    #[arg(long)]
    since: Option<String>,

    /// Inclusive upper bound, `YYYY-MM-DD`.
    #[arg(long)]
    until: Option<String>,
}

#[derive(Args)]
struct TraceArgs {
    /// Follow the file as it grows.
    #[arg(long)]
    tail: bool,

    /// Only show entries for this route.
    #[arg(long)]
    route: Option<String>,

    /// Only show entries for this client.
    #[arg(long)]
    client: Option<String>,
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("llm-gateway: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve(args) => {
            let debug = args.debug || args.debug_full;
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(server::serve(server::ServeOptions {
                debug,
                debug_full: args.debug_full,
                port_override: args.port,
            }))
        }

        Command::Init => cli::init::run(),

        Command::Launch(args) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(launch::run(launch::Options {
                client: args.client,
                model_override: args.model,
                isolate: args.isolate,
                print_only: args.print,
                forwarded_args: args.args,
            }))
        }

        Command::Config(ConfigCommand::Check) => cli::config_cmd::check(),
        Command::Config(ConfigCommand::Show) => cli::config_cmd::show(),
        Command::Config(ConfigCommand::Gitignore) => cli::config_cmd::gitignore(),

        Command::Stats(args) => cli::stats::run(cli::stats::Options {
            by: args.by,
            since: args.since,
            until: args.until,
        }),

        Command::Trace(args) => cli::trace::run(cli::trace::Options {
            tail: args.tail,
            route: args.route,
            client: args.client,
        }),

        Command::Providers => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(cli::providers::run())
        }

        Command::Update(args) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(cli::update::run(cli::update::Options {
                check_only: args.check,
            }))
        }
    }
}
