//! `openai-responses` request → `anthropic-messages` request.
//!
//! What is deliberately dropped, and why (mirroring
//! [`super::responses_to_chat`]'s own table, since both face the same
//! Responses-shaped `input[]`):
//!
//! | Responses field | why it is dropped |
//! |---|---|
//! | `reasoning` | Codex's own extended-thinking config; no `anthropic-messages` equivalent this gateway can produce |
//! | `include` | selects extra fields (`reasoning.encrypted_content`, …) on a Responses-shaped body; meaningless once translated |
//! | `store` / `previous_response_id` | server-side conversation state Responses offers and Anthropic has no concept of |
//! | `prompt_cache_key` / `client_metadata` | Responses-specific caching/telemetry hints with no Anthropic equivalent |
//! | `text` | Responses' structured-output/verbosity config; Anthropic has no equivalent mechanism |
//! | `metadata` | opaque client bookkeeping the target provider has no field for |
//! | non-`function` tools (`local_shell`, `web_search`, a `namespace` grouping, …) | Codex's own extensions, executed by Codex itself or by OpenAI's own infrastructure; no `anthropic-messages` provider can run them |
//! | `input` items other than `message`/`function_call`/`function_call_output` (`reasoning`, …) | carry nothing an Anthropic message can represent |

use serde_json::{json, Value};

/// Anthropic requires `max_tokens` on every request; Responses has no direct
/// equivalent (`max_output_tokens` is read below when present, so this only
/// applies when even that is absent). Picked to be generous enough that a
/// real answer is never truncated by the gateway's own default rather than
/// the model's judgement.
const DEFAULT_MAX_TOKENS: u64 = 4096;

/// Translate an OpenAI Responses request body into an Anthropic Messages one.
///
/// See the module docs for the table of fields this drops and why. Unlike
/// [`super::anthropic_to_chat::anthropic_to_chat`]'s tool-call mapping, no
/// double string-encode/decode round trip is needed for `function_call`:
/// Responses' `arguments` is a JSON *string* and Anthropic's `tool_use.input`
/// is a real object, so exactly one parse happens, the same shape of parse
/// [`response::chat_to_anthropic`]'s `tool_use_block` already does from
/// chat's `tool_calls[].function.arguments`.
pub fn responses_to_anthropic(payload: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();

    if let Some(model) = payload.get("model").and_then(|m| m.as_str()) {
        out.insert("model".to_string(), json!(model));
    }

    // `instructions` is always plain text on the Responses wire, unlike
    // Anthropic's `system`, which may also be a block array — no flattening
    // needed going this direction.
    if let Some(instructions) = payload.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.is_empty() {
            out.insert("system".to_string(), json!(instructions));
        }
    }

    let mut messages = Vec::new();
    match payload.get("input") {
        Some(Value::String(s)) => {
            if !s.is_empty() {
                messages.push(json!({"role": "user", "content": s}));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                messages.extend(translate_input_item(item));
            }
        }
        _ => {}
    }
    out.insert("messages".to_string(), Value::Array(messages));

    let max_tokens = payload
        .get("max_output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_TOKENS);
    out.insert("max_tokens".to_string(), json!(max_tokens));

    for key in ["temperature", "top_p", "stream"] {
        if let Some(value) = payload.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }

    if let Some(tools) = translate_tools(payload) {
        out.insert("tools".to_string(), tools);
    }
    if let Some(tool_choice) = translate_tool_choice(payload) {
        out.insert("tool_choice".to_string(), tool_choice);
    }

    Value::Object(out)
}

/// Translate one Responses `input[]` item into zero or more Anthropic
/// messages. A `message` item becomes zero or one message; `function_call`
/// and `function_call_output` each become exactly one. Anything else (Codex's
/// own extensions, `reasoning`, …) is dropped rather than guessed at.
fn translate_input_item(item: &Value) -> Vec<Value> {
    match item.get("type").and_then(|t| t.as_str()) {
        Some("message") | None => translate_input_message(item).into_iter().collect(),
        Some("function_call") => vec![function_call_message(item)],
        Some("function_call_output") => vec![function_call_output_message(item)],
        _ => Vec::new(),
    }
}

