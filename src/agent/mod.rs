//! Agent CLIs as upstream providers.
//!
//! A subscription is not an API key. A Claude Pro/Max plan authenticates *Claude
//! Code*, and no credential the gateway could hold would let it speak to
//! Anthropic on that plan's behalf. What it can do is ask the official client to
//! do the work — the client already holds the login — and translate what comes
//! back into the protocol the caller speaks.
//!
//! That makes `claude -p` a provider like any other from the config's point of
//! view:
//!
//! ```json5
//! "claude-subscription": {
//!   transport: "claude-cli",
//!   api: "anthropic-messages",
//! },
//! ```
//!
//! Only the transport differs, so routes, fallback, tracing and accounting all
//! work unchanged. The protocol *is* `anthropic-messages`, because that is what
//! the CLI's output is — see [`claude_cli`] for the translation and for what a
//! process-backed upstream cannot carry (the caller's tools, above all).

pub mod claude_cli;
pub mod codex_cli;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{LazyLock, Mutex};

use bytes::Bytes;
use futures_util::Stream;
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{Error, Result};
use crate::record::trace_log::DroppedByTransport;
use crate::route::Target;

/// A spawned upstream, shaped like the HTTP one so `upstream::Accepted` can hold
/// either.
pub struct Spawned {
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    pub body: super::upstream::BodyStream,
    /// Set exactly when `status` is not a success — the same detail message
    /// (CLI stderr excerpt, exit status, or the CLI's own reported reason)
    /// that is also serialized into `body`'s error JSON, kept here as a
    /// plain string too so `upstream::send_with_fallback` can put it on the
    /// `TraceAttempt` it records (see issue #39). `None` for a streaming
    /// attempt: `spawn_claude` reports `status: OK` for one unconditionally,
    /// before the child has run at all, so there is nothing to report yet.
    pub detail: Option<String>,
}

/// How many `claude -p`/`codex exec` child processes one provider may run at
/// once when its config sets no `maxConcurrent` of its own.
///
/// A real gateway's trace log showed a burst of ~100 requests/minute (a
/// parallel subagent fan-out) spawning that many `claude` processes with
/// nothing to smooth it out, and roughly half came back `http_502` — see the
/// 2026-08-03 entry in `docs/decisions.md` and issue #40. Conservative
/// rather than tuned: cheap to raise per-provider via `maxConcurrent` once an
/// operator knows their own machine's and subscription's real ceiling.
pub const DEFAULT_MAX_CONCURRENT: u32 = 8;

/// Bounds concurrent agent-CLI child processes, one [`Semaphore`] per
/// provider id, so a burst of requests queues for a free slot instead of
/// spawning every process at once (see [`DEFAULT_MAX_CONCURRENT`]).
///
/// A single process-wide instance ([`LIMITER`]) rather than something
/// threaded through `AppState`: `crate::agent::spawn` already has everything
/// it needs (the provider id and its `max_concurrent` off `Target`) without
/// carrying a limiter reference through `upstream::send_with_fallback`'s
/// generic `build` closure. A provider's semaphore is sized once, the first
/// time that provider id is used, and kept for the process's life — the same
/// once-per-process-lifetime tradeoff `ui::pca::BasisCache` already accepts
/// for its own reasons; a hot-reloaded change to `maxConcurrent` takes effect
/// only for a provider id not already in use.
struct AgentLimiter {
    semaphores: Mutex<HashMap<String, std::sync::Arc<Semaphore>>>,
}

impl AgentLimiter {
    fn new() -> Self {
        Self {
            semaphores: Mutex::new(HashMap::new()),
        }
    }

    /// Wait for a free slot for `provider`, creating its semaphore (sized to
    /// `max_concurrent`) the first time this provider id is seen.
    async fn acquire(&self, provider: &str, max_concurrent: u32) -> OwnedSemaphorePermit {
        let sem = {
            // A `std::sync::Mutex` guarding only a hashmap lookup/insert — no
            // `.await` while held, so this never blocks the runtime.
            let mut map = self
                .semaphores
                .lock()
                .expect("agent limiter mutex poisoned");
            map.entry(provider.to_string())
                .or_insert_with(|| {
                    std::sync::Arc::new(Semaphore::new(max_concurrent.max(1) as usize))
                })
                .clone()
        };
        sem.acquire_owned()
            .await
            .expect("agent limiter semaphore is never closed")
    }
}

