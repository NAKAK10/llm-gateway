//! `openai-chat` response → `anthropic-messages` response (non-streaming).
//!
//! An OpenAI Chat Completion and an Anthropic Message describe the same
//! outcome — assistant text, maybe tool calls, a stop reason, token counts —
//! but disagree on almost every field name and a few of the semantics. This
//! module is the one place that reconciles them for a complete, buffered
//! body; [`super::stream`] does the same job incrementally for SSE.
//!
//! What does **not** survive the trip:
//!
//! - **Multiple choices.** Anthropic has no concept of `n > 1`; only
//!   `choices[0]` is read. No client this gateway serves asks for more.
//! - **Reasoning content.** Some `openai-chat` servers attach
//!   `message.reasoning_content` or `message.reasoning` alongside the answer.
//!   These are dropped rather than turned into an Anthropic `thinking` block:
//!   a real `thinking` block carries a `signature` that only Anthropic's own
//!   models can produce, and Claude Code either rejects an unsigned one or
//!   treats it as untrusted. Forwarding the reasoning text as an ordinary
//!   `text` block would also be wrong — it would be presented as the answer.
//!   Dropping it is the honest choice until there is a real translation for
//!   it.
//! - **Which stop string matched.** `openai-chat` does not report this, so
//!   `stop_sequence` is always `null`.
//!
//! `usage` in the translated body is for the *client's* benefit only — it
//! lets Claude Code render token counts the way it expects. The gateway's own
//! accounting reads the upstream body directly, in `usage::parse`, and never
//! looks at anything this module produces.

use serde_json::{json, Value};

/// Translate a complete OpenAI Chat completion body into an Anthropic Message.
///
/// `model` is the target model the proxy resolved the request to; it is used
/// only when the upstream body does not name a model itself. Never panics or
/// errors: a malformed or empty `body` yields a thin-but-valid message rather
/// than propagating a failure, because by the time this runs the request has
/// already been accepted.
pub fn chat_to_anthropic(body: &Value, model: &str) -> Value {
    let id = message_id(body);
    let model = body_model(body).unwrap_or(model).to_string();

    let choice = body.get("choices").and_then(|c| c.get(0));
    let message = choice.and_then(|c| c.get("message"));

    let mut content = Vec::new();
    if let Some(text) = message.map(message_text).filter(|t| !t.is_empty()) {
        content.push(json!({ "type": "text", "text": text }));
    }
    let mut saw_tool_use = false;
    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for call in tool_calls {
            content.push(tool_use_block(call));
            saw_tool_use = true;
        }
    }
    // Anthropic requires a non-empty `content` array; an empty text block is
    // the closest honest equivalent to "the model said nothing".
    if content.is_empty() {
        content.push(json!({ "type": "text", "text": "" }));
    }

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str());
    // A tool call in the content always wins over the reported finish reason:
    // several openai-chat servers report `finish_reason: "stop"` alongside
    // populated `tool_calls`, and an Anthropic client that sees `end_turn`
    // never executes the tool call it was just handed.
    let stop_reason = if saw_tool_use {
        "tool_use"
    } else {
        stop_reason_from_finish_reason(finish_reason)
    };

    let usage = body.get("usage");
    let input_tokens = usage_field(usage, "prompt_tokens");
    let output_tokens = usage_field(usage, "completion_tokens");
    let cache_read = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .map(|d| usage_field(Some(d), "cached_tokens"))
        .unwrap_or(0);

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_read_input_tokens": cache_read,
            // openai-chat has no equivalent of Anthropic's prompt-caching
            // write cost, so this is always 0.
            "cache_creation_input_tokens": 0,
        },
    })
}

