//! `openai-chat` response → client response (non-streaming).
//!
//! An OpenAI Chat Completion and a client-protocol response describe the same
//! outcome — assistant text, maybe tool calls, a stop reason, token counts —
//! but disagree on almost every field name and a few of the semantics. This
//! module is the one place that reconciles them for a complete, buffered
//! body; [`super::stream`] does the same job incrementally for SSE.
//!
//! Split by direction: [`chat_to_anthropic`] rebuilds an Anthropic Message,
//! [`chat_to_responses`] rebuilds an OpenAI Responses object, and [`errors`]
//! translates upstream and gateway error bodies into each protocol's own
//! envelope.
//!
//! What does **not** survive the trip, in either direction:
//!
//! - **Multiple choices.** Neither target protocol has a concept of `n > 1`;
//!   only `choices[0]` is read. No client this gateway serves asks for more.
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
//!   Anthropic's `stop_sequence` is always `null`.
//!
//! `usage` in the translated body is for the *client's* benefit only — it
//! lets the client render token counts the way it expects. The gateway's own
//! accounting reads the upstream body directly, in `usage::parse`, and never
//! looks at anything this module produces.

mod anthropic_to_responses;
mod chat_to_anthropic;
mod chat_to_responses;
mod errors;

use serde_json::Value;

pub use anthropic_to_responses::anthropic_to_responses;
pub use chat_to_anthropic::chat_to_anthropic;
pub use chat_to_responses::chat_to_responses;
pub use errors::{
    anthropic_error_to_responses, anthropic_gateway_error, chat_error_to_anthropic,
    chat_error_to_responses, responses_error_to_anthropic, responses_gateway_error,
};

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

/// The body's own `model`, when present and non-empty. Preferred over the
/// proxy-resolved target because the upstream's own answer is the honest one
/// — it is what actually generated the response.
fn body_model(body: &Value) -> Option<&str> {
    body.get("model")
        .and_then(|v| v.as_str())
        .filter(|m| !m.is_empty())
}

/// Map `finish_reason` to an Anthropic `stop_reason`. Callers must apply the
/// tool-call override on top of this — see [`chat_to_anthropic::chat_to_anthropic`].
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
