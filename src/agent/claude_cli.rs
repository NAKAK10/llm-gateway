//! Driving `claude -p` as an upstream: argv, prompt, and output translation.
//!
//! Everything here is **pure**. Spawning lives in [`super::spawn`]; this module
//! decides *what* to run and *how to read what came back*, so both can be tested
//! without a subscription or a child process.
//!
//! # Why the CLI at all
//!
//! A Claude Pro/Max plan is a credential for Claude Code, not an API key. The
//! gateway cannot present it upstream — but it can ask the official client to do
//! the work, with its own login. That is the whole trick, and it is the same one
//! OpenClaw uses (`agentRuntime.id: "claude-cli"`).
//!
//! # What the CLI gives us
//!
//! With `--output-format stream-json --verbose --include-partial-messages`,
//! Claude Code emits the provider's **own Anthropic stream events**, wrapped one
//! per line:
//!
//! ```text
//! {"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{…}}}
//! ```
//!
//! So streaming translation is unwrapping, not rebuilding: the `event` object is
//! re-emitted verbatim as an SSE frame. Token deltas, `usage` (including cache
//! counts) and `stop_reason` all arrive as the model produced them.
//!
//! Without that flag the CLI emits one `assistant` event carrying a complete
//! Anthropic `message`, which is what the non-streaming path reads.
//!
//! # What is lost, and why
//!
//! - **The caller's tools.** `claude -p` runs Claude Code's own tools, not the
//!   ones in the request, and there is no way to hand it a foreign tool schema.
//!   A request with `tools` gets a text answer instead of `tool_use` blocks, so
//!   this upstream suits generation, not an agent loop.
//! - **Multi-turn structure.** The CLI takes one prompt, so a `messages` array
//!   is flattened into a labelled transcript (see [`prompt`]).
//! - **Sampling controls.** `temperature`, `top_p` and `stop_sequences` have
//!   no CLI equivalent and are dropped. `max_tokens` has no equivalent
//!   either, but is approximated by appending a brevity instruction to the
//!   system prompt (see [`system_prompt`]) — a caller expecting a short
//!   verdict (Claude Code's own `<transcript>`-prefixed utility calls, for
//!   instance) otherwise gets several hundred tokens of unrequested
//!   reasoning, which is slow enough through a freshly spawned CLI process to
//!   blow past the caller's own timeout (issue #17). This is guidance, not
//!   an enforced cap: the model can still ignore it.
//!
//! # Isolation, and why each flag is there
//!
//! The child is not a general-purpose Claude Code session; it is a text
//! generator. Four decisions keep it that way, and one of them prevents an
//! infinite loop:
//!
//! - `--allowedTools ""` — every tool call is denied. Verified behaviour: the
//!   model may still *attempt* one, and the attempt is refused without the
//!   process hanging on a permission prompt.
//! - `--setting-sources project`, run in an empty scratch directory — no user
//!   settings, so no hooks fire and, critically, **`~/.claude/settings.json`'s
//!   `env` block cannot apply**. That block can contain `ANTHROPIC_BASE_URL`
//!   pointing back at this gateway, which would make the child call us,
//!   forever.
//! - [`STRIPPED_ENV`] removes the same variables from the child's environment,
//!   for the same reason: the shell that started `serve` may well have exported
//!   them.
//! - `--strict-mcp-config` — no MCP servers, so no tool surface arrives that way
//!   either.

use serde_json::Value;

/// Environment variables removed from the child.
///
/// `ANTHROPIC_BASE_URL` is the load-bearing one: inherited from a `serve`
/// started in a redirected shell, it would point the child straight back at this
/// gateway. The rest are removed so the child authenticates with its own
/// subscription login and nothing else — an inherited `ANTHROPIC_API_KEY` would
/// silently bill an API account for what the user asked to run on their plan.
pub const STRIPPED_ENV: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_SMALL_FAST_MODEL",
    "ANTHROPIC_CUSTOM_HEADERS",
];

/// The program name looked up on `PATH`.
pub const PROGRAM: &str = "claude";

/// Build the child's arguments.
///
/// `model` is passed through as given: the CLI accepts both aliases (`sonnet`,
/// `opus`) and full ids, so `claude-cli/sonnet` and
/// `claude-cli/claude-sonnet-4-6` both work without a mapping table here.
pub fn args(model: &str, system: Option<&str>, streaming: bool, extra: &[String]) -> Vec<String> {
    let mut args = vec!["-p".to_string()];
    if !super::codex_cli::is_cli_default(model) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    args.extend([
        // `stream-json` is the only format that reports usage and stop_reason;
        // `--verbose` is required alongside it for `-p`.
        "--output-format".to_string(),
        "stream-json".to_string(),
        "--verbose".to_string(),
        // See the module docs: these four turn a Claude Code session into a
        // text generator that cannot loop back into this gateway.
        "--allowedTools".to_string(),
        String::new(),
        "--setting-sources".to_string(),
        "project".to_string(),
        "--strict-mcp-config".to_string(),
    ]);

    if let Some(system) = system.map(str::trim).filter(|s| !s.is_empty()) {
        // Replaces Claude Code's own agent preamble rather than appending to it
        // (`--append-system-prompt`): the caller's `system` is the whole
        // instruction here, and inheriting an agent prompt would answer as an
        // agent.
        args.push("--system-prompt".to_string());
        args.push(system.to_string());
    }

    if streaming {
        args.push("--include-partial-messages".to_string());
    }

    args.extend(extra.iter().cloned());
    args
}

