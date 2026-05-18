//! # heso-engine-api
//!
//! The [`EngineApi`] trait is the swappable boundary between heso and the underlying
//! browser engine. All higher-level crates (`heso-extract`, `heso-act`, `heso-cli`,
//! `heso-mcp`) depend on this trait, never on a concrete engine.
//!
//! Concrete engines live in sibling crates: `heso-engine-servo` (M1), and potentially
//! `heso-engine-ladybird` or others later. See ADR 0002 and ADR 0003.
//!
//! ## Status
//!
//! M0-skeleton. The trait surface is intentionally sparse. It will grow as we
//! integrate Servo and learn which capabilities every engine must expose.

use heso_core::{Result, Url};

/// A handle to an opened page. Engine impls return their own concrete type;
/// downstream consumers use it via the trait surface.
///
/// **M0 placeholder.** The real `Page` will carry handles to the DOM, accessibility
/// tree, screenshot buffer, and identity for signed actions.
pub trait Page {
    /// Return the page's current URL.
    fn url(&self) -> &Url;

    /// Return the page's text content as a single string.
    ///
    /// **M0 placeholder.** Real impls will return structured text with positions.
    fn text(&self) -> impl std::future::Future<Output = Result<String>> + Send;
}

/// The contract every browser engine must satisfy to be used by heso.
///
/// Engine impls live in `heso-engine-<name>` crates. No engine-vendor types
/// appear in this trait's signatures — translate at the impl boundary.
pub trait EngineApi: Send + Sync {
    /// The concrete page type this engine returns.
    type Page: Page;

    /// Open a URL and return a handle to the loaded page.
    ///
    /// Async per [ADR 0011] — engines are async-first (chromiumoxide today,
    /// any future engine almost certainly the same). Concrete impls may
    /// satisfy this with a sync body (it's wrapped in a ready future
    /// automatically).
    ///
    /// [ADR 0011]: ../../decisions/0011-chromium-cdp-first-engine.md
    fn open(
        &self,
        url: &Url,
    ) -> impl std::future::Future<Output = Result<Self::Page>> + Send;
}
