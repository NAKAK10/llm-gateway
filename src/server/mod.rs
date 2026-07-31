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
    init_tracing();

    let shared = SharedConfig::load(paths::config_file())?;
    let config = shared.get();

    let host = config.server.host.clone();
    let port = options.port_override.unwrap_or(config.server.port);

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

    let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    tracing::info!("llm-gateway listening on http://{host}:{port}");
    if mode.debug {
        tracing::info!(
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

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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
