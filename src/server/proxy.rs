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

/// The system-prompt classification threshold, mirrored here the same way
/// `CLASSIFICATION_THRESHOLD` is above, for the same reason — available (for
/// trace-log text and tests) even in a `--no-default-features` build that
/// never compiles `crate::semantic`. Must stay equal to
/// `crate::semantic::index::SYSTEM_CLASSIFICATION_THRESHOLD` when that module
/// is compiled in.
#[cfg(feature = "semantic")]
use crate::semantic::index::SYSTEM_CLASSIFICATION_THRESHOLD;
#[cfg(not(feature = "semantic"))]
const SYSTEM_CLASSIFICATION_THRESHOLD: f32 = 0.50;

/// How many of the request's classifiable texts (newest first) classification
/// tries before falling back to the reserved `default` route.
///
/// Agentic clients routinely send turns whose newest message is a short
/// reply ("continue", "yes") that scores below the threshold on its own, or
/// carries no text at all. The conversation history that arrives with every
/// request already holds richer signal such a turn is continuing — the
/// user's own earlier text, or (see [`classification_texts`]'s doc comment)
/// a `tool_result`/`role: "tool"`/`function_call_output` an agent's tool
/// call already produced — so classification walks back to the most recent
/// candidate that clears the threshold instead of keeping per-conversation
/// state in the gateway. The bound exists only to cap work on pathological
/// inputs; each embed is sub-millisecond, so eight is generous, not tight.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
const HISTORY_WALK_LIMIT: usize = 8;

/// Display-only `resolve_as` / trace-log `matched_route` label for a
/// `<transcript>`-prefixed utility request resolved via `Config::auto_mode`.
/// Never looked up as a route name — `Config::auto_mode` is resolved
/// directly, via `route::resolve_model`, bypassing route-name lookup
/// entirely — so nothing checks whether this string collides with an actual
/// route name.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
const AUTO_MODE_LABEL: &str = "<auto-mode>";

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

/// Mark every target in `resolution` as a `<transcript>` utility-bypass
/// target when `outcome` says this request is one — all three
/// `UtilityBypassResolution` shapes count, whether or not `resolved_targets`
/// was set (see `classify_request`'s `<transcript>` branch). One central
/// point rather than three, so it does not need repeating at every
/// `classify_request` return site. A no-op for every other outcome, and for
/// an empty target list.
///
/// `crate::agent::claude_cli` reads `Target::is_utility_bypass` to trim its
/// own overhead for a call that expects a fast, short verdict rather than a
/// full agent turn — see the 2026-08-03 entry in `docs/decisions.md`.
fn mark_utility_bypass_targets(resolution: &mut route::Resolution, outcome: &SemanticOutcome) {
    if matches!(outcome, SemanticOutcome::UtilityBypass(_)) {
        for target in &mut resolution.targets {
            target.is_utility_bypass = true;
        }
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

/// Whether an attempt built for `target` can be expected to come back with a
/// `usage` object at all, given the request as it will actually be sent.
///
/// `count_tokens` responses never carry `usage` — that part was already
/// true before #22. The addition is the `openai-chat` streaming case:
/// upstreams in that shape only report usage when
/// `stream_options.include_usage` is on the wire, which the request only
/// gets when `target.inject_usage` is set (see the injection just above
/// wherever `request` was built). A provider deliberately configured with
/// `injectUsage: false`, given a client that never asked for
/// `stream_options` itself, is never going to see a `usage` object in its
/// response — that is the normal, expected shape of that configuration, not
/// a failed extraction, so `expect_usage` must say `false` for it. Both
/// `usage::tee`'s "usage could not be extracted" warning and
/// `UsageRecord::usage_missing` key off this same flag, so a provider set up
/// this way stops looking like every request silently fails (#22).
fn expect_usage_for(
    target: &route::Target,
    streaming: bool,
    count_tokens: bool,
    request: &serde_json::Value,
) -> bool {
    if count_tokens {
        return false;
    }
    let known_to_have_no_stream_options = streaming
        && target.api == ApiKind::OpenaiChat
        && !target.inject_usage
        && request.get("stream_options").is_none();
    !known_to_have_no_stream_options
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
    let mut semantic_attempt = if auto_route_requested(&headers) {
        classify_request(&state, &config, &payload, expected_api, &requested_model)
    } else {
        SemanticAttempt {
            resolve_as: requested_model.clone(),
            outcome: SemanticOutcome::Manual,
            resolved_targets: None,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
            system_score: None,
        }
    };

    // `Config::auto_mode` resolves straight to targets, with no route name
    // lookup at all — see `classify_request`'s `<transcript>` bypass. Every
    // other outcome (including the other two `UtilityBypass` shapes) still
    // goes through `route::resolve` by name, same as before this field
    // existed.
    let mut resolution = if let Some(targets) = semantic_attempt.resolved_targets.take() {
        route::Resolution {
            route_name: semantic_attempt.resolve_as.clone(),
            targets,
        }
    } else {
        match route::resolve(&config, semantic_attempt.resolve_as.as_str()) {
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
        }
    };

    mark_utility_bypass_targets(&mut resolution, &semantic_attempt.outcome);

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
    // `serve --ui`'s live feed wants the same routing/prompt data `--debug`
    // records to disk, but it is a different, lower-stakes decision
    // (ephemeral, in-memory, gone the moment nothing is subscribed) — so it
    // gets its own gate rather than being tied to `debug`. `trace_input` is
    // computed whenever either wants it; which of the two (or both) actually
    // happens with the built record is decided per call site below.
    let want_live = state.live.is_some();
    let trace_input =
        (debug || want_live).then(|| extract_input(expected_api, &payload, body.len(), streaming));

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

    // `build` runs once per target tried, in order, so recording an
    // `expect_usage` alongside each `Attempt` (rather than recomputing it
    // once from `accepted` after the fact) lets whichever attempt is finally
    // accepted be looked up by its 1-based position below — the same
    // position `Accepted::attempt` already uses. A `Mutex` rather than a
    // `RefCell`: `build` is held across `.await` points inside
    // `send_with_fallback`, and this whole future must stay `Send` for axum
    // to accept it as a handler — `&RefCell<_>` is not `Send`, `&Mutex<_>` is.
    let expect_usage_per_attempt = std::sync::Mutex::new(Vec::new());

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

        expect_usage_per_attempt
            .lock()
            .unwrap()
            .push(expect_usage_for(target, streaming, count_tokens, &request));

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
                        usage_missing: false,
                        dur_ms,
                        status: "error".to_string(),
                        stream: streaming,
                        error: Some(err.to_string()),
                    });
                }
                if let Some(input) = trace_input {
                    let record = trace_record(
                        &client,
                        endpoint,
                        &requested_model,
                        input,
                        &resolution,
                        intended_resolved(&resolution, expected_api),
                        attempts,
                        None,
                        semantic_attempt,
                    );
                    if debug {
                        state.recorder.trace(record.clone());
                    }
                    if let Some(live) = &state.live {
                        let point = live_point(&state, &record);
                        live.publish(live_event_from(
                            &record,
                            record.attempts.len().max(1) as u32,
                            "error",
                            dur_ms,
                            Some(err.to_string()),
                            point,
                        ));
                    }
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
    // Whole-`AppState` clone rather than just the `live` field: `live_point`
    // takes `&AppState` uniformly across both the `semantic`-feature-on and
    // -off builds (see its doc comment), and every field here is already
    // `Arc`/cheap-`Clone`.
    let state_for_report = state.clone();
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
    // `accepted.attempt` is 1-based and `expect_usage_per_attempt` gained one
    // entry per attempt `build` was called for, in the same order — so the
    // accepted attempt's entry sits at `attempt - 1`. Defaults to the
    // conservative `!count_tokens` if that invariant is ever violated, which
    // only means a real extraction failure could log a warning it otherwise
    // wouldn't — never the other way around.
    let expect_usage = expect_usage_per_attempt
        .lock()
        .unwrap()
        .get(accepted.attempt as usize - 1)
        .copied()
        .unwrap_or(!count_tokens);

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
                // The persisted record's fields stay plain `u64` — an
                // unobserved field is recorded the same as an observed zero,
                // which keeps `usage-*.jsonl` byte-compatible with records
                // written before `Usage` gained `Option` fields (#23). The
                // `usage_missing` flag below is what actually distinguishes
                // "nothing observed" from a genuine zero-token response.
                in_tok: usage.input_tokens.unwrap_or(0),
                out_tok: usage.output_tokens.unwrap_or(0),
                cache_read_tok: usage.cache_read_tokens.unwrap_or(0),
                cache_write_tok: usage.cache_write_tokens.unwrap_or(0),
                // A `success` response with no usage at all means extraction
                // failed, not that it genuinely cost zero tokens — see
                // `tee::ObserveStream`'s warning for the same signal at the
                // point it's first observed. Gated on `expect_usage` the same
                // way that warning is (#22): a provider configured with
                // `injectUsage: false` and no client-supplied
                // `stream_options` never gets a `usage` object back, and
                // that is the expected shape of a normal response, not a
                // failed extraction.
                usage_missing: status_str == "success" && expect_usage && usage.is_empty(),
                dur_ms,
                status: status_str.to_string(),
                stream: streaming,
                error: None,
            });
        }
        if let Some(input) = trace_input {
            let record = trace_record(
                &client,
                endpoint,
                &requested,
                input,
                &resolution,
                resolved,
                attempts,
                (!usage.is_empty()).then_some(TraceUsage {
                    in_tok: usage.input_tokens.unwrap_or(0),
                    out_tok: usage.output_tokens.unwrap_or(0),
                    cache_read_tok: usage.cache_read_tokens.unwrap_or(0),
                    cache_write_tok: usage.cache_write_tokens.unwrap_or(0),
                }),
                semantic_attempt,
            );
            if debug {
                recorder.trace(record.clone());
            }
            if let Some(live) = &state_for_report.live {
                let point = live_point(&state_for_report, &record);
                live.publish(live_event_from(
                    &record, attempt_n, status_str, dur_ms, None, point,
                ));
            }
        }
    });

    // Usage is observed on the *upstream* bytes, in the upstream's protocol,
    // before any translation — which is what keeps token accounting correct on
    // a translated route (`usage::parse` never sees a rebuilt body).
    //
    // `expect_usage` is the same flag computed above for `usage_missing`
    // (see `expect_usage_for`): `count_tokens` responses never carry a
    // `usage` object at all, and neither does an `openai-chat` streaming
    // response from a provider with `injectUsage: false` when the client
    // didn't ask for `stream_options` itself (#22) — both are expected
    // shapes, not extraction failures, so both must suppress the same
    // warning here that `usage_missing` suppresses in the trace/usage log.
    // Routing count_tokens through `observe` at all (rather than bypassing
    // it) keeps this the one place that understands both the streaming and
    // buffered body shapes, and keeps every endpoint observed on the same
    // upstream-bytes-below-translation path.
    let observed = tee::observe(accepted.body, accepted.api, streaming, expect_usage, report);

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
        let record = trace_record(
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
                detail: None,
                dropped: None,
            }],
            None,
            semantic_attempt,
        );
        // `trace_input` is `Some` whenever `--debug` *or* `serve --ui`'s live
        // feed wants it (see `proxy`) — disk persistence must stay gated on
        // `--debug` alone, so it needs its own check here rather than
        // reusing `trace_input.is_some()` the way this used to.
        if state.recorder.mode().debug {
            state.recorder.trace(record.clone());
        }
        if let Some(live) = &state.live {
            let point = live_point(state, &record);
            live.publish(live_event_from(&record, 1, "success", 0, None, point));
        }
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

