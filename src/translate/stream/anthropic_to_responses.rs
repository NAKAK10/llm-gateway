//! `anthropic-messages` SSE → `openai-responses` SSE.
//!
//! The mirror image of [`super::chat_to_responses::ChatToResponses`]'s
//! problem: that converter has to *invent* item boundaries from `openai-chat`'s
//! flat deltas. Anthropic already tells us, explicitly, when a content block
//! opens and closes (`content_block_start` / `content_block_stop`) — so this
//! converter's job is simpler, not harder: translate one already-structured
//! sequence into another.
//!
//! | `anthropic-messages` | `openai-responses` |
//! |---|---|
//! | `message_start` | `response.created` + `response.in_progress` |
//! | `content_block_start` (`text`) | `response.output_item.added` + `response.content_part.added` |
//! | `content_block_start` (`tool_use`) | `response.output_item.added` |
//! | `content_block_delta` / `text_delta` | `response.output_text.delta` |
//! | `content_block_delta` / `input_json_delta` | `response.function_call_arguments.delta` |
//! | `content_block_stop` | the closing `…done` pair for the block's kind + `response.output_item.done` |
//! | `message_delta.delta.stop_reason` | the terminal event's `status` |
//! | `message_delta.usage` / `message_start.message.usage` | the terminal event's `usage` |
//! | `message_stop` | `response.completed` / `response.incomplete` |
//! | `error` | `response.failed` |
//!
//! The same three invariants documented on
//! [`super::chat_to_anthropic`]/[`super::chat_to_responses`] hold here too:
//! exactly one item open at a time, a terminal event exactly once on every
//! path (including a stream that never sends `message_stop`), and
//! `partial_json`/`input_json_delta` fragments are forwarded verbatim, never
//! re-serialized — the client concatenates them and parses once.
//!
//! The event vocabulary consumed here (`message_start`, `content_block_start`,
//! `content_block_delta` with `text_delta`/`input_json_delta`,
//! `content_block_stop`, `message_delta`, `message_stop`, `error`) is exactly
//! what [`super::chat_to_anthropic::ChatToAnthropic`] already *produces* on
//! its output side, and what a real Anthropic Messages stream (or the
//! `claude-cli` agent transport's own `CliToAnthropic` emitter, see
//! `agent::claude_cli`) sends on the wire.

use serde_json::{json, Value};

use crate::usage::parse::find_event_boundary;

use super::{emit, error_message, event_data, StreamConverter, MAX_EVENT_BYTES};

/// Which `output` item is currently open, and what it is. Unlike
/// [`super::chat_to_responses::OpenItem`], no synthetic key-matching is
/// needed: Anthropic's own `index` on the content block tells us unambiguously
/// which block a delta belongs to.
enum OpenItem {
    Message {
        output_index: u64,
        item_id: String,
        /// Accumulated so far, so the closing `output_text.done` and the
        /// item's own `content` in `output_item.done` can restate the whole
        /// text.
        text: String,
        /// The Anthropic content-block index this item corresponds to, so a
        /// delta naming a different index (should not happen, but this
        /// converter tolerates a malformed stream) is not misapplied.
        block_index: u64,
    },
    FunctionCall {
        output_index: u64,
        item_id: String,
        call_id: String,
        name: String,
        /// Accumulated so far, for the same reason as `Message::text`.
        arguments: String,
        block_index: u64,
    },
}

/// The last usage numbers seen, in Anthropic's own field names. Anthropic
/// splits usage across two events (`message_start` reports `input_tokens`
/// early; `message_delta` corrects `output_tokens` and adds cache figures at
/// the end), so each field is updated independently rather than all at once
/// the way `ChatUsage` does for a single combined `usage` object.
#[derive(Default)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
}

impl AnthropicUsage {
    fn record(&mut self, usage: &Value) {
        if let Some(v) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
            self.input_tokens = v;
        }
        if let Some(v) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
            self.output_tokens = v;
        }
        if let Some(v) = usage
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64())
        {
            self.cache_read = v;
        }
    }
}

