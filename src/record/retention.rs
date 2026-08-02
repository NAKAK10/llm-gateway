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
/// pruned, since a stale log directory should not be able to wedge `serve`'s
/// startup or its daily prune pass — the only caller now that `stats` is
/// read-only (#20).
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
    /// Every line was stale — the file was emptied (not unlinked; see
    /// `commit_usage_prune`).
    Deleted(u64),
    /// Some (but not all) lines were stale — the file was rewritten.
    Trimmed(u64),
    /// Nothing was old enough to remove.
    Unchanged,
}

/// Rewrite a usage file with lines older than `cutoff_day` removed.
///
/// The file is emptied (not unlinked) if nothing would be left in it — see
/// `commit_usage_prune`.
///
/// `serve` is the only writer of `usage-*.jsonl`: `stats` used to prune too
/// (#15), which raced `serve`'s own append across process boundaries with no
/// lock between them (#20). Now `stats` only reads, and `serve` serializes
/// its own appends and prunes through one task (see `record::Recorder`), so
/// this function only ever runs on that task and can never race an append
/// against the same file. The `read_len`/`current_len` check in
/// `commit_usage_prune` is kept anyway as a defense-in-depth belt-and-braces
/// check — cheap, and it costs nothing to keep catching a future caller that
/// reintroduces a second writer.
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
/// grown or shrunk since that read, in which case something wrote to it in
/// between, and committing a plan based on the stale read would silently
/// discard whatever it wrote. Nothing should be able to trigger that anymore
/// now that `serve`'s `Recorder` task is the only writer of `usage-*.jsonl`
/// and serializes its own prunes against its own appends (see
/// `prune_usage_file`'s doc comment), but the check costs one `metadata`
/// call and is cheap insurance against a future caller reintroducing a
/// second writer. Skipping is safe either way: the next prune pass picks the
/// file up again with a fresh read.
fn commit_usage_prune(path: &Path, plan: UsagePrunePlan) -> Result<UsagePruneOutcome> {
    let current_len = std::fs::metadata(path)?.len();
    if current_len != plan.read_len {
        return Ok(UsagePruneOutcome::Unchanged);
    }

    if plan.kept.is_empty() {
        // `usage_log::append` reopens the file by path on every call rather
        // than holding a handle open, so unlinking here (as this used to)
        // would not lose an in-flight write — but it would still leave a
        // moment where the path does not exist at all (between the unlink
        // and whichever append next recreates it with `create(true)`), and
        // it makes this branch a special case with its own failure mode
        // instead of sharing `Trimmed`'s. Writing empty content through the
        // same `write_atomically` path both closes that gap and keeps one
        // atomic-replace code path for both outcomes.
        write_atomically(path, "")?;
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
/// `stats` no longer prunes (#20) and `serve`'s own prunes are serialized
/// through one task, so nothing should ever call this twice for the same
/// `path` concurrently. A fixed name like `usage-2026-08.jsonl.tmp` would be
/// fine under that guarantee alone, but mixing in the process id and a fresh
/// UUID is cheap insurance if that guarantee is ever broken — two writers
/// interleaving into the same temp file before racing to rename over the
/// target would be a much worse failure mode than a merely-redundant unique
/// name. The `.tmp` suffix stays last so the result still fails the
/// `usage-*.jsonl` filter used by both `prune_at` above and `stats`'s file
/// listing.
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
///
/// The temp file is `sync_all`'d before the rename so its data is on disk
/// before the rename's metadata can be — without that, a crash right after
/// the rename lands could leave `path` pointing at a temp file whose content
/// never made it past the page cache, i.e. a usage log truncated to zero (or
/// garbage) instead of either its old or new contents. The rename itself is
/// not further fsync'd (nor is the parent directory's own fsync taken):
/// durability of the rename operation and directory-entry fsync semantics
/// are both filesystem- and platform-dependent enough (notably on macOS,
/// which this project also runs on) that chasing them here would trade a
/// small crash window for real portability risk, for a usage log where an
/// unlucky crash losing the very last prune is an acceptable trade — the
/// data it prunes is, by definition, already past retention.
fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_file_name(tmp_file_name(path));
    let file = std::fs::File::create(&tmp)?;
    // `File::create` truncates but does not seek; `contents` is written
    // through the same handle we're about to sync so there's no window
    // where a separate open could see a stale (or empty) temp file.
    write_and_sync(&file, contents).inspect_err(|_| {
        // Best-effort: if the write or sync failed, don't leave the temp
        // file behind for the next prune pass to trip over.
        let _ = std::fs::remove_file(&tmp);
    })?;
    drop(file);

    if let Err(err) = std::fs::rename(&tmp, path) {
        // Don't let a failed rename leave `.pid.uuid.tmp` litter behind —
        // nothing else will ever clean it up, since its name deliberately
        // never matches the `usage-*.jsonl` filter `prune_at` and `stats`
        // both use.
        let _ = std::fs::remove_file(&tmp);
        return Err(err.into());
    }
    Ok(())
}

/// Write `contents` to `file` and `sync_all` it before returning — split out
/// of `write_atomically` so the temp-file cleanup there can wrap both
/// fallible steps with one `inspect_err`.
fn write_and_sync(mut file: &std::fs::File, contents: &str) -> Result<()> {
    use std::io::Write;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
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
    fn usage_file_is_emptied_not_unlinked_once_every_line_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-2026-01.jsonl");
        write_lines(&path, &[&usage_line("2026-01-01T00:00:00Z")]);

        let summary = prune_at(dir.path(), now(), 28).unwrap();

        assert_eq!(summary.files_deleted, 1);
        assert_eq!(summary.files_trimmed, 0);
        // The file itself must still be there — see the `Deleted` branch of
        // `commit_usage_prune` for why unlinking it would be unsafe.
        assert!(path.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    }

    /// #20: unlinking the file (the old behavior) leaves a `serve` writer
    /// with nothing to append to at that path until it happens to recreate
    /// it. Emptying the file in place instead must mean an append landing
    /// right after a full prune is readable afterward, with no gap where the
    /// path does not exist.
    #[test]
    fn a_file_emptied_by_pruning_still_accepts_and_shows_later_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage-2026-01.jsonl");
        write_lines(&path, &[&usage_line("2026-01-01T00:00:00Z")]);

        let summary = prune_at(dir.path(), now(), 28).unwrap();
        assert_eq!(summary.files_deleted, 1);
        assert!(path.exists());

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", usage_line("2026-08-25T00:00:00Z")).unwrap();
        drop(file);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 1);
        assert!(contents.contains("2026-08-25"));
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

    /// The race this module used to be exposed to (#20, before `stats`
    /// became read-only): something else writes to the file after the prune
    /// plan was computed from an earlier read, but before that plan is
    /// committed. Committing anyway would silently discard the concurrent
    /// write; the size check in `commit_usage_prune` must refuse to. Kept as
    /// a regression test for that defense-in-depth check even though nothing
    /// should be able to trigger it in practice anymore.
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

    /// Even though nothing should call this twice concurrently for the same
    /// `path` anymore (see `tmp_file_name`'s doc comment), two calls must
    /// still get different temp file names, or their writes could land in
    /// the same file and mix together before one of them wins the rename.
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
