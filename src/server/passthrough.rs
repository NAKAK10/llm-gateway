//! Handing an upstream response to the client without touching its body.
//!
//! This module is the reason the gateway can be trusted with streaming: it never
//! parses, buffers or re-serialises a response. `reqwest`'s byte stream is
//! attached straight to an `axum` body, so whatever the upstream emitted is what
//! the client sees, chunk boundaries included.
//!
//! The subtle part is headers, not bodies. Three of them describe how the body
//! was framed upstream and become lies the moment the body is re-framed by our
//! own server:
//!
//! - `content-length` — recomputed by axum/hyper; a stale value makes the client
//!   wait forever for bytes that will never come.
//! - `transfer-encoding` — the new connection has its own framing.
//! - `content-encoding` — the request is sent with `accept-encoding: identity`,
//!   so the body is not actually encoded; advertising otherwise makes the client
//!   try to inflate plain text.
//!
//! Everything else is forwarded, because provider-specific headers (rate-limit
//! counters, request ids) are genuinely useful to the client and to debugging.

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures_util::Stream;
use http::header::{HeaderMap, HeaderName, HeaderValue};

/// Response headers that must not be copied from the upstream.
const STRIPPED: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "content-encoding",
    // Hop-by-hop headers: meaningful only for the upstream connection.
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
];

/// Request headers that must not be forwarded upstream.
///
/// Authorization is dropped because the gateway substitutes the provider's own
/// credential — forwarding the client's gateway token would leak it to every
/// provider and, worse, some providers would try to authenticate with it.
const NOT_FORWARDED: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "content-length",
    "connection",
    "keep-alive",
    "accept-encoding",
    "transfer-encoding",
    // Ours, not the upstream's business.
    "x-gw-client",
    "x-gw-auto-route",
];

/// Build the header set to send upstream.
///
/// `accept-encoding: identity` is set explicitly: compression would put a
/// decoder between us and the wire, and a decoder re-chunks. For a streaming
/// protocol that shows up as visible stalls.
pub fn upstream_headers(inbound: &HeaderMap, extra: &[(String, String)]) -> HeaderMap {
    let mut out = HeaderMap::new();

    for (name, value) in inbound.iter() {
        if NOT_FORWARDED.contains(&name.as_str()) {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }

    out.insert(
        http::header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );

    for (name, value) in extra {
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            out.insert(n, v);
        }
    }

    out
}

/// Turn an upstream response into a client response, forwarding the body as a
/// stream.
///
/// `body` is taken separately from `parts` so callers can wrap the stream (to
/// observe usage) without this function knowing or caring.
pub fn respond<S>(status: http::StatusCode, upstream_headers: &HeaderMap, body: S) -> Response
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let mut response = Response::new(Body::from_stream(body));
    *response.status_mut() = status;

    let headers = response.headers_mut();
    for (name, value) in upstream_headers.iter() {
        if STRIPPED.contains(&name.as_str()) {
            continue;
        }
        headers.insert(name.clone(), value.clone());
    }

    // Ask every intermediary not to buffer. nginx and friends will otherwise
    // hold a streaming response until it completes, which turns token-by-token
    // output into one long pause followed by everything at once.
    if is_event_stream(upstream_headers) {
        headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
        headers.insert(
            http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        );
    }

    response
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/event-stream"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::try_from(*k).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn framing_headers_are_not_copied_downstream() {
        let upstream = headers(&[
            ("content-type", "text/event-stream"),
            ("content-length", "1234"),
            ("transfer-encoding", "chunked"),
            ("content-encoding", "gzip"),
            ("anthropic-ratelimit-requests-remaining", "42"),
        ]);
        let response = respond(
            http::StatusCode::OK,
            &upstream,
            futures_util::stream::empty(),
        );
        let got = response.headers();

        assert!(got.get("content-length").is_none());
        assert!(got.get("transfer-encoding").is_none());
        assert!(got.get("content-encoding").is_none());
        // Provider metadata is useful and must survive.
        assert_eq!(
            got.get("anthropic-ratelimit-requests-remaining").unwrap(),
            "42"
        );
    }

    #[test]
    fn streaming_responses_disable_intermediary_buffering() {
        let upstream = headers(&[("content-type", "text/event-stream")]);
        let response = respond(
            http::StatusCode::OK,
            &upstream,
            futures_util::stream::empty(),
        );
        assert_eq!(response.headers().get("x-accel-buffering").unwrap(), "no");
        assert_eq!(response.headers().get("cache-control").unwrap(), "no-cache");
    }

    #[test]
    fn non_streaming_responses_are_left_alone() {
        let upstream = headers(&[("content-type", "application/json")]);
        let response = respond(
            http::StatusCode::OK,
            &upstream,
            futures_util::stream::empty(),
        );
        assert!(response.headers().get("x-accel-buffering").is_none());
    }

    #[test]
    fn status_is_preserved() {
        let response = respond(
            http::StatusCode::TOO_MANY_REQUESTS,
            &HeaderMap::new(),
            futures_util::stream::empty(),
        );
        assert_eq!(response.status(), http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[test]
    fn client_credentials_are_never_forwarded_upstream() {
        let inbound = headers(&[
            ("authorization", "Bearer sk-gateway-token"),
            ("x-api-key", "sk-gateway-token"),
            ("x-gw-client", "claude-code"),
            ("anthropic-version", "2023-06-01"),
        ]);
        let out = upstream_headers(&inbound, &[]);

        assert!(out.get("authorization").is_none());
        assert!(out.get("x-api-key").is_none());
        assert!(out.get("x-gw-client").is_none());
        // Protocol headers the upstream actually needs must pass through.
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn compression_is_refused_so_chunk_boundaries_survive() {
        let inbound = headers(&[("accept-encoding", "gzip, br")]);
        let out = upstream_headers(&inbound, &[]);
        assert_eq!(out.get("accept-encoding").unwrap(), "identity");
    }

    #[test]
    fn provider_headers_are_added() {
        let out = upstream_headers(
            &HeaderMap::new(),
            &[("x-title".to_string(), "llm-gateway".to_string())],
        );
        assert_eq!(out.get("x-title").unwrap(), "llm-gateway");
    }
}