static LIMITER: LazyLock<AgentLimiter> = LazyLock::new(AgentLimiter::new);

/// What an agent-CLI transport's translation is about to throw away from
/// `payload`, for the trace log — see [`crate::record::trace_log::DroppedByTransport`]
/// and the module docs on [`claude_cli`] ("What is lost, and why"). `None`
/// when nothing would be dropped, so callers can record `Option::None`
/// instead of an all-empty object.
///
/// Pure inspection: this does not change what gets sent. `spawn` and its
/// helpers ([`claude_cli::prompt`], [`claude_cli::system_prompt`]) already do
/// the actual dropping independently of this function.
pub fn dropped_by_transport(payload: &serde_json::Value) -> Option<DroppedByTransport> {
    let tools = payload
        .get("tools")
        .and_then(|v| v.as_array())
        .filter(|tools| !tools.is_empty())
        .map(|tools| tools.len());

    let messages = payload.get("messages").and_then(|v| v.as_array());
    let assistant_prefill = messages
        .and_then(|m| m.last())
        .and_then(|last| last.get("role"))
        .and_then(|r| r.as_str())
        .is_some_and(|role| role == "assistant");
    let flattened_messages = messages
        .map(|m| m.len())
        .filter(|&n| n >= 2);

    let dropped = DroppedByTransport {
        tools,
        assistant_prefill,
        flattened_messages,
    };
    (!dropped.is_empty()).then_some(dropped)
}

/// Run one request against `target`'s agent CLI.
///
/// Errors only when the child could not be started at all — a missing binary, a
/// scratch directory that cannot be created. Everything the child itself does
/// wrong (a failed run, no output, a usage limit) comes back as a normal
/// response with an Anthropic error body, because by then the request has an
/// upstream and the client deserves the reason rather than a gateway error.
pub async fn spawn(
    target: &Target,
    payload: &serde_json::Value,
    streaming: bool,
) -> Result<Spawned> {
    match target.transport {
        crate::config::Transport::CodexCli => spawn_codex(target, payload, streaming).await,
        // `Http` cannot reach here: `upstream` only calls this for an agent
        // transport.
        _ => spawn_claude(target, payload, streaming).await,
    }
}

async fn spawn_claude(
    target: &Target,
    payload: &serde_json::Value,
    streaming: bool,
) -> Result<Spawned> {
    let cwd = scratch_dir()?;
    let system = claude_cli::system_prompt(payload, target.is_utility_bypass);
    let args = claude_cli::args(
        &target.model_ref.model,
        system.as_deref(),
        streaming,
        &target.agent_args,
        target.is_utility_bypass,
    );
    let prompt = claude_cli::prompt(payload);

    let mut command = tokio::process::Command::new(claude_cli::PROGRAM);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Without this an aborted request leaves a `claude` process running: the
        // response stream is dropped, but the child would keep generating.
        .kill_on_drop(true);
    for name in claude_cli::STRIPPED_ENV {
        command.env_remove(name);
    }

    // Held for however long the child process itself runs — including its
    // full streaming lifetime, well past this function's return — so the cap
    // actually bounds concurrently *running* processes, not just concurrent
    // calls to this function. See [`AgentLimiter`].
    let permit = LIMITER
        .acquire(&target.model_ref.provider, target.max_concurrent)
        .await;

    let mut child = command.spawn().map_err(|source| {
        Error::Other(format!(
            "could not start `{}` for provider `{}`: {source}. \
         A `claude-cli` provider needs Claude Code installed and logged in.",
            claude_cli::PROGRAM,
            target.model_ref.provider,
        ))
    })?;

    // The prompt goes over stdin, and stdin is then closed: the CLI waits for it
    // otherwise, and a request that hangs for a few seconds per call is worse
    // than one that is slightly harder to read in a process listing.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        drop(stdin);
    }

    let model = target.model_ref.model.clone();
    if streaming {
        Ok(Spawned {
            status: http::StatusCode::OK,
            headers: sse_headers(),
            body: stream_body(child, model, permit),
            detail: None,
        })
    } else {
        buffered_body(child, model, permit, target.is_utility_bypass).await
    }
}

