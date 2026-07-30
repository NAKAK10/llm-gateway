//! Semantic routing (issue #2), gated behind the `semantic` feature.
//!
//! This module covers fetching and verifying the embedding model
//! ([`model_file`]), loading it and turning text into vectors ([`embed`]),
//! and classifying request text against `routes[].description` ([`index`]).
//! Wiring the result into [`crate::route`] / request handling is separate,
//! later work — this module does not touch request handling.

pub mod embed;
pub mod index;
pub mod model_file;
