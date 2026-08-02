//! `llm-gateway stats` — what was spent, and on what.
//!
//! Reads `usage-*.jsonl` and aggregates. Token counts are always meaningful;
//! money is not always available, because a flat-rate subscription has no
//! per-token price. So the table always shows tokens and shows cost only when
//! something computed one.
//!
//! Records are stamped in UTC (see `record::usage_log`), but day boundaries
//! for grouping and `--since`/`--until` are shown and filtered in the local
//! timezone — otherwise a JST evening looks like it happened "the next day".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::ValueEnum;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths;
use crate::record::usage_log::UsageRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GroupBy {
    Route,
    Client,
    Provider,
    Model,
    Day,
}

impl std::fmt::Display for GroupBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Route => "route",
            Self::Client => "client",
            Self::Provider => "provider",
            Self::Model => "model",
            Self::Day => "day",
        };
        f.write_str(s)
    }
}

pub struct Options {
    pub by: GroupBy,
    /// `YYYY-MM-DD`, inclusive.
    pub since: Option<String>,
    /// `YYYY-MM-DD`, inclusive.
    pub until: Option<String>,
    /// Show every retained record instead of defaulting to the last 7 days.
    pub all: bool,
}

/// One aggregated row.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Row {
    pub key: String,
    pub calls: u64,
    pub failures: u64,
    /// `success` calls whose usage could not be extracted at all — a
    /// missing/unparseable upstream `usage`, not a genuine 0-token response.
    /// Counted separately so it never silently hides inside `in_tok`/`out_tok`
    /// totals of 0. See `UsageRecord::usage_missing`.
    pub unknown: u64,
    pub in_tok: u64,
    pub out_tok: u64,
    pub cache_read_tok: u64,
    pub cache_write_tok: u64,
}

impl Row {
    /// Every token counted against this row, cache included. Prompt caching
    /// clients (Claude Code, above all) put most of their real usage in
    /// `cache_read_tok`/`cache_write_tok`, so a total that ignores them makes
    /// heavy cache users look nearly idle.
    pub fn total_tokens(&self) -> u64 {
        self.in_tok + self.out_tok + self.cache_read_tok + self.cache_write_tok
    }
}