/// Diagnostic-only, opt-in dump of a buffered `claude -p` round trip.
///
/// This exists to answer one question: what does `claude -p` actually return
/// for `is_utility_bypass` requests (the Claude Code Auto Mode security
/// monitor), whose response length has been observed swinging from ~5 tokens
/// to ~1000 for what looks like the same input. Input-side causes (tools,
/// assistant prefill, brevity-hint injection) are already ruled out by
/// measurement, so this records the response side instead: the CLI's raw
/// stdout/stderr and what `message_from_jsonl` made of them.
///
/// Silent no-op unless `LLM_GATEWAY_UTILITY_DUMP` is set to a file path — with
/// it unset this module behaves exactly as before, no I/O and no formatting
/// work performed.
mod utility_dump {
    use std::io::Write;

    const ENV_VAR: &str = "LLM_GATEWAY_UTILITY_DUMP";

    /// Append one record for `stdout`/`stderr`/the `message_from_jsonl`
    /// outcome to the path named by `LLM_GATEWAY_UTILITY_DUMP`, if set.
    ///
    /// Only called for `is_utility_bypass` targets (see call site in
    /// `buffered_body`) — non-bypass traffic is not what's under
    /// investigation and this stays silent for it regardless of the env var.
    /// Any I/O failure (bad path, permissions) is swallowed after a
    /// `tracing::warn!`: a diagnostic dump must never be able to break a
    /// request that would otherwise have succeeded.
    pub(super) fn record(stdout: &[u8], stderr: &[u8], outcome: &Result<serde_json::Value, String>) {
        let Some(path) = std::env::var_os(ENV_VAR) else {
            return;
        };
        let record = format_record(stdout, stderr, outcome);
        let result = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(record.as_bytes()));
        if let Err(source) = result {
            tracing::warn!(
                path = %std::path::Path::new(&path).display(),
                %source,
                "could not write LLM_GATEWAY_UTILITY_DUMP record"
            );
        }
    }

    /// Render one human-readable record. Pulled out from [`record`] so it can
    /// be unit-tested without touching the filesystem.
    fn format_record(stdout: &[u8], stderr: &[u8], outcome: &Result<serde_json::Value, String>) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let stdout_text = String::from_utf8_lossy(stdout);
        let stderr_text = String::from_utf8_lossy(stderr).trim().to_string();

        let mut out = String::new();
        out.push_str(&"=".repeat(80));
        out.push('\n');
        out.push_str(&format!("timestamp_unix: {now}\n"));
        out.push_str(&format!("stdout_bytes: {}\n", stdout.len()));
        out.push_str("--- stdout (raw JSONL) ---\n");
        out.push_str(&stdout_text);
        if !stdout_text.ends_with('\n') {
            out.push('\n');
        }
        if !stderr_text.is_empty() {
            out.push_str("--- stderr ---\n");
            out.push_str(&stderr_text);
            out.push('\n');
        }
        match outcome {
            Ok(message) => {
                out.push_str("message_from_jsonl: OK\n");
                out.push_str("--- final body ---\n");
                out.push_str(
                    &serde_json::to_string_pretty(message)
                        .unwrap_or_else(|_| message.to_string()),
                );
                out.push('\n');
            }
            Err(detail) => {
                out.push_str("message_from_jsonl: ERROR\n");
                out.push_str("--- error detail ---\n");
                out.push_str(detail);
                out.push('\n');
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn formats_a_successful_record() {
            let outcome = Ok(serde_json::json!({"type": "message", "content": []}));
            let text = format_record(b"{\"type\":\"assistant\"}\n", b"", &outcome);
            assert!(text.contains("stdout_bytes: 21"));
            assert!(text.contains("{\"type\":\"assistant\"}"));
            assert!(text.contains("message_from_jsonl: OK"));
            assert!(text.contains("\"type\": \"message\""));
            assert!(!text.contains("--- stderr ---"));
        }

        #[test]
        fn formats_a_failed_record_with_stderr() {
            let outcome: Result<serde_json::Value, String> =
                Err("no assistant message found".to_string());
            let text = format_record(b"", b"  usage limit reached  \n", &outcome);
            assert!(text.contains("stdout_bytes: 0"));
            assert!(text.contains("--- stderr ---\nusage limit reached"));
            assert!(text.contains("message_from_jsonl: ERROR"));
            assert!(text.contains("no assistant message found"));
        }

        /// With the env var unset, `record` must not touch the filesystem at
        /// all — the whole point of the opt-in is that a gateway that never
        /// sets `LLM_GATEWAY_UTILITY_DUMP` behaves exactly as before.
        #[test]
        fn record_is_a_no_op_without_the_env_var() {
            // SAFETY: no other test in this process reads or writes this
            // specific env var, so there is no data race on it here.
            unsafe {
                std::env::remove_var(ENV_VAR);
            }
            let outcome = Ok(serde_json::json!({}));
            // If this touched the filesystem with no path configured it
            // would panic well before returning; reaching here is the
            // assertion.
            record(b"stdout", b"stderr", &outcome);
        }
    }
}

