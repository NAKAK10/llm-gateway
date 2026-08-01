//! The one code path every proxied request goes through.
//!
//! All four POST endpoints do the same five things: parse the body far enough
//! to read `model`, resolve a route, rewrite `model` per target, forward with
//! fallback, and record what happened. The endpoints differ only in protocol
//! constants, so the logic lives here once.
//!
//! The request body *is* parsed (that is unavoidable — `model` must be
//! rewritten); the response body is not. See `passthrough` for why that
//! asymmetry is the point.
//!
//! The one exception is a **translated route** — a client whose protocol
//! differs from the target provider's, which used to be refused outright. There
//! the request is rebuilt and the response is rebuilt on the way back (see
//! `crate::translate`). Both paths live side by side in this file, and which
//! one a request took is recorded in the trace log's `resolved.translation`.
//!
//! Translation is decided **per target**, not once for the whole route: a
//! route's default and its fallbacks may speak different protocols
//! (cross-protocol fallback), so which translation — if any — applies can
//! differ from one attempt to the next. [`filter_reachable_targets`] drops
//! whatever the client's protocol cannot reach (directly or through
//! translation) before `upstream::send_with_fallback` ever sees the target
//! list.

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::response::Response;
use http::HeaderMap;
use time::format_description::well_known::Rfc3339;

use crate::config::{ApiKind, Config};
use crate::error::Error;
use crate::record::trace_log::{
    TraceCandidate, TraceInput, TraceRecord, TraceResolved, TraceRouting, TraceUsage, TraceWalkStep,
};
use crate::record::usage_log::UsageRecord;
use crate::record::Recorder;
use crate::route;
use crate::server::{client_name, endpoint_api, error_response, passthrough, AppState};
use crate::translate::adapter::{self, ResponseShape};
use crate::translate::Translation;
use crate::upstream::{self, Attempt};
use crate::usage::tee::{self, StreamOutcome};

/// The classification match threshold, mirrored here so it is available
/// (for trace-log text and tests) even in a `--no-default-features` build
/// that never compiles `crate::semantic`. Must stay equal to
/// `crate::semantic::index::CLASSIFICATION_THRESHOLD` when that module is
/// compiled in.
#[cfg(feature = "semantic")]
use crate::semantic::index::CLASSIFICATION_THRESHOLD;
#[cfg(not(feature = "semantic"))]
const CLASSIFICATION_THRESHOLD: f32 = 0.45;

/// How many of the request's user texts (newest first) classification tries
/// before falling back to the reserved `default` route.
///
/// Agentic clients routinely send turns whose newest user message is a
/// `tool_result` with no text, or a short reply ("continue", "yes") that
/// scores below the threshold on its own. The conversation history that
/// arrives with every request already holds the instruction such a turn is
/// continuing, so classification walks back to the most recent user text
/// that clears the threshold instead of keeping per-conversation state in
/// the gateway. The bound exists only to cap work on pathological inputs;
/// each embed is sub-millisecond, so eight is generous, not tight.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
const HISTORY_WALK_LIMIT: usize = 8;

/// Whether classification should run for this request.
///
/// Defaults to `true` (the historical always-classify behaviour) so a
/// request from anything other than `llm-gateway launch` — a manually
/// configured client, curl, etc. — is unaffected. `llm-gateway launch` sets
/// `x-gw-auto-route: 0` when the session answered "no" to its auto-classify
/// prompt.
fn auto_route_requested(headers: &HeaderMap) -> bool {
    match headers.get("x-gw-auto-route").and_then(|v| v.to_str().ok()) {
        Some(v) => !matches!(v.trim(), "0" | "false" | "no" | "off"),
        None => true,
    }
}

/// Drop every target `expected_api` cannot reach, in place: reachable means
/// the target speaks `expected_api` itself, or `Translation::select` finds a
/// translation from `expected_api` to it. Returns how many targets were
/// dropped, so the caller can log the outcome or bail out with a 400 when the
/// count equals the resolution's original length.
///
/// A route's default and its fallbacks may each speak a different protocol
/// now (cross-protocol fallback), so this runs once per request, over the
/// whole target list, rather than the old single check against `targets[0]`.
fn filter_reachable_targets(resolution: &mut route::Resolution, expected_api: ApiKind) -> usize {
    let before = resolution.targets.len();
    resolution
        .targets
        .retain(|t| t.api == expected_api || Translation::select(expected_api, t.api).is_some());
    before - resolution.targets.len()
}

