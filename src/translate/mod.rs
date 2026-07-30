//! Cross-protocol translation — the one place a body is rebuilt.
//!
//! Everything else in this crate forwards responses byte-for-byte (see
//! `server::passthrough`). That guarantee holds for every same-protocol
//! request, which is still the overwhelming majority: translation only runs
//! when the client's protocol and the target provider's protocol differ —
//! precisely the combination that used to be a flat `400`.
//!
//! Today exactly one direction exists:
//!
//! | client speaks | provider speaks | translation |
//! |---|---|---|
//! | `anthropic-messages` | `openai-chat` | [`Translation::AnthropicToChat`] |
//!
//! That single direction is what makes `launch claude` useful: Claude Code only
//! ever speaks `/v1/messages`, and all of Ollama (local and cloud), Gemini,
//! Groq, DeepSeek, Mistral, Together, Sakana AI and PLaMo speak `openai-chat`.
//! Without it those providers are unreachable from Claude Code.
//!
//! Split by direction and by shape, because the three problems are genuinely
//! different:
//!
//! - [`request`] — one JSON object in, one JSON object out. Pure.
//! - [`response`] — one complete JSON body in, one out. Pure.
//! - [`stream`] — an SSE event *sequence* in, a different event sequence out.
//!   Stateful, and the hard part: `openai-chat` streams flat deltas while
//!   `anthropic-messages` streams explicitly opened and closed content blocks.
//! - [`adapter`] — plugs the above into the byte stream the proxy forwards.
//!
//! Translation is deliberately **infallible**. A body that does not look the
//! way it should produces a thin-but-valid result rather than an error: the
//! request has already been accepted, and a translation bug must not be able
//! to turn a working route into a 500.

pub mod adapter;
pub mod request;
pub mod response;
pub mod stream;

use crate::config::ApiKind;

/// A protocol pair the gateway can translate between.
///
/// Named by direction (`<client>To<provider>`), because translation is not
/// symmetric: the request goes one way and the response comes back the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Translation {
    /// An `anthropic-messages` client (Claude Code) talking to an
    /// `openai-chat` provider (Ollama, Groq, DeepSeek, …).
    AnthropicToChat,
}

impl Translation {
    /// The translation needed to let a `client`-speaking caller reach a
    /// `provider`-speaking upstream.
    ///
    /// `None` covers two very different cases, and callers must treat them
    /// differently:
    ///
    /// - `client == provider` — nothing to do, use the passthrough path.
    /// - anything else — the pair is not supported; the request must be
    ///   refused rather than forwarded as garbage.
    ///
    /// So check protocol equality *first*, then call this.
    pub fn select(client: ApiKind, provider: ApiKind) -> Option<Self> {
        match (client, provider) {
            (ApiKind::AnthropicMessages, ApiKind::OpenaiChat) => Some(Self::AnthropicToChat),
            _ => None,
        }
    }

    /// Stable identifier for logs and the trace record's `resolved.translation`.
    pub fn label(self) -> &'static str {
        match self {
            Self::AnthropicToChat => "anthropic-messages->openai-chat",
        }
    }

    /// Rebuild a request body in the provider's protocol.
    pub fn request(self, payload: &serde_json::Value) -> serde_json::Value {
        match self {
            Self::AnthropicToChat => request::anthropic_to_chat(payload),
        }
    }

    /// Rebuild a complete, non-streaming response body in the client's
    /// protocol. `model` is what to report when the upstream body does not
    /// name a model itself.
    pub fn response(self, body: &serde_json::Value, model: &str) -> serde_json::Value {
        match self {
            Self::AnthropicToChat => response::chat_to_anthropic(body, model),
        }
    }

    /// Rebuild an upstream *error* body in the client's protocol, so the
    /// client's own error handling sees the envelope it expects instead of a
    /// foreign one.
    pub fn error(self, body: &serde_json::Value, status: u16) -> serde_json::Value {
        match self {
            Self::AnthropicToChat => response::chat_error_to_anthropic(body, status),
        }
    }

    /// Whether `POST /v1/messages/count_tokens` can be forwarded at all.
    ///
    /// `openai-chat` has no token-counting endpoint, so it cannot: the proxy
    /// answers locally with [`request::estimate_input_tokens`] instead. Kept
    /// as a method rather than assumed at the call site so a future
    /// translation whose target *does* count tokens can say so.
    pub fn can_forward_count_tokens(self) -> bool {
        match self {
            Self::AnthropicToChat => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_supported_pair_is_anthropic_client_to_chat_provider() {
        assert_eq!(
            Translation::select(ApiKind::AnthropicMessages, ApiKind::OpenaiChat),
            Some(Translation::AnthropicToChat)
        );
    }

    #[test]
    fn unsupported_pairs_are_none() {
        // The reverse direction does not exist yet.
        assert!(Translation::select(ApiKind::OpenaiChat, ApiKind::AnthropicMessages).is_none());
        // Responses is not translated in either direction (issue #4).
        assert!(
            Translation::select(ApiKind::AnthropicMessages, ApiKind::OpenaiResponses).is_none()
        );
        assert!(Translation::select(ApiKind::OpenaiResponses, ApiKind::OpenaiChat).is_none());
    }

    #[test]
    fn same_protocol_is_also_none_so_callers_must_check_equality_first() {
        assert!(Translation::select(ApiKind::OpenaiChat, ApiKind::OpenaiChat).is_none());
        assert!(
            Translation::select(ApiKind::AnthropicMessages, ApiKind::AnthropicMessages).is_none()
        );
    }
}
