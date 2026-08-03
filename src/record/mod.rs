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
//! [`TRUNCATE_CHARS`] on their way into the file unless `--debug-full` was
//! given — see [`Recorder::trace`] for why the clip lives at the write
//! boundary rather than in the record.

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
    ///
    /// Truncation happens **here**, not where the record is built: a
    /// `TraceRecord` carries the prompt text in full so `serve --ui`'s live
    /// feed can show all of it (in memory, for as long as a tab is open),
    /// and clipping it to [`TRUNCATE_CHARS`] is a property of *this file on
    /// disk* rather than of the record. Doing it at the boundary keeps the
    /// two decisions independent — see the module docs on `crate::server::live`
    /// for why the dashboard is not gated on `--debug` at all.
    pub fn trace(&self, mut record: trace_log::TraceRecord) {
        if self.mode.debug {
            let limit = self.mode.truncate_at();
            if limit.is_some() {
                for text in [
                    &mut record.input.last_user_text,
                    &mut record.input.system_text,
                ]
                .into_iter()
                .flatten()
                {
                    *text = truncate(text, limit);
                }
            }
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

        recorder.trace(trace_record(None, None));

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

    fn trace_record(
        last_user_text: Option<&str>,
        system_text: Option<&str>,
    ) -> trace_log::TraceRecord {
        trace_log::TraceRecord {
            ts: "2026-07-30T00:00:00Z".to_string(),
            req_id: "id".to_string(),
            client: "claude-code".to_string(),
            endpoint: "/v1/messages".to_string(),
            requested_model: "claude-sonnet-4-6".to_string(),
            input: trace_log::TraceInput {
                messages_n: 1,
                last_user_text: last_user_text.map(String::from),
                system_text: system_text.map(String::from),
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
        }
    }

    /// Read back whatever `trace` wrote for today, waiting for the background
    /// writer to land it.
    async fn written_trace(dir: &std::path::Path) -> trace_log::TraceRecord {
        let now = time::OffsetDateTime::now_utc();
        let path = dir.join(trace_log::file_name(
            now.year(),
            now.month() as u8,
            now.day(),
        ));
        for _ in 0..100 {
            if let Ok(c) = std::fs::read_to_string(&path) {
                if let Some(line) = c.lines().next() {
                    return serde_json::from_str(line).unwrap();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("trace record was never written to {}", path.display());
    }

    /// The record itself carries prompt text in full (the live feed needs it
    /// that way); the clip is applied on the way to disk.
    #[tokio::test]
    async fn trace_text_is_truncated_on_its_way_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(
            dir.path().to_path_buf(),
            RecordMode {
                usage: false,
                debug: true,
                debug_full: false,
            },
        )
        .unwrap();

        let long = "a".repeat(TRUNCATE_CHARS + 100);
        recorder.trace(trace_record(Some(&long), Some(&long)));

        let record = written_trace(dir.path()).await;
        for text in [record.input.last_user_text, record.input.system_text] {
            let text = text.expect("both texts were recorded");
            assert_eq!(text.chars().count(), TRUNCATE_CHARS + 1); // + ellipsis
            assert!(text.ends_with('…'));
        }
    }

    #[tokio::test]
    async fn debug_full_writes_trace_text_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Recorder::start(
            dir.path().to_path_buf(),
            RecordMode {
                usage: false,
                debug: true,
                debug_full: true,
            },
        )
        .unwrap();

        let long = "a".repeat(TRUNCATE_CHARS + 100);
        recorder.trace(trace_record(Some(&long), None));

        let record = written_trace(dir.path()).await;
        assert_eq!(record.input.last_user_text.as_deref(), Some(long.as_str()));
    }

    fn usage_record(ts: String, in_tok: u64) -> usage_log::UsageRecord {
        usage_log::UsageRecord {
            ts,
            client: "claude-code".to_string(),
            route: "claude-*".to_string(),
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            attempt: 1,
            in_tok,
            out_tok: 1,
            cache_read_tok: 0,
            cache_write_tok: 0,
            usage_missing: false,
            dur_ms: 1,
            status: "success".to_string(),
            stream: false,
            error: None,
        }
    }

    /// #20: `stats` used to run its own `retention::prune`, a second writer
    /// racing `serve`'s appends across process boundaries. The fix routes
    /// `serve`'s startup prune through the very same channel/task as every
    /// `usage()` append (see the comment on `Entry::Prune` above) — this
    /// proves that wiring actually holds, by queuing appends immediately
    /// after `Recorder::start` (before the startup `Entry::Prune`, sent by a
    /// separate task, is guaranteed to have reached the writer) and checking
    /// none of them are lost to it. If the startup prune instead ran
    /// independently — a plain `tokio::spawn(retention::prune(...))`, say —
    /// its read-modify-write could race one of these appends and silently
    /// drop it, exactly the bug #20 describes.
    #[tokio::test]
    async fn startup_prune_is_serialized_with_appends_through_the_same_task() {
        let dir = tempfile::tempdir().unwrap();
        let mode = RecordMode {
            usage: true,
            debug: false,
            debug_full: false,
        };

        let now = time::OffsetDateTime::now_utc();
        let path = dir
            .path()
            .join(usage_log::file_name(now.year(), now.month() as u8));

        // A stale line for the startup prune to actually have work to do —
        // otherwise a no-op prune would pass this test for the wrong reason.
        let stale = usage_record("2000-01-01T00:00:00Z".to_string(), 999);
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&stale).unwrap()),
        )
        .unwrap();

        let recorder = Recorder::start(dir.path().to_path_buf(), mode).unwrap();

        const APPENDS: usize = 20;
        let ts = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        for i in 0..APPENDS {
            recorder.usage(usage_record(ts.clone(), i as u64));
        }

        // Poll until the writer task has drained the startup prune and every
        // append queued above.
        let mut contents = String::new();
        for _ in 0..200 {
            if let Ok(c) = std::fs::read_to_string(&path) {
                if c.lines().count() >= APPENDS {
                    contents = c;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(
            contents.lines().count(),
            APPENDS,
            "an append was lost to the startup prune instead of being serialized after it"
        );
        assert!(
            !contents.contains("2000-01-01"),
            "the startup prune did not remove the stale line"
        );
    }
}
