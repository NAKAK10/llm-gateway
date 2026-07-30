//! Extracting `usage` from all three wire formats.
//!
//! Each protocol reports token counts in a different place, and streaming
//! reports them differently again:
//!
//! | protocol | non-streaming | streaming |
//! |---|---|---|
//! | `anthropic-messages` | `usage` | `message_start.message.usage` (input) then `message_delta.usage` (output) |
//! | `openai-chat` | `usage` | a final chunk whose `usage` is non-null — only sent when `stream_options.include_usage` was requested |
//! | `openai-responses` | `response.usage` | `response.completed` → `response.usage` |
//!
//! Field names also differ: Anthropic uses `input_tokens`/`output_tokens`,
//! OpenAI uses `prompt_tokens`/`completion_tokens` for chat and
//! `input_tokens`/`output_tokens` for responses.

use crate::config::ApiKind;
use crate::usage::Usage;

/// Pull usage out of a complete, non-streaming JSON response body.
pub fn from_json(api: ApiKind, body: &serde_json::Value) -> Usage {
    let _ = (api, body);
    todo!("src/usage/parse.rs")
}

/// Incremental parser for a Server-Sent Events stream.
///
/// Fed raw bytes as they pass through to the client. Keeps only the bytes of a
/// partial event, so memory use stays bounded regardless of response length.
#[derive(Debug, Default)]
pub struct SseUsageScanner {
    pub(crate) api: Option<ApiKind>,
    pub(crate) buffer: Vec<u8>,
    pub(crate) usage: Usage,
}

impl SseUsageScanner {
    pub fn new(api: ApiKind) -> Self {
        Self {
            api: Some(api),
            buffer: Vec::new(),
            usage: Usage::default(),
        }
    }

    /// Observe a chunk. Never modifies or withholds it — the caller has already
    /// forwarded these bytes downstream.
    pub fn push(&mut self, chunk: &[u8]) {
        let _ = chunk;
        todo!("src/usage/parse.rs")
    }

    /// Usage seen so far. Complete once the stream has ended.
    pub fn finish(self) -> Usage {
        self.usage
    }
}
