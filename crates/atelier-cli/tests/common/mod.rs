//! Shared test helpers for atelier-cli integration tests.
//!
//! Rust integration tests compile each `tests/*.rs` file as its own crate
//! root, so anything imported from a `common/` subdirectory is also
//! compiled per-test-file. The `#[allow(dead_code)]` blanket below
//! suppresses the otherwise-noisy warnings for helpers that only some
//! test files reference.

#![allow(dead_code)]

pub mod canonical;
