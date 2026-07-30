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

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::response::Response;
use http::HeaderMap;
use time::format_description::well_known::Rfc3339;

use crate::config::{ApiKind, Config};
use crate::error::Error;
use crate::record::trace_log::{
    TraceCandidate, TraceInput, TraceRecord, TraceResolved, TraceRouting, TraceUsage,
};
use crate::record::usage_log::UsageRecord;
use crate::record::Recorder;
use crate::route;
use crate::server::{client_name, endpoint_api, error_response, passthrough, AppState};
use crate::translate::adapter::{self, ResponseShape};
use crate::translate::Translation;
use crate::upstream::{self, Attempt};
use crate::usage::tee::{self, StreamOutcome};

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

    // If `requested_model` names an auto route, this classifies the request
    // and picks a candidate to resolve against instead — a plain route (the
    // overwhelming majority of requests) never reaches `classify_request`'s
    // body, since the exact-name check inside it fails immediately.
    let semantic_attempt =
        classify_request(&state, &config, &requested_model, &payload, expected_api);
    let resolve_target = semantic_attempt
        .as_ref()
        .map(|attempt| attempt.resolve_as.as_str())
        .unwrap_or(requested_model.as_str());

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

    // Protocol check. Validation guarantees a route's targets all share one
    // ApiKind, so checking the first is checking them all.
    //
    // Equal protocols are the passthrough path and stay byte-for-byte. When
    // they differ, a translation may exist — that is what lets Claude Code
    // (`anthropic-messages` only) reach the many `openai-chat` providers. Only
    // a direction nothing can translate is still refused, because forwarding a
    // request in the wrong protocol produces a confusing upstream 400 instead
    // of an explanation.
    let route_api = resolution.targets[0].api;
    let translation = if route_api == expected_api {
        None
    } else {
        match Translation::select(expected_api, route_api) {
            Some(translation) => Some(translation),
            None => {
                return error_response(
                    http::StatusCode::BAD_REQUEST,
                    &format!(
                        "route `{}` is backed by {route_api} providers but {endpoint} speaks \
                         {expected_api}, and this gateway cannot translate {expected_api} → \
                         {route_api}; use the matching endpoint or point the route at a \
                         {expected_api} provider",
                        resolution.route_name,
                    ),
                );
            }
        }
    };

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
    // confidently wrong number. First target only.
    if count_tokens {
        if let Some(translation) = translation.filter(|t| !t.can_forward_count_tokens()) {
            return count_tokens_locally(
                &state,
                &client,
                endpoint,
                &requested_model,
                trace_input,
                &resolution,
                translation,
                &payload,
                semantic_attempt,
            );
        }
        resolution.targets.truncate(1);
    }

    let started = Instant::now();
    let mut attempts = Vec::new();

    let build = |target: &route::Target| -> crate::error::Result<Attempt> {
        // One translation for the whole resolution, not one per target:
        // validation guarantees every target of a route speaks the same
        // protocol, so the pair decided above holds for each attempt.
        let mut request = match translation {
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
            headers: passthrough::upstream_headers(&headers, &target.headers),
            count_tokens,
        })
    };

    let accepted =
        match upstream::send_with_fallback(&state.http, &resolution, build, &mut attempts).await {
            Ok(accepted) => accepted,
            Err(err) => {
                let dur_ms = started.elapsed().as_millis() as u64;
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
                        intended_resolved(&resolution, translation),
                        attempts,
                        None,
                        semantic_attempt,
                    ));
                }
                return error_response(http::StatusCode::BAD_GATEWAY, &err.to_string());
            }
        };

    let status = http::StatusCode::from_u16(accepted.response.status().as_u16())
        .unwrap_or(http::StatusCode::BAD_GATEWAY);
    let upstream_headers = convert_headers(accepted.response.headers());

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
                }),
                semantic_attempt,
            ));
        }
    });

    // Usage is observed on the *upstream* bytes, in the upstream's protocol,
    // before any translation — which is what keeps token accounting correct on
    // a translated route (`usage::parse` never sees a rebuilt body).
    let observed = tee::observe(
        accepted.response.bytes_stream(),
        accepted.api,
        streaming,
        report,
    );

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
    translation: Translation,
    payload: &serde_json::Value,
    semantic_attempt: Option<SemanticAttempt>,
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
            intended_resolved(resolution, Some(translation)),
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
/// target is the one that would have served it, so that is what gets recorded.
fn intended_resolved(
    resolution: &route::Resolution,
    translation: Option<Translation>,
) -> TraceResolved {
    let target = &resolution.targets[0];
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
    semantic_attempt: Option<SemanticAttempt>,
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

/// What semantic classification did for one request, distilled into what
/// [`trace_record`] needs to fill in [`TraceRouting`].
///
/// `resolve_as` is what actually went into `route::resolve` — either the
/// winning candidate's route name, or (when nothing cleared the threshold)
/// `requested_model` unchanged, passed back through so the trace-building
/// code does not need to remember it separately.
struct SemanticAttempt {
    resolve_as: String,
    matched: bool,
    candidates: Vec<TraceCandidate>,
    score: Option<f32>,
    threshold: f32,
    embed_ms: u64,
}

/// Build the `routing` block of a trace record.
///
/// `semantic_attempt` is `None` whenever classification never ran for this
/// request — `requested_model` did not name an auto route, or (`semantic`
/// feature disabled, or no `semantic` route existed at startup) the
/// classifier was never loaded — and the trace stays `mode: "explicit"`,
/// exactly as before semantic routing existed.
fn routing_from(
    resolution: &route::Resolution,
    semantic_attempt: Option<SemanticAttempt>,
) -> TraceRouting {
    let Some(attempt) = semantic_attempt else {
        return TraceRouting {
            mode: "explicit".to_string(),
            matched_route: resolution.route_name.clone(),
            reason: resolution.kind.reason().to_string(),
            candidates: Vec::new(),
            score: None,
            threshold: None,
            embed_ms: None,
        };
    };

    let reason = if attempt.matched {
        "semantic classification matched a candidate".to_string()
    } else {
        format!(
            "semantic classification: best candidate did not clear threshold {:.2}; \
             falling back to the auto route's own model",
            attempt.threshold
        )
    };

    TraceRouting {
        mode: "semantic".to_string(),
        matched_route: resolution.route_name.clone(),
        reason,
        candidates: attempt.candidates,
        score: attempt.score,
        threshold: Some(attempt.threshold),
        embed_ms: Some(attempt.embed_ms),
    }
}

/// Classify `requested_model` against its `semantic` candidates, if it names
/// an auto route and the classifier is loaded.
///
/// `None` when `requested_model` does not name a route with a `semantic`
/// block, when the classifier was never built (no such route existed at
/// startup — see `crate::server::prepare_classifier`), or when the request's
/// last user message could not be extracted at all. In every one of those
/// cases classification never ran, and the caller falls back to resolving
/// `requested_model` directly, with the trace staying `mode: "explicit"`.
#[cfg(feature = "semantic")]
fn classify_request(
    state: &AppState,
    config: &Config,
    requested_model: &str,
    payload: &serde_json::Value,
    expected_api: ApiKind,
) -> Option<SemanticAttempt> {
    let classifier = state.classifier.as_ref()?;
    let semantic = config.routes.get(requested_model)?.semantic.as_ref()?;
    let threshold = semantic.threshold;

    let text = classification_text(expected_api, payload)?;
    let verdict = classifier.classify(requested_model, &text, expected_api)?;

    let candidates: Vec<TraceCandidate> = verdict
        .candidates
        .iter()
        .map(|(route, score)| TraceCandidate {
            route: route.clone(),
            score: *score,
        })
        .collect();
    // The top candidate's score regardless of whether it cleared the
    // threshold — useful in the trace log even on a fallback, to show how
    // close the closest candidate came.
    let score = verdict.candidates.first().map(|(_, score)| *score);

    Some(match verdict.matched {
        Some((route, _)) => SemanticAttempt {
            resolve_as: route,
            matched: true,
            candidates,
            score,
            threshold,
            embed_ms: verdict.embed_ms,
        },
        None => SemanticAttempt {
            resolve_as: requested_model.to_string(),
            matched: false,
            candidates,
            score,
            threshold,
            embed_ms: verdict.embed_ms,
        },
    })
}

#[cfg(not(feature = "semantic"))]
fn classify_request(
    _state: &AppState,
    _config: &Config,
    _requested_model: &str,
    _payload: &serde_json::Value,
    _expected_api: ApiKind,
) -> Option<SemanticAttempt> {
    None
}

/// The last user message's text, untruncated — used for semantic
/// classification. `Embedder::embed` does its own bounding (800 chars / 64
/// tokens), so the 200-character truncation `extract_input` applies for the
/// trace log must not leak into what gets classified.
#[cfg_attr(not(feature = "semantic"), allow(dead_code))]
fn classification_text(api: ApiKind, payload: &serde_json::Value) -> Option<String> {
    let messages = match api {
        ApiKind::OpenaiResponses => payload.get("input"),
        _ => payload.get("messages"),
    };
    last_user_text(api, messages)
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"))
}

/// reqwest and axum currently agree on `http` 1.x header types, but going
/// through an explicit copy keeps that an implementation detail.
fn convert_headers(headers: &http::HeaderMap) -> http::HeaderMap {
    headers.clone()
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
    fn classification_text_is_not_truncated_unlike_the_trace_log_version() {
        // `Embedder::embed` does its own bounding (800 chars); the 200-char
        // truncation `extract_input` applies for the trace log must not
        // leak into what actually gets classified.
        let long = "a".repeat(300);
        let payload = json!({ "messages": [{"role": "user", "content": long.clone()}] });
        let text = classification_text(ApiKind::AnthropicMessages, &payload).unwrap();
        assert_eq!(text, long);
    }

    #[test]
    fn classification_text_is_none_without_a_user_message() {
        let payload = json!({});
        assert!(classification_text(ApiKind::OpenaiChat, &payload).is_none());
    }

    fn resolution(route_name: &str, kind: route::MatchKind) -> route::Resolution {
        route::Resolution {
            route_name: route_name.to_string(),
            kind,
            targets: Vec::new(),
        }
    }

    #[test]
    fn routing_from_stays_explicit_without_a_semantic_attempt() {
        let res = resolution("claude-*", route::MatchKind::Wildcard);
        let routing = routing_from(&res, None);

        assert_eq!(routing.mode, "explicit");
        assert_eq!(routing.matched_route, "claude-*");
        assert_eq!(routing.reason, route::MatchKind::Wildcard.reason());
        assert!(routing.candidates.is_empty());
        assert!(routing.score.is_none());
        assert!(routing.threshold.is_none());
        assert!(routing.embed_ms.is_none());
    }

    #[test]
    fn routing_from_reports_a_semantic_match() {
        let res = resolution("role-writer", route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: "role-writer".to_string(),
            matched: true,
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
            threshold: 0.45,
            embed_ms: 2,
        };

        let routing = routing_from(&res, Some(attempt));

        assert_eq!(routing.mode, "semantic");
        assert_eq!(routing.matched_route, "role-writer");
        assert_eq!(routing.score, Some(0.8));
        assert_eq!(routing.threshold, Some(0.45));
        assert_eq!(routing.embed_ms, Some(2));
        assert_eq!(routing.candidates.len(), 2);
        assert!(routing.reason.contains("matched"), "{}", routing.reason);
    }

    #[test]
    fn routing_from_explains_a_fallback_below_threshold() {
        let res = resolution("auto", route::MatchKind::Exact);
        let attempt = SemanticAttempt {
            resolve_as: "auto".to_string(),
            matched: false,
            candidates: vec![TraceCandidate {
                route: "role-writer".to_string(),
                score: 0.2,
            }],
            score: Some(0.2),
            threshold: 0.45,
            embed_ms: 1,
        };

        let routing = routing_from(&res, Some(attempt));

        assert_eq!(routing.mode, "semantic");
        assert_eq!(routing.matched_route, "auto");
        assert_eq!(routing.score, Some(0.2));
        assert_eq!(routing.threshold, Some(0.45));
        assert_eq!(routing.candidates.len(), 1);
        assert!(
            routing.reason.contains("0.45"),
            "reason should mention the threshold: {}",
            routing.reason
        );
    }

    /// Builds just enough `AppState` to exercise `classify_request` without a
    /// loaded embedding model: a `Recorder` over a scratch directory and a
    /// config, nothing more.
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

    fn auto_route_config() -> crate::config::Config {
        use crate::config::{ModelConfig, ProviderConfig, RouteConfig, SecretRef, SemanticConfig};

        let mut config = crate::config::Config::default();
        config.providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                base_url: "https://example.test".to_string(),
                api: ApiKind::AnthropicMessages,
                api_key: Some(SecretRef::new("k")),
                headers: Default::default(),
                inject_usage: true,
            },
        );
        config.routes.insert(
            "role-writer".to_string(),
            RouteConfig {
                description: Some(crate::config::Description("writes prose".to_string())),
                model: ModelConfig {
                    default: "anthropic/opus-pinned".to_string(),
                    fallbacks: Vec::new(),
                },
                ..Default::default()
            },
        );
        config.routes.insert(
            "auto".to_string(),
            RouteConfig {
                model: ModelConfig {
                    default: "anthropic/opus-pinned".to_string(),
                    fallbacks: Vec::new(),
                },
                semantic: Some(SemanticConfig {
                    candidates: vec!["role-writer".to_string()],
                    threshold: 0.45,
                }),
                ..Default::default()
            },
        );
        config
    }

    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_does_nothing_without_a_loaded_classifier() {
        // A `semantic` route existed at startup but, in this test, the
        // classifier is `None` — same as a config that never asked for
        // semantic routing at all: the caller must fall back, not panic.
        let (_dir, state) = test_state(auto_route_config());
        let config = state.config.get();
        let payload = json!({ "messages": [{"role": "user", "content": "hello"}] });

        let attempt = classify_request(
            &state,
            &config,
            "auto",
            &payload,
            ApiKind::AnthropicMessages,
        );
        assert!(attempt.is_none());
    }

    #[cfg(feature = "semantic")]
    #[tokio::test]
    async fn classify_request_does_nothing_for_a_plain_route() {
        let (_dir, state) = test_state(auto_route_config());
        let config = state.config.get();
        let payload = json!({ "messages": [{"role": "user", "content": "hello"}] });

        let attempt = classify_request(
            &state,
            &config,
            "role-writer",
            &payload,
            ApiKind::AnthropicMessages,
        );
        assert!(attempt.is_none());
    }

    #[cfg(not(feature = "semantic"))]
    #[tokio::test]
    async fn classify_request_is_always_a_no_op_without_the_semantic_feature() {
        let (_dir, state) = test_state(auto_route_config());
        let config = state.config.get();
        let payload = json!({ "messages": [{"role": "user", "content": "hello"}] });

        let attempt = classify_request(
            &state,
            &config,
            "auto",
            &payload,
            ApiKind::AnthropicMessages,
        );
        assert!(attempt.is_none());
    }
}
