//! `openai-chat` SSE → `openai-responses` SSE.
//!
//! Faces the same "flat deltas in, structured items out" problem as
//! [`super::chat_to_anthropic::ChatToAnthropic`], just against a different
//! target shape — a Responses stream opens and closes typed `output` items
//! (`message`, `function_call`) instead of Anthropic content blocks, and
//! reports completion as a whole `response` object rather than a
//! `stop_reason` delta. The three invariants documented on
//! [`super::chat_to_anthropic`] hold for it too.

use serde_json::{json, Value};

use crate::usage::parse::find_event_boundary;

use super::{
    call_argument_fragment, delta_text, emit, error_message, event_data, ChatUsage,
    StreamConverter, MAX_EVENT_BYTES,
};

/// Which `output` item is currently open, and what it is. The Responses
/// equivalent of `OpenBlock` in [`super::chat_to_anthropic`]: exactly one is
/// open at a time, and it is always closed (a `…done` + `output_item.done`
/// pair) before the next opens or the stream ends.
enum OpenItem {
    Message {
        output_index: u64,
        item_id: String,
        /// Accumulated so far, so the closing `output_text.done` and the
        /// item's own `content` in `output_item.done` can restate the whole
        /// text — `openai-chat` never sends it back to us in one piece.
        text: String,
    },
    FunctionCall {
        output_index: u64,
        item_id: String,
        call_id: String,
        name: String,
        /// Accumulated so far, for the same reason as `Message::text`.
        arguments: String,
        /// Which upstream `tool_calls[]` entry this item belongs to. See
        /// [`ChatToResponses::tool_key`] — the same matching problem, solved
        /// the same way as [`super::chat_to_anthropic::ChatToAnthropic::tool_key`].
        key: i64,
    },
}

/// Incremental converter from an OpenAI Chat SSE stream to an OpenAI
/// Responses SSE stream. See the module docs for how it relates to
/// [`super::chat_to_anthropic::ChatToAnthropic`].
pub struct ChatToResponses {
    /// Reported as the response's `model` when upstream chunks do not name
    /// one.
    model: String,
    /// The `model` actually reported once `response.created` has gone out —
    /// carried into `response.completed`/`response.incomplete`/
    /// `response.failed` so all four agree.
    resolved_model: String,
    /// Bytes of an event that has not been terminated yet.
    buffer: Vec<u8>,
    /// Whether `response.created`/`response.in_progress` have gone out.
    /// Nothing else may precede them.
    started: bool,
    /// Whether the terminal event has gone out. Guards every path against
    /// emitting it twice.
    finished: bool,
    /// Set once `response.created` has gone out; carried into every later
    /// event that names the response.
    response_id: String,
    /// The next `sequence_number` to hand out. Every event in a Responses
    /// stream is numbered, starting at 1.
    sequence: u64,
    /// Next output-item index to hand out. Responses output indices are
    /// per-response and strictly increasing.
    next_output_index: u64,
    open: Option<OpenItem>,
    /// Every item that has been closed so far, in the shape it must appear
    /// in `response.completed`'s `output` array.
    completed_output: Vec<Value>,
    /// Counter for tool calls that arrive without an upstream `index`. Counts
    /// *down* from 0, mirroring
    /// [`super::chat_to_anthropic::ChatToAnthropic`]'s `synthetic_key`.
    synthetic_key: i64,
    /// From `finish_reason`, mapped once when it arrives: `"incomplete"` for
    /// `length`, `"completed"` for anything else.
    stop_status: Option<&'static str>,
    usage: ChatUsage,
}