/// Turn a just-built [`TraceRecord`] into a [`crate::server::live::LiveEvent`]
/// for `serve --ui`'s live feed.
///
/// Reuses the record rather than rebuilding the same data from scratch: both
/// describe "what came in, which route/model it triggered, how it turned
/// out," and a `TraceRecord` is already exactly that shape (see
/// `crate::record::trace_log`) — this only adds what a `TraceRecord` does
/// not carry (`status`/`dur_ms`/`attempt` are derived elsewhere in this file
/// rather than stored on it) and the vector-map point.
fn live_event_from(
    record: &TraceRecord,
    attempt: u32,
    status: &str,
    dur_ms: u64,
    error: Option<String>,
    point: Option<[f32; 2]>,
) -> crate::server::live::LiveEvent {
    let usage = record.usage.unwrap_or_default();
    // The record holds the prompt in full (see `extract_input`); the row gets
    // a clip, the expanded row gets everything. `prompt_full` is left `None`
    // when the clip already *is* the whole text, so the common short-prompt
    // case does not send the same string twice.
    let prompt_preview = record
        .input
        .last_user_text
        .as_deref()
        .map(|text| crate::record::truncate(text, Some(crate::record::TRUNCATE_CHARS)));
    let prompt_full = record
        .input
        .last_user_text
        .clone()
        .filter(|text| Some(text) != prompt_preview.as_ref());
    crate::server::live::LiveEvent {
        ts: record.ts.clone(),
        req_id: record.req_id.clone(),
        client: record.client.clone(),
        endpoint: record.endpoint.clone(),
        requested_model: record.requested_model.clone(),
        prompt_preview,
        prompt_full,
        system_prompt: record.input.system_text.clone(),
        routing_mode: record.routing.mode.clone(),
        reason: record.routing.reason.clone(),
        matched_route: record.routing.matched_route.clone(),
        candidates: record
            .routing
            .candidates
            .iter()
            .map(|c| crate::server::live::LiveCandidate {
                route: c.route.clone(),
                score: c.score,
            })
            .collect(),
        score: record.routing.score,
        provider: record.resolved.provider.clone(),
        model: record.resolved.model.clone(),
        api: record.resolved.api.clone(),
        translation: record.resolved.translation.clone(),
        attempt,
        streaming: record.input.stream,
        status: status.to_string(),
        dur_ms,
        in_tok: usage.in_tok,
        out_tok: usage.out_tok,
        cache_read_tok: usage.cache_read_tok,
        cache_write_tok: usage.cache_write_tok,
        error,
        point,
    }
}

/// Where the text that decided routing (`record.routing.decided_by_text`)
/// lands on the same 2-D map `GET /api/routes/vectors` draws — `None` when
/// nothing decided routing by text, no classifier is loaded to re-embed it
/// with, or nobody is subscribed to see the result.
///
/// Only called when `state.live` is already known to be `Some` (see the call
/// sites below) — but `Some` only means `serve --ui` is on for this run, not
/// that any tab is actually open. `LiveFeed::publish` is already a no-op
/// with zero subscribers, but that is too late: by then this function's
/// work (re-embedding `text`, then fitting or reusing a `Basis`) has already
/// run, once per request, on the tokio worker handling it (#27). Checking
/// `has_subscribers` first, before any of that, is what actually makes an
/// unwatched dashboard free.
#[cfg(feature = "semantic")]
fn live_point(state: &AppState, record: &TraceRecord) -> Option<[f32; 2]> {
    if !state
        .live
        .as_ref()
        .is_some_and(|live| live.has_subscribers())
    {
        return None;
    }
    let classifier = state.classifier.as_ref()?;
    let text = record.routing.decided_by_text.as_deref()?;
    crate::server::ui::project_point(state, classifier, text)
}

#[cfg(not(feature = "semantic"))]
fn live_point(_state: &AppState, _record: &TraceRecord) -> Option<[f32; 2]> {
    None
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
    /// Pre-resolved targets that bypass the route-name lookup entirely — set
    /// only when [`SemanticOutcome::UtilityBypass`] resolved via
    /// `Config::auto_mode` directly. `resolve_as` is still populated in that
    /// case, but only as a display-only label (`"<auto-mode>"`) for the trace
    /// log's `matched_route` — it is never looked up as a route name.
    resolved_targets: Option<Vec<route::Target>>,
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
    /// The system prompt's top candidate score, recorded whenever
    /// system-prompt classification was *attempted* — even when it missed
    /// [`SYSTEM_CLASSIFICATION_THRESHOLD`] and the outcome fell through to
    /// the ordinary user-text walk below. `None` when the request carried no
    /// system prompt (`system_prompt_text` returned `None`), or no
    /// classifier was loaded to score it. Mirrors `TraceRouting`'s
    /// `system_score` field, which is why this is worth keeping even on a
    /// miss — it doubles as tuning data for the threshold.
    system_score: Option<f32>,
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
    /// The request's *system prompt* — an agent definition, e.g. a Claude
    /// Code subagent's `.claude/agents/*.md` prompt — cleared
    /// [`SYSTEM_CLASSIFICATION_THRESHOLD`] on its own; `routing.mode =
    /// "semantic_system"`. User texts were never consulted: a system prompt
    /// is the strongest available signal for "what role is this agent
    /// playing," and this is tried before the user-text walk below (see the
    /// 2026-08-01 entry in `docs/decisions.md` for the misroute this fixes —
    /// a subagent's own investigation prompt getting pulled toward whatever
    /// object the user's instruction happened to mention).
    MatchedSystem,
    /// User texts existed but none of the `texts_tried` newest ones cleared
    /// the threshold: fall back to `default`.
    BelowThreshold { texts_tried: usize },
    /// The newest user text begins with `<transcript>` — a client-internal
    /// utility request (e.g. Claude Code's auto-mode permission classifier
    /// asking its own gateway a yes/no question), not a real user turn.
    /// Embedding classification is skipped entirely; see
    /// [`UtilityBypassResolution`] for how `resolve_as` (or
    /// `resolved_targets`) ended up decided — so a utility request never
    /// 404s just because no route is named after the client's internal
    /// classifier model, and, when `Config::auto_mode` is configured, never
    /// gets routed through the (possibly slow) `default` route at all.
    UtilityBypass(UtilityBypassResolution),
}

