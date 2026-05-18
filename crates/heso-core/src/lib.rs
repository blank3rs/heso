//! # heso-core
//!
//! Shared types and the canonical error enum for the heso workspace.
//!
//! Other crates depend on this for: [`Url`] re-export, [`Error`] enum, [`Result`] alias.
//! Keep this crate small — it sits at the bottom of the dependency graph and changes here
//! ripple everywhere.

pub use url::Url;

/// Workspace-wide result alias. Use this in public APIs of crates that
/// build on `heso-core`.
pub type Result<T> = std::result::Result<T, Error>;

/// The top-level error enum returned by `heso-core` and re-used as a wrapping
/// error by higher-level crates that don't want to define their own.
///
/// Per [`.agent/CONVENTIONS.md`](../../.agent/CONVENTIONS.md), each downstream crate
/// should normally define its own `thiserror` enum with a variant that wraps
/// `heso_core::Error` where useful.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A URL failed to parse.
    #[error("invalid URL: {0}")]
    InvalidUrl(#[from] url::ParseError),

    /// An I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Catch-all for unimplemented surface during M0 skeleton work.
    /// Should not exist in released crates — replace with a real variant when filled in.
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),
}
