//! `openai-chat` SSE → `anthropic-messages` SSE.
//!
//! | `openai-chat` | `anthropic-messages` |
//! |---|---|
//! | first chunk | `message_start` |
//! | `delta.content` | `content_block_start` (text, once) + `content_block_delta` / `text_delta` |
//! | `delta.tool_calls[].function.name` | `content_block_start` (`tool_use`) |
//! | `delta.tool_calls[].function.arguments` | `content_block_delta` / `input_json_delta` |
//! | `finish_reason` | `message_delta.delta.stop_reason` |
//! | final `usage` chunk | `message_delta.usage` |
//! | `[DONE]` | `content_block_stop` + `message_delta` + `message_stop` |
//!
//! Three rules earn their own mention, because getting any of them wrong
//! produces a client that hangs or silently ignores half the response:
//!
//! 1. **Exactly one content block is open at a time**, and every block is
//!    closed before the next opens. Anthropic's stream is a sequence, not a
//!    set of interleaved channels.
//! 2. **The terminal events are emitted exactly once, on every path.**
//!    `[DONE]`, a `finish_reason`, a mid-stream error, or an upstream that just
//!    stops — all of them must still produce `message_stop`. A client waiting
//!    for `message_stop` waits forever, which is the worst failure available
//!    here.
//! 3. **`partial_json` fragments are forwarded verbatim.** Clients concatenate
//!    them and parse once at `content_block_stop`, so re-serialising a fragment
//!    (which is not valid JSON on its own) corrupts the tool call.

use serde_json::{json, Value};

use crate::usage::parse::find_event_boundary;

use super::{
    call_argument_fragment, delta_text, emit, error_message, event_data, ChatUsage,
    StreamConverter, MAX_EVENT_BYTES,
};

/// Which content block is currently open, and what it is.
enum OpenBlock {
    Text {
        index: u64,
    },
    Tool {
        index: u64,
        /// Which upstream `tool_calls[]` entry this block belongs to, so
        /// argument fragments can be matched to it. See [`ChatToAnthropic::tool_key`].
        key: i64,
    },
}

impl OpenBlock {
    fn index(&self) -> u64 {
        match self {
            Self::Text { index } | Self::Tool { index, .. } => *index,
        }
    }
}

/// Incremental converter from an OpenAI Chat SSE stream to an Anthropic
/// Messages SSE stream.
///
/// Fed raw upstream bytes by [`crate::translate::adapter`], which forwards
/// whatever comes back straight to the client. Never fails: an unparseable
/// event is skipped, because a translation error mid-stream cannot be reported
/// to a client that is already reading a 200.
pub struct ChatToAnthropic {
    /// Reported as the message's `model` when upstream chunks do not name one.
    model: String,
    /// Bytes of an event that has not been terminated yet.
    buffer: Vec<u8>,
    /// Whether `message_start` has gone out. Nothing else may precede it.
    started: bool,
    /// Whether the terminal events have gone out. Guards every path against
    /// emitting them twice.
    finished: bool,
    open: Option<OpenBlock>,
    /// Next content-block index to hand out. Anthropic block indices are
    /// per-message and strictly increasing.
    next_index: u64,
    /// Counter for tool calls that arrive without an upstream `index`. Counts
    /// *down* from 0 so a synthesized key can never collide with a real
    /// upstream index (which is always >= 0).
    synthetic_key: i64,
    /// From `finish_reason`, mapped once when it arrives — the chunk carrying
    /// it usually precedes the usage-only chunk, so it has to be remembered
    /// rather than acted on immediately.
    stop_reason: Option<&'static str>,
    saw_tool_use: bool,
    usage: ChatUsage,
}

impl ChatToAnthropic {
    /// `model` is what to report when upstream chunks do not name one.
    pub fn new(model: String) -> Self {
        Self {
            model,
            buffer: Vec::new(),
            started: false,
            finished: false,
            open: None,
            next_index: 0,
            synthetic_key: 0,
            stop_reason: None,
            saw_tool_use: false,
            usage: ChatUsage::default(),
        }
    }