/// Incremental converter from an Anthropic Messages SSE stream to an OpenAI
/// Responses SSE stream. See the module docs for the event mapping and why
/// this side of the problem is simpler than
/// [`super::chat_to_responses::ChatToResponses`]'s.
pub struct AnthropicToResponses {
    /// Reported as the response's `model` when upstream events do not name
    /// one.
    model: String,
    /// The `model` actually reported once `response.created` has gone out.
    resolved_model: String,
    /// Bytes of an event that has not been terminated yet.
    buffer: Vec<u8>,
    /// Whether `response.created`/`response.in_progress` have gone out.
    started: bool,
    /// Whether the terminal event has gone out. Guards every path against
    /// emitting it twice.
    finished: bool,
    response_id: String,
    /// The next `sequence_number` to hand out.
    sequence: u64,
    /// Next output-item index to hand out.
    next_output_index: u64,
    open: Option<OpenItem>,
    /// Every item that has been closed so far, in the shape it must appear
    /// in `response.completed`'s `output` array.
    completed_output: Vec<Value>,
    /// From `message_delta.delta.stop_reason`: `"incomplete"` for
    /// `max_tokens`, `"completed"` for anything else (including no
    /// `message_delta` at all, when the stream ends early).
    stop_status: Option<&'static str>,
    usage: AnthropicUsage,
}

impl AnthropicToResponses {
    /// `model` is what to report when upstream events do not name one.
    pub fn new(model: String) -> Self {
        Self {
            model,
            resolved_model: String::new(),
            buffer: Vec::new(),
            started: false,
            finished: false,
            response_id: String::new(),
            sequence: 0,
            next_output_index: 0,
            open: None,
            completed_output: Vec::new(),
            stop_status: None,
            usage: AnthropicUsage::default(),
        }
    }

    /// Feed raw upstream SSE bytes; returns the Responses SSE bytes to emit
    /// downstream, which is empty when the chunk only carried a partial
    /// event.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.buffer.extend_from_slice(chunk);

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