/// How a `<transcript>`-prefixed utility request's bypass was resolved —
/// three distinct ways `SemanticOutcome::UtilityBypass` can end up deciding
/// `resolve_as` (or `resolved_targets`), each needing its own trace-log
/// wording.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UtilityBypassResolution {
    /// `Config::auto_mode` was configured: resolved straight to its
    /// pre-resolved targets, bypassing route-name lookup (and the `default`
    /// route) entirely — see `route::resolve_model`.
    AutoModeConfig,
    /// `Config::auto_mode` was unset (or, defensively, failed to resolve
    /// even though `validate` should have already caught that): the
    /// requested model name matched a route by exact name.
    RequestedModel,
    /// Neither `auto_mode` nor the requested model name resolved to
    /// anything: fell back to the reserved `default` route.
    DefaultFallback,
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
        SemanticOutcome::MatchedSystem => (
            "semantic_system",
            format!(
                "the request's system prompt (agent definition) cleared the system-prompt \
                 threshold {SYSTEM_CLASSIFICATION_THRESHOLD:.2}; user texts were not consulted"
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
        SemanticOutcome::UtilityBypass(UtilityBypassResolution::AutoModeConfig) => (
            "utility_bypass",
            "client-internal utility request (text begins with `<transcript>`); \
             classification skipped, resolved to the configured `autoMode` target"
                .to_string(),
        ),
        SemanticOutcome::UtilityBypass(UtilityBypassResolution::RequestedModel) => (
            "utility_bypass",
            "client-internal utility request (text begins with `<transcript>`); \
             classification skipped, resolved by the requested model name"
                .to_string(),
        ),
        SemanticOutcome::UtilityBypass(UtilityBypassResolution::DefaultFallback) => (
            "utility_bypass",
            format!(
                "client-internal utility request (text begins with `<transcript>`); \
                 classification skipped, but the requested model name matches no route; \
                 falling back to the reserved `{}` route",
                crate::config::DEFAULT_ROUTE,
            ),
        ),
    };

    // The threshold to report differs by *which* classification step ran:
    // the ordinary user-text walk always applies `CLASSIFICATION_THRESHOLD`,
    // `MatchedSystem` applies the stricter `SYSTEM_CLASSIFICATION_THRESHOLD`
    // instead — reporting the wrong one would make a system-prompt match
    // look like it barely cleared the bar (or didn't) when it actually
    // cleared a higher one.
    let threshold = match attempt.outcome {
        SemanticOutcome::Matched { .. } | SemanticOutcome::BelowThreshold { .. } => {
            Some(CLASSIFICATION_THRESHOLD)
        }
        SemanticOutcome::MatchedSystem => Some(SYSTEM_CLASSIFICATION_THRESHOLD),
        _ => None,
    };
    let embed_ms = threshold.is_some().then_some(attempt.embed_ms);
    TraceRouting {
        mode: mode.to_string(),
        matched_route: resolution.route_name.clone(),
        reason,
        candidates: attempt.candidates,
        score: attempt.score,
        threshold,
        embed_ms,
        decided_by_text: attempt.decided_by_text,
        walk: (!attempt.walk.is_empty()).then_some(attempt.walk),
        system_score: attempt.system_score,
    }
}