/// Translate a complete OpenAI Chat completion body into an OpenAI Responses
/// object (non-streaming).
///
/// The two shapes describe the same outcome from opposite ends of OpenAI's
/// own API surface, so the fields line up more directly than the Anthropic
/// mapping above does — but `chat` has no concept of a `status`, an
/// `incomplete_details`, or an `output` array of typed items, so those are
/// synthesized here rather than copied.
///
/// `model` is the target model the proxy resolved the request to; used only
/// when the upstream body does not name a model itself. Never panics: a
/// malformed or empty `body` yields a thin-but-valid response rather than
/// propagating a failure, for the same reason [`chat_to_anthropic`] does.
pub fn chat_to_responses(body: &Value, model: &str) -> Value {
    let id = response_id(body);
    let model = body_model(body).unwrap_or(model).to_string();
    let created_at = body
        .get("created")
        .and_then(|v| v.as_u64())
        .unwrap_or_else(now_epoch_seconds);

    let choice = body.get("choices").and_then(|c| c.get(0));
    let message = choice.and_then(|c| c.get("message"));

    let mut output = Vec::new();
    if let Some(text) = message.map(message_text).filter(|t| !t.is_empty()) {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", uuid::Uuid::now_v7()),
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "annotations": [], "text": text }],
        }));
    }
    if let Some(tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
    {
        for call in tool_calls {
            output.push(function_call_item(call));
        }
    }

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(|f| f.as_str());
    // `length` is the one `finish_reason` Responses reports as its own
    // top-level `status` rather than folding into a normal completion — Codex
    // reads `incomplete_details.reason` to tell the two apart.
    let (status, incomplete_details) = if finish_reason == Some("length") {
        ("incomplete", Some(json!({ "reason": "max_output_tokens" })))
    } else {
        ("completed", None)
    };

    let usage = body.get("usage");
    let input_tokens = usage_field(usage, "prompt_tokens");
    let output_tokens = usage_field(usage, "completion_tokens");
    let cache_read = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .map(|d| usage_field(Some(d), "cached_tokens"))
        .unwrap_or(0);

    let mut response = serde_json::Map::new();
    response.insert("id".to_string(), json!(id));
    response.insert("object".to_string(), json!("response"));
    response.insert("created_at".to_string(), json!(created_at));
    response.insert("status".to_string(), json!(status));
    if let Some(details) = incomplete_details {
        response.insert("incomplete_details".to_string(), details);
    }
    response.insert("model".to_string(), json!(model));
    response.insert("output".to_string(), Value::Array(output));
    response.insert(
        "usage".to_string(),
        json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            // `chat` has no cost breakdown beyond the cached-token count, so
            // the total is always the sum of the two halves above rather
            // than a number trusted verbatim from upstream.
            "total_tokens": input_tokens + output_tokens,
            "input_tokens_details": { "cached_tokens": cache_read },
        }),
    );
    Value::Object(response)
}

/// The response `id`: the upstream `id` prefixed with `resp_` (unless it
/// already carries that prefix), or a freshly synthesized one when the
/// upstream did not send an `id` at all.
fn response_id(body: &Value) -> String {
    match body.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => {
            if id.starts_with("resp_") {
                id.to_string()
            } else {
                format!("resp_{id}")
            }
        }
        _ => format!("resp_{}", uuid::Uuid::now_v7()),
    }
}

/// Wall-clock fallback for `created_at` when the upstream body carries no
/// `created` field of its own (rare, but some `openai-chat`-compatible
/// servers omit it).
fn now_epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One `message.tool_calls[]` entry → a Responses `function_call` output
/// item.
///
/// `call_id` carries the *upstream* tool call id, not a freshly synthesized
/// one (unless upstream sent none at all): Codex matches a later
/// `function_call_output` input item back to this call by `call_id`, so it
/// must be the same id the assistant model itself produced.
fn function_call_item(call: &Value) -> Value {
    let call_id = call
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::now_v7()));

    let function = call.get("function");
    let name = function
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Unlike the Anthropic mapping, `arguments` stays a JSON *string* here —
    // that is the wire shape Responses itself uses for a `function_call`
    // item, so no parse/re-serialize round trip is needed.
    let arguments = function
        .and_then(|f| f.get("arguments"))
        .and_then(|v| v.as_str())
        .unwrap_or("{}")
        .to_string();

    json!({
        "type": "function_call",
        "id": format!("fc_{}", uuid::Uuid::now_v7()),
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
        "status": "completed",
    })
}

/// The message `id`: the upstream `id` prefixed with `msg_` (unless it
/// already carries that prefix), or a freshly synthesized one when the
/// upstream did not send an `id` at all — Ollama, for one, sometimes omits it.
fn message_id(body: &Value) -> String {
    match body.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() => {
            if id.starts_with("msg_") {
                id.to_string()
            } else {
                format!("msg_{id}")
            }
        }
        _ => format!("msg_{}", uuid::Uuid::now_v7()),
    }
}

