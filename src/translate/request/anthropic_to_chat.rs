//! `anthropic-messages` request → `openai-chat` request.
//!
//! What is deliberately dropped, and why it cannot be carried over:
//!
//! | Anthropic field | why it is dropped |
//! |---|---|
//! | `thinking` / `thinking` content blocks | no `openai-chat` equivalent; a `signature` cannot survive the round trip anyway |
//! | `top_k` | no chat equivalent |
//! | server-side tools (`web_search_20250305`, `bash_20250124`, `text_editor_…`) | executed inside Anthropic's own infrastructure; no `openai-chat` provider can run them |
//! | `cache_control` on system blocks | a prompt-caching hint the target provider has no protocol for |

/// Translate an Anthropic Messages request body into an OpenAI Chat one.
///
/// `model` is copied through even though `proxy.rs` overwrites it with the
/// resolved target's model right after this returns — keeping the mapping
/// makes the function total (every input field that has a home ends up
/// somewhere in the output) and lets tests exercise it without a caller.
pub fn anthropic_to_chat(payload: &serde_json::Value) -> serde_json::Value {
    let mut out = serde_json::Map::new();

    if let Some(model) = payload.get("model").and_then(|m| m.as_str()) {
        out.insert("model".to_string(), serde_json::json!(model));
    }

    let mut messages = Vec::new();
    if let Some(system) = system_message(payload) {
        messages.push(system);
    }
    if let Some(items) = payload.get("messages").and_then(|m| m.as_array()) {
        for message in items {
            messages.extend(translate_message(message));
        }
    }
    out.insert("messages".to_string(), serde_json::Value::Array(messages));

    // Anthropic requires `max_tokens`; chat accepts it under the same name.
    // Deliberately *not* renamed to `max_completion_tokens` — older
    // openai-chat-compatible servers such as Ollama do not know that name.
    if let Some(max_tokens) = payload.get("max_tokens") {
        out.insert("max_tokens".to_string(), max_tokens.clone());
    }
    for key in ["temperature", "top_p", "stream"] {
        if let Some(value) = payload.get(key) {
            out.insert(key.to_string(), value.clone());
        }
    }
    if let Some(stop) = payload.get("stop_sequences") {
        out.insert("stop".to_string(), stop.clone());
    }
    if let Some(user_id) = payload
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|v| v.as_str())
    {
        out.insert("user".to_string(), serde_json::json!(user_id));
    }

    if let Some(tools) = translate_tools(payload) {
        out.insert("tools".to_string(), tools);
    }
    if let Some(tool_choice) = translate_tool_choice(payload) {
        out.insert("tool_choice".to_string(), tool_choice);
    }
    // A sibling of `tool_choice`, not part of the value translated above, so
    // it is read from the untranslated field regardless of whether
    // `tool_choice` itself produced anything.
    if payload
        .get("tool_choice")
        .and_then(|tc| tc.get("disable_parallel_tool_use"))
        .and_then(|v| v.as_bool())
        == Some(true)
    {
        out.insert("parallel_tool_calls".to_string(), serde_json::json!(false));
    }

    // `stream_options` is deliberately never set here: `proxy.rs` injects it
    // per target (only when the target wants usage on a streamed response),
    // and would skip that injection if it found the key already present.

    serde_json::Value::Object(out)
}

/// The leading `role: "system"` message, if `system` is present and
/// non-empty once flattened.
///
/// `system` is either a plain string or an array of blocks; only `type:
/// "text"` blocks contribute (Anthropic defines no other block type for
/// `system` today, but a forward-compatible one would simply have no text to
/// contribute). `cache_control` on a block is a prompt-caching hint with no
/// `openai-chat` equivalent and is ignored outright.
fn system_message(payload: &serde_json::Value) -> Option<serde_json::Value> {
    let text = system_text(payload.get("system")?);
    (!text.is_empty()).then(|| serde_json::json!({"role": "system", "content": text}))
}

/// Flatten a `system` value (string, or an array of text blocks) into plain
/// text. `pub(crate)` because `agent::claude_cli` needs the same rule when it
/// turns a request into a `--system-prompt` argument.
pub(crate) fn system_text(system: &serde_json::Value) -> String {
    match system {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => text_blocks(blocks).join("\n\n"),
        _ => String::new(),
    }
}

fn text_blocks(blocks: &[serde_json::Value]) -> Vec<&str> {
    blocks
        .iter()
        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect()
}

/// Translate one Anthropic message into zero or more chat messages.
///
/// It is more than one when the Anthropic message carries `tool_result`
/// blocks: those become their own `role: "tool"` messages, and OpenAI
/// requires those to directly follow the assistant message that requested
/// the calls — so they are returned *before* whatever remains of the
/// original message's text and image content.
fn translate_message(message: &serde_json::Value) -> Vec<serde_json::Value> {
    let role = message
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("user");

    match message.get("content") {
        Some(serde_json::Value::String(s)) => {
            if s.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::json!({"role": role, "content": s})]
            }
        }
        Some(serde_json::Value::Array(blocks)) => translate_content_blocks(role, blocks),
        _ => Vec::new(),
    }
}

