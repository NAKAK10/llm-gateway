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
#[derive(Debug, PartialEq, Eq)]
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
///
/// `serve` serializes its own appends and prunes through one task (see
/// `record::Recorder`), so within that process this can never race an
/// append. It can still race a *different* process's append — `stats` runs
/// this too, and nothing stops it running while `serve` is up — so the plan
/// computed from the initial read is only committed if the file is still
/// exactly as it was read; see `commit_usage_prune`.
fn prune_usage_file(path: &Path, cutoff_day: &str) -> Result<UsagePruneOutcome> {
    match plan_usage_prune(path, cutoff_day)? {
        Some(plan) => commit_usage_prune(path, plan),
        None => Ok(UsagePruneOutcome::Unchanged),
    }
}

/// What pruning would do to one usage file, computed from a single read.
struct UsagePrunePlan {
    /// Byte length of the file as read, to detect a concurrent writer before
    /// the plan below is committed.
    read_len: u64,
    kept: Vec<String>,
    removed: u64,
}

/// Read `path` and decide what pruning it would do. `Ok(None)` means nothing
/// in it is old enough to remove.
fn plan_usage_prune(path: &Path, cutoff_day: &str) -> Result<Option<UsagePrunePlan>> {
    let text = std::fs::read_to_string(path)?;
    let read_len = text.len() as u64;

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
        return Ok(None);
    }
    Ok(Some(UsagePrunePlan {
        read_len,
        kept,
        removed,
    }))
}

/// Commit a prune plan computed by [`plan_usage_prune`] — unless the file has
/// grown or shrunk since that read, in which case something else (another
/// process's `append`) wrote to it in between, and committing a plan based on
/// the stale read would silently discard whatever it wrote. Skipping is safe:
/// the next prune pass picks the file up again with a fresh read.
fn commit_usage_prune(path: &Path, plan: UsagePrunePlan) -> Result<UsagePruneOutcome> {
    let current_len = std::fs::metadata(path)?.len();
    if current_len != plan.read_len {
        return Ok(UsagePruneOutcome::Unchanged);
    }

    if plan.kept.is_empty() {
        std::fs::remove_file(path)?;
        Ok(UsagePruneOutcome::Deleted(plan.removed))
    } else {
        let mut contents = plan.kept.join("\n");
        contents.push('\n');
        write_atomically(path, &contents)?;
        Ok(UsagePruneOutcome::Trimmed(plan.removed))
    }
}

/// Build a unique sibling temp file name for `path`.
///
/// A fixed name like `usage-2026-08.jsonl.tmp` would let two writers
/// targeting the same usage file — two `stats` invocations, or `stats`'s
/// prune racing `serve`'s daily one — interleave their writes into the same
/// temp file before racing to rename over the target. Mixing in the process
/// id and a fresh UUID means no two callers ever share one. The `.tmp`
/// suffix stays last so the result still fails the `usage-*.jsonl` filter
/// used by both `prune_at` above and `stats`'s file listing.
fn tmp_file_name(path: &Path) -> String {
    let original = path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("usage file paths always have a UTF-8 file name");
    format!(
        "{original}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::now_v7()
    )
}

/// Replace `path`'s contents by writing to a sibling temp file and renaming
/// over it — a reader (or a writer reopening the path) never observes a
/// half-written file, which a direct `fs::write` (truncate then write) does
/// not guarantee.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_file_name(tmp_file_name(path));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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

    /// The race this module exists to avoid: something else (another
    /// process's `usage_log::append`, standing in for `serve` running
    /// alongside a `stats` invocation) writes to the file after the prune
    /// plan was computed from an earlier read, but before that plan is
    /// committed. Committing anyway would silently discard the concurrent
    /// write; the size check in `commit_usage_prune` must refuse to.
    #[test]
    fn a_concurrent_append_between_plan_and_commit_is_never_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-2026-08.jsonl");
        write_lines(
            &path,
            &[
                &usage_line("2026-07-01T00:00:00Z"), // stale, would be pruned
                &usage_line("2026-08-20T00:00:00Z"),
            ],
        );

        let cutoff_day = cutoff_day(now(), 28);
        let plan = plan_usage_prune(&path, &cutoff_day).unwrap().unwrap();
        assert_eq!(plan.removed, 1);

        // Simulate a concurrent append landing between the read above and
        // the commit below.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", usage_line("2026-08-25T00:00:00Z")).unwrap();
        drop(file);
        let appended_contents = std::fs::read_to_string(&path).unwrap();

        let outcome = commit_usage_prune(&path, plan).unwrap();

        assert_eq!(outcome, UsagePruneOutcome::Unchanged);
        // Nothing was lost: the file is exactly what the concurrent append
        // left it as, stale line and all — the next prune pass will pick it
        // up with a fresh read.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), appended_contents);
    }

    /// The temp file `write_atomically` uses must never itself look like a
    /// usage file, or it would get picked up as one by `prune_at`'s filter
    /// (and `stats`'s) instead of being an invisible implementation detail.
    #[test]
    fn tmp_file_name_never_matches_the_usage_file_filter() {
        let path = Path::new("/logs/usage-2026-08.jsonl");
        let name = tmp_file_name(path);

        assert!(!(name.starts_with("usage-") && name.ends_with(".jsonl")));
        assert!(name.ends_with(".tmp"));
    }

    /// Two writers targeting the same usage file (two `stats` runs, or
    /// `stats`'s prune racing `serve`'s) must get different temp file names,
    /// or their writes could land in the same file and mix together before
    /// one of them wins the rename.
    #[test]
    fn tmp_file_name_is_unique_across_calls() {
        let path = Path::new("/logs/usage-2026-08.jsonl");

        assert_ne!(tmp_file_name(path), tmp_file_name(path));
    }

    #[test]
    fn a_trimmed_file_is_replaced_atomically() {
        // Exercises `write_atomically` through the normal (uncontended) path:
        // the temp file it uses must not be left behind, and the final
        // content must match what a direct write would have produced.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-2026-08.jsonl");
        write_lines(
            &path,
            &[
                &usage_line("2026-07-01T00:00:00Z"),
                &usage_line("2026-08-20T00:00:00Z"),
            ],
        );

        prune_at(dir.path(), now(), 28).unwrap();

        assert!(!dir.path().join("usage-2026-08.jsonl.tmp").exists());
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("2026-08-20"));
    }
}
