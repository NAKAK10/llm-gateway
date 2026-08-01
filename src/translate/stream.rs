//! `openai-chat` SSE → `anthropic-messages` SSE.
//!
//! The hard half of translation. The two protocols do not disagree about
//! field names here so much as about *shape*: `openai-chat` streams a flat
//! sequence of deltas and lets the client work out what belongs to what, while
//! `anthropic-messages` streams explicitly opened and closed content blocks. So
//! this is a state machine, and its job is to invent the structure the upstream
//! never sent.
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
//!
//! What is lost: `reasoning_content` / `reasoning` deltas that DeepSeek and
//! some Ollama models emit are dropped rather than turned into `thinking`
//! blocks — a real `thinking` block carries a `signature` only Anthropic can
//! produce, and forwarding the reasoning as ordinary text would present it as
//! the answer. Prompt-cache accounting and citations have no `openai-chat`
//! source at all.
//!
//! `usage` in the emitted events is for the client's benefit only; the
//! gateway's own accounting reads the upstream bytes in `usage::parse` and
//! never sees anything produced here.
//!
//! [`ChatToResponses`] is the second converter in this module, for
//! `openai-responses` clients (Codex CLI). It faces the same "flat deltas
//! in, structured items out" problem as [`ChatToAnthropic`], just against a
//! different target shape — a Responses stream opens and closes typed
//! `output` items (`message`, `function_call`) instead of Anthropic content
//! blocks, and reports completion as a whole `response` object rather than a
//! `stop_reason` delta. The three invariants above hold for it too.

use serde_json::{json, Value};

use crate::usage::parse::find_subslice;

/// An SSE event is complete once a blank line separates it from the next.
const EVENT_SEPARATOR: &[u8] = b"\n\n";

/// Defensive cap on a single partial event, mirroring `usage::parse`: a
/// misbehaving upstream must not be able to grow our buffer without bound.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Common interface over a stateful "upstream chat SSE → client SSE"
/// converter, so [`crate::translate::adapter::Mode::Sse`] can hold whichever
/// one a given [`crate::translate::Translation`] needs behind a single type.
pub trait StreamConverter: Send {
    /// Feed raw upstream SSE bytes; returns the client-protocol SSE bytes to
    /// emit downstream, which is empty when the chunk only carried a partial
    /// event.
    fn push(&mut self, chunk: &[u8]) -> Vec<u8>;

    /// The closing events for a stream that ended without its own explicit
    /// terminator. Idempotent: a call after the stream is already closed
    /// returns empty.
    fn finish(&mut self) -> Vec<u8>;
}

/// The plain text of a `delta.content` field: usually a string; some proxies
/// mimic the Responses shape and send an array of `{"type":"text","text":…}`
/// parts even in a chat stream, which are concatenated.
fn delta_text(delta: &Value) -> String {
    match delta.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// One `tool_calls[]` entry's argument fragment, if it has one. Forwarded
/// verbatim when it is already a string (a fragment is not valid JSON on its
/// own, and the client reassembles the string before parsing it); some
/// servers send the whole argument object instead, which is serialised once
/// into a single fragment.
fn call_argument_fragment(function: Option<&Value>) -> Option<String> {
    match function.and_then(|f| f.get("arguments")) {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(value @ Value::Object(_)) => Some(value.to_string()),
        _ => None,
    }
}

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

/// The last usage numbers seen upstream, in `openai-chat` names. Shared by
/// both converters in this module — the source shape being translated is the
/// same regardless of which client protocol comes out the other end.
#[derive(Default)]
struct ChatUsage {
    prompt: u64,
    completion: u64,
    cached: u64,
}

impl ChatUsage {
    fn record(&mut self, usage: &Value) {
        let field = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        self.prompt = field("prompt_tokens");
        self.completion = field("completion_tokens");
        self.cached = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
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

        while let Some(pos) = find_subslice(&self.buffer, EVENT_SEPARATOR) {
            let event_end = pos + EVENT_SEPARATOR.len();
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
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
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
                    "usage": { "input_tokens": self.usage.prompt, "output_tokens": 0 },
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
                    "input_tokens": self.usage.prompt,
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

/// One client-protocol SSE frame. Both the `event:` name and the `type`
/// inside the payload are sent, because clients dispatch on the former and
/// validate the latter.
fn emit(out: &mut Vec<u8>, event: &str, data: Value) {
    out.extend_from_slice(format!("event: {event}\ndata: {data}\n\n").as_bytes());
}

/// The concatenated `data:` payload of one SSE event.
///
/// Multi-line `data` fields are joined per the SSE spec; every payload in this
/// protocol is single-line JSON, so plain concatenation is enough. `event:`
/// lines are ignored — `openai-chat` does not name its events, and where a
/// provider does, the name carries nothing the payload does not.
fn event_data(event: &str) -> String {
    let mut data = String::new();
    for line in event.split('\n') {
        let line = line.strip_prefix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data: ") {
            data.push_str(rest);
        } else if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest);
        }
    }
    data.trim().to_string()
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

/// A human-readable message out of whatever an upstream put in `error`: an
/// object with a `message`, a bare string (Ollama), or something unexpected —
/// in which case the raw JSON beats saying nothing.
fn error_message(error: &Value) -> String {
    if let Some(message) = error.get("message").and_then(|m| m.as_str()) {
        return message.to_string();
    }
    match error {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Which `output` item is currently open, and what it is. The Responses
/// equivalent of [`OpenBlock`]: exactly one is open at a time, and it is
/// always closed (a `…done` + `output_item.done` pair) before the next opens
/// or the stream ends.
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
        /// [`ChatToAnthropic::tool_key`] — the same matching problem, solved
        /// the same way.
        key: i64,
    },
}

/// Incremental converter from an OpenAI Chat SSE stream to an OpenAI
/// Responses SSE stream. See the module docs for how it relates to
/// [`ChatToAnthropic`].
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
    /// *down* from 0, mirroring [`ChatToAnthropic::synthetic_key`].
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

        while let Some(pos) = find_subslice(&self.buffer, EVENT_SEPARATOR) {
            let event_end = pos + EVENT_SEPARATOR.len();
            let event: Vec<u8> = self.buffer.drain(..event_end).collect();
            self.handle_event(&event[..pos], &mut out);
        }

        if self.buffer.len() > MAX_EVENT_BYTES {
            self.buffer.clear();
        }

        out
    }

    /// The closing events, for a stream that ended without a `[DONE]` of its
    /// own. Idempotent, same as [`ChatToAnthropic::finish`].
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
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
    /// [`ChatToAnthropic::tool_key`].
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
        assert_eq!(delta["usage"]["input_tokens"], 41);
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
