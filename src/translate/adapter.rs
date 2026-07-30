//! Plugging translation into the byte stream the proxy forwards.
//!
//! The proxy hands the upstream body to `usage::tee::observe` and then to
//! `server::passthrough::respond`. On a translated route one more layer goes
//! between them: this one. It keeps the same stream item type
//! (`Result<Bytes, reqwest::Error>`), so `respond` and the usage observer stay
//! exactly as they are — the observer still sees the *upstream* bytes in the
//! upstream's protocol, which is what makes token accounting correct on a
//! translated route.
//!
//! Two shapes, because SSE and a plain JSON body have opposite requirements:
//!
//! - SSE is converted event by event, so the first token still reaches the
//!   client the moment the upstream emits it. Streaming latency is the whole
//!   point of the gateway and translation must not buffer it away.
//! - A non-streaming body cannot be converted incrementally at all — the
//!   Anthropic shape needs `stop_reason` and `usage`, which only exist once the
//!   last byte has arrived. So it is buffered and emitted as one chunk.
//!
//! A translation failure never propagates as a stream error: the client gets a
//! valid Anthropic error envelope instead, because a client that receives a
//! malformed body reports something far less useful than one that receives a
//! real error message.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;

use crate::translate::stream::ChatToAnthropic;
use crate::translate::Translation;

/// Cap on how much of a non-streaming body gets buffered for translation.
/// Matches `usage::tee`'s cap for the same reason: a misbehaving upstream must
/// not be able to grow our memory without bound.
const MAX_BUFFERED_BODY_BYTES: usize = 32 * 1024 * 1024;

/// How much of an untranslatable body to quote back in the error message.
const SNIPPET_CHARS: usize = 200;

/// What the upstream response body is, and therefore how it must be rebuilt.
///
/// Decided by the proxy from the status line and the request's `stream` flag,
/// before the first byte is read.
#[derive(Debug, Clone)]
pub enum ResponseShape {
    /// A successful streaming response: `text/event-stream`.
    Sse {
        /// Reported as the message's `model` when the upstream events do not
        /// name one themselves.
        model: String,
    },
    /// A successful non-streaming response: one complete JSON object.
    Json { model: String },
    /// Any non-success status, streaming or not — providers return a plain
    /// JSON error body in both cases.
    Error { status: u16 },
}

/// Wrap an upstream body stream so what reaches the client speaks the client's
/// protocol.
pub fn translate_body<S>(
    inner: S,
    translation: Translation,
    shape: ResponseShape,
) -> impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let mode = match shape {
        ResponseShape::Sse { model } => Mode::Sse(ChatToAnthropic::new(model)),
        ResponseShape::Json { model } => Mode::Buffer {
            kind: JsonKind::Success { model },
            buffer: Vec::new(),
            overflowed: false,
        },
        ResponseShape::Error { status } => Mode::Buffer {
            kind: JsonKind::Error { status },
            buffer: Vec::new(),
            overflowed: false,
        },
    };

    TranslateStream {
        inner: Box::pin(inner),
        translation,
        mode,
        finished: false,
    }
}

enum JsonKind {
    Success { model: String },
    Error { status: u16 },
}

enum Mode {
    /// Event-by-event conversion, no buffering beyond a partial event.
    Sse(ChatToAnthropic),
    /// The whole body, translated once the upstream signals the end.
    Buffer {
        kind: JsonKind,
        buffer: Vec<u8>,
        /// Set once `buffer` would exceed [`MAX_BUFFERED_BODY_BYTES`]. The
        /// body is then abandoned and the client gets an error envelope —
        /// forwarding a truncated JSON object would be worse.
        overflowed: bool,
    },
}

/// Every field is `Unpin` (`inner` is already boxed and pinned), so the struct
/// is too and `poll_next` needs no unsafe projection — same reasoning as
/// `usage::tee::ObserveStream`.
struct TranslateStream {
    inner: Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>,
    translation: Translation,
    mode: Mode,
    /// Set once the last downstream bytes have been produced, so the poll
    /// after that ends the stream instead of emitting terminal events twice.
    finished: bool,
}

