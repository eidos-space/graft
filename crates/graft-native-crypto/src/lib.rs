//! Target-aware feature routing for the existing `RustCrypto` SHA-2 backend.
//!
//! This crate has no runtime API or cryptographic implementation. Its optional
//! dependency selects assembly only on supported native targets, leaving other
//! targets on SHA-2's default backend. A separate package is necessary because
//! Cargo features cannot directly enable a dependency feature conditionally on
//! the compilation target.
