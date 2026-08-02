//! `serve --ui` — the local dashboard.
//!
//! Three views sharing one page (`GET /ui`, a single self-contained HTML
//! document — see `assets/index.html`) and a handful of JSON/SSE endpoints:
//!
//! | endpoint | shows |
//! |---|---|
//! | `GET /api/live` | Server-Sent Events: one [`crate::server::live::LiveEvent`] per completed request, live |
//! | `GET /api/routes/vectors` | every route's embedding, projected to 2-D — see [`pca`] |
//! | `GET /api/usage` | the same aggregation `llm-gateway stats` prints, as JSON |
//!
//! Mounted only when `serve --ui` (or `config.ui.enabled`) is on — see
//! [`crate::server::router`].
//!
//! ## Auth
//!
//! The dashboard shows the same prompt text and routing decisions the proxy
//! itself handles, so it needs a lock on the door — but it cannot always use
//! the proxy's own lock. `server.apiKey` gates the proxy routes via an
//! `Authorization`/`x-api-key` header, and a browser has no way to attach
//! either of those to a plain page navigation (`GET /ui`) or an
//! `EventSource` (`GET /api/live`); a config with `server.apiKey` set would
//! otherwise make the dashboard unusable, and a config without one would
//! leave every route wide open (see [`ui_guard`]'s doc comment for the fix).
//!
//! So every route here answers to [`ui_guard`] instead of the proxy's
//! `auth_middleware` (see how [`crate::server::router`] layers the two
//! separately): a per-run token, generated in `serve` and printed once at
//! startup, trades for an `HttpOnly`/`SameSite=Strict` session cookie at
//! `GET /ui?token=…`, and either that cookie *or* the proxy's own header
//! credential (when `server.apiKey` is configured) gets a request through.
//! `ui_guard` also refuses any `Host` other than a loopback name, which is
//! what actually stops a third-party page from reaching these routes via DNS
//! rebinding — a cookie alone does not, since rebinding makes the browser
//! treat the attacker's origin as this one.

pub mod pca;

use std::convert::Infallible;
use std::pin::Pin;

use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, Response};
use axum::routing::get;
use axum::Router;
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;

use crate::cli::stats::{self, GroupBy, Row};
use crate::server::AppState;

/// The dashboard's routes, merged into the main router only when the
/// dashboard is on (`AppState.live.is_some()`) — see
/// [`crate::server::router`]. Kept separate from the proxy's own routes so
/// that check stays in one obvious place rather than scattered across
/// per-handler guards.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ui", get(index))
        .route("/api/live", get(live_stream))
        .route("/api/usage", get(usage))
        .route("/api/routes/vectors", get(routes_vectors))
}

/// Name of the cookie [`ui_guard`] hands out once a caller proves it holds
/// the dashboard token, and checks on every request after that.
const SESSION_COOKIE: &str = "gw_ui_token";

/// Guards every route in [`router`] — layered on in place of the proxy's own
/// `auth_middleware` (see `crate::server::router` and this module's docs on
/// why one shared layer cannot serve both).
///
/// Order of checks: `Host` first, unconditionally — a request whose `Host`
/// is not a loopback name is refused before anything else, which is the
/// actual defense against DNS rebinding (a rebound page still sends its
/// *original* hostname in `Host`, even once the browser resolves it to
/// 127.0.0.1). Then either credential gets the request through: the same
/// header `server.apiKey` accepts on the proxy routes
/// (`crate::server::header_key_ok`), or the session cookie. Lacking both,
/// `GET /ui` gets one more chance — `?token=…` — since that is the only way
/// a session cookie can ever come to exist; a valid token mints the cookie
/// on the way out.
pub async fn ui_guard(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !host_is_loopback(request.headers()) {
        return crate::server::error_response(
            http::StatusCode::FORBIDDEN,
            "the dashboard only answers to Host: 127.0.0.1, localhost, or [::1]",
        );
    }

    if let Some(expected) = state.inbound_key.as_deref() {
        if crate::server::header_key_ok(request.headers(), expected) {
            return next.run(request).await;
        }
    }

    let Some(expected_token) = state.ui_token.as_deref() else {
        // Unreachable in practice: `router()` above is only layered with
        // this guard when `state.live` is `Some`, which `serve` always sets
        // together with `ui_token` — but an honest 401 beats a panic if that
        // invariant is ever broken.
        return crate::server::error_response(
            http::StatusCode::UNAUTHORIZED,
            "dashboard token not configured",
        );
    };

    if session_cookie_matches(request.headers(), expected_token) {
        return next.run(request).await;
    }

    if request.uri().path() == "/ui" {
        if let Some(token) = token_query_param(request.uri()) {
            if token == expected_token {
                let mut response = next.run(request).await;
                response
                    .headers_mut()
                    .insert(http::header::SET_COOKIE, session_cookie(expected_token));
                return response;
            }
        }
    }

    crate::server::error_response(
        http::StatusCode::UNAUTHORIZED,
        "missing or invalid dashboard token — open the URL `serve --ui` printed at startup",
    )
}