    /// Feed raw upstream SSE bytes; returns the Anthropic SSE bytes to emit
    /// downstream, which is empty when the chunk only carried a partial event.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.buffer.extend_from_slice(chunk);

        // `find_event_boundary` (shared with `usage::parse`, which frames an
        // SSE stream the same way) accepts both a plain `\n\n` blank line
        // and a CRLF-framed `\r\n\r\n` one — an upstream that uses CRLF line
        // endings otherwise never produces a boundary this loop recognizes,
        // and the buffer grows until `MAX_EVENT_BYTES` discards it whole.
        while let Some((pos, sep_len)) = find_event_boundary(&self.buffer) {
            let event_end = pos + sep_len;
            let event: Vec<u8> = self.buffer.drain(..event_end).collect();
            self.handle_event(&event[..pos], &mut out);
        }

        if self.buffer.len() > MAX_EVENT_BYTES {
            self.buffer.clear();
        }

        out
    }

    /// The closing events, for a stream that ended without a `[DONE]` of its
    /// own — a truncated generation, or a provider that simply closes the
    /// connection. Idempotent: a second call, or a call after `[DONE]` or an
    /// error frame already closed the stream, returns empty.
    ///
    /// A connection can also close right after the final `data:` line with
    /// no trailing blank line — that leftover partial event is still a
    /// complete, parseable one (often the one carrying `usage`, per
    /// `openai-chat`'s convention of a final usage-only chunk), so it is run
    /// through `handle_event` here rather than dropped. Same handling as
    /// `usage::parse::SseUsageScanner::finish`.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let event = std::mem::take(&mut self.buffer);
            self.handle_event(&event, &mut out);
        }
        self.emit_terminal(&mut out);
        out
    }

    /// Handle one complete event's bytes (without the trailing separator).
    fn handle_event(&mut self, event: &[u8], out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        let Ok(text) = std::str::from_utf8(event) else {
            return;
        };
        let data = event_data(text);
        if data.is_empty() {
            return;
        }
        // `[DONE]` is the end of the chat stream, and the right moment to close
        // the Anthropic one: waiting for the connection to drop instead would
        // delay `message_stop` behind whatever the provider does next.
        if data == "[DONE]" {
            self.emit_terminal(out);
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
            return;
        };

        self.start_message(&chunk, out);

        // Usage can ride on any chunk: OpenAI sends it alone at the end,
        // others attach it to the final content chunk.
        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.record_usage(usage);
        }

        // Some providers report a mid-generation failure as a frame inside an
        // otherwise-successful stream. Anthropic has an event for exactly
        // that, and it is terminal — no `message_stop` follows an `error`.
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            self.close_open_block(out);
            emit(
                out,
                "error",
                json!({
                    "type": "error",
                    "error": { "type": "api_error", "message": error_message(error) },
                }),
            );
            self.finished = true;
            return;
        }

        let Some(choice) = chunk.get("choices").and_then(|c| c.get(0)) else {
            return;
        };
        if let Some(delta) = choice.get("delta") {
            self.handle_text(delta, out);
            self.handle_tool_calls(delta, out);
        }
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            self.stop_reason = Some(stop_reason_from_finish_reason(finish_reason));
        }
    }

    /// Emit `message_start`, once, before anything else can go out.
    ///
    /// `chunk` may be `Value::Null` (called from [`Self::emit_terminal`] for a
    /// stream that produced nothing parseable) — every accessor here tolerates
    /// that.
    fn start_message(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;

        let id = match chunk.get("id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => {
                if id.starts_with("msg_") {
                    id.to_string()
                } else {
                    format!("msg_{id}")
                }
            }
            _ => format!("msg_{}", uuid::Uuid::now_v7()),
        };
        let model = chunk
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|m| !m.is_empty())
            .map(String::from)
            .unwrap_or_else(|| self.model.clone());

        emit(
            out,
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    // Genuinely unknown at this point: `openai-chat` reports
                    // prompt tokens only in its final usage chunk. Corrected in
                    // `message_delta`, which current Anthropic streams also use
                    // to restate the full usage.
                    "usage": { "input_tokens": self.usage.non_cached_prompt(), "output_tokens": 0 },
                },
            }),
        );
    }

    fn record_usage(&mut self, usage: &Value) {
        self.usage.record(usage);
    }

    fn handle_text(&mut self, delta: &Value, out: &mut Vec<u8>) {
        let text = delta_text(delta);
        if text.is_empty() {
            return;
        }

        let index = self.ensure_text_block(out);
        emit(
            out,
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "text_delta", "text": text },
            }),
        );
    }

    /// The index of an open text block, opening one (and closing whatever else
    /// was open) if needed. Text arriving after a tool call simply starts a new
    /// text block — Anthropic permits several per message.
    fn ensure_text_block(&mut self, out: &mut Vec<u8>) -> u64 {
        if let Some(OpenBlock::Text { index }) = &self.open {
            return *index;
        }
        self.close_open_block(out);

        let index = self.next_index;
        self.next_index += 1;
        emit(
            out,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": { "type": "text", "text": "" },
            }),
        );
        self.open = Some(OpenBlock::Text { index });
        index
    }

    fn handle_tool_calls(&mut self, delta: &Value, out: &mut Vec<u8>) {
        let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
            return;
        };

        for call in calls {
            let function = call.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .filter(|n| !n.is_empty());

            let index = match self.tool_key(call, name.is_some()) {
                // A fragment for the tool block already open: keep filling it.
                Some(key) if self.open_tool_key() == Some(key) => {
                    self.open.as_ref().map(OpenBlock::index)
                }
                Some(key) => Some(self.open_tool_block(key, call, name, out)),
                // No `index` and no `name`: an argument fragment, which belongs
                // to the most recently opened tool block.
                None => match &self.open {
                    Some(block @ OpenBlock::Tool { .. }) => Some(block.index()),
                    // Nothing to attach it to — dropping it is the only option
                    // that does not invent a tool call the model never made.
                    _ => None,
                },
            };
            let Some(index) = index else {
                continue;
            };

            if let Some(fragment) = call_argument_fragment(function) {
                emit(
                    out,
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": fragment },
                    }),
                );
            }
        }
    }

    /// Which tool call an upstream entry refers to.
    ///
    /// `Some(key)` identifies a specific call: the upstream `index` when it is
    /// there, or a freshly synthesized negative key when it is not but a `name`
    /// is (Ollama omits `index`, so a named entry always starts a new call).
    /// `None` means "the one already open" — an argument-only fragment.
    fn tool_key(&mut self, call: &Value, named: bool) -> Option<i64> {
        match call.get("index").and_then(|v| v.as_i64()) {
            Some(index) => Some(index),
            None if named => {
                self.synthetic_key -= 1;
                Some(self.synthetic_key)
            }
            None => None,
        }
    }

    fn open_tool_key(&self) -> Option<i64> {
        match &self.open {
            Some(OpenBlock::Tool { key, .. }) => Some(*key),
            _ => None,
        }
    }

    fn open_tool_block(
        &mut self,
        key: i64,
        call: &Value,
        name: Option<&str>,
        out: &mut Vec<u8>,
    ) -> u64 {
        self.close_open_block(out);

        let id = call
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::now_v7()));
        let index = self.next_index;
        self.next_index += 1;

        emit(
            out,
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": {
                    "type": "tool_use",
                    "id": id,
                    "name": name.unwrap_or(""),
                    "input": {},
                },
            }),
        );
        self.open = Some(OpenBlock::Tool { index, key });
        self.saw_tool_use = true;
        index
    }

    fn close_open_block(&mut self, out: &mut Vec<u8>) {
        if let Some(block) = self.open.take() {
            emit(
                out,
                "content_block_stop",
                json!({ "type": "content_block_stop", "index": block.index() }),
            );
        }
    }

    /// `content_block_stop` (if a block is open) + `message_delta` +
    /// `message_stop`, once per stream.
    fn emit_terminal(&mut self, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        // Even a stream that produced nothing parseable gets a well-formed
        // sequence: a client that never sees `message_start` cannot make sense
        // of `message_stop` either.
        self.start_message(&Value::Null, out);
        self.close_open_block(out);

        // A tool call in the content always wins over the reported finish
        // reason: several openai-chat servers say `stop` while streaming tool
        // calls, and an Anthropic client that sees `end_turn` never executes
        // the tool it was just handed.
        let stop_reason = if self.saw_tool_use {
            "tool_use"
        } else {
            self.stop_reason.unwrap_or("end_turn")
        };

        emit(
            out,
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                "usage": {
                    // `self.usage.prompt` includes `cached`, but Anthropic's
                    // `input_tokens`/`cache_read_input_tokens` are exclusive
                    // — reporting it verbatim would double-count the cached
                    // portion for a client that sums the two (#24).
                    "input_tokens": self.usage.non_cached_prompt(),
                    "output_tokens": self.usage.completion,
                    "cache_read_input_tokens": self.usage.cached,
                    "cache_creation_input_tokens": 0,
                },
            }),
        );
        emit(out, "message_stop", json!({ "type": "message_stop" }));
        self.finished = true;
    }
}

