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

use crate::config::ApiKind;
use crate::usage::Usage;

/// An SSE event is done once a blank line separates it from the next.
const EVENT_SEPARATOR: &[u8] = b"\n\n";

/// Defensive cap on how large a single buffered (partial) SSE event may grow.
/// A well-behaved upstream never gets close to this; it exists so a
/// misbehaving one cannot make the observer's buffer grow without bound.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Pull a `u64` out of a JSON object field, defaulting to 0 when the field is
/// missing, null, or not a number.
fn field(value: &serde_json::Value, key: &str) -> u64 {
    value.get(key).and_then(|v| v.as_u64()).unwrap_or(0)
}

/// Extract usage from an `anthropic-messages` `usage` object.
///
/// Used for both the non-streaming response body and the `message.usage`
/// object nested inside a streamed `message_start` event.
fn anthropic_usage(usage: &serde_json::Value) -> Usage {
    Usage {
        input_tokens: field(usage, "input_tokens"),
        output_tokens: field(usage, "output_tokens"),
        cache_read_tokens: field(usage, "cache_read_input_tokens"),
        cache_write_tokens: field(usage, "cache_creation_input_tokens"),
    }
}

/// Extract usage from an `openai-chat` `usage` object.
fn openai_chat_usage(usage: &serde_json::Value) -> Usage {
    let cache_read = usage
        .get("prompt_tokens_details")
        .map(|d| field(d, "cached_tokens"))
        .unwrap_or(0);
    Usage {
        input_tokens: field(usage, "prompt_tokens"),
        output_tokens: field(usage, "completion_tokens"),
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
    }
}

/// Extract usage from an `openai-responses` `usage` object.
fn openai_responses_usage(usage: &serde_json::Value) -> Usage {
    let cache_read = usage
        .get("input_tokens_details")
        .map(|d| field(d, "cached_tokens"))
        .unwrap_or(0);
    Usage {
        input_tokens: field(usage, "input_tokens"),
        output_tokens: field(usage, "output_tokens"),
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
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

        // An event ends at a blank line (`\n\n`). Drain every complete event
        // currently in the buffer, keeping only the trailing partial one.
        while let Some(pos) = find_subslice(&self.buffer, EVENT_SEPARATOR) {
            let event_end = pos + EVENT_SEPARATOR.len();
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
            let line = line.strip_prefix("\r").unwrap_or(line);
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
                if json.get("type").and_then(|t| t.as_str()) == Some("response.completed") {
                    if let Some(usage) = json.get("response").and_then(|r| r.get("usage")) {
                        self.usage.merge(openai_responses_usage(usage));
                    }
                }
            }
        }
    }

    /// Usage seen so far. Complete once the stream has ended.
    pub fn finish(self) -> Usage {
        self.usage
    }
}

/// Find the first occurrence of `needle` in `haystack`, byte-wise.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
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
                input_tokens: 12,
                output_tokens: 34,
                cache_read_tokens: 5,
                cache_write_tokens: 7,
            }
        );
    }

    #[test]
    fn anthropic_missing_fields_default_to_zero() {
        let body = json!({ "usage": { "input_tokens": 12 } });
        let usage = from_json(ApiKind::AnthropicMessages, &body);
        assert_eq!(
            usage,
            Usage {
                input_tokens: 12,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
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
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 3,
                cache_write_tokens: 0,
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
                input_tokens: 100,
                output_tokens: 200,
                cache_read_tokens: 40,
                cache_write_tokens: 0,
            }
        );
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
                input_tokens: 50,
                output_tokens: 75,
                cache_read_tokens: 8,
                cache_write_tokens: 2,
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
                input_tokens: 50,
                output_tokens: 75,
                cache_read_tokens: 8,
                cache_write_tokens: 2,
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
                input_tokens: 50,
                output_tokens: 75,
                cache_read_tokens: 8,
                cache_write_tokens: 2,
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
                input_tokens: 15,
                output_tokens: 25,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
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
                input_tokens: 30,
                output_tokens: 60,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }
        );
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
