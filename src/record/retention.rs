//! Deleting old log data.
//!
//! `usage-YYYY-MM.jsonl` is monthly, so a file can straddle the retention
//! cutoff — old lines are filtered out of it in place, and the file is only
//! removed once every line in it is gone. `trace-YYYY-MM-DD.jsonl` is daily,
//! so the whole file is removed at once, keyed off the date already in its
//! name (no need to parse each line).
//!
//! Malformed lines are kept rather than dropped: a corrupt line already lost
//! whatever data it had, and pruning is not the place to lose more.

use std::path::Path;

use crate::error::Result;
use crate::record::usage_log::UsageRecord;

/// How long usage/trace data is kept before it is deleted.
pub const RETENTION_DAYS: i64 = 28;

/// What happened during one prune pass. Mostly useful for logging.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneSummary {
    pub files_deleted: u64,
    pub files_trimmed: u64,
    pub lines_removed: u64,
}

/// Delete anything older than [`RETENTION_DAYS`] under `dir`.
///
/// Best-effort: a single unreadable file does not stop the rest from being
/// pruned, since a stale log directory should not be able to wedge `stats` or
/// startup.
pub fn prune(dir: &Path) -> Result<PruneSummary> {
    let now = time::OffsetDateTime::now_utc();
    prune_at(dir, now, RETENTION_DAYS)
}

/// Same as [`prune`], but with an injectable "now" so it can be tested
/// without waiting for the calendar.
pub fn prune_at(dir: &Path, now: time::OffsetDateTime, keep_days: i64) -> Result<PruneSummary> {
    let mut summary = PruneSummary::default();

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Nothing written yet — nothing to prune.
        Err(_) => return Ok(summary),
    };

    let cutoff_day = cutoff_day(now, keep_days);

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if let Some(day) = name
            .strip_prefix("trace-")
            .and_then(|rest| rest.strip_suffix(".jsonl"))
        {
            if day < cutoff_day.as_str() && std::fs::remove_file(&path).is_ok() {
                summary.files_deleted += 1;
            }
        } else if name.starts_with("usage-") && name.ends_with(".jsonl") {
            if let Ok(outcome) = prune_usage_file(&path, &cutoff_day) {
                match outcome {
                    UsagePruneOutcome::Deleted(removed) => {
                        summary.files_deleted += 1;
                        summary.lines_removed += removed;
                    }
                    UsagePruneOutcome::Trimmed(removed) => {
                        summary.files_trimmed += 1;
                        summary.lines_removed += removed;
                    }
                    UsagePruneOutcome::Unchanged => {}
                }
            }
        }
    }

    Ok(summary)
}

/// `YYYY-MM-DD` of the oldest day still worth keeping.
fn cutoff_day(now: time::OffsetDateTime, keep_days: i64) -> String {
    let cutoff_date = now.date() - time::Duration::days(keep_days);
    format!(
        "{:04}-{:02}-{:02}",
        cutoff_date.year(),
        cutoff_date.month() as u8,
        cutoff_date.day()
    )
}

/// What happened to one usage file after considering its lines.
enum UsagePruneOutcome {
    /// Every line was stale — the whole file was removed.
    Deleted(u64),
    /// Some (but not all) lines were stale — the file was rewritten.
    Trimmed(u64),
    /// Nothing was old enough to remove.
    Unchanged,
}

/// Rewrite a usage file with lines older than `cutoff_day` removed.
///
/// The file is deleted outright if nothing would be left in it.
fn prune_usage_file(path: &Path, cutoff_day: &str) -> Result<UsagePruneOutcome> {
    let text = std::fs::read_to_string(path)?;

    let mut kept = Vec::new();
    let mut removed = 0u64;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<UsageRecord>(trimmed) {
            Ok(record) => {
                let day = record.ts.get(..10).unwrap_or(&record.ts);
                if day < cutoff_day {
                    removed += 1;
                } else {
                    kept.push(trimmed.to_string());
                }
            }
            // Can't tell how old a malformed line is — keep it.
            Err(_) => kept.push(trimmed.to_string()),
        }
    }

    if removed == 0 {
        return Ok(UsagePruneOutcome::Unchanged);
    }

    if kept.is_empty() {
        std::fs::remove_file(path)?;
        Ok(UsagePruneOutcome::Deleted(removed))
    } else {
        let mut contents = kept.join("\n");
        contents.push('\n');
        std::fs::write(path, contents)?;
        Ok(UsagePruneOutcome::Trimmed(removed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_lines(path: &Path, lines: &[&str]) {
        let mut file = std::fs::File::create(path).unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
    }

    fn usage_line(ts: &str) -> String {
        format!(
            r#"{{"ts":"{ts}","client":"c","route":"r","provider":"p","model":"m","attempt":1,"in_tok":1,"out_tok":1,"cache_read_tok":0,"cache_write_tok":0,"dur_ms":1,"status":"success","stream":false}}"#
        )
    }

    fn now() -> time::OffsetDateTime {
        // Fixed "now" so tests don't depend on the calendar.
        time::macros::datetime!(2026-08-29 00:00:00 UTC)
    }

    #[test]
    fn old_trace_files_are_deleted_and_recent_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("trace-2026-07-01.jsonl");
        let recent = dir.path().join("trace-2026-08-15.jsonl");
        write_lines(&old, &["{}"]);
        write_lines(&recent, &["{}"]);

        let summary = prune_at(dir.path(), now(), 28).unwrap();

        assert_eq!(summary.files_deleted, 1);
        assert!(!old.exists());
        assert!(recent.exists());
    }

    #[test]
    fn usage_file_keeps_recent_lines_and_drops_old_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-2026-08.jsonl");
        write_lines(
            &path,
            &[
                &usage_line("2026-07-01T00:00:00Z"), // older than 28 days from 2026-08-29
                &usage_line("2026-08-20T00:00:00Z"), // within 28 days
            ],
        );

        let summary = prune_at(dir.path(), now(), 28).unwrap();

        assert_eq!(summary.files_trimmed, 1);
        assert_eq!(summary.lines_removed, 1);
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("2026-08-20"));
    }

    #[test]
    fn usage_file_is_deleted_once_every_line_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-2026-01.jsonl");
        write_lines(&path, &[&usage_line("2026-01-01T00:00:00Z")]);

        let summary = prune_at(dir.path(), now(), 28).unwrap();

        assert_eq!(summary.files_deleted, 1);
        assert_eq!(summary.files_trimmed, 0);
        assert!(!path.exists());
    }

    #[test]
    fn malformed_lines_are_never_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-2026-01.jsonl");
        write_lines(&path, &["not json", &usage_line("2026-01-01T00:00:00Z")]);

        prune_at(dir.path(), now(), 28).unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert_eq!(contents.trim(), "not json");
    }

    #[test]
    fn untouched_files_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-2026-08.jsonl");
        write_lines(&path, &[&usage_line("2026-08-28T00:00:00Z")]);

        let summary = prune_at(dir.path(), now(), 28).unwrap();

        assert_eq!(summary, PruneSummary::default());
    }
}
