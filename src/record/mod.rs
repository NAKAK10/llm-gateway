//! On-disk records.
//!
//! Two files, two purposes:
//!
//! - `usage-YYYY-MM.jsonl` — one line per request, always written. Feeds `stats`.
//! - `trace-YYYY-MM-DD.jsonl` — the full routing decision, only with `--debug`.
//!   This is the file that answers "why did *that* model get picked?", which is
//!   the whole reason the routing metadata is structured rather than logged as
//!   prose.
//!
//! Trace records contain prompt text. User messages are truncated to
//! [`TRUNCATE_CHARS`] unless `--debug-full` was given.

pub mod trace_log;
pub mod usage_log;

use std::path::PathBuf;
use std::sync::Arc;

use crate::error::Result;

/// How much of a user message a trace record keeps by default.
pub const TRUNCATE_CHARS: usize = 200;

/// What the recorder is allowed to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordMode {
    pub usage: bool,
    pub debug: bool,
    /// Keep prompt text untruncated. Only meaningful with `debug`.
    pub debug_full: bool,
}

impl RecordMode {
    pub fn truncate_at(&self) -> Option<usize> {
        if self.debug_full {
            None
        } else {
            Some(TRUNCATE_CHARS)
        }
    }
}

/// Owns the append handles for both files.
///
/// Writes are queued to a background task so a slow disk cannot add latency to
/// a request.
pub struct Recorder {
    pub(crate) mode: RecordMode,
    pub(crate) dir: PathBuf,
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<Entry>,
}

/// One queued write.
pub enum Entry {
    Usage(usage_log::UsageRecord),
    Trace(trace_log::TraceRecord),
}

impl Recorder {
    /// Create the log directory and start the writer task.
    pub fn start(dir: PathBuf, mode: RecordMode) -> Result<Arc<Self>> {
        let _ = (dir, mode);
        todo!("src/record/mod.rs")
    }

    pub fn mode(&self) -> RecordMode {
        self.mode
    }

    /// Queue a usage line. Dropped silently if usage recording is off.
    pub fn usage(&self, record: usage_log::UsageRecord) {
        if self.mode.usage {
            let _ = self.tx.send(Entry::Usage(record));
        }
    }

    /// Queue a trace line. Dropped silently unless `--debug` is on.
    pub fn trace(&self, record: trace_log::TraceRecord) {
        if self.mode.debug {
            let _ = self.tx.send(Entry::Trace(record));
        }
    }
}

/// Truncate on a character boundary and mark that it happened, so a reader can
/// tell a short prompt from a clipped one.
pub fn truncate(text: &str, limit: Option<usize>) -> String {
    match limit {
        None => text.to_string(),
        Some(max) => {
            if text.chars().count() <= max {
                text.to_string()
            } else {
                let head: String = text.chars().take(max).collect();
                format!("{head}…")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_marks_clipped_text() {
        assert_eq!(truncate("hello", Some(10)), "hello");
        assert_eq!(truncate("hello", Some(3)), "hel…");
        assert_eq!(truncate("hello", None), "hello");
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        // Would panic on a byte-index slice.
        let s = "日本語のテキストです";
        assert_eq!(truncate(s, Some(3)), "日本語…");
    }
}