/// The body's own `model`, when present and non-empty. Preferred over the
/// proxy-resolved target because the upstream's own answer is the honest one
/// — it is what actually generated the response.
fn body_model(body: &Value) -> Option<&str> {
    body.get("model")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
}

/// Flatten `message.content` into plain text. Usually a string; `null` when
/// the assistant said nothing but made tool calls instead; rarely an array of
/// `{"type":"text","text":…}` parts from proxies that mimic the Responses
/// shape, which are concatenated.
fn message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// One `message.tool_calls[]` entry → an Anthropic `tool_use` content block.
fn tool_use_block(call: &Value) -> Value {
    let id = call
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::now_v7()));

    let function = call.get("function");
    let name = function
        .and_then(|f| f.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // `function.arguments` is a JSON *string* on the wire and must be parsed
    // into an object. When it is missing, empty, or not valid JSON, fall back
    // to `{}` rather than failing the whole translation: a client that
    // receives a string where a JSON object is required errors out entirely,
    // while an empty object just degrades to a tool call with default
    // arguments.
    let input = function
        .and_then(|f| f.get("arguments"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));

    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

/// Map `finish_reason` to an Anthropic `stop_reason`. Callers must apply the
/// tool-call override on top of this — see [`chat_to_anthropic`].
fn stop_reason_from_finish_reason(finish_reason: Option<&str>) -> &'static str {
    match finish_reason {
        Some("stop") => "end_turn",
        Some("length") => "max_tokens",
        Some("tool_calls") | Some("function_call") => "tool_use",
        Some("content_filter") => "end_turn",
        _ => "end_turn",
    }
}