/// The prompt written to the child's stdin.
///
/// stdin rather than argv because a real request easily exceeds a comfortable
/// argument length, and because it avoids one layer of quoting.
///
/// A single user message becomes its text verbatim — the common case for a
/// generation route, and worth keeping clean. Anything longer is rendered as a
/// labelled transcript, which is the best available approximation: the CLI
/// accepts one prompt, so the turn structure has to live inside it.
pub fn prompt(payload: &Value) -> String {
    let Some(messages) = payload.get("messages").and_then(|m| m.as_array()) else {
        return String::new();
    };

    let rendered: Vec<(String, String)> = messages
        .iter()
        .map(|message| {
            let role = message
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("user")
                .to_string();
            (role, crate::translate::request::message_text(message))
        })
        .filter(|(_, text)| !text.trim().is_empty())
        .collect();

    match rendered.as_slice() {
        [] => String::new(),
        [(role, text)] if role == "user" => text.clone(),
        turns => turns
            .iter()
            .map(|(role, text)| {
                let label = if role == "assistant" {
                    "Assistant"
                } else {
                    "Human"
                };
                format!("{label}: {text}")
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
    }
}

/// The `--system-prompt` value for a request, if it has one.
pub fn system(payload: &Value) -> Option<String> {
    let text = crate::translate::request::system_text(payload.get("system")?);
    (!text.trim().is_empty()).then_some(text)
}

/// A brevity instruction derived from the request's `max_tokens`, standing in
/// for the sampling control the CLI has no flag for. Best-effort: it curbs
/// the multi-hundred-token rambling measured on judgment-style utility calls
/// (issue #17), but the model can still ignore it.
fn token_budget_hint(payload: &Value) -> Option<String> {
    let max_tokens = payload.get("max_tokens")?.as_u64()?;
    Some(format!(
        "IMPORTANT: Respond in at most about {max_tokens} tokens. Be direct \
         and concise — do not pad the answer with unrequested explanation or \
         reasoning."
    ))
}

/// The full `--system-prompt` value: the caller's system text, if any, with
/// [`token_budget_hint`] appended when the request set `max_tokens`.
pub fn system_prompt(payload: &Value) -> Option<String> {
    match (system(payload), token_budget_hint(payload)) {
        (Some(base), Some(hint)) => Some(format!("{base}\n\n{hint}")),
        (Some(base), None) => Some(base),
        (None, Some(hint)) => Some(hint),
        (None, None) => None,
    }
}

/// Incremental converter from the CLI's JSONL to an Anthropic SSE stream.
///
/// Mostly an unwrapper: with `--include-partial-messages` the CLI's
/// `stream_event` lines *contain* the Anthropic events, so each one is re-emitted
/// as an SSE frame unchanged. The work is in the edges — lines that are not
/// stream events, a run that fails, and a child that stops without closing the
/// message.
pub struct CliToAnthropic {
    /// Reported when the CLI never names a model itself.
    model: String,
    /// Bytes of a line that has not been terminated yet.
    buffer: Vec<u8>,
    /// Whether a `message_start` has gone downstream.
    started: bool,
    /// Whether the terminal events have gone downstream.
    finished: bool,
    /// Whether the first `message_stop` stream event has been seen — the
    /// turn is logically over, even though its terminal events may still be
    /// held back waiting for `result`. `--allowedTools ""` lets the model
    /// attempt (and get refused) a tool call, and a run can then carry a
    /// second `message_start`…`message_stop` pair whose `content_block`
    /// indices are not known to line up with the first's. Rather than
    /// forward a second `message_start` with no `message_stop` in between
    /// (an Anthropic SSE contract violation) or risk colliding indices, every
    /// `stream_event` after this flips is dropped — the client sees exactly
    /// what it would have before PR16 introduced this file's usage
    /// correction. The top-level `result` line is unaffected: it is not a
    /// `stream_event`, and applying its usage is the whole point of that
    /// correction.
    turn_ended: bool,
    /// The `message_delta` stream event, held back rather than forwarded
    /// immediately: its `usage` is a mid-run snapshot, and the authoritative
    /// total only arrives with the top-level `result` line that follows.
    pending_delta: Option<Value>,
    /// The `message_stop` stream event, held back alongside `pending_delta`
    /// so the two are always emitted together, in order.
    pending_stop: Option<Value>,
    /// Whether a top-level `result` line has been processed. Emission waits
    /// for this even if `pending_stop` is already set, so a `result` that
    /// arrives a moment after `message_stop` (the normal order) still gets
    /// to correct the usage before anything goes out.
    result_seen: bool,
    /// `result`'s `usage`, kept aside independently of `pending_delta`: a run
    /// that never streams a `message_delta` (no `stream_event` line at all —
    /// possible, and the case issue #11 raised) still has to report real
    /// token counts. `close()` reads this to build the synthesized
    /// `message_delta`'s `usage` rather than hardcoding zero.
    result_usage: Option<Value>,
}

/// Defensive cap on a single JSONL line, mirroring the SSE scanners.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

impl CliToAnthropic {
    pub fn new(model: String) -> Self {
        Self {
            model,
            buffer: Vec::new(),
            started: false,
            finished: false,
            turn_ended: false,
            pending_delta: None,
            pending_stop: None,
            result_seen: false,
            result_usage: None,
        }
    }

    /// Feed raw stdout bytes; returns the SSE bytes to emit downstream.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        self.buffer.extend_from_slice(chunk);

        while let Some(pos) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=pos).collect();
            self.handle_line(&line[..pos], &mut out);
        }

        if self.buffer.len() > MAX_LINE_BYTES {
            self.buffer.clear();
        }

        out
    }

    /// Terminal events, for a child that ended without emitting them itself —
    /// killed, crashed, or simply not asked to stream. Idempotent.
    ///
    /// If `message_delta`/`message_stop` were already seen but held back
    /// waiting for `result`'s authoritative usage (which never arrived — the
    /// child was killed, or produced no `result` line), they are flushed now
    /// with whatever usage they already carried rather than left unsent.
    /// `emit_pending` synthesizes whichever of the two never arrived, so the
    /// client always gets a `message_stop` and does not hang waiting for one.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if self.pending_delta.is_some() || self.pending_stop.is_some() {
            self.emit_pending(&mut out);
        } else {
            self.close(&mut out, "end_turn");
        }
        out
    }

    fn handle_line(&mut self, line: &[u8], out: &mut Vec<u8>) {
        if self.finished {
            return;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            return;
        };
        if text.trim().is_empty() {
            return;
        }
        let Ok(event) = serde_json::from_str::<Value>(text) else {
            return;
        };

        match event.get("type").and_then(|t| t.as_str()) {
            // The interesting case: the payload already *is* an Anthropic
            // stream event, so it goes out (almost) untouched — except
            // `message_delta`/`message_stop`, which are held back until
            // `result` has had a chance to correct the usage. See
            // `pending_delta`/`pending_stop`.
            Some("stream_event") => {
                // Once the first turn's `message_stop` has been seen, every
                // further `stream_event` belongs to a second assistant
                // message the CLI can emit when the model attempts a denied
                // tool call (see `turn_ended`'s doc comment). None of it goes
                // out: not a fresh `message_start`, not its content blocks,
                // nothing.
                if self.turn_ended {
                    return;
                }
                let Some(inner) = event.get("event") else {
                    return;
                };
                let Some(name) = inner.get("type").and_then(|t| t.as_str()) else {
                    return;
                };
                match name {
                    "message_start" => {
                        self.started = true;
                        emit(out, name, inner);
                    }
                    "message_delta" => {
                        self.pending_delta = Some(inner.clone());
                    }
                    "message_stop" => {
                        self.pending_stop = Some(inner.clone());
                        self.turn_ended = true;
                        self.try_flush(out);
                    }
                    _ => emit(out, name, inner),
                }
            }
            // A failed run. `result` carries the reason, and after it the CLI
            // says nothing more, so this is terminal.
            Some("result") if event.get("is_error").and_then(|e| e.as_bool()) == Some(true) => {
                let message = event
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("claude CLI reported an error")
                    .to_string();
                // A run can fail after already billing real tokens (a usage
                // limit hit mid-generation, for instance) — this `usage`, if
                // present, is the only place those counts are ever reported.
                // See `error`'s doc comment for where it goes.
                let usage = event.get("usage").filter(|u| u.is_object()).cloned();
                self.error(out, &message, usage.as_ref());
            }
            // A successful run. The assistant event's usage seen so far (now
            // sitting in `pending_delta`) is a mid-run snapshot — it reported
            // `output_tokens: 1` for an answer that cost 5. `result`'s usage
            // is the total, and it is what accounting reads downstream (it
            // scans this converter's own SSE output), so it replaces rather
            // than merges into the held-back `message_delta`.
            Some("result") => {
                if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
                    // Kept aside regardless of `pending_delta`: a run that
                    // never streamed a `message_delta` at all (no
                    // `stream_event` line reached us — issue #11) still
                    // needs this for the `message_delta` `close()` will have
                    // to synthesize.
                    self.result_usage = Some(usage.clone());
                    if let Some(delta) = self.pending_delta.as_mut() {
                        delta["usage"] = usage.clone();
                    }
                }
                self.result_seen = true;
                self.try_flush(out);
            }
            // `system`, `rate_limit_event`, `assistant`: redundant while
            // partial messages are streaming.
            _ => {}
        }
    }

    /// Emit the held-back `message_delta`/`message_stop` once both are ready:
    /// `message_stop` must have arrived (there is nothing to flush yet
    /// otherwise) and `result` must have been seen, so a `result` a moment
    /// behind `message_stop` — the normal order — still gets to correct the
    /// usage before either goes out.
    fn try_flush(&mut self, out: &mut Vec<u8>) {
        if self.finished || self.pending_stop.is_none() || !self.result_seen {
            return;
        }
        self.emit_pending(out);
    }

    /// Emit whatever of `pending_delta`/`pending_stop` is set, in order, and
    /// mark the stream finished.
    ///
    /// `message_stop` is always emitted, even if the CLI never sent one to
    /// hold back: this is reached from `finish()` when the child died after
    /// `message_delta` but before `message_stop`, and a client that got a
    /// `message_delta` with nothing after it never sees its stream close.
    fn emit_pending(&mut self, out: &mut Vec<u8>) {
        if let Some(delta) = self.pending_delta.take() {
            emit(out, "message_delta", &delta);
        }
        let stop = self
            .pending_stop
            .take()
            .unwrap_or_else(|| serde_json::json!({"type": "message_stop"}));
        emit(out, "message_stop", &stop);
        self.finished = true;
    }

    /// Whether the terminal events have already gone out. A caller deciding
    /// whether to report a non-zero exit as an error should check this first:
    /// a child that finished its turn (`message_stop` seen, `result` applied)
    /// and then exited oddly afterwards still produced a real answer.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Emit an Anthropic `error` frame and stop.
    ///
    /// No `message_stop` follows: Anthropic's own streams treat `error` as
    /// terminal in its own right, and `ChatToAnthropic`'s error path in
    /// `translate/stream.rs` already made that same call for the same
    /// reason — a mid-stream `error` closes the stream by itself, so adding
    /// a `message_stop` after it would not be completing the contract, it
    /// would be inventing an event Anthropic's own API never sends there.
    ///
    /// `usage` still has to go out, though, and that part *is* this
    /// converter's own problem to solve, unlike the openai-chat translator:
    /// that one's accounting reads the *upstream* bytes directly
    /// (`usage::parse`), but claude-cli's usage is read from *this
    /// converter's own* SSE output by the `tee` in `src/server/proxy.rs`. A
    /// `usage` that arrived only on an errored `result` line therefore has
    /// nowhere else to land. It is folded into whatever `message_delta` is
    /// already pending (or a fresh one, if the run never streamed one at
    /// all) and flushed immediately before the `error` frame — so a client
    /// that already received content still gets a coherent final delta
    /// instead of being cut off mid-message, and the token count is not
    /// silently dropped on the floor.
    pub fn error(&mut self, out: &mut Vec<u8>, message: &str, usage: Option<&Value>) {
        if self.finished {
            return;
        }
        if usage.is_some() || self.pending_delta.is_some() {
            let mut delta = self.pending_delta.take().unwrap_or_else(|| {
                serde_json::json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": Value::Null, "stop_sequence": Value::Null },
                })
            });
            if let Some(usage) = usage {
                delta["usage"] = usage.clone();
            }
            emit(out, "message_delta", &delta);
        }
        emit(
            out,
            "error",
            &serde_json::json!({
                "type": "error",
                "error": { "type": "api_error", "message": message },
            }),
        );
        self.finished = true;
    }

    /// Close a message the child left open — otherwise a client waits for a
    /// `message_stop` that will never come.
    fn close(&mut self, out: &mut Vec<u8>, stop_reason: &str) {
        if self.finished {
            return;
        }
        if !self.started {
            self.started = true;
            emit(
                out,
                "message_start",
                &serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": format!("msg_{}", uuid::Uuid::now_v7()),
                        "type": "message",
                        "role": "assistant",
                        "model": self.model,
                        "content": [],
                        "stop_reason": Value::Null,
                        "stop_sequence": Value::Null,
                        "usage": { "input_tokens": 0, "output_tokens": 0 },
                    },
                }),
            );
        }
        // `result_usage` covers the run that never streamed a
        // `message_delta` at all (issue #11): without it this would always
        // report zero tokens. `message_start`'s usage above is left at zero
        // regardless — downstream accounting merges by field, later non-zero
        // values win (`Usage::merge`), so the real counts landing here are
        // enough.
        let usage = self
            .result_usage
            .clone()
            .unwrap_or_else(|| serde_json::json!({ "output_tokens": 0 }));
        emit(
            out,
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                "usage": usage,
            }),
        );
        emit(
            out,
            "message_stop",
            &serde_json::json!({"type": "message_stop"}),
        );
        self.finished = true;
    }
}

