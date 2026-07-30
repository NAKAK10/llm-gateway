//! `llm-gateway trace` — read routing decisions back.
//!
//! Pretty-prints `trace-*.jsonl`, optionally following the newest file as it
//! grows. This is the operator's answer to "why did that request go to *that*
//! model?" — the data is already structured, this just renders it.

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use crate::config::Config;
use crate::error::Result;
use crate::paths;
use crate::record::trace_log::{self, TraceRecord};

pub struct Options {
    /// Follow the newest trace file, like `tail -f`.
    pub tail: bool,
    /// Only entries whose matched route equals this.
    pub route: Option<String>,
    /// Only entries from this client tag.
    pub client: Option<String>,
}

pub fn run(options: Options) -> Result<()> {
    // Same resilience as `stats`: a broken config should not stop `trace`
    // from reading a log directory that already exists.
    let logs_dir = Config::read(&paths::config_file())
        .ok()
        .map(|c| paths::logs_dir(&c.logging.dir))
        .unwrap_or_else(|| PathBuf::from("./logs"));

    let now = time::OffsetDateTime::now_utc();
    let path = logs_dir.join(trace_log::file_name(
        now.year(),
        now.month() as u8,
        now.day(),
    ));

    if !path.exists() {
        println!("no trace file for today; run serve --debug");
        return Ok(());
    }

    let matches = |record: &TraceRecord| {
        if let Some(route) = &options.route {
            if &record.routing.matched_route != route {
                return false;
            }
        }
        if let Some(client) = &options.client {
            if &record.client != client {
                return false;
            }
        }
        true
    };

    let file = std::fs::File::open(&path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            if !options.tail {
                break;
            }
            // Wait for more bytes to arrive, then resume from exactly where
            // we left off — reopening would lose that position.
            let pos = reader.stream_position()?;
            std::thread::sleep(std::time::Duration::from_secs(1));
            reader.seek(SeekFrom::Start(pos))?;
            continue;
        }

        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<TraceRecord>(trimmed) {
            if matches(&record) {
                println!("{}", format_line(&record));
            }
        }
    }

    Ok(())
}

/// Render one record as a compact single line for the terminal.
///
/// Pure, so formatting is testable.
pub fn format_line(record: &TraceRecord) -> String {
    let time = record.ts.get(11..19).unwrap_or(&record.ts);

    let result = record
        .attempts
        .last()
        .map(|a| a.result.as_str())
        .unwrap_or("no-attempt");
    let attempts_suffix = if record.attempts.len() > 1 {
        format!(" ({} attempts)", record.attempts.len())
    } else {
        String::new()
    };
    let usage_suffix = match &record.usage {
        Some(usage) => format!(" in={} out={}", usage.in_tok, usage.out_tok),
        None => String::new(),
    };

    format!(
        "{time} {client}→{route} [{provider}/{model}] {result}{attempts_suffix}{usage_suffix}",
        client = record.client,
        route = record.routing.matched_route,
        provider = record.resolved.provider,
        model = record.resolved.model,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::trace_log::{
        TraceAttempt, TraceInput, TraceResolved, TraceRouting, TraceUsage,
    };

    fn base_record() -> TraceRecord {
        TraceRecord {
            ts: "2026-08-01T12:34:56Z".to_string(),
            req_id: "req-1".to_string(),
            client: "claude-code".to_string(),
            endpoint: "/v1/messages".to_string(),
            requested_model: "claude-sonnet-4-6".to_string(),
            input: TraceInput {
                messages_n: 1,
                last_user_text: None,
                tokens_est: 10,
                tools: Vec::new(),
                has_image: false,
                stream: false,
            },
            routing: TraceRouting {
                mode: "explicit".to_string(),
                matched_route: "claude-*".to_string(),
                reason: "exact match".to_string(),
                candidates: Vec::new(),
                score: None,
                threshold: None,
                embed_ms: None,
            },
            resolved: TraceResolved {
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4-6".to_string(),
                api: "anthropic-messages".to_string(),
            },
            attempts: vec![TraceAttempt {
                n: 1,
                target: "anthropic/claude-sonnet-4-6".to_string(),
                result: "ok_first_byte".to_string(),
                ms: 250,
            }],
            usage: None,
        }
    }

    #[test]
    fn single_attempt_without_usage() {
        let record = base_record();
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→claude-* [anthropic/claude-sonnet-4-6] ok_first_byte"
        );
    }

    #[test]
    fn multiple_attempts_are_counted() {
        let mut record = base_record();
        record.attempts.push(TraceAttempt {
            n: 2,
            target: "openrouter-anthropic/anthropic/claude-sonnet-4-6".to_string(),
            result: "http_429".to_string(),
            ms: 80,
        });
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→claude-* [anthropic/claude-sonnet-4-6] http_429 (2 attempts)"
        );
    }

    #[test]
    fn usage_is_appended_when_present() {
        let mut record = base_record();
        record.usage = Some(TraceUsage {
            in_tok: 120,
            out_tok: 340,
        });
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→claude-* [anthropic/claude-sonnet-4-6] ok_first_byte in=120 out=340"
        );
    }
}
