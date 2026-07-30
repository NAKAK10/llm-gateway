//! Calling upstreams, and falling back when one refuses.
//!
//! **Fallback only happens before the first response byte reaches the client.**
//! Once a status line and the first chunk have gone out, the response is
//! committed — there is no way to un-send it and try another provider. So the
//! attempt loop inspects the HTTP status, and only then hands the body stream
//! over for forwarding. This is a real limitation, not an oversight: it means
//! fallback protects against refusals and outages, not mid-generation failures.
//!
//! Timeouts follow from the same constraint. A whole-request timeout would kill
//! a long but healthy generation, so the deadline is on *first byte* only.

use std::time::Duration;

use crate::config::ApiKind;
use crate::error::Result;
use crate::record::trace_log::TraceAttempt;
use crate::route::{Resolution, Target};

/// How long to wait for an upstream to start responding.
///
/// Generous, because a large prompt against a big model can legitimately take
/// this long to produce its first token.
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(120);

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the shared HTTP client.
///
/// Content encodings are deliberately **not** enabled. A compressed stream gets
/// re-buffered by the decoder, which destroys SSE chunk boundaries and adds
/// latency for exactly the case that matters most.
pub fn client() -> Result<reqwest::Client> {
    todo!("src/upstream.rs")
}

/// A request ready to be sent to one upstream.
pub struct Attempt<'a> {
    pub target: &'a Target,
    /// Request body with `model` already rewritten for this target.
    pub body: Vec<u8>,
    /// Headers copied from the inbound request, minus hop-by-hop ones.
    pub headers: http::HeaderMap,
    pub streaming: bool,
}

/// A successful upstream response, not yet forwarded.
pub struct Accepted {
    pub response: reqwest::Response,
    /// 1-based index into the resolution's target list.
    pub attempt: u32,
    pub target_provider: String,
    pub target_model: String,
    pub api: ApiKind,
}

/// Try each target in order until one returns a non-retryable response.
///
/// A response is "accepted" when its status is not one of the retryable classes
/// (connection failure, timeout, 408, 429, 5xx). A 4xx that is the *client's*
/// fault — a malformed request, a bad tool schema — is returned as-is rather
/// than retried, because sending the same broken request to another provider
/// just burns money and hides the real error.
///
/// `attempts` accumulates one entry per try for the trace log, including the
/// successful one.
pub async fn send_with_fallback(
    http: &reqwest::Client,
    resolution: &Resolution,
    build: impl Fn(&Target) -> Result<Attempt<'_>>,
    attempts: &mut Vec<TraceAttempt>,
) -> Result<Accepted> {
    let _ = (http, resolution, build, attempts);
    todo!("src/upstream.rs")
}

/// The upstream URL for a target, given the protocol it speaks.
pub fn endpoint_url(target: &Target, count_tokens: bool) -> String {
    let base = target.base_url.trim_end_matches('/');
    match target.api {
        // Anthropic's base URL is the host root, so the version prefix is ours
        // to add. Everything else already includes `/v1` in `base_url`.
        ApiKind::AnthropicMessages => {
            if count_tokens {
                format!("{base}/v1/messages/count_tokens")
            } else {
                format!("{base}/v1/messages")
            }
        }
        ApiKind::OpenaiChat => format!("{base}/chat/completions"),
        ApiKind::OpenaiResponses => format!("{base}/responses"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelRef;

    fn target(api: ApiKind, base: &str) -> Target {
        Target {
            model_ref: ModelRef {
                provider: "p".into(),
                model: "m".into(),
            },
            api,
            base_url: base.to_string(),
            inject_usage: true,
        }
    }

    #[test]
    fn anthropic_urls_add_the_version_prefix() {
        let t = target(ApiKind::AnthropicMessages, "https://api.anthropic.com");
        assert_eq!(endpoint_url(&t, false), "https://api.anthropic.com/v1/messages");
        assert_eq!(
            endpoint_url(&t, true),
            "https://api.anthropic.com/v1/messages/count_tokens"
        );
    }

    #[test]
    fn openai_urls_append_to_an_existing_version_prefix() {
        let chat = target(ApiKind::OpenaiChat, "https://openrouter.ai/api/v1");
        assert_eq!(
            endpoint_url(&chat, false),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        let resp = target(ApiKind::OpenaiResponses, "https://api.openai.com/v1");
        assert_eq!(endpoint_url(&resp, false), "https://api.openai.com/v1/responses");
    }
}
