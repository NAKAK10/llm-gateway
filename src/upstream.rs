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
//! a long but healthy generation, so the deadline covers connecting and waiting
//! for response headers only.

use std::time::{Duration, Instant};

use http::HeaderValue;

use crate::config::ApiKind;
use crate::error::{Error, Result};
use crate::record::trace_log::TraceAttempt;
use crate::route::{Resolution, Target};

/// How long to wait for an upstream to return response headers.
///
/// Generous, because a large prompt against a big model can legitimately take
/// this long to produce its first token.
pub const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(120);

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the shared HTTP client.
///
/// Content encodings are deliberately **not** enabled (and the corresponding
/// cargo features are off). A compressed stream gets re-buffered by the
/// decoder, which destroys SSE chunk boundaries and adds latency for exactly
/// the case that matters most.
pub fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        // No overall timeout: streaming responses are unbounded by design.
        .build()
        .map_err(Error::from)
}

/// A request ready to be sent to one upstream.
pub struct Attempt {
    /// Request body with `model` already rewritten for this target.
    pub body: Vec<u8>,
    /// The same body, still parsed. An HTTP target sends the bytes; an agent CLI
    /// needs the fields (`messages`, `system`), and re-parsing what we just
    /// serialised would be silly.
    pub payload: serde_json::Value,
    /// Headers copied from the inbound request, minus hop-by-hop ones, plus
    /// provider extras. Auth is added here, per target, at send time.
    pub headers: http::HeaderMap,
    /// Route to `/v1/messages/count_tokens` instead of `/v1/messages`.
    pub count_tokens: bool,
    /// Whether the client asked for a stream. HTTP forwards the flag inside the
    /// body; an agent CLI needs it as a command-line decision.
    pub streaming: bool,
}

/// A response body on its way to the client.
///
/// Boxed so an HTTP response and a subprocess's output can be held by the same
/// field. The item type keeps `reqwest::Error` even for a local body — a
/// process-backed upstream simply never produces one, and reporting its failures
/// as an error *body* (which the client can read) beats a stream error (which it
/// cannot). That choice is what lets `usage::tee`, `translate::adapter` and
/// `server::passthrough` stay exactly as they were.
pub type BodyStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>,
>;

/// A successful (or final) upstream response, not yet forwarded.
pub struct Accepted {
    pub status: http::StatusCode,
    /// Upstream response headers, already copied out.
    pub headers: http::HeaderMap,
    pub body: BodyStream,
    /// 1-based index into the resolution's target list.
    pub attempt: u32,
    pub target_provider: String,
    pub target_model: String,
    pub api: ApiKind,
}

