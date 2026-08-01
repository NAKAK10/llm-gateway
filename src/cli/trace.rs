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
    // Most requests never touch the cache, so this is only worth a look when
    // it did something — same reasoning as `decided_by_suffix` below.
    let cache_suffix = match &record.usage {
        Some(usage) if usage.cache_read_tok > 0 || usage.cache_write_tok > 0 => {
            format!(
                " cache_r={} cache_w={}",
                usage.cache_read_tok, usage.cache_write_tok
            )
        }
        _ => String::new(),
    };
    // Only semantic routing has a score to show; explicit matches leave it
    // `None` and this stays empty, so the common case is unaffected.
    let score_suffix = match record.routing.score {
        Some(score) => format!(" score={score:.2}"),
        None => String::new(),
    };
    // Shown because a translated response is the one case where the bytes the
    // client saw are not the bytes the provider sent — "why does this output
    // look slightly different?" starts here.
    let translation_suffix = match &record.resolved.translation {
        Some(label) => format!(" xlat={label}"),
        None => String::new(),
    };
    // `semantic_history` is the one mode where "which text won" is not
    // obvious from the route alone — the newest text didn't clear threshold,
    // an older one did. Shown only there; every other mode either has no
    // decided-by text or it is unambiguously the newest message already
    // implied by `input.last_user_text`.
    let decided_by_suffix = if record.routing.mode == "semantic_history" {
        match &record.routing.decided_by_text {
            Some(text) => format!(" decided_by={text:?}"),
            None => String::new(),
        }
    } else {
        String::new()
    };

    format!(
        "{time} {client}→{route}{score_suffix} [{provider}/{model}]{translation_suffix} {result}{attempts_suffix}{usage_suffix}{cache_suffix}{decided_by_suffix}",
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
                decided_by_text: None,
                walk: None,
            },
            resolved: TraceResolved {
                provider: "anthropic".to_string(),
                model: "claude-sonnet-4-6".to_string(),
                api: "anthropic-messages".to_string(),
                translation: None,
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
            cache_read_tok: 0,
            cache_write_tok: 0,
        });
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→claude-* [anthropic/claude-sonnet-4-6] ok_first_byte in=120 out=340"
        );
    }

    #[test]
    fn cache_tokens_are_appended_when_present() {
        let mut record = base_record();
        record.usage = Some(TraceUsage {
            in_tok: 120,
            out_tok: 340,
            cache_read_tok: 500,
            cache_write_tok: 20,
        });
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→claude-* [anthropic/claude-sonnet-4-6] ok_first_byte \
             in=120 out=340 cache_r=500 cache_w=20"
        );
    }

    #[test]
    fn cache_tokens_are_omitted_when_zero() {
        let mut record = base_record();
        record.usage = Some(TraceUsage {
            in_tok: 120,
            out_tok: 340,
            cache_read_tok: 0,
            cache_write_tok: 0,
        });
        let line = format_line(&record);
        assert!(!line.contains("cache_r="));
        assert!(!line.contains("cache_w="));
    }

    #[test]
    fn a_translated_route_is_marked_so_a_rebuilt_response_is_never_a_surprise() {
        let mut record = base_record();
        record.resolved.provider = "ollama".to_string();
        record.resolved.model = "qwen3.5:latest".to_string();
        record.resolved.api = "openai-chat".to_string();
        record.resolved.translation = Some("anthropic-messages->openai-chat".to_string());
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→claude-* [ollama/qwen3.5:latest] \
             xlat=anthropic-messages->openai-chat ok_first_byte"
        );
    }

    #[test]
    fn semantic_score_is_appended_when_present() {
        let mut record = base_record();
        record.routing.mode = "semantic".to_string();
        record.routing.matched_route = "role-writer".to_string();
        record.routing.score = Some(0.812);
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→role-writer score=0.81 [anthropic/claude-sonnet-4-6] ok_first_byte"
        );
    }

    #[test]
    fn semantic_history_shows_which_text_decided_it() {
        let mut record = base_record();
        record.routing.mode = "semantic_history".to_string();
        record.routing.matched_route = "role-tester".to_string();
        record.routing.score = Some(0.7);
        record.routing.decided_by_text = Some("now write the tests".to_string());
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→role-tester score=0.70 [anthropic/claude-sonnet-4-6] \
             ok_first_byte decided_by=\"now write the tests\""
        );
    }

    #[test]
    fn decided_by_text_is_not_shown_outside_semantic_history() {
        // `semantic` mode's newest text is already implied by
        // `input.last_user_text`; showing it again here would be noise.
        let mut record = base_record();
        record.routing.mode = "semantic".to_string();
        record.routing.matched_route = "role-writer".to_string();
        record.routing.score = Some(0.8);
        record.routing.decided_by_text = Some("write me a poem".to_string());
        assert_eq!(
            format_line(&record),
            "12:34:56 claude-code→role-writer score=0.80 [anthropic/claude-sonnet-4-6] ok_first_byte"
        );
    }
}
