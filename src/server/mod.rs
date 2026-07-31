//! The HTTP surface.
//!
//! Five endpoints, chosen because between them they cover every client:
//!
//! | endpoint | protocol | who needs it |
//! |---|---|---|
//! | `POST /v1/messages` | Anthropic Messages | Claude Code |
//! | `POST /v1/messages/count_tokens` | Anthropic Messages | Claude Code (context accounting) |
//! | `POST /v1/chat/completions` | OpenAI Chat | opencode, OpenClaw |
//! | `POST /v1/responses` | OpenAI Responses | Codex CLI |
//! | `GET /v1/models` | — | opencode (it fails silently on a name mismatch) |
//!
//! Handlers are deliberately thin: read the body, rewrite `model`, ask
//! [`crate::upstream`] to find a target, forward. All the interesting decisions
//! live in `route`, `upstream` and `passthrough`.

pub mod chat;
pub mod messages;
pub mod models;
pub mod passthrough;
pub mod responses;

mod proxy;

use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

use crate::config::watch::SharedConfig;
use crate::config::ApiKind;
#[cfg(not(feature = "semantic"))]
use crate::config::Config;
use crate::error::{Error, Result};
use crate::record::{RecordMode, Recorder};
use crate::{paths, upstream};

pub use proxy::proxy;

/// Options from the `serve` subcommand.
pub struct ServeOptions {
    pub debug: bool,
    pub debug_full: bool,
    pub port_override: Option<u16>,
}

/// Everything a handler needs.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<SharedConfig>,
    pub http: reqwest::Client,
    pub recorder: Arc<Recorder>,
    /// Resolved once at startup. Changing `server.apiKey` needs a restart —
    /// like host and port, it is part of the listener's identity, and
    /// re-resolving a Keychain reference per request would prompt constantly.
    pub inbound_key: Option<String>,
    /// Loaded once at startup by [`prepare_classifier`] — every request is
    /// classified, so this is always `Some` on a normally started server.
    /// Kept `Option` (rather than a bare `Arc`) so tests can exercise
    /// `classify_request`'s fallback path without a real ~500MB embedding
    /// model loaded; `serve` itself never leaves it `None`. Absent entirely
    /// (not even the field exists) in a build without the `semantic`
    /// feature; see the warning `serve` logs in that case.
    #[cfg(feature = "semantic")]
    pub classifier: Option<Arc<crate::semantic::index::Classifier>>,
}

