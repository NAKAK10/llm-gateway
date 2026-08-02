//! `openai-chat` response → `anthropic-messages` response (non-streaming).

use serde_json::{json, Value};

use super::{body_model, message_id, stop_reason_from_finish_reason, usage_field};

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
    let output_tokens = usage_field(usage, "completion_tokens");
    let cache_read = usage
        .and_then(|u| u.get("prompt_tokens_details"))
        .map(|d| usage_field(Some(d), "cached_tokens"))
        .unwrap_or(0);
    // `prompt_tokens` includes `cached_tokens` (OpenAI's convention), but
    // Anthropic's `input_tokens`/`cache_read_input_tokens` are exclusive —
    // reporting `prompt_tokens` verbatim as `input_tokens` here would make a
    // client that sums the two (Claude Code does, to show context usage)
    // double-count the cached portion. Subtracting matches the same
    // normalization `usage::parse` already applies to the gateway's own
    // accounting (see #24); see `docs/decisions.md` for when this changed.
    let input_tokens = usage_field(usage, "prompt_tokens").saturating_sub(cache_read);

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
    fn usage_is_mapped_excluding_cached_tokens_from_input_tokens() {
        // #24: Anthropic's `input_tokens`/`cache_read_input_tokens` are
        // exclusive, unlike OpenAI's `prompt_tokens` (which already includes
        // `cached_tokens`) — so `prompt_tokens` (10) minus `cached_tokens`
        // (3) is what a client that sums the two must see, or it
        // double-counts the cached portion.
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
                "input_tokens": 7,
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
}
