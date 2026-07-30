//! `llm-gateway trace` — read routing decisions back.
//!
//! Pretty-prints `trace-*.jsonl`, optionally following the newest file as it
//! grows. This is the operator's answer to "why did that request go to *that*
//! model?" — the data is already structured, this just renders it.

use crate::error::Result;

pub struct Options {
    /// Follow the newest trace file, like `tail -f`.
    pub tail: bool,
    /// Only entries whose matched route equals this.
    pub route: Option<String>,
    /// Only entries from this client tag.
    pub client: Option<String>,
}

pub fn run(options: Options) -> Result<()> {
    let _ = options;
    todo!("src/cli/trace.rs")
}

/// Render one record as a compact single line for the terminal.
///
/// Pure, so formatting is testable.
pub fn format_line(record: &crate::record::trace_log::TraceRecord) -> String {
    let _ = record;
    todo!("src/cli/trace.rs")
}