/// Try each target in order until one returns a non-retryable response.
///
/// Retryable: connection failure, header timeout, 408, 429, 5xx. Everything
/// else — including 400/401/403 — is returned as-is, because sending the same
/// broken request to another provider just burns money and hides the real
/// error.
///
/// The **last** target's response is always accepted, even when retryable:
/// forwarding the real 429 from the final fallback tells the client more than
/// any synthesized error could.
///
/// `attempts` accumulates one entry per try for the trace log, including the
/// accepted one.
pub async fn send_with_fallback(
    http: &reqwest::Client,
    resolution: &Resolution,
    build: impl Fn(&Target) -> Result<Attempt>,
    attempts: &mut Vec<TraceAttempt>,
) -> Result<Accepted> {
    let total = resolution.targets.len();
    debug_assert!(total > 0, "resolve() never returns an empty target list");

    for (index, target) in resolution.targets.iter().enumerate() {
        let n = (index + 1) as u32;
        let is_last = index + 1 == total;
        let started = Instant::now();

        let attempt = build(target)?;

        // A local agent CLI: no credential to resolve (the child authenticates
        // itself), no URL, and no first-byte deadline — process startup is the
        // latency, and it is bounded by the binary existing.
        if target.transport.is_agent_cli() {
            match crate::agent::spawn(target, &attempt.payload, attempt.streaming).await {
                Ok(spawned) => {
                    attempts.push(TraceAttempt {
                        n,
                        target: target.to_string(),
                        result: if spawned.status.is_success() {
                            "ok_first_byte".to_string()
                        } else {
                            format!("http_{}", spawned.status.as_u16())
                        },
                        ms: started.elapsed().as_millis() as u64,
                    });
                    return Ok(Accepted {
                        status: spawned.status,
                        headers: spawned.headers,
                        body: spawned.body,
                        attempt: n,
                        target_provider: target.model_ref.provider.clone(),
                        target_model: target.model_ref.model.clone(),
                        api: target.api,
                    });
                }
                Err(err) => {
                    tracing::info!(target = %target, "agent transport unavailable: {err}");
                    attempts.push(TraceAttempt {
                        n,
                        target: target.to_string(),
                        result: "spawn_error".to_string(),
                        ms: started.elapsed().as_millis() as u64,
                    });
                    if is_last {
                        break;
                    }
                    continue;
                }
            }
        }

        // Resolve the credential per attempt so a rotated env var or Keychain
        // entry is picked up without a reload. An unresolvable key skips the
        // target instead of aborting the request — the next fallback may have
        // a perfectly good one.
        let key = match &target.api_key {
            None => None,
            Some(secret) => match secret.resolve() {
                Ok(v) => Some(v),
                Err(err) => {
                    tracing::info!(target = %target, "skipping target: {err}");
                    attempts.push(TraceAttempt {
                        n,
                        target: target.to_string(),
                        result: "key_unresolved".to_string(),
                        ms: started.elapsed().as_millis() as u64,
                    });
                    continue;
                }
            },
        };

        let url = endpoint_url(target, attempt.count_tokens);
        let mut headers = attempt.headers;
        apply_auth(&mut headers, target.api, key.as_deref());

        let request = http.post(&url).headers(headers).body(attempt.body);

        let outcome = tokio::time::timeout(FIRST_BYTE_TIMEOUT, request.send()).await;
        let ms = started.elapsed().as_millis() as u64;

        match outcome {
            Err(_) => {
                attempts.push(TraceAttempt {
                    n,
                    target: target.to_string(),
                    result: "timeout".to_string(),
                    ms,
                });
                if is_last {
                    break;
                }
            }
            Ok(Err(err)) => {
                let result = if err.is_connect() {
                    "connect_error"
                } else {
                    "send_error"
                };
                tracing::info!(target = %target, "upstream error: {err}");
                attempts.push(TraceAttempt {
                    n,
                    target: target.to_string(),
                    result: result.to_string(),
                    ms,
                });
                if is_last {
                    break;
                }
            }
            Ok(Ok(response)) => {
                let status = response.status();
                let retryable = matches!(status.as_u16(), 408 | 429 | 500..=599);
                if retryable && !is_last {
                    attempts.push(TraceAttempt {
                        n,
                        target: target.to_string(),
                        result: format!("http_{}", status.as_u16()),
                        ms,
                    });
                    continue;
                }
                attempts.push(TraceAttempt {
                    n,
                    target: target.to_string(),
                    result: if status.is_success() {
                        "ok_first_byte".to_string()
                    } else {
                        format!("http_{}", status.as_u16())
                    },
                    ms,
                });
                return Ok(Accepted {
                    status: http::StatusCode::from_u16(response.status().as_u16())
                        .unwrap_or(http::StatusCode::BAD_GATEWAY),
                    headers: response.headers().clone(),
                    body: Box::pin(response.bytes_stream()),
                    attempt: n,
                    target_provider: target.model_ref.provider.clone(),
                    target_model: target.model_ref.model.clone(),
                    api: target.api,
                });
            }
        }
    }

    let detail = attempts
        .iter()
        .map(|a| format!("{} → {} ({}ms)", a.target, a.result, a.ms))
        .collect::<Vec<_>>()
        .join("; ");
    Err(Error::AllUpstreamsFailed {
        route: resolution.route_name.clone(),
        detail,
    })
}

