//! Semantic routing (issue #2), gated behind the `semantic` feature.
//!
//! This module currently covers only fetching and verifying the embedding
//! model (see [`model_file`]). Inference, index construction and wiring the
//! result into [`crate::route`] are separate, later work — this module does
//! not touch request handling.

pub mod model_file;