/// Assemble a complete Anthropic message from a finished run's JSONL.
///
/// Used for a non-streaming request, where the client wants one JSON body. The
/// CLI's `assistant` event already carries a full Anthropic `message`; the only
/// thing missing from it is `stop_reason`, which arrives with `result`.
///
/// `Err` is the message for an error envelope: a failed run, or output with no
/// assistant turn in it at all.
pub fn message_from_jsonl(bytes: &[u8], model: &str) -> Result<Value, String> {
    let mut message: Option<Value> = None;
    let mut stop_reason: Option<String> = None;
    let mut final_usage: Option<Value> = None;
    let mut error: Option<String> = None;

    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(text) = std::str::from_utf8(line) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(text.trim()) else {
            continue;
        };
        match event.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                if let Some(inner) = event.get("message") {
                    message = Some(inner.clone());
                }
            }
            Some("result") => {
                if event.get("is_error").and_then(|e| e.as_bool()) == Some(true) {
                    error = Some(
                        event
                            .get("result")
                            .and_then(|r| r.as_str())
                            .unwrap_or("claude CLI reported an error")
                            .to_string(),
                    );
                }
                if let Some(reason) = event.get("stop_reason").and_then(|r| r.as_str()) {
                    stop_reason = Some(reason.to_string());
                }
                // The `assistant` event's usage is a mid-run snapshot — it
                // reported `output_tokens: 1` for an answer that cost 5. The
                // `result` event's is the total, and this body is what cost
                // accounting reads, so the total is the one to keep.
                if let Some(usage) = event.get("usage").filter(|u| u.is_object()) {
                    final_usage = Some(usage.clone());
                }
            }
            _ => {}
        }
    }

    if let Some(error) = error {
        return Err(error);
    }
    let Some(mut message) = message else {
        return Err("claude CLI produced no assistant message".to_string());
    };

    // Fill in what the assistant event leaves open. The CLI reports
    // `stop_reason: null` there because, mid-run, it does not know yet.
    if message
        .get("stop_reason")
        .map(|r| r.is_null())
        .unwrap_or(true)
    {
        message["stop_reason"] =
            Value::String(stop_reason.unwrap_or_else(|| "end_turn".to_string()));
    }
    if let Some(usage) = final_usage {
        message["usage"] = usage;
    }
    if message.get("model").and_then(|m| m.as_str()).is_none() {
        message["model"] = Value::String(model.to_string());
    }
    // Anthropic requires a non-empty content array.
    let empty = message
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| blocks.is_empty())
        .unwrap_or(true);
    if empty {
        message["content"] = serde_json::json!([{ "type": "text", "text": "" }]);
    }

    Ok(message)
}

