//! `anthropic-messages` response → `openai-responses` response
//! (non-streaming).

use serde_json::{json, Value};

use super::{now_epoch_seconds, response_id, usage_field};

/// Translate a complete Anthropic Message body into an OpenAI Responses
/// object (non-streaming).
///
/// Modeled closely on [`super::chat_to_responses::chat_to_responses`] — same
/// output shape (`output: [...]`, `status`, nested `usage`) — but reads
/// Anthropic's native `content[]`/`stop_reason`/`usage` fields as input
/// instead of a chat completion's `choices[0].message`.
///
/// `model` is the target model the proxy resolved the request to; used only
/// when the upstream body does not name a model itself. Never panics: a
/// malformed or empty `body` yields a thin-but-valid response rather than
/// propagating a failure, for the same reason every other translator in this
/// crate does.
pub fn anthropic_to_responses(body: &Value, model: &str) -> Value {
    let id = response_id(body);
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or(model)
        .to_string();
    let created_at = now_epoch_seconds();

    let mut output = Vec::new();
    let blocks = body.get("content").and_then(|c| c.as_array());

    if let Some(text) = blocks.map(|b| content_text(b)).filter(|t| !t.is_empty()) {
        output.push(json!({
            "type": "message",
            "id": format!("msg_{}", uuid::Uuid::now_v7()),
            "role": "assistant",
            "status": "completed",
            "content": [{ "type": "output_text", "annotations": [], "text": text }],
        }));
    }
    if let Some(blocks) = blocks {
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                output.push(function_call_item(block));
            }
        }
    }

    let stop_reason = body.get("stop_reason").and_then(|v| v.as_str());
    // `max_tokens` is the one Anthropic `stop_reason` Responses reports as
    // its own top-level `status` rather than folding into a normal
    // completion — same distinction `chat_to_responses` draws for chat's
    // `finish_reason == "length"`.
    let (status, incomplete_details) = if stop_reason == Some("max_tokens") {
        ("incomplete", Some(json!({ "reason": "max_output_tokens" })))
    } else {
        ("completed", None)
    };

    let usage = body.get("usage");
    let cache_read = usage_field(usage, "cache_read_input_tokens");
    // Anthropic's `input_tokens` already excludes the cached portion (unlike
    // `openai-chat`'s `prompt_tokens`), so unlike `chat_to_responses` there is
    // no subtraction to do here — it is added back only to report Responses'
    // own (inclusive) `input_tokens` convention consistently with the cached
    // count it also reports alongside.
    let input_tokens = usage_field(usage, "input_tokens") + cache_read;
    let output_tokens = usage_field(usage, "output_tokens");

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
            "total_tokens": input_tokens + output_tokens,
            "input_tokens_details": { "cached_tokens": cache_read },
        }),
    );
    Value::Object(response)
}

/// Flatten every `type: "text"` content block into one string, concatenated
/// in order — Anthropic may split an answer across several text blocks
/// (separated by `tool_use` blocks, for instance); Responses wants one
/// `message` item's worth of text.
fn content_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("")
}

