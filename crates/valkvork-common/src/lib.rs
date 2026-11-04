//! Common types and utilities for Valkvork.

pub mod config;
pub mod errors;
pub mod models;
pub mod tls;
pub mod units;
pub mod validation;

pub use errors::{ValidationError, ValidationErrorCode, ValkvorkError};

/// Initialise JSON `tracing` logs (NFR-07). `RUST_LOG` controls the level and
/// defaults to `info`.
pub fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .try_init();
}
