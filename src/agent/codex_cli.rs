//! Driving `codex exec` as an upstream: argv, prompt, and output translation.
//!
//! Same idea as [`super::claude_cli`] — a ChatGPT subscription authenticates the
//! Codex CLI, not the gateway, so the gateway runs the CLI — but the output shape
//! is different in one way that matters.
//!
//! # What the CLI gives us
//!
//! `codex exec --json` prints one JSON event per line:
//!
//! ```text
//! {"type":"thread.started","thread_id":"…"}
//! {"type":"turn.started"}
//! {"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"…"}}
//! {"type":"turn.completed","usage":{…}}
//! ```
//!
//! These are **item-level, not token-level**: an `agent_message` arrives complete.
//! So unlike the Claude CLI transport there is nothing to stream incrementally —
//! a streaming request gets the whole answer as one delta when the turn
//! completes. That is a latency difference, not a correctness one, and it is why
//! this transport is documented as unsuitable for a route someone watches type.
//!
//! # Which protocol this renders
//!
//! `openai-chat`. Codex's own events are not any published wire format, so the
//! gateway has to pick one to render, and chat completions is the one the most
//! clients can reach — including Claude Code, via the existing
//! `anthropic-messages` → `openai-chat` translation.
//!
//! # What is lost
//!
//! The caller's tools (Codex runs its own), multi-turn structure (one prompt),
//! sampling parameters, and token-by-token streaming. Errors are *not* lost: a
//! rejected model comes back as the CLI's own message, which is the one thing
//! that matters when a plan does not include a model.

use serde_json::Value;

/// Environment variables removed from the child.
///
/// `OPENAI_API_KEY` is the load-bearing one here: with it set, `codex` would bill
/// an API account for what the user asked to run on their subscription — the
/// exact opposite of the point. The `ANTHROPIC_*` entries are stripped for the
/// same reason `claude_cli` strips them: a `serve` started in a redirected shell
/// must not pass its own redirect down to a child.
pub const STRIPPED_ENV: &[&str] = &[
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
];

/// The program name looked up on `PATH`.
pub const PROGRAM: &str = "codex";

/// Build the child's arguments.
///
/// `--sandbox read-only` and an empty working directory are the tool-denial
/// equivalent of `claude_cli`'s `--allowedTools ""`: Codex will still run shell
/// commands if it decides to, and read-only in a directory with nothing in it is
/// the narrowest thing it can do. `--dangerously-bypass-approvals-and-sandbox` is
/// deliberately never passed.
pub fn args(model: &str, cwd: &std::path::Path, extra: &[String]) -> Vec<String> {
    let mut args = vec![
        "exec".to_string(),
        "--json".to_string(),
        "--sandbox".to_string(),
        "read-only".to_string(),
        // The scratch directory is not a git repository, and Codex refuses to
        // run outside one unless told this.
        "--skip-git-repo-check".to_string(),
        "-C".to_string(),
        cwd.to_string_lossy().to_string(),
    ];
    if !is_cli_default(model) {
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    args.extend(extra.iter().cloned());
    args
}

/// Whether a model reference means "whatever the CLI is configured to use".
///
/// A route's model part cannot be empty (it is half of `provider/model`), so the
/// literal `default` is the way to say "do not pass `-m` at all". That matters
/// for Codex specifically: which models a ChatGPT plan allows is not knowable
/// from here, and the CLI's own configured default is the only safe guess.
pub fn is_cli_default(model: &str) -> bool {
    let model = model.trim();
    model.is_empty() || model == "default"
}

/// The prompt written to the child's stdin.
///
/// Codex reads stdin when no prompt argument is given. `system` is prepended
/// rather than passed separately: `codex exec` has no system-prompt flag, and
/// dropping the caller's system message would silently change the answer.
pub fn prompt(payload: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(system) = payload
        .get("system")
        .map(crate::translate::request::system_text)
    {
        if !system.trim().is_empty() {
            parts.push(system);
        }
    }
    // An `openai-chat` request keeps its system message in `messages`, so both
    // shapes are handled: whichever the client sent, it ends up first.
    let body = super::claude_cli::prompt(payload);
    if !body.trim().is_empty() {
        parts.push(body);
    }
    parts.join("\n\n")
}

/// Why a run produced no answer.
///
/// The distinction is about what to *show*: when the CLI reported a reason, that
/// sentence is the whole story and adding stderr just appends progress noise
/// ("Reading prompt from stdin..."). When it reported nothing, stderr is all
/// there is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The CLI said what went wrong; use it verbatim.
    Reported(String),
    /// No assistant message and no reason — the caller should add stderr.
    Silent,
}