/// True when the `Host` header names this loopback listener — with or
/// without a port, and in either IPv4 or IPv6 form. Anything else (an
/// attacker-controlled domain name that DNS rebinding pointed at 127.0.0.1,
/// for instance) is refused: see [`ui_guard`]'s doc comment.
fn host_is_loopback(headers: &http::HeaderMap) -> bool {
    let Some(host) = headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let host_only = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    host_only.eq_ignore_ascii_case("127.0.0.1")
        || host_only.eq_ignore_ascii_case("localhost")
        || host_only.eq_ignore_ascii_case("::1")
}

/// True when some `Cookie` header carries `SESSION_COOKIE=expected`.
fn session_cookie_matches(headers: &http::HeaderMap, expected: &str) -> bool {
    headers
        .get_all(http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .any(|(name, value)| name == SESSION_COOKIE && value == expected)
}

/// The bare `?token=…` query parameter, unescaped. Good enough here because
/// the only value that ever needs to compare equal is the token itself — a
/// UUID, which has nothing a browser would percent-encode.
fn token_query_param(uri: &http::Uri) -> Option<&str> {
    uri.query()?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "token").then_some(value)
    })
}

/// Build the `Set-Cookie` value [`ui_guard`] hands back once a token checks
/// out. `HttpOnly` keeps it out of reach of any script running on the page
/// (including the dashboard's own, and anything DNS rebinding lets a
/// third-party page run against this origin); `SameSite=Strict` keeps it
/// from ever being attached to a request that did not originate from a page
/// already on this origin. No `Secure`: the dashboard is loopback-only plain
/// HTTP by design (see `crate::server::bind_or_offer_to_free_port`), and
/// `Secure` would just make the cookie unusable there.
fn session_cookie(token: &str) -> http::HeaderValue {
    let value = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/");
    http::HeaderValue::from_str(&value).expect("cookie name and a UUID token are both plain ASCII")
}

/// The dashboard page itself: one file, styles and script inline. See the
/// module docs on why this is a single `include_str!` rather than several —
/// nothing here justifies a template engine or a bundler.
async fn index() -> Html<&'static str> {
    Html(include_str!("assets/index.html"))
}

/// `GET /api/live` — Server-Sent Events, one [`crate::server::live::LiveEvent`]
/// (JSON) per completed request, for as long as the tab stays open.
///
/// Never terminates on its own; a client that disconnects simply drops its
/// `Receiver`, which is exactly what a `broadcast` subscriber is for.
async fn live_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream: Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        match state.live.as_ref() {
            Some(live) => {
                let rx = live.subscribe();
                Box::pin(BroadcastStream::new(rx).filter_map(|item| async move {
                    // A lagged subscriber (see `LiveFeed`'s channel capacity)
                    // just misses the events it fell behind on — there is no
                    // way to redeliver them, and no reason to close the
                    // connection over it.
                    let event = item.ok()?;

                    // `serde_json::to_string` cannot represent NaN — no
                    // valid JSON number spells it — so a NaN `point` (a
                    // degenerate embedding, or route vectors that collapsed
                    // to a single point) makes serialization below fail and
                    // this whole event get silently dropped (#30). Catch it
                    // here instead, where it is unambiguous *why* the drop
                    // is happening, rather than downstream where all that is
                    // visible is "the dashboard shows nothing."
                    if event.point.is_some_and(|p| point_has_nan(&p)) {
                        warn_nan_once(&NAN_WARNED_LIVE, "GET /api/live");
                        return None;
                    }

                    match serde_json::to_string(&event) {
                        Ok(json) => Some(Ok(Event::default().data(json))),
                        Err(err) => {
                            // Not the NaN case (ruled out above) — an
                            // unexpected failure, so every occurrence is
                            // logged rather than throttled like the NaN
                            // warning: this path is not expected to ever
                            // run, unlike NaN, which a misbehaving embedding
                            // model could trigger continuously.
                            tracing::warn!("dropping one live event: failed to serialize: {err}");
                            None
                        }
                    }
                }))
            }
            // Unreachable in practice — `router()` above is only merged in
            // when `state.live` is `Some` — but an empty stream is a more
            // honest response than a panic if that invariant is ever broken.
            None => Box::pin(futures_util::stream::empty()),
        };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Debug, Deserialize)]
