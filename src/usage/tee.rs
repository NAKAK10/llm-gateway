//! Forward a byte stream unchanged while observing it.
//!
//! Correctness rule for this module: whatever goes in comes out, in the same
//! chunks, in the same order. The observer runs on a copy and may never delay,
//! reorder or alter a byte.
//!
//! Reporting happens on `Drop`, not on clean completion. A client that
//! disconnects halfway still cost real tokens upstream, and a handler future
//! that gets cancelled never reaches its own end — so the only reliable place
//! to record is the destructor.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_util::Stream;

use crate::config::ApiKind;
use crate::usage::parse::{self, SseUsageScanner};
use crate::usage::Usage;

/// Called exactly once when the stream ends or is dropped.
pub type ReportFn = Box<dyn FnOnce(Usage, StreamOutcome) + Send>;

/// How the stream finished — recorded so an aborted request is not filed as a
/// clean success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamOutcome {
    /// Upstream signalled end of stream.
    Complete,
    /// Dropped before the upstream finished (client disconnect or cancellation).
    Aborted,
    /// The upstream stream itself produced an error.
    UpstreamError,
}

/// Cap on how much of a non-streaming body gets buffered for usage
/// extraction. Non-streaming bodies are ordinary JSON responses and stay well
/// under this in practice; past it, usage observation is abandoned and the
/// stream keeps passing bytes through untouched.
const MAX_BUFFERED_BODY_BYTES: usize = 32 * 1024 * 1024;

/// How usage is being extracted from the bytes passing through.
enum Scan {
    /// SSE, scanned incrementally as chunks arrive.
    Streaming(SseUsageScanner),
    /// A complete, non-streaming JSON body, parsed once the stream ends.
    Buffering {
        api: ApiKind,
        buffer: Vec<u8>,
        /// Set once `buffer` would exceed [`MAX_BUFFERED_BODY_BYTES`]. Usage
        /// observation is abandoned for the rest of the response.
        overflowed: bool,
    },
}

impl Scan {
    fn observe(&mut self, chunk: &[u8]) {
        match self {
            Scan::Streaming(scanner) => scanner.push(chunk),
            Scan::Buffering {
                buffer, overflowed, ..
            } => {
                if *overflowed {
                    return;
                }
                if buffer.len() + chunk.len() > MAX_BUFFERED_BODY_BYTES {
                    *overflowed = true;
                    *buffer = Vec::new();
                    return;
                }
                buffer.extend_from_slice(chunk);
            }
        }
    }

    fn finish(self) -> Usage {
        match self {
            Scan::Streaming(scanner) => scanner.finish(),
            Scan::Buffering {
                api,
                buffer,
                overflowed,
            } => {
                if overflowed {
                    return Usage::default();
                }
                serde_json::from_slice::<serde_json::Value>(&buffer)
                    .map(|body| parse::from_json(api, &body))
                    .unwrap_or_default()
            }
        }
    }
}

type InnerStream = Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>;

/// The observing wrapper. Every field is `Unpin`, so the struct as a whole is
/// too — the only thing requiring pinning (`inner`) is already boxed and
/// pinned, so `poll_next` needs no unsafe projection.
struct ObserveStream {
    inner: InnerStream,
    scan: Option<Scan>,
    report: Option<ReportFn>,
    outcome: StreamOutcome,
}