/// Pull a `u64` out of a JSON object field, defaulting to 0 when the field is
/// missing, null, or not a number.
fn usage_field(usage: Option<&Value>, key: &str) -> u64 {
    usage
        .and_then(|u| u.get(key))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// Pull a human-readable message out of an upstream `openai-chat` error
/// body, tolerating the shapes real servers actually send: the common
/// `{"error": {"message": "…", …}}` object, Ollama's bare
/// `{"error": "plain string"}`, and anything else (in which case the status
/// and the raw body beat saying nothing).
fn chat_error_message(body: &Value, status: u16) -> String {
    let error = body.get("error");
    let message = match error {
        Some(Value::Object(_)) => error
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    };
    message.unwrap_or_else(|| format!("upstream returned HTTP {status}: {body}"))
}

/// Translate an upstream error body into the Anthropic error envelope.
///
/// Never panics: even a body that is not the expected `{"error": …}` shape
/// (or is entirely unreadable) produces a valid envelope carrying whatever
/// information could be recovered, so the operator can still tell what went
/// wrong upstream.
pub fn chat_error_to_anthropic(body: &Value, status: u16) -> Value {
    json!({
        "type": "error",
        "error": {
            "type": error_type_from_status(status),
            "message": chat_error_message(body, status),
        },
    })
}

/// Translate an upstream error body into the OpenAI error envelope Responses
/// clients (Codex CLI) expect. Same robustness contract as
/// [`chat_error_to_anthropic`]: never panics, and an unreadable body still
/// produces a valid envelope.
pub fn chat_error_to_responses(body: &Value, status: u16) -> Value {
    json!({
        "error": {
            "message": chat_error_message(body, status),
            "type": error_type_from_status(status),
            "code": Value::Null,
        },
    })
}

/// The gateway's own error, in the Anthropic envelope — for a failure inside
/// translation itself rather than one the upstream reported (see
/// [`chat_error_to_anthropic`] for that case). `type: api_error` because this
/// is the gateway's own failure, not the client's.
pub fn anthropic_gateway_error(message: &str) -> Value {
    json!({
        "type": "error",
        "error": { "type": "api_error", "message": message },
    })
}

/// The gateway's own error, in the OpenAI envelope Responses clients expect
/// — the [`chat_error_to_responses`] counterpart of
/// [`anthropic_gateway_error`].
pub fn responses_gateway_error(message: &str) -> Value {
    json!({
        "error": { "message": message, "type": "api_error", "code": Value::Null },
    })
}

/// Map an HTTP status to one of Anthropic's documented error `type` strings.
/// Claude Code branches on these — notably `rate_limit_error` and
/// `overloaded_error` drive its retry behaviour — so this mapping is
/// behaviour, not cosmetics.
fn error_type_from_status(status: u16) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        500 => "api_error",
        529 => "overloaded_error",
        // Any other 4xx: still the client's fault, so the generic
        // client-error type is the closest honest fit. Any other 5xx: the
        // gateway or the upstream's own fault, so the generic server-error
        // type. Written as a guarded catch-all (rather than an explicit
        // `..=499` range) because the literal arms above already cover the
        // specific codes inside that range and a range arm alongside them
        // reads as overlapping.
        other if (400..500).contains(&other) => "invalid_request_error",
        _ => "api_error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_plain_text_completion_translates_to_a_text_block() {
        let body = json!({
            "id": "chatcmpl-abc",
            "model": "qwen3",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello there" },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 20 },
        });

        let msg = chat_to_anthropic(&body, "fallback-model");

        assert_eq!(msg["id"], "msg_chatcmpl-abc");
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["model"], "qwen3");
        assert_eq!(
            msg["content"],
            json!([{ "type": "text", "text": "hello there" }])
        );
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["stop_sequence"], Value::Null);
    }

    #[test]
    fn a_tool_call_completion_parses_arguments_into_an_object() {
        let body = json!({
            "id": "chatcmpl-1",
            "model": "qwen3",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"city\":\"tokyo\"}",
                        },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(
            msg["content"],
            json!([{
                "type": "tool_use",
                "id": "call_1",
                "name": "get_weather",
                "input": { "city": "tokyo" },
            }])
        );
        assert_eq!(msg["stop_reason"], "tool_use");
    }

    #[test]
    fn finish_reason_stop_with_tool_calls_still_yields_stop_reason_tool_use() {
        // Several openai-chat servers report `stop` alongside populated
        // `tool_calls`; an Anthropic client that sees `end_turn` here never
        // executes the tool. This is the fix the module docs call out.
        let body = json!({
            "id": "chatcmpl-2",
            "model": "qwen3",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "noop", "arguments": "{}" },
                    }],
                },
                "finish_reason": "stop",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(msg["stop_reason"], "tool_use");
    }

    #[test]
    fn unparseable_arguments_become_an_empty_object() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "f", "arguments": "not json" },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(msg["content"][0]["input"], json!({}));
    }

    #[test]
    fn empty_arguments_string_becomes_an_empty_object() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "f", "arguments": "" },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(msg["content"][0]["input"], json!({}));
    }

    #[test]
    fn a_missing_tool_call_id_is_synthesized() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{ "function": { "name": "f", "arguments": "{}" } }],
                },
                "finish_reason": "tool_calls",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        let id = msg["content"][0]["id"].as_str().unwrap();
        assert!(id.starts_with("toolu_"), "{id}");
    }

    #[test]
    fn a_missing_chat_id_is_synthesized_as_a_uuid_v7() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        let id = msg["id"].as_str().unwrap();
        assert!(id.starts_with("msg_"), "{id}");
        assert!(uuid::Uuid::parse_str(id.trim_start_matches("msg_")).is_ok());
    }

    #[test]
    fn a_chat_id_already_prefixed_with_msg_is_not_double_prefixed() {
        let body = json!({
            "id": "msg_already",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(msg["id"], "msg_already");
    }

    #[test]
    fn a_missing_model_falls_back_to_the_proxy_resolved_target() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop",
            }],
        });

        let msg = chat_to_anthropic(&body, "resolved-model");

        assert_eq!(msg["model"], "resolved-model");
    }

    #[test]
    fn array_valued_content_parts_are_flattened_by_concatenation() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": [
                        { "type": "text", "text": "hello " },
                        { "type": "text", "text": "world" },
                    ],
                },
                "finish_reason": "stop",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(
            msg["content"],
            json!([{ "type": "text", "text": "hello world" }])
        );
    }

    #[test]
    fn usage_is_mapped_including_cached_tokens() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "prompt_tokens_details": { "cached_tokens": 3 },
            },
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(
            msg["usage"],
            json!({
                "input_tokens": 10,
                "output_tokens": 20,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 0,
            })
        );
    }

    #[test]
    fn missing_usage_fields_default_to_zero() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(
            msg["usage"],
            json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_read_input_tokens": 0,
                "cache_creation_input_tokens": 0,
            })
        );
    }

    #[test]
    fn an_empty_completion_still_produces_a_valid_message_with_a_blank_text_block() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "" },
                "finish_reason": "stop",
            }],
        });

        let msg = chat_to_anthropic(&body, "m");

        assert_eq!(msg["content"], json!([{ "type": "text", "text": "" }]));
    }

    #[test]
    fn a_completely_empty_body_does_not_panic() {
        let msg = chat_to_anthropic(&json!({}), "fallback");

        assert_eq!(msg["type"], "message");
        assert_eq!(msg["model"], "fallback");
        assert_eq!(msg["content"], json!([{ "type": "text", "text": "" }]));
        assert_eq!(msg["stop_reason"], "end_turn");
    }

    #[test]
    fn finish_reason_length_maps_to_max_tokens() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "cut off" },
                "finish_reason": "length",
            }],
        });

        assert_eq!(chat_to_anthropic(&body, "m")["stop_reason"], "max_tokens");
    }

    #[test]
    fn finish_reason_content_filter_maps_to_end_turn() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "redacted" },
                "finish_reason": "content_filter",
            }],
        });

        assert_eq!(chat_to_anthropic(&body, "m")["stop_reason"], "end_turn");
    }

    #[test]
    fn missing_finish_reason_defaults_to_end_turn() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
            }],
        });

        assert_eq!(chat_to_anthropic(&body, "m")["stop_reason"], "end_turn");
    }

    #[test]
    fn an_object_error_message_is_used_directly() {
        let body = json!({
            "error": { "message": "model not found", "type": "invalid_request_error" },
        });

        let out = chat_error_to_anthropic(&body, 404);

        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "not_found_error");
        assert_eq!(out["error"]["message"], "model not found");
    }

    #[test]
    fn a_string_valued_error_is_used_directly() {
        let body = json!({ "error": "model is not running" });

        let out = chat_error_to_anthropic(&body, 500);

        assert_eq!(out["error"]["type"], "api_error");
        assert_eq!(out["error"]["message"], "model is not running");
    }

    #[test]
    fn a_body_with_no_error_field_produces_a_message_from_the_status_and_body() {
        let body = json!({ "something_else": true });

        let out = chat_error_to_anthropic(&body, 500);

        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("500"), "{message}");
        assert!(message.contains("something_else"), "{message}");
    }

    #[test]
    fn each_status_class_maps_to_its_documented_error_type() {
        let cases: &[(u16, &str)] = &[
            (400, "invalid_request_error"),
            (401, "authentication_error"),
            (403, "permission_error"),
            (404, "not_found_error"),
            (413, "request_too_large"),
            (429, "rate_limit_error"),
            (500, "api_error"),
            (529, "overloaded_error"),
            (422, "invalid_request_error"), // other 4xx
            (503, "api_error"),             // other 5xx
        ];

        for (status, expected) in cases {
            let body = json!({ "error": { "message": "x" } });
            let out = chat_error_to_anthropic(&body, *status);
            assert_eq!(out["error"]["type"], *expected, "status {status}");
        }
    }

    #[test]
    fn a_completely_empty_error_body_does_not_panic() {
        let out = chat_error_to_anthropic(&json!({}), 500);

        assert_eq!(out["type"], "error");
        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("500"), "{message}");
    }

    #[test]
    fn a_plain_text_completion_becomes_a_message_output_item() {
        let body = json!({
            "id": "chatcmpl-abc",
            "model": "qwen3",
            "created": 1_700_000_000u64,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hello there" },
                "finish_reason": "stop",
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 20 },
        });

        let resp = chat_to_responses(&body, "fallback-model");

        assert_eq!(resp["id"], "resp_chatcmpl-abc");
        assert_eq!(resp["object"], "response");
        assert_eq!(resp["created_at"], 1_700_000_000u64);
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["model"], "qwen3");
        assert_eq!(resp["output"][0]["type"], "message");
        assert_eq!(resp["output"][0]["role"], "assistant");
        assert_eq!(resp["output"][0]["status"], "completed");
        assert_eq!(resp["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(resp["output"][0]["content"][0]["text"], "hello there");
        assert_eq!(resp["usage"]["input_tokens"], 10);
        assert_eq!(resp["usage"]["output_tokens"], 20);
        assert_eq!(resp["usage"]["total_tokens"], 30);
    }

    #[test]
    fn a_tool_call_completion_becomes_a_function_call_item_with_the_upstream_call_id() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "get_weather", "arguments": "{\"city\":\"tokyo\"}" },
                    }],
                },
                "finish_reason": "tool_calls",
            }],
        });

        let resp = chat_to_responses(&body, "m");

        // No text was said, so no message item — only the function call.
        assert_eq!(resp["output"].as_array().unwrap().len(), 1);
        let item = &resp["output"][0];
        assert_eq!(item["type"], "function_call");
        // Codex matches a later `function_call_output` back to this call by
        // `call_id`, so it must be the upstream id verbatim, not a fresh one.
        assert_eq!(item["call_id"], "call_1");
        assert_eq!(item["name"], "get_weather");
        assert_eq!(item["arguments"], "{\"city\":\"tokyo\"}");
        assert_eq!(item["status"], "completed");
    }

    #[test]
    fn a_missing_tool_call_id_is_synthesized_for_responses_too() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "tool_calls": [{ "function": { "name": "f", "arguments": "{}" } }],
                },
                "finish_reason": "tool_calls",
            }],
        });

        let resp = chat_to_responses(&body, "m");

        let call_id = resp["output"][0]["call_id"].as_str().unwrap();
        assert!(call_id.starts_with("call_"), "{call_id}");
    }

    #[test]
    fn finish_reason_length_becomes_an_incomplete_response() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "cut off" },
                "finish_reason": "length",
            }],
        });

        let resp = chat_to_responses(&body, "m");

        assert_eq!(resp["status"], "incomplete");
        assert_eq!(resp["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn a_missing_model_falls_back_to_the_proxy_resolved_target_for_responses() {
        let body = json!({
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "hi" },
                "finish_reason": "stop",
            }],
        });

        assert_eq!(
            chat_to_responses(&body, "resolved-model")["model"],
            "resolved-model"
        );
    }

    #[test]
    fn a_completely_empty_body_does_not_panic_for_responses() {
        let resp = chat_to_responses(&json!({}), "fallback");

        assert_eq!(resp["object"], "response");
        assert_eq!(resp["model"], "fallback");
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["output"], json!([]));
    }

    #[test]
    fn an_object_error_message_translates_to_the_openai_envelope() {
        let body = json!({
            "error": { "message": "model not found", "type": "invalid_request_error" },
        });

        let out = chat_error_to_responses(&body, 404);

        assert_eq!(out["error"]["message"], "model not found");
        assert_eq!(out["error"]["type"], "not_found_error");
        // No top-level `type` field — that is an Anthropic envelope detail.
        assert!(out.get("type").is_none());
    }

    #[test]
    fn a_string_valued_error_translates_to_the_openai_envelope() {
        let body = json!({ "error": "model is not running" });

        let out = chat_error_to_responses(&body, 500);

        assert_eq!(out["error"]["message"], "model is not running");
        assert_eq!(out["error"]["type"], "api_error");
    }

    #[test]
    fn a_completely_empty_responses_error_body_does_not_panic() {
        let out = chat_error_to_responses(&json!({}), 500);

        let message = out["error"]["message"].as_str().unwrap();
        assert!(message.contains("500"), "{message}");
    }

    #[test]
    fn gateway_errors_use_each_protocols_own_envelope() {
        let anthropic = anthropic_gateway_error("oversized body");
        assert_eq!(anthropic["type"], "error");
        assert_eq!(anthropic["error"]["type"], "api_error");
        assert_eq!(anthropic["error"]["message"], "oversized body");

        let responses = responses_gateway_error("oversized body");
        assert!(responses.get("type").is_none());
        assert_eq!(responses["error"]["type"], "api_error");
        assert_eq!(responses["error"]["message"], "oversized body");
    }
}