/// Proxy one request. `endpoint` must be one of the four POST paths.
pub async fn proxy(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    endpoint: &'static str,
) -> Response {
    let config = state.config.get();
    let client = client_name(&headers);
    let count_tokens = endpoint == "/v1/messages/count_tokens";
    let expected_api =
        endpoint_api(endpoint).expect("proxy() is only wired to the four POST endpoints");

    // The body must parse: `model` has to be rewritten, so an opaque forward is
    // not an option here.
    let payload: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            return error_response(
                http::StatusCode::BAD_REQUEST,
                &format!("request body is not valid JSON: {err}"),
            );
        }
    };
    let Some(requested_model) = payload
        .get("model")
        .and_then(|m| m.as_str())
        .map(String::from)
    else {
        return error_response(
            http::StatusCode::BAD_REQUEST,
            "request has no `model` field",
        );
    };

    // Every request is classified against every candidate route's
    // `description`, regardless of what model name the client sent — see
    // `classify_request`. The requested model name plays no part in
    // selection anymore; it is kept only for error messages and the trace
    // log.
    //
    // The one opt-out is `x-gw-auto-route: 0`, set by `llm-gateway launch`
    // when the session answered "no" to the auto-classify prompt — then the
    // model name the client sent is what gets resolved, unclassified.
    let semantic_attempt = if auto_route_requested(&headers) {
        classify_request(&state, &config, &payload, expected_api, &requested_model)
    } else {
        SemanticAttempt {
            resolve_as: requested_model.clone(),
            outcome: SemanticOutcome::Manual,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
        }
    };
    let resolve_target = semantic_attempt.resolve_as.as_str();

    let mut resolution = match route::resolve(&config, resolve_target) {
        Ok(r) => r,
        Err(Error::NoRoute(_)) => {
            return error_response(
                http::StatusCode::NOT_FOUND,
                &format!(
                    "no route matches model `{requested_model}`; \
                     GET /v1/models lists the available names"
                ),
            );
        }
        Err(err) => {
            return error_response(http::StatusCode::INTERNAL_SERVER_ERROR, &err.to_string());
        }
    };

    // Reachability filter. A route's default and its fallbacks may each speak
    // a different protocol now (cross-protocol fallback), so which targets
    // this request can even reach is decided per target rather than once for
    // the whole route.
    //
    // Equal protocols are the passthrough path and stay byte-for-byte. When
    // they differ, a translation may exist — that is what lets Claude Code
    // (`anthropic-messages` only) reach the many `openai-chat` providers. A
    // target neither the same protocol nor translatable to reaches is dropped
    // here, before `upstream::send_with_fallback` ever sees it, because
    // forwarding a request in the wrong protocol produces a confusing
    // upstream 400 instead of an explanation.
    let target_apis: Vec<ApiKind> = resolution.targets.iter().map(|t| t.api).collect();
    let total_targets = resolution.targets.len();
    let unreachable = filter_reachable_targets(&mut resolution, expected_api);
    if unreachable > 0 {
        tracing::debug!(
            route = %resolution.route_name,
            client_api = %expected_api,
            unreachable,
            total = total_targets,
            "route `{}`: {unreachable} of {total_targets} target(s) unreachable from {expected_api} \
             (no translation), skipped",
            resolution.route_name,
        );
    }
    if resolution.targets.is_empty() {
        let spoken: std::collections::BTreeSet<&str> =
            target_apis.iter().map(|a| a.as_str()).collect();
        return error_response(
            http::StatusCode::BAD_REQUEST,
            &format!(
                "route `{}` is backed by providers speaking {} but {endpoint} speaks \
                 {expected_api}, and this gateway cannot translate {expected_api} → any of them; \
                 use the matching endpoint or point the route (default or a fallback) at a \
                 {expected_api} provider",
                resolution.route_name,
                spoken.into_iter().collect::<Vec<_>>().join(", "),
            ),
        );
    }

    let streaming = payload
        .get("stream")
        .and_then(|s| s.as_bool())
        .unwrap_or(false);
    let debug = state.recorder.mode().debug;
    let trace_input = debug.then(|| {
        extract_input(
            expected_api,
            &payload,
            body.len(),
            streaming,
            state.recorder.mode().truncate_at(),
        )
    });

    // Token counting is a question, not a generation: the answer is
    // model-specific, so falling back to a different provider would return a
    // confidently wrong number. First (already filtered-to-reachable) target
    // only.
    if count_tokens {
        let first_target = &resolution.targets[0];
        let first_translation = Translation::select(expected_api, first_target.api);
        // Neither an untranslatable pair nor an agent CLI can answer this
        // question: `openai-chat` has no counting endpoint, and `claude -p`
        // would run a whole generation to answer it.
        let cannot_forward = first_translation.is_some_and(|t| !t.can_forward_count_tokens())
            || first_target.transport == crate::config::Transport::ClaudeCli;
        if cannot_forward {
            return count_tokens_locally(
                &state,
                &client,
                endpoint,
                &requested_model,
                trace_input,
                &resolution,
                expected_api,
                &payload,
                semantic_attempt,
            );
        }
        resolution.targets.truncate(1);
    }

    let started = Instant::now();
    let mut attempts = Vec::new();

    let build = |target: &route::Target| -> crate::error::Result<Attempt> {
        // Translation is decided per target, not once for the whole
        // resolution: a route's default and its fallbacks may speak
        // different protocols now. `resolution.targets` was already filtered
        // to reachable-only, so `None` here means "same protocol,
        // passthrough" — never "unreachable".
        //
        // A `claude-cli` fallback that stays `anthropic-messages` gets this
        // same untranslated (already Anthropic-shaped) body handed straight
        // to `crate::agent::spawn`, which expects `messages`/`system` as the
        // client sent them.
        let target_translation = Translation::select(expected_api, target.api);
        let mut request = match target_translation {
            Some(translation) => translation.request(&payload),
            None => payload.clone(),
        };
        request["model"] = serde_json::Value::String(target.model_ref.model.clone());

        // Streamed chat responses carry no usage unless asked; asking costs one
        // extra final chunk. Anthropic and Responses report usage unprompted.
        if streaming
            && target.api == ApiKind::OpenaiChat
            && target.inject_usage
            && !count_tokens
            && request.get("stream_options").is_none()
        {
            request["stream_options"] = serde_json::json!({ "include_usage": true });
        }

        Ok(Attempt {
            body: serde_json::to_vec(&request)?,
            payload: request,
            headers: passthrough::upstream_headers(&headers, &target.headers),
            count_tokens,
            streaming,
        })
    };

    let accepted =
        match upstream::send_with_fallback(&state.http, &resolution, build, &mut attempts).await {
            Ok(accepted) => accepted,
            Err(err) => {
                let dur_ms = started.elapsed().as_millis() as u64;
                tracing::warn!(
                    route = %resolution.route_name,
                    attempts = attempts.len(),
                    "all upstreams failed for route `{}`: {err}",
                    resolution.route_name,
                );
                if !count_tokens {
                    state.recorder.usage(UsageRecord {
                        ts: now_rfc3339(),
                        client: client.clone(),
                        route: resolution.route_name.clone(),
                        provider: resolution.targets[0].model_ref.provider.clone(),
                        model: resolution.targets[0].model_ref.model.clone(),
                        attempt: attempts.len().max(1) as u32,
                        in_tok: 0,
                        out_tok: 0,
                        cache_read_tok: 0,
                        cache_write_tok: 0,
                        dur_ms,
                        status: "error".to_string(),
                        stream: streaming,
                        error: Some(err.to_string()),
                    });
                }
                if let Some(input) = trace_input {
                    state.recorder.trace(trace_record(
                        &client,
                        endpoint,
                        &requested_model,
                        input,
                        &resolution,
                        intended_resolved(&resolution, expected_api),
                        attempts,
                        None,
                        semantic_attempt,
                    ));
                }
                return error_response(http::StatusCode::BAD_GATEWAY, &err.to_string());
            }
        };

    // Console visibility for "which route/provider/model actually served
    // this request, and did it take more than one attempt to get there" —
    // gated behind `logging.logging` (on by default), same as the
    // per-attempt logs in `upstream::send_with_fallback`.
    if accepted.attempt > 1 {
        tracing::info!(
            route = %resolution.route_name,
            provider = %accepted.target_provider,
            model = %accepted.target_model,
            attempt = accepted.attempt,
            "route `{}` served by {}/{} after falling back ({} attempt{})",
            resolution.route_name,
            accepted.target_provider,
            accepted.target_model,
            accepted.attempt,
            if accepted.attempt == 1 { "" } else { "s" },
        );
    } else {
        tracing::info!(
            route = %resolution.route_name,
            provider = %accepted.target_provider,
            model = %accepted.target_model,
            "route `{}` served by {}/{}",
            resolution.route_name,
            accepted.target_provider,
            accepted.target_model,
        );
    }

    let status = accepted.status;
    let upstream_headers = accepted.headers.clone();

    // Which target actually answered decides the translation, not which one
    // was first: a cross-protocol fallback may have answered instead of the
    // default, and its own protocol is what `Translation::select` needs here.
    let translation = Translation::select(expected_api, accepted.api);

    // Everything the report closure needs, captured before the stream starts.
    let recorder: Arc<Recorder> = state.recorder.clone();
    let resolved = TraceResolved {
        provider: accepted.target_provider.clone(),
        model: accepted.target_model.clone(),
        api: accepted.api.as_str().to_string(),
        translation: translation.map(|t| t.label().to_string()),
    };
    // The model to report in a translated response body, when the upstream one
    // does not name itself. Cloned before `model` below is moved into the
    // report closure.
    let response_model = accepted.target_model.clone();
    let record_usage = !count_tokens;
    let route_name = resolution.route_name.clone();
    let provider = accepted.target_provider.clone();
    let model = accepted.target_model.clone();
    let attempt_n = accepted.attempt;
    let ok_status = status.is_success();
    let requested = requested_model.clone();

    // Recording happens when the stream is dropped — the only moment that
    // exists for aborted requests too. See `usage::tee`.
    let report: tee::ReportFn = Box::new(move |usage, outcome| {
        let dur_ms = started.elapsed().as_millis() as u64;
        let status_str = match (outcome, ok_status) {
            (StreamOutcome::Aborted, _) => "aborted",
            (StreamOutcome::UpstreamError, _) => "error",
            (StreamOutcome::Complete, true) => "success",
            (StreamOutcome::Complete, false) => "error",
        };
        if record_usage {
            recorder.usage(UsageRecord {
                ts: now_rfc3339(),
                client: client.clone(),
                route: route_name.clone(),
                provider: provider.clone(),
                model: model.clone(),
                attempt: attempt_n,
                in_tok: usage.input_tokens,
                out_tok: usage.output_tokens,
                cache_read_tok: usage.cache_read_tokens,
                cache_write_tok: usage.cache_write_tokens,
                dur_ms,
                status: status_str.to_string(),
                stream: streaming,
                error: None,
            });
        }
        if let Some(input) = trace_input {
            recorder.trace(trace_record(
                &client,
                endpoint,
                &requested,
                input,
                &resolution,
                resolved,
                attempts,
                (!usage.is_empty()).then_some(TraceUsage {
                    in_tok: usage.input_tokens,
                    out_tok: usage.output_tokens,
                    cache_read_tok: usage.cache_read_tokens,
                    cache_write_tok: usage.cache_write_tokens,
                }),
                semantic_attempt,
            ));
        }
    });

    // Usage is observed on the *upstream* bytes, in the upstream's protocol,
    // before any translation — which is what keeps token accounting correct on
    // a translated route (`usage::parse` never sees a rebuilt body).
    let observed = tee::observe(accepted.body, accepted.api, streaming, report);

    match translation {
        // The passthrough path: nothing at all between the upstream stream and
        // the client's socket.
        None => passthrough::respond(status, &upstream_headers, observed),
        Some(translation) => {
            // An error body is a plain JSON object even when the request asked
            // for a stream, so the status decides the shape before `streaming`
            // does.
            let shape = if !status.is_success() {
                ResponseShape::Error {
                    status: status.as_u16(),
                }
            } else if streaming {
                ResponseShape::Sse {
                    model: response_model,
                }
            } else {
                ResponseShape::Json {
                    model: response_model,
                }
            };
            passthrough::respond(
                status,
                &upstream_headers,
                adapter::translate_body(observed, translation, shape),
            )
        }
    }
}