/// One Anthropic SSE frame.
fn emit(out: &mut Vec<u8>, event: &str, data: &Value) {
    out.extend_from_slice(format!("event: {event}\ndata: {data}\n\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn names(bytes: &[u8]) -> Vec<String> {
        String::from_utf8(bytes.to_vec())
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("event: ").map(String::from))
            .collect()
    }

    fn stream_event(inner: Value) -> String {
        format!("{}\n", json!({"type": "stream_event", "event": inner}))
    }

    #[test]
    fn args_deny_tools_and_isolate_settings() {
        let args = args("sonnet", None, false, &[]);
        // `--allowedTools ""` is deny-all; the empty string must be its own
        // argument, not omitted.
        let index = args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(args[index + 1], "");
        assert!(args
            .windows(2)
            .any(|w| w == ["--setting-sources", "project"]));
        assert!(args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(args.windows(2).any(|w| w == ["--model", "sonnet"]));
        assert!(!args.iter().any(|a| a == "--include-partial-messages"));
    }

    #[test]
    fn streaming_asks_for_partial_messages() {
        let args = args("opus", None, true, &[]);
        assert!(args.iter().any(|a| a == "--include-partial-messages"));
    }

    #[test]
    fn a_system_prompt_replaces_rather_than_appends() {
        let args = args("sonnet", Some("You are terse."), false, &[]);
        assert!(args
            .windows(2)
            .any(|w| w == ["--system-prompt", "You are terse."]));
        assert!(!args.iter().any(|a| a == "--append-system-prompt"));
    }

    #[test]
    fn an_empty_system_prompt_is_not_passed_at_all() {
        let args = args("sonnet", Some("   "), false, &[]);
        assert!(!args.iter().any(|a| a == "--system-prompt"));
    }

    #[test]
    fn extra_args_come_last_so_they_can_override() {
        let args = args("sonnet", None, false, &["--add-dir".into(), "/tmp".into()]);
        assert_eq!(&args[args.len() - 2..], &["--add-dir", "/tmp"]);
    }

    #[test]
    fn a_single_user_message_becomes_the_prompt_verbatim() {
        let payload = json!({"messages": [{"role": "user", "content": "日本語で挨拶して"}]});
        assert_eq!(prompt(&payload), "日本語で挨拶して");
    }

    #[test]
    fn a_conversation_is_flattened_into_a_labelled_transcript() {
        let payload = json!({"messages": [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": [{"type": "text", "text": "reply"}]},
            {"role": "user", "content": "second"},
        ]});
        assert_eq!(
            prompt(&payload),
            "Human: first\n\nAssistant: reply\n\nHuman: second"
        );
    }

    #[test]
    fn empty_and_absent_messages_do_not_panic() {
        assert_eq!(prompt(&json!({})), "");
        assert_eq!(prompt(&json!({"messages": []})), "");
        assert_eq!(
            prompt(&json!({"messages": [{"role": "user", "content": ""}]})),
            ""
        );
    }

    #[test]
    fn system_blocks_are_flattened() {
        let payload = json!({"system": [
            {"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "b"},
        ]});
        assert_eq!(system(&payload).as_deref(), Some("a\n\nb"));
        assert!(system(&json!({})).is_none());
    }

    #[test]
    fn token_budget_hint_is_present_when_max_tokens_is_set() {
        let hint = token_budget_hint(&json!({"max_tokens": 64})).unwrap();
        assert!(hint.contains("64"), "{hint}");
    }

    #[test]
    fn token_budget_hint_is_absent_without_max_tokens() {
        assert!(token_budget_hint(&json!({})).is_none());
    }

    #[test]
    fn system_prompt_appends_the_budget_hint_to_the_callers_system() {
        let payload = json!({"system": "You are terse.", "max_tokens": 64});
        let combined = system_prompt(&payload).unwrap();
        assert!(combined.starts_with("You are terse."), "{combined}");
        assert!(combined.contains("64"), "{combined}");
    }

    #[test]
    fn system_prompt_is_just_the_hint_when_the_caller_has_no_system() {
        let payload = json!({"max_tokens": 64});
        let combined = system_prompt(&payload).unwrap();
        assert!(combined.contains("64"), "{combined}");
    }

    #[test]
    fn system_prompt_is_just_the_callers_system_without_max_tokens() {
        let payload = json!({"system": "You are terse."});
        assert_eq!(system_prompt(&payload).as_deref(), Some("You are terse."));
    }

    #[test]
    fn system_prompt_is_none_when_neither_is_present() {
        assert!(system_prompt(&json!({})).is_none());
    }

    #[test]
    fn stream_events_are_forwarded_verbatim() {
        let mut converter = CliToAnthropic::new("sonnet".into());
        let mut out = converter.push(
            stream_event(json!({
                "type": "message_start",
                "message": {"id": "msg_1", "model": "claude-sonnet-5", "content": []},
            }))
            .as_bytes(),
        );
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "hi"},
                }))
                .as_bytes(),
            ),
        );
        // `message_delta`/`message_stop` are held back until `result`
        // arrives — the normal order, since the CLI always emits `result`
        // last — so both are included here to see them go out.
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 1},
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.push(stream_event(json!({"type": "message_stop"})).as_bytes()));
        out.extend(
            converter
                .push(b"{\"type\":\"result\",\"is_error\":false,\"stop_reason\":\"end_turn\"}\n"),
        );

        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_delta",
                "message_delta",
                "message_stop"
            ]
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"text\":\"hi\""), "{text}");
        // Nothing was rebuilt, so the upstream ids survive.
        assert!(text.contains("msg_1"), "{text}");
    }

    #[test]
    fn streaming_result_usage_replaces_the_mid_run_snapshot() {
        // Same correction as the non-streaming path
        // (`the_result_events_usage_wins_over_the_mid_run_snapshot`), applied
        // to the streamed converter: the `assistant`/`message_delta` usage
        // seen while the run is still in progress is a snapshot, and
        // `result`'s usage is the total.
        let mut converter = CliToAnthropic::new("sonnet".into());
        converter.push(
            stream_event(json!({
                "type": "message_start",
                "message": {"usage": {"input_tokens": 2, "output_tokens": 0}},
            }))
            .as_bytes(),
        );
        converter.push(
            stream_event(json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 1},
            }))
            .as_bytes(),
        );
        let mut out = converter.push(stream_event(json!({"type": "message_stop"})).as_bytes());
        // Nothing goes out yet: `message_stop` is held back for `result`.
        assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));

        out.extend(converter.push(
            b"{\"type\":\"result\",\"is_error\":false,\"stop_reason\":\"end_turn\",\
              \"usage\":{\"input_tokens\":2,\"output_tokens\":5,\"cache_read_input_tokens\":15456}}\n",
        ));

        assert_eq!(names(&out), vec!["message_delta", "message_stop"]);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"output_tokens\":5"), "{text}");
        assert!(text.contains("15456"), "{text}");
    }

    #[test]
    fn a_result_with_no_usage_field_still_flushes_the_pending_snapshot() {
        let mut converter = CliToAnthropic::new("sonnet".into());
        converter.push(
            stream_event(json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 3},
            }))
            .as_bytes(),
        );
        let mut out = converter.push(stream_event(json!({"type": "message_stop"})).as_bytes());
        out.extend(
            converter
                .push(b"{\"type\":\"result\",\"is_error\":false,\"stop_reason\":\"end_turn\"}\n"),
        );

        assert_eq!(names(&out), vec!["message_delta", "message_stop"]);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"output_tokens\":3"), "{text}");
    }

    #[test]
    fn the_clis_own_bookkeeping_lines_are_dropped() {
        let mut converter = CliToAnthropic::new("sonnet".into());
        let mut out = converter.push(b"{\"type\":\"system\",\"subtype\":\"hook_started\"}\n");
        out.extend(converter.push(b"{\"type\":\"rate_limit_event\"}\n"));
        out.extend(converter.push(b"{\"type\":\"assistant\",\"message\":{}}\n"));
        assert!(out.is_empty(), "{:?}", String::from_utf8_lossy(&out));
    }

    #[test]
    fn lines_split_across_chunks_are_reassembled() {
        let whole = stream_event(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "reassembled"},
        }));
        let (a, b) = whole.split_at(whole.len() / 2);

        let mut converter = CliToAnthropic::new("sonnet".into());
        let mut out = converter.push(a.as_bytes());
        assert!(out.is_empty(), "half a line must emit nothing");
        out.extend(converter.push(b.as_bytes()));
        assert!(String::from_utf8(out).unwrap().contains("reassembled"));
    }

    #[test]
    fn a_child_that_stops_mid_message_is_still_closed() {
        let mut converter = CliToAnthropic::new("sonnet".into());
        let mut out = converter.push(
            stream_event(json!({"type": "message_start", "message": {"content": []}})).as_bytes(),
        );
        out.extend(converter.finish());
        let emitted = names(&out);
        assert!(
            emitted.contains(&"message_delta".to_string()),
            "{emitted:?}"
        );
        assert!(emitted.contains(&"message_stop".to_string()), "{emitted:?}");
    }

    #[test]
    fn a_run_that_produced_nothing_still_gets_a_well_formed_sequence() {
        let mut converter = CliToAnthropic::new("fallback-model".into());
        let out = converter.finish();
        assert_eq!(
            names(&out),
            vec!["message_start", "message_delta", "message_stop"]
        );
        assert!(String::from_utf8(out).unwrap().contains("fallback-model"));
    }

    #[test]
    fn finish_flushes_a_message_stop_still_pending_on_result_and_is_then_idempotent() {
        // The child sent `message_stop` but was killed (or crashed) before a
        // `result` line ever arrived — `finish` must still deliver the
        // pending `message_stop` rather than leave the client's stream open
        // forever, and a second call must not repeat it.
        let mut converter = CliToAnthropic::new("sonnet".into());
        let held = converter.push(stream_event(json!({"type": "message_stop"})).as_bytes());
        assert!(held.is_empty(), "{:?}", String::from_utf8_lossy(&held));

        let first = converter.finish();
        assert_eq!(names(&first), vec!["message_stop"]);
        assert!(converter.finish().is_empty());
    }

    #[test]
    fn a_second_assistant_message_after_the_first_message_stop_is_dropped_entirely() {
        // `--allowedTools ""` lets the model attempt a tool call that then
        // gets refused, and the CLI can respond with a whole second
        // `message_start`…`message_stop` pair in the same run. Before PR16
        // `finished` was set on the first `message_stop`, so everything past
        // it was silently ignored; this is the same outcome via
        // `turn_ended`, kept independent of `finished` so `result`'s usage
        // correction (the reason `finished` could no longer be set there)
        // still lands.
        let mut converter = CliToAnthropic::new("sonnet".into());
        let mut out = converter.push(
            stream_event(json!({
                "type": "message_start",
                "message": {"id": "msg_1", "model": "claude-sonnet-5", "content": []},
            }))
            .as_bytes(),
        );
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "hi"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "tool_use"},
                    "usage": {"output_tokens": 1},
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.push(stream_event(json!({"type": "message_stop"})).as_bytes()));

        // The second assistant turn: a fresh `message_start` re-using index
        // 0, which — forwarded verbatim — would collide with the first
        // turn's own index 0 downstream.
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "message_start",
                    "message": {"id": "msg_2", "model": "claude-sonnet-5", "content": []},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "should not appear"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 9},
                }))
                .as_bytes(),
            ),
        );
        out.extend(converter.push(stream_event(json!({"type": "message_stop"})).as_bytes()));

        out.extend(
            converter
                .push(b"{\"type\":\"result\",\"is_error\":false,\"stop_reason\":\"end_turn\"}\n"),
        );

        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_delta",
                "message_delta",
                "message_stop"
            ]
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("msg_1"), "{text}");
        assert!(!text.contains("msg_2"), "{text}");
        assert!(!text.contains("should not appear"), "{text}");
        // The first turn's own `message_delta` usage survives — a second
        // `message_stop`'s never overwrites it.
        assert!(text.contains("\"output_tokens\":1"), "{text}");
    }

    #[test]
    fn finish_synthesizes_a_message_stop_when_only_message_delta_was_pending() {
        // The child died after streaming `message_delta` but before
        // `message_stop` — `emit_pending` must not leave the client with a
        // dangling `message_delta` and no terminator (the pre-PR16 `close`
        // always emitted both).
        let mut converter = CliToAnthropic::new("sonnet".into());
        let held = converter.push(
            stream_event(json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {"output_tokens": 4},
            }))
            .as_bytes(),
        );
        assert!(held.is_empty(), "{:?}", String::from_utf8_lossy(&held));

        let out = converter.finish();
        assert_eq!(names(&out), vec!["message_delta", "message_stop"]);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"output_tokens\":4"), "{text}");
    }

    #[test]
    fn a_run_with_no_stream_events_still_reports_the_results_usage() {
        // Issue #11: a run can produce only a top-level `result` line with no
        // `stream_event` at all. `result`'s usage has to survive to the
        // `message_delta` `close()` synthesizes, or every such run gets
        // recorded as zero tokens.
        let mut converter = CliToAnthropic::new("sonnet".into());
        let held = converter.push(
            b"{\"type\":\"result\",\"is_error\":false,\"stop_reason\":\"end_turn\",\
              \"usage\":{\"input_tokens\":7,\"output_tokens\":9}}\n",
        );
        assert!(held.is_empty(), "{:?}", String::from_utf8_lossy(&held));

        let out = converter.finish();
        assert_eq!(
            names(&out),
            vec!["message_start", "message_delta", "message_stop"]
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"output_tokens\":9"), "{text}");
    }

    #[test]
    fn a_failed_run_becomes_an_error_frame() {
        let mut converter = CliToAnthropic::new("sonnet".into());
        let out = converter
            .push(b"{\"type\":\"result\",\"is_error\":true,\"result\":\"usage limit reached\"}\n");
        assert_eq!(names(&out), vec!["error"]);
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("usage limit reached"));
        // Terminal: nothing may follow it.
        assert!(converter.finish().is_empty());
    }

    #[test]
    fn an_errored_results_usage_still_reaches_the_downstream_scanner() {
        // Issue #19: a run can fail after already billing real tokens (a
        // usage-limit hit mid-generation), and this converter's own SSE
        // output is what `crate::usage::parse::SseUsageScanner` reads —
        // there is no other path for that usage to be recorded. It must
        // land on a `message_delta`, the only event the scanner reads usage
        // from besides `message_start`.
        let mut converter = CliToAnthropic::new("sonnet".into());
        let out = converter.push(
            b"{\"type\":\"result\",\"is_error\":true,\"result\":\"usage limit reached\",\
              \"usage\":{\"input_tokens\":11,\"output_tokens\":22}}\n",
        );
        assert_eq!(names(&out), vec!["message_delta", "error"]);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"input_tokens\":11"), "{text}");
        assert!(text.contains("\"output_tokens\":22"), "{text}");
        assert!(text.contains("usage limit reached"), "{text}");

        let mut scanner =
            crate::usage::parse::SseUsageScanner::new(crate::config::ApiKind::AnthropicMessages);
        scanner.push(text.as_bytes());
        let usage = scanner.finish();
        assert_eq!(usage.input_tokens, Some(11));
        assert_eq!(usage.output_tokens, Some(22));
    }

    #[test]
    fn a_client_that_already_received_content_is_not_left_hanging_on_error() {
        // Once content has streamed, the client is mid-message. `error` is
        // still the last frame it gets — no `message_stop` follows, matching
        // both Anthropic's own contract (an `error` event is itself
        // terminal) and `ChatToAnthropic`'s error path in
        // `translate/stream.rs` — but the pending `message_delta` snapshot
        // is flushed first rather than silently dropped, so the client's
        // last view of the turn is coherent instead of truncated with no
        // explanation of where the content it already has stands.
        let mut converter = CliToAnthropic::new("sonnet".into());
        let mut out = converter.push(
            stream_event(json!({
                "type": "message_start",
                "message": {"id": "msg_1", "content": []},
            }))
            .as_bytes(),
        );
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "partial answer"},
                }))
                .as_bytes(),
            ),
        );
        out.extend(
            converter.push(
                stream_event(json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 3},
                }))
                .as_bytes(),
            ),
        );
        // No `message_stop` stream event this time — the run fails before
        // one is ever sent.
        out.extend(
            converter.push(
                b"{\"type\":\"result\",\"is_error\":true,\"result\":\"usage limit reached\"}\n",
            ),
        );

        assert_eq!(
            names(&out),
            vec![
                "message_start",
                "content_block_delta",
                "message_delta",
                "error"
            ]
        );
        let text = String::from_utf8(out).unwrap();
        // The delta held from the stream (its own usage snapshot) is what
        // gets flushed, since the errored `result` here carries none of its
        // own to override it with.
        assert!(text.contains("\"output_tokens\":3"), "{text}");
        assert!(converter.is_finished());
        // Idempotent: `finish()` after an error adds nothing more.
        assert!(converter.finish().is_empty());
    }

    #[test]
    fn a_successful_result_line_emits_nothing_extra() {
        let mut converter = CliToAnthropic::new("sonnet".into());
        let out = converter
            .push(b"{\"type\":\"result\",\"is_error\":false,\"stop_reason\":\"end_turn\"}\n");
        assert!(out.is_empty());
    }

    #[test]
    fn a_non_streaming_run_assembles_the_assistant_message() {
        let jsonl = concat!(
            r#"{"type":"system","subtype":"init"}"#,
            "\n",
            r#"{"type":"assistant","message":{"id":"msg_9","model":"claude-sonnet-5","role":"assistant","type":"message","content":[{"type":"text","text":"PONG"}],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":1,"cache_read_input_tokens":15456}}}"#,
            "\n",
            r#"{"type":"result","is_error":false,"stop_reason":"end_turn"}"#,
            "\n",
        );
        let message = message_from_jsonl(jsonl.as_bytes(), "sonnet").unwrap();
        assert_eq!(message["content"][0]["text"], "PONG");
        // Taken from `result`, because the assistant event reports null.
        assert_eq!(message["stop_reason"], "end_turn");
        assert_eq!(message["model"], "claude-sonnet-5");
        // Usage survives untouched, cache counts included.
        assert_eq!(message["usage"]["cache_read_input_tokens"], 15456);
    }

    #[test]
    fn the_result_events_usage_wins_over_the_mid_run_snapshot() {
        // Measured: a 5-token answer reports `output_tokens: 1` on the assistant
        // event and 5 on the result event. Accounting reads this body.
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"stop_reason":null,"usage":{"input_tokens":2,"output_tokens":1}}}"#,
            "\n",
            r#"{"type":"result","is_error":false,"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":5,"cache_read_input_tokens":15456}}"#,
            "\n",
        );
        let message = message_from_jsonl(jsonl.as_bytes(), "sonnet").unwrap();
        assert_eq!(message["usage"]["output_tokens"], 5);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 15456);
    }

    #[test]
    fn a_non_streaming_failure_is_reported_as_an_error() {
        let jsonl = r#"{"type":"result","is_error":true,"result":"usage limit reached"}"#;
        let err = message_from_jsonl(jsonl.as_bytes(), "sonnet").unwrap_err();
        assert!(err.contains("usage limit reached"), "{err}");
    }

    #[test]
    fn output_without_an_assistant_turn_is_an_error_not_an_empty_message() {
        let err = message_from_jsonl(b"{\"type\":\"system\"}\n", "sonnet").unwrap_err();
        assert!(err.contains("no assistant message"), "{err}");
    }

    #[test]
    fn an_assistant_message_with_no_content_still_validates() {
        let jsonl = r#"{"type":"assistant","message":{"content":[],"stop_reason":null}}"#;
        let message = message_from_jsonl(jsonl.as_bytes(), "sonnet").unwrap();
        assert_eq!(message["content"][0]["text"], "");
        assert_eq!(message["model"], "sonnet");
    }

    #[test]
    fn the_recursion_guard_covers_the_variable_that_causes_it() {
        assert!(STRIPPED_ENV.contains(&"ANTHROPIC_BASE_URL"));
        assert!(STRIPPED_ENV.contains(&"ANTHROPIC_API_KEY"));
    }
}