impl Stream for TranslateStream {
    type Item = std::result::Result<Bytes, reqwest::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if this.finished {
                return Poll::Ready(None);
            }

            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    match &mut this.mode {
                        Mode::Sse(converter) => {
                            let out = converter.push(&bytes);
                            // An upstream chunk can carry only a partial event,
                            // in which case there is nothing to emit yet — poll
                            // again rather than handing the client an empty
                            // chunk.
                            if out.is_empty() {
                                continue;
                            }
                            return Poll::Ready(Some(Ok(Bytes::from(out))));
                        }
                        Mode::Buffer {
                            buffer, overflowed, ..
                        } => {
                            if !*overflowed {
                                if buffer.len() + bytes.len() > MAX_BUFFERED_BODY_BYTES {
                                    *overflowed = true;
                                    buffer.clear();
                                } else {
                                    buffer.extend_from_slice(&bytes);
                                }
                            }
                            continue;
                        }
                    }
                }
                // Forwarded as-is: the response is already committed, and a
                // stream error is the one thing the client can interpret
                // correctly without our help.
                Poll::Ready(Some(Err(err))) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(err)));
                }
                Poll::Ready(None) => {
                    this.finished = true;
                    let out = this.terminal();
                    if out.is_empty() {
                        return Poll::Ready(None);
                    }
                    return Poll::Ready(Some(Ok(out)));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl TranslateStream {
    /// The bytes to emit when the upstream body ends: the closing SSE events,
    /// or the whole translated JSON body.
    fn terminal(&mut self) -> Bytes {
        let translation = self.translation;
        match &mut self.mode {
            Mode::Sse(converter) => Bytes::from(converter.finish()),
            Mode::Buffer {
                kind,
                buffer,
                overflowed,
            } => {
                let body = std::mem::take(buffer);
                let json = if *overflowed {
                    Err(format!(
                        "upstream response exceeded {MAX_BUFFERED_BODY_BYTES} bytes, \
                         which is more than this gateway will translate"
                    ))
                } else {
                    parse_body(&body)
                };

                let value = match (kind, json) {
                    (JsonKind::Success { model }, Ok(body)) => translation.response(&body, model),
                    (JsonKind::Error { status }, Ok(body)) => translation.error(&body, *status),
                    // An error status *and* an unreadable body: the status is
                    // still the useful part, so it leads the message.
                    (JsonKind::Error { status }, Err(detail)) => {
                        anthropic_error(&format!("upstream returned HTTP {status}: {detail}"))
                    }
                    (JsonKind::Success { .. }, Err(detail)) => anthropic_error(&format!(
                        "upstream returned 2xx with a body this gateway could not \
                         translate: {detail}"
                    )),
                };
                Bytes::from(value.to_string())
            }
        }
    }
}

/// Parse a buffered body, describing what was there instead when it is not
/// JSON — an HTML error page from a reverse proxy is the common case, and
/// seeing the first line of it is what identifies the problem.
fn parse_body(body: &[u8]) -> std::result::Result<serde_json::Value, String> {
    if body.is_empty() {
        return Err("empty body".to_string());
    }
    serde_json::from_slice(body).map_err(|err| {
        let text = String::from_utf8_lossy(body);
        let snippet: String = text.chars().take(SNIPPET_CHARS).collect();
        format!("{err} (body starts: {snippet})")
    })
}

/// The Anthropic error envelope. `type: api_error` because this is the
/// gateway's own failure, not the client's.
fn anthropic_error(message: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "error": { "type": "api_error", "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    fn source(chunks: Vec<&'static str>) -> impl Stream<Item = Result<Bytes, reqwest::Error>> {
        futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok(Bytes::from_static(c.as_bytes()))),
        )
    }

    async fn collect(
        stream: impl Stream<Item = Result<Bytes, reqwest::Error>>,
    ) -> std::result::Result<String, ()> {
        let chunks: Vec<_> = stream.collect().await;
        let mut out = String::new();
        for chunk in chunks {
            match chunk {
                Ok(bytes) => out.push_str(&String::from_utf8_lossy(&bytes)),
                Err(_) => return Err(()),
            }
        }
        Ok(out)
    }

    #[tokio::test]
    async fn a_non_streaming_body_is_translated_as_one_chunk() {
        let body = r#"{"id":"chatcmpl-1","model":"qwen3","choices":[{"index":0,
            "message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":3,"completion_tokens":1}}"#;
        let out = translate_body(
            source(vec![body]),
            Translation::AnthropicToChat,
            ResponseShape::Json {
                model: "fallback".to_string(),
            },
        );

        let text = collect(out).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["content"][0]["text"], "hi");
        assert_eq!(value["stop_reason"], "end_turn");
    }

    #[tokio::test]
    async fn an_error_body_becomes_an_anthropic_error_envelope() {
        let body = r#"{"error":{"message":"model not found","type":"invalid_request_error"}}"#;
        let out = translate_body(
            source(vec![body]),
            Translation::AnthropicToChat,
            ResponseShape::Error { status: 404 },
        );

        let text = collect(out).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["message"], "model not found");
    }

    #[tokio::test]
    async fn a_body_that_is_not_json_still_produces_a_readable_error() {
        let out = translate_body(
            source(vec!["<html>502 Bad Gateway</html>"]),
            Translation::AnthropicToChat,
            ResponseShape::Error { status: 502 },
        );

        let text = collect(out).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "error");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("502"), "{message}");
        assert!(message.contains("Bad Gateway"), "{message}");
    }

    #[tokio::test]
    async fn an_empty_success_body_does_not_panic() {
        let out = translate_body(
            source(vec![]),
            Translation::AnthropicToChat,
            ResponseShape::Json {
                model: "m".to_string(),
            },
        );

        let text = collect(out).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["type"], "error");
    }

    #[tokio::test]
    async fn sse_events_are_converted_as_they_arrive() {
        let out = translate_body(
            source(vec![
                "data: {\"id\":\"chatcmpl-1\",\"model\":\"qwen3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"he\"}}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"llo\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            ]),
            Translation::AnthropicToChat,
            ResponseShape::Sse { model: "m".to_string() },
        );

        let text = collect(out).await.unwrap();
        assert!(text.contains("event: message_start"), "{text}");
        assert!(text.contains("\"text\":\"he\""), "{text}");
        assert!(text.contains("\"text\":\"llo\""), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
        // An Anthropic stream has no `[DONE]` sentinel; forwarding one would
        // be a foreign token in a protocol that does not define it.
        assert!(!text.contains("[DONE]"), "{text}");
    }

    #[tokio::test]
    async fn a_stream_that_ends_without_a_finish_reason_is_still_closed_properly() {
        // A truncated upstream stream must not leave the client waiting for a
        // `message_stop` that never comes.
        let out = translate_body(
            source(vec![
                "data: {\"id\":\"c1\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n",
            ]),
            Translation::AnthropicToChat,
            ResponseShape::Sse { model: "m".to_string() },
        );

        let text = collect(out).await.unwrap();
        assert!(text.contains("event: content_block_stop"), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
    }
}
