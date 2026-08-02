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

pub mod retention;
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
    pub(crate) tx: tokio::sync::mpsc::UnboundedSender<Entry>,
}

/// One queued write, or a request to prune.
///
/// Both records are boxed so neither variant forces every queued entry to pay
/// for the larger one.
pub enum Entry {
    Usage(Box<usage_log::UsageRecord>),
    Trace(Box<trace_log::TraceRecord>),
    /// Routed through the same channel as the writes rather than run from an
    /// independent task: pruning rewrites `usage-*.jsonl` in place, and doing
    /// that concurrently with this task's own `usage_log::append` (an
    /// unsynchronized `O_APPEND` write to the same file) can silently drop
    /// whatever line was appended mid-prune. One task processing everything
    /// in order makes that race structurally impossible within this process.
    Prune,
}

impl Recorder {
    /// Create the log directory and start the writer task.
    pub fn start(dir: PathBuf, mode: RecordMode) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Entry>();
        let writer_dir = dir.clone();
        tokio::spawn(async move {
            while let Some(entry) = rx.recv().await {
                match entry {
                    Entry::Usage(record) => {
                        if let Err(err) = usage_log::append(&writer_dir, &record) {
                            tracing::warn!(error = %err, "failed to write log record");
                        }
                    }
                    Entry::Trace(record) => {
                        if let Err(err) = trace_log::append(&writer_dir, &record) {
                            tracing::warn!(error = %err, "failed to write log record");
                        }
                    }
                    // Same task as the appends above, deliberately — see
                    // `Entry::Prune`.
                    Entry::Prune => match retention::prune(&writer_dir) {
                        Ok(summary) if summary.files_deleted > 0 || summary.files_trimmed > 0 => {
                            tracing::info!(
                                files_deleted = summary.files_deleted,
                                files_trimmed = summary.files_trimmed,
                                lines_removed = summary.lines_removed,
                                "pruned old logs"
                            );
                        }
                        Ok(_) => {}
                        Err(err) => tracing::warn!(error = %err, "failed to prune old logs"),
                    },
                }
            }
        });

        // Prune on startup, then once a day for as long as the server runs —
        // otherwise a long-lived `serve` would only ever shed old data on the
        // next restart. Requested through `tx` rather than run directly so it
        // always lands on the writer task, never concurrently with it.
        let prune_tx = tx.clone();
        tokio::spawn(async move {
            loop {
                if prune_tx.send(Entry::Prune).is_err() {
                    return; // writer task is gone; nothing left to prune for.
                }
                tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)).await;
            }
        });

        Ok(Arc::new(Self { mode, tx }))
    }

    pub fn mode(&self) -> RecordMode {
        self.mode
    }

    /// Queue a usage line. Dropped silently if usage recording is off.
    pub fn usage(&self, record: usage_log::UsageRecord) {
        if self.mode.usage {
            let _ = self.tx.send(Entry::Usage(Box::new(record)));
        }
    }

    /// Queue a trace line. Dropped silently unless `--debug` is on.
    pub fn trace(&self, record: trace_log::TraceRecord) {
        if self.mode.debug {
            let _ = self.tx.send(Entry::Trace(Box::new(record)));
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

    #[tokio::test]
    async fn queued_usage_record_is_written_by_the_background_task() {
        let dir = tempfile::tempdir().unwrap();
        let mode = RecordMode {
            usage: true,
            debug: false,
            debug_full: false,
        };
        let recorder = Recorder::start(dir.path().to_path_buf(), mode).unwrap();

        recorder.usage(usage_log::UsageRecord {
            ts: "2026-07-30T00:00:00Z".to_string(),
            client: "claude-code".to_string(),
            route: "claude-*".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            attempt: 1,
            in_tok: 1,
            out_tok: 2,
            cache_read_tok: 0,
            cache_write_tok: 0,
            usage_missing: false,
            dur_ms: 10,
            status: "success".to_string(),
            stream: false,
            error: None,
        });

        let now = time::OffsetDateTime::now_utc();
        let path = dir
            .path()
            .join(usage_log::file_name(now.year(), now.month() as u8));

        // The write happens on a background task; give it a moment to land.
        let mut contents = String::new();
        for _ in 0..100 {
            if let Ok(c) = std::fs::read_to_string(&path) {
                if !c.is_empty() {
                    contents = c;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let line = contents.lines().next().expect("usage record was written");
        let record: usage_log::UsageRecord = serde_json::from_str(line).unwrap();
        assert_eq!(record.in_tok, 1);
        assert_eq!(record.out_tok, 2);
    }

    #[tokio::test]
    async fn trace_disabled_by_mode_is_dropped_silently() {
        let dir = tempfile::tempdir().unwrap();
        let mode = RecordMode {
            usage: true,
            debug: false,
            debug_full: false,
        };
        let recorder = Recorder::start(dir.path().to_path_buf(), mode).unwrap();

        recorder.trace(trace_log::TraceRecord {
            ts: "2026-07-30T00:00:00Z".to_string(),
            req_id: "id".to_string(),
            client: "claude-code".to_string(),
            endpoint: "/v1/messages".to_string(),
            requested_model: "claude-sonnet-4-6".to_string(),
            input: trace_log::TraceInput {
                messages_n: 1,
                last_user_text: None,
                system_text: None,
                tokens_est: 1,
                tools: vec![],
                has_image: false,
                stream: false,
            },
            routing: trace_log::TraceRouting {
                mode: "explicit".to_string(),
                matched_route: "claude-*".to_string(),
                reason: "exact match".to_string(),
                candidates: vec![],
                score: None,
                threshold: None,
                embed_ms: None,
                decided_by_text: None,
                walk: None,
                system_score: None,
            },
            resolved: trace_log::TraceResolved {
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4-6".to_string(),
                api: "anthropic-messages".to_string(),
                translation: None,
            },
            attempts: vec![],
            usage: None,
        });

        // Give any (wrongly) queued write a moment to land, then confirm
        // nothing was ever written to disk.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let now = time::OffsetDateTime::now_utc();
        let path = dir.path().join(trace_log::file_name(
            now.year(),
            now.month() as u8,
            now.day(),
        ));
        assert!(!path.exists());
    }
}