/// Load config, bind, and serve until interrupted.
pub async fn serve(options: ServeOptions) -> Result<()> {
    let shared = SharedConfig::load(paths::config_file())?;
    let config = shared.get();

    init_tracing(config.logging.logging);

    let host = config.server.host.clone();
    let port = options.port_override.unwrap_or(config.server.port);

    // Bound first, before the slower recorder/classifier setup below: a port
    // conflict is caught in milliseconds this way instead of after a
    // multi-second classification model load, and the interactive prompt (if
    // any) happens before anything else has been started.
    let listener = bind_or_offer_to_free_port(&host, port).await?;

    // Validation already refuses non-loopback + no key, but validation runs
    // against the file — re-check here so a future code path can't bypass it.
    let inbound_key = match &config.server.api_key {
        Some(secret) => Some(secret.resolve()?),
        None if !config.server.is_loopback() => {
            return Err(Error::Other(format!(
                "refusing to bind {host} without server.apiKey: \
                 one key would expose every configured provider to the network"
            )));
        }
        None => None,
    };

    let mode = RecordMode {
        usage: config.logging.usage,
        debug: options.debug || config.logging.debug,
        debug_full: options.debug_full,
    };
    let recorder = Recorder::start(paths::logs_dir(&config.logging.dir), mode)?;

    let http = upstream::client()?;

    #[cfg(feature = "semantic")]
    let classifier = Some(prepare_classifier(&shared, &http).await?);
    #[cfg(not(feature = "semantic"))]
    warn_if_semantic_routes_are_unusable(&config);

    let state = AppState {
        config: shared.clone(),
        http,
        recorder,
        inbound_key,
        #[cfg(feature = "semantic")]
        classifier,
    };

    // Watch after the first successful load: a broken edit from here on keeps
    // the old config serving.
    let _watch = match crate::config::watch::spawn(shared) {
        Ok(guard) => Some(guard),
        Err(err) => {
            tracing::warn!("config hot-reload disabled: {err}");
            None
        }
    };

    // Always shown, independent of `logging.logging`: this is the startup
    // confirmation every server-like CLI prints, not a diagnostic.
    eprintln!("llm-gateway listening on http://{host}:{port}");
    if mode.debug {
        eprintln!(
            "trace recording is ON — prompt text is written to disk{}",
            if mode.debug_full {
                " (untruncated)"
            } else {
                " (truncated to 200 chars)"
            }
        );
    }

    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Bind `host:port`, offering to free it when something else already has it.
///
/// That "something else" is almost always a previous `llm-gateway serve` left
/// running, and finding that out by scrolling past a multi-second classifier
/// load first is not a good way to learn it — hence this runs before any of
/// that setup starts. Asks before killing anything, and only ever targets the
/// process(es) actually listening on this exact port, never every
/// `llm-gateway` process on the machine (which could include one serving a
/// different port on purpose).
async fn bind_or_offer_to_free_port(host: &str, port: u16) -> Result<tokio::net::TcpListener> {
    match tokio::net::TcpListener::bind((host, port)).await {
        Ok(listener) => Ok(listener),
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            let pids = pids_listening_on(port);
            if pids.is_empty() {
                return Err(Error::Other(format!(
                    "port {port} is already in use, but the process holding it could not be \
                     identified (see `lsof -i :{port}`) — free it manually and try again"
                )));
            }
            cliclack::log::warning(format!(
                "port {port} is already in use by {} (pid {})",
                if pids.len() == 1 {
                    "another process"
                } else {
                    "other processes"
                },
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            ))?;
            let kill = cliclack::confirm(format!(
                "kill {} and start this one instead?",
                if pids.len() == 1 { "it" } else { "them" }
            ))
            .initial_value(false)
            .interact()?;
            if !kill {
                return Err(Error::Other(format!(
                    "port {port} is still in use — not starting; stop the other process, or \
                     pass a different port with `--port`"
                )));
            }
            for pid in &pids {
                kill_pid(*pid);
            }
            // The kernel needs a moment to actually release the socket after
            // the process holding it dies; poll briefly rather than fail on
            // the first attempt back.
            let mut last_err = None;
            for attempt in 0..20 {
                match tokio::net::TcpListener::bind((host, port)).await {
                    Ok(listener) => return Ok(listener),
                    Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                        last_err = Some(e);
                        if attempt < 19 {
                            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        }
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            Err(Error::Other(format!(
                "killed pid(s) {} but port {port} is still in use after 3s: {}",
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                last_err.expect("loop only exits via return or after recording last_err"),
            )))
        }
        Err(err) => Err(err.into()),
    }
}

/// PIDs of processes with an open listening socket on `port`, via `lsof` —
/// present on macOS and virtually every Linux distro, unlike `ss`/`fuser`
/// whose output shape differs enough between distros to not be worth parsing.
#[cfg(unix)]
fn pids_listening_on(port: u16) -> Vec<u32> {
    let output = std::process::Command::new("lsof")
        .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
        .output();
    match output {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

#[cfg(not(unix))]
fn pids_listening_on(_port: u16) -> Vec<u32> {
    // No portable, dependency-free way to map a port to a PID on Windows —
    // callers treat an empty result as "identify and free it manually".
    Vec::new()
}

/// Ask the process holding the port to shut down. `SIGTERM` first — this is
/// normally another `llm-gateway serve`, which shuts its recorder and config
/// watcher down cleanly on it — with `kill(1)`'s own exit code ignored: a
/// process that already exited between `lsof` and here is not a failure.
#[cfg(unix)]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

#[cfg(not(unix))]
fn kill_pid(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status();
}

/// Prepare the embedding model and build the initial [`crate::semantic::index::Classifier`].
///
/// Blocks: `model_file::ensure` may download ~512MB on first run (progress
/// goes to stderr already) and `Embedder::load` takes 1.5-4s, so the load
/// runs on a blocking task rather than tying up an async worker. Blocking
/// startup on this is a deliberate choice — every request is classified now,
/// so there is no "maybe skip it" case left — but it must be visible in the
/// logs, hence the `info!` calls around it.
#[cfg(feature = "semantic")]
async fn prepare_classifier(
    shared: &Arc<SharedConfig>,
    http: &reqwest::Client,
) -> Result<Arc<crate::semantic::index::Classifier>> {
    tracing::info!(
        "preparing the embedding model for content classification \
         (may download ~512MB on first run)..."
    );
    let files = crate::semantic::model_file::ensure(http).await?;

    let shared = Arc::clone(shared);
    let embedder =
        tokio::task::spawn_blocking(move || crate::semantic::embed::Embedder::load(&files))
            .await
            .map_err(|err| {
                Error::Other(format!("semantic model loading task panicked: {err}"))
            })??;
    tracing::info!("classification model loaded");

    Ok(Arc::new(crate::semantic::index::Classifier::new(
        shared, embedder,
    )))
}

/// Warn once, at startup, that this build cannot classify requests at all.
///
/// Without the `semantic` feature every request falls back to the reserved
/// `default` route unconditionally — still a working gateway, just not the
/// classifying one.
#[cfg(not(feature = "semantic"))]
fn warn_if_semantic_routes_are_unusable(_config: &Config) {
    tracing::warn!(
        "this build does not have the `semantic` feature enabled; every request will be routed \
         to the reserved `{}` route without classification. Rebuild with the default features \
         (or `--features semantic`) to enable content-based routing.",
        crate::config::DEFAULT_ROUTE
    );
}

/// Set up the console (stderr) log.
///
/// Off by default (`verbose == false`, i.e. `config.logging.logging` unset):
/// only `warn!`/`error!` reach the console, which hides the routine
/// diagnostics — embedding-model preparation, which route/provider was
/// picked, per-attempt fallback outcomes — emitted at `info!` elsewhere in
/// this crate. Setting `logging.logging` to `true` raises the default level
/// to `info` so those show. An explicit `RUST_LOG` always wins over both,
/// for whoever wants finer control.
fn init_tracing(verbose: bool) {
    use tracing_subscriber::EnvFilter;
    let default_level = if verbose { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    // Ignore a second call (tests may race); the first subscriber wins.
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Build the router. Split out so tests can drive it without binding a port.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/messages", post(messages::handle))
        .route("/v1/messages/count_tokens", post(messages::count_tokens))
        .route("/v1/chat/completions", post(chat::handle))
        .route("/v1/responses", post(responses::handle))
        .route("/v1/models", get(models::handle))
        .route("/health", get(|| async { "ok" }))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .with_state(state)
}

/// Reject requests that lack the inbound key, when one is configured.
///
/// Accepts either header form because the clients differ: Claude Code sends
/// `Authorization: Bearer …` (from `ANTHROPIC_AUTH_TOKEN`) or `x-api-key`
/// (from `ANTHROPIC_API_KEY`), and the OpenAI-protocol clients send Bearer.
async fn auth_middleware(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(expected) = &state.inbound_key else {
        return next.run(request).await;
    };

    // /health stays open: liveness probes should not need a secret.
    if request.uri().path() == "/health" {
        return next.run(request).await;
    }

    let headers = request.headers();
    let bearer_ok = headers
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|token| token == expected);
    let api_key_ok = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|token| token == expected);

    if bearer_ok || api_key_ok {
        next.run(request).await
    } else {
        error_response(
            http::StatusCode::UNAUTHORIZED,
            "invalid or missing gateway API key",
        )
    }
}

/// Identify the caller for logging.
///
/// `launch` always injects `x-gw-client`, so a request without it either came
/// from a manually configured client or from something unexpected — both worth
/// being able to tell apart in the logs.
pub fn client_name(headers: &http::HeaderMap) -> String {
    headers
        .get("x-gw-client")
        .or_else(|| headers.get(http::header::USER_AGENT))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// A JSON error body in a shape every client tolerates.
///
/// Anthropic and OpenAI clients disagree on error envelopes, but both surface
/// `error.message` — so that is the one field guaranteed to reach a human.
pub fn error_response(status: http::StatusCode, message: &str) -> axum::response::Response {
    let body = serde_json::json!({
        "error": {
            "type": "gateway_error",
            "message": message,
        }
    });
    let mut response = axum::response::Response::new(axum::body::Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

/// The [`ApiKind`] an inbound endpoint speaks. Used to refuse a route whose
/// providers speak a different protocol than the caller.
pub fn endpoint_api(path: &str) -> Option<ApiKind> {
    match path {
        "/v1/messages" | "/v1/messages/count_tokens" => Some(ApiKind::AnthropicMessages),
        "/v1/chat/completions" => Some(ApiKind::OpenaiChat),
        "/v1/responses" => Some(ApiKind::OpenaiResponses),
        _ => None,
    }
}