/// Classify the request against every candidate route.
///
/// Always attempted, regardless of what model name the client sent — the
/// requested model name plays no part in route selection anymore.
///
/// Three steps, in order, the first to decide wins:
///
/// 1. The `<transcript>` bypass (unchanged by this doc's other two steps —
///    see the comment at its call site for why it must stay first).
/// 2. The request's **system prompt**, at the stricter
///    [`SYSTEM_CLASSIFICATION_THRESHOLD`] — see [`system_prompt_text`]. An
///    agent definition (a Claude Code subagent's own system prompt, say) is
///    the strongest available signal for "what role is this agent playing,"
///    stronger than anything the user's own text says; a request whose
///    system prompt clears the bar never even looks at user text.
/// 3. The ordinary **user-text history walk**: the newest user text is tried
///    first, so a genuine topic change always wins immediately. When it
///    scores below [`CLASSIFICATION_THRESHOLD`] — or the newest user message
///    carries no text at all, the normal state of an agentic turn whose last
///    message is a `tool_result` — the walk continues to earlier user texts
///    (bounded by [`HISTORY_WALK_LIMIT`]) and takes the first that clears the
///    bar.
///
/// The conversation history that arrives with every request is the only
/// state any of this needs: the same request always classifies the same
/// way, no matter which gateway process sees it or when.
#[cfg(feature = "semantic")]
fn classify_request(
    state: &AppState,
    config: &Config,
    payload: &serde_json::Value,
    expected_api: ApiKind,
    requested_model: &str,
) -> SemanticAttempt {
    let fallback = |outcome: SemanticOutcome, system_score: Option<f32>| SemanticAttempt {
        resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
        outcome,
        resolved_targets: None,
        candidates: Vec::new(),
        score: None,
        embed_ms: 0,
        decided_by_text: None,
        walk: Vec::new(),
        system_score,
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

    // Texts before classifier: a textless request falls back no matter
    // what, and "no user text" is the more precise reason to record for it.
    let texts = classification_texts(expected_api, payload);

    // Claude Code's auto-mode permission classifier calls back through this
    // same gateway with its own internal yes/no prompt, wrapped in a
    // `<transcript>` block — not a real user turn. Semantic classification
    // would route it by whichever role description it happens to cosine-match
    // closest, which is wrong. Worse, the fallback the pre-`auto_mode` bypass
    // used — the requested model name, or the shared `default` route — was
    // observed to point `default` at a slow, multi-second subprocess target
    // in a real environment, which starved this fast yes/no judgment into a
    // rejection (see the 2026-08-01 entry in `docs/decisions.md`). When
    // `Config::auto_mode` is set, it is resolved straight to targets here —
    // via `route::resolve_model`, never a route-name lookup — so the operator
    // can pin a fast target regardless of what model name the client's
    // internal classifier happens to ask for; the gateway never fabricates
    // the judgment itself, it only picks which real LLM answers it. Unset
    // falls back to the pre-existing behaviour: resolve by the model name the
    // client asked for, same as `x-gw-auto-route: 0` does, or the reserved
    // `default` route if that name matches no route.
    if texts.first().is_some_and(|t| t.starts_with("<transcript>")) {
        if let Some(auto_mode) = &config.auto_mode {
            match route::resolve_model(config, "the `autoMode` config", auto_mode) {
                Ok(targets) => {
                    tracing::info!(
                        "utility request bypassed classification, resolved to the configured \
                         `autoMode` target"
                    );
                    return SemanticAttempt {
                        resolve_as: AUTO_MODE_LABEL.to_string(),
                        outcome: SemanticOutcome::UtilityBypass(
                            UtilityBypassResolution::AutoModeConfig,
                        ),
                        resolved_targets: Some(targets),
                        candidates: Vec::new(),
                        score: None,
                        embed_ms: 0,
                        decided_by_text: None,
                        walk: Vec::new(),
                        system_score: None,
                    };
                }
                Err(err) => {
                    // Defensive only — `validate` already checks `auto_mode`
                    // at config-load time, so this should not happen in
                    // practice. Fall through to the requested-model/`default`
                    // resolution below rather than failing the request
                    // outright.
                    tracing::warn!(
                        "configured `autoMode` failed to resolve ({err}); falling back to the \
                         requested model name"
                    );
                }
            }
        }

        let resolved_to_requested = route::resolve(config, requested_model).is_ok();
        let (resolve_as, bypass_resolution) = if resolved_to_requested {
            (
                requested_model.to_string(),
                UtilityBypassResolution::RequestedModel,
            )
        } else {
            (
                crate::config::DEFAULT_ROUTE.to_string(),
                UtilityBypassResolution::DefaultFallback,
            )
        };
        tracing::info!(
            resolve_as = %resolve_as,
            "utility request bypassed classification, resolved as `{resolve_as}`"
        );
        return SemanticAttempt {
            resolve_as,
            outcome: SemanticOutcome::UtilityBypass(bypass_resolution),
            resolved_targets: None,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
            system_score: None,
        };
    }

    // System-prompt classification: tried before the user-text walk below,
    // at the stricter `SYSTEM_CLASSIFICATION_THRESHOLD` — see this
    // function's doc comment for why a system prompt takes priority over
    // user text when it clears that bar. Requires a loaded classifier, same
    // as the walk does; when one is not available this simply contributes
    // nothing (`system_score` stays `None`) and the `NoText`/`NoClassifier`
    // fallbacks below behave exactly as they did before this step existed.
    let mut system_score = None;
    if let Some(classifier) = state.classifier.as_ref() {
        if let Some(system_text) = system_prompt_text(expected_api, payload) {
            let verdict = classifier.classify_with_threshold(
                &system_text,
                expected_api,
                SYSTEM_CLASSIFICATION_THRESHOLD,
            );
            system_score = verdict.candidates.first().map(|(_, score)| *score);

            if let Some((route, matched_score)) = verdict.matched.clone() {
                tracing::info!(
                    route = %route,
                    score = matched_score,
                    embed_ms = verdict.embed_ms,
                    "classified request to route `{route}` from its system prompt \
                     (score {matched_score:.3}, embed {}ms); user texts were not consulted",
                    verdict.embed_ms,
                );
                return SemanticAttempt {
                    resolve_as: route,
                    outcome: SemanticOutcome::MatchedSystem,
                    resolved_targets: None,
                    candidates: as_trace(&verdict.candidates),
                    score: system_score,
                    embed_ms: verdict.embed_ms,
                    decided_by_text: Some(crate::record::truncate(&system_text, Some(200))),
                    walk: Vec::new(),
                    system_score,
                };
            }

            tracing::info!(
                score = system_score,
                embed_ms = verdict.embed_ms,
                "system prompt scored below the system-prompt threshold \
                 {SYSTEM_CLASSIFICATION_THRESHOLD:.2} (closest {}), falling through to user texts",
                system_score
                    .map(|s| format!("{s:.3}"))
                    .unwrap_or_else(|| "n/a".to_string()),
            );
        }
    }

    if texts.is_empty() {
        return fallback(SemanticOutcome::NoText, system_score);
    }
    let Some(classifier) = state.classifier.as_ref() else {
        return fallback(SemanticOutcome::NoClassifier, system_score);
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
                resolved_targets: None,
                candidates: as_trace(&verdict.candidates),
                score,
                embed_ms: embed_ms_total,
                decided_by_text: Some(crate::record::truncate(text, Some(200))),
                walk,
                system_score,
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
        resolved_targets: None,
        candidates: as_trace(&newest.candidates),
        score,
        embed_ms: embed_ms_total,
        decided_by_text: None,
        walk,
        system_score,
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
        resolved_targets: None,
        candidates: Vec::new(),
        score: None,
        embed_ms: 0,
        decided_by_text: None,
        walk: Vec::new(),
        system_score: None,
    }
}

/// Every message's classifiable text, newest first, untruncated — the
/// candidate texts for semantic classification.
///
/// This is more than plain user text: a `tool_result` block (Anthropic), a
/// `role: "tool"` message (`openai-chat`), or a `function_call_output` item
/// (`openai-responses`) all count too, via [`classification_content_text`].
/// Only a message with *no* text anywhere — a bare image, a `tool_result`
/// with no `content` at all — contributes nothing, so the history walk only
/// ever spends its bounded attempts on text that can actually score.
/// `Embedder::embed` does its own bounding (800 chars / 64 tokens), so the
/// 200-character truncation `extract_input` applies for the trace log must
/// not leak into what gets classified.
///
/// Tool-result content used to be excluded outright — see the 2026-07-31
/// entry in `docs/decisions.md` for `HISTORY_WALK_LIMIT`'s original
/// reasoning, "walk past a turn with no signal to find the instruction it's
/// continuing." That assumed a `tool_result` never *is* the instruction,
/// only ever masks one. It sometimes is: a bare URL classifies as a trivial
/// chore, but the content an agent fetches from it afterward can reveal the
/// task is not trivial at all — and that content, once it lands in history,
/// deserves the same shot at deciding routing that ordinary user text gets.
/// Being newest-first is what makes this safe rather than a free-for-all:
/// fetched content only wins if it is more recent than whatever user text
/// would otherwise have decided the route, exactly the same recency rule
/// that already governs plain conversation. See the entry in
/// `docs/decisions.md` this change adds for the last-resort-tier design that
/// was considered and rejected instead (it cannot fix the bug it was meant
/// to fix — the walk finds the stale original text long before reaching a
/// tier tried only after it exhausts).
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
            .filter(|item| is_classification_source(item))
            .filter_map(|item| classification_item_text(api, item))
            .map(|text| strip_system_reminders(&text))
            .filter(|text| !text.trim().is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether `item` — one entry of `messages` (Anthropic/`openai-chat`) or
/// `input` (`openai-responses`) — can carry classifiable text at all, for
/// [`classification_texts`]'s filter.
///
/// `role: "user"` covers ordinary text and, for Anthropic, `tool_result`
/// blocks nested in its `content` (they share the same envelope — see
/// `docs/`'s Anthropic tool-result shape). `role: "tool"` is `openai-chat`'s
/// standalone tool-result message. `openai-responses`' `function_call_output`
/// items carry no `role` field at all, so they need their own check.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
fn is_classification_source(item: &serde_json::Value) -> bool {
    matches!(
        item.get("role").and_then(|r| r.as_str()),
        Some("user") | Some("tool")
    ) || item.get("type").and_then(|t| t.as_str()) == Some("function_call_output")
}

/// Pulls the classifiable text out of one `messages`/`input` item — the
/// `openai-responses` `function_call_output` shape (`output` is a sibling of
/// `type`, not nested under `content`) needs its own branch; everything else
/// reads its `content` field via [`classification_content_text`].
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
fn classification_item_text(api: ApiKind, item: &serde_json::Value) -> Option<String> {
    if api == ApiKind::OpenaiResponses
        && item.get("type").and_then(|t| t.as_str()) == Some("function_call_output")
    {
        return match item.get("output") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(other) => Some(other.to_string()),
            None => None,
        };
    }
    classification_content_text(api, item.get("content")?)
}

/// Like [`content_text`], but for classification only: also reads a
/// `tool_result` block's own `content` (Anthropic) and a `role: "tool"`
/// message's plain-string `content` (`openai-chat`, already handled by the
/// `Value::String` arm shared with ordinary text).
///
/// A deliberate fork rather than a change to `content_text` itself:
/// `content_text` is also used by [`system_prompt_text`] (a system prompt
/// never carries a `tool_result` block, so this would be a no-op there
/// anyway) and by `last_user_text` (`extract_input`'s trace-log summary of
/// "what did the user last say") — changing that field's meaning to include
/// tool-result content is a separate decision this change does not make.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
fn classification_content_text(api: ApiKind, content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(blocks) => {
            let text_key_type = match api {
                ApiKind::OpenaiResponses => "input_text",
                _ => "text",
            };
            // `text`-type blocks are the message's own authored words; a
            // `tool_result` block is quoted output the message merely
            // carries alongside them. Joining in array order let a
            // `tool_result` that happens to sit before the text block push
            // that text out of first position — which broke the
            // `<transcript>` utility-bypass check in `classify_request`
            // (`texts.first().starts_with("<transcript>")`) for exactly the
            // shape a permission-classifier call needing to show the tool
            // result it is judging would use. Collecting `text` blocks first
            // means the message's own words always lead the joined string,
            // no matter where amid quoted tool output they were placed.
            let mut text_parts = Vec::new();
            let mut tool_result_parts = Vec::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some(t) if t == text_key_type => {
                        if let Some(text) = b.get("text").and_then(|t| t.as_str()) {
                            text_parts.push(text.to_string());
                        }
                    }
                    Some("tool_result") => {
                        let text = crate::translate::request::tool_result_text(b.get("content"));
                        if !text.is_empty() {
                            tool_result_parts.push(text);
                        }
                    }
                    _ => {}
                }
            }
            text_parts.extend(tool_result_parts);
            (!text_parts.is_empty()).then(|| text_parts.join("\n"))
        }
        _ => None,
    }
}

/// Remove every `<system-reminder>...</system-reminder>` block from `text`,
/// for [`classification_texts`] and [`system_prompt_text`] — see their doc
/// comments for why.
///
/// A plain string scan rather than a regex: the tags are literal and never
/// nested, so `find` on each half is enough and this stays dependency-free.
/// A block missing its closing tag has everything from the opening tag to
/// the end of the text dropped, on the theory that a stray reminder leaking
/// into classification is worse than losing whatever real text might follow
/// it — the harness always closes its own blocks, so this only fires on
/// malformed or adversarial input.
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