/// Answer `POST /v1/messages/count_tokens` locally, for a route whose provider
/// has no token-counting endpoint to forward the question to.
///
/// Returning an estimate is better than the alternatives. A `400` would leave
/// Claude Code unable to size its context window — it decides when to compact
/// from this number — so the session degrades in a way that looks like a model
/// problem rather than a missing endpoint. Forwarding to a *different*
/// (Anthropic) provider would be worse still: a token count is model-specific,
/// so the answer would be confidently wrong instead of approximately right.
///
/// The estimate is recorded in the trace log as an attempt with
/// `result: "estimated_locally"`, so a count that looks off can be traced to
/// this function rather than to the provider.
#[allow(clippy::too_many_arguments)]
fn count_tokens_locally(
    state: &AppState,
    client: &str,
    endpoint: &str,
    requested_model: &str,
    trace_input: Option<TraceInput>,
    resolution: &route::Resolution,
    expected_api: ApiKind,
    payload: &serde_json::Value,
    semantic_attempt: SemanticAttempt,
) -> Response {
    let input_tokens = crate::translate::request::estimate_input_tokens(payload);
    let target = &resolution.targets[0];

    if let Some(input) = trace_input {
        state.recorder.trace(trace_record(
            client,
            endpoint,
            requested_model,
            input,
            resolution,
            intended_resolved(resolution, expected_api),
            vec![crate::record::trace_log::TraceAttempt {
                n: 1,
                target: target.to_string(),
                result: "estimated_locally".to_string(),
                ms: 0,
            }],
            None,
            semantic_attempt,
        ));
    }

    json_response(
        http::StatusCode::OK,
        &serde_json::json!({ "input_tokens": input_tokens }),
    )
}

/// The `resolved` block for a request no upstream ever answered — the first
/// (already filtered-to-reachable) target is the one that would have served
/// it, so that is what gets recorded; its translation is derived the same
/// way every other target's is, from its own protocol and the client's.
fn intended_resolved(resolution: &route::Resolution, expected_api: ApiKind) -> TraceResolved {
    let target = &resolution.targets[0];
    let translation = Translation::select(expected_api, target.api);
    TraceResolved {
        provider: target.model_ref.provider.clone(),
        model: target.model_ref.model.clone(),
        api: target.api.as_str().to_string(),
        translation: translation.map(|t| t.label().to_string()),
    }
}

