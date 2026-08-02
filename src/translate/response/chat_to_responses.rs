//! `openai-chat` response → `openai-responses` response (non-streaming).

use serde_json::{json, Value};

use super::{body_model, now_epoch_seconds, response_id, usage_field};

/// Translate a complete OpenAI Chat completion body into an OpenAI Responses
/// object (non-streaming).
///
/// The two shapes describe the same outcome from opposite ends of OpenAI's
/// own API surface, so the fields line up more directly than the Anthropic
/// mapping does — but `chat` has no concept of a `status`, an
/// `incomplete_details`, or an `output` array of typed items, so those are
/// synthesized here rather than copied.
///
/// `model` is the target model the proxy resolved the request to; used only
/// when the upstream body does not name a model itself. Never panics: a
/// malformed or empty `body` yields a thin-but-valid response rather than
/// propagating a failure, for the same reason [`super::chat_to_anthropic::chat_to_anthropic`] does.
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