fn translate_content_blocks(role: &str, blocks: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut tool_messages = Vec::new();
    let mut parts = Vec::new();
    let mut tool_calls = Vec::new();

    for block in blocks {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    parts.push(serde_json::json!({"type": "text", "text": text}));
                }
            }
            Some("image") => {
                if let Some(url) = image_url(block) {
                    parts.push(serde_json::json!({"type": "image_url", "image_url": {"url": url}}));
                }
            }
            Some("tool_use") => tool_calls.push(tool_call(block)),
            Some("tool_result") => tool_messages.push(tool_result_message(block)),
            // `thinking` / `redacted_thinking` have no chat equivalent (a
            // `signature` cannot survive the round trip); everything else
            // (`document`, `search_result`, and any future block type) is
            // likewise dropped rather than guessed at.
            _ => {}
        }
    }

    let mut out = tool_messages;

    let has_content = !parts.is_empty();
    let has_tool_calls = !tool_calls.is_empty();
    if has_content || has_tool_calls {
        let mut msg = serde_json::Map::new();
        msg.insert("role".to_string(), serde_json::json!(role));
        if has_content {
            // The single most commonly tested shape: a lone text part comes
            // back as a plain string rather than a one-element array.
            let content = match parts.as_slice() {
                [one] if one.get("type").and_then(|t| t.as_str()) == Some("text") => {
                    one["text"].clone()
                }
                _ => serde_json::Value::Array(parts),
            };
            msg.insert("content".to_string(), content);
        } else {
            // An assistant message that is pure tool calls keeps `content:
            // null` — an empty array makes some providers 400, but `null` is
            // what OpenAI itself emits for this shape.
            msg.insert("content".to_string(), serde_json::Value::Null);
        }
        if has_tool_calls {
            msg.insert(
                "tool_calls".to_string(),
                serde_json::Value::Array(tool_calls),
            );
        }
        out.push(serde_json::Value::Object(msg));
    }

    out
}

/// `{"type":"tool_use",…}` → an entry of an assistant message's `tool_calls`.
///
/// OpenAI carries `arguments` as a JSON-encoded *string*, not an object —
/// the single most common bug in a translation like this one.
fn tool_call(block: &serde_json::Value) -> serde_json::Value {
    let id = block.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let input = block.get("input").cloned().unwrap_or(serde_json::json!({}));
    let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
    serde_json::json!({
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments},
    })
}

/// `{"type":"tool_result",…}` → a standalone `role: "tool"` message.
///
/// Anthropic nests these inside a `user` turn; `openai-chat` wants them as
/// their own message with `tool_call_id` pointing back at the call.
/// `is_error` has no `openai-chat` field to land in and is dropped.
fn tool_result_message(block: &serde_json::Value) -> serde_json::Value {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let content = tool_result_text(block.get("content"));
    serde_json::json!({"role": "tool", "tool_call_id": tool_use_id, "content": content})
}

/// `pub(crate)`, re-exported via `translate::request`, for
/// `server::proxy::classification_content_text` to reuse — the
/// string-or-array-of-blocks handling a `tool_result` block's `content` can
/// take is exactly this, and duplicating it there would drift from this one.
pub(crate) fn tool_result_text(content: Option<&serde_json::Value>) -> String {
    match content {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(blocks)) => text_blocks(blocks).join("\n"),
        _ => String::new(),
    }
}

