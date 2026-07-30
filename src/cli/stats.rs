//! `llm-gateway stats` — what was spent, and on what.
//!
//! Reads `usage-*.jsonl` and aggregates. Token counts are always meaningful;
//! money is not always available, because a flat-rate subscription has no
//! per-token price. So the table always shows tokens and shows cost only when
//! something computed one.

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::ValueEnum;

use crate::config::Config;
use crate::error::Result;
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
}

/// One aggregated row.
#[derive(Debug, Clone, Default)]
pub struct Row {
    pub key: String,
    pub calls: u64,
    pub failures: u64,
    pub in_tok: u64,
    pub out_tok: u64,
}

pub fn run(options: Options) -> Result<()> {
    // `Config::load()` also validates, and a broken config should not stop
    // `stats` from reading logs that already exist — fall back to the
    // default logs directory rather than erroring out.
    let logs_dir = Config::read(&paths::config_file())
        .ok()
        .map(|c| paths::logs_dir(&c.logging.dir))
        .unwrap_or_else(|| PathBuf::from("./logs"));

    let mut records = Vec::new();
    let mut skipped = 0u64;

    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
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
            let text = std::fs::read_to_string(&path)?;
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
    }

    if skipped > 0 {
        eprintln!("skipped {skipped} malformed lines");
    }

    if records.is_empty() {
        println!("no usage records found in {}", logs_dir.display());
        return Ok(());
    }

    let rows = aggregate(
        &records,
        options.by,
        options.since.as_deref(),
        options.until.as_deref(),
    );

    let mut table = comfy_table::Table::new();
    table.set_header(vec![
        options.by.to_string(),
        "calls".to_string(),
        "fail".to_string(),
        "in_tok".to_string(),
        "out_tok".to_string(),
    ]);

    let mut total = Row {
        key: "TOTAL".to_string(),
        ..Default::default()
    };
    for row in &rows {
        table.add_row(vec![
            row.key.clone(),
            format_count(row.calls),
            format_count(row.failures),
            format_count(row.in_tok),
            format_count(row.out_tok),
        ]);
        total.calls += row.calls;
        total.failures += row.failures;
        total.in_tok += row.in_tok;
        total.out_tok += row.out_tok;
    }
    table.add_row(vec![
        total.key.clone(),
        format_count(total.calls),
        format_count(total.failures),
        format_count(total.in_tok),
        format_count(total.out_tok),
    ]);

    println!("{table}");

    Ok(())
}

/// Aggregate already-parsed records. Pure, so it can be tested directly.
pub fn aggregate(
    records: &[UsageRecord],
    by: GroupBy,
    since: Option<&str>,
    until: Option<&str>,
) -> Vec<Row> {
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();

    for record in records {
        let day = day_of(&record.ts);
        if let Some(since) = since {
            if day < since {
                continue;
            }
        }
        if let Some(until) = until {
            if day > until {
                continue;
            }
        }

        let key = match by {
            GroupBy::Route => record.route.clone(),
            GroupBy::Client => record.client.clone(),
            GroupBy::Provider => record.provider.clone(),
            GroupBy::Model => record.model.clone(),
            GroupBy::Day => day.to_string(),
        };

        let row = rows.entry(key.clone()).or_insert_with(|| Row {
            key,
            ..Default::default()
        });
        row.calls += 1;
        if record.status != "success" {
            row.failures += 1;
        }
        row.in_tok += record.in_tok;
        row.out_tok += record.out_tok;
    }

    let mut result: Vec<Row> = rows.into_values().collect();
    match by {
        // Day is a timeline, so it reads best in chronological order.
        GroupBy::Day => result.sort_by(|a, b| a.key.cmp(&b.key)),
        // Everything else reads best biggest-first.
        _ => result.sort_by_key(|row| std::cmp::Reverse(row.in_tok + row.out_tok)),
    }
    result
}

/// The leading `YYYY-MM-DD` of an RFC 3339 timestamp.
fn day_of(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
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

        let rows = aggregate(&records, GroupBy::Route, None, None);

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
        );
        let calls: u64 = rows.iter().map(|r| r.calls).sum();
        assert_eq!(calls, 2);

        let all = aggregate(
            &records,
            GroupBy::Day,
            Some("2026-08-01"),
            Some("2026-08-03"),
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

        let rows = aggregate(&records, GroupBy::Route, None, None);
        assert_eq!(rows[0].calls, 3);
        assert_eq!(rows[0].failures, 2);
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

        let rows = aggregate(&records, GroupBy::Route, None, None);
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["big", "medium", "small"]);
    }

    #[test]
    fn day_grouping_sorts_chronologically() {
        let records = vec![
            record("2026-08-03T00:00:00Z", "r", "p", "m", "success", 1, 1),
            record("2026-08-01T00:00:00Z", "r", "p", "m", "success", 100, 100),
            record("2026-08-02T00:00:00Z", "r", "p", "m", "success", 10, 10),
        ];

        let rows = aggregate(&records, GroupBy::Day, None, None);
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(keys, vec!["2026-08-01", "2026-08-02", "2026-08-03"]);
    }

    #[test]
    fn counts_are_comma_grouped() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(5), "5");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(1234567), "1,234,567");
    }
}