/// The assistant text and usage from a finished `codex exec --json` run.
pub fn result_from_jsonl(bytes: &[u8]) -> Result<(String, Usage), Failure> {
    let mut text = String::new();
    let mut usage = Usage::default();
    let mut error: Option<String> = None;

    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(raw) = std::str::from_utf8(line) else {
            continue;
        };
        let Ok(event) = serde_json::from_str::<Value>(raw.trim()) else {
            continue;
        };

        match event.get("type").and_then(|t| t.as_str()) {
            Some("item.completed") => {
                let Some(item) = event.get("item") else {
                    continue;
                };
                // Only `agent_message` is the answer. An `error` item is a
                // warning the CLI prints mid-run ("model metadata not found")
                // and appears on successful runs too, so it is not fatal.
                if item.get("type").and_then(|t| t.as_str()) == Some("agent_message") {
                    if let Some(part) = item.get("text").and_then(|t| t.as_str()) {
                        text.push_str(part);
                    }
                }
            }
            Some("turn.completed") => {
                if let Some(reported) = event.get("usage") {
                    usage = Usage::from_codex(reported);
                }
            }
            // Both shapes carry the reason; `turn.failed` nests it.
            Some("error") => {
                if let Some(message) = event.get("message").and_then(|m| m.as_str()) {
                    error = Some(unwrap_nested_error(message));
                }
            }
            Some("turn.failed") => {
                if let Some(message) = event
                    .get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                {
                    error = Some(unwrap_nested_error(message));
                }
            }
            _ => {}
        }
    }

    // A failure with text already produced keeps the text: a turn that answered
    // and then hit a limit is more useful truncated than discarded.
    if text.trim().is_empty() {
        return Err(match error {
            Some(reason) => Failure::Reported(reason),
            None => Failure::Silent,
        });
    }
    Ok((text, usage))
}

/// Codex nests an upstream error as a JSON string inside its own `message`.
/// Unwrapping it turns
/// `{"type":"error","status":400,"error":{"message":"…"}}` into just the message,
/// which is what a user needs to read.
fn unwrap_nested_error(message: &str) -> String {
    serde_json::from_str::<Value>(message)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| message.to_string())
}

/// Token counts, in `openai-chat` names.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cached_tokens: u64,
}

impl Usage {
    fn from_codex(usage: &Value) -> Self {
        let field = |key: &str| usage.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        Self {
            prompt_tokens: field("input_tokens"),
            completion_tokens: field("output_tokens"),
            cached_tokens: field("cached_input_tokens"),
        }
    }

    fn to_json(self) -> Value {
        serde_json::json!({
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "total_tokens": self.prompt_tokens + self.completion_tokens,
            "prompt_tokens_details": { "cached_tokens": self.cached_tokens },
        })
    }
}

/// One `chat.completion` body, as a non-streaming `openai-chat` client expects.
pub fn chat_completion(text: &str, usage: Usage, model: &str) -> Value {
    serde_json::json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::now_v7()),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }],
        "usage": usage.to_json(),
    })
}

/// The SSE a streaming `openai-chat` client expects, for an answer that arrived
/// all at once.
///
/// One content chunk, one finish chunk, one usage chunk, `[DONE]` — the shape
/// `usage::parse` and `translate::stream` are already written against, so a
/// client cannot tell this apart from a provider that simply answered quickly.
pub fn chat_stream(text: &str, usage: Usage, model: &str) -> Vec<u8> {
    let id = format!("chatcmpl-{}", uuid::Uuid::now_v7());
    let mut out = Vec::new();
    let mut push = |value: Value| {
        out.extend_from_slice(format!("data: {value}\n\n").as_bytes());
    };

    push(serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "model": model,
        "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": Value::Null}],
    }));
    push(serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "model": model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
    }));
    push(serde_json::json!({
        "id": id, "object": "chat.completion.chunk", "model": model,
        "choices": [], "usage": usage.to_json(),
    }));
    out.extend_from_slice(b"data: [DONE]\n\n");
    out
}