struct UsageQuery {
    #[serde(default)]
    by: Option<String>,
    since: Option<String>,
    until: Option<String>,
    #[serde(default, deserialize_with = "deserialize_truthy")]
    all: bool,
}

/// `axum::extract::Query` deserializes via `serde_urlencoded`, which forwards
/// a plain `bool` field straight to `str::parse::<bool>()` — accepting only
/// the literal strings `"true"`/`"false"`. The dashboard's own "all"
/// checkbox (`assets/index.html`) sends `all=1`, which made every checked
/// request a guaranteed 400. Accept the usual truthy spellings a query
/// string shows up with instead; anything else (including the field being
/// absent, handled by `#[serde(default)]` before this ever runs) is `false`.
fn deserialize_truthy<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(matches!(raw.as_str(), "1" | "true" | "on" | "yes"))
}

impl UsageQuery {
    fn group_by(&self) -> GroupBy {
        match self.by.as_deref() {
            Some("client") => GroupBy::Client,
            Some("provider") => GroupBy::Provider,
            Some("model") => GroupBy::Model,
            Some("day") => GroupBy::Day,
            _ => GroupBy::Route,
        }
    }
}

#[derive(serde::Serialize)]
struct UsageResponse {
    by: &'static str,
    rows: Vec<Row>,
    total: Row,
}

/// `GET /api/usage?by=route|client|provider|model|day&since=YYYY-MM-DD&until=YYYY-MM-DD&all=1`
///
/// The same aggregation `llm-gateway stats` prints, reused verbatim
/// (`crate::cli::stats::aggregate`) so the two never drift apart. Day
/// grouping and `--since`/`--until` here are always UTC, unlike the CLI's
/// local-time version: `time::UtcOffset::current_local_offset` is only sound
/// to call before a multi-threaded runtime starts (see `cli::stats::run`),
/// which `serve`'s request handlers are well past — the browser already
/// knows the viewer's timezone and is the better place to re-bucket a `day`
/// grouping if that is ever wanted. That also means this handler, unlike
/// `cli::stats::run`, never needs to resolve a local offset at all — nothing
/// below touches `current_local_offset`, so moving the work onto a blocking
/// task (next paragraph) raises no version of the "must run before a
/// multi-threaded runtime starts" concern that comment describes.
///
/// The read (`read_dir` + every `usage-*.jsonl` read whole) and the parse +
/// aggregate that follows it are all synchronous, and unbounded by log size
/// (#28) — a fine trade against `spawn_blocking`'s move-everything-in cost
/// while logs stay small, but not something an `async fn` should ever do
/// directly: once logs grow to tens of MB this would block the worker
/// thread handling it for as long as the read and parse take, delaying every
/// other request that thread would otherwise have serviced. `spawn_blocking`
/// moves both onto the blocking pool instead, where blocking is expected.
/// The result-size cap this still needs is left for a follow-up (see the
/// issue) — this change is scoped to getting the blocking work off the
/// async worker, not to changing what gets returned.
async fn usage(State(state): State<AppState>, Query(query): Query<UsageQuery>) -> Response {
    let config = state.config.get();
    let logs_dir = crate::paths::logs_dir(&config.logging.dir);
    let by = query.group_by();
    let since = if query.all { None } else { query.since };
    let until = if query.all { None } else { query.until };

    let aggregated = tokio::task::spawn_blocking(move || {
        let (records, _skipped) = stats::read_usage_records(&logs_dir);
        let rows = stats::aggregate(
            &records,
            by,
            since.as_deref(),
            until.as_deref(),
            time::UtcOffset::UTC,
        );
        let total = rows.iter().fold(Row::default(), |mut acc, row| {
            acc.calls += row.calls;
            acc.failures += row.failures;
            acc.unknown += row.unknown;
            acc.in_tok += row.in_tok;
            acc.out_tok += row.out_tok;
            acc.cache_read_tok += row.cache_read_tok;
            acc.cache_write_tok += row.cache_write_tok;
            acc
        });
        (rows, total)
    })
    .await;

    // Only panics inside the blocking closure land here (a bug, not a
    // runtime condition a caller can act on) — `stats::read_usage_records`
    // and `stats::aggregate` do not themselves return `Result`. `?` upstream
    // in `usage`'s callers is not an option here (this is the top-level
    // handler), so a 500 is the honest answer: it is the same shape a panic
    // in a directly-`await`ed async handler would produce.
    let (rows, total) = match aggregated {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!("usage aggregation task panicked: {err}");
            return crate::server::error_response(
                http::StatusCode::INTERNAL_SERVER_ERROR,
                "failed to aggregate usage logs",
            );
        }
    };

    json_response(&UsageResponse {
        by: group_by_label(by),
        rows,
        total,
    })
}