/// One `content[]` `tool_use` block → a Responses `function_call` output
/// item.
///
/// `call_id` carries the upstream `tool_use.id` verbatim: a later
/// `function_call_output` input item matches back to this call by that id,
/// so it must be the id the assistant model itself produced, not a fresh one.
/// Anthropic's `input` is already a JSON object; Responses' `arguments` wants
/// a JSON *string*, so this is the one place in this direction a string
/// re-encode is needed (the reverse of
/// [`super::chat_to_anthropic::chat_to_anthropic`]'s `tool_use_block` parse).
fn function_call_item(block: &Value) -> Value {
    let call_id = block
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("call_{}", uuid::Uuid::now_v7()));
    let name = block
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
    let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());

    json!({
        "type": "function_call",
        "id": format!("fc_{}", uuid::Uuid::now_v7()),
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
        "status": "completed",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_text_only_message_becomes_a_message_output_item() {
        let body = json!({
            "id": "msg_abc",
            "type": "message",
            "role": "assistant",
            "model": "claude-opus",
            "content": [{"type": "text", "text": "hello there"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 20},
        });

        let resp = anthropic_to_responses(&body, "fallback-model");

        assert_eq!(resp["id"], "resp_msg_abc");
        assert_eq!(resp["object"], "response");
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["model"], "claude-opus");
        assert_eq!(resp["output"][0]["type"], "message");
        assert_eq!(resp["output"][0]["content"][0]["text"], "hello there");
        assert_eq!(resp["usage"]["input_tokens"], 10);
        assert_eq!(resp["usage"]["output_tokens"], 20);
        assert_eq!(resp["usage"]["total_tokens"], 30);
    }

    #[test]
    fn a_tool_use_block_becomes_a_function_call_item_with_arguments_reserialized() {
        let body = json!({
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Tokyo"}},
            ],
            "stop_reason": "tool_use",
        });

        let resp = anthropic_to_responses(&body, "m");

        // No text was said, so no message item — only the function call.
        assert_eq!(resp["output"].as_array().unwrap().len(), 1);
        let item = &resp["output"][0];
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], "toolu_1");
        assert_eq!(item["name"], "get_weather");
        // `arguments` must be a re-serialized JSON *string*, not the object.
        assert_eq!(item["arguments"], json!("{\"city\":\"Tokyo\"}"));
        assert!(item["arguments"].is_string());
    }

    #[test]
    fn a_missing_tool_use_id_is_synthesized() {
        let body = json!({
            "content": [{"type": "tool_use", "name": "f", "input": {}}],
            "stop_reason": "tool_use",
        });

        let resp = anthropic_to_responses(&body, "m");

        let call_id = resp["output"][0]["call_id"].as_str().unwrap();
        assert!(call_id.starts_with("call_"), "{call_id}");
    }

    #[test]
    fn text_and_tool_use_together_produce_both_items() {
        let body = json!({
            "content": [
                {"type": "text", "text": "let me check"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {}},
            ],
            "stop_reason": "tool_use",
        });

        let resp = anthropic_to_responses(&body, "m");

        let output = resp["output"].as_array().unwrap();
        assert_eq!(output.len(), 2);
        assert_eq!(output[0]["type"], "message");
        assert_eq!(output[1]["type"], "function_call");
    }

    #[test]
    fn stop_reason_end_turn_and_stop_sequence_are_completed() {
        for reason in ["end_turn", "stop_sequence"] {
            let body = json!({
                "content": [{"type": "text", "text": "hi"}],
                "stop_reason": reason,
            });
            assert_eq!(anthropic_to_responses(&body, "m")["status"], "completed");
        }
    }

    #[test]
    fn stop_reason_max_tokens_becomes_an_incomplete_response() {
        let body = json!({
            "content": [{"type": "text", "text": "cut off"}],
            "stop_reason": "max_tokens",
        });

        let resp = anthropic_to_responses(&body, "m");

        assert_eq!(resp["status"], "incomplete");
        assert_eq!(resp["incomplete_details"]["reason"], "max_output_tokens");
    }

    #[test]
    fn usage_includes_cache_read_tokens_in_input_tokens() {
        let body = json!({
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 7,
                "output_tokens": 20,
                "cache_read_input_tokens": 3,
            },
        });

        let resp = anthropic_to_responses(&body, "m");

        assert_eq!(resp["usage"]["input_tokens"], 10);
        assert_eq!(resp["usage"]["output_tokens"], 20);
        assert_eq!(resp["usage"]["total_tokens"], 30);
        assert_eq!(resp["usage"]["input_tokens_details"]["cached_tokens"], 3);
    }

    #[test]
    fn a_missing_model_falls_back_to_the_proxy_resolved_target() {
        let body = json!({"content": [{"type": "text", "text": "hi"}]});
        assert_eq!(
            anthropic_to_responses(&body, "resolved-model")["model"],
            "resolved-model"
        );
    }

    #[test]
    fn a_completely_empty_body_does_not_panic() {
        let resp = anthropic_to_responses(&json!({}), "fallback");

        assert_eq!(resp["object"], "response");
        assert_eq!(resp["model"], "fallback");
        assert_eq!(resp["status"], "completed");
        assert_eq!(resp["output"], json!([]));
    }
}