/// Add the provider credential in the form its protocol expects.
///
/// Anthropic authenticates with `x-api-key` and requires `anthropic-version`;
/// the version header normally arrives from the client (Claude Code sends it),
/// so it is only defaulted when absent — e.g. when an OpenAI-protocol client's
/// request is ever routed to an Anthropic-shaped upstream.
fn apply_auth(headers: &mut http::HeaderMap, api: ApiKind, key: Option<&str>) {
    match api {
        ApiKind::AnthropicMessages => {
            if let Some(key) = key {
                if let Ok(v) = HeaderValue::from_str(key) {
                    headers.insert("x-api-key", v);
                }
            }
            if !headers.contains_key("anthropic-version") {
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
        }
        ApiKind::OpenaiChat | ApiKind::OpenaiResponses => {
            if let Some(key) = key {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {key}")) {
                    headers.insert(http::header::AUTHORIZATION, v);
                }
            }
        }
    }
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
    use crate::config::{ModelRef, SecretRef};

    fn target(api: ApiKind, base: &str) -> Target {
        Target {
            transport: Default::default(),
            agent_args: Vec::new(),
            model_ref: ModelRef {
                provider: "p".into(),
                model: "m".into(),
            },
            api,
            base_url: base.to_string(),
            api_key: None,
            headers: Vec::new(),
            inject_usage: true,
        }
    }

    #[test]
    fn anthropic_urls_add_the_version_prefix() {
        let t = target(ApiKind::AnthropicMessages, "https://api.anthropic.com");
        assert_eq!(
            endpoint_url(&t, false),
            "https://api.anthropic.com/v1/messages"
        );
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
        assert_eq!(
            endpoint_url(&resp, false),
            "https://api.openai.com/v1/responses"
        );
    }

    #[test]
    fn auth_shape_follows_the_protocol() {
        let mut h = http::HeaderMap::new();
        apply_auth(&mut h, ApiKind::AnthropicMessages, Some("sk-ant-x"));
        assert_eq!(h.get("x-api-key").unwrap(), "sk-ant-x");
        assert_eq!(h.get("anthropic-version").unwrap(), "2023-06-01");
        assert!(h.get("authorization").is_none());

        let mut h = http::HeaderMap::new();
        apply_auth(&mut h, ApiKind::OpenaiChat, Some("sk-x"));
        assert_eq!(h.get("authorization").unwrap(), "Bearer sk-x");
    }

    #[test]
    fn client_supplied_anthropic_version_is_kept() {
        let mut h = http::HeaderMap::new();
        h.insert("anthropic-version", HeaderValue::from_static("2024-10-22"));
        apply_auth(&mut h, ApiKind::AnthropicMessages, Some("k"));
        assert_eq!(h.get("anthropic-version").unwrap(), "2024-10-22");
    }

    /// An unresolvable key on the first target must not kill the request while
    /// a later target can still work.
    #[tokio::test]
    async fn unresolvable_key_skips_to_next_target() {
        let mut bad = target(ApiKind::OpenaiChat, "http://127.0.0.1:9");
        bad.api_key = Some(SecretRef::new("${LLM_GATEWAY_TEST_UNSET_VAR_1}"));
        // Second target also fails (nothing listens on port 9), but the point
        // is the *attempt trail*: key_unresolved followed by connect_error.
        let also_bad = target(ApiKind::OpenaiChat, "http://127.0.0.1:9");

        let resolution = Resolution {
            route_name: "r".into(),
            targets: vec![bad, also_bad],
        };
        let http = client().unwrap();
        let mut attempts = Vec::new();
        let result = send_with_fallback(
            &http,
            &resolution,
            |_t| {
                Ok(Attempt {
                    body: b"{}".to_vec(),
                    payload: serde_json::json!({}),
                    streaming: false,
                    headers: http::HeaderMap::new(),
                    count_tokens: false,
                })
            },
            &mut attempts,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].result, "key_unresolved");
        assert_eq!(attempts[1].result, "connect_error");
    }
}