fn group_by_label(by: GroupBy) -> &'static str {
    match by {
        GroupBy::Route => "route",
        GroupBy::Client => "client",
        GroupBy::Provider => "provider",
        GroupBy::Model => "model",
        GroupBy::Day => "day",
    }
}

#[derive(serde::Serialize)]
struct RoutePoints {
    name: String,
    points: Vec<[f32; 2]>,
}

#[derive(serde::Serialize)]
struct VectorMapResponse {
    routes: Vec<RoutePoints>,
}

/// `GET /api/routes/vectors` — every candidate route's embedding(s),
/// projected to 2-D via [`pca`]. See [`crate::server::live::LiveEvent::point`]
/// for how an incoming request's position on this same map is computed.
#[cfg(feature = "semantic")]
async fn routes_vectors(State(state): State<AppState>) -> Response {
    let Some(classifier) = state.classifier.as_ref() else {
        return json_response(&VectorMapResponse { routes: Vec::new() });
    };

    let named = classifier.route_vectors();
    let generation = state.config.generation();
    // Only clones every route's vectors a second time (for the fit itself)
    // on a cache miss — see `pca::BasisCache`. `named` is needed regardless,
    // to build the per-route points below.
    let basis = state.basis_cache.get_or_fit(generation, || {
        named.iter().flat_map(|(_, vs)| vs.clone()).collect()
    });

    let Some(basis) = basis else {
        return json_response(&VectorMapResponse { routes: Vec::new() });
    };

    let mut any_nan = false;
    let routes = named
        .into_iter()
        .map(|(name, vectors)| {
            let points: Vec<[f32; 2]> = vectors.iter().map(|v| basis.project(v)).collect();
            any_nan |= points.iter().any(point_has_nan);
            RoutePoints { name, points }
        })
        .collect();

    if any_nan {
        warn_nan_once(&NAN_WARNED_VECTORS, "GET /api/routes/vectors");
    }

    json_response(&VectorMapResponse { routes })
}

/// A build without the `semantic` feature has no embeddings to plot — the map
/// view shows "not available" for it rather than a 404, since it is still a
/// meaningful answer ("this build cannot classify, so there is nothing to
/// show you") rather than a missing endpoint.
#[cfg(not(feature = "semantic"))]
async fn routes_vectors(State(_state): State<AppState>) -> Response {
    json_response(&VectorMapResponse { routes: Vec::new() })
}