/// A JSON body the gateway produced itself, as opposed to one it forwarded.
fn json_response(status: http::StatusCode, body: &serde_json::Value) -> Response {
    let mut response = Response::new(axum::body::Body::from(body.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    response
}

#[allow(clippy::too_many_arguments)]
fn trace_record(
    client: &str,
    endpoint: &str,
    requested_model: &str,
    input: TraceInput,
    resolution: &route::Resolution,
    resolved: TraceResolved,
    attempts: Vec<crate::record::trace_log::TraceAttempt>,
    usage: Option<TraceUsage>,
    semantic_attempt: SemanticAttempt,
) -> TraceRecord {
    TraceRecord {
        ts: now_rfc3339(),
        req_id: uuid::Uuid::now_v7().to_string(),
        client: client.to_string(),
        endpoint: endpoint.to_string(),
        requested_model: requested_model.to_string(),
        input,
        routing: routing_from(resolution, semantic_attempt),
        resolved,
        attempts,
        usage,
    }
}

/// What classification did for one request, distilled into what
/// [`trace_record`] needs to fill in [`TraceRouting`].
///
/// `resolve_as` is what actually went into `route::resolve`: the winning
/// candidate's route name, the reserved `default` route
/// (`crate::config::DEFAULT_ROUTE`) when nothing matched, or the model name
/// the client sent when classification was skipped on purpose
/// ([`SemanticOutcome::Manual`]).
struct SemanticAttempt {
    resolve_as: String,
    outcome: SemanticOutcome,
    /// The scored candidate list of the embed that decided the outcome: the
    /// matching text's on a match, the newest text's on a
    /// below-threshold fallback (to show how close the closest came).
    candidates: Vec<TraceCandidate>,
    score: Option<f32>,
    /// Total embedding time across every text the history walk tried.
    embed_ms: u64,
    /// The first 200 characters of the text that decided `resolve_as` —
    /// `Some` only on [`SemanticOutcome::Matched`]. Every other outcome has
    /// no single text to blame (no match happened, or none was attempted).
    decided_by_text: Option<String>,
    /// Every text the walk tried and its top candidate score, newest text
    /// first — empty unless the walk actually ran.
    walk: Vec<TraceWalkStep>,
}

/// Why `resolve_as` ended up being what it is — one variant per
/// `routing.mode` value the trace log can report.
///
/// The `allow(dead_code)`: a `--no-default-features` build never constructs
/// the classifier-driven variants, but `routing_from` must still describe
/// them — the enum is the trace vocabulary, not just what one build emits.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SemanticOutcome {
    /// The request carried `x-gw-auto-route: 0`: classification skipped on
    /// purpose, `resolve_as` is the model name the client sent.
    Manual,
    /// No classifier was available (unloaded embedding model, or a build
    /// without the `semantic` feature): `resolve_as` is `default`
    /// unconditionally.
    NoClassifier,
    /// A classifier was available but the request carried no user text to
    /// classify anywhere in its history — e.g. every user message is a
    /// bare `tool_result`. Distinct from [`SemanticOutcome::NoClassifier`]
    /// so the trace does not blame a missing classifier for a textless
    /// request.
    NoText,
    /// A user text cleared the threshold. `texts_back` is how far the
    /// history walk went to find it: `0` is the newest user text (the
    /// everyday case, `routing.mode = "semantic"`), `n > 0` means the
    /// newest `n` texts scored below the threshold and an earlier one
    /// matched (`routing.mode = "semantic_history"`).
    Matched { texts_back: usize },
    /// User texts existed but none of the `texts_tried` newest ones cleared
    /// the threshold: fall back to `default`.
    BelowThreshold { texts_tried: usize },
    /// The newest user text begins with `<transcript>` — a client-internal
    /// utility request (e.g. Claude Code's auto-mode permission classifier
    /// asking its own gateway a yes/no question), not a real user turn.
    /// Embedding classification is skipped entirely; `resolve_as` is the
    /// model name the client sent when a route matches it
    /// (`resolved_to_requested: true`), or the reserved `default` route
    /// otherwise — so a utility request never 404s just because no route is
    /// named after the client's internal classifier model.
    UtilityBypass { resolved_to_requested: bool },
}

/// Build the `routing` block of a trace record.
fn routing_from(resolution: &route::Resolution, attempt: SemanticAttempt) -> TraceRouting {
    let (mode, reason) = match attempt.outcome {
        SemanticOutcome::Manual => (
            "manual",
            "x-gw-auto-route: 0; classification skipped, routed by the model name \
             the client sent"
                .to_string(),
        ),
        SemanticOutcome::NoClassifier => (
            "no_classifier",
            format!(
                "no classifier available; falling back to the reserved `{}` route",
                crate::config::DEFAULT_ROUTE
            ),
        ),
        SemanticOutcome::NoText => (
            "no_text",
            format!(
                "the request carries no classifiable user text; falling back to the \
                 reserved `{}` route",
                crate::config::DEFAULT_ROUTE
            ),
        ),
        SemanticOutcome::Matched { texts_back: 0 } => (
            "semantic",
            "semantic classification matched a candidate".to_string(),
        ),
        SemanticOutcome::Matched { texts_back } => (
            "semantic_history",
            format!(
                "the newest user text did not clear threshold {CLASSIFICATION_THRESHOLD:.2}; \
                 the user text {texts_back} message{} back matched",
                if texts_back == 1 { "" } else { "s" },
            ),
        ),
        SemanticOutcome::BelowThreshold { texts_tried } => (
            "semantic",
            format!(
                "semantic classification: none of the newest {texts_tried} user text{} \
                 cleared threshold {CLASSIFICATION_THRESHOLD:.2}; falling back to the \
                 reserved `{}` route",
                if texts_tried == 1 { "" } else { "s" },
                crate::config::DEFAULT_ROUTE,
            ),
        ),
        SemanticOutcome::UtilityBypass {
            resolved_to_requested: true,
        } => (
            "utility_bypass",
            "client-internal utility request (text begins with `<transcript>`); \
             classification skipped, resolved by the requested model name"
                .to_string(),
        ),
        SemanticOutcome::UtilityBypass {
            resolved_to_requested: false,
        } => (
            "utility_bypass",
            format!(
                "client-internal utility request (text begins with `<transcript>`); \
                 classification skipped, but the requested model name matches no route; \
                 falling back to the reserved `{}` route",
                crate::config::DEFAULT_ROUTE,
            ),
        ),
    };

    let ran = matches!(
        attempt.outcome,
        SemanticOutcome::Matched { .. } | SemanticOutcome::BelowThreshold { .. }
    );
    TraceRouting {
        mode: mode.to_string(),
        matched_route: resolution.route_name.clone(),
        reason,
        candidates: attempt.candidates,
        score: attempt.score,
        threshold: ran.then_some(CLASSIFICATION_THRESHOLD),
        embed_ms: ran.then_some(attempt.embed_ms),
        decided_by_text: attempt.decided_by_text,
        walk: (!attempt.walk.is_empty()).then_some(attempt.walk),
    }
}

/// Classify the request's content against every candidate route.
///
/// Always attempted, regardless of what model name the client sent — the
/// requested model name plays no part in route selection anymore.
///
/// The newest user text is tried first, so a genuine topic change always
/// wins immediately. When it scores below the threshold — or the newest
/// user message carries no text at all, the normal state of an agentic
/// turn whose last message is a `tool_result` — the walk continues to
/// earlier user texts (bounded by [`HISTORY_WALK_LIMIT`]) and takes the
/// first that clears the bar. The conversation history that arrives with
/// every request is the only state this needs: the same request always
/// classifies the same way, no matter which gateway process sees it or
/// when.
#[cfg(feature = "semantic")]
fn classify_request(
    state: &AppState,
    config: &Config,
    payload: &serde_json::Value,
    expected_api: ApiKind,
    requested_model: &str,
) -> SemanticAttempt {
    let fallback = |outcome: SemanticOutcome| SemanticAttempt {
        resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
        outcome,
        candidates: Vec::new(),
        score: None,
        embed_ms: 0,
        decided_by_text: None,
        walk: Vec::new(),
    };

    // Texts before classifier: a textless request falls back no matter
    // what, and "no user text" is the more precise reason to record for it.
    let texts = classification_texts(expected_api, payload);

    // Claude Code's auto-mode permission classifier calls back through this
    // same gateway with its own internal yes/no prompt, wrapped in a
    // `<transcript>` block — not a real user turn. Semantic classification
    // would route it by whichever role description it happens to cosine-match
    // closest, which is wrong: resolve it by the model name the client asked
    // for instead, same as `x-gw-auto-route: 0` does, and skip embedding
    // entirely.
    if texts.first().is_some_and(|t| t.starts_with("<transcript>")) {
        let resolved_to_requested = route::resolve(config, requested_model).is_ok();
        let resolve_as = if resolved_to_requested {
            requested_model.to_string()
        } else {
            crate::config::DEFAULT_ROUTE.to_string()
        };
        tracing::info!(
            resolve_as = %resolve_as,
            "utility request bypassed classification, resolved as `{resolve_as}`"
        );
        return SemanticAttempt {
            resolve_as,
            outcome: SemanticOutcome::UtilityBypass {
                resolved_to_requested,
            },
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
        };
    }

    if texts.is_empty() {
        return fallback(SemanticOutcome::NoText);
    }
    let Some(classifier) = state.classifier.as_ref() else {
        return fallback(SemanticOutcome::NoClassifier);
    };

    let as_trace = |candidates: &[(String, f32)]| -> Vec<TraceCandidate> {
        candidates
            .iter()
            .map(|(route, score)| TraceCandidate {
                route: route.clone(),
                score: *score,
            })
            .collect()
    };

    let mut embed_ms_total = 0;
    // The newest text's verdict, kept for the fallback trace record: on a
    // below-threshold fallback the newest text's candidate list is the one
    // that shows how close the closest candidate came.
    let mut newest_verdict = None;
    let texts_tried = texts.len().min(HISTORY_WALK_LIMIT);
    // Every text the walk tries, with its top score — trace-log diagnostics
    // for "which text decided this, and how close did the ones before it
    // come," independent of whether the walk ends in a match.
    let mut walk: Vec<TraceWalkStep> = Vec::new();

    for (texts_back, text) in texts.iter().take(HISTORY_WALK_LIMIT).enumerate() {
        let verdict = classifier.classify(text, expected_api);
        embed_ms_total += verdict.embed_ms;
        walk.push(TraceWalkStep {
            texts_back,
            score: verdict.candidates.first().map(|(_, score)| *score),
        });

        if let Some((route, matched_score)) = verdict.matched.clone() {
            // Console visibility for "did the embedding classifier actually
            // run, and what did it pick" — gated behind `logging.logging`
            // (on by default) via the console filter in `server::init_tracing`.
            if texts_back == 0 {
                tracing::info!(
                    route = %route,
                    score = matched_score,
                    embed_ms = embed_ms_total,
                    "classified request to route `{route}` (score {matched_score:.3}, embed {}ms)",
                    embed_ms_total,
                );
            } else {
                tracing::info!(
                    route = %route,
                    score = matched_score,
                    texts_back = texts_back,
                    embed_ms = embed_ms_total,
                    "classified request to route `{route}` from the user text {texts_back} \
                     message{} back (score {matched_score:.3}, embed {}ms)",
                    if texts_back == 1 { "" } else { "s" },
                    embed_ms_total,
                );
            }
            let score = verdict.candidates.first().map(|(_, score)| *score);
            return SemanticAttempt {
                resolve_as: route,
                outcome: SemanticOutcome::Matched { texts_back },
                candidates: as_trace(&verdict.candidates),
                score,
                embed_ms: embed_ms_total,
                decided_by_text: Some(crate::record::truncate(text, Some(200))),
                walk,
            };
        }
        if texts_back == 0 {
            newest_verdict = Some(verdict);
        }
    }

    let newest = newest_verdict.expect("texts is non-empty, so the newest text was classified");
    // The newest text's top score regardless of whether it cleared the
    // threshold — useful in the trace log even on a fallback, to show how
    // close the closest candidate came.
    let score = newest.candidates.first().map(|(_, score)| *score);
    tracing::info!(
        route = %crate::config::DEFAULT_ROUTE,
        score = score,
        texts_tried = texts_tried,
        embed_ms = embed_ms_total,
        "none of the newest {texts_tried} user text{} cleared the classification threshold \
         (closest score {} on the newest), falling back to `{}` (embed {}ms)",
        if texts_tried == 1 { "" } else { "s" },
        score.map(|s| format!("{s:.3}")).unwrap_or_else(|| "n/a".to_string()),
        crate::config::DEFAULT_ROUTE,
        embed_ms_total,
    );
    SemanticAttempt {
        resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
        outcome: SemanticOutcome::BelowThreshold { texts_tried },
        candidates: as_trace(&newest.candidates),
        score,
        embed_ms: embed_ms_total,
        decided_by_text: None,
        walk,
    }
}

#[cfg(not(feature = "semantic"))]
fn classify_request(
    _state: &AppState,
    _config: &Config,
    _payload: &serde_json::Value,
    _expected_api: ApiKind,
    _requested_model: &str,
) -> SemanticAttempt {
    SemanticAttempt {
        resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
        outcome: SemanticOutcome::NoClassifier,
        candidates: Vec::new(),
        score: None,
        embed_ms: 0,
        decided_by_text: None,
        walk: Vec::new(),
    }
}

/// Every user message's text, newest first, untruncated — the candidate
/// texts for semantic classification.
///
/// A user message with no text — an Anthropic `tool_result` turn, an
/// image-only message — contributes nothing rather than an empty entry, so
/// the history walk only ever spends its bounded attempts on text that can
/// actually score. `Embedder::embed` does its own bounding (800 chars / 64
/// tokens), so the 200-character truncation `extract_input` applies for the
/// trace log must not leak into what gets classified.
///
/// Every `<system-reminder>` block is stripped out of each text before it is
/// considered (see [`strip_system_reminders`]): Claude Code's harness injects
/// one of these into the *first* user message of every session — CLAUDE.md
/// contents, tool-list boilerplate, and similar text that is identical across
/// sessions and has nothing to do with what the user actually asked for. That
/// boilerplate was observed to cosine-match the `role-explorer` route's
/// description at 0.519 — comfortably over the classification threshold — so
/// once the history walk reached a session's first message, it classified the
/// injected reminder instead of the real instruction it wrapped. A message
/// that is nothing *but* reminder blocks strips down to blank and is skipped
/// exactly like a message with no text at all, so the walk keeps going to an
/// older, genuine instruction instead of routing on boilerplate.
///
/// This only changes what gets embedded for routing — the request forwarded
/// to the provider is untouched; nothing here reaches the proxied payload.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
fn classification_texts(api: ApiKind, payload: &serde_json::Value) -> Vec<String> {
    let messages = match api {
        ApiKind::OpenaiResponses => payload.get("input"),
        _ => payload.get("messages"),
    };
    match messages {
        Some(serde_json::Value::String(s)) => {
            let stripped = strip_system_reminders(s);
            if stripped.trim().is_empty() {
                Vec::new()
            } else {
                vec![stripped]
            }
        }
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .rev()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .filter_map(|m| m.get("content"))
            .filter_map(|content| content_text(api, content))
            .map(|text| strip_system_reminders(&text))
            .filter(|text| !text.trim().is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Remove every `<system-reminder>...</system-reminder>` block from `text`,
/// for [`classification_texts`] — see that function's doc comment for why.
///
/// A plain string scan rather than a regex: the tags are literal and never
/// nested, so `find` on each half is enough and this stays dependency-free.
/// A block missing its closing tag has everything from the opening tag to
/// the end of the text dropped, on the theory that a stray reminder leaking
/// into classification is worse than losing whatever real text might follow
/// it — the harness always closes its own blocks, so this only fires on
/// malformed or adversarial input.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + OPEN.len()..];
        match after_open.find(CLOSE) {
            Some(end) => rest = &after_open[end + CLOSE.len()..],
            // No closing tag: everything from here to the end is dropped.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}

/// Best-effort summary of the request for the trace log.
///
/// Every accessor tolerates absence: a malformed-but-parseable body should
/// produce a thin record, never a panic — the observer must not be the thing
/// that breaks a request.
fn extract_input(
    api: ApiKind,
    payload: &serde_json::Value,
    body_len: usize,
    stream: bool,
    truncate_at: Option<usize>,
) -> TraceInput {
    let messages = match api {
        ApiKind::OpenaiResponses => payload.get("input"),
        _ => payload.get("messages"),
    };

    let messages_n = match messages {
        Some(serde_json::Value::Array(a)) => a.len(),
        Some(serde_json::Value::String(_)) => 1,
        _ => 0,
    };

    let last_user_text =
        last_user_text(api, messages).map(|t| crate::record::truncate(&t, truncate_at));

    TraceInput {
        messages_n,
        last_user_text,
        // Bytes-per-token is model-dependent; /4 is close enough to spot a
        // long-context request, which is all this is for.
        tokens_est: (body_len / 4) as u64,
        tools: tool_names(api, payload),
        has_image: has_image(api, messages),
        stream,
    }
}

fn last_user_text(api: ApiKind, messages: Option<&serde_json::Value>) -> Option<String> {
    match messages {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .and_then(|m| m.get("content"))
            .and_then(|content| content_text(api, content)),
        _ => None,
    }
}

fn content_text(api: ApiKind, content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let text_key_type = match api {
                ApiKind::OpenaiResponses => "input_text",
                _ => "text",
            };
            let text: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some(text_key_type))
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect();
            (!text.is_empty()).then(|| text.join("\n"))
        }
        _ => None,
    }
}

fn tool_names(api: ApiKind, payload: &serde_json::Value) -> Vec<String> {
    let Some(tools) = payload.get("tools").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    tools
        .iter()
        .filter_map(|tool| match api {
            // Chat nests the name under `function`; the other two are flat.
            ApiKind::OpenaiChat => tool.get("function").and_then(|f| f.get("name")),
            _ => tool.get("name"),
        })
        .filter_map(|n| n.as_str())
        .map(String::from)
        .collect()
}

fn has_image(api: ApiKind, messages: Option<&serde_json::Value>) -> bool {
    let image_type = match api {
        ApiKind::AnthropicMessages => "image",
        ApiKind::OpenaiChat => "image_url",
        ApiKind::OpenaiResponses => "input_image",
    };
    let Some(serde_json::Value::Array(items)) = messages else {
        return false;
    };
    items.iter().any(|m| {
        m.get("content")
            .and_then(|c| c.as_array())
            .is_some_and(|blocks| {
                blocks
                    .iter()
                    .any(|b| b.get("type").and_then(|t| t.as_str()) == Some(image_type))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_target(api: ApiKind) -> route::Target {
        route::Target {
            model_ref: crate::config::ModelRef {
                provider: "p".to_string(),
                model: "m".to_string(),
            },
            api,
            transport: Default::default(),
            agent_args: Vec::new(),
            base_url: "https://example.test".to_string(),
            api_key: None,
            headers: Vec::new(),
            inject_usage: true,
        }
    }

    fn test_resolution(targets: Vec<route::Target>) -> route::Resolution {
        route::Resolution {
            route_name: "r".to_string(),
            kind: route::MatchKind::Exact,
            targets,
        }
    }

    /// The Claude Code shape from the config that motivated this change: an
    /// `anthropic-messages` client can reach both an `openai-chat` default
    /// (through translation) and an `anthropic-messages` fallback (directly),
    /// so neither is dropped.
    #[test]
    fn filter_reachable_targets_keeps_both_when_each_is_reachable_a_different_way() {
        let mut res = test_resolution(vec![
            test_target(ApiKind::OpenaiChat),
            test_target(ApiKind::AnthropicMessages),
        ]);

        let dropped = filter_reachable_targets(&mut res, ApiKind::AnthropicMessages);

        assert_eq!(dropped, 0);
        assert_eq!(res.targets.len(), 2);
    }

    /// The reverse client: `openai-chat` has no translation back to
    /// `anthropic-messages`, so the fallback is unreachable and dropped while
    /// the same-protocol default survives.
    #[test]
    fn filter_reachable_targets_drops_the_untranslatable_fallback() {
        let mut res = test_resolution(vec![
            test_target(ApiKind::OpenaiChat),
            test_target(ApiKind::AnthropicMessages),
        ]);

        let dropped = filter_reachable_targets(&mut res, ApiKind::OpenaiChat);

        assert_eq!(dropped, 1);
        assert_eq!(res.targets.len(), 1);
        assert_eq!(res.targets[0].api, ApiKind::OpenaiChat);
    }

    /// The point of `Translation::ResponsesToChat`: an `openai-responses`
    /// client (Codex CLI) can now reach an `openai-chat` target, though not
    /// an `anthropic-messages` one (there is no translation for that pair).
    #[test]
    fn filter_reachable_targets_lets_a_responses_client_reach_a_chat_target() {
        let mut res = test_resolution(vec![
            test_target(ApiKind::OpenaiChat),
            test_target(ApiKind::AnthropicMessages),
        ]);

        let dropped = filter_reachable_targets(&mut res, ApiKind::OpenaiResponses);

        assert_eq!(dropped, 1);
        assert_eq!(res.targets.len(), 1);
        assert_eq!(res.targets[0].api, ApiKind::OpenaiChat);
    }

    /// Every target unreachable: the caller is expected to treat `dropped`
    /// equal to the original length as "refuse the request", not to forward
    /// to nothing. `openai-responses` has no translation to
    /// `anthropic-messages` (only to `openai-chat`, via the case above), so a
    /// route backed solely by an `anthropic-messages` provider is entirely
    /// unreachable from it.
    #[test]
    fn filter_reachable_targets_can_drop_every_target() {
        let mut res = test_resolution(vec![test_target(ApiKind::AnthropicMessages)]);

        let dropped = filter_reachable_targets(&mut res, ApiKind::OpenaiResponses);

        assert_eq!(dropped, 1);
        assert!(res.targets.is_empty());
    }

    #[test]
    fn anthropic_input_summary() {
        let payload = json!({
            "model": "claude-x",
            "stream": true,
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "reply"},
                {"role": "user", "content": [
                    {"type": "text", "text": "この関数のテストを書いて"},
                    {"type": "image", "source": {}},
                ]},
            ],
            "tools": [{"name": "bash"}, {"name": "read"}],
        });
        let input = extract_input(ApiKind::AnthropicMessages, &payload, 400, true, Some(200));
        assert_eq!(input.messages_n, 3);
        assert_eq!(
            input.last_user_text.as_deref(),
            Some("この関数のテストを書いて")
        );
        assert_eq!(input.tools, vec!["bash", "read"]);
        assert!(input.has_image);
        assert_eq!(input.tokens_est, 100);
    }

    #[test]
    fn chat_tool_names_are_nested_under_function() {
        let payload = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "function": {"name": "write"}}],
        });
        let input = extract_input(ApiKind::OpenaiChat, &payload, 40, false, None);
        assert_eq!(input.tools, vec!["write"]);
        assert!(!input.has_image);
    }

    #[test]
    fn responses_string_input_counts_as_one_message() {
        let payload = json!({ "input": "ping" });
        let input = extract_input(ApiKind::OpenaiResponses, &payload, 20, false, Some(200));
        assert_eq!(input.messages_n, 1);
        assert_eq!(input.last_user_text.as_deref(), Some("ping"));
    }

    #[test]
    fn long_user_text_is_truncated_with_a_marker() {
        let long = "a".repeat(300);
        let payload = json!({ "messages": [{"role": "user", "content": long}] });
        let input = extract_input(ApiKind::AnthropicMessages, &payload, 400, false, Some(200));
        let text = input.last_user_text.unwrap();
        assert_eq!(text.chars().count(), 201); // 200 + ellipsis
        assert!(text.ends_with('…'));
    }

    #[test]
    fn absent_fields_produce_a_thin_record_not_a_panic() {
        let payload = json!({});
        let input = extract_input(ApiKind::OpenaiChat, &payload, 2, false, None);
        assert_eq!(input.messages_n, 0);
        assert!(input.last_user_text.is_none());
        assert!(input.tools.is_empty());
    }

    #[test]
    fn classification_texts_are_not_truncated_unlike_the_trace_log_version() {
        // `Embedder::embed` does its own bounding (800 chars); the 200-char
        // truncation `extract_input` applies for the trace log must not
        // leak into what actually gets classified.
        let long = "a".repeat(300);
        let payload = json!({ "messages": [{"role": "user", "content": long.clone()}] });
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec![long]);
    }

    #[test]
    fn classification_texts_are_empty_without_a_user_message() {
        let payload = json!({});
        assert!(classification_texts(ApiKind::OpenaiChat, &payload).is_empty());
    }

    #[test]
    fn classification_texts_are_newest_first() {
        let payload = json!({ "messages": [
            {"role": "user", "content": "write the parser"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "now test it"},
        ]});
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["now test it", "write the parser"]);
    }

    #[test]
    fn classification_texts_skip_textless_user_messages() {
        // The everyday agentic turn: the newest user message is a bare
        // `tool_result` with no text block at all. It must contribute
        // nothing, leaving the last real instruction as the newest text.
        let payload = json!({ "messages": [
            {"role": "user", "content": "この関数のテストを書いて"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
        ]});
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["この関数のテストを書いて"]);
    }

    #[test]
    fn classification_texts_skip_blank_texts() {
        let payload = json!({ "messages": [
            {"role": "user", "content": "real instruction"},
            {"role": "user", "content": "   "},
        ]});
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["real instruction"]);
    }

    #[test]
    fn classification_texts_strip_a_system_reminder_around_the_real_instruction() {
        // The harness-injected block wraps real text; only the real
        // instruction should remain a candidate for classification.
        let content = "<system-reminder>CLAUDE.md contents here</system-reminder>\
                       please refactor the parser";
        let payload = json!({ "messages": [{"role": "user", "content": content}] });
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["please refactor the parser"]);
    }

    #[test]
    fn classification_texts_skip_a_message_that_is_only_a_system_reminder() {
        // A message that strips down to nothing must be treated exactly like
        // a textless message: skipped, so the walk reaches the older, real
        // instruction instead of classifying boilerplate.
        let payload = json!({ "messages": [
            {"role": "user", "content": "write the release notes"},
            {"role": "user", "content": "<system-reminder>CLAUDE.md contents here</system-reminder>"},
        ]});
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["write the release notes"]);
    }

    #[test]
    fn classification_texts_drop_everything_after_an_unclosed_system_reminder() {
        // A missing closing tag is treated as "the rest is boilerplate too" —
        // safer to drop trailing text than let a truncated reminder leak into
        // classification.
        let content = "real instruction<system-reminder>never closed";
        let payload = json!({ "messages": [{"role": "user", "content": content}] });
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["real instruction"]);
    }

    #[test]
    fn classification_texts_strip_multiple_system_reminder_blocks() {
        let content = "<system-reminder>first</system-reminder>\
                       the real ask\
                       <system-reminder>second</system-reminder>";
        let payload = json!({ "messages": [{"role": "user", "content": content}] });
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["the real ask"]);
    }

    fn resolution(route_name: &str, kind: route::MatchKind) -> route::Resolution {
        route::Resolution {
            route_name: route_name.to_string(),
            kind,
            targets: Vec::new(),
        }
    }

    #[test]
    fn routing_from_reports_no_classifier() {
        let res = resolution(crate::config::DEFAULT_ROUTE, route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
            outcome: SemanticOutcome::NoClassifier,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
        };
        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "no_classifier");
        assert_eq!(routing.matched_route, crate::config::DEFAULT_ROUTE);
        assert!(routing.candidates.is_empty());
        assert!(routing.score.is_none());
        assert!(routing.threshold.is_none());
        assert!(routing.embed_ms.is_none());
        assert!(routing.decided_by_text.is_none());
        assert!(routing.walk.is_none());
    }

    #[test]
    fn routing_from_reports_no_text_distinct_from_no_classifier() {
        let res = resolution(crate::config::DEFAULT_ROUTE, route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
            outcome: SemanticOutcome::NoText,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
        };
        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "no_text");
        assert_eq!(routing.matched_route, crate::config::DEFAULT_ROUTE);
        assert!(
            !routing.reason.contains("no classifier"),
            "a textless request must not be blamed on a missing classifier: {}",
            routing.reason
        );
        assert!(routing.threshold.is_none());
        assert!(routing.embed_ms.is_none());
    }

    #[test]
    fn routing_from_reports_a_semantic_match() {
        let res = resolution("role-writer", route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: "role-writer".to_string(),
            outcome: SemanticOutcome::Matched { texts_back: 0 },
            candidates: vec![
                TraceCandidate {
                    route: "role-writer".to_string(),
                    score: 0.8,
                },
                TraceCandidate {
                    route: "role-reader".to_string(),
                    score: 0.3,
                },
            ],
            score: Some(0.8),
            embed_ms: 2,
            decided_by_text: Some("write me a poem".to_string()),
            walk: vec![TraceWalkStep {
                texts_back: 0,
                score: Some(0.8),
            }],
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "semantic");
        assert_eq!(routing.matched_route, "role-writer");
        assert_eq!(routing.score, Some(0.8));
        assert_eq!(routing.threshold, Some(CLASSIFICATION_THRESHOLD));
        assert_eq!(routing.embed_ms, Some(2));
        assert_eq!(routing.candidates.len(), 2);
        assert!(routing.reason.contains("matched"), "{}", routing.reason);
        assert_eq!(routing.decided_by_text.as_deref(), Some("write me a poem"));
        assert_eq!(routing.walk.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn routing_from_reports_a_history_match_with_its_distance() {
        let res = resolution("role-tester", route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: "role-tester".to_string(),
            outcome: SemanticOutcome::Matched { texts_back: 2 },
            candidates: vec![TraceCandidate {
                route: "role-tester".to_string(),
                score: 0.7,
            }],
            score: Some(0.7),
            embed_ms: 3,
            decided_by_text: Some("now write the tests".to_string()),
            walk: vec![
                TraceWalkStep {
                    texts_back: 0,
                    score: Some(0.2),
                },
                TraceWalkStep {
                    texts_back: 1,
                    score: Some(0.3),
                },
                TraceWalkStep {
                    texts_back: 2,
                    score: Some(0.7),
                },
            ],
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "semantic_history");
        assert_eq!(routing.matched_route, "role-tester");
        assert_eq!(routing.threshold, Some(CLASSIFICATION_THRESHOLD));
        assert_eq!(routing.embed_ms, Some(3));
        assert!(
            routing.reason.contains("2 messages back"),
            "reason should say how far the walk went: {}",
            routing.reason
        );
        assert_eq!(
            routing.decided_by_text.as_deref(),
            Some("now write the tests")
        );
        assert_eq!(routing.walk.as_ref().map(Vec::len), Some(3));
    }

    #[test]
    fn routing_from_explains_a_fallback_below_threshold() {
        let res = resolution(crate::config::DEFAULT_ROUTE, route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
            outcome: SemanticOutcome::BelowThreshold { texts_tried: 3 },
            candidates: vec![TraceCandidate {
                route: "role-writer".to_string(),
                score: 0.2,
            }],
            score: Some(0.2),
            embed_ms: 1,
            decided_by_text: None,
            walk: vec![
                TraceWalkStep {
                    texts_back: 0,
                    score: Some(0.2),
                },
                TraceWalkStep {
                    texts_back: 1,
                    score: Some(0.15),
                },
                TraceWalkStep {
                    texts_back: 2,
                    score: Some(0.1),
                },
            ],
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "semantic");
        assert_eq!(routing.matched_route, crate::config::DEFAULT_ROUTE);
        assert_eq!(routing.score, Some(0.2));
        assert_eq!(routing.threshold, Some(CLASSIFICATION_THRESHOLD));
        assert_eq!(routing.candidates.len(), 1);
        assert!(
            routing.reason.contains("0.45"),
            "reason should mention the threshold: {}",
            routing.reason
        );
        assert!(
            routing.reason.contains("3 user texts"),
            "reason should say how many texts were tried: {}",
            routing.reason
        );
        // A fallback has no single text that decided the outcome, but the
        // walk that tried and rejected each candidate is still there to
        // diagnose from.
        assert!(routing.decided_by_text.is_none());
        let walk = routing.walk.expect("a below-threshold walk records scores");
        assert_eq!(walk.len(), 3);
        assert_eq!(walk[0].score, Some(0.2));
        assert_eq!(walk[2].score, Some(0.1));
    }

    #[test]
    fn routing_from_reports_manual_mode_with_the_sent_model_name() {
        let res = resolution("gpt-5-codex", route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: "gpt-5-codex".to_string(),
            outcome: SemanticOutcome::Manual,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "manual");
        assert_eq!(routing.matched_route, "gpt-5-codex");
        assert!(routing.candidates.is_empty());
        assert!(routing.score.is_none());
        assert!(routing.embed_ms.is_none());
        assert!(routing.reason.contains("x-gw-auto-route"));
        assert!(routing.decided_by_text.is_none());
        assert!(routing.walk.is_none());
    }

    #[test]
    fn routing_from_reports_utility_bypass_resolved_to_the_requested_model() {
        let res = resolution("role-writer", route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: "role-writer".to_string(),
            outcome: SemanticOutcome::UtilityBypass {
                resolved_to_requested: true,
            },
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "utility_bypass");
        assert_eq!(routing.matched_route, "role-writer");
        assert!(routing.candidates.is_empty());
        assert!(routing.score.is_none());
        assert!(routing.threshold.is_none());
        assert!(routing.embed_ms.is_none());
        assert!(routing.decided_by_text.is_none());
        assert!(routing.walk.is_none());
        assert!(
            routing.reason.contains("<transcript>"),
            "{}",
            routing.reason
        );
    }

    #[test]
    fn routing_from_reports_utility_bypass_falling_back_to_default() {
        let res = resolution(crate::config::DEFAULT_ROUTE, route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
            outcome: SemanticOutcome::UtilityBypass {
                resolved_to_requested: false,
            },
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "utility_bypass");
        assert_eq!(routing.matched_route, crate::config::DEFAULT_ROUTE);
        assert!(
            routing.reason.contains(crate::config::DEFAULT_ROUTE),
            "reason should mention the fallback route: {}",
            routing.reason
        );
        assert!(
            routing.reason.contains("matches no route"),
            "reason should explain why it fell back: {}",
            routing.reason
        );
    }

    #[test]
    fn auto_route_requested_defaults_to_true_without_the_header() {
        assert!(auto_route_requested(&HeaderMap::new()));
    }

    #[test]
    fn auto_route_requested_is_false_for_disabling_values() {
        for value in ["0", "false", "no", "off"] {
            let mut headers = HeaderMap::new();
            headers.insert("x-gw-auto-route", value.parse().unwrap());
            assert!(
                !auto_route_requested(&headers),
                "{value} should disable auto-route"
            );
        }
    }

    #[test]
    fn auto_route_requested_is_true_for_1() {
        let mut headers = HeaderMap::new();
        headers.insert("x-gw-auto-route", "1".parse().unwrap());
        assert!(auto_route_requested(&headers));
    }

    /// Builds just enough `AppState` to exercise `classify_request` without a
    /// loaded embedding model: a `Recorder` over a scratch directory and a
    /// config, nothing more. `classifier` stays `None` — see
    /// `crate::server::AppState::classifier`'s doc comment for why that is a
    /// legitimate (if unusual outside tests) state.
    fn test_state(config: crate::config::Config) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(
            dir.path().to_path_buf(),
            crate::record::RecordMode {
                usage: false,
                debug: false,
                debug_full: false,
            },
        )
        .unwrap();
        let state = AppState {
            config: crate::config::watch::SharedConfig::from_config(
                config,
                dir.path().join("config.json"),
            ),
            http: reqwest::Client::new(),
            recorder,
            inbound_key: None,
            #[cfg(feature = "semantic")]
            classifier: None,
        };
        (dir, state)
    }

    fn classifiable_config() -> crate::config::Config {
        use crate::config::{ModelConfig, ProviderConfig, RouteConfig, SecretRef};

        let mut config = crate::config::Config::default();
        config.providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                base_url: "https://example.test".to_string(),
                api: ApiKind::AnthropicMessages,
                api_key: Some(SecretRef::new("k")),
                headers: Default::default(),
                inject_usage: true,
                transport: Default::default(),
                agent_args: Vec::new(),
            },
        );
        config.routes.insert(
            "role-writer".to_string(),
            RouteConfig {
                description: Some(crate::config::Description(vec!["writes prose".to_string()])),
                model: ModelConfig {
                    default: "anthropic/opus-pinned".to_string(),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
        config.routes.insert(
            crate::config::DEFAULT_ROUTE.to_string(),
            RouteConfig {
                description: Some(crate::config::Description(vec!["catch-all".to_string()])),
                model: ModelConfig {
                    default: "anthropic/opus-pinned".to_string(),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
        config
    }

    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_falls_back_to_default_without_a_loaded_classifier() {
        // `classifier` is `None` in this test state — same situation a
        // never-started classifier would leave a request in: the caller
        // must fall back to `default`, not panic.
        let (_dir, state) = test_state(classifiable_config());
        let config = state.config.get();
        let payload = json!({ "messages": [{"role": "user", "content": "hello"}] });

        let attempt = classify_request(
            &state,
            &config,
            &payload,
            ApiKind::AnthropicMessages,
            "opus",
        );
        assert_eq!(attempt.outcome, SemanticOutcome::NoClassifier);
        assert_eq!(attempt.resolve_as, crate::config::DEFAULT_ROUTE);
    }

    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_reports_no_text_for_a_textless_request() {
        // The classifier being absent and the request being textless are
        // different fallbacks; a textless request short-circuits before the
        // classifier is even consulted, so this is observable without a
        // loaded model.
        let (_dir, state) = test_state(classifiable_config());
        let config = state.config.get();
        let payload = json!({ "messages": [
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]},
        ]});

        let attempt = classify_request(
            &state,
            &config,
            &payload,
            ApiKind::AnthropicMessages,
            "opus",
        );
        assert_eq!(attempt.outcome, SemanticOutcome::NoText);
        assert_eq!(attempt.resolve_as, crate::config::DEFAULT_ROUTE);
    }

    /// The bug this feature fixes: Claude Code's auto-mode permission
    /// classifier sends its internal yes/no prompt (a `<transcript>...`
    /// body) through the same gateway endpoint a real turn would use. It
    /// must never be classified by the embedding router — it must resolve
    /// straight to the model name the client asked for.
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_bypasses_classification_for_a_transcript_prefixed_request() {
        let (_dir, state) = test_state(classifiable_config());
        let config = state.config.get();
        let payload = json!({ "messages": [
            {"role": "user", "content": "<transcript>\nsome tool call history\n</transcript>\nis this safe?"},
        ]});

        let attempt = classify_request(
            &state,
            &config,
            &payload,
            ApiKind::AnthropicMessages,
            "role-writer",
        );

        assert_eq!(
            attempt.outcome,
            SemanticOutcome::UtilityBypass {
                resolved_to_requested: true
            }
        );
        assert_eq!(attempt.resolve_as, "role-writer");
        assert!(attempt.candidates.is_empty());
        assert!(attempt.score.is_none());
        assert_eq!(attempt.embed_ms, 0);
        assert!(attempt.decided_by_text.is_none());
        assert!(attempt.walk.is_empty());
    }

    /// When the requested model name matches no configured route,
    /// bypassing classification must not turn into a 404 — it falls back to
    /// the reserved `default` route instead, same escape hatch `Manual`
    /// mode does not have but this one needs (the client-internal request
    /// carries whatever model name Claude Code itself is configured with,
    /// which this gateway's routes have no reason to know about).
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_falls_back_to_default_when_the_requested_model_has_no_route() {
        let (_dir, state) = test_state(classifiable_config());
        let config = state.config.get();
        let payload = json!({ "messages": [
            {"role": "user", "content": "<transcript>...</transcript>\nis this safe?"},
        ]});

        let attempt = classify_request(
            &state,
            &config,
            &payload,
            ApiKind::AnthropicMessages,
            "claude-opus-4-not-a-configured-route",
        );

        assert_eq!(
            attempt.outcome,
            SemanticOutcome::UtilityBypass {
                resolved_to_requested: false
            }
        );
        assert_eq!(attempt.resolve_as, crate::config::DEFAULT_ROUTE);
    }

    #[cfg(not(feature = "semantic"))]
    #[tokio::test]
    async fn classify_request_is_always_a_no_op_without_the_semantic_feature() {
        let (_dir, state) = test_state(classifiable_config());
        let config = state.config.get();
        let payload = json!({ "messages": [{"role": "user", "content": "hello"}] });

        let attempt = classify_request(
            &state,
            &config,
            &payload,
            ApiKind::AnthropicMessages,
            "opus",
        );
        assert_eq!(attempt.outcome, SemanticOutcome::NoClassifier);
        assert_eq!(attempt.resolve_as, crate::config::DEFAULT_ROUTE);
    }
}