impl Stream for ObserveStream {
    type Item = std::result::Result<Bytes, reqwest::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(bytes))) => {
                if let Some(scan) = this.scan.as_mut() {
                    scan.observe(&bytes);
                }
                Poll::Ready(Some(Ok(bytes)))
            }
            Poll::Ready(Some(Err(err))) => {
                // First terminal signal wins: an errored item is commonly
                // followed by a `None` on the next poll (the underlying
                // stream considers itself finished), which must not overwrite
                // `UpstreamError` back to `Complete`.
                if this.outcome == StreamOutcome::Aborted {
                    this.outcome = StreamOutcome::UpstreamError;
                }
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                if this.outcome == StreamOutcome::Aborted {
                    this.outcome = StreamOutcome::Complete;
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for ObserveStream {
    fn drop(&mut self) {
        if let Some(report) = self.report.take() {
            let usage = self.scan.take().map(Scan::finish).unwrap_or_default();
            report(usage, self.outcome);
        }
    }
}

/// Wrap a byte stream so its usage is scanned and reported, passing every chunk
/// through untouched.
pub fn observe<S>(
    inner: S,
    api: ApiKind,
    streaming: bool,
    report: ReportFn,
) -> impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let scan = if streaming {
        Scan::Streaming(SseUsageScanner::new(api))
    } else {
        Scan::Buffering {
            api,
            buffer: Vec::new(),
            overflowed: false,
        }
    };
    ObserveStream {
        inner: Box::pin(inner),
        scan: Some(scan),
        report: Some(report),
        // Overwritten by `poll_next` on clean end or upstream error; if the
        // stream is dropped before either happens, this is what gets reported.
        outcome: StreamOutcome::Aborted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::sync::{Arc, Mutex};

    type ReportSlot = Arc<Mutex<Option<(Usage, StreamOutcome)>>>;

    fn collect_report() -> (ReportFn, ReportSlot) {
        let slot: ReportSlot = Arc::new(Mutex::new(None));
        let slot_clone = slot.clone();
        let report: ReportFn = Box::new(move |usage, outcome| {
            *slot_clone.lock().unwrap() = Some((usage, outcome));
        });
        (report, slot)
    }

    #[tokio::test]
    async fn passthrough_bytes_are_identical_and_usage_is_reported_on_completion() {
        let chunks = [
            "event: message_start\ndata: ".to_string(),
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}"#
                .to_string(),
            "\n\n".to_string(),
            r#"event: message_delta
data: {"type":"message_delta","usage":{"output_tokens":20}}

"#
            .to_string(),
        ];
        let input: Vec<Bytes> = chunks.iter().map(|s| Bytes::from(s.clone())).collect();
        let expected: Vec<u8> = input.iter().flat_map(|b| b.to_vec()).collect();

        let source = futures_util::stream::iter(input.clone().into_iter().map(Ok));
        let (report, slot) = collect_report();
        let observed = observe(source, ApiKind::AnthropicMessages, true, report);

        let out: Vec<Bytes> = observed.map(|r| r.unwrap()).collect().await;
        let actual: Vec<u8> = out.iter().flat_map(|b| b.to_vec()).collect();
        assert_eq!(actual, expected);

        let (usage, outcome) = slot.lock().unwrap().take().expect("report was called");
        assert_eq!(outcome, StreamOutcome::Complete);
        assert_eq!(
            usage,
            Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }
        );
    }

    #[tokio::test]
    async fn non_streaming_body_is_buffered_and_parsed_on_completion() {
        let body = r#"{"usage":{"prompt_tokens":5,"completion_tokens":7}}"#;
        let input = vec![Bytes::from(body)];
        let source = futures_util::stream::iter(input.into_iter().map(Ok));
        let (report, slot) = collect_report();
        let observed = observe(source, ApiKind::OpenaiChat, false, report);

        let out: Vec<Bytes> = observed.map(|r| r.unwrap()).collect().await;
        assert_eq!(out.len(), 1);
        assert_eq!(&out[0][..], body.as_bytes());

        let (usage, outcome) = slot.lock().unwrap().take().expect("report was called");
        assert_eq!(outcome, StreamOutcome::Complete);
        assert_eq!(usage.input_tokens, 5);
        assert_eq!(usage.output_tokens, 7);
    }

    #[tokio::test]
    async fn dropping_the_stream_early_reports_aborted() {
        let input = vec![Bytes::from_static(b"data: incomplete-event")];
        let source = futures_util::stream::iter(input.into_iter().map(Ok));
        let (report, slot) = collect_report();
        let observed = observe(source, ApiKind::AnthropicMessages, true, report);

        // Never poll it to completion — just drop it, as a cancelled handler
        // future would.
        drop(observed);

        let (_, outcome) = slot.lock().unwrap().take().expect("report was called");
        assert_eq!(outcome, StreamOutcome::Aborted);
    }

    #[tokio::test]
    async fn upstream_error_is_reported_and_forwarded() {
        let err_stream = futures_util::stream::once(async {
            // Build a genuine reqwest::Error via a request to an invalid URL.
            reqwest::Client::new()
                .get("http://") // fails to build/send
                .send()
                .await
                .unwrap_err()
        })
        .map(Err);
        let (report, slot) = collect_report();
        let observed = observe(err_stream, ApiKind::AnthropicMessages, true, report);
        let out: Vec<_> = observed.collect().await;
        assert_eq!(out.len(), 1);
        assert!(out[0].is_err());

        let (_, outcome) = slot.lock().unwrap().take().expect("report was called");
        assert_eq!(outcome, StreamOutcome::UpstreamError);
    }
}