/// Run one request against `codex exec`.
///
/// Buffered rather than streamed, because Codex's events are item-level: the
/// assistant message arrives complete, so there is nothing to forward
/// incrementally. A streaming request still gets a well-formed `openai-chat`
/// stream — it simply all arrives at once. See [`codex_cli`].
async fn spawn_codex(
    target: &Target,
    payload: &serde_json::Value,
    streaming: bool,
) -> Result<Spawned> {
    let cwd = scratch_dir()?;
    let args = codex_cli::args(&target.model_ref.model, &cwd, &target.agent_args);
    let prompt = codex_cli::prompt(payload);

    let mut command = tokio::process::Command::new(codex_cli::PROGRAM);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in codex_cli::STRIPPED_ENV {
        command.env_remove(name);
    }

    // Held until this function returns — codex is always buffered (see the
    // doc comment above), so by then the child has already fully run; see
    // [`AgentLimiter`].
    let _permit = LIMITER
        .acquire(&target.model_ref.provider, target.max_concurrent)
        .await;

    let mut child = command.spawn().map_err(|source| {
        Error::Other(format!(
            "could not start `{}` for provider `{}`: {source}. \
             A `codex-cli` provider needs the Codex CLI installed and logged in.",
            codex_cli::PROGRAM,
            target.model_ref.provider,
        ))
    })?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(prompt.as_bytes()).await;
        drop(stdin);
    }

    let output = child.wait_with_output().await?;
    let model = target.model_ref.model.clone();

    match codex_cli::result_from_jsonl(&output.stdout) {
        Ok((text, usage)) => {
            let (headers, bytes) = if streaming {
                (
                    sse_headers(),
                    Bytes::from(codex_cli::chat_stream(&text, usage, &model)),
                )
            } else {
                (
                    json_headers(),
                    Bytes::from(codex_cli::chat_completion(&text, usage, &model).to_string()),
                )
            };
            Ok(Spawned {
                status: http::StatusCode::OK,
                headers,
                body: Box::pin(futures_util::stream::once(async move { Ok(bytes) })),
                detail: None,
            })
        }
        Err(failure) => {
            // When the CLI said what went wrong, that sentence is the whole
            // story — "this model is not supported when using Codex with a
            // ChatGPT account" tells a user exactly what to change, and
            // appending stderr to it only adds progress noise.
            let message = match failure {
                codex_cli::Failure::Reported(reason) => reason,
                codex_cli::Failure::Silent => {
                    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    if stderr.is_empty() {
                        "codex produced no assistant message".to_string()
                    } else {
                        format!(
                            "codex produced no assistant message: {}",
                            stderr.chars().take(300).collect::<String>()
                        )
                    }
                }
            };
            let bytes = Bytes::from(codex_cli::chat_error(&message).to_string());
            Ok(Spawned {
                status: http::StatusCode::BAD_GATEWAY,
                headers: json_headers(),
                body: Box::pin(futures_util::stream::once(async move { Ok(bytes) })),
                detail: Some(message),
            })
        }
    }
}

/// Where the child runs: an empty directory, so `--setting-sources project`
/// finds no project settings and the child has nothing of the user's to read.
fn scratch_dir() -> Result<std::path::PathBuf> {
    let dir = crate::paths::config_dir().join("agent-cwd");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn sse_headers() -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/event-stream"),
    );
    headers
}

fn json_headers() -> http::HeaderMap {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers
}

