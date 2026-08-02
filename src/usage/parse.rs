//! Extracting `usage` from all three wire formats.
//!
//! Each protocol reports token counts in a different place, and streaming
//! reports them differently again:
//!
//! | protocol | non-streaming | streaming |
//! |---|---|---|
//! | `anthropic-messages` | `usage` | `message_start.message.usage` (input) then `message_delta.usage` (output) |
//! | `openai-chat` | `usage` | a final chunk whose `usage` is non-null — only sent when `stream_options.include_usage` was requested |
//! | `openai-responses` | `response.usage` | `response.completed` → `response.usage` |
//!
//! Field names also differ: Anthropic uses `input_tokens`/`output_tokens`,
//! OpenAI uses `prompt_tokens`/`completion_tokens` for chat and
//! `input_tokens`/`output_tokens` for responses.
//!
//! The two families also disagree about what `input_tokens` *counts*:
//! Anthropic's excludes cache reads/writes (they are their own fields), while
//! OpenAI's `prompt_tokens`/`input_tokens` already include `cached_tokens`.
//! Reporting either verbatim as [`Usage::input_tokens`] would make
//! `in_tok + cache_read_tok` double-count cache on OpenAI-shaped responses
//! but not on Anthropic ones, so the cached portion is subtracted out here —
//! [`Usage::input_tokens`] always means "new, non-cached input" regardless of
//! which upstream produced it.

use crate::config::ApiKind;
use crate::usage::Usage;

/// Defensive cap on how large a single buffered (partial) SSE event may grow.
/// A well-behaved upstream never gets close to this; it exists so a
/// misbehaving one cannot make the observer's buffer grow without bound.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Pull a `u64` out of a JSON object field, defaulting to 0 when the field is
/// missing, null, or not a number.
fn field(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Pull a `u64` out of a JSON object field, but — unlike `field` — `None`
/// when the key is absent (or not a number) rather than defaulting to 0.
///
/// Only used for Anthropic's streaming events, which are genuinely partial:
/// a `message_delta` event has no `input_tokens` key at all, and that must
/// stay distinguishable from a `message_start` that reported `input_tokens:
/// 0` — see [`Usage`]'s doc comment for why the difference matters to
/// `merge`.
fn field_opt(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|v| v.as_u64())
}

/// Extract usage from an `anthropic-messages` `usage` object.
///
/// Used for both the non-streaming response body and the `message.usage`
/// object nested inside a streamed `message_start` event — in both cases
/// each field is set only when present, since a `message_delta` event's
/// `usage` object legitimately omits `input_tokens` entirely.
fn anthropic_usage(usage: &serde_json::Value) -> Usage {
    Usage {
        input_tokens: field_opt(usage, "input_tokens"),
        output_tokens: field_opt(usage, "output_tokens"),
        cache_read_tokens: field_opt(usage, "cache_read_input_tokens"),
        cache_write_tokens: field_opt(usage, "cache_creation_input_tokens"),
    }
}

/// Extract usage from an `openai-chat` `usage` object.
///
/// Unlike Anthropic's streaming events, an `openai-chat` usage object is
/// always a complete snapshot when it appears at all (there is exactly one
/// per response), so every field here is `Some` — including a `cache_read`
/// of `Some(0)` when the subtraction below lands on zero, which must
/// overwrite a non-zero value from an earlier event rather than being
/// mistaken for "not reported this time". See [`Usage`]'s doc comment.
fn openai_chat_usage(usage: &serde_json::Value) -> Usage {
    let cache_read = usage
        .get("prompt_tokens_details")
        .map(|d| field(d, "cached_tokens"))
        .unwrap_or(0);
    Usage {
        // `prompt_tokens` includes `cached_tokens`; see the module docs.
        input_tokens: Some(field(usage, "prompt_tokens").saturating_sub(cache_read)),
        output_tokens: Some(field(usage, "completion_tokens")),
        cache_read_tokens: Some(cache_read),
        cache_write_tokens: Some(0),
    }
}

