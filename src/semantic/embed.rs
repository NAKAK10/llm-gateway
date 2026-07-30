//! Thin wrapper around [`model2vec_rs::model::StaticModel`], tuned for a
//! request-time classification call rather than bulk embedding.
//!
//! Two things make this different from calling `StaticModel` directly:
//!
//! - the text handed to the model is bounded *before* it ever reaches the
//!   tokenizer (see [`Embedder::embed`]), so a huge prompt costs the same as
//!   a short one;
//! - a tokenizer panic is caught rather than allowed to take the whole
//!   gateway down over one malformed request.

use std::panic::AssertUnwindSafe;

use model2vec_rs::model::StaticModel;

use crate::error::{Error, Result};
use crate::record;
use crate::semantic::model_file::ModelFiles;

/// Embedding dimension of `potion-multilingual-128M`.
pub const DIM: usize = 256;

/// Characters kept from the input *before* tokenization. Generous on
/// purpose — this bound exists only to cap tokenizer cost on pathological
/// input, not to shape what gets classified.
const MAX_CHARS: usize = 800;

/// Tokens kept per text, enforced by `encode_with_args` itself. 512 (the
/// crate's own default) is bulk-embedding sizing; a routing decision needs
/// far less context than that.
const MAX_TOKENS: usize = 64;

/// A loaded static embedding model.
pub struct Embedder {
    model: StaticModel,
}

impl Embedder {
    /// Load the model from `files`' local paths.
    ///
    /// Blocking: reading and parsing `model.safetensors` takes 1.5-4s. Call
    /// this from `tokio::task::spawn_blocking`, never from the request path.
    ///
    /// `files` must point at paths that already exist on disk —
    /// `StaticModel::from_pretrained` treats a non-existent path as a
    /// HuggingFace repo id and tries to fetch it over the network instead.
    /// `model_file::ensure` (the only producer of `ModelFiles`) guarantees
    /// this by construction.
    pub fn load(files: &ModelFiles) -> Result<Self> {
        let dir = files.config.parent().ok_or_else(|| {
            Error::Other(format!(
                "semantic model config path {} has no parent directory",
                files.config.display()
            ))
        })?;

        // `normalize: None` reads the model's own `config.json`, which pins
        // `normalize: true` for potion-multilingual-128M — `StaticModel`
        // then L2-normalizes every embedding it returns (see
        // `StaticModel::pool_ids` upstream). `Embedder::embed` relies on
        // that: it does not normalize again, and cosine similarity in
        // `index.rs` is a plain dot product.
        let model = StaticModel::from_pretrained(dir, None, None, None).map_err(|source| {
            Error::Other(format!(
                "failed to load semantic embedding model from {}: {source}",
                dir.display()
            ))
        })?;

        Ok(Self { model })
    }

    /// Embed `text` into a 256-dim, already-unit-length vector.
    ///
    /// `text` is truncated to `MAX_CHARS` characters (on a UTF-8 boundary,
    /// via [`crate::record::truncate`]) before it reaches the tokenizer, and
    /// `encode_with_args` is asked to stop at `MAX_TOKENS` tokens on top of
    /// that. The character cut matters on its own: `encode_with_args`'s
    /// `max_length` truncates *after* tokenizing the whole string it is
    /// given, so a multi-megabyte prompt would still pay full tokenization
    /// cost if it were passed through untrimmed.
    ///
    /// Runs on the caller's task — no `spawn_blocking` — since a bounded
    /// input keeps this under 100us, well below the cost of a task hop.
    ///
    /// Returns `None` if tokenization panicked or produced an empty vector,
    /// so the caller can fall back to the auto route's own model instead of
    /// failing the request.
    pub fn embed(&self, text: &str) -> Option<Vec<f32>> {
        let truncated = record::truncate(text, Some(MAX_CHARS));
        let sentences = [truncated];

        // `encode_with_args` calls `.expect(...)` on a tokenizer failure
        // instead of returning a `Result`, so a single malformed request
        // could otherwise panic the task handling it. `catch_unwind`
        // requires the closure to be `UnwindSafe`; `&StaticModel` does not
        // derive that (it holds a `tokenizers::Tokenizer` behind a plain
        // reference), so this asserts unwind safety rather than proving it
        // to the compiler. That assertion holds in practice: the call below
        // is read-only (`&self`), touches no state shared with the rest of
        // the gateway, and a caught panic simply drops the local `sentences`
        // value — there is nothing left half-mutated to observe afterwards.
        let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| {
            self.model.encode_with_args(&sentences, Some(MAX_TOKENS), 1)
        }));

        let mut vectors = outcome.ok()?;
        let vector = vectors.pop()?;
        if vector.is_empty() {
            return None;
        }
        Some(vector)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reports_a_useful_error_for_a_nonexistent_directory() {
        let files = ModelFiles {
            config: "/nonexistent/llm-gateway-semantic-test/config.json".into(),
            tokenizer: "/nonexistent/llm-gateway-semantic-test/tokenizer.json".into(),
            model: "/nonexistent/llm-gateway-semantic-test/model.safetensors".into(),
        };

        match Embedder::load(&files) {
            Ok(_) => panic!("expected loading a nonexistent model directory to fail"),
            Err(err) => assert!(err.to_string().contains("failed to load"), "{err}"),
        }
    }
}