/// Translate the child's stdout into SSE as it arrives.
///
/// A reader task owns the child and pushes finished frames down a channel, which
/// keeps the stream itself trivial and means the child is reaped by that task
/// rather than by whoever polls last. `permit` moves into that task too, so
/// the concurrency slot it holds (see [`AgentLimiter`]) is only released once
/// the child has actually exited — not when this function returns, well
/// before the child's streaming output is done.
fn stream_body(
    mut child: tokio::process::Child,
    model: String,
    permit: OwnedSemaphorePermit,
) -> super::upstream::BodyStream {
    use tokio::io::AsyncReadExt;

    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);

    tokio::spawn(async move {
        let _permit = permit;
        let mut converter = claude_cli::CliToAnthropic::new(model);
        let mut stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return,
        };
        let mut buf = [0u8; 8192];

        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let out = converter.push(&buf[..n]);
                    if !out.is_empty() && tx.send(Bytes::from(out)).await.is_err() {
                        // Client is gone. `kill_on_drop` handles the child when
                        // this task returns.
                        return;
                    }
                }
                Err(_) => break,
            }
        }

        // A non-zero exit with nothing useful on stdout is the case worth
        // explaining: the reason is on stderr, and without this the client would
        // see an empty, well-formed, entirely unhelpful message.
        let status = child.wait().await;
        let failed = !matches!(&status, Ok(status) if status.success());
        let mut out = Vec::new();
        if failed {
            let mut stderr = String::new();
            if let Some(mut handle) = child.stderr.take() {
                let mut raw = Vec::new();
                let _ = handle.read_to_end(&mut raw).await;
                stderr = String::from_utf8_lossy(&raw).trim().to_string();
            }
            let detail = if stderr.is_empty() {
                match status {
                    Ok(status) => format!("`claude` exited with {status}"),
                    Err(err) => format!("`claude` could not be waited for: {err}"),
                }
            } else {
                stderr.chars().take(500).collect::<String>()
            };
            // A non-zero exit after the turn already completed (`message_stop`
            // seen, `result`'s usage applied) is not a failed request — the
            // answer is real and about to be flushed by `finish()` below, so
            // no error frame is injected into a stream the client has
            // already seen close successfully. But it is not nothing,
            // either: the CLI exiting badly on every turn is a real
            // degradation (a crash loop, a broken install) that would
            // otherwise be invisible, so it is at least logged.
            if converter.is_finished() {
                tracing::warn!(
                    detail = %detail,
                    "claude CLI exited non-zero after completing its turn"
                );
            } else {
                converter.error(&mut out, &detail, None);
            }
        }
        out.extend(converter.finish());
        if !out.is_empty() {
            let _ = tx.send(Bytes::from(out)).await;
        }
    });

    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|bytes| (Ok(bytes), rx))
    }))
}

/// Wait for the child and return one complete Anthropic message. `permit` is
/// only for its `Drop` — held until the child has fully run, then released.
///
/// `is_utility_bypass` gates nothing about the request or response — it only
/// decides whether [`utility_dump::record`] writes a diagnostic copy of this
/// round trip (see that module's docs).
async fn buffered_body(
    child: tokio::process::Child,
    model: String,
    permit: OwnedSemaphorePermit,
    is_utility_bypass: bool,
) -> Result<Spawned> {
    let output = child.wait_with_output().await?;
    drop(permit);

    let jsonl_result = claude_cli::message_from_jsonl(&output.stdout, &model);
    if is_utility_bypass {
        utility_dump::record(&output.stdout, &output.stderr, &jsonl_result);
    }

    let (status, body, detail) = match jsonl_result {
        Ok(message) => (http::StatusCode::OK, message, None),
        Err(detail) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let message = if stderr.is_empty() {
                detail
            } else {
                format!(
                    "{detail} ({})",
                    stderr.chars().take(500).collect::<String>()
                )
            };
            let body = serde_json::json!({
                "type": "error",
                "error": { "type": "api_error", "message": message },
            });
            (http::StatusCode::BAD_GATEWAY, body, Some(message))
        }
    };

    let bytes = Bytes::from(body.to_string());
    Ok(Spawned {
        status,
        headers: json_headers(),
        body: Box::pin(futures_util::stream::once(async move { Ok(bytes) })),
        detail,
    })
}