impl ChatToResponses {
    /// `model` is what to report when upstream chunks do not name one.
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
            synthetic_key: 0,
            stop_status: None,
            usage: ChatUsage::default(),
        }
    }

    /// Feed raw upstream SSE bytes; returns the Responses SSE bytes to emit
    /// downstream, which is empty when the chunk only carried a partial
    /// event.
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
    /// own. Idempotent, and flushes a leftover unterminated event first, same
    /// as [`super::chat_to_anthropic::ChatToAnthropic::finish`].
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
        let data = event_data(text);
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.emit_terminal(out);
            return;
        }
        let Ok(chunk) = serde_json::from_str::<Value>(&data) else {
            return;
        };

        self.start_response(&chunk, out);

        if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.usage.record(usage);
        }

        // A mid-generation failure reported inside an otherwise-successful
        // stream. `response.failed` is terminal — nothing follows it.
        if let Some(error) = chunk.get("error").filter(|e| !e.is_null()) {
            self.close_open_item(out);
            let response = json!({
                "id": self.response_id.clone(),
                "object": "response",
                "status": "failed",
                "model": self.resolved_model.clone(),
                "output": self.completed_output.clone(),
                "error": { "message": error_message(error), "type": "api_error", "code": Value::Null },
            });
            self.emit_seq(out, "response.failed", json!({ "response": response }));
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
            self.stop_status = Some(if finish_reason == "length" {
                "incomplete"
            } else {
                "completed"
            });
        }
    }

    /// Emit `response.created` + `response.in_progress`, once, before
    /// anything else can go out.
    ///
    /// `chunk` may be `Value::Null` (called from [`Self::emit_terminal`] for a
    /// stream that produced nothing parseable) — every accessor here
    /// tolerates that.
    fn start_response(&mut self, chunk: &Value, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;

        let id = match chunk.get("id").and_then(|v| v.as_str()) {
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

        let model = chunk
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|m| !m.is_empty())
            .map(String::from)
            .unwrap_or_else(|| self.model.clone());
        self.resolved_model = model.clone();

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

    fn handle_text(&mut self, delta: &Value, out: &mut Vec<u8>) {
        let text = delta_text(delta);
        if text.is_empty() {
            return;
        }

        let (output_index, item_id) = self.ensure_message_item(out);
        if let Some(OpenItem::Message { text: buf, .. }) = &mut self.open {
            buf.push_str(&text);
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

    /// The open message item's `(output_index, item_id)`, opening one (and
    /// closing whatever else was open) if needed.
    fn ensure_message_item(&mut self, out: &mut Vec<u8>) -> (u64, String) {
        if let Some(OpenItem::Message {
            output_index,
            item_id,
            ..
        }) = &self.open
        {
            return (*output_index, item_id.clone());
        }
        self.close_open_item(out);

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
            item_id: item_id.clone(),
            text: String::new(),
        });
        (output_index, item_id)
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

            let target = match self.tool_key(call, name.is_some()) {
                Some(key) if self.open_tool_key() == Some(key) => match &self.open {
                    Some(OpenItem::FunctionCall {
                        output_index,
                        item_id,
                        ..
                    }) => Some((*output_index, item_id.clone())),
                    _ => None,
                },
                Some(key) => Some(self.open_function_call(key, call, name, out)),
                // No `index` and no `name`: an argument fragment, which
                // belongs to the most recently opened function call.
                None => match &self.open {
                    Some(OpenItem::FunctionCall {
                        output_index,
                        item_id,
                        ..
                    }) => Some((*output_index, item_id.clone())),
                    _ => None,
                },
            };
            let Some((output_index, item_id)) = target else {
                continue;
            };

            if let Some(fragment) = call_argument_fragment(function) {
                if let Some(OpenItem::FunctionCall { arguments, .. }) = &mut self.open {
                    arguments.push_str(&fragment);
                }
                self.emit_seq(
                    out,
                    "response.function_call_arguments.delta",
                    json!({ "item_id": item_id, "output_index": output_index, "delta": fragment }),
                );
            }
        }
    }

    /// Which tool call an upstream entry refers to. Same rule as
    /// [`super::chat_to_anthropic::ChatToAnthropic::tool_key`].
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
            Some(OpenItem::FunctionCall { key, .. }) => Some(*key),
            _ => None,
        }
    }

    fn open_function_call(
        &mut self,
        key: i64,
        call: &Value,
        name: Option<&str>,
        out: &mut Vec<u8>,
    ) -> (u64, String) {
        self.close_open_item(out);

        let output_index = self.next_output_index;
        self.next_output_index += 1;
        let item_id = format!("fc_{}", uuid::Uuid::now_v7());
        // Codex matches a later `function_call_output` back to this call by
        // `call_id`, so it must be the upstream id verbatim when there is
        // one — only synthesized as a last resort.
        let call_id = call
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .unwrap_or_else(|| format!("call_{}", uuid::Uuid::now_v7()));
        let name = name.unwrap_or("").to_string();

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
            item_id: item_id.clone(),
            call_id,
            name,
            arguments: String::new(),
            key,
        });
        (output_index, item_id)
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
                "input_tokens": self.usage.prompt,
                "output_tokens": self.usage.completion,
                "total_tokens": self.usage.prompt + self.usage.completion,
                "input_tokens_details": { "cached_tokens": self.usage.cached },
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

