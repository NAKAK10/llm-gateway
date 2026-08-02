//! `openai-responses` request → `openai-chat` request.
//!
//! What is deliberately dropped, and why:
//!
//! | Responses field | why it is dropped |
//! |---|---|
//! | `reasoning` | Codex's own extended-thinking config; no `openai-chat` provider this gateway reaches implements it |
//! | `include` | selects extra fields (`reasoning.encrypted_content`, …) on a Responses-shaped body; meaningless once translated |
//! | `store` / `previous_response_id` | server-side conversation state Responses offers and `openai-chat` has no concept of |
//! | `prompt_cache_key` / `client_metadata` | Responses-specific caching/telemetry hints with no chat equivalent |
//! | `text` | Responses' structured-output/verbosity config; `openai-chat` has its own, incompatible, mechanism for this |
//! | `metadata` | opaque client bookkeeping the target provider has no field for |
//! | non-`function` tools (`local_shell`, `web_search`, a `namespace` grouping, …) | Codex's own extensions, executed by Codex itself or by OpenAI's own infrastructure; no `openai-chat` provider can run them |

/// Translate an OpenAI Responses request body into an OpenAI Chat one.
///
/// See the module docs for the table of fields this drops and why.
/// `stream_options` is deliberately never set here, for the same reason
/// [`super::anthropic_to_chat::anthropic_to_chat`] never sets it: `proxy.rs`
/// injects it per target, and would skip that injection if it found the key
/// already present.
pub fn responses_to_chat(payload: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();

    if let Some(model) = payload.get("model").and_then(|m| m.as_str()) {
        out.insert("model".to_string(), serde_json::json!(model));
    }

    let mut messages = Vec::new();
    if let Some(instructions) = payload.get("instructions").and_then(|v| v.as_str()) {
        if !instructions.is_empty() {
            messages.push(serde_json::json!({"role": "system", "content": instructions}));
        }
    }
    match payload.get("input") {
        Some(serde_json::Value::String(s)) => {
            if !s.is_empty() {
                messages.push(serde_json::json!({"role": "user", "content": s}));
            }
        }
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                messages.extend(translate_input_item(item));
            }
        }
        _ => {}
    }
    out.insert("messages".to_string(), serde_json::Value::Array(messages));

    if let Some(max_tokens) = payload.get("max_output_tokens") {
        out.insert("max_tokens".to_string(), max_tokens.clone());
    }
    for key in ["temperature", "top_p", "stream"] {
        if let Some(value) = payload.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    if let Some(parallel) = payload.get("parallel_tool_calls") {
        out.insert("parallel_tool_calls".to_string(), parallel.clone());
    }

    if let Some(tools) = translate_responses_tools(payload) {
        out.insert("tools".to_string(), tools);
    }
    if let Some(tool_choice) = translate_responses_tool_choice(payload) {
        out.insert("tool_choice".to_string(), tool_choice);
    }

    serde_json::Value::Object(out)
}

/// Translate one Responses `input[]` item into zero or more chat messages.
///
/// A `message` item (or one that names no `type` at all — some clients omit
/// it) becomes zero or one chat message; `function_call` and
/// `function_call_output` each become exactly one. `reasoning` and any other
/// unrecognized item type carry nothing a chat message can represent and are
/// dropped rather than guessed at.
fn translate_input_item(item: &serde_json::Value) -> Vec<serde_json::Value> {
    match item.get("type").and_then(|t| t.as_str()) {
        Some("message") | None => translate_input_message(item).into_iter().collect(),
        // Kept as one assistant message per call rather than merging
        // consecutive `function_call` items into a single message with
        // several `tool_calls` — simpler, and every `openai-chat`-compatible
        // server this gateway targets tolerates several consecutive
        // assistant messages just as well as one with several tool calls.
        Some("function_call") => vec![function_call_message(item)],
        Some("function_call_output") => vec![function_call_output_message(item)],
        _ => Vec::new(),
    }
}

/// `{"type":"message","role":…,"content":…}` → a chat message, or `None` when
/// there is no content to carry over. `developer` maps to `system` — chat has
/// no third role for it.
fn translate_input_message(item: &serde_json::Value) -> Option<serde_json::Value> {
    let role = match item.get("role").and_then(|r| r.as_str()).unwrap_or("user") {
        "developer" => "system",
        other => other,
    };
    match item.get("content") {
        Some(serde_json::Value::String(s)) => {
            (!s.is_empty()).then(|| serde_json::json!({"role": role, "content": s}))
        }
        Some(serde_json::Value::Array(parts)) => translate_input_parts(parts)
            .map(|content| serde_json::json!({"role": role, "content": content})),
        _ => None,
    }
}

