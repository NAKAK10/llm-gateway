//! `openai-chat` SSE → client SSE.
//!
//! The hard half of translation. The two protocols do not disagree about
//! field names here so much as about *shape*: `openai-chat` streams a flat
//! sequence of deltas and lets the client work out what belongs to what, while
//! a client protocol streams explicitly opened and closed content. So this is
//! a state machine, and its job is to invent the structure the upstream never
//! sent.
//!
//! Split by direction: [`chat_to_anthropic::ChatToAnthropic`] targets
//! `anthropic-messages`, [`chat_to_responses::ChatToResponses`] targets
//! `openai-responses`. See each converter's own module docs for its event
//! table and invariants.
//!
//! What is lost in both directions: `reasoning_content` / `reasoning` deltas
//! that DeepSeek and some Ollama models emit are dropped rather than turned
//! into a `thinking` block — a real `thinking` block carries a `signature`
//! only Anthropic can produce, and forwarding the reasoning as ordinary text
//! would present it as the answer. Prompt-cache accounting and citations have
//! no `openai-chat` source at all.
//!
//! `usage` in the emitted events is for the client's benefit only; the
//! gateway's own accounting reads the upstream bytes in `usage::parse` and
//! never sees anything produced here.

mod anthropic_to_responses;
mod chat_to_anthropic;
mod chat_to_responses;

use serde_json::Value;

pub use anthropic_to_responses::AnthropicToResponses;
pub use chat_to_anthropic::ChatToAnthropic;
pub use chat_to_responses::ChatToResponses;

/// Defensive cap on a single partial event, mirroring `usage::parse`: a
/// misbehaving upstream must not be able to grow our buffer without bound.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Common interface over a stateful "upstream chat SSE → client SSE"
/// converter, so [`crate::translate::adapter::Mode::Sse`] can hold whichever
/// one a given [`crate::translate::Translation`] needs behind a single type.
pub trait StreamConverter: Send {
    /// Feed raw upstream SSE bytes; returns the client-protocol SSE bytes to
    /// emit downstream, which is empty when the chunk only carried a partial
    /// event.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8>;

    /// The closing events for a stream that ended without its own explicit
    /// terminator. Idempotent: a call after the stream is already closed
    /// returns empty.
    fn finish(&mut self) -> Vec<u8>;
}

/// The plain text of a `delta.content` field: usually a string; some proxies
/// mimic the Responses shape and send an array of `{"type":"text","text":…}`
/// parts even in a chat stream, which are concatenated.
fn delta_text(delta: &Value) -> String {
    match delta.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// One `tool_calls[]` entry's argument fragment, if it has one. Forwarded
/// verbatim when it is already a string (a fragment is not valid JSON on its
/// own, and the client reassembles the string before parsing it); some
/// servers send the whole argument object instead, which is serialised once
/// into a single fragment.
fn call_argument_fragment(function: Option<&Value>) -> Option<String> {
    match function.and_then(|f| f.get("arguments")) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(value @ Value::Object(_)) => Some(value.to_string()),
        _ => None,
    }
}

/// The last usage numbers seen upstream, in `openai-chat` names. Shared by
/// both converters in this module — the source shape being translated is the
/// same regardless of which client protocol comes out the other end.
#[derive(Default)]
struct ChatUsage {
    prompt: u64,
    completion: u64,
    cached: u64,
}

impl ChatUsage {
    fn record(&mut self, usage: &Value) {
        let field = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        self.prompt = field("prompt_tokens");
        self.completion = field("completion_tokens");
        self.cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
    }

    /// `prompt` with the cached portion subtracted out — what Anthropic's
    /// `input_tokens` means (exclusive of `cache_read_input_tokens`), unlike
    /// `prompt_tokens`, which already includes it. Only `ChatToAnthropic`
    /// needs this: the Responses converter reports `self.prompt` verbatim
    /// alongside `input_tokens_details.cached_tokens`, OpenAI's own
    /// (inclusive) nested convention, which is already internally
    /// consistent — see #24.
    fn non_cached_prompt(&self) -> u64 {
        self.prompt.saturating_sub(self.cached)
    }
}

/// One client-protocol SSE frame. Both the `event:` name and the `type`
/// inside the payload are sent, because clients dispatch on the former and
/// validate the latter.
fn emit(out: &mut Vec<u8>, event: &str, data: Value) {
    out.extend_from_slice(format!("event: {event}\ndata: {data}\n\n").as_bytes());
}

/// The concatenated `data:` payload of one SSE event.
///
/// Multi-line `data` fields are joined per the SSE spec; every payload in this
/// protocol is single-line JSON, so plain concatenation is enough. `event:`
/// lines are ignored — `openai-chat` does not name its events, and where a
/// provider does, the name carries nothing the payload does not.
fn event_data(event: &str) -> String {
    let mut data = String::new();
    for line in event.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data: ") {
            data.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest);
        }
    }
    data.trim().to_string()
}

/// A human-readable message out of whatever an upstream put in `error`: an
/// object with a `message`, a bare string (Ollama), or something unexpected —
/// in which case the raw JSON beats saying nothing.
fn error_message(error: &Value) -> String {
    if let Some(message) = error.get("message").and_then(|m| m.as_str()) {
        return message.to_string();
    }
    match error {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