/// The request's system prompt — an agent's own definition of its role,
/// independent of anything the user's own text says. Where it lives differs
/// by protocol: Anthropic Messages has a dedicated `system` field, OpenAI
/// Chat carries it as the first `system`/`developer` message, and OpenAI
/// Responses has a dedicated `instructions` field, falling back to the first
/// `system`/`developer` item in `input` when `instructions` is empty or
/// absent (some clients — opencode, notably — send it that way instead).
///
/// `<system-reminder>...</system-reminder>` blocks are stripped the same way
/// [`classification_texts`] strips them from user turns (see
/// [`strip_system_reminders`]) before the result is checked for blankness,
/// so a system prompt that is nothing but harness boilerplate counts as
/// absent rather than as a classification input.
///
/// Only the *beginning* of whatever this returns ever reaches the
/// classifier: `Embedder::embed` truncates its input to 800 characters / 64
/// tokens (see that function's doc comment). This is exactly the shape a
/// subagent definition has — Claude Code's `.claude/agents/*.md` prompts
/// (and the equivalent in other harnesses) put the role description first,
/// so it survives the truncation — while a harness's own generic preamble
/// ("You are Claude Code, an interactive CLI tool...") does not reliably
/// distinguish one role from another even within its first 800 characters.
/// That is part of why system-prompt classification uses a stricter
/// threshold than user-text classification does — see
/// `SYSTEM_CLASSIFICATION_THRESHOLD`'s doc comment.
///
/// `None` when there is nothing to classify: the field/message is absent,
/// present but empty, or strips down to blank.
fn system_prompt_text(api: ApiKind, payload: &serde_json::Value) -> Option<String> {
    let raw = match api {
        ApiKind::AnthropicMessages => {
            crate::translate::request::system_text(payload.get("system")?)
        }
        ApiKind::OpenaiChat => {
            let messages = payload.get("messages")?.as_array()?;
            let system = messages.iter().find(|m| {
                matches!(
                    m.get("role").and_then(|r| r.as_str()),
                    Some("system") | Some("developer")
                )
            })?;
            crate::translate::request::message_text(system)
        }
        ApiKind::OpenaiResponses => {
            let instructions = payload
                .get("instructions")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty());
            match instructions {
                Some(instructions) => instructions.to_string(),
                None => {
                    let items = payload.get("input")?.as_array()?;
                    let system = items.iter().find(|m| {
                        matches!(
                            m.get("role").and_then(|r| r.as_str()),
                            Some("system") | Some("developer")
                        )
                    })?;
                    content_text(api, system.get("content")?)?
                }
            }
        }
    };

    let stripped = strip_system_reminders(&raw);
    (!stripped.trim().is_empty()).then_some(stripped)
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
///
/// Prompt text comes back **in full**. The 200-character clip belongs to the
/// trace file, not to the record, and is applied on the way there
/// (`crate::record::Recorder::trace`) so the live feed can hand the dashboard
/// the whole prompt to expand.
fn extract_input(
    api: ApiKind,
    payload: &serde_json::Value,
    body_len: usize,
    stream: bool,
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

    let last_user_text = last_user_text(api, messages);
    let system_text = system_prompt_text(api, payload);

    TraceInput {
        messages_n,
        last_user_text,
        system_text,
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
            timeout: crate::upstream::FIRST_BYTE_TIMEOUT,
            max_concurrent: crate::agent::DEFAULT_MAX_CONCURRENT,
            is_utility_bypass: false,
        }
    }

    /// Regression tests for #22: `expect_usage_for` is what both
    /// `usage::tee`'s "usage could not be extracted" warning and
    /// `UsageRecord::usage_missing` key off, so the false-positive case in
    /// the issue (`injectUsage: false`, streaming, no client-supplied
    /// `stream_options`) must resolve to `false`, and every other
    /// combination must keep resolving to `true` (the old, unconditional
    /// `!count_tokens` behavior).
    #[test]
    fn expect_usage_for_is_false_only_for_the_no_usage_openai_chat_streaming_case() {
        let mut target = test_target(ApiKind::OpenaiChat);
        target.inject_usage = false;
        let request = json!({ "messages": [] });
        assert!(!expect_usage_for(&target, true, false, &request));
    }

    #[test]
    fn expect_usage_for_is_true_when_inject_usage_is_set() {
        // `inject_usage: true` means `stream_options` gets added before the
        // request goes out (see the injection in `proxy`'s `build`
        // closure), so usage is expected the normal way.
        let mut target = test_target(ApiKind::OpenaiChat);
        target.inject_usage = true;
        let request = json!({ "messages": [] });
        assert!(expect_usage_for(&target, true, false, &request));
    }

    #[test]
    fn expect_usage_for_is_true_when_the_client_already_sent_stream_options() {
        // Even with `injectUsage: false`, a client that asked for
        // `stream_options` itself may still get usage back — only the
        // "neither side asked" combination is a known no-usage case.
        let mut target = test_target(ApiKind::OpenaiChat);
        target.inject_usage = false;
        let request = json!({ "messages": [], "stream_options": { "include_usage": true } });
        assert!(expect_usage_for(&target, true, false, &request));
    }

    #[test]
    fn expect_usage_for_is_true_for_non_streaming_openai_chat() {
        let mut target = test_target(ApiKind::OpenaiChat);
        target.inject_usage = false;
        let request = json!({ "messages": [] });
        assert!(expect_usage_for(&target, false, false, &request));
    }

    #[test]
    fn expect_usage_for_is_true_for_anthropic_regardless_of_inject_usage() {
        // `injectUsage`/`stream_options` are an `openai-chat` streaming
        // concept only — Anthropic reports usage unprompted.
        let mut target = test_target(ApiKind::AnthropicMessages);
        target.inject_usage = false;
        let request = json!({ "messages": [] });
        assert!(expect_usage_for(&target, true, false, &request));
    }

    #[test]
    fn expect_usage_for_is_false_for_count_tokens_regardless_of_everything_else() {
        let target = test_target(ApiKind::OpenaiChat);
        let request = json!({ "messages": [] });
        assert!(!expect_usage_for(&target, true, true, &request));
    }

    fn test_resolution(targets: Vec<route::Target>) -> route::Resolution {
        route::Resolution {
            route_name: "r".to_string(),
            targets,
        }
    }

    fn test_semantic_attempt(decided_by_text: Option<&str>) -> SemanticAttempt {
        SemanticAttempt {
            resolve_as: "role-writer".to_string(),
            outcome: SemanticOutcome::Manual,
            resolved_targets: None,
            candidates: vec![TraceCandidate {
                route: "role-writer".to_string(),
                score: 0.9,
            }],
            score: Some(0.9),
            embed_ms: 3,
            decided_by_text: decided_by_text.map(String::from),
            walk: Vec::new(),
            system_score: None,
        }
    }

    fn test_trace_record(usage: Option<TraceUsage>) -> TraceRecord {
        let resolution = route::Resolution {
            route_name: "role-writer".to_string(),
            targets: vec![test_target(ApiKind::AnthropicMessages)],
        };
        let input = TraceInput {
            messages_n: 1,
            last_user_text: Some("hello".to_string()),
            system_text: None,
            tokens_est: 10,
            tools: vec![],
            has_image: false,
            stream: false,
        };
        let resolved = TraceResolved {
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            api: "anthropic-messages".to_string(),
            translation: None,
        };
        trace_record(
            "claude-code",
            "/v1/messages",
            "claude-sonnet-4-6",
            input,
            &resolution,
            resolved,
            Vec::new(),
            usage,
            test_semantic_attempt(Some("hello")),
        )
    }

    #[test]
    fn live_event_from_maps_the_trace_record_and_the_extra_fields() {
        let record = test_trace_record(Some(TraceUsage {
            in_tok: 10,
            out_tok: 20,
            cache_read_tok: 1,
            cache_write_tok: 2,
        }));

        let event = live_event_from(&record, 1, "success", 120, None, Some([0.1, -0.2]));

        assert_eq!(event.matched_route, "role-writer");
        assert_eq!(event.provider, "anthropic");
        assert_eq!(event.model, "claude-sonnet-4-6");
        assert_eq!(event.status, "success");
        assert_eq!(event.dur_ms, 120);
        assert_eq!(event.in_tok, 10);
        assert_eq!(event.out_tok, 20);
        assert_eq!(event.cache_read_tok, 1);
        assert_eq!(event.cache_write_tok, 2);
        assert_eq!(event.prompt_preview.as_deref(), Some("hello"));
        assert_eq!(event.point, Some([0.1, -0.2]));
        assert_eq!(event.candidates.len(), 1);
        assert_eq!(event.candidates[0].route, "role-writer");
        assert!((event.candidates[0].score - 0.9).abs() < 1e-6);
        // Nothing was clipped, so there is no second copy to send.
        assert!(event.prompt_full.is_none());
    }

    /// A prompt longer than the row can show is sent twice: clipped for the
    /// collapsed row, whole for the expanded one. Without `prompt_full` the
    /// dashboard could only ever show the first 200 characters, which is what
    /// made a long prompt undiagnosable from the UI.
    #[test]
    fn live_event_from_carries_a_long_prompt_both_clipped_and_whole() {
        let long = "a".repeat(300);
        let mut record = test_trace_record(None);
        record.input.last_user_text = Some(long.clone());
        record.input.system_text = Some("You are a security monitor.".to_string());

        let event = live_event_from(&record, 1, "success", 120, None, None);

        let preview = event.prompt_preview.expect("the row still gets a preview");
        assert_eq!(preview.chars().count(), 201); // 200 + ellipsis
        assert!(preview.ends_with('…'));
        assert_eq!(event.prompt_full.as_deref(), Some(long.as_str()));
        assert_eq!(
            event.system_prompt.as_deref(),
            Some("You are a security monitor.")
        );
    }

    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn live_point_skips_projection_when_nobody_is_subscribed() {
        // #27: `live_point` must bail out before it would need a classifier
        // at all once `has_subscribers` says nobody is listening — proven
        // here by a `state.classifier` of `None` (which `test_state` always
        // leaves unset — see its doc comment) not causing a panic or an
        // early `?`-return that would also pass with the gate missing: if
        // the `has_subscribers` check were removed, this call would still
        // return `None`, just by falling through the `classifier.as_ref()?`
        // below instead — so the meaningful assertion is `has_subscribers`
        // itself (see `live.rs`'s own tests for that), and this test exists
        // to pin `live_point`'s observable contract: no subscribers, no
        // point, regardless of why.
        let (_dir, mut state) = test_state(crate::config::Config::default());
        state.live = Some(std::sync::Arc::new(crate::server::live::LiveFeed::new()));
        assert!(!state.live.as_ref().unwrap().has_subscribers());

        let record = test_trace_record(None);
        assert_eq!(live_point(&state, &record), None);
    }

    #[test]
    fn live_event_from_defaults_usage_to_zero_without_a_usage_block() {
        let record = test_trace_record(None);

        let event = live_event_from(&record, 2, "error", 50, Some("boom".to_string()), None);

        assert_eq!(event.in_tok, 0);
        assert_eq!(event.out_tok, 0);
        assert_eq!(event.attempt, 2);
        assert_eq!(event.error.as_deref(), Some("boom"));
        assert!(event.point.is_none());
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
    /// client (Codex CLI) can reach an `openai-chat` target through
    /// translation, same as an `anthropic-messages` one (see the test right
    /// after this one, for `Translation::ResponsesToAnthropic`).
    #[test]
    fn filter_reachable_targets_lets_a_responses_client_reach_a_chat_target() {
        let mut res = test_resolution(vec![
            test_target(ApiKind::OpenaiChat),
            test_target(ApiKind::AnthropicMessages),
        ]);

        let dropped = filter_reachable_targets(&mut res, ApiKind::OpenaiResponses);

        assert_eq!(dropped, 0);
        assert_eq!(res.targets.len(), 2);
    }

    /// The point of `Translation::ResponsesToAnthropic`: an
    /// `openai-responses` client (Codex CLI) can now reach an
    /// `anthropic-messages` target too (notably the `claude-cli` agent
    /// transport), not just an `openai-chat` one.
    #[test]
    fn filter_reachable_targets_lets_a_responses_client_reach_an_anthropic_target() {
        let mut res = test_resolution(vec![test_target(ApiKind::AnthropicMessages)]);

        let dropped = filter_reachable_targets(&mut res, ApiKind::OpenaiResponses);

        assert_eq!(dropped, 0);
        assert_eq!(res.targets.len(), 1);
        assert_eq!(res.targets[0].api, ApiKind::AnthropicMessages);
    }

    /// Every target unreachable: the caller is expected to treat `dropped`
    /// equal to the original length as "refuse the request", not to forward
    /// to nothing. `openai-chat` has no translation back to
    /// `anthropic-messages` (only the reverse direction exists), so a route
    /// backed solely by an `anthropic-messages` provider is entirely
    /// unreachable from an `openai-chat` client.
    #[test]
    fn filter_reachable_targets_can_drop_every_target() {
        let mut res = test_resolution(vec![test_target(ApiKind::AnthropicMessages)]);

        let dropped = filter_reachable_targets(&mut res, ApiKind::OpenaiChat);

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
        let input = extract_input(ApiKind::AnthropicMessages, &payload, 400, true);
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
        let input = extract_input(ApiKind::OpenaiChat, &payload, 40, false);
        assert_eq!(input.tools, vec!["write"]);
        assert!(!input.has_image);
    }

    #[test]
    fn responses_string_input_counts_as_one_message() {
        let payload = json!({ "input": "ping" });
        let input = extract_input(ApiKind::OpenaiResponses, &payload, 20, false);
        assert_eq!(input.messages_n, 1);
        assert_eq!(input.last_user_text.as_deref(), Some("ping"));
    }

    /// The record keeps prompt text whole — the 200-character clip is the
    /// trace *file*'s policy (`Recorder::trace`), and the live feed needs the
    /// untruncated text to expand a row.
    #[test]
    fn long_user_text_is_kept_in_full_on_the_record() {
        let long = "a".repeat(300);
        let payload = json!({ "messages": [{"role": "user", "content": long.clone()}] });
        let input = extract_input(ApiKind::AnthropicMessages, &payload, 400, false);
        assert_eq!(input.last_user_text.as_deref(), Some(long.as_str()));
    }

    #[test]
    fn absent_fields_produce_a_thin_record_not_a_panic() {
        let payload = json!({});
        let input = extract_input(ApiKind::OpenaiChat, &payload, 2, false);
        assert_eq!(input.messages_n, 0);
        assert!(input.last_user_text.is_none());
        assert!(input.tools.is_empty());
    }

    #[test]
    fn classification_texts_are_not_truncated_unlike_the_trace_log_version() {
        // `Embedder::embed` does its own bounding (800 chars); the 200-char
        // truncation the trace log applies must not leak into what actually
        // gets classified.
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
    fn classification_texts_skip_textless_tool_results() {
        // A `tool_result` with no `content` field at all is genuinely
        // blank — unlike one carrying real content (see the tests below),
        // it must still contribute nothing.
        let payload = json!({ "messages": [
            {"role": "user", "content": "この関数のテストを書いて"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "bash", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1"}]},
        ]});
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["この関数のテストを書いて"]);
    }

    #[test]
    fn classification_texts_prefer_a_newer_tool_result_over_an_older_user_message() {
        // The bug this change fixes: a bare URL scores against a trivial
        // route on its own, but the content an agent fetches from it
        // afterward — landing here as a `tool_result` — is newer and more
        // informative, so it must be tried first, not skipped in favor of
        // the stale original message.
        let payload = json!({ "messages": [
            {"role": "user", "content": "見て: https://example.com/issues/1"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "gh", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "issue body: implement a distributed consensus protocol"},
            ]},
        ]});
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(
            texts,
            vec![
                "issue body: implement a distributed consensus protocol",
                "見て: https://example.com/issues/1",
            ]
        );
    }

    #[test]
    fn classification_texts_read_array_tool_result_content() {
        // A `tool_result` block's own `content` can itself be an array of
        // text blocks rather than a plain string.
        let payload = json!({ "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [
                    {"type": "text", "text": "line one"},
                    {"type": "text", "text": "line two"},
                ]},
            ]},
        ]});
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(texts, vec!["line one\nline two"]);
    }

    #[test]
    fn classification_texts_put_a_messages_own_text_before_a_tool_result_in_the_same_content_array()
    {
        // A single message can carry both a `tool_result` block and a
        // `text` block together — e.g. Claude Code's internal auto-mode
        // permission classifier attaching the tool result it is judging
        // alongside its own `<transcript>`-wrapped yes/no question. Joining
        // in raw array order let the `tool_result` (placed first here) push
        // the message's own text out of first position, which broke the
        // `<transcript>` bypass's `starts_with` check in `classify_request`.
        // The message's own text must always lead, regardless of where the
        // blocks fall in the array.
        let payload = json!({ "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "some tool call history"},
                {"type": "text", "text": "<transcript>...</transcript>\nis this safe?"},
            ]},
        ]});
        let texts = classification_texts(ApiKind::AnthropicMessages, &payload);
        assert_eq!(
            texts,
            vec!["<transcript>...</transcript>\nis this safe?\nsome tool call history"]
        );
    }

    #[test]
    fn classification_texts_read_openai_chat_tool_messages() {
        let payload = json!({ "messages": [
            {"role": "user", "content": "見て: https://example.com/issues/1"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1"}]},
            {"role": "tool", "tool_call_id": "call_1", "content": "issue body: complex spec"},
        ]});
        let texts = classification_texts(ApiKind::OpenaiChat, &payload);
        assert_eq!(
            texts,
            vec![
                "issue body: complex spec",
                "見て: https://example.com/issues/1"
            ]
        );
    }

    #[test]
    fn classification_texts_read_openai_responses_function_call_output() {
        let payload = json!({ "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "見て: https://example.com/issues/1"},
            ]},
            {"type": "function_call", "call_id": "call_1", "name": "gh"},
            {"type": "function_call_output", "call_id": "call_1", "output": "issue body: complex spec"},
        ]});
        let texts = classification_texts(ApiKind::OpenaiResponses, &payload);
        assert_eq!(
            texts,
            vec![
                "issue body: complex spec",
                "見て: https://example.com/issues/1"
            ]
        );
    }

    #[test]
    fn classification_texts_json_encode_a_non_string_function_call_output() {
        let payload = json!({ "input": [
            {"type": "function_call_output", "call_id": "call_1", "output": {"temp_f": 72}},
        ]});
        let texts = classification_texts(ApiKind::OpenaiResponses, &payload);
        assert_eq!(texts, vec!["{\"temp_f\":72}"]);
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

    #[test]
    fn system_prompt_text_reads_an_anthropic_system_string() {
        let payload = json!({
            "system": "You are a read-only exploration subagent.",
            "messages": [],
        });
        assert_eq!(
            system_prompt_text(ApiKind::AnthropicMessages, &payload).as_deref(),
            Some("You are a read-only exploration subagent.")
        );
    }

    #[test]
    fn system_prompt_text_joins_an_anthropic_system_block_array() {
        let payload = json!({
            "system": [
                {"type": "text", "text": "part one"},
                {"type": "text", "text": "part two"},
            ],
            "messages": [],
        });
        assert_eq!(
            system_prompt_text(ApiKind::AnthropicMessages, &payload).as_deref(),
            Some("part one\n\npart two")
        );
    }

    #[test]
    fn system_prompt_text_reads_the_leading_chat_system_message() {
        let payload = json!({ "messages": [
            {"role": "system", "content": "be terse"},
            {"role": "user", "content": "hi"},
        ]});
        assert_eq!(
            system_prompt_text(ApiKind::OpenaiChat, &payload).as_deref(),
            Some("be terse")
        );
    }

    #[test]
    fn system_prompt_text_reads_a_chat_developer_message_too() {
        let payload = json!({ "messages": [
            {"role": "developer", "content": "be terse"},
            {"role": "user", "content": "hi"},
        ]});
        assert_eq!(
            system_prompt_text(ApiKind::OpenaiChat, &payload).as_deref(),
            Some("be terse")
        );
    }

    #[test]
    fn system_prompt_text_reads_responses_instructions() {
        let payload = json!({
            "instructions": "You are a read-only exploration subagent.",
            "input": "go explore",
        });
        assert_eq!(
            system_prompt_text(ApiKind::OpenaiResponses, &payload).as_deref(),
            Some("You are a read-only exploration subagent.")
        );
    }

    /// opencode sends its system prompt as a leading `input[]` item rather
    /// than the dedicated `instructions` field — this is the fallback path.
    #[test]
    fn system_prompt_text_falls_back_to_a_responses_input_system_item() {
        let payload = json!({
            "input": [
                {
                    "type": "message",
                    "role": "system",
                    "content": [{"type": "input_text", "text": "be terse"}],
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}],
                },
            ],
        });
        assert_eq!(
            system_prompt_text(ApiKind::OpenaiResponses, &payload).as_deref(),
            Some("be terse")
        );
    }

    /// Empty `instructions` must not win over a real system item in `input`.
    #[test]
    fn system_prompt_text_prefers_input_system_item_when_instructions_is_empty() {
        let payload = json!({
            "instructions": "",
            "input": [{
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "be terse"}],
            }],
        });
        assert_eq!(
            system_prompt_text(ApiKind::OpenaiResponses, &payload).as_deref(),
            Some("be terse")
        );
    }

    #[test]
    fn system_prompt_text_is_none_without_a_system_field() {
        let payload = json!({ "messages": [{"role": "user", "content": "hi"}] });
        assert!(system_prompt_text(ApiKind::AnthropicMessages, &payload).is_none());
        assert!(system_prompt_text(ApiKind::OpenaiChat, &payload).is_none());

        let responses_payload = json!({ "input": "hi" });
        assert!(system_prompt_text(ApiKind::OpenaiResponses, &responses_payload).is_none());
    }

    /// Same reasoning as `classification_texts_skip_a_message_that_is_only_a_system_reminder`,
    /// applied to the system prompt: harness boilerplate must count as no
    /// system prompt at all, not as a classification input.
    #[test]
    fn system_prompt_text_is_none_when_it_strips_down_to_only_a_system_reminder() {
        let payload = json!({
            "system": "<system-reminder>CLAUDE.md contents here</system-reminder>",
            "messages": [],
        });
        assert!(system_prompt_text(ApiKind::AnthropicMessages, &payload).is_none());
    }

    fn resolution(route_name: &str) -> route::Resolution {
        route::Resolution {
            route_name: route_name.to_string(),
            targets: Vec::new(),
        }
    }

    #[test]
    fn routing_from_reports_no_classifier() {
        let res = resolution(crate::config::DEFAULT_ROUTE);
        let attempt = SemanticAttempt {
            resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
            outcome: SemanticOutcome::NoClassifier,
            resolved_targets: None,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
            system_score: None,
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
        let res = resolution(crate::config::DEFAULT_ROUTE);
        let attempt = SemanticAttempt {
            resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
            outcome: SemanticOutcome::NoText,
            resolved_targets: None,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
            system_score: None,
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
        let res = resolution("role-writer");
        let attempt = SemanticAttempt {
            resolve_as: "role-writer".to_string(),
            outcome: SemanticOutcome::Matched { texts_back: 0 },
            resolved_targets: None,
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
            system_score: None,
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
        let res = resolution("role-tester");
        let attempt = SemanticAttempt {
            resolve_as: "role-tester".to_string(),
            outcome: SemanticOutcome::Matched { texts_back: 2 },
            resolved_targets: None,
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
            system_score: None,
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

    /// `MatchedSystem`'s own trace shape: `mode` is `semantic_system`, the
    /// threshold reported is the stricter system-prompt one (not the
    /// ordinary `CLASSIFICATION_THRESHOLD`), and the reason says outright
    /// that user texts were never consulted — the whole point of trying the
    /// system prompt first.
    #[test]
    fn routing_from_reports_a_system_prompt_match() {
        let res = resolution("role-explorer");
        let attempt = SemanticAttempt {
            resolve_as: "role-explorer".to_string(),
            outcome: SemanticOutcome::MatchedSystem,
            resolved_targets: None,
            candidates: vec![
                TraceCandidate {
                    route: "role-explorer".to_string(),
                    score: 0.65,
                },
                TraceCandidate {
                    route: "role-implementer".to_string(),
                    score: 0.4,
                },
            ],
            score: Some(0.65),
            embed_ms: 2,
            decided_by_text: Some("You are a read-only exploration subagent.".to_string()),
            walk: Vec::new(),
            system_score: Some(0.65),
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "semantic_system");
        assert_eq!(routing.matched_route, "role-explorer");
        assert_eq!(routing.score, Some(0.65));
        assert_eq!(routing.threshold, Some(SYSTEM_CLASSIFICATION_THRESHOLD));
        assert_eq!(routing.embed_ms, Some(2));
        assert_eq!(routing.candidates.len(), 2);
        assert_eq!(routing.system_score, Some(0.65));
        assert!(
            routing.reason.contains("system prompt"),
            "{}",
            routing.reason
        );
        assert!(
            routing.reason.contains("user texts were not consulted"),
            "{}",
            routing.reason
        );
        assert_eq!(
            routing.decided_by_text.as_deref(),
            Some("You are a read-only exploration subagent.")
        );
        // No user-text walk ran at all — the system prompt decided this on
        // its own.
        assert!(routing.walk.is_none());
    }

    #[test]
    fn routing_from_explains_a_fallback_below_threshold() {
        let res = resolution(crate::config::DEFAULT_ROUTE);
        let attempt = SemanticAttempt {
            resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
            outcome: SemanticOutcome::BelowThreshold { texts_tried: 3 },
            resolved_targets: None,
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
            // A system prompt was present and attempted but missed
            // `SYSTEM_CLASSIFICATION_THRESHOLD`, so the user-text walk ran
            // (and produced the rest of this attempt) — `system_score` must
            // still reach the trace record even though it played no part in
            // the final decision.
            system_score: Some(0.3),
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "semantic");
        assert_eq!(routing.matched_route, crate::config::DEFAULT_ROUTE);
        assert_eq!(routing.score, Some(0.2));
        assert_eq!(routing.threshold, Some(CLASSIFICATION_THRESHOLD));
        assert_eq!(routing.candidates.len(), 1);
        assert_eq!(
            routing.system_score,
            Some(0.3),
            "system-prompt attempts are recorded even on a miss, for threshold tuning"
        );
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
        let res = resolution("gpt-5-codex");
        let attempt = SemanticAttempt {
            resolve_as: "gpt-5-codex".to_string(),
            outcome: SemanticOutcome::Manual,
            resolved_targets: None,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
            system_score: None,
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
        let res = resolution("role-writer");
        let attempt = SemanticAttempt {
            resolve_as: "role-writer".to_string(),
            outcome: SemanticOutcome::UtilityBypass(UtilityBypassResolution::RequestedModel),
            resolved_targets: None,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
            system_score: None,
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
        let res = resolution(crate::config::DEFAULT_ROUTE);
        let attempt = SemanticAttempt {
            resolve_as: crate::config::DEFAULT_ROUTE.to_string(),
            outcome: SemanticOutcome::UtilityBypass(UtilityBypassResolution::DefaultFallback),
            resolved_targets: None,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
            system_score: None,
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

    /// The third `UtilityBypass` shape: `Config::auto_mode` configured, so
    /// the reason must say so distinctly from either of the other two — a
    /// human reading the trace log needs to tell "pinned to the fast
    /// operator-chosen target" apart from "fell back to the requested model"
    /// or "fell back to `default`".
    #[test]
    fn routing_from_reports_utility_bypass_resolved_via_auto_mode_config() {
        let res = resolution(AUTO_MODE_LABEL);
        let attempt = SemanticAttempt {
            resolve_as: AUTO_MODE_LABEL.to_string(),
            outcome: SemanticOutcome::UtilityBypass(UtilityBypassResolution::AutoModeConfig),
            resolved_targets: None,
            candidates: Vec::new(),
            score: None,
            embed_ms: 0,
            decided_by_text: None,
            walk: Vec::new(),
            system_score: None,
        };

        let routing = routing_from(&res, attempt);

        assert_eq!(routing.mode, "utility_bypass");
        assert_eq!(routing.matched_route, AUTO_MODE_LABEL);
        assert!(
            routing.reason.contains("autoMode"),
            "reason should mention the configured autoMode target: {}",
            routing.reason
        );
        assert!(
            !routing.reason.contains("matches no route"),
            "the auto_mode path never blames a missing route match: {}",
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
            #[cfg(feature = "semantic")]
            basis_cache: std::sync::Arc::new(crate::server::ui::pca::BasisCache::new()),
            live: None,
            ui_token: None,
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
                timeout_seconds: None,
                max_concurrent: None,
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
        // A `tool_result` with no `content` at all — genuinely textless,
        // unlike one carrying real content (which now *does* classify; see
        // `classification_texts_prefer_a_newer_tool_result_over_an_older_user_message`).
        let payload = json!({ "messages": [
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1"}]},
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

    /// A request that *does* carry a system prompt must behave exactly like
    /// one that doesn't when no classifier is loaded — system-prompt
    /// classification needs a classifier the same way the user-text walk
    /// does, so its presence contributes nothing (`system_score` stays
    /// `None`) rather than changing the outcome or panicking.
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_with_a_system_prompt_but_no_classifier_behaves_like_no_classifier() {
        let (_dir, state) = test_state(classifiable_config());
        let config = state.config.get();
        let payload = json!({
            "system": "You are a read-only exploration subagent.",
            "messages": [{"role": "user", "content": "hello"}],
        });

        let attempt = classify_request(
            &state,
            &config,
            &payload,
            ApiKind::AnthropicMessages,
            "opus",
        );
        assert_eq!(attempt.outcome, SemanticOutcome::NoClassifier);
        assert_eq!(attempt.resolve_as, crate::config::DEFAULT_ROUTE);
        assert!(attempt.system_score.is_none());
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
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::RequestedModel)
        );
        assert_eq!(attempt.resolve_as, "role-writer");
        assert!(attempt.resolved_targets.is_none());
        assert!(attempt.candidates.is_empty());
        assert!(attempt.score.is_none());
        assert_eq!(attempt.embed_ms, 0);
        assert!(attempt.decided_by_text.is_none());
        assert!(attempt.walk.is_empty());
    }

    /// Regression covered directly at the `classify_request` level, not
    /// just `classification_texts`: a `<transcript>`-prefixed message whose
    /// `content` array also carries a `tool_result` block ahead of the text
    /// block must still bypass classification. Before the fix, joining
    /// those blocks in raw array order pushed the `<transcript>` text out of
    /// first position, the bypass's `starts_with` check missed it, and the
    /// request fell through to ordinary semantic classification — exactly
    /// the slow-target failure mode `Config::auto_mode` exists to prevent.
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_bypasses_classification_for_a_transcript_message_that_also_carries_a_tool_result(
    ) {
        let (_dir, state) = test_state(classifiable_config());
        let config = state.config.get();
        let payload = json!({ "messages": [
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "some tool call history"},
                {"type": "text", "text": "<transcript>...</transcript>\nis this safe?"},
            ]},
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
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::RequestedModel)
        );
        assert_eq!(attempt.resolve_as, "role-writer");
    }

    /// System-prompt classification must never even be attempted on a
    /// `<transcript>`-prefixed utility request, no matter how strongly its
    /// system prompt might otherwise match a route — the `<transcript>`
    /// bypass has to stay the first check `classify_request` makes. A
    /// present-but-irrelevant `system` field is enough to prove the check
    /// order: if system-prompt classification ran before the bypass, this
    /// request (whose only text is the `<transcript>` yes/no prompt) would
    /// still resolve as a bypass here since there is no loaded classifier —
    /// but `system_score` being `None` shows the system-prompt step was
    /// never reached at all, not merely that it found nothing.
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_transcript_bypass_wins_over_a_system_prompt() {
        let (_dir, state) = test_state(classifiable_config());
        let config = state.config.get();
        let payload = json!({
            "system": "You are a read-only exploration subagent.",
            "messages": [
                {"role": "user", "content": "<transcript>\nsome tool call history\n</transcript>\nis this safe?"},
            ],
        });

        let attempt = classify_request(
            &state,
            &config,
            &payload,
            ApiKind::AnthropicMessages,
            "role-writer",
        );

        assert_eq!(
            attempt.outcome,
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::RequestedModel)
        );
        assert_eq!(attempt.resolve_as, "role-writer");
        assert!(attempt.system_score.is_none());
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
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::DefaultFallback)
        );
        assert_eq!(attempt.resolve_as, crate::config::DEFAULT_ROUTE);
        assert!(attempt.resolved_targets.is_none());
    }

    /// The fix for the bug that motivated `Config::auto_mode`: when it is
    /// configured, a `<transcript>`-prefixed request must resolve straight
    /// to its targets — never through `route::resolve` (by the requested
    /// model name or `default`), so it can never land on a slow, shared
    /// route. `resolved_targets` carries the pre-resolved targets directly,
    /// and `resolve_as` is the display-only `AUTO_MODE_LABEL`, never a real
    /// route name.
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_resolves_via_the_configured_auto_mode_target() {
        let mut config = classifiable_config();
        config.auto_mode = Some(crate::config::ModelConfig {
            default: "anthropic/claude-haiku-fast".to_string(),
            fallbacks: Vec::new(),
        });
        let (_dir, state) = test_state(config.clone());
        let payload = json!({ "messages": [
            {"role": "user", "content": "<transcript>...</transcript>\nis this safe?"},
        ]});

        // Deliberately pass a requested model name that resolves to a real
        // route (`role-writer`), so a pass here can only be explained by the
        // `auto_mode` branch actually winning over the requested-model
        // fallback, not by coincidence.
        let attempt = classify_request(
            &state,
            &config,
            &payload,
            ApiKind::AnthropicMessages,
            "role-writer",
        );

        assert_eq!(
            attempt.outcome,
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::AutoModeConfig)
        );
        assert_eq!(attempt.resolve_as, AUTO_MODE_LABEL);
        let targets = attempt
            .resolved_targets
            .expect("auto_mode resolves targets directly");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].model_ref.model, "claude-haiku-fast");
    }

    /// `mark_utility_bypass_targets` is the one place `Target::is_utility_bypass`
    /// gets set — this pins its contract directly rather than only through
    /// the full `proxy()` handler (which would need a live upstream to
    /// exercise end to end). Every `UtilityBypassResolution` variant must
    /// mark its targets; every other outcome must leave them untouched.
    #[test]
    fn mark_utility_bypass_targets_marks_every_utility_bypass_variant() {
        for outcome in [
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::AutoModeConfig),
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::RequestedModel),
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::DefaultFallback),
        ] {
            let mut resolution = route::Resolution {
                route_name: "whatever".to_string(),
                targets: vec![test_target(ApiKind::AnthropicMessages)],
            };
            mark_utility_bypass_targets(&mut resolution, &outcome);
            assert!(
                resolution.targets[0].is_utility_bypass,
                "{outcome:?} should mark its targets"
            );
        }
    }

    #[test]
    fn mark_utility_bypass_targets_leaves_ordinary_outcomes_untouched() {
        let mut resolution = route::Resolution {
            route_name: "role-writer".to_string(),
            targets: vec![test_target(ApiKind::AnthropicMessages)],
        };
        mark_utility_bypass_targets(&mut resolution, &SemanticOutcome::Matched { texts_back: 0 });
        assert!(!resolution.targets[0].is_utility_bypass);
    }

    /// Defensive fallback: if `Config::auto_mode` somehow fails to resolve at
    /// request time (it should never happen in practice — `validate` checks
    /// it at config-load time), the request must not be dropped. It falls
    /// through to the pre-existing requested-model/`default` resolution
    /// instead of erroring out.
    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_falls_back_when_auto_mode_fails_to_resolve() {
        let mut config = classifiable_config();
        // References a provider that does not exist — `validate` would
        // reject this at load time, but `classify_request` must still cope
        // defensively if it ever sees a config in this state.
        config.auto_mode = Some(crate::config::ModelConfig {
            default: "does-not-exist/some-model".to_string(),
            fallbacks: Vec::new(),
        });
        let (_dir, state) = test_state(config.clone());
        let payload = json!({ "messages": [
            {"role": "user", "content": "<transcript>...</transcript>\nis this safe?"},
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
            SemanticOutcome::UtilityBypass(UtilityBypassResolution::RequestedModel)
        );
        assert_eq!(attempt.resolve_as, "role-writer");
        assert!(attempt.resolved_targets.is_none());
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