pub fn run(options: Options) -> Result<()> {
    // Validate before touching disk: a typo'd `--since` should be reported
    // as exactly that, not silently exclude or include everything (a bare
    // string comparison against `YYYY-MM-DD` day keys would do either
    // depending on the typo, with no indication anything was wrong).
    validate_date_range(options.since.as_deref(), options.until.as_deref())?;

    // `stats` displays and filters by calendar day in local time — records
    // are stamped in UTC, so without this a JST evening (or any zone ahead
    // of UTC) shows up as "yesterday". `current_local_offset` is safe to
    // call here: `stats` is a single-threaded CLI invocation that has not
    // spawned any threads yet.
    let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);

    // `Config::load()` also validates, and a broken config should not stop
    // `stats` from reading logs that already exist — fall back to the
    // default logs directory rather than erroring out.
    let logs_dir = Config::read(&paths::config_file())
        .ok()
        .map(|c| paths::logs_dir(&c.logging.dir))
        .unwrap_or_else(|| PathBuf::from("./logs"));

    // `stats` used to prune here too, but that made it a second writer to
    // `usage-*.jsonl` racing `serve`'s own append/prune task (#20) — the
    // length check in `retention::commit_usage_prune` narrowed that race's
    // window but could not close it, since `stats` and `serve` are separate
    // processes with no lock between them. `stats` is now read-only:
    // `serve`'s `Recorder` prunes on startup and once a day (see
    // `record::mod`), which is the only writer of `usage-*.jsonl`, so there
    // is nothing left to race. A machine that only ever runs `stats` (never
    // `serve`) simply keeps its logs unpruned — acceptable, since retention
    // exists to bound a long-running server's disk use, not a CLI's.
    let (records, skipped) = read_usage_records(&logs_dir);

    if skipped > 0 {
        eprintln!("skipped {skipped} malformed lines");
    }

    if records.is_empty() {
        println!("no usage records found in {}", logs_dir.display());
        return Ok(());
    }

    // With no explicit range and no `--all`, default to the last 7 days —
    // that's what "how much did I just use" almost always means, and
    // `--since`/`--until`/`--all` remain the escape hatch for anything else.
    let default_since = default_since_7_days(offset);
    let since = if options.all {
        None
    } else {
        options.since.clone().or_else(|| default_since.clone())
    };
    let until = if options.all {
        None
    } else {
        options.until.clone()
    };

    if since.is_some() || until.is_some() {
        let label = match (&since, &until) {
            (Some(s), Some(u)) => format!("{s}..{u}"),
            (Some(s), None) => format!("since {s}"),
            (None, Some(u)) => format!("until {u}"),
            (None, None) => unreachable!(),
        };
        println!(
            "showing {label} (use --all for everything retained), dates in local time ({})",
            format_offset(offset)
        );
    } else {
        println!("dates in local time ({})", format_offset(offset));
    }

    let rows = aggregate(
        &records,
        options.by,
        since.as_deref(),
        until.as_deref(),
        offset,
    );
    print_table(&options.by.to_string(), &rows);

    // Also show a per-model breakdown, since that's usually what people
    // want to know ("how much did each model actually get used") and it's
    // easy to miss when the main table is grouped by something else.
    if options.by != GroupBy::Model {
        let model_rows = aggregate(
            &records,
            GroupBy::Model,
            since.as_deref(),
            until.as_deref(),
            offset,
        );
        println!();
        print_table("model", &model_rows);
    }

    Ok(())
}

/// Read every `usage-*.jsonl` file in `logs_dir`, oldest month first.
///
/// Shared between the `stats` CLI and `serve --ui`'s `GET /api/usage` — both
/// want the same "read what's on disk, tolerate what's missing or
/// unreadable" behavior. Returns the parsed records plus a count of lines
/// that failed to parse (kept rather than logged here, since a CLI caller
/// wants that on stderr and an HTTP caller does not).
pub fn read_usage_records(logs_dir: &Path) -> (Vec<UsageRecord>, u64) {
    let mut records = Vec::new();
    let mut skipped = 0u64;

    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return (records, skipped);
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("usage-") && name.ends_with(".jsonl"))
                .unwrap_or(false)
        })
        .collect();
    paths.sort();

    for path in paths {
        // One unreadable file should not abort reading the rest — the
        // directory listing above already tolerates unreadable entries the
        // same way; a file that vanishes or loses permissions between
        // listing and reading deserves the same treatment.
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<UsageRecord>(line) {
                Ok(record) => records.push(record),
                Err(_) => skipped += 1,
            }
        }
    }

    (records, skipped)
}

/// Render one grouped table plus its `TOTAL` row, sharing the same shape for
/// both the primary table and the per-model breakdown.
fn print_table(header_key: &str, rows: &[Row]) {
    let mut table = comfy_table::Table::new();
    table.set_header(vec![
        header_key.to_string(),
        "calls".to_string(),
        "fail".to_string(),
        "unk".to_string(),
        "in_tok".to_string(),
        "out_tok".to_string(),
        "cache_r".to_string(),
        "cache_w".to_string(),
        "total".to_string(),
    ]);

    let mut total = Row {
        key: "TOTAL".to_string(),
        ..Default::default()
    };
    for row in rows {
        table.add_row(vec![
            row.key.clone(),
            format_count(row.calls),
            format_count(row.failures),
            format_count(row.unknown),
            format_count(row.in_tok),
            format_count(row.out_tok),
            format_count(row.cache_read_tok),
            format_count(row.cache_write_tok),
            format_count(row.total_tokens()),
        ]);
        total.calls += row.calls;
        total.failures += row.failures;
        total.unknown += row.unknown;
        total.in_tok += row.in_tok;
        total.out_tok += row.out_tok;
        total.cache_read_tok += row.cache_read_tok;
        total.cache_write_tok += row.cache_write_tok;
    }
    table.add_row(vec![
        total.key.clone(),
        format_count(total.calls),
        format_count(total.failures),
        format_count(total.unknown),
        format_count(total.in_tok),
        format_count(total.out_tok),
        format_count(total.cache_read_tok),
        format_count(total.cache_write_tok),
        format_count(total.total_tokens()),
    ]);

    println!("{table}");
}

