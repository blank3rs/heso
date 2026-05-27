//! # heso-engine-api
//!
//! The [`EngineApi`] trait is the historic abstraction the CLI dispatched
//! through. Today there is one concrete engine — [`heso-engine-fetch`]
//! (see [ADR 0012], native fetch + HTML5 + extractors) plus its
//! [`heso-engine-js`] DOM/script companion (see [ADR 0014]). This crate
//! exists as a thin shim while the CLI is migrated to depend on those
//! crates directly; [ADR 0017] retires the planner/AST shape that
//! originally motivated the abstraction.
//!
//! ## Status
//!
//! Slated for deletion in the ADR 0017 phase-2 code cleanup. New code
//! should not implement [`EngineApi`]; call `FetchEngine` (and, for the
//! hydration path, `JsSession`) directly.
//!
//! [`heso-engine-fetch`]: ../heso_engine_fetch/index.html
//! [`heso-engine-js`]: ../heso_engine_js/index.html
//! [ADR 0012]: ../../decisions/0012-fetch-only-native-engine.md
//! [ADR 0014]: ../../decisions/0014-bundled-quickjs-agent-dom.md
//! [ADR 0017]: ../../decisions/0017-verbs-as-agent-surface.md

use heso_core::{Result, Url};

/// A handle to an opened page. Concrete engines (`FetchEngine`) return
/// their own type; the CLI consumes it through this trait.
pub trait Page {
    /// Return the page's current URL.
    fn url(&self) -> &Url;

    /// Return the page's text content as a single string.
    fn text(&self) -> impl std::future::Future<Output = Result<String>> + Send;
}

/// The minimal contract the CLI dispatch layer uses to drive an engine.
/// Implemented today by [`heso_engine_fetch::FetchEngine`]; no other
/// implementor exists in-tree.
pub trait EngineApi: Send + Sync {
    /// The concrete page type this engine returns.
    type Page: Page;

    /// Open a URL and return a handle to the loaded page. Async because
    /// every real engine performs network I/O.
    fn open(&self, url: &Url) -> impl std::future::Future<Output = Result<Self::Page>> + Send;
}