/// Whether the CLI this transport needs is installed. Used by
/// `llm-gateway providers`, which has no request to send but can still answer
/// "would this provider work at all?".
pub fn is_available_for(transport: crate::config::Transport) -> bool {
    let program = match transport {
        crate::config::Transport::CodexCli => codex_cli::PROGRAM,
        _ => claude_cli::PROGRAM,
    };
    std::process::Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Type-checks that a `Spawned` body satisfies the stream contract the rest of
/// the pipeline is written against.
#[allow(dead_code)]
fn assert_body_is_a_byte_stream(
    body: super::upstream::BodyStream,
) -> impl Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send {
    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The whole point of [`AgentLimiter`] (issue #40): once a provider is at
    /// its cap, the next `acquire` must actually wait rather than handing out
    /// an unbounded number of permits — and it must unblock the moment a
    /// slot frees up, not stay stuck.
    #[tokio::test]
    async fn agent_limiter_queues_beyond_its_cap() {
        let limiter = AgentLimiter::new();
        let p1 = limiter.acquire("p", 2).await;
        let _p2 = limiter.acquire("p", 2).await;

        let blocked =
            tokio::time::timeout(Duration::from_millis(50), limiter.acquire("p", 2)).await;
        assert!(
            blocked.is_err(),
            "a third acquire at cap 2 must not resolve until a permit is freed"
        );

        drop(p1);
        let _p3 = tokio::time::timeout(Duration::from_millis(200), limiter.acquire("p", 2))
            .await
            .expect("dropping p1 should free a slot for the queued acquire");
    }

    /// Each provider id gets its own semaphore — a busy `claude-cli` provider
    /// must never throttle an unrelated `codex-cli` (or a second `claude-cli`
    /// alias) provider's requests.
    #[tokio::test]
    async fn agent_limiter_tracks_providers_independently() {
        let limiter = AgentLimiter::new();
        let _a = limiter.acquire("provider-a", 1).await;

        let _b = tokio::time::timeout(Duration::from_millis(50), limiter.acquire("provider-b", 1))
            .await
            .expect("a different provider id must not share provider-a's limit");
    }

    /// `max_concurrent: 0` in a resolved `Target` would deadlock every
    /// request against that provider forever — `validate` already rejects
    /// this at config-load time (`config::validate`'s `maxConcurrent must be
    /// at least 1` check), but the limiter itself also refuses to construct
    /// a zero-permit semaphore, so a config that somehow reaches this point
    /// anyway degrades to "one at a time" instead of hanging.
    #[tokio::test]
    async fn agent_limiter_treats_zero_as_one() {
        let limiter = AgentLimiter::new();
        let _p = tokio::time::timeout(Duration::from_millis(50), limiter.acquire("p", 0))
            .await
            .expect("a max_concurrent of 0 must still grant one permit, not hang forever");
    }

    #[test]
    fn dropped_by_transport_reports_tool_count() {
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "a"}, {"name": "b"}],
        });
        let dropped = dropped_by_transport(&payload).expect("tools were dropped");
        assert_eq!(dropped.tools, Some(2));
        assert!(!dropped.assistant_prefill);
        assert_eq!(dropped.flattened_messages, None);
    }

    #[test]
    fn dropped_by_transport_flags_assistant_prefill() {
        let payload = serde_json::json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "partial"},
            ],
        });
        let dropped = dropped_by_transport(&payload).expect("prefill was dropped");
        assert!(dropped.assistant_prefill);
        assert_eq!(dropped.flattened_messages, Some(2));
    }

    #[test]
    fn dropped_by_transport_is_none_when_nothing_would_be_dropped() {
        let payload = serde_json::json!({
            "messages": [{"role": "user", "content": "hi"}],
        });
        assert!(dropped_by_transport(&payload).is_none());
    }

    #[test]
    fn dropped_by_transport_counts_flattened_messages() {
        let payload = serde_json::json!({
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"},
                {"role": "user", "content": "three"},
            ],
        });
        let dropped = dropped_by_transport(&payload).expect("messages were flattened");
        assert_eq!(dropped.flattened_messages, Some(3));
        // Last message is user, not assistant.
        assert!(!dropped.assistant_prefill);
    }
}