    /// The closing events, for a stream that ended without its own
    /// `message_stop`. Idempotent, and flushes a leftover unterminated event
    /// first, same as every other converter in this module.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let event = std::mem::take(&mut self.buffer);
            self.handle_event(&event, &mut out);
        }
        self.emit_terminal(&mut out);
        out
    }

    /// One SSE frame, with `type` and `sequence_number` filled in from
    /// `self.sequence` — every Responses streaming event carries both.
    fn emit_seq(&mut self, out: &mut Vec<u8>, event: &str, mut data: Value) {
        self.sequence += 1;
        if let Value::Object(map) = &mut data {
            map.insert("type".to_string(), json!(event));
            map.insert("sequence_number".to_string(), json!(self.sequence));
        }
        emit(out, event, data);
    }

    fn handle_event(&mut self, event: &[u8], out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        let Ok(text) = std::str::from_utf8(event) else {
            return;
        };
        let raw = event_data(text);
        if raw.is_empty() {
            return;
        }
        let Ok(data) = serde_json::from_str::<Value>(&raw) else {
            return;
        };

        self.start_response(&data, out);

        match data.get("type").and_then(|t| t.as_str()) {
            Some("content_block_start") => self.handle_content_block_start(&data, out),
            Some("content_block_delta") => self.handle_content_block_delta(&data, out),
            Some("content_block_stop") => self.close_open_item(out),
            Some("message_delta") => {
                if let Some(stop_reason) = data
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(|s| s.as_str())
                {
                    self.stop_status = Some(if stop_reason == "max_tokens" {
                        "incomplete"
                    } else {
                        "completed"
                    });
                }
                if let Some(usage) = data.get("usage") {
                    self.usage.record(usage);
                }
            }
            Some("message_stop") => self.emit_terminal(out),
            // A mid-generation failure. Anthropic treats this as terminal in
            // its own right — no `message_stop` follows an `error` — so
            // `response.failed` is emitted directly rather than waiting for
            // a terminal event that will never come.
            Some("error") => self.handle_error(&data, out),
            // `message_start` is already handled by `start_response` above;
            // anything else this converter does not recognize is dropped.
            _ => {}
        }
    }

    /// Emit `response.created` + `response.in_progress`, once, before
    /// anything else can go out.
    ///
    /// `data` may be any event (or `Value::Null`, from [`Self::emit_terminal`]
    /// for a stream that produced nothing parseable) — only a real
    /// `message_start` event carries a `message` field, so every other call
    /// falls through to the defaults harmlessly.
    fn start_response(&mut self, data: &Value, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;

        let message = data.get("message");
        let id = match message.and_then(|m| m.get("id")).and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => {
                if id.starts_with("resp_") {
                    id.to_string()
                } else {
                    format!("resp_{id}")
                }
            }
            _ => format!("resp_{}", uuid::Uuid::now_v7()),
        };
        self.response_id = id.clone();

        let model = message
            .and_then(|m| m.get("model"))
            .and_then(|v| v.as_str())
            .filter(|m| !m.is_empty())
            .map(String::from)
            .unwrap_or_else(|| self.model.clone());
        self.resolved_model = model.clone();

        if let Some(usage) = message.and_then(|m| m.get("usage")) {
            self.usage.record(usage);
        }

        let response = json!({
            "id": id, "object": "response", "status": "in_progress",
            "model": model, "output": [],
        });
        self.emit_seq(
            out,
            "response.created",
            json!({ "response": response.clone() }),
        );
        self.emit_seq(out, "response.in_progress", json!({ "response": response }));
    }

    fn handle_content_block_start(&mut self, data: &Value, out: &mut Vec<u8>) {
        // Defensive: a well-formed Anthropic stream always closes the
        // previous block first, but this converter tolerates one that does
        // not rather than losing track of an item that never closes.
        self.close_open_item(out);

        let index = data.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
        let block = data.get("content_block");

        match block.and_then(|b| b.get("type")).and_then(|t| t.as_str()) {
            Some("tool_use") => {
                let call_id = block
                    .and_then(|b| b.get("id"))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| format!("call_{}", uuid::Uuid::now_v7()));
                let name = block
                    .and_then(|b| b.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let output_index = self.next_output_index;
                self.next_output_index += 1;
                let item_id = format!("fc_{}", uuid::Uuid::now_v7());
                self.emit_seq(
                    out,
                    "response.output_item.added",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "id": item_id.clone(), "type": "function_call", "call_id": call_id.clone(),
                            "name": name.clone(), "arguments": "", "status": "in_progress",
                        },
                    }),
                );
                self.open = Some(OpenItem::FunctionCall {
                    output_index,
                    item_id,
                    call_id,
                    name,
                    arguments: String::new(),
                    block_index: index,
                });
            }
            Some("text") => {
                let output_index = self.next_output_index;
                self.next_output_index += 1;
                let item_id = format!("msg_{}", uuid::Uuid::now_v7());
                self.emit_seq(
                    out,
                    "response.output_item.added",
                    json!({
                        "output_index": output_index,
                        "item": {
                            "id": item_id.clone(), "type": "message", "role": "assistant",
                            "status": "in_progress", "content": [],
                        },
                    }),
                );
                self.emit_seq(
                    out,
                    "response.content_part.added",
                    json!({
                        "item_id": item_id, "output_index": output_index, "content_index": 0,
                        "part": { "type": "output_text", "text": "" },
                    }),
                );
                self.open = Some(OpenItem::Message {
                    output_index,
                    item_id,
                    text: String::new(),
                    block_index: index,
                });
            }
            // `thinking` / `redacted_thinking` and any future block type
            // carry nothing a Responses output item can represent — dropped
            // rather than guessed at. The block's own `content_block_stop`
            // then finds nothing open and is a no-op.
            _ => {}
        }
    }

    fn handle_content_block_delta(&mut self, data: &Value, out: &mut Vec<u8>) {
        let Some(index) = data.get("index").and_then(|v| v.as_u64()) else {
            return;
        };
        let delta = data.get("delta");

        enum Kind {
            Text,
            Tool,
        }
        let matched = match &self.open {
            Some(OpenItem::Message {
                block_index,
                output_index,
                item_id,
                ..
            }) if *block_index == index => Some((Kind::Text, *output_index, item_id.clone())),
            Some(OpenItem::FunctionCall {
                block_index,
                output_index,
                item_id,
                ..
            }) if *block_index == index => Some((Kind::Tool, *output_index, item_id.clone())),
            _ => None,
        };
        let Some((kind, output_index, item_id)) = matched else {
            return;
        };

        match kind {
            Kind::Text => {
                if delta.and_then(|d| d.get("type")).and_then(|t| t.as_str()) != Some("text_delta")
                {
                    return;
                }
                let text = delta
                    .and_then(|d| d.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if text.is_empty() {
                    return;
                }
                if let Some(OpenItem::Message { text: buf, .. }) = &mut self.open {
                    buf.push_str(text);
                }
                self.emit_seq(
                    out,
                    "response.output_text.delta",
                    json!({
                        "item_id": item_id, "output_index": output_index, "content_index": 0,
                        "delta": text,
                    }),
                );
            }
            Kind::Tool => {
                if delta.and_then(|d| d.get("type")).and_then(|t| t.as_str())
                    != Some("input_json_delta")
                {
                    return;
                }
                // Forwarded verbatim, never reparsed: a `partial_json`
                // fragment is not valid JSON on its own, and Responses'
                // `function_call_arguments.delta` carries the same
                // string-fragment contract.
                let fragment = delta
                    .and_then(|d| d.get("partial_json"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if fragment.is_empty() {
                    return;
                }
                if let Some(OpenItem::FunctionCall { arguments, .. }) = &mut self.open {
                    arguments.push_str(fragment);
                }
                self.emit_seq(
                    out,
                    "response.function_call_arguments.delta",
                    json!({ "item_id": item_id, "output_index": output_index, "delta": fragment }),
                );
            }
        }
    }

    /// Close whatever item is open: the `…done` pair for its kind, plus
    /// `response.output_item.done`, and record its final shape in
    /// `completed_output` for `response.completed`'s `output` array.
    fn close_open_item(&mut self, out: &mut Vec<u8>) {
        let Some(item) = self.open.take() else {
            return;
        };
        match item {
            OpenItem::Message {
                output_index,
                item_id,
                text,
                ..
            } => {
                self.emit_seq(
                    out,
                    "response.output_text.done",
                    json!({
                        "item_id": item_id.clone(), "output_index": output_index, "content_index": 0,
                        "text": text.clone(),
                    }),
                );
                let part = json!({ "type": "output_text", "annotations": [], "text": text });
                self.emit_seq(
                    out,
                    "response.content_part.done",
                    json!({
                        "item_id": item_id.clone(), "output_index": output_index, "content_index": 0,
                        "part": part.clone(),
                    }),
                );
                let item_json = json!({
                    "type": "message", "id": item_id, "role": "assistant",
                    "status": "completed", "content": [part],
                });
                self.emit_seq(
                    out,
                    "response.output_item.done",
                    json!({ "output_index": output_index, "item": item_json.clone() }),
                );
                self.completed_output.push(item_json);
            }
            OpenItem::FunctionCall {
                output_index,
                item_id,
                call_id,
                name,
                arguments,
                ..
            } => {
                self.emit_seq(
                    out,
                    "response.function_call_arguments.done",
                    json!({
                        "item_id": item_id.clone(), "output_index": output_index,
                        "arguments": arguments.clone(),
                    }),
                );
                let item_json = json!({
                    "type": "function_call", "id": item_id, "call_id": call_id,
                    "name": name, "arguments": arguments, "status": "completed",
                });
                self.emit_seq(
                    out,
                    "response.output_item.done",
                    json!({ "output_index": output_index, "item": item_json.clone() }),
                );
                self.completed_output.push(item_json);
            }
        }
    }

    fn handle_error(&mut self, data: &Value, out: &mut Vec<u8>) {
        self.close_open_item(out);
        let message = data
            .get("error")
            .map(error_message)
            .unwrap_or_else(|| "unknown upstream error".to_string());
        let response = json!({
            "id": self.response_id.clone(),
            "object": "response",
            "status": "failed",
            "model": self.resolved_model.clone(),
            "output": self.completed_output.clone(),
            "error": { "message": message, "type": "api_error", "code": Value::Null },
        });
        self.emit_seq(out, "response.failed", json!({ "response": response }));
        self.finished = true;
    }

    /// `response.completed` (or `.incomplete`), once per stream — closing
    /// whatever item is still open first.
    fn emit_terminal(&mut self, out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        // Even a stream that produced nothing parseable gets a well-formed
        // sequence: a client that never saw `response.created` cannot make
        // sense of a terminal event either.
        self.start_response(&Value::Null, out);
        self.close_open_item(out);

        let status = self.stop_status.unwrap_or("completed");
        let mut response = json!({
            "id": self.response_id.clone(),
            "object": "response",
            "status": status,
            "model": self.resolved_model.clone(),
            "output": self.completed_output.clone(),
            "usage": {
                "input_tokens": self.usage.input_tokens,
                "output_tokens": self.usage.output_tokens,
                "total_tokens": self.usage.input_tokens + self.usage.output_tokens,
                "input_tokens_details": { "cached_tokens": self.usage.cache_read },
            },
        });
        let event_name = if status == "incomplete" {
            response["incomplete_details"] = json!({ "reason": "max_output_tokens" });
            "response.incomplete"
        } else {
            "response.completed"
        };
        self.emit_seq(out, event_name, json!({ "response": response }));
        self.finished = true;
    }
}

impl StreamConverter for AnthropicToResponses {
    fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.push(chunk)
    }

    fn finish(&mut self) -> Vec<u8> {
        self.finish()
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

    /// One Anthropic SSE frame, named the same as its own `type` field — the
    /// same shape [`super::super::chat_to_anthropic::ChatToAnthropic`]
    /// already produces on its output side.
    fn anthropic_event(value: Value) -> String {
        let name = value
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("event")
            .to_string();
        format!("event: {name}\ndata: {value}\n\n")
    }

    #[test]
    fn a_text_stream_produces_the_full_responses_event_sequence() {
        let mut converter = AnthropicToResponses::new("fallback".to_string());
        let mut out = converter.push(
            anthropic_event(json!({
                "type": "message_start",
                "message": {
                    "id": "msg_1", "model": "claude-opus", "content": [],
                    "usage": {"input_tokens": 10, "output_tokens": 0},
                },
            }))
            .as_bytes(),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "Hel"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "lo"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({"type": "content_block_stop", "index": 0})).as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null},
                    "usage": {"input_tokens": 10, "output_tokens": 5},
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.push(anthropic_event(json!({"type": "message_stop"})).as_bytes()));

        assert_eq!(
            names(&out),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.completed",
            ]
        );

        let events = frames(&out);
        assert_eq!(events[0].1["response"]["id"], "resp_msg_1");
        assert_eq!(events[0].1["response"]["model"], "claude-opus");
        assert_eq!(events[4].1["delta"], "Hel");
        assert_eq!(events[5].1["delta"], "lo");
        assert_eq!(events[9].1["response"]["status"], "completed");
        assert_eq!(
            events[9].1["response"]["output"][0]["content"][0]["text"],
            "Hello"
        );
        assert_eq!(events[9].1["response"]["usage"]["input_tokens"], 10);
        assert_eq!(events[9].1["response"]["usage"]["output_tokens"], 5);
        // Every event is sequentially numbered, starting at 1.
        let sequence: Vec<u64> = events
            .iter()
            .map(|(_, v)| v["sequence_number"].as_u64().unwrap())
            .collect();
        assert_eq!(sequence, (1..=sequence.len() as u64).collect::<Vec<_>>());
    }

    #[test]
    fn a_tool_call_streamed_in_fragments_becomes_one_function_call_item() {
        let mut converter = AnthropicToResponses::new("m".to_string());
        let mut out = converter.push(
            anthropic_event(json!({
                "type": "message_start",
                "message": {"id": "msg_1", "model": "m", "usage": {"input_tokens": 1}},
            }))
            .as_bytes(),
        );
        out.extend(converter.push(
            anthropic_event(json!({
                "type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {}},
            }))
            .as_bytes(),
        ));
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "{\"path\":"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "input_json_delta", "partial_json": "\"a.rs\"}"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({"type": "content_block_stop", "index": 0})).as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.push(anthropic_event(json!({"type": "message_stop"})).as_bytes()));

        let events = frames(&out);
        assert_eq!(
            names(&out),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done",
                "response.completed",
            ]
        );
        assert_eq!(events[2].1["item"]["type"], "function_call");
        assert_eq!(events[2].1["item"]["call_id"], "toolu_1");
        assert_eq!(events[2].1["item"]["name"], "read_file");
        // Fragments are forwarded verbatim — not reparsed or re-serialized.
        assert_eq!(events[3].1["delta"], "{\"path\":");
        assert_eq!(events[4].1["delta"], "\"a.rs\"}");
        assert_eq!(events[5].1["arguments"], "{\"path\":\"a.rs\"}");
        let output = &events[7].1["response"]["output"];
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["call_id"], "toolu_1");
        assert_eq!(output[0]["arguments"], "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn a_stream_that_never_gets_message_stop_is_still_closed_by_finish() {
        let mut converter = AnthropicToResponses::new("m".to_string());
        let mut out = converter.push(
            anthropic_event(json!({
                "type": "message_start",
                "message": {"id": "msg_1", "model": "m"},
            }))
            .as_bytes(),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "half a sen"},
                }))
                .as_bytes(),
            ),
        );
        // No `content_block_stop`, no `message_delta`, no `message_stop` —
        // the connection just ends.
        out.extend(converter.finish());

        let emitted = names(&out);
        assert!(
            emitted.contains(&"response.output_item.done".to_string()),
            "{emitted:?}"
        );
        assert!(
            emitted.contains(&"response.completed".to_string()),
            "{emitted:?}"
        );
        let events = frames(&out);
        let (_, completed) = events.last().unwrap();
        assert_eq!(
            completed["response"]["output"][0]["content"][0]["text"],
            "half a sen"
        );
    }

    #[test]
    fn a_mid_stream_error_event_becomes_a_response_failed_event() {
        let mut converter = AnthropicToResponses::new("m".to_string());
        let mut out = converter.push(
            anthropic_event(json!({
                "type": "message_start",
                "message": {"id": "msg_1", "model": "m"},
            }))
            .as_bytes(),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "partial"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": "upstream model crashed"},
                }))
                .as_bytes(),
            ),
        );
        // The error is terminal: nothing more may be emitted after it.
        out.extend(converter.finish());

        let events = frames(&out);
        assert_eq!(
            names(&out),
            vec![
                "response.created",
                "response.in_progress",
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta",
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done",
                "response.failed",
            ]
        );
        let (_, failed) = events.last().unwrap();
        assert_eq!(failed["response"]["status"], "failed");
        assert_eq!(
            failed["response"]["error"]["message"],
            "upstream model crashed"
        );
    }

    #[test]
    fn a_stream_that_produced_nothing_still_gets_a_well_formed_sequence() {
        let mut converter = AnthropicToResponses::new("fallback-model".to_string());
        let out = converter.finish();

        assert_eq!(
            names(&out),
            vec![
                "response.created",
                "response.in_progress",
                "response.completed"
            ]
        );
        let events = frames(&out);
        assert_eq!(events[0].1["response"]["model"], "fallback-model");
        assert_eq!(events[2].1["response"]["status"], "completed");
    }

    #[test]
    fn finish_is_idempotent() {
        let mut converter = AnthropicToResponses::new("m".to_string());
        converter.push(anthropic_event(json!({"type": "message_start", "message": {}})).as_bytes());
        assert!(!converter.finish().is_empty());
        assert!(converter.finish().is_empty());
    }

    #[test]
    fn max_tokens_stop_reason_produces_an_incomplete_response() {
        let mut converter = AnthropicToResponses::new("m".to_string());
        let mut out = converter.push(
            anthropic_event(json!({"type": "message_start", "message": {"id": "msg_1"}}))
                .as_bytes(),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "max_tokens"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.push(anthropic_event(json!({"type": "message_stop"})).as_bytes()));

        let events = frames(&out);
        let (name, incomplete) = events.last().unwrap();
        assert_eq!(name, "response.incomplete");
        assert_eq!(incomplete["response"]["status"], "incomplete");
        assert_eq!(
            incomplete["response"]["incomplete_details"]["reason"],
            "max_output_tokens"
        );
    }

    #[test]
    fn a_thinking_block_is_dropped_without_panicking() {
        let mut converter = AnthropicToResponses::new("m".to_string());
        let mut out = converter.push(
            anthropic_event(json!({"type": "message_start", "message": {"id": "msg_1"}}))
                .as_bytes(),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_start", "index": 0,
                    "content_block": {"type": "thinking", "thinking": ""},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({
                    "type": "content_block_delta", "index": 0,
                    "delta": {"type": "thinking_delta", "thinking": "pondering"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                anthropic_event(json!({"type": "content_block_stop", "index": 0})).as_bytes(),
            ),
        );
        out.extend(converter.push(anthropic_event(json!({"type": "message_stop"})).as_bytes()));

        // No output item was ever opened for the dropped block.
        assert!(!names(&out).contains(&"response.output_item.added".to_string()));
        assert!(names(&out).contains(&"response.completed".to_string()));
    }
}