/// Project `text`'s embedding onto a 2-D basis fit from the current route
/// set — the same computation [`routes_vectors`] does, for the live feed's
/// `point` field (see `crate::server::live::LiveEvent`).
///
/// Recomputes its own basis rather than sharing one cached from
/// `routes_vectors`: shares `state.basis_cache` with it rather than always
/// fitting its own — both land on the same axes either way (fitting is
/// deterministic on the same data, see `pca`'s module docs), so reusing
/// whichever fit is already current for `state.config`'s generation costs
/// nothing in correctness and, on every call after the first per generation,
/// skips a second embedding-vector clone and `Basis::fit` entirely (#27).
///
/// Only called (see `crate::server::proxy::live_point`) once the caller has
/// already checked `state.live`'s `LiveFeed::has_subscribers` — with no tab
/// open there is nobody to show the point to, so the embed-and-project work
/// this does is skipped upstream of here rather than wasted on it.
#[cfg(feature = "semantic")]
pub fn project_point(
    state: &AppState,
    classifier: &crate::semantic::index::Classifier,
    text: &str,
) -> Option<[f32; 2]> {
    let vector = classifier.embed(text)?;
    let generation = state.config.generation();
    let basis = state.basis_cache.get_or_fit(generation, || {
        classifier
            .route_vectors()
            .into_iter()
            .flat_map(|(_, vs)| vs)
            .collect()
    })?;
    Some(basis.project(&vector))
}

/// True when either coordinate is NaN — the one shape a projected point can
/// take that `serde_json` cannot serialize, since JSON has no spelling for
/// it. A pure, standalone function so #30's detection logic is unit-testable
/// without going through an HTTP handler or the SSE stream.
fn point_has_nan(point: &[f32; 2]) -> bool {
    point.iter().any(|c| c.is_nan())
}

/// Guards the `tracing::warn!` calls below so a run of NaN-producing
/// projections (a degenerate embedding model, or route vectors that
/// happened to collapse to a single point) logs once per endpoint instead of
/// once per SSE event or per dashboard poll — the fact worth knowing is
/// "this is happening at all," and either endpoint could otherwise flood the
/// log for as long as the condition persists.
static NAN_WARNED_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// Only `routes_vectors` (`#[cfg(feature = "semantic")]`) uses this one — the
// build without `semantic` has no embeddings, so nothing there can ever
// produce a NaN point to warn about.
#[cfg(feature = "semantic")]
static NAN_WARNED_VECTORS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Returns whether this call actually logged (the first call on a fresh
/// `warned`) — a plain `bool` so the once-per-endpoint throttling is
/// assertable in a test without capturing `tracing` output.
fn warn_nan_once(warned: &std::sync::atomic::AtomicBool, endpoint: &str) -> bool {
    let already_warned = warned.swap(true, std::sync::atomic::Ordering::Relaxed);
    if !already_warned {
        tracing::warn!(
            "{endpoint}: a projected point contains NaN — the embedding model or the \
             current route vectors may have degenerated; affected point(s) are dropped \
             rather than sent (further occurrences on this endpoint are not logged)"
        );
    }
    !already_warned
}

fn json_response<T: serde::Serialize>(body: &T) -> Response {
    let payload = serde_json::to_string(body).unwrap_or_else(|_| "{}".to_string());
    let mut response = Response::new(axum::body::Body::from(payload));
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn point_has_nan_detects_either_coordinate() {
        assert!(!point_has_nan(&[0.0, 0.0]));
        assert!(!point_has_nan(&[1.5, -2.5]));
        assert!(point_has_nan(&[f32::NAN, 0.0]));
        assert!(point_has_nan(&[0.0, f32::NAN]));
        assert!(point_has_nan(&[f32::NAN, f32::NAN]));
    }

    #[test]
    fn warn_nan_once_only_warns_the_first_time() {
        let warned = AtomicBool::new(false);
        assert!(warn_nan_once(&warned, "test"), "first call should warn");
        assert!(
            !warn_nan_once(&warned, "test"),
            "second call on the same flag should stay quiet"
        );
        assert!(
            !warn_nan_once(&warned, "test"),
            "third call on the same flag should stay quiet too"
        );
    }

    #[test]
    fn warn_nan_once_is_independent_per_flag() {
        // NAN_WARNED_LIVE and NAN_WARNED_VECTORS must not share state — a
        // NaN on one endpoint should not silence the warning on the other.
        let live = AtomicBool::new(false);
        let vectors = AtomicBool::new(false);
        assert!(warn_nan_once(&live, "live"));
        assert!(warn_nan_once(&vectors, "vectors"));
    }
}
