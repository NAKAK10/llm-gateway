//! Reading token counts out of a response we are not allowed to modify.
//!
//! Cost accounting needs `usage`, but the whole point of the passthrough design
//! is that the response body reaches the client unchanged. So the stream is
//! *observed*, not rewritten: bytes are forwarded immediately and a copy is fed
//! to a parser that only looks for the usage fields.

pub mod parse;
pub mod tee;

/// Token counts for one request, as reported by the upstream.
///
/// Every field is `Option<u64>` rather than `u64` so that "never observed"
/// and "observed, and it was zero" are two different values instead of one.
/// That distinction matters for `merge`: Anthropic's streaming protocol
/// reports input tokens in `message_start` and output tokens in a later,
/// separate `message_delta` event, so each event's `Usage` only carries the
/// fields it actually saw and leaves the rest `None` — a plain `u64` with "0
/// means unset" would work for that case, but not once OpenAI-shaped usage
/// entered the picture: `openai_chat_usage`/`openai_responses_usage`
/// subtract `cached_tokens` out of `input_tokens` (see `usage::parse`'s
/// module docs), and that subtraction can legitimately land on 0 for a
/// fully-cached request. A `u64`-based "non-zero wins" merge would then treat
/// that observed zero as absent and leave a stale, higher value from an
/// earlier event in place — silently double-counting the cached portion
/// again. `Option` lets a real, observed 0 overwrite the same way any other
/// observed value does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Anthropic reports cache reads and writes separately; both are billed
    /// differently from ordinary input tokens, so they are kept apart.
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
}

impl Usage {
    /// True when nothing at all was ever observed — not "every field happened
    /// to be zero", which is a legitimate (if unusual) real response.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Merge a later observation into an earlier one.
    ///
    /// Anthropic sends input tokens in `message_start` and output tokens in
    /// `message_delta`, so a streamed response only has complete usage after
    /// both have been seen. Each event's `Usage` only sets the fields it
    /// actually reported (see the struct docs above), so "later `Some` wins,
    /// `None` leaves the running value alone" is enough to combine them
    /// correctly without needing to special-case either protocol here.
    pub fn merge(&mut self, other: Usage) {
        if other.input_tokens.is_some() {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens.is_some() {
            self.output_tokens = other.output_tokens;
        }
        if other.cache_read_tokens.is_some() {
            self.cache_read_tokens = other.cache_read_tokens;
        }
        if other.cache_write_tokens.is_some() {
            self.cache_write_tokens = other.cache_write_tokens;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_keeps_the_running_value_for_a_field_the_later_observation_never_saw() {
        // Mirrors Anthropic's `message_start` (input) / `message_delta`
        // (output) split: the second event carries no `input_tokens` field
        // at all, and that must leave the first event's value in place.
        let mut usage = Usage {
            input_tokens: Some(50),
            output_tokens: None,
            cache_read_tokens: Some(8),
            cache_write_tokens: Some(2),
        };
        usage.merge(Usage {
            input_tokens: None,
            output_tokens: Some(75),
            cache_read_tokens: None,
            cache_write_tokens: None,
        });
        assert_eq!(
            usage,
            Usage {
                input_tokens: Some(50),
                output_tokens: Some(75),
                cache_read_tokens: Some(8),
                cache_write_tokens: Some(2),
            }
        );
    }

    #[test]
    fn merge_lets_an_observed_zero_overwrite_a_earlier_nonzero_value() {
        // The bug #23 fixes: a `u64`-based "non-zero wins" merge would treat
        // this second, genuinely-observed 0 as if it had never arrived, and
        // leave the stale 30 in place.
        let mut usage = Usage {
            input_tokens: Some(30),
            output_tokens: Some(5),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
        };
        usage.merge(Usage {
            input_tokens: Some(0),
            output_tokens: Some(10),
            cache_read_tokens: Some(100),
            cache_write_tokens: Some(0),
        });
        assert_eq!(usage.input_tokens, Some(0));
        assert_eq!(usage.output_tokens, Some(10));
        assert_eq!(usage.cache_read_tokens, Some(100));
    }

    #[test]
    fn unobserved_and_observed_zero_are_distinct() {
        assert_ne!(
            Usage::default(),
            Usage {
                input_tokens: Some(0),
                ..Default::default()
            }
        );
        assert!(Usage::default().is_empty());
        assert!(!Usage {
            input_tokens: Some(0),
            ..Default::default()
        }
        .is_empty());
    }
}
