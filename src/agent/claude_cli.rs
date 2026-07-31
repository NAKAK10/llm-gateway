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
//! - **Sampling controls.** `temperature`, `top_p`, `stop_sequences` and
//!   `max_tokens` have no CLI equivalent and are dropped.
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
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.close(&mut out, "end_turn");
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
            // stream event, so it goes out untouched.
            Some("stream_event") => {
                let Some(inner) = event.get("event") else {
                    return;
                };
                let Some(name) = inner.get("type").and_then(|t| t.as_str()) else {
                    return;
                };
                match name {
                    "message_start" => self.started = true,
                    "message_stop" => self.finished = true,
                    _ => {}
                }
                emit(out, name, inner);
            }
            // A failed run. `result` carries the reason, and after it the CLI
            // says nothing more, so this is terminal.
            Some("result") if event.get("is_error").and_then(|e| e.as_bool()) == Some(true) => {
                let message = event
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or("claude CLI reported an error")
                    .to_string();
                self.error(out, &message);
            }
            // `system`, `rate_limit_event`, `assistant`, a successful `result`:
            // all redundant while partial messages are streaming.
            _ => {}
        }
    }

    /// Emit an Anthropic `error` frame and stop.
    pub fn error(&mut self, out: &mut Vec<u8>, message: &str) {
        if self.finished {
            return;
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
        emit(
            out,
            "message_delta",
            &serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": stop_reason, "stop_sequence": Value::Null },
                "usage": { "output_tokens": 0 },
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
        out.extend(converter.push(stream_event(json!({"type": "message_stop"})).as_bytes()));

        assert_eq!(
            names(&out),
            vec!["message_start", "content_block_delta", "message_stop"]
        );
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("\"text\":\"hi\""), "{text}");
        // Nothing was rebuilt, so the upstream ids survive.
        assert!(text.contains("msg_1"), "{text}");
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
    fn finish_is_idempotent_and_never_follows_message_stop() {
        let mut converter = CliToAnthropic::new("sonnet".into());
        converter.push(stream_event(json!({"type": "message_stop"})).as_bytes());
        assert!(converter.finish().is_empty());
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
