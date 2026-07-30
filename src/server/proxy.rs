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

use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::response::Response;
use http::HeaderMap;
use time::format_description::well_known::Rfc3339;

use crate::config::ApiKind;
use crate::error::Error;
use crate::record::trace_log::{TraceInput, TraceRecord, TraceResolved, TraceRouting, TraceUsage};
use crate::record::usage_log::UsageRecord;
use crate::record::Recorder;
use crate::route;
use crate::server::{client_name, endpoint_api, error_response, passthrough, AppState};
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

    let mut resolution = match route::resolve(&config, &requested_model) {
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

    // Same-protocol check. Validation guarantees a route's targets all share
    // one ApiKind, so checking the first is checking them all.
    let route_api = resolution.targets[0].api;
    if route_api != expected_api {
        return error_response(
            http::StatusCode::BAD_REQUEST,
            &format!(
                "route `{}` is backed by {route_api} providers but {endpoint} speaks \
                 {expected_api}; use the matching endpoint or point the route at a \
                 {expected_api} provider",
                resolution.route_name,
            ),
        );
    }

    // Token counting is a question, not a generation: the answer is
    // model-specific, so falling back to a different provider would return a
    // confidently wrong number. First target only.
    if count_tokens {
        resolution.targets.truncate(1);
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

    let started = Instant::now();
    let mut attempts = Vec::new();

    let build = |target: &route::Target| -> crate::error::Result<Attempt> {
        let mut request = payload.clone();
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
                        None,
                        attempts,
                        None,
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
    };
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
                Some(resolved),
                attempts,
                (!usage.is_empty()).then_some(TraceUsage {
                    in_tok: usage.input_tokens,
                    out_tok: usage.output_tokens,
                }),
            ));
        }
    });

    let observed = tee::observe(
        accepted.response.bytes_stream(),
        accepted.api,
        streaming,
        report,
    );
    passthrough::respond(status, &upstream_headers, observed)
}

#[allow(clippy::too_many_arguments)]
fn trace_record(
    client: &str,
    endpoint: &str,
    requested_model: &str,
    input: TraceInput,
    resolution: &route::Resolution,
    resolved: Option<TraceResolved>,
    attempts: Vec<crate::record::trace_log::TraceAttempt>,
    usage: Option<TraceUsage>,
) -> TraceRecord {
    TraceRecord {
        ts: now_rfc3339(),
        req_id: uuid::Uuid::now_v7().to_string(),
        client: client.to_string(),
        endpoint: endpoint.to_string(),
        requested_model: requested_model.to_string(),
        input,
        routing: TraceRouting {
            mode: "explicit".to_string(),
            matched_route: resolution.route_name.clone(),
            reason: resolution.kind.reason().to_string(),
            candidates: Vec::new(),
            score: None,
            threshold: None,
            embed_ms: None,
        },
        resolved: resolved.unwrap_or_else(|| TraceResolved {
            provider: resolution.targets[0].model_ref.provider.clone(),
            model: resolution.targets[0].model_ref.model.clone(),
            api: resolution.targets[0].api.as_str().to_string(),
        }),
        attempts,
        usage,
    }
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
}