/// Render a UTC offset as `UTC+09:00`, the conventional hours:minutes form.
///
/// `time::UtcOffset`'s own `Display` always prints seconds too (`+09:00:00`),
/// because the type can represent second-precision offsets — a handful of
/// zones really did use one before international standardization (e.g. the
/// Netherlands used UTC+00:19:32 until 1937). No offset in modern use has a
/// non-zero seconds component, so the common case is trimmed to `HH:MM` for
/// readability; on the rare chance one shows up (a historical `TZ` set on
/// purpose, say), the seconds are kept rather than silently rounded away.
fn format_offset(offset: time::UtcOffset) -> String {
    let (hours, minutes, seconds) = offset.as_hms();
    let sign = if hours < 0 || minutes < 0 || seconds < 0 {
        '-'
    } else {
        '+'
    };
    if seconds == 0 {
        format!("UTC{sign}{:02}:{:02}", hours.abs(), minutes.abs())
    } else {
        format!(
            "UTC{sign}{:02}:{:02}:{:02}",
            hours.abs(),
            minutes.abs(),
            seconds.abs()
        )
    }
}

/// Parse a `--since`/`--until` value as `YYYY-MM-DD`.
fn parse_date(s: &str) -> Result<time::Date> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    time::Date::parse(s, &format)
        .map_err(|_| Error::Other(format!("invalid date `{s}` (expected YYYY-MM-DD)")))
}

/// Validate `--since`/`--until` are each well-formed `YYYY-MM-DD`, and — if
/// both are given — that `since` does not come after `until`. A reversed
/// range is almost certainly a typo, not an intentionally empty one; left
/// unchecked it silently produces a table with zero rows and no indication
/// anything was wrong (the day-key comparison in `aggregate` is a plain
/// string compare, which would just never match anything).
fn validate_date_range(since: Option<&str>, until: Option<&str>) -> Result<()> {
    let since_date = since.map(parse_date).transpose()?;
    let until_date = until.map(parse_date).transpose()?;
    if let (Some(s), Some(u)) = (since_date, until_date) {
        if s > u {
            return Err(Error::Other(format!(
                "--since `{}` is after --until `{}` (expected --since <= --until)",
                since.unwrap_or_default(),
                until.unwrap_or_default(),
            )));
        }
    }
    Ok(())
}