impl StreamConverter for ChatToResponses {
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
    fn a_text_stream_produces_the_full_responses_event_sequence() {
        let mut converter = ChatToResponses::new("fallback".to_string());
        let mut out = converter.push(text_chunk("Hel", None).as_bytes());
        out.extend(converter.push(text_chunk("lo", Some("stop")).as_bytes()));
        out.extend(converter.push(b"data: [DONE]\n\n"));
        out.extend(converter.finish());

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
        assert_eq!(events[0].1["response"]["id"], "resp_chatcmpl-1");
        assert_eq!(events[0].1["response"]["model"], "qwen3.5");
        assert_eq!(events[4].1["delta"], "Hel");
        assert_eq!(events[5].1["delta"], "lo");
        assert_eq!(events[9].1["response"]["status"], "completed");
        assert_eq!(
            events[9].1["response"]["output"][0]["content"][0]["text"],
            "Hello"
        );
        // Every event is sequentially numbered, starting at 1.
        let sequence: Vec<u64> = events
            .iter()
            .map(|(_, v)| v["sequence_number"].as_u64().unwrap())
            .collect();
        assert_eq!(sequence, (1..=sequence.len() as u64).collect::<Vec<_>>());
    }

    #[test]
    fn the_done_sentinel_is_consumed_and_never_forwarded_for_responses() {
        let mut converter = ChatToResponses::new("m".to_string());
        let mut out = converter.push(text_chunk("hi", Some("stop")).as_bytes());
        out.extend(converter.push(b"data: [DONE]\n\n"));

        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("[DONE]"), "{text}");
        assert!(text.contains("event: response.completed"), "{text}");
    }

    #[test]
    fn crlf_framed_events_are_converted_same_as_lf_for_responses() {
        // Same fix as `crlf_framed_events_are_converted_same_as_lf`, applied
        // to the Responses converter — the drain loop is byte-for-byte the
        // same code, so it had the same bug.
        let crlf = text_chunk("hello", Some("stop")).replace('\n', "\r\n");
        let mut converter = ChatToResponses::new("m".to_string());
        let mut out = converter.push(crlf.as_bytes());
        out.extend(converter.finish());

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"delta\":\"hello\""), "{text}");
        assert!(text.contains("event: response.completed"), "{text}");
    }

    #[test]
    fn finish_flushes_a_final_event_with_no_trailing_blank_line_for_responses() {
        let unterminated = text_chunk("hi", Some("stop"));
        let unterminated = unterminated.strip_suffix("\n\n").unwrap();
        let mut converter = ChatToResponses::new("m".to_string());
        let mut out = converter.push(unterminated.as_bytes());
        assert!(
            !String::from_utf8_lossy(&out).contains("output_text.delta"),
            "an event with no separator yet must not be handled early"
        );
        out.extend(converter.finish());

        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"delta\":\"hi\""), "{text}");
        assert!(text.contains("event: response.completed"), "{text}");
    }

    #[test]
    fn a_tool_call_streamed_in_fragments_becomes_one_function_call_item() {
        let mut converter = ChatToResponses::new("m".to_string());
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
        // `call_id` must be the upstream id verbatim: Codex matches a later
        // `function_call_output` back to this call by that id.
        assert_eq!(events[2].1["item"]["call_id"], "call_1");
        assert_eq!(events[2].1["item"]["name"], "read_file");
        assert_eq!(events[3].1["delta"], "{\"path\":");
        assert_eq!(events[4].1["delta"], "\"a.rs\"}");
        assert_eq!(events[5].1["arguments"], "{\"path\":\"a.rs\"}");
        let output = &events[7].1["response"]["output"];
        assert_eq!(output[0]["type"], "function_call");
        assert_eq!(output[0]["call_id"], "call_1");
        assert_eq!(output[0]["arguments"], "{\"path\":\"a.rs\"}");
    }

    #[test]
    fn a_tool_call_without_an_id_gets_a_synthesized_call_id() {
        let mut converter = ChatToResponses::new("m".to_string());
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
        let added = events
            .iter()
            .find(|(name, _)| name == "response.output_item.added")
            .unwrap();
        assert_eq!(added.1["item"]["name"], "bash");
        assert!(added.1["item"]["call_id"]
            .as_str()
            .unwrap()
            .starts_with("call_"));
    }

    #[test]
    fn text_after_a_tool_call_opens_a_new_message_item() {
        let mut converter = ChatToResponses::new("m".to_string());
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
        let output_indices: Vec<u64> = events
            .iter()
            .filter(|(name, _)| name == "response.output_item.added")
            .map(|(_, value)| value["output_index"].as_u64().unwrap())
            .collect();
        assert_eq!(output_indices, vec![0, 1, 2]);
        // Every item that opened also closed.
        assert_eq!(
            events
                .iter()
                .filter(|(name, _)| name == "response.output_item.done")
                .count(),
            3
        );
    }

    #[test]
    fn finish_reason_length_produces_an_incomplete_response() {
        let mut converter = ChatToResponses::new("m".to_string());
        let mut out = converter.push(text_chunk("cut off", Some("length")).as_bytes());
        out.extend(converter.finish());

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
    fn the_usage_only_final_chunk_feeds_response_completed() {
        let mut converter = ChatToResponses::new("m".to_string());
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
        let (_, completed) = events
            .iter()
            .find(|(name, _)| name == "response.completed")
            .unwrap();
        assert_eq!(completed["response"]["usage"]["input_tokens"], 41);
        assert_eq!(completed["response"]["usage"]["output_tokens"], 7);
        assert_eq!(completed["response"]["usage"]["total_tokens"], 48);
        assert_eq!(
            completed["response"]["usage"]["input_tokens_details"]["cached_tokens"],
            12
        );
    }

    #[test]
    fn a_truncated_responses_stream_is_still_closed_by_finish() {
        let mut converter = ChatToResponses::new("m".to_string());
        let mut out = converter.push(text_chunk("half a sen", None).as_bytes());
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
    }

    #[test]
    fn responses_finish_is_idempotent() {
        let mut converter = ChatToResponses::new("m".to_string());
        converter.push(text_chunk("hi", Some("stop")).as_bytes());
        assert!(!converter.finish().is_empty());
        assert!(converter.finish().is_empty());
    }

    #[test]
    fn a_responses_stream_that_produced_nothing_still_gets_a_well_formed_sequence() {
        let mut converter = ChatToResponses::new("fallback-model".to_string());
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
    fn a_mid_stream_error_frame_becomes_a_response_failed_event() {
        let mut converter = ChatToResponses::new("m".to_string());
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
}