impl StreamConverter for ChatToAnthropic {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.push(chunk)
    }

    fn finish(&mut self) -> Vec<u8> {
        self.finish()
    }
}

/// `openai-chat` `finish_reason` → Anthropic `stop_reason`.
///
/// `content_filter` becomes `end_turn` rather than `refusal`: the refusal came
/// from a provider-side filter, not from the model declining, and clients treat
/// `refusal` as a statement about the model.
fn stop_reason_from_finish_reason(finish_reason: &str) -> &'static str {
    match finish_reason {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Split every emitted frame into `(event name, payload)`.
    fn frames(bytes: &[u8]) -> Vec<(String, Value)> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .map(|frame| {
                let name = frame
                    .lines()
                    .find_map(|l| l.strip_prefix("event: "))
                    .unwrap_or("")
                    .to_string();
                let data = event_data(frame);
                (name, serde_json::from_str(&data).unwrap())
            })
            .collect()
    }

    fn names(bytes: &[u8]) -> Vec<String> {
        frames(bytes).into_iter().map(|(name, _)| name).collect()
    }

    fn chunk(value: Value) -> String {
        format!("data: {value}\n\n")
    }

    fn text_chunk(content: &str, finish_reason: Option<&str>) -> String {
        chunk(json!({
            "id": "chatcmpl-1",
            "model": "qwen3.5",
            "choices": [{
                "index": 0,
                "delta": { "content": content },
                "finish_reason": finish_reason,
            }],
        }))
    }

    #[test]
    fn event_data_joins_crlf_framed_multiline_data_lines() {
        // Per the SSE spec, multiple `data:` lines within one event are
        // joined with `\n`. Under CRLF framing that means each line (as
        // produced by splitting the event on `\n`) carries a trailing `\r`,
        // not a leading one — stripping the wrong end leaves the `\r` stuck
        // between the two lines' payloads instead of removed, which splits a
        // token (here, the digits of `15`) and breaks JSON parsing.
        let event = "data: {\"choices\": [], \"usage\": {\"prompt_tokens\": 1\r\n\
                      data: 5, \"completion_tokens\": 25}}";
        let data = event_data(event);
        let json: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(json["usage"]["prompt_tokens"], 15);
        assert_eq!(json["usage"]["completion_tokens"], 25);
    }

    #[test]
    fn a_text_stream_produces_the_full_anthropic_event_sequence() {
        let mut converter = ChatToAnthropic::new("fallback".to_string());
        let mut out = converter.push(text_chunk("Hel", None).as_bytes());
        out.extend(converter.push(text_chunk("lo", Some("stop")).as_bytes()));
        out.extend(converter.push(b"data: [DONE]\n\n"));
        out.extend(converter.finish());

        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let events = frames(&out);
        assert_eq!(events[0].1["message"]["id"], "msg_chatcmpl-1");
        assert_eq!(events[0].1["message"]["model"], "qwen3.5");
        assert_eq!(events[2].1["delta"]["text"], "Hel");
        assert_eq!(events[3].1["delta"]["text"], "lo");
        assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn the_done_sentinel_is_consumed_and_never_forwarded() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(text_chunk("hi", Some("stop")).as_bytes());
        out.extend(converter.push(b"data: [DONE]\n\n"));

        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("[DONE]"), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
    }

    #[test]
    fn events_split_across_chunk_boundaries_are_reassembled() {
        let whole = text_chunk("hello", Some("stop"));
        let (a, b) = whole.split_at(whole.len() / 2);

        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(a.as_bytes());
        out.extend(converter.push(b.as_bytes()));
        out.extend(converter.finish());

        let events = frames(&out);
        let deltas: Vec<&Value> = events
            .iter()
            .filter(|(name, _)| name == "content_block_delta")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0]["delta"]["text"], "hello");
    }

    #[test]
    fn two_events_in_one_chunk_are_both_converted() {
        let combined = format!("{}{}", text_chunk("a", None), text_chunk("b", None));
        let mut converter = ChatToAnthropic::new("m".to_string());
        let out = converter.push(combined.as_bytes());

        let events = frames(&out);
        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
            ]
        );
        assert_eq!(events[2].1["delta"]["text"], "a");
        assert_eq!(events[3].1["delta"]["text"], "b");
    }

    #[test]
    fn crlf_framed_events_are_converted_same_as_lf() {
        // Some upstreams (and the proxies in front of them) frame SSE with
        // `\r\n` line endings, so the event separator is `\r\n\r\n` rather
        // than `\n\n` — before `find_event_boundary` was wired in here, a
        // converter fed this never found a boundary at all, and the whole
        // stream sat unparsed in the buffer until `MAX_EVENT_BYTES` dropped
        // it.
        let crlf = text_chunk("hello", Some("stop")).replace('\n', "\r\n");
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(crlf.as_bytes());
        out.extend(converter.finish());

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"text\":\"hello\""), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
    }

    #[test]
    fn finish_flushes_a_final_event_with_no_trailing_blank_line() {
        // A connection can close right after the final `data:` line with no
        // terminating blank line — that leftover event (often the one
        // carrying the finish reason) is still complete and parseable, and
        // must not be silently dropped by `finish`.
        let unterminated = text_chunk("hi", Some("stop"));
        let unterminated = unterminated.strip_suffix("\n\n").unwrap();
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(unterminated.as_bytes());
        assert!(
            !String::from_utf8_lossy(&out).contains("content_block_delta"),
            "an event with no separator yet must not be handled early"
        );
        out.extend(converter.finish());

        let events = frames(&out);
        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        // "hi" only appears if the unterminated event was actually run
        // through `handle_event` by `finish` — dropped, the stream would
        // still be well-formed (`emit_terminal` alone produces a valid
        // empty sequence) but silently missing the content.
        assert_eq!(events[2].1["delta"]["text"], "hi");
    }

    #[test]
    fn a_tool_call_streamed_in_fragments_becomes_one_tool_use_block() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(
            chunk(json!({
                "id": "c1",
                "choices": [{"index": 0, "delta": {"tool_calls": [{
                    "index": 0, "id": "call_1", "type": "function",
                    "function": {"name": "read_file", "arguments": ""},
                }]}}],
            }))
            .as_bytes(),
        );
        out.extend(
            converter.push(
                chunk(json!({
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": 0, "function": {"arguments": "{\"path\":"},
                    }]}}],
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                chunk(json!({
                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                        "index": 0, "function": {"arguments": "\"a.rs\"}"},
                    }]}}],
                    "finish_reason": "tool_calls",
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.finish());

        let events = frames(&out);
        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        assert_eq!(events[1].1["content_block"]["type"], "tool_use");
        assert_eq!(events[1].1["content_block"]["id"], "call_1");
        assert_eq!(events[1].1["content_block"]["name"], "read_file");
        // Fragments are forwarded verbatim; the client concatenates them.
        assert_eq!(events[2].1["delta"]["partial_json"], "{\"path\":");
        assert_eq!(events[3].1["delta"]["partial_json"], "\"a.rs\"}");
        assert_eq!(events[5].1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn a_whole_tool_call_without_an_index_or_id_still_becomes_a_block() {
        // Ollama's shape: one chunk, complete arguments, no `index`, no `id`.
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(
            chunk(json!({
                "choices": [{"delta": {"tool_calls": [{
                    "function": {"name": "bash", "arguments": "{\"cmd\":\"ls\"}"},
                }]}}],
            }))
            .as_bytes(),
        );
        out.extend(converter.finish());

        let events = frames(&out);
        assert_eq!(events[1].0, "content_block_start");
        assert_eq!(events[1].1["content_block"]["name"], "bash");
        assert!(events[1].1["content_block"]["id"]
            .as_str()
            .unwrap()
            .starts_with("toolu_"));
        assert_eq!(events[2].1["delta"]["partial_json"], "{\"cmd\":\"ls\"}");
    }

    #[test]
    fn arguments_sent_as_an_object_are_serialised_once() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let out = converter.push(
            chunk(json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0, "id": "call_1",
                    "function": {"name": "f", "arguments": {"a": 1}},
                }]}}],
            }))
            .as_bytes(),
        );

        let events = frames(&out);
        assert_eq!(events[2].1["delta"]["partial_json"], "{\"a\":1}");
    }

    #[test]
    fn two_tool_calls_get_their_own_blocks_and_stop_events() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(
            chunk(json!({
                "choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "c1", "function": {"name": "a", "arguments": "{}"}},
                ]}}],
            }))
            .as_bytes(),
        );
        out.extend(
            converter.push(
                chunk(json!({
                    "choices": [{"delta": {"tool_calls": [
                        {"index": 1, "id": "c2", "function": {"name": "b", "arguments": "{}"}},
                    ]}}],
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.finish());

        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );
        let events = frames(&out);
        assert_eq!(events[1].1["index"], 0);
        assert_eq!(events[4].1["index"], 1);
    }

    #[test]
    fn text_after_a_tool_call_opens_a_new_text_block() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(text_chunk("thinking out loud", None).as_bytes());
        out.extend(
            converter.push(
                chunk(json!({
                    "choices": [{"delta": {"tool_calls": [
                        {"index": 0, "id": "c1", "function": {"name": "a", "arguments": "{}"}},
                    ]}}],
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.push(text_chunk("and more", None).as_bytes()));
        out.extend(converter.finish());

        let events = frames(&out);
        let starts: Vec<u64> = events
            .iter()
            .filter(|(name, _)| name == "content_block_start")
            .map(|(_, value)| value["index"].as_u64().unwrap())
            .collect();
        assert_eq!(starts, vec![0, 1, 2]);
        // Every block that opened also closed.
        assert_eq!(
            events
                .iter()
                .filter(|(name, _)| name == "content_block_stop")
                .count(),
            3
        );
    }

    #[test]
    fn a_reported_stop_is_overridden_when_tool_calls_were_streamed() {
        // Several openai-chat servers do exactly this, and an Anthropic client
        // that believes `end_turn` never runs the tool.
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(
            chunk(json!({
                "choices": [{
                    "delta": {"tool_calls": [
                        {"index": 0, "id": "c1", "function": {"name": "a", "arguments": "{}"}},
                    ]},
                    "finish_reason": "stop",
                }],
            }))
            .as_bytes(),
        );
        out.extend(converter.finish());

        let events = frames(&out);
        let (_, delta) = events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn max_tokens_is_reported_when_the_upstream_hit_its_limit() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(text_chunk("cut off", Some("length")).as_bytes());
        out.extend(converter.finish());

        let events = frames(&out);
        let (_, delta) = events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "max_tokens");
    }

    #[test]
    fn the_usage_only_final_chunk_feeds_message_delta() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(text_chunk("hi", Some("stop")).as_bytes());
        out.extend(
            converter.push(
                chunk(json!({
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 41,
                        "completion_tokens": 7,
                        "prompt_tokens_details": {"cached_tokens": 12},
                    },
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.push(b"data: [DONE]\n\n"));

        let events = frames(&out);
        let (_, delta) = events
            .iter()
            .find(|(name, _)| name == "message_delta")
            .unwrap();
        // #24: `prompt_tokens` (41) includes `cached_tokens` (12); Anthropic's
        // `input_tokens` excludes it, so the client-facing value is the
        // non-cached remainder (29), not 41 — otherwise a client that sums
        // `input_tokens + cache_read_input_tokens` double-counts the cache.
        assert_eq!(delta["usage"]["input_tokens"], 29);
        assert_eq!(delta["usage"]["output_tokens"], 7);
        assert_eq!(delta["usage"]["cache_read_input_tokens"], 12);
    }

    #[test]
    fn a_truncated_stream_is_still_closed_by_finish() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(text_chunk("half a sen", None).as_bytes());
        out.extend(converter.finish());

        let emitted = names(&out);
        assert!(
            emitted.contains(&"content_block_stop".to_string()),
            "{emitted:?}"
        );
        assert!(emitted.contains(&"message_stop".to_string()), "{emitted:?}");
    }

    #[test]
    fn finish_is_idempotent() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        converter.push(text_chunk("hi", Some("stop")).as_bytes());
        assert!(!converter.finish().is_empty());
        assert!(converter.finish().is_empty());
    }

    #[test]
    fn a_stream_that_produced_nothing_still_gets_a_well_formed_sequence() {
        let mut converter = ChatToAnthropic::new("fallback-model".to_string());
        let out = converter.finish();

        let events = frames(&out);
        assert_eq!(
            names(&out),
            vec!["message_start", "message_delta", "message_stop"]
        );
        assert_eq!(events[0].1["message"]["model"], "fallback-model");
        assert_eq!(events[1].1["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn a_mid_stream_error_frame_becomes_an_anthropic_error_event() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(text_chunk("partial", None).as_bytes());
        out.extend(
            converter
                .push(chunk(json!({ "error": {"message": "upstream model crashed"} })).as_bytes()),
        );
        // The error is terminal: nothing more may be emitted after it.
        out.extend(converter.finish());

        let events = frames(&out);
        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "error",
            ]
        );
        assert_eq!(events[4].1["error"]["message"], "upstream model crashed");
    }

    #[test]
    fn a_string_valued_error_is_still_reported() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let out = converter.push(chunk(json!({ "error": "model not loaded" })).as_bytes());

        let events = frames(&out);
        let (_, error) = events.iter().find(|(name, _)| name == "error").unwrap();
        assert_eq!(error["error"]["message"], "model not loaded");
    }

    #[test]
    fn reasoning_deltas_are_dropped_rather_than_shown_as_the_answer() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let out = converter.push(
            chunk(json!({
                "id": "c1",
                "choices": [{"delta": {"reasoning_content": "let me think"}}],
            }))
            .as_bytes(),
        );

        // `message_start` only: no content block was opened.
        assert_eq!(names(&out), vec!["message_start"]);
    }

    #[test]
    fn unparseable_and_empty_frames_are_skipped() {
        let mut converter = ChatToAnthropic::new("m".to_string());
        let mut out = converter.push(b": keep-alive comment\n\n");
        out.extend(converter.push(b"data: not json at all\n\n"));
        out.extend(converter.push(b"\n\n"));
        assert!(out.is_empty());

        out.extend(converter.push(text_chunk("recovered", None).as_bytes()));
        let events = frames(&out);
        assert_eq!(events[2].1["delta"]["text"], "recovered");
    }
}