/// `{"type":"message","role":…,"content":…}` → an Anthropic message, or
/// `None` when there is no text to carry over. `developer` maps to `user`,
/// same fallback Anthropic itself has no third role for.
fn translate_input_message(item: &Value) -> Option<Value> {
    let role = match item.get("role").and_then(|r| r.as_str()).unwrap_or("user") {
        "assistant" => "assistant",
        _ => "user",
    };
    let text = match item.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => input_text(parts),
        _ => String::new(),
    };
    (!text.is_empty()).then(|| json!({"role": role, "content": [{"type": "text", "text": text}]}))
}

/// Flatten a `message` item's content parts: every `input_text` /
/// `output_text` block is concatenated into one string. Other part types
/// (`input_image`, …) have no Anthropic block built here — dropped rather
/// than guessed at, matching `request::responses_to_chat`'s treatment of the
/// same input shape for images it does not carry over either.
fn input_text(parts: &[Value]) -> String {
    let mut text = String::new();
    for part in parts {
        if matches!(
            part.get("type").and_then(|t| t.as_str()),
            Some("input_text") | Some("output_text")
        ) {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                text.push_str(t);
            }
        }
    }
    text
}

/// `{"type":"function_call","call_id","name","arguments"}` → an assistant
/// message whose sole content block is this `tool_use`. `arguments` is a
/// JSON *string* on the Responses wire and must be parsed into an object —
/// Anthropic's `tool_use.input` is a real object, not a string. Falls back to
/// `{}` on a missing or unparseable value rather than failing the whole
/// translation.
fn function_call_message(item: &Value) -> Value {
    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let input = item
        .get("arguments")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}));
    json!({
        "role": "assistant",
        "content": [{"type": "tool_use", "id": call_id, "name": name, "input": input}],
    })
}