/// An `openai-chat` error body, for a run that produced nothing usable.
pub fn chat_error(message: &str) -> Value {
    serde_json::json!({
        "error": { "type": "api_error", "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn args_keep_codex_read_only_and_out_of_a_repo() {
        let args = args("gpt-5", std::path::Path::new("/tmp/scratch"), &[]);
        assert!(args.starts_with(&["exec".to_string(), "--json".to_string()]));
        assert!(args.windows(2).any(|w| w == ["--sandbox", "read-only"]));
        assert!(args.iter().any(|a| a == "--skip-git-repo-check"));
        assert!(args.windows(2).any(|w| w == ["-C", "/tmp/scratch"]));
        assert!(args.windows(2).any(|w| w == ["-m", "gpt-5"]));
        // Never, under any circumstances, from this code path.
        assert!(!args
            .iter()
            .any(|a| a == "--dangerously-bypass-approvals-and-sandbox"));
    }

    #[test]
    fn an_empty_or_default_model_is_left_to_the_clis_own_configuration() {
        for model in ["", "default", "  default  "] {
            let args = args(model, std::path::Path::new("/tmp/x"), &[]);
            assert!(
                !args.iter().any(|a| a == "-m"),
                "`{model}` should not pass -m"
            );
        }
        assert!(is_cli_default("default"));
        assert!(!is_cli_default("gpt-5"));
    }

    #[test]
    fn the_system_prompt_is_prepended_because_codex_has_no_flag_for_it() {
        let payload = json!({
            "system": "You are terse.",
            "messages": [{"role": "user", "content": "hello"}],
        });
        assert_eq!(prompt(&payload), "You are terse.\n\nhello");
    }

    #[test]
    fn a_bare_request_prompts_with_just_the_message() {
        let payload = json!({"messages": [{"role": "user", "content": "hello"}]});
        assert_eq!(prompt(&payload), "hello");
    }

    #[test]
    fn a_successful_run_yields_text_and_usage() {
        let jsonl = concat!(
            r#"{"type":"thread.started","thread_id":"t1"}"#,
            "\n",
            r#"{"type":"turn.started"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"PONG"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{"input_tokens":11,"cached_input_tokens":4,"output_tokens":2}}"#,
            "\n",
        );
        let (text, usage) = result_from_jsonl(jsonl.as_bytes()).unwrap();
        assert_eq!(text, "PONG");
        assert_eq!(
            usage,
            Usage {
                prompt_tokens: 11,
                completion_tokens: 2,
                cached_tokens: 4
            }
        );
    }

    /// The failure this transport will actually hit: a plan that does not allow
    /// the requested model. Verified against the real CLI — the reason is nested
    /// as JSON inside `message`, and a user needs the inner sentence, not the
    /// envelope.
    #[test]
    fn a_rejected_model_reports_the_clis_own_sentence() {
        let jsonl = concat!(
            r#"{"type":"item.completed","item":{"id":"item_0","type":"error","message":"Model metadata for `gpt-5.6-sol` not found."}}"#,
            "\n",
            r#"{"type":"error","message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-5.6-sol' model is not supported when using Codex with a ChatGPT account.\"}}"}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"message\":\"The 'gpt-5.6-sol' model is not supported when using Codex with a ChatGPT account.\"}}"}}"#,
            "\n",
        );
        let err = result_from_jsonl(jsonl.as_bytes()).unwrap_err();
        assert_eq!(
            err,
            Failure::Reported(
                "The 'gpt-5.6-sol' model is not supported when using Codex with a ChatGPT account."
                    .to_string()
            )
        );
        // The mid-run "metadata not found" warning must not be mistaken for the
        // failure — it appears on successful runs too.
        let Failure::Reported(message) = err else {
            unreachable!()
        };
        assert!(!message.contains("metadata"), "{message}");
    }

    #[test]
    fn output_with_neither_an_answer_nor_a_reason_is_silent() {
        // Only then is stderr worth appending — see `Failure`.
        let err = result_from_jsonl(b"{\"type\":\"turn.started\"}\n").unwrap_err();
        assert_eq!(err, Failure::Silent);
    }

    #[test]
    fn text_produced_before_a_failure_is_kept() {
        let jsonl = concat!(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"partial"}}"#,
            "\n",
            r#"{"type":"turn.failed","error":{"message":"usage limit reached"}}"#,
            "\n",
        );
        let (text, _) = result_from_jsonl(jsonl.as_bytes()).unwrap();
        assert_eq!(text, "partial");
    }

    #[test]
    fn a_non_streaming_body_is_a_chat_completion() {
        let body = chat_completion(
            "hi",
            Usage {
                prompt_tokens: 3,
                completion_tokens: 1,
                cached_tokens: 0,
            },
            "gpt-5",
        );
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "hi");
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"]["prompt_tokens"], 3);
        assert_eq!(body["usage"]["total_tokens"], 4);
    }

    /// The synthesized stream has to satisfy the two readers already in the
    /// pipeline: the usage scanner and the chat→anthropic translator.
    #[test]
    fn the_synthesized_stream_is_readable_by_the_existing_machinery() {
        let sse = chat_stream(
            "hello",
            Usage {
                prompt_tokens: 7,
                completion_tokens: 2,
                cached_tokens: 1,
            },
            "gpt-5",
        );

        let mut scanner =
            crate::usage::parse::SseUsageScanner::new(crate::config::ApiKind::OpenaiChat);
        scanner.push(&sse);
        let usage = scanner.finish();
        // `prompt_tokens` (7) includes `cached_tokens` (1); the scanner
        // reports `input_tokens` as the non-cached remainder, so the two
        // don't double-count when summed downstream.
        assert_eq!(usage.input_tokens, Some(6));
        assert_eq!(usage.output_tokens, Some(2));
        assert_eq!(usage.cache_read_tokens, Some(1));

        let mut translator = crate::translate::stream::ChatToAnthropic::new("gpt-5".to_string());
        let mut out = translator.push(&sse);
        out.extend(translator.finish());
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("event: message_start"), "{text}");
        assert!(text.contains("\"text\":\"hello\""), "{text}");
        assert!(text.contains("event: message_stop"), "{text}");
    }

    #[test]
    fn the_recursion_and_billing_guards_cover_the_right_variables() {
        // With OPENAI_API_KEY set, `codex` would bill an API account for what the
        // user asked to run on their subscription.
        assert!(STRIPPED_ENV.contains(&"OPENAI_API_KEY"));
        assert!(STRIPPED_ENV.contains(&"ANTHROPIC_BASE_URL"));
    }
}
