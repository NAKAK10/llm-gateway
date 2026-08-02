//! Cross-protocol translation — the one place a body is rebuilt.
//!
//! Everything else in this crate forwards responses byte-for-byte (see
//! `server::passthrough`). That guarantee holds for every same-protocol
//! request, which is still the overwhelming majority: translation only runs
//! when the client's protocol and the target provider's protocol differ —
//! precisely the combination that used to be a flat `400`.
//!
//! Today three directions exist:
//!
//! | client speaks | provider speaks | translation |
//! |---|---|---|
//! | `anthropic-messages` | `openai-chat` | [`Translation::AnthropicToChat`] |
//! | `openai-responses` | `openai-chat` | [`Translation::ResponsesToChat`] |
//! | `openai-responses` | `anthropic-messages` | [`Translation::ResponsesToAnthropic`] |
//!
//! The first is what makes `launch claude` useful: Claude Code only ever
//! speaks `/v1/messages`, and all of Ollama (local and cloud), Gemini, Groq,
//! DeepSeek, Mistral, Together, Sakana AI and PLaMo speak `openai-chat`.
//! Without it those providers are unreachable from Claude Code.
//!
//! The second is what makes `launch codex` useful once Codex CLI drops
//! `wire_api = "chat"` support: Codex only ever speaks `/v1/responses`, so
//! the same roster of `openai-chat`-only providers needs this direction too.
//!
//! The third lets Codex CLI reach an `anthropic-messages` provider directly
//! — notably the `claude-cli` agent transport, whose upstream only ever
//! speaks Anthropic's own wire shape (see `agent::claude_cli`).
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
    /// An `openai-responses` client (Codex CLI) talking to an `openai-chat`
    /// provider (Ollama, Groq, DeepSeek, …).
    ResponsesToChat,
    /// An `openai-responses` client (Codex CLI) talking to an
    /// `anthropic-messages` provider (notably the `claude-cli` agent
    /// transport).
    ResponsesToAnthropic,
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
            (ApiKind::OpenaiResponses, ApiKind::OpenaiChat) => Some(Self::ResponsesToChat),
            (ApiKind::OpenaiResponses, ApiKind::AnthropicMessages) => {
                Some(Self::ResponsesToAnthropic)
            }
            _ => None,
        }
    }

    /// Stable identifier for logs and the trace record's `resolved.translation`.
    pub fn label(self) -> &'static str {
        match self {
            Self::AnthropicToChat => "anthropic-messages->openai-chat",
            Self::ResponsesToChat => "openai-responses->openai-chat",
            Self::ResponsesToAnthropic => "openai-responses->anthropic-messages",
        }
    }

    /// Rebuild a request body in the provider's protocol.
    pub fn request(self, payload: &serde_json::Value) -> serde_json::Value {
        match self {
            Self::AnthropicToChat => request::anthropic_to_chat(payload),
            Self::ResponsesToChat => request::responses_to_chat(payload),
            Self::ResponsesToAnthropic => request::responses_to_anthropic(payload),
        }
    }

    /// Rebuild a complete, non-streaming response body in the client's
    /// protocol. `model` is what to report when the upstream body does not
    /// name a model itself.
    pub fn response(self, body: &serde_json::Value, model: &str) -> serde_json::Value {
        match self {
            Self::AnthropicToChat => response::chat_to_anthropic(body, model),
            Self::ResponsesToChat => response::chat_to_responses(body, model),
            Self::ResponsesToAnthropic => response::anthropic_to_responses(body, model),
        }
    }

    /// Rebuild an upstream *error* body in the client's protocol, so the
    /// client's own error handling sees the envelope it expects instead of a
    /// foreign one.
    pub fn error(self, body: &serde_json::Value, status: u16) -> serde_json::Value {
        match self {
            Self::AnthropicToChat => response::chat_error_to_anthropic(body, status),
            Self::ResponsesToChat => response::chat_error_to_responses(body, status),
            Self::ResponsesToAnthropic => response::anthropic_error_to_responses(body, status),
        }
    }

    /// The gateway's own error envelope, in the client's protocol — for a
    /// failure inside translation itself (an unreadable or oversized
    /// upstream body), as opposed to an error the upstream reported (see
    /// [`Self::error`] above, which translates a real upstream error body).
    pub fn gateway_error(self, message: &str) -> serde_json::Value {
        match self {
            Self::AnthropicToChat => response::anthropic_gateway_error(message),
            Self::ResponsesToChat => response::responses_gateway_error(message),
            // The client speaks `openai-responses` here too, same as
            // `ResponsesToChat` — the gateway's own failure envelope depends
            // only on the client's protocol, not on which provider it was
            // headed for.
            Self::ResponsesToAnthropic => response::responses_gateway_error(message),
        }
    }

    /// A fresh stateful converter for this translation's streaming direction,
    /// boxed so `translate::adapter::Mode::Sse` can hold either output
    /// protocol behind one type. `model` is what the converter reports when
    /// upstream chunks do not name one themselves.
    pub fn stream_converter(self, model: String) -> Box<dyn stream::StreamConverter> {
        match self {
            Self::AnthropicToChat => Box::new(stream::ChatToAnthropic::new(model)),
            Self::ResponsesToChat => Box::new(stream::ChatToResponses::new(model)),
            Self::ResponsesToAnthropic => Box::new(stream::AnthropicToResponses::new(model)),
        }
    }

    /// Whether `POST /v1/messages/count_tokens` can be forwarded at all.
    ///
    /// `openai-chat` has no token-counting endpoint, so it cannot: the proxy
    /// answers locally with [`request::estimate_input_tokens`] instead. Kept
    /// as a method rather than assumed at the call site so a future
    /// translation whose target *does* count tokens can say so.
    ///
    /// `ResponsesToChat` answers `false` too, but for a different reason than
    /// documentation might suggest at a glance: `openai-responses` has no
    /// `count_tokens` endpoint of its own for this to matter to, so the
    /// question never actually reaches this method in practice (see
    /// `server::endpoint_api`) — it is answered here only to keep the method
    /// total over every variant.
    pub fn can_forward_count_tokens(self) -> bool {
        match self {
            Self::AnthropicToChat => false,
            Self::ResponsesToChat => false,
            Self::ResponsesToAnthropic => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_supported_pairs_are_anthropic_and_responses_clients_to_a_chat_provider() {
        assert_eq!(
            Translation::select(ApiKind::AnthropicMessages, ApiKind::OpenaiChat),
            Some(Translation::AnthropicToChat)
        );
        assert_eq!(
            Translation::select(ApiKind::OpenaiResponses, ApiKind::OpenaiChat),
            Some(Translation::ResponsesToChat)
        );
        assert_eq!(
            Translation::select(ApiKind::OpenaiResponses, ApiKind::AnthropicMessages),
            Some(Translation::ResponsesToAnthropic)
        );
    }

    #[test]
    fn unsupported_pairs_are_none() {
        // The reverse directions do not exist yet.
        assert!(Translation::select(ApiKind::OpenaiChat, ApiKind::AnthropicMessages).is_none());
        assert!(Translation::select(ApiKind::OpenaiChat, ApiKind::OpenaiResponses).is_none());
        // `anthropic-messages` has no translation to `openai-responses` (the
        // reverse of `ResponsesToAnthropic`).
        assert!(
            Translation::select(ApiKind::AnthropicMessages, ApiKind::OpenaiResponses).is_none()
        );
    }

    #[test]
    fn responses_to_anthropic_labels_and_reports_correctly() {
        assert_eq!(
            Translation::ResponsesToAnthropic.label(),
            "openai-responses->anthropic-messages"
        );
        assert!(!Translation::ResponsesToAnthropic.can_forward_count_tokens());
    }

    #[test]
    fn same_protocol_is_also_none_so_callers_must_check_equality_first() {
        assert!(Translation::select(ApiKind::OpenaiChat, ApiKind::OpenaiChat).is_none());
        assert!(
            Translation::select(ApiKind::AnthropicMessages, ApiKind::AnthropicMessages).is_none()
        );
    }
}