/// `{"type":"function_call_output","call_id","output"}` → a user message
/// whose sole content block is a `tool_result`. `output` is usually a
/// string; anything else is JSON-encoded rather than dropped, since a tool's
/// structured result is still meaningful to the model as text.
fn function_call_output_message(item: &Value) -> Value {
    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
    let content = match item.get("output") {
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    json!({
        "role": "user",
        "content": [{"type": "tool_result", "tool_use_id": call_id, "content": content}],
    })
}

/// Only flat `{"type":"function",…}` tool definitions survive. Codex's own
/// extensions (`local_shell`, `web_search`, a `namespace` grouping) have no
/// `anthropic-messages` equivalent and are dropped rather than guessed at.
fn translate_tools(payload: &Value) -> Option<Value> {
    let tools = payload.get("tools")?.as_array()?;
    let translated: Vec<Value> = tools
        .iter()
        .filter(|tool| tool.get("type").and_then(|t| t.as_str()) == Some("function"))
        .map(|tool| {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let mut out = serde_json::Map::new();
            out.insert("name".to_string(), json!(name));
            if let Some(description) = tool.get("description").and_then(|v| v.as_str()) {
                out.insert("description".to_string(), json!(description));
            }
            out.insert(
                "input_schema".to_string(),
                tool.get("parameters").cloned().unwrap_or(json!({})),
            );
            Value::Object(out)
        })
        .collect();
    (!translated.is_empty()).then_some(Value::Array(translated))
}

/// `tool_choice` shape translation: Responses' flat form maps to Anthropic's
/// tagged-object form.
fn translate_tool_choice(payload: &Value) -> Option<Value> {
    match payload.get("tool_choice")? {
        Value::String(s) if s == "auto" => Some(json!({"type": "auto"})),
        Value::String(s) if s == "none" => Some(json!({"type": "none"})),
        Value::String(s) if s == "required" => Some(json!({"type": "any"})),
        tool_choice @ Value::Object(_)
            if tool_choice.get("type").and_then(|t| t.as_str()) == Some("function") =>
        {
            let name = tool_choice
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(json!({"type": "tool", "name": name}))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn instructions_become_the_system_field() {
        let payload = json!({"instructions": "be terse", "input": "hi"});
        let anthropic = responses_to_anthropic(&payload);
        assert_eq!(anthropic["system"], "be terse");
    }

    #[test]
    fn empty_instructions_emit_no_system_field() {
        let payload = json!({"instructions": "", "input": "hi"});
        let anthropic = responses_to_anthropic(&payload);
        assert!(anthropic.get("system").is_none());
    }

    #[test]
    fn plain_string_input_becomes_a_single_user_message() {
        let payload = json!({"input": "ping"});
        let anthropic = responses_to_anthropic(&payload);
        assert_eq!(
            anthropic["messages"],
            json!([{"role": "user", "content": "ping"}])
        );
    }

    #[test]
    fn a_text_only_message_array_becomes_anthropic_text_blocks() {
        let payload = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}],
            }],
        });
        let anthropic = responses_to_anthropic(&payload);
        assert_eq!(
            anthropic["messages"][0],
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]})
        );
    }

    #[test]
    fn function_call_round_trips_with_arguments_as_a_parsed_object() {
        let payload = json!({
            "input": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "get_weather",
                "arguments": "{\"city\":\"Tokyo\"}",
            }],
        });
        let anthropic = responses_to_anthropic(&payload);
        let msg = &anthropic["messages"][0];
        assert_eq!(msg["role"], "assistant");
        let block = &msg["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call_1");
        assert_eq!(block["name"], "get_weather");
        // Must be a parsed OBJECT, not the original string.
        assert_eq!(block["input"], json!({"city": "Tokyo"}));
        assert!(block["input"].is_object());
    }

    #[test]
    fn function_call_output_becomes_a_tool_result_block() {
        let payload = json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "72F and sunny",
            }],
        });
        let anthropic = responses_to_anthropic(&payload);
        let msg = &anthropic["messages"][0];
        assert_eq!(msg["role"], "user");
        assert_eq!(
            msg["content"][0],
            json!({"type": "tool_result", "tool_use_id": "call_1", "content": "72F and sunny"})
        );
    }

    #[test]
    fn tools_and_tool_choice_translate_to_anthropic_shape() {
        let payload = json!({
            "input": "hi",
            "tools": [{
                "type": "function",
                "name": "get_weather",
                "description": "look up weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
            }],
            "tool_choice": {"type": "function", "name": "get_weather"},
        });
        let anthropic = responses_to_anthropic(&payload);
        assert_eq!(
            anthropic["tools"][0],
            json!({
                "name": "get_weather",
                "description": "look up weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
            })
        );
        assert_eq!(
            anthropic["tool_choice"],
            json!({"type": "tool", "name": "get_weather"})
        );
    }

    #[test]
    fn tool_choice_variants_translate() {
        let cases = [
            (json!("auto"), json!({"type": "auto"})),
            (json!("none"), json!({"type": "none"})),
            (json!("required"), json!({"type": "any"})),
        ];
        for (input, expected) in cases {
            let payload = json!({"input": "hi", "tool_choice": input});
            let anthropic = responses_to_anthropic(&payload);
            assert_eq!(anthropic["tool_choice"], expected);
        }
    }

    #[test]
    fn codex_specific_fields_are_dropped() {
        let payload = json!({
            "input": "hi",
            "previous_response_id": "resp_abc",
            "store": true,
            "reasoning": {"effort": "high"},
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": "abc",
            "client_metadata": {"foo": "bar"},
            "text": {"format": {"type": "text"}},
            "metadata": {"k": "v"},
        });
        let anthropic = responses_to_anthropic(&payload);
        for key in [
            "previous_response_id",
            "store",
            "reasoning",
            "include",
            "prompt_cache_key",
            "client_metadata",
            "text",
            "metadata",
        ] {
            assert!(
                anthropic.get(key).is_none(),
                "{key} leaked into the anthropic body"
            );
        }
    }

    #[test]
    fn missing_max_output_tokens_gets_the_default() {
        let payload = json!({"input": "hi"});
        let anthropic = responses_to_anthropic(&payload);
        assert_eq!(anthropic["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn max_output_tokens_is_carried_over_as_max_tokens() {
        let payload = json!({"input": "hi", "max_output_tokens": 512});
        let anthropic = responses_to_anthropic(&payload);
        assert_eq!(anthropic["max_tokens"], 512);
    }

    #[test]
    fn empty_payload_produces_a_thin_valid_request_without_panicking() {
        let anthropic = responses_to_anthropic(&json!({}));
        assert_eq!(anthropic["messages"], json!([]));
        assert_eq!(anthropic["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn garbage_shaped_payload_does_not_panic() {
        let anthropic = responses_to_anthropic(&json!("just a string"));
        assert_eq!(anthropic["messages"], json!([]));
    }
}