/// Aggregate already-parsed records. Pure, so it can be tested directly.
///
/// `offset` decides which calendar day each record's (UTC-stamped) timestamp
/// falls on, for both grouping by day and `since`/`until` filtering.
pub fn aggregate(
    records: &[UsageRecord],
    by: GroupBy,
    since: Option<&str>,
    until: Option<&str>,
    offset: time::UtcOffset,
) -> Vec<Row> {
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();

    for record in records {
        let day = day_of(&record.ts, offset);
        if let Some(since) = since {
            if day.as_str() < since {
                continue;
            }
        }
        if let Some(until) = until {
            if day.as_str() > until {
                continue;
            }
        }

        let key = match by {
            GroupBy::Route => record.route.clone(),
            GroupBy::Client => record.client.clone(),
            GroupBy::Provider => record.provider.clone(),
            GroupBy::Model => record.model.clone(),
            GroupBy::Day => day.clone(),
        };

        let row = rows.entry(key.clone()).or_insert_with(|| Row {
            key,
            ..Default::default()
        });
        row.calls += 1;
        if record.status != "success" {
            row.failures += 1;
        }
        if record.usage_missing {
            row.unknown += 1;
        }
        row.in_tok += record.in_tok;
        row.out_tok += record.out_tok;
        row.cache_read_tok += record.cache_read_tok;
        row.cache_write_tok += record.cache_write_tok;
    }

    let mut result: Vec<Row> = rows.into_values().collect();
    match by {
        // Day is a timeline, so it reads best in chronological order.
        GroupBy::Day => result.sort_by(|a, b| a.key.cmp(&b.key)),
        // Everything else reads best biggest-first, cache tokens included —
        // a prompt-caching-heavy client can have most of its real usage
        // there, and `in_tok + out_tok` alone would rank it as nearly idle.
        _ => result.sort_by_key(|row| std::cmp::Reverse(row.total_tokens())),
    }
    result
}

/// The `YYYY-MM-DD` calendar day an RFC 3339 UTC timestamp falls on in
/// `offset`. Falls back to the raw leading 10 characters if `ts` does not
/// parse as RFC 3339 — a record from a future schema version should still
/// aggregate into *some* day rather than being dropped.
fn day_of(ts: &str, offset: time::UtcOffset) -> String {
    match time::OffsetDateTime::parse(ts, &time::format_description::well_known::Rfc3339) {
        Ok(dt) => {
            let local = dt.to_offset(offset).date();
            format!(
                "{:04}-{:02}-{:02}",
                local.year(),
                local.month() as u8,
                local.day()
            )
        }
        Err(_) => ts.get(..10).unwrap_or(ts).to_string(),
    }
}

/// `YYYY-MM-DD` 7 days ago in `offset` (inclusive of today), for the default
/// `stats` window.
fn default_since_7_days(offset: time::UtcOffset) -> Option<String> {
    let today = time::OffsetDateTime::now_utc().to_offset(offset).date();
    let since = today - time::Duration::days(6);
    Some(format!(
        "{:04}-{:02}-{:02}",
        since.year(),
        since.month() as u8,
        since.day()
    ))
}