/// Extract usage from an `openai-responses` `usage` object.
///
/// Same reasoning as `openai_chat_usage`: always a complete snapshot, so
/// every field is `Some`.
fn openai_responses_usage(usage: &serde_json::Value) -> Usage {
    let cache_read = usage
        .get("input_tokens_details")
        .map(|d| field(d, "cached_tokens"))
        .unwrap_or(0);
    Usage {
        // `input_tokens` includes `cached_tokens`; see the module docs.
        input_tokens: Some(field(usage, "input_tokens").saturating_sub(cache_read)),
        output_tokens: Some(field(usage, "output_tokens")),
        cache_read_tokens: Some(cache_read),
        cache_write_tokens: Some(0),
    }
}

/// Pull usage out of a complete, non-streaming JSON response body.
pub fn from_json(api: ApiKind, body: &serde_json::Value) -> Usage {
    match api {
        ApiKind::AnthropicMessages => body.get("usage").map(anthropic_usage).unwrap_or_default(),
        ApiKind::OpenaiChat => body.get("usage").map(openai_chat_usage).unwrap_or_default(),
        ApiKind::OpenaiResponses => body
            .get("usage")
            .map(openai_responses_usage)
            .unwrap_or_default(),
    }
}

/// Incremental parser for a Server-Sent Events stream.
///
/// Fed raw bytes as they pass through to the client. Keeps only the bytes of a
/// partial event, so memory use stays bounded regardless of response length.
#[derive(Debug, Default)]
pub struct SseUsageScanner {
    pub(crate) api: Option<ApiKind>,
    pub(crate) buffer: Vec<u8>,
    pub(crate) usage: Usage,
}

impl SseUsageScanner {
    pub fn new(api: ApiKind) -> Self {
        Self {
            api: Some(api),
            buffer: Vec::new(),
            usage: Usage::default(),
        }
    }

    /// Observe a chunk. Never modifies or withholds it — the caller has already
    /// forwarded these bytes downstream.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);

        // An event ends at a blank line — `\n\n`, or `\r\n\r\n` for an
        // upstream that frames its stream with CRLF line endings. Drain
        // every complete event currently in the buffer, keeping only the
        // trailing partial one.
        while let Some((pos, sep_len)) = find_event_boundary(&self.buffer) {
            let event_end = pos + sep_len;
            let event: Vec<u8> = self.buffer.drain(..event_end).collect();
            self.handle_event(&event[..pos]);
        }

        // Defensive: an event that never terminates (or a huge single event)
        // must not let the buffer grow without bound.
        if self.buffer.len() > MAX_EVENT_BYTES {
            self.buffer.clear();
        }
    }

    /// Handle one complete event's bytes (without the trailing separator).
    fn handle_event(&mut self, event: &[u8]) {
        let Some(api) = self.api else {
            return;
        };
        let Ok(text) = std::str::from_utf8(event) else {
            return;
        };

        // Concatenate every `data: ` line — multi-line `data` fields are
        // joined with `\n` per the SSE spec, but every payload we care about
        // here is single-line JSON, so plain concatenation is fine.
        let mut data = String::new();
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if let Some(rest) = line.strip_prefix("data: ") {
                data.push_str(rest);
            } else if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest);
            }
        }
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(data) else {
            return;
        };

        match api {
            ApiKind::AnthropicMessages => match json.get("type").and_then(|t| t.as_str()) {
                Some("message_start") => {
                    if let Some(usage) = json.get("message").and_then(|m| m.get("usage")) {
                        self.usage.merge(anthropic_usage(usage));
                    }
                }
                Some("message_delta") => {
                    if let Some(usage) = json.get("usage") {
                        self.usage.merge(anthropic_usage(usage));
                    }
                }
                _ => {}
            },
            ApiKind::OpenaiChat => {
                if let Some(usage) = json.get("usage") {
                    if !usage.is_null() {
                        self.usage.merge(openai_chat_usage(usage));
                    }
                }
            }
            ApiKind::OpenaiResponses => {
                // `.completed` is the normal end of a successful run, but a
                // response that hit its token limit or failed mid-generation
                // still reports the usage it burned through `.incomplete` /
                // `.failed` instead — skipping those undercounts exactly the
                // requests worth accounting for most.
                let event_type = json.get("type").and_then(|t| t.as_str());
                let terminal = matches!(
                    event_type,
                    Some("response.completed" | "response.incomplete" | "response.failed")
                );
                if terminal {
                    if let Some(usage) = json.get("response").and_then(|r| r.get("usage")) {
                        self.usage.merge(openai_responses_usage(usage));
                    }
                }
            }
        }
    }

    /// Usage seen so far. Complete once the stream has ended.
    ///
    /// A well-formed SSE stream ends its last event with a blank line like
    /// every other, but a connection can also simply close right after the
    /// final `data:` line with no trailing separator — that leftover partial
    /// event is still a complete, parseable one, so it is handled here
    /// rather than silently dropped with whatever usage it carried.
    pub fn finish(mut self) -> Usage {
        if !self.buffer.is_empty() {
            let event = std::mem::take(&mut self.buffer);
            self.handle_event(&event);
        }
        self.usage
    }
}