/// `{"type":"image","source":{…}}` → `{"type":"image_url","image_url":{"url":…}}`.
///
/// Any `source.type` other than `base64`/`url` (or a missing `source`) drops
/// the block — there is nothing recognizable to build a URL from.
fn image_url(block: &serde_json::Value) -> Option<String> {
    let source = block.get("source")?;
    match source.get("type").and_then(|t| t.as_str()) {
        Some("base64") => {
            let media_type = source.get("media_type").and_then(|v| v.as_str())?;
            let data = source.get("data").and_then(|v| v.as_str())?;
            Some(format!("data:{media_type};base64,{data}"))
        }
        Some("url") => source.get("url").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

/// Only client-defined tools (those with an `input_schema`) survive.
/// Anthropic server-side tools (`web_search_20250305`, `bash_20250124`,
/// `text_editor_…`) have no `input_schema` and run inside Anthropic's own
/// infrastructure — no `openai-chat` provider could execute them even if the
/// shape were translated, so they are dropped rather than sent along to
/// confuse the target.
fn translate_tools(payload: &serde_json::Value) -> Option<serde_json::Value> {
    let tools = payload.get("tools")?.as_array()?;
    let translated: Vec<serde_json::Value> = tools
        .iter()
        .filter_map(|tool| {
            let input_schema = tool.get("input_schema")?;
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let mut function = serde_json::Map::new();
            function.insert("name".to_string(), serde_json::json!(name));
            if let Some(description) = tool.get("description").and_then(|v| v.as_str()) {
                function.insert("description".to_string(), serde_json::json!(description));
            }
            function.insert("parameters".to_string(), input_schema.clone());
            Some(serde_json::json!({"type": "function", "function": function}))
        })
        .collect();
    (!translated.is_empty()).then_some(serde_json::Value::Array(translated))
}

/// `tool_choice` shape translation. `disable_parallel_tool_use` is handled
/// separately by the caller since it lands under a different top-level key.
fn translate_tool_choice(payload: &serde_json::Value) -> Option<serde_json::Value> {
    let tool_choice = payload.get("tool_choice")?;
    match tool_choice.get("type").and_then(|t| t.as_str()) {
        Some("auto") => Some(serde_json::json!("auto")),
        Some("any") => Some(serde_json::json!("required")),
        Some("none") => Some(serde_json::json!("none")),
        Some("tool") => {
            let name = tool_choice
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(serde_json::json!({"type": "function", "function": {"name": name}}))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_string_content_is_copied_through() {
        let payload = json!({
            "model": "claude-opus",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hello there"}],
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["model"], "claude-opus");
        assert_eq!(chat["max_tokens"], 1024);
        assert_eq!(
            chat["messages"],
            json!([{"role": "user", "content": "hello there"}])
        );
    }

    #[test]
    fn system_string_becomes_a_leading_system_message() {
        let payload = json!({
            "system": "be terse",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(
            chat["messages"][0],
            json!({"role": "system", "content": "be terse"})
        );
    }

    #[test]
    fn system_block_array_is_joined_and_cache_control_is_ignored() {
        let payload = json!({
            "system": [
                {"type": "text", "text": "part one", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "part two"},
            ],
            "messages": [],
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(
            chat["messages"][0],
            json!({"role": "system", "content": "part one\n\npart two"})
        );
    }

    #[test]
    fn empty_system_emits_no_system_message() {
        let payload = json!({"system": "", "messages": []});
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["messages"], json!([]));
    }

    #[test]
    fn tool_use_round_trips_with_arguments_as_a_json_string() {
        let payload = json!({
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "Tokyo"}},
                ],
            }],
        });
        let chat = anthropic_to_chat(&payload);
        let msg = &chat["messages"][0];
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["content"], json!(null));
        let call = &msg["tool_calls"][0];
        assert_eq!(call["id"], "call_1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "get_weather");
        // `arguments` must be a *string*, not an object — the classic bug.
        assert_eq!(call["function"]["arguments"], json!("{\"city\":\"Tokyo\"}"));
    }

    #[test]
    fn tool_result_becomes_a_tool_message_before_remaining_user_content() {
        let payload = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "72F and sunny"},
                    {"type": "text", "text": "what should I wear?"},
                ],
            }],
        });
        let chat = anthropic_to_chat(&payload);
        let messages = chat["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "72F and sunny"})
        );
        assert_eq!(
            messages[1],
            json!({"role": "user", "content": "what should I wear?"})
        );
    }

    #[test]
    fn tool_result_content_blocks_are_flattened_with_newlines() {
        let payload = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": [
                        {"type": "text", "text": "line one"},
                        {"type": "text", "text": "line two"},
                    ],
                }],
            }],
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["messages"][0]["content"], "line one\nline two");
    }

    #[test]
    fn tool_result_missing_content_becomes_an_empty_string() {
        let payload = json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call_1"}],
            }],
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["messages"][0]["content"], "");
    }

    #[test]
    fn base64_image_becomes_a_data_url() {
        let payload = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this?"},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": "abc123",
                    }},
                ],
            }],
        });
        let chat = anthropic_to_chat(&payload);
        let content = chat["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content[0], json!({"type": "text", "text": "what is this?"}));
        assert_eq!(
            content[1],
            json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,abc123"}})
        );
    }

    #[test]
    fn url_image_is_passed_through_verbatim() {
        let payload = json!({
            "messages": [{
                "role": "user",
                "content": [{"type": "image", "source": {"type": "url", "url": "https://example/x.png"}}],
            }],
        });
        let chat = anthropic_to_chat(&payload);
        // A single image block is the only part, so it still comes back as a
        // one-element array (the string-collapse rule only applies to a lone
        // text part).
        assert_eq!(
            chat["messages"][0]["content"],
            json!([{"type": "image_url", "image_url": {"url": "https://example/x.png"}}])
        );
    }

    #[test]
    fn unrecognized_image_source_drops_the_block() {
        let payload = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "kept"},
                    {"type": "image", "source": {"type": "file", "file_id": "f1"}},
                ],
            }],
        });
        let chat = anthropic_to_chat(&payload);
        // Only the text part survives, so it collapses to a plain string.
        assert_eq!(chat["messages"][0]["content"], "kept");
    }

    #[test]
    fn a_lone_text_part_collapses_to_a_plain_string() {
        let payload = json!({
            "messages": [{"role": "user", "content": [{"type": "text", "text": "solo"}]}],
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["messages"][0]["content"], "solo");
    }

    #[test]
    fn server_side_tools_and_thinking_blocks_are_dropped() {
        let payload = json!({
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "let me consider...", "signature": "sig"},
                    {"type": "redacted_thinking", "data": "opaque"},
                    {"type": "text", "text": "the answer is 4"},
                ],
            }],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "add", "description": "adds numbers", "input_schema": {"type": "object"}},
            ],
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["messages"][0]["content"], "the answer is 4");
        let tools = chat["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "add");
    }

    #[test]
    fn client_tool_is_translated_to_a_function_definition() {
        let payload = json!({
            "tools": [{
                "name": "get_weather",
                "description": "look up weather",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
            }],
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(
            chat["tools"][0],
            json!({
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "look up weather",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                },
            })
        );
    }

    #[test]
    fn tools_key_is_omitted_when_nothing_survives() {
        let payload = json!({
            "tools": [{"type": "bash_20250124", "name": "bash"}],
            "messages": [],
        });
        let chat = anthropic_to_chat(&payload);
        assert!(chat.get("tools").is_none());
    }

    #[test]
    fn tool_choice_variants_translate() {
        let cases = [
            (json!({"type": "auto"}), json!("auto")),
            (json!({"type": "any"}), json!("required")),
            (json!({"type": "none"}), json!("none")),
            (
                json!({"type": "tool", "name": "get_weather"}),
                json!({"type": "function", "function": {"name": "get_weather"}}),
            ),
        ];
        for (input, expected) in cases {
            let payload = json!({"messages": [], "tool_choice": input});
            let chat = anthropic_to_chat(&payload);
            assert_eq!(chat["tool_choice"], expected);
        }
    }

    #[test]
    fn disable_parallel_tool_use_becomes_a_top_level_flag() {
        let payload = json!({
            "messages": [],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["parallel_tool_calls"], false);
    }

    #[test]
    fn scalar_fields_are_copied_and_renamed() {
        let payload = json!({
            "messages": [],
            "temperature": 0.5,
            "top_p": 0.9,
            "stream": true,
            "stop_sequences": ["STOP"],
            "top_k": 40,
            "metadata": {"user_id": "user-123"},
            "thinking": {"type": "enabled", "budget_tokens": 1024},
        });
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["temperature"], 0.5);
        assert_eq!(chat["top_p"], 0.9);
        assert_eq!(chat["stream"], true);
        assert_eq!(chat["stop"], json!(["STOP"]));
        assert_eq!(chat["user"], "user-123");
        assert!(chat.get("top_k").is_none());
        assert!(chat.get("thinking").is_none());
        assert!(chat.get("metadata").is_none());
    }

    #[test]
    fn stream_options_is_never_set_here() {
        // `proxy.rs` injects it per target; if this function set it too,
        // the injection would be skipped even for a target that needs it.
        let payload = json!({"messages": [], "stream": true});
        let chat = anthropic_to_chat(&payload);
        assert!(chat.get("stream_options").is_none());
    }

    #[test]
    fn empty_payload_produces_a_thin_valid_request_without_panicking() {
        let payload = json!({});
        let chat = anthropic_to_chat(&payload);
        assert!(chat.get("model").is_none());
        assert_eq!(chat["messages"], json!([]));
    }

    #[test]
    fn garbage_shaped_payload_does_not_panic() {
        // A string where an object was expected: every accessor must
        // tolerate this rather than panic on a type mismatch.
        let payload = json!("just a string, not an object at all");
        let chat = anthropic_to_chat(&payload);
        assert_eq!(chat["messages"], json!([]));
    }

    #[test]
    fn malformed_message_shapes_are_dropped_not_panicked_on() {
        let payload = json!({
            "messages": [
                {"role": "user"},
                {"role": "user", "content": null},
                {"role": "user", "content": []},
                {"content": "no role at all"},
            ],
        });
        let chat = anthropic_to_chat(&payload);
        let messages = chat["messages"].as_array().unwrap();
        // The first three contribute nothing; the last defaults to `user`.
        assert_eq!(messages.len(), 1);
        assert_eq!(
            messages[0],
            json!({"role": "user", "content": "no role at all"})
        );
    }
}
