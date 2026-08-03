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
        buffered_body(child, model, permit).await
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
async fn buffered_body(
    child: tokio::process::Child,
    model: String,
    permit: OwnedSemaphorePermit,
) -> Result<Spawned> {
    let output = child.wait_with_output().await?;
    drop(permit);

    let (status, body, detail) = match claude_cli::message_from_jsonl(&output.stdout, &model) {
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
}
