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

use std::process::Stdio;

use bytes::Bytes;
use futures_util::Stream;
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};
use crate::route::Target;

/// A spawned upstream, shaped like the HTTP one so `upstream::Accepted` can hold
/// either.
pub struct Spawned {
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    pub body: super::upstream::BodyStream,
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
    let cwd = scratch_dir()?;
    let system = claude_cli::system(payload);
    let args = claude_cli::args(
        &target.model_ref.model,
        system.as_deref(),
        streaming,
        &target.agent_args,
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
            body: stream_body(child, model),
        })
    } else {
        buffered_body(child, model).await
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
/// rather than by whoever polls last.
fn stream_body(mut child: tokio::process::Child, model: String) -> super::upstream::BodyStream {
    use tokio::io::AsyncReadExt;

    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);

    tokio::spawn(async move {
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
                stderr.chars().take(500).collect()
            };
            converter.error(&mut out, &detail);
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

/// Wait for the child and return one complete Anthropic message.
async fn buffered_body(child: tokio::process::Child, model: String) -> Result<Spawned> {
    let output = child.wait_with_output().await?;

    let (status, body) = match claude_cli::message_from_jsonl(&output.stdout, &model) {
        Ok(message) => (http::StatusCode::OK, message),
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
            (
                http::StatusCode::BAD_GATEWAY,
                serde_json::json!({
                    "type": "error",
                    "error": { "type": "api_error", "message": message },
                }),
            )
        }
    };

    let bytes = Bytes::from(body.to_string());
    Ok(Spawned {
        status,
        headers: json_headers(),
        body: Box::pin(futures_util::stream::once(async move { Ok(bytes) })),
    })
}

/// Whether the CLI this transport needs is installed. Used by
/// `llm-gateway providers`, which has no request to send but can still answer
/// "would this provider work at all?".
pub fn is_available() -> bool {
    std::process::Command::new(claude_cli::PROGRAM)
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
