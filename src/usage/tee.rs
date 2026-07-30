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

use bytes::Bytes;
use futures_util::Stream;

use crate::config::ApiKind;
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

/// Wrap a byte stream so its usage is scanned and reported, passing every chunk
/// through untouched.
pub fn observe<S>(inner: S, api: ApiKind, streaming: bool, report: ReportFn) -> impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send
where
    S: Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let _ = (inner, api, streaming, report);
    todo!("src/usage/tee.rs");
    #[allow(unreachable_code)]
    futures_util::stream::empty()
}
