//! Reading token counts out of a response we are not allowed to modify.
//!
//! Cost accounting needs `usage`, but the whole point of the passthrough design
//! is that the response body reaches the client unchanged. So the stream is
//! *observed*, not rewritten: bytes are forwarded immediately and a copy is fed
//! to a parser that only looks for the usage fields.

pub mod parse;
pub mod tee;

/// Token counts for one request, as reported by the upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Anthropic reports cache reads and writes separately; both are billed
    /// differently from ordinary input tokens, so they are kept apart.
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Merge a later observation into an earlier one.
    ///
    /// Anthropic sends input tokens in `message_start` and output tokens in
    /// `message_delta`, so a streamed response only has complete usage after
    /// both have been seen. Later non-zero values win.
    pub fn merge(&mut self, other: Usage) {
        if other.input_tokens > 0 {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens > 0 {
            self.output_tokens = other.output_tokens;
        }
        if other.cache_read_tokens > 0 {
            self.cache_read_tokens = other.cache_read_tokens;
        }
        if other.cache_write_tokens > 0 {
            self.cache_write_tokens = other.cache_write_tokens;
        }
    }
}