/// Find where the next complete SSE event ends: a blank line, either the
/// ordinary `\n\n` or a CRLF-framed `\r\n\r\n`. Returns `(event_start_end,
/// separator_len)` so the caller can drain the event plus its separator in
/// one slice.
pub(crate) fn find_event_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    let lf = find_subslice(buffer, b"\n\n").map(|pos| (pos, 2));
    let crlf = find_subslice(buffer, b"\r\n\r\n").map(|pos| (pos, 4));
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Find the first occurrence of `needle` in `haystack`, byte-wise.
///
/// Shared with `translate::stream`, which frames an SSE stream the same way
/// this scanner does — one implementation of "where does this event end?" is
/// one place for that answer to be wrong.
pub(crate) fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_non_streaming_usage_is_extracted() {
        let body = json!({
            "usage": {
                "input_tokens": 12,
                "output_tokens": 34,
                "cache_read_input_tokens": 5,
                "cache_creation_input_tokens": 7,
            }
        });
        let usage = from_json(ApiKind::AnthropicMessages, &body);
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(12),
                output_tokens: Some(34),
                cache_read_tokens: Some(5),
                cache_write_tokens: Some(7),
            }
        );
    }

    #[test]
    fn anthropic_missing_fields_are_unobserved_not_zero() {
        // A `message_delta` event legitimately omits `input_tokens` entirely
        // — that must stay distinguishable from an event that reported it as
        // 0, or `merge` cannot tell "nothing new here" from "this is now
        // genuinely zero" (see the `Usage` doc comment and #23).
        let body = json!({ "usage": { "input_tokens": 12 } });
        let usage = from_json(ApiKind::AnthropicMessages, &body);
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(12),
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
            }
        );
    }

    #[test]
    fn anthropic_missing_usage_object_does_not_panic() {
        let body = json!({ "not_usage": true });
        let usage = from_json(ApiKind::AnthropicMessages, &body);
        assert_eq!(usage, Usage::default());
    }

    #[test]
    fn openai_chat_non_streaming_usage_is_extracted() {
        let body = json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "prompt_tokens_details": { "cached_tokens": 3 },
            }
        });
        let usage = from_json(ApiKind::OpenaiChat, &body);
        assert_eq!(
            usage,
            Usage {
                // `prompt_tokens` (10) already includes `cached_tokens` (3);
                // `input_tokens` is the non-cached remainder.
                input_tokens: Some(7),
                output_tokens: Some(20),
                cache_read_tokens: Some(3),
                cache_write_tokens: Some(0),
            }
        );
    }

    #[test]
    fn openai_responses_non_streaming_usage_is_extracted() {
        let body = json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 200,
                "input_tokens_details": { "cached_tokens": 40 },
            }
        });
        let usage = from_json(ApiKind::OpenaiResponses, &body);
        assert_eq!(
            usage,
            Usage {
                // Same normalization as `openai-chat`: `input_tokens` (100)
                // includes `cached_tokens` (40).
                input_tokens: Some(60),
                output_tokens: Some(200),
                cache_read_tokens: Some(40),
                cache_write_tokens: Some(0),
            }
        );
    }

    #[test]
    fn openai_chat_input_tokens_never_underflows_when_cache_exceeds_prompt() {
        // Should never happen from a well-behaved upstream, but a
        // `saturating_sub` keeps a malformed one from panicking or wrapping.
        let body = json!({
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 1,
                "prompt_tokens_details": { "cached_tokens": 9 },
            }
        });
        let usage = from_json(ApiKind::OpenaiChat, &body);
        assert_eq!(usage.input_tokens, Some(0));
    }

    fn anthropic_message_start_event(
        input_tokens: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> String {
        format!(
            "event: message_start\ndata: {}\n\n",
            json!({
                "type": "message_start",
                "message": {
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": 0,
                        "cache_read_input_tokens": cache_read,
                        "cache_creation_input_tokens": cache_write,
                    }
                }
            })
        )
    }

    fn anthropic_message_delta_event(output_tokens: u64) -> String {
        format!(
            "event: message_delta\ndata: {}\n\n",
            json!({
                "type": "message_delta",
                "usage": { "output_tokens": output_tokens }
            })
        )
    }

    #[test]
    fn anthropic_sse_two_events_in_one_chunk() {
        let mut scanner = SseUsageScanner::new(ApiKind::AnthropicMessages);
        let mut sse = anthropic_message_start_event(50, 8, 2);
        sse.push_str(&anthropic_message_delta_event(75));
        scanner.push(sse.as_bytes());
        let usage = scanner.finish();
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(50),
                output_tokens: Some(75),
                cache_read_tokens: Some(8),
                cache_write_tokens: Some(2),
            }
        );
    }

    #[test]
    fn anthropic_sse_events_split_across_chunks() {
        let mut scanner = SseUsageScanner::new(ApiKind::AnthropicMessages);
        let start = anthropic_message_start_event(50, 8, 2);
        let delta = anthropic_message_delta_event(75);
        scanner.push(start.as_bytes());
        scanner.push(delta.as_bytes());
        let usage = scanner.finish();
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(50),
                output_tokens: Some(75),
                cache_read_tokens: Some(8),
                cache_write_tokens: Some(2),
            }
        );
    }

    #[test]
    fn anthropic_sse_event_boundary_does_not_align_with_chunk_boundary() {
        let mut scanner = SseUsageScanner::new(ApiKind::AnthropicMessages);
        let start = anthropic_message_start_event(50, 8, 2);
        let delta = anthropic_message_delta_event(75);
        let combined = format!("{start}{delta}");
        // Split in the middle of the `message_start` event's JSON payload, and
        // again in the middle of the separator between events, to make sure
        // partial buffering handles both cases.
        let mid = combined.len() / 3;
        let (a, rest) = combined.split_at(mid);
        let mid2 = rest.len() / 2;
        let (b, c) = rest.split_at(mid2);
        scanner.push(a.as_bytes());
        scanner.push(b.as_bytes());
        scanner.push(c.as_bytes());
        let usage = scanner.finish();
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(50),
                output_tokens: Some(75),
                cache_read_tokens: Some(8),
                cache_write_tokens: Some(2),
            }
        );
    }

    #[test]
    fn openai_chat_sse_final_usage_chunk_is_captured() {
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiChat);
        let mid_chunk = format!(
            "data: {}\n\n",
            json!({ "choices": [{ "delta": { "content": "hi" } }], "usage": null })
        );
        let final_chunk = format!(
            "data: {}\n\n",
            json!({
                "choices": [],
                "usage": { "prompt_tokens": 15, "completion_tokens": 25 }
            })
        );
        let done = "data: [DONE]\n\n";
        scanner.push(mid_chunk.as_bytes());
        scanner.push(final_chunk.as_bytes());
        scanner.push(done.as_bytes());
        let usage = scanner.finish();
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(15),
                output_tokens: Some(25),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
            }
        );
    }

    #[test]
    fn openai_chat_second_usage_event_that_is_a_full_cache_hit_overwrites_the_first() {
        // Regression test for #23: some `openai-chat` upstreams emit more
        // than one non-null `usage` chunk in a single stream. If the second
        // one is a full cache hit (`cached_tokens == prompt_tokens`), the
        // subtracted `input_tokens` is a genuine, observed 0 — it must
        // overwrite the first event's `input_tokens`, not be skipped as
        // "nothing new" the way a plain `u64` merge would skip it, which
        // would leave `in_tok + cache_read_tok` double-counting the cached
        // portion again.
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiChat);
        let first = format!(
            "data: {}\n\n",
            json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 10,
                    "prompt_tokens_details": { "cached_tokens": 20 },
                }
            })
        );
        let second = format!(
            "data: {}\n\n",
            json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 20,
                    "prompt_tokens_details": { "cached_tokens": 100 },
                }
            })
        );
        scanner.push(first.as_bytes());
        scanner.push(second.as_bytes());
        let usage = scanner.finish();
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(0),
                output_tokens: Some(20),
                cache_read_tokens: Some(100),
                cache_write_tokens: Some(0),
            }
        );
    }

    #[test]
    fn openai_responses_completed_event_is_captured() {
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiResponses);
        let event = format!(
            "data: {}\n\n",
            json!({
                "type": "response.completed",
                "response": {
                    "usage": { "input_tokens": 30, "output_tokens": 60 }
                }
            })
        );
        scanner.push(event.as_bytes());
        let usage = scanner.finish();
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(30),
                output_tokens: Some(60),
                cache_read_tokens: Some(0),
                cache_write_tokens: Some(0),
            }
        );
    }

    #[test]
    fn openai_responses_incomplete_event_usage_is_captured() {
        // A response that hit `max_output_tokens` still burned real tokens —
        // skipping `.incomplete` would silently undercount every truncated
        // response.
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiResponses);
        let event = format!(
            "data: {}\n\n",
            json!({
                "type": "response.incomplete",
                "response": {
                    "usage": { "input_tokens": 30, "output_tokens": 60 }
                }
            })
        );
        scanner.push(event.as_bytes());
        assert_eq!(scanner.finish().output_tokens, Some(60));
    }

    #[test]
    fn openai_responses_failed_event_usage_is_captured() {
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiResponses);
        let event = format!(
            "data: {}\n\n",
            json!({
                "type": "response.failed",
                "response": {
                    "usage": { "input_tokens": 12, "output_tokens": 3 }
                }
            })
        );
        scanner.push(event.as_bytes());
        assert_eq!(scanner.finish().output_tokens, Some(3));
    }

    #[test]
    fn find_event_boundary_finds_a_plain_lf_separator() {
        assert_eq!(find_event_boundary(b"data: a\n\ndata: b"), Some((7, 2)));
    }

    #[test]
    fn find_event_boundary_finds_a_crlf_separator() {
        assert_eq!(find_event_boundary(b"data: a\r\n\r\ndata: b"), Some((7, 4)));
    }

    #[test]
    fn find_event_boundary_prefers_whichever_separator_comes_first() {
        // A CRLF-framed event followed by an LF-framed one: the LF pair
        // inside `\r\n\r\n` must not be mistaken for its own, earlier
        // boundary — `\r\n\r\n` contains no bare `\n\n` (the two `\n`s are
        // each preceded by `\r`), so the CRLF boundary at position 7 is the
        // only one found, and it must win despite starting the search for
        // both patterns at position 0.
        let buffer = b"data: a\r\n\r\ndata: b\n\n";
        assert_eq!(find_event_boundary(buffer), Some((7, 4)));

        // Once the CRLF event is drained, the LF boundary in what remains is
        // the only one left.
        let rest = &buffer[11..];
        assert_eq!(find_event_boundary(rest), Some((7, 2)));
    }

    #[test]
    fn find_event_boundary_is_none_without_a_blank_line() {
        assert_eq!(find_event_boundary(b"data: still one event\r\nmore"), None);
    }

    #[test]
    fn find_event_boundary_finds_a_separator_split_across_two_pushes() {
        // `push` only searches the buffer it is holding at call time; a
        // separator whose bytes were fed across two `push` calls does not
        // exist as a contiguous slice until both halves have arrived. This
        // mirrors that: search the concatenation, not each half separately.
        let mut buffer = b"data: a\r\n".to_vec();
        assert_eq!(find_event_boundary(&buffer), None);
        buffer.extend_from_slice(b"\r\ndata: b");
        assert_eq!(find_event_boundary(&buffer), Some((7, 4)));
    }

    #[test]
    fn crlf_framed_sse_events_are_parsed() {
        // Some upstreams (and the proxies in front of them) frame SSE with
        // `\r\n` line endings, so the event separator is `\r\n\r\n` rather
        // than `\n\n` — a scanner that only looks for `\n\n` never finds the
        // boundary and the event sits unparsed in the buffer forever.
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiChat);
        let event = format!(
            "data: {}\r\n\r\n",
            json!({
                "choices": [],
                "usage": { "prompt_tokens": 15, "completion_tokens": 25 }
            })
        );
        scanner.push(event.as_bytes());
        let usage = scanner.finish();
        assert_eq!(usage.input_tokens, Some(15));
        assert_eq!(usage.output_tokens, Some(25));
    }

    #[test]
    fn crlf_and_lf_events_in_the_same_stream_both_parse() {
        let mut scanner = SseUsageScanner::new(ApiKind::AnthropicMessages);
        let start = anthropic_message_start_event(50, 8, 2).replace('\n', "\r\n");
        let delta = anthropic_message_delta_event(75); // plain `\n\n`
        scanner.push(start.as_bytes());
        scanner.push(delta.as_bytes());
        let usage = scanner.finish();
        assert_eq!(usage.input_tokens, Some(50));
        assert_eq!(usage.output_tokens, Some(75));
    }

    #[test]
    fn crlf_framed_event_with_multiple_data_lines_joins_cleanly() {
        // Per the SSE spec, multiple `data:` lines within one event are
        // joined with `\n`. Under CRLF framing that means each line (as
        // produced by splitting the event on `\n`) carries a trailing `\r`,
        // not a leading one — stripping the wrong end leaves the `\r` stuck
        // between the two lines' payloads instead of removed, which splits a
        // token (here, the digits of `15`) and breaks JSON parsing.
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiChat);
        scanner.push(
            b"data: {\"choices\": [], \"usage\": {\"prompt_tokens\": 1\r\n\
              data: 5, \"completion_tokens\": 25}}\r\n\r\n",
        );
        let usage = scanner.finish();
        assert_eq!(usage.input_tokens, Some(15));
        assert_eq!(usage.output_tokens, Some(25));
    }

    #[test]
    fn a_final_event_with_no_trailing_blank_line_is_still_parsed_on_finish() {
        // A connection can close right after the last `data:` line with no
        // terminating blank line — the event is still complete and
        // parseable, so `finish` must not just drop it on the floor.
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiChat);
        scanner.push(
            format!(
                "data: {}",
                json!({
                    "choices": [],
                    "usage": { "prompt_tokens": 9, "completion_tokens": 4 }
                })
            )
            .as_bytes(),
        );
        let usage = scanner.finish();
        assert_eq!(usage.input_tokens, Some(9));
        assert_eq!(usage.output_tokens, Some(4));
    }

    #[test]
    fn done_marker_alone_does_not_panic_or_change_usage() {
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiChat);
        scanner.push(b"data: [DONE]\n\n");
        assert_eq!(scanner.finish(), Usage::default());
    }

    #[test]
    fn unparseable_data_is_skipped_without_panicking() {
        let mut scanner = SseUsageScanner::new(ApiKind::OpenaiChat);
        scanner.push(b"data: not json at all\n\n");
        assert_eq!(scanner.finish(), Usage::default());
    }
}
