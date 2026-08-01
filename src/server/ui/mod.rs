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
//! [`crate::server::router`] — and, like every other endpoint, behind
//! `server.apiKey` when one is configured. There is no separate auth story
//! for the dashboard: it sees the same prompt text and routing decisions the
//! proxy itself handles, so it gets the same lock on the door.

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
                    let json = serde_json::to_string(&event).ok()?;
                    Some(Ok(Event::default().data(json)))
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
    #[serde(default)]
    all: bool,
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
/// grouping if that is ever wanted.
async fn usage(State(state): State<AppState>, Query(query): Query<UsageQuery>) -> Response {
    let config = state.config.get();
    let logs_dir = crate::paths::logs_dir(&config.logging.dir);
    let (records, _skipped) = stats::read_usage_records(&logs_dir);

    let by = query.group_by();
    let since = if query.all {
        None
    } else {
        query.since.as_deref()
    };
    let until = if query.all {
        None
    } else {
        query.until.as_deref()
    };

    let rows = stats::aggregate(&records, by, since, until, time::UtcOffset::UTC);
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
    let all_vectors: Vec<Vec<f32>> = named.iter().flat_map(|(_, vs)| vs.clone()).collect();

    let Some(basis) = pca::Basis::fit(&all_vectors) else {
        return json_response(&VectorMapResponse { routes: Vec::new() });
    };

    let routes = named
        .into_iter()
        .map(|(name, vectors)| RoutePoints {
            name,
            points: vectors.iter().map(|v| basis.project(v)).collect(),
        })
        .collect();

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
/// `routes_vectors`: fitting is cheap (see `pca`'s module docs) and
/// deterministic on the same data, so a fresh fit here lands on the same
/// axes as the map view's, without either endpoint needing to hold state for
/// the other.
#[cfg(feature = "semantic")]
pub fn project_point(
    classifier: &crate::semantic::index::Classifier,
    text: &str,
) -> Option<[f32; 2]> {
    let vector = classifier.embed(text)?;
    let all_vectors: Vec<Vec<f32>> = classifier
        .route_vectors()
        .into_iter()
        .flat_map(|(_, vs)| vs)
        .collect();
    let basis = pca::Basis::fit(&all_vectors)?;
    Some(basis.project(&vector))
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