/// Flatten a `message` item's content parts: every `input_text` /
/// `output_text` block is concatenated into one string (mirroring how
/// [`super::super::response::chat_to_anthropic`]'s reverse direction flattens
/// a chat response's array-valued content), and every `input_image` becomes
/// its own `image_url` part. `None` when nothing recognizable survives.
fn translate_input_parts(parts: &[serde_json::Value]) -> Option<serde_json::Value> {
    let mut text = String::new();
    let mut images = Vec::new();
    for part in parts {
        match part.get("type").and_then(|t| t.as_str()) {
            Some("input_text") | Some("output_text") => {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
            Some("input_image") => {
                if let Some(url) = part.get("image_url").and_then(|v| v.as_str()) {
                    images
                        .push(serde_json::json!({"type": "image_url", "image_url": {"url": url}}));
                }
            }
            _ => {}
        }
    }

    if images.is_empty() {
        return (!text.is_empty()).then(|| serde_json::json!(text));
    }
    let mut out_parts = Vec::new();
    if !text.is_empty() {
        out_parts.push(serde_json::json!({"type": "text", "text": text}));
    }
    out_parts.extend(images);
    Some(serde_json::Value::Array(out_parts))
}

/// `{"type":"function_call","call_id","name","arguments"}` → an assistant
/// message whose sole `tool_calls[]` entry is this call. `arguments` is
/// already a JSON *string* on the Responses wire, same as `openai-chat`
/// wants it — no parse/re-serialize round trip needed, unlike the Anthropic
/// direction where `input` is a real object.
fn function_call_message(item: &serde_json::Value) -> serde_json::Value {
    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
    let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = item
        .get("arguments")
        .and_then(|v| v.as_str())
        .unwrap_or("{}");
    serde_json::json!({
        "role": "assistant",
        "content": serde_json::Value::Null,
        "tool_calls": [{
            "id": call_id,
            "type": "function",
            "function": {"name": name, "arguments": arguments},
        }],
    })
}

/// `{"type":"function_call_output","call_id","output"}` → a standalone
/// `role: "tool"` message. `output` is usually a string; anything else is
/// JSON-encoded rather than dropped, since a tool's structured result is
/// still meaningful to the model as text.
fn function_call_output_message(item: &serde_json::Value) -> serde_json::Value {
    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
    let content = match item.get("output") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    serde_json::json!({"role": "tool", "tool_call_id": call_id, "content": content})
}

/// Only flat `{"type":"function",…}` tool definitions survive. Codex's own
/// extensions — `local_shell`, `web_search`, a `namespace` grouping of
/// several function tools — have no `openai-chat` equivalent and are dropped
/// rather than guessed at. `strict` has no `openai-chat` field to land in and
/// is dropped too.
fn translate_responses_tools(payload: &serde_json::Value) -> Option<serde_json::Value> {
    let tools = payload.get("tools")?.as_array()?;
    let translated: Vec<serde_json::Value> = tools
        .iter()
        .filter(|tool| tool.get("type").and_then(|t| t.as_str()) == Some("function"))
        .map(|tool| {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let mut function = serde_json::Map::new();
            function.insert("name".to_string(), serde_json::json!(name));
            if let Some(description) = tool.get("description").and_then(|v| v.as_str()) {
                function.insert("description".to_string(), serde_json::json!(description));
            }
            if let Some(parameters) = tool.get("parameters") {
                function.insert("parameters".to_string(), parameters.clone());
            }
            serde_json::json!({"type": "function", "function": function})
        })
        .collect();
    (!translated.is_empty()).then_some(serde_json::Value::Array(translated))
}

/// `tool_choice` shape translation: Responses' flat form maps directly to
/// chat's, differing only for a named function.
fn translate_responses_tool_choice(payload: &serde_json::Value) -> Option<serde_json::Value> {
    match payload.get("tool_choice")? {
        serde_json::Value::String(s) if matches!(s.as_str(), "auto" | "none" | "required") => {
            Some(serde_json::json!(s))
        }
        tool_choice @ serde_json::Value::Object(_)
            if tool_choice.get("type").and_then(|t| t.as_str()) == Some("function") =>
        {
            let name = tool_choice
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(serde_json::json!({"type": "function", "function": {"name": name}}))
        }
        _ => None,
    }
}

/// The text content of one message: the string content verbatim, or every
/// `type: "text"` block joined. Non-text blocks (images, tool calls, tool
/// results) are not counted here — they either have no text of their own or
/// are already reflected in `tools`.
///
/// `pub(crate)` for the same reason as
/// [`super::anthropic_to_chat::system_text`]: flattening a request into a
/// single prompt uses the same rule as estimating its size.
pub(crate) fn message_text(message: &serde_json::Value) -> String {
    match message.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn responses_instructions_become_a_leading_system_message() {
        let payload = json!({
            "instructions": "be terse",
            "input": "hi",
        });
        let chat = responses_to_chat(&payload);
        assert_eq!(
            chat["messages"][0],
            json!({"role": "system", "content": "be terse"})
        );
        assert_eq!(
            chat["messages"][1],
            json!({"role": "user", "content": "hi"})
        );
    }

    #[test]
    fn responses_string_input_becomes_a_single_user_message() {
        let payload = json!({"input": "ping"});
        let chat = responses_to_chat(&payload);
        assert_eq!(
            chat["messages"],
            json!([{"role": "user", "content": "ping"}])
        );
    }

    #[test]
    fn a_message_item_with_input_text_parts_collapses_to_a_plain_string() {
        let payload = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hello"}],
            }],
        });
        let chat = responses_to_chat(&payload);
        assert_eq!(
            chat["messages"][0],
            json!({"role": "user", "content": "hello"})
        );
    }

    #[test]
    fn developer_role_maps_to_system() {
        let payload = json!({
            "input": [{
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "be terse"}],
            }],
        });
        let chat = responses_to_chat(&payload);
        assert_eq!(chat["messages"][0]["role"], "system");
    }

    #[test]
    fn a_message_item_with_no_type_is_still_translated() {
        let payload = json!({
            "input": [{"role": "user", "content": "no type field at all"}],
        });
        let chat = responses_to_chat(&payload);
        assert_eq!(
            chat["messages"][0],
            json!({"role": "user", "content": "no type field at all"})
        );
    }

    #[test]
    fn multiple_text_parts_are_concatenated_not_kept_separate() {
        let payload = json!({
            "input": [{
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "hello "},
                    {"type": "output_text", "text": "world"},
                ],
            }],
        });
        let chat = responses_to_chat(&payload);
        assert_eq!(chat["messages"][0]["content"], "hello world");
    }

    #[test]
    fn an_input_image_part_becomes_an_image_url_part() {
        let payload = json!({
            "input": [{
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "what is this?"},
                    {"type": "input_image", "image_url": "https://example/x.png"},
                ],
            }],
        });
        let chat = responses_to_chat(&payload);
        let content = chat["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0], json!({"type": "text", "text": "what is this?"}));
        assert_eq!(
            content[1],
            json!({"type": "image_url", "image_url": {"url": "https://example/x.png"}})
        );
    }

    #[test]
    fn function_call_and_function_call_output_round_trip() {
        let payload = json!({
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"Tokyo\"}",
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "72F and sunny",
                },
            ],
        });
        let chat = responses_to_chat(&payload);
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);

        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"], json!(null));
        let call = &messages[0]["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "get_weather");
        // Already a JSON string on the wire — no parse/re-serialize round trip.
        assert_eq!(call["function"]["arguments"], "{\"city\":\"Tokyo\"}");

        assert_eq!(
            messages[1],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "72F and sunny"})
        );
    }

    #[test]
    fn a_non_string_function_call_output_is_json_encoded() {
        let payload = json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "call_1",
                "output": {"temp_f": 72, "condition": "sunny"},
            }],
        });
        let chat = responses_to_chat(&payload);
        let content = chat["messages"][0]["content"].as_str().unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(reparsed, json!({"temp_f": 72, "condition": "sunny"}));
    }

    #[test]
    fn a_reasoning_item_and_other_unknown_items_are_dropped_without_panicking() {
        let payload = json!({
            "input": [
                {"type": "reasoning", "id": "rs_1", "summary": []},
                {"type": "something_from_the_future", "data": "opaque"},
                {"type": "message", "role": "user", "content": "kept"},
            ],
        });
        let chat = responses_to_chat(&payload);
        assert_eq!(
            chat["messages"],
            json!([{"role": "user", "content": "kept"}])
        );
    }

    #[test]
    fn namespace_and_web_search_tools_are_dropped_but_function_tools_survive() {
        let payload = json!({
            "tools": [
                {
                    "type": "namespace",
                    "name": "multi_agent_v1",
                    "tools": [{"type": "function", "name": "spawn_agent"}],
                },
                {"type": "web_search"},
                {
                    "type": "function",
                    "name": "get_weather",
                    "description": "look up weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                    "strict": false,
                },
            ],
        });
        let chat = responses_to_chat(&payload);
        let tools = chat["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "get_weather");
        assert_eq!(
            tools[0]["function"]["parameters"],
            json!({"type": "object", "properties": {"city": {"type": "string"}}})
        );
        assert!(tools[0]["function"].get("strict").is_none());
    }

    #[test]
    fn tools_key_is_omitted_when_nothing_survives_translation() {
        let payload = json!({"tools": [{"type": "web_search"}], "input": "hi"});
        let chat = responses_to_chat(&payload);
        assert!(chat.get("tools").is_none());
    }

    #[test]
    fn responses_tool_choice_variants_translate() {
        let cases = [
            (json!("auto"), json!("auto")),
            (json!("none"), json!("none")),
            (json!("required"), json!("required")),
            (
                json!({"type": "function", "name": "get_weather"}),
                json!({"type": "function", "function": {"name": "get_weather"}}),
            ),
        ];
        for (input, expected) in cases {
            let payload = json!({"input": "hi", "tool_choice": input});
            let chat = responses_to_chat(&payload);
            assert_eq!(chat["tool_choice"], expected);
        }
    }

    #[test]
    fn responses_scalar_fields_are_copied_and_renamed() {
        let payload = json!({
            "input": "hi",
            "max_output_tokens": 512,
            "temperature": 0.5,
            "top_p": 0.9,
            "stream": true,
            "parallel_tool_calls": false,
            "reasoning": {"effort": "high"},
            "include": ["reasoning.encrypted_content"],
            "store": false,
            "prompt_cache_key": "abc",
            "client_metadata": {"foo": "bar"},
            "previous_response_id": null,
            "text": {"format": {"type": "text"}},
            "metadata": {"k": "v"},
        });
        let chat = responses_to_chat(&payload);
        assert_eq!(chat["max_tokens"], 512);
        assert_eq!(chat["temperature"], 0.5);
        assert_eq!(chat["top_p"], 0.9);
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["parallel_tool_calls"], false);
        for key in [
            "reasoning",
            "include",
            "store",
            "prompt_cache_key",
            "client_metadata",
            "previous_response_id",
            "text",
            "metadata",
            "instructions",
        ] {
            assert!(chat.get(key).is_none(), "{key} leaked into the chat body");
        }
    }

    #[test]
    fn responses_stream_options_is_never_set_here() {
        let payload = json!({"input": "hi", "stream": true});
        let chat = responses_to_chat(&payload);
        assert!(chat.get("stream_options").is_none());
    }

    #[test]
    fn empty_responses_payload_produces_a_thin_valid_request_without_panicking() {
        let chat = responses_to_chat(&json!({}));
        assert!(chat.get("model").is_none());
        assert_eq!(chat["messages"], json!([]));
    }

    #[test]
    fn garbage_shaped_responses_payload_does_not_panic() {
        let chat = responses_to_chat(&json!("just a string, not an object at all"));
        assert_eq!(chat["messages"], json!([]));
    }

    #[test]
    fn malformed_responses_input_items_are_dropped_not_panicked_on() {
        let payload = json!({
            "input": [
                {"type": "message", "role": "user"},
                {"type": "message", "role": "user", "content": null},
                {"type": "message", "role": "user", "content": []},
                {"type": "function_call_output", "call_id": "c1"},
            ],
        });
        let chat = responses_to_chat(&payload);
        let messages = chat["messages"].as_array().unwrap();
        // The first three contribute nothing (no usable content); the
        // `function_call_output` with no `output` field still produces a
        // tool message with empty content rather than being dropped.
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            json!({"role": "tool", "tool_call_id": "c1", "content": ""})
        );
    }
}
