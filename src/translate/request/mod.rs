//! Client requests → `openai-chat` requests.
//!
//! Two directions live here, [`anthropic_to_chat`] and [`responses_to_chat`],
//! because both face the same problem from a different client shape: where
//! the system prompt lives, whether tool call arguments are a string or an
//! object, which message role carries a tool's result, and what a "list of
//! content blocks" even means. Both build a **fresh** `openai-chat` object
//! field by field rather than patching the client's own body in place,
//! because passing an unrecognized key through (`cache_control`, `thinking`,
//! `top_k`, `reasoning`, `store`, …) makes strict `openai-chat` servers answer
//! with a `400`, which is worse than silently dropping something the target
//! protocol has no room for.
//!
//! See [`anthropic_to_chat`] and [`responses_to_chat`]'s own module docs for
//! the table of fields each direction drops and why.
//!
//! Translation is part of the crate-wide **infallible** contract (see
//! `translate` module docs): every accessor here tolerates a missing or
//! wrongly-shaped field and produces a thin-but-valid result rather than
//! panicking. The request has already been accepted by the client; a
//! translation bug must not be able to turn it into a `500`.

mod anthropic_to_chat;
mod responses_to_anthropic;
mod responses_to_chat;

pub use anthropic_to_chat::anthropic_to_chat;
pub(crate) use anthropic_to_chat::system_text;
pub use responses_to_anthropic::responses_to_anthropic;
pub(crate) use responses_to_chat::message_text;
pub use responses_to_chat::responses_to_chat;

/// Locally estimated `input_tokens` for a `count_tokens` request that cannot
/// be forwarded (`openai-chat` has no token-counting endpoint at all).
///
/// This is deliberately an estimate, not a real tokenizer: Claude Code uses
/// the number for context sizing and auto-compaction, so answering with a
/// rough count that keeps the session roughly correct (±30%) is far better
/// than a `400` on every keystroke of a translated route, which would
/// disable auto-compaction outright. Heuristic: four ASCII characters per
/// token, one token per non-ASCII character (CJK text is close to one token
/// per character for this tokenizer family), counted over `system`, every
/// message's text content, and tool names/descriptions/schemas.
pub fn estimate_input_tokens(payload: &serde_json::Value) -> u64 {
    let mut total = 0u64;

    if let Some(system) = payload.get("system") {
        total += text_tokens(&system_text(system));
    }

    if let Some(messages) = payload.get("messages").and_then(|m| m.as_array()) {
        for message in messages {
            total += text_tokens(&message_text(message));
        }
    }

    if let Some(tools) = payload.get("tools").and_then(|t| t.as_array()) {
        for tool in tools {
            if let Some(name) = tool.get("name").and_then(|v| v.as_str()) {
                total += text_tokens(name);
            }
            if let Some(description) = tool.get("description").and_then(|v| v.as_str()) {
                total += text_tokens(description);
            }
            if let Some(schema) = tool.get("input_schema") {
                total += text_tokens(&schema.to_string());
            }
        }
    }

    total.max(1)
}

/// Character-counting token estimate for one string: cheap, deterministic,
/// and close enough for context-sizing purposes without a real tokenizer.
fn text_tokens(s: &str) -> u64 {
    let mut ascii_len = 0u64;
    let mut non_ascii = 0u64;
    for c in s.chars() {
        if c.is_ascii() {
            ascii_len += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii_len / 4 + non_ascii
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn estimate_counts_roughly_four_ascii_chars_per_token() {
        let payload = json!({
            "messages": [{"role": "user", "content": "12345678"}],
        });
        assert_eq!(estimate_input_tokens(&payload), 2);
    }

    #[test]
    fn estimate_counts_one_token_per_non_ascii_character() {
        let payload = json!({
            "messages": [{"role": "user", "content": "こんにちは"}],
        });
        assert_eq!(estimate_input_tokens(&payload), 5);
    }

    #[test]
    fn estimate_has_a_floor_of_one_token() {
        let payload = json!({});
        assert_eq!(estimate_input_tokens(&payload), 1);
    }

    #[test]
    fn estimate_includes_system_and_tool_schemas() {
        let short = json!({"messages": [{"role": "user", "content": "hi"}]});
        let with_extras = json!({
            "system": "a fairly long system prompt to push the count up",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "get_weather",
                "description": "look up the weather for a city",
                "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}},
            }],
        });
        assert!(estimate_input_tokens(&with_extras) > estimate_input_tokens(&short));
    }
}