/// Render with `,` every three digits, e.g. `12,345`.
fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const UTC: time::UtcOffset = time::UtcOffset::UTC;

    fn record(
        ts: &str,
        route: &str,
        provider: &str,
        model: &str,
        status: &str,
        in_tok: u64,
        out_tok: u64,
    ) -> UsageRecord {
        UsageRecord {
            ts: ts.to_string(),
            client: "claude-code".to_string(),
            route: route.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            attempt: 1,
            in_tok,
            out_tok,
            cache_read_tok: 0,
            cache_write_tok: 0,
            usage_missing: false,
            dur_ms: 100,
            status: status.to_string(),
            stream: false,
            error: None,
        }
    }

    #[test]
    fn groups_by_the_requested_key() {
        let records = vec![
            record(
                "2026-08-01T00:00:00Z",
                "claude-*",
                "anthropic",
                "claude-sonnet-4-6",
                "success",
                10,
                20,
            ),
            record(
                "2026-08-01T01:00:00Z",
                "claude-*",
                "anthropic",
                "claude-sonnet-4-6",
                "success",
                5,
                5,
            ),
            record(
                "2026-08-01T02:00:00Z",
                "gpt-*",
                "openai",
                "gpt-5.6",
                "success",
                1,
                1,
            ),
        ];

        let rows = aggregate(&records, GroupBy::Route, None, None, UTC);

        assert_eq!(rows.len(), 2);
        let claude = rows.iter().find(|r| r.key == "claude-*").unwrap();
        assert_eq!(claude.calls, 2);
        assert_eq!(claude.in_tok, 15);
        assert_eq!(claude.out_tok, 25);
    }

    #[test]
    fn since_and_until_bounds_are_inclusive() {
        let records = vec![
            record("2026-08-01T00:00:00Z", "r", "p", "m", "success", 1, 1),
            record("2026-08-02T00:00:00Z", "r", "p", "m", "success", 1, 1),
            record("2026-08-03T00:00:00Z", "r", "p", "m", "success", 1, 1),
        ];

        let rows = aggregate(
            &records,
            GroupBy::Day,
            Some("2026-08-01"),
            Some("2026-08-02"),
            UTC,
        );
        let calls: u64 = rows.iter().map(|r| r.calls).sum();
        assert_eq!(calls, 2);

        let all = aggregate(
            &records,
            GroupBy::Day,
            Some("2026-08-01"),
            Some("2026-08-03"),
            UTC,
        );
        let calls: u64 = all.iter().map(|r| r.calls).sum();
        assert_eq!(calls, 3);
    }

    #[test]
    fn non_success_status_counts_as_a_failure() {
        let records = vec![
            record("2026-08-01T00:00:00Z", "r", "p", "m", "success", 1, 1),
            record("2026-08-01T00:00:01Z", "r", "p", "m", "error", 1, 1),
            record("2026-08-01T00:00:02Z", "r", "p", "m", "aborted", 1, 1),
        ];

        let rows = aggregate(&records, GroupBy::Route, None, None, UTC);
        assert_eq!(rows[0].calls, 3);
        assert_eq!(rows[0].failures, 2);
    }

    #[test]
    fn usage_missing_is_counted_separately_from_a_failure() {
        let records = vec![
            record("2026-08-01T00:00:00Z", "r", "p", "m", "success", 1, 1),
            {
                let mut r = record("2026-08-01T00:00:01Z", "r", "p", "m", "success", 0, 0);
                r.usage_missing = true;
                r
            },
        ];

        let rows = aggregate(&records, GroupBy::Route, None, None, UTC);
        assert_eq!(rows[0].calls, 2);
        assert_eq!(rows[0].failures, 0);
        assert_eq!(rows[0].unknown, 1);
    }

    #[test]
    fn default_grouping_sorts_by_total_tokens_descending() {
        let records = vec![
            record("2026-08-01T00:00:00Z", "small", "p", "m", "success", 1, 1),
            record("2026-08-01T00:00:00Z", "big", "p", "m", "success", 100, 100),
            record(
                "2026-08-01T00:00:00Z",
                "medium",
                "p",
                "m",
                "success",
                10,
                10,
            ),
        ];

        let rows = aggregate(&records, GroupBy::Route, None, None, UTC);
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["big", "medium", "small"]);
    }

    /// A prompt-caching client can put nearly all of its real usage in cache
    /// reads, with `in_tok`/`out_tok` almost zero — sorting on those alone
    /// would rank it below a route that barely used cache at all.
    #[test]
    fn default_grouping_sort_includes_cache_tokens() {
        let mut cache_heavy = record(
            "2026-08-01T00:00:00Z",
            "cache-heavy",
            "p",
            "m",
            "success",
            5,
            5,
        );
        cache_heavy.cache_read_tok = 1_000_000;
        let plain = record(
            "2026-08-01T00:00:00Z",
            "plain",
            "p",
            "m",
            "success",
            100,
            100,
        );

        let rows = aggregate(&[cache_heavy, plain], GroupBy::Route, None, None, UTC);
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["cache-heavy", "plain"]);
    }

    #[test]
    fn day_grouping_sorts_chronologically() {
        let records = vec![
            record("2026-08-03T00:00:00Z", "r", "p", "m", "success", 1, 1),
            record("2026-08-01T00:00:00Z", "r", "p", "m", "success", 100, 100),
            record("2026-08-02T00:00:00Z", "r", "p", "m", "success", 10, 10),
        ];

        let rows = aggregate(&records, GroupBy::Day, None, None, UTC);
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["2026-08-01", "2026-08-02", "2026-08-03"]);
    }

    /// A record stamped just after UTC midnight falls on the *previous*
    /// calendar day in a timezone ahead of UTC (e.g. JST, UTC+9) — the whole
    /// point of converting before grouping.
    #[test]
    fn day_grouping_uses_the_given_offset() {
        let records = vec![record(
            "2026-08-02T00:30:00Z",
            "r",
            "p",
            "m",
            "success",
            1,
            1,
        )];

        let jst = time::UtcOffset::from_hms(9, 0, 0).unwrap();
        let rows = aggregate(&records, GroupBy::Day, None, None, jst);
        assert_eq!(rows[0].key, "2026-08-02"); // 09:30 JST, still Aug 2nd

        let rows_utc = aggregate(&records, GroupBy::Day, None, None, UTC);
        assert_eq!(rows_utc[0].key, "2026-08-02"); // 00:30 UTC, also Aug 2nd

        let records_late = vec![record(
            "2026-08-01T20:00:00Z",
            "r",
            "p",
            "m",
            "success",
            1,
            1,
        )];
        let rows_jst_late = aggregate(&records_late, GroupBy::Day, None, None, jst);
        // 20:00 UTC + 9h = 05:00 the next day in JST.
        assert_eq!(rows_jst_late[0].key, "2026-08-02");
        let rows_utc_late = aggregate(&records_late, GroupBy::Day, None, None, UTC);
        assert_eq!(rows_utc_late[0].key, "2026-08-01");
    }

    #[test]
    fn since_and_until_reject_malformed_dates() {
        assert!(parse_date("2026-8-1").is_err());
        assert!(parse_date("not-a-date").is_err());
        assert!(parse_date("2026-08-01").is_ok());
    }

    #[test]
    fn offset_formats_as_hours_and_minutes() {
        let jst = time::UtcOffset::from_hms(9, 0, 0).unwrap();
        assert_eq!(format_offset(jst), "UTC+09:00");
        assert_eq!(format_offset(UTC), "UTC+00:00");

        let west = time::UtcOffset::from_hms(-5, -30, 0).unwrap();
        assert_eq!(format_offset(west), "UTC-05:30");
    }

    /// A handful of historical zones had a non-zero seconds component (e.g.
    /// the Netherlands' UTC+00:19:32 before 1937). Nothing in modern use has
    /// one, but if it ever shows up the seconds must still appear rather
    /// than being silently rounded away.
    #[test]
    fn offset_formats_with_seconds_when_present() {
        let historical = time::UtcOffset::from_hms(0, 19, 32).unwrap();
        assert_eq!(format_offset(historical), "UTC+00:19:32");
    }

    #[test]
    fn validate_date_range_rejects_since_after_until() {
        let err = validate_date_range(Some("2026-08-10"), Some("2026-08-01")).unwrap_err();
        assert!(err.to_string().contains("--since `2026-08-10`"));
        assert!(err.to_string().contains("--until `2026-08-01`"));
    }

    #[test]
    fn validate_date_range_accepts_equal_or_ordered_bounds() {
        assert!(validate_date_range(Some("2026-08-01"), Some("2026-08-01")).is_ok());
        assert!(validate_date_range(Some("2026-08-01"), Some("2026-08-10")).is_ok());
        assert!(validate_date_range(Some("2026-08-01"), None).is_ok());
        assert!(validate_date_range(None, Some("2026-08-01")).is_ok());
        assert!(validate_date_range(None, None).is_ok());
    }

    #[test]
    fn counts_are_comma_grouped() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(5), "5");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1234567), "1,234,567");
    }

    #[test]
    fn cache_tokens_are_aggregated() {
        let mut a = record(
            "2026-08-01T00:00:00Z",
            "claude-*",
            "anthropic",
            "claude-sonnet-4-6",
            "success",
            10,
            20,
        );
        a.cache_read_tok = 100;
        a.cache_write_tok = 7;
        let mut b = record(
            "2026-08-01T01:00:00Z",
            "claude-*",
            "anthropic",
            "claude-sonnet-4-6",
            "success",
            5,
            5,
        );
        b.cache_read_tok = 50;
        b.cache_write_tok = 3;

        let rows = aggregate(&[a, b], GroupBy::Route, None, None, UTC);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].cache_read_tok, 150);
        assert_eq!(rows[0].cache_write_tok, 10);
        assert_eq!(rows[0].total_tokens(), 15 + 25 + 150 + 10);
    }

    /// A `usage-*.jsonl` line written before cache accounting existed has no
    /// `cache_read_tok`/`cache_write_tok` fields at all — that must still
    /// parse and aggregate without falling over, with cache totals at zero.
    #[test]
    fn old_usage_records_without_cache_fields_still_aggregate() {
        let json = r#"{"ts":"2026-08-01T00:00:00Z","client":"claude-code","route":"claude-*","provider":"anthropic","model":"claude-sonnet-4-6","attempt":1,"in_tok":10,"out_tok":20,"dur_ms":100,"status":"success","stream":false}"#;
        let record: UsageRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.cache_read_tok, 0);
        assert_eq!(record.cache_write_tok, 0);
        assert!(!record.usage_missing);

        let rows = aggregate(&[record], GroupBy::Route, None, None, UTC);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].in_tok, 10);
        assert_eq!(rows[0].cache_read_tok, 0);
        assert_eq!(rows[0].cache_write_tok, 0);
    }

    /// Serializes tests below that point `LLM_GATEWAY_CONFIG_DIR` at a temp
    /// directory — the env var is process-global, so two such tests running
    /// on different threads (the default under `cargo test`) could otherwise
    /// see each other's config dir.
    static CONFIG_DIR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// #20/#15: `stats` used to prune `usage-*.jsonl` on every run, racing
    /// `serve`'s own append across process boundaries with no lock between
    /// them. It must now only read — a fully-stale usage file (every line
    /// past retention) must come back from `run()` exactly as it went in.
    #[test]
    fn stats_run_does_not_prune_the_usage_log_it_reads() {
        let _guard = CONFIG_DIR_ENV_LOCK.lock().unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        // `Config::read` (unlike `load_from`) does not validate, and every
        // field defaults — an empty object is a complete, if minimal,
        // config.json.
        std::fs::write(config_dir.path().join("config.json"), "{}").unwrap();

        let logs_dir = config_dir.path().join("logs");
        std::fs::create_dir_all(&logs_dir).unwrap();
        let usage_path = logs_dir.join("usage-2020-01.jsonl");
        let stale_line = r#"{"ts":"2020-01-01T00:00:00Z","client":"c","route":"r","provider":"p","model":"m","attempt":1,"in_tok":1,"out_tok":1,"cache_read_tok":0,"cache_write_tok":0,"dur_ms":1,"status":"success","stream":false}"#;
        std::fs::write(&usage_path, format!("{stale_line}\n")).unwrap();

        // SAFETY: `set_var`/`remove_var` are only unsound under concurrent
        // access from another thread; `CONFIG_DIR_ENV_LOCK` above rules that
        // out for every test that touches this variable.
        unsafe {
            std::env::set_var(crate::paths::CONFIG_DIR_ENV, config_dir.path());
        }
        let result = run(Options {
            by: GroupBy::Route,
            since: None,
            until: None,
            all: true,
        });
        unsafe {
            std::env::remove_var(crate::paths::CONFIG_DIR_ENV);
        }
        result.unwrap();

        // If `run()` still pruned, this decades-old line would have been
        // stripped (the file emptied, per the `Deleted` outcome) — it must
        // survive untouched.
        let contents = std::fs::read_to_string(&usage_path).unwrap();
        assert_eq!(contents, format!("{stale_line}\n"));
    }
}
