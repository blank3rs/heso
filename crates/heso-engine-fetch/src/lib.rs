//! # heso-engine-fetch
//!
//! The static path of heso — the headless browser for the agent-relevant
//! half of the web. Native HTTP + HTML implementation of
//! [`heso_engine_api::EngineApi`]: `reqwest` + `scraper`, no Chrome, no
//! Node, single Rust binary, deploys anywhere `heso.exe` runs.
//!
//! Per [ADR 0012], this is the static engine. Per [ADR 0014], the JS engine
//! lives in the sibling crate [`heso-engine-js`](../heso_engine_js/index.html)
//! (QuickJS via `rquickjs`, Phase 1A landed). Together they cover the
//! in-scope half from [ADR 0016] — fetch, parse, JS, DOM (Phase 1B),
//! forms, clicks, sessions — and explicitly drop the rendering half
//! (canvas, WebGL, video, CSS layout).
//!
//! ## What it does
//!
//! - HTTP fetch via [`reqwest`] (`rustls` TLS, gzip/brotli, HTTP/2, follows
//!   up to 20 redirects).
//! - HTML parse via [`scraper`] (which uses Servo's `html5ever`).
//! - Visible-text extraction, walking the DOM and skipping
//!   `<script>` / `<style>` / `<noscript>` / `<template>` subtrees.
//! - Captures the post-redirect final URL on the [`FetchPage`] so
//!   `Page::url()` returns the URL the agent actually landed on.
//!
//! ## What it does not do
//!
//! - **No JavaScript on this path.** SPAs that need JS to populate the DOM
//!   will look empty here. Use the sibling JS engine for those (Phase 1B
//!   wires the DOM, Phase 1C runs `<script>` on load).
//! - **No CSS layout.** We extract semantic structure (HTML/ARIA), not
//!   visual position. That's the bet — see [ADR 0016].
//! - **No form submission with JS validation.** Plain `<form>` POSTs are
//!   possible later via the same `reqwest::Client`; JS-validated forms
//!   need the JS engine wired through.
//!
//! For the majority of read-only agent tasks (docs, news, blogs, marketing
//! sites, listings, simple e-commerce), this is enough — and the unique
//! heso value (signed receipts, content-addressed pages, terminal-shell
//! primitive vocabulary, deterministic replay) all works on top of it.
//!
//! ## Why this beats "reqwest + scraper in agent's own code"
//!
//! - **Stable element refs across snapshots** — future primitives (`find`,
//!   `cat @e3`) will assign deterministic `@e0/@e1/...` refs at parse time
//!   so a planner-emitted trace can name an element on one fetch and still
//!   refer to it on the next.
//! - **AX-tree-shaped representation** (planned) — derive semantic
//!   structure from ARIA + HTML5 tags so the agent sees a tree of
//!   `(role, name, ref)` instead of raw DOM nodes.
//! - **Signed deterministic receipts** — every `heso run` produces a
//!   `Receipt` with a BLAKE3 `trace_hash`. Static fetches are deterministic
//!   by construction (no clock, no RNG); the receipt is replayable
//!   anywhere given the same URL + recorded network bytes.
//! - **One agent contract** — `heso.run(start_url, request)`. Plain
//!   English in, signed structured data out. No CSS selectors, no XPath.
//!
//! [ADR 0012]: ../../decisions/0012-fetch-only-native-engine.md
//! [ADR 0014]: ../../decisions/0014-bundled-quickjs-agent-dom.md
//! [ADR 0016]: ../../decisions/0016-positioning-headless-browser-for-agents.md

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod actions;
pub mod data_attrs;
pub mod explore;
pub mod inline_data;
pub mod metadata;
pub mod plat;
pub mod tree;

pub use actions::{
    extract as extract_actions, filter as filter_actions, resolve as resolve_action, ElementRef,
};
pub use data_attrs::{extract as extract_data_attrs, DataAttrValue};
pub use explore::{
    linked_pages_to_json, ExploreOptions, LinkedPage, DEFAULT_LINK_CAP, HARD_LINK_CAP,
};
pub use inline_data::extract as extract_inline_data;
pub use metadata::{extract as extract_metadata, PageMetadata};
pub use plat::{
    canonical_json as plat_canonical_json, hash as plat_hash, verify as plat_verify,
    VerifyError as PlatVerifyError,
};
pub use tree::{build_tree, HtmlTree, LsRow, PwdRow, TreeError, TreeNode};

use std::collections::HashSet;
use std::sync::Arc;

use heso_core::{Result as HesoResult, Url};
use heso_engine_api::{EngineApi, Page};
use reqwest::Client;
use reqwest_cookie_store::CookieStoreMutex;
use scraper::{ElementRef as ScraperElementRef, Html, Node};

// ============================================================================
// Error type
// ============================================================================

/// Errors produced by the fetch engine.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// HTTP request failed (network, TLS, timeout, status mapping, …).
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    /// A URL string could not be parsed.
    #[error("URL parse error: {0}")]
    BadUrl(#[from] url::ParseError),
}

impl From<Error> for heso_core::Error {
    fn from(e: Error) -> Self {
        heso_core::Error::Io(std::io::Error::other(e.to_string()))
    }
}

// ============================================================================
// FetchEngine
// ============================================================================

/// A pure-Rust HTTP+HTML browser engine. Holds a shared `reqwest::Client`
/// (which itself owns a connection pool) plus the shared cookie jar
/// `reqwest` writes Set-Cookie responses into and reads Cookie requests
/// out of — clone-cheap, `Send + Sync`.
#[derive(Debug, Clone)]
pub struct FetchEngine {
    client: Client,
    /// Shared cookie jar. Same `Arc` is handed to `reqwest` via
    /// `ClientBuilder::cookie_provider` (the source of truth for
    /// `Set-Cookie` ingestion + `Cookie` header outgoing) **and**
    /// exposed via [`Self::cookie_jar`] so `heso-engine-js` can install
    /// the `document.cookie` getter/setter bridge against the same
    /// store. RFC 6265 parsing + path/domain matching lives inside
    /// `cookie_store::CookieStore`.
    cookie_jar: Arc<CookieStoreMutex>,
}

impl FetchEngine {
    /// Construct a new engine with sensible defaults: rustls TLS, gzip +
    /// brotli decoding, HTTP/2, follows up to 20 redirects, identifies as
    /// `heso/<version>`, and a fresh empty cookie jar wired into the
    /// `reqwest::Client` via `cookie_provider`. Cookies persist for the
    /// lifetime of this `FetchEngine` (and any clone — `Arc` semantics).
    pub fn new() -> HesoResult<Self> {
        let cookie_jar = Arc::new(CookieStoreMutex::default());
        let client = Client::builder()
            .user_agent(concat!("heso/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::limited(20))
            // Hand the shared jar to reqwest. Per `reqwest` docs:
            // calling `cookie_provider(my_store)` is the spec-compliant
            // alternative to `cookie_store(true)` — Set-Cookie response
            // headers go INTO `my_store`, outgoing requests pull Cookie
            // headers OUT of it. The jar is `Arc<CookieStoreMutex>`
            // shared with [`Self::cookie_jar`] so any other caller
            // (e.g. `heso-engine-js`'s `document.cookie` bridge) sees
            // the exact same store.
            .cookie_provider(cookie_jar.clone())
            .build()
            .map_err(Error::from)?;
        Ok(Self { client, cookie_jar })
    }

    /// Access the underlying [`reqwest::Client`]. Used by the [`explore`]
    /// module so per-link cartography fetches share connection pooling
    /// with the main `open` path. Crate-public on purpose — the agent
    /// surface should go through [`EngineApi::open`] or
    /// [`FetchEngine::open_with_explore`], not poke the HTTP client
    /// directly.
    pub(crate) fn client_ref(&self) -> &Client {
        &self.client
    }

    /// A public, clone-cheap handle to the underlying [`reqwest::Client`].
    ///
    /// Threaded into [`heso_engine_js::JsEngine::new_with_fetch`] so
    /// the JS-side `fetch()` global shares cookies, TLS state, the
    /// `heso/<version>` User-Agent, and (when item M lands) the
    /// recorded-network playback layer with the rest of the
    /// workspace.
    ///
    /// `reqwest::Client` is internally an `Arc` — wrapping in another
    /// `Arc` here is for API hygiene (so callers can hold an
    /// `Arc<Client>` directly without an extra clone in their
    /// signatures), not for cheaper cloning.
    pub fn client(&self) -> Arc<reqwest::Client> {
        Arc::new(self.client.clone())
    }

    /// A clone of the shared cookie jar. Same `Arc` reqwest writes
    /// `Set-Cookie` responses into and reads `Cookie` request headers
    /// out of — handing the same clone to
    /// `heso_engine_js::JsEngine::new_with_fetch_and_cookies` makes JS
    /// `document.cookie` reads/writes operate on the exact same store,
    /// which is what closes the login-flow loop (server sets cookie →
    /// next fetch sends it; JS sets cookie → next reqwest call sends
    /// it; reqwest receives cookie → next `document.cookie` read sees
    /// it).
    ///
    /// The jar lives behind `CookieStoreMutex` so concurrent access
    /// from background tasks (e.g. `open_with_explore`'s per-link
    /// fetches) is safe. Locking is short-lived inside the
    /// `CookieStore` trait impl `reqwest` calls into.
    pub fn cookie_jar(&self) -> Arc<CookieStoreMutex> {
        self.cookie_jar.clone()
    }

    /// Open a URL with optional link-graph cartography per
    /// [`ExploreOptions`]. Equivalent to [`EngineApi::open`] when `opts`
    /// is [`ExploreOptions::none`]; when exploration is enabled, the
    /// returned [`FetchPage`] has its `linked_pages` field populated with
    /// pre-fetched mini-trees for every link that survived the filters
    /// (same-origin, skip-list, dedupe, cap). Per-link errors are folded
    /// into [`LinkedPage::error`]; the whole call only fails if the
    /// initial fetch fails.
    ///
    /// See [`crate::explore`] for the full algorithm + filter rules.
    pub async fn open_with_explore(
        &self,
        url: &Url,
        opts: ExploreOptions,
    ) -> HesoResult<FetchPage> {
        let mut page = self.open_static(url).await?;
        if opts.is_disabled() {
            return Ok(page);
        }
        let visited = Arc::new(tokio::sync::Mutex::new({
            let mut s = HashSet::new();
            // Seed with the parent URL so a self-link can't be re-fetched
            // a level deeper.
            s.insert(canonical_self_key(&page.url));
            s
        }));
        // `explore` takes owned values so the spawned `JoinSet` workers
        // are `'static`. Cloning the parent's actions + url is cheap
        // relative to the network round-trips that follow.
        let linked = explore::explore(
            self.clone(),
            page.actions.clone(),
            page.url.clone(),
            opts,
            visited,
        )
        .await;
        page.linked_pages = linked;
        Ok(page)
    }

    /// HTTP-only fetch — returns `(final_url, raw_html_body)`. The
    /// post-redirect URL is the same one [`Self::open_static`] would
    /// land on, so callers can use this when they need the raw HTML
    /// for downstream parsing (e.g. the JS engine's `eval_with_html`
    /// path) without paying the cost of metadata/tree/actions
    /// extraction.
    pub async fn fetch_text(&self, url: &Url) -> HesoResult<(Url, String)> {
        let response = self
            .client
            .get(url.as_str())
            .send()
            .await
            .map_err(Error::from)?;
        let final_url_str = response.url().as_str().to_owned();
        let final_url = Url::parse(&final_url_str).map_err(Error::from)?;
        let html_text = response.text().await.map_err(Error::from)?;
        Ok((final_url, html_text))
    }

    /// Internal: the original static `open` path, factored out so
    /// [`FetchEngine::open_with_explore`] can compose it without
    /// re-dispatching through the trait (which lacks an options
    /// parameter).
    async fn open_static(&self, url: &Url) -> HesoResult<FetchPage> {
        let response = self
            .client
            .get(url.as_str())
            .send()
            .await
            .map_err(Error::from)?;

        let final_url_str = response.url().as_str().to_owned();
        let final_url = Url::parse(&final_url_str).map_err(Error::from)?;

        let html_text = response.text().await.map_err(Error::from)?;

        // Past the last `.await`; `scraper::Html` is `!Send` but that's
        // fine for sync work done in-frame.
        let doc = Html::parse_document(&html_text);
        let body_text = extract_visible_text_from_doc(&doc);
        let metadata = metadata::extract(&doc);
        let tree = tree::build_tree_from_doc(&doc, &final_url);
        let actions = actions::extract(&doc);
        let inline_data = inline_data::extract(&doc);
        let data_attrs = data_attrs::extract(&doc);

        Ok(FetchPage {
            url: final_url,
            body_text,
            body_html: html_text,
            tree,
            metadata,
            actions,
            linked_pages: Vec::new(),
            inline_data,
            data_attrs,
        })
    }
}

/// Canonical comparison key for a base URL — same shape
/// [`crate::explore`] uses for its visited-set. Local helper to avoid
/// pulling `pub(crate)` machinery up here.
fn canonical_self_key(u: &Url) -> String {
    let scheme = u.scheme().to_ascii_lowercase();
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let port = u
        .port_or_known_default()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let path = u.path();
    let query = u.query().unwrap_or("");
    if query.is_empty() {
        format!("{scheme}://{host}:{port}{path}")
    } else {
        format!("{scheme}://{host}:{port}{path}?{query}")
    }
}

impl Default for FetchEngine {
    fn default() -> Self {
        Self::new().expect("default reqwest Client should always build")
    }
}

// ============================================================================
// FetchPage
// ============================================================================

/// A loaded page. Pre-extracts everything an agent typically wants off a
/// single parse: post-redirect URL, visible body text, heading-derived
/// [`HtmlTree`] for `ls`/`cat` navigation, structured [`PageMetadata`]
/// (JSON-LD, OpenGraph, …), and the action graph (every interactive element
/// with a stable `@e0/@e1/…` ref). The parsed DOM is intentionally *not*
/// retained — `scraper::Html` is not `Send`, and every layer below this one
/// consumes pre-extracted views.
///
/// `linked_pages` is populated only when the page was opened via
/// [`FetchEngine::open_with_explore`] with a non-zero depth; for plain
/// [`EngineApi::open`] it's always empty.
#[derive(Debug, Clone)]
pub struct FetchPage {
    url: Url,
    body_text: String,
    /// The raw HTML body of the response, exactly as it came over the
    /// wire (post-decompression). Populated alongside `body_text` and
    /// `actions` so callers that need to hand the same bytes to a JS
    /// engine (for `<script>` execution, DOM mutation, etc.) don't
    /// have to issue a second HTTP round-trip via [`FetchEngine::fetch_text`].
    pub body_html: String,
    /// The page expressed as a navigable tree of sections, built from the
    /// HTML's heading structure. See [`crate::tree`].
    pub tree: HtmlTree,
    /// Structured metadata extracted from `<meta>`, `<link>`, and
    /// `<script type="application/ld+json">` blocks. See [`crate::metadata`].
    pub metadata: PageMetadata,
    /// The action graph — every interactive element (links, buttons,
    /// inputs, forms) with a stable `@e0/@e1/…` ref the agent can name in
    /// primitives like `cat @e7` or `click @e3`. See [`crate::actions`].
    pub actions: Vec<ElementRef>,
    /// Pre-fetched mini-trees for outgoing links — populated only when
    /// the page was opened via [`FetchEngine::open_with_explore`] with
    /// `depth > 0`. Always empty for plain [`EngineApi::open`]. See
    /// [`crate::explore`].
    pub linked_pages: Vec<LinkedPage>,
    /// Inline-JSON `<script type="application/json">` blobs — the
    /// hydration payloads SSR frameworks (Next.js `__NEXT_DATA__`,
    /// Apple `__ACGH_DATA__`, Nuxt `__NUXT_DATA__`, Astro, generic
    /// Remix) embed for client-side rendering. On "server-rendered SPA"
    /// pages where the visible DOM is sparse, this is where the actual
    /// content lives. See [`crate::inline_data`].
    pub inline_data: std::collections::BTreeMap<String, serde_json::Value>,
    /// JSON-shaped payloads found in `data-*` element attributes —
    /// the older-React / Vue.js / Stimulus / Alpine.js / vanilla
    /// widget pattern of stashing component props directly on
    /// elements. Keyed by attribute name (with the `data-` prefix);
    /// values are document-ordered lists of (tag, JSON) records.
    /// See [`crate::data_attrs`].
    pub data_attrs: std::collections::BTreeMap<String, Vec<DataAttrValue>>,
}

impl Page for FetchPage {
    fn url(&self) -> &Url {
        &self.url
    }

    async fn text(&self) -> HesoResult<String> {
        Ok(self.body_text.clone())
    }
}

// ============================================================================
// EngineApi impl
// ============================================================================

impl EngineApi for FetchEngine {
    type Page = FetchPage;

    /// Trait-shaped entry — no exploration. For link-graph cartography,
    /// call [`FetchEngine::open_with_explore`] directly.
    async fn open(&self, url: &Url) -> HesoResult<Self::Page> {
        self.open_static(url).await
    }
}

// ============================================================================
// Text extraction
// ============================================================================

/// Parse `html` and return the visible body text. Convenience wrapper
/// around [`extract_visible_text_from_doc`] for callers that hold a
/// raw HTML string (e.g. the post-mutation snapshot serialized out of
/// a [`heso_engine_js::JsSession::document_html`]).
///
/// `<script>`, `<style>`, `<noscript>`, and `<template>` content is
/// dropped; whitespace is normalized (runs collapse to single spaces).
pub fn extract_visible_text(html: &str) -> String {
    extract_visible_text_from_doc(&Html::parse_document(html))
}

/// Walk an already-parsed document and return the visible body text, with
/// `<script>`, `<style>`, `<noscript>`, and `<template>` content skipped.
/// Whitespace is normalized: runs of whitespace collapse to single spaces.
fn extract_visible_text_from_doc(doc: &Html) -> String {
    let mut out = String::new();
    walk(doc.root_element(), &mut out);
    // Same normalisation as `tree::collapse_ws`, single allocation.
    tree::collapse_ws(&out)
}

/// Recursive DOM walker — appends text from each visible descendant text
/// node, skipping non-visible subtrees by tag name.
fn walk(elem: ScraperElementRef<'_>, out: &mut String) {
    let tag = elem.value().name();
    if matches!(tag, "script" | "style" | "noscript" | "template") {
        return;
    }
    for child in elem.children() {
        match child.value() {
            Node::Text(t) => {
                out.push_str(t);
                out.push(' ');
            }
            Node::Element(_) => {
                if let Some(child_ref) = ScraperElementRef::wrap(child) {
                    walk(child_ref, out);
                }
            }
            _ => {}
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_visible_text_and_skips_scripts() {
        let html = r#"
        <!doctype html>
        <html><head>
          <title>X</title>
          <style>body { color: red }</style>
          <script>console.log('hi')</script>
        </head><body>
          <h1>Hello</h1>
          <p>World <span>of agents</span>.</p>
          <noscript>fallback</noscript>
          <script>var x = 1</script>
        </body></html>
        "#;
        let text = extract_visible_text(html);
        assert!(text.contains("Hello"), "got: {text}");
        assert!(text.contains("World"), "got: {text}");
        assert!(text.contains("of agents"), "got: {text}");
        assert!(!text.contains("console.log"), "script leaked: {text}");
        assert!(!text.contains("color: red"), "style leaked: {text}");
        assert!(!text.contains("fallback"), "noscript leaked: {text}");
    }

    #[test]
    fn whitespace_is_normalized() {
        let html = "<html><body>  a  \t b\n\n c  </body></html>";
        assert_eq!(extract_visible_text(html), "a b c");
    }

    #[test]
    fn fetch_engine_constructs_cleanly() {
        // Just verify the default builder works in tests — no network call.
        let _engine = FetchEngine::new().expect("default engine builds");
    }

    /// Live network test, runs by default — example.com is a stable
    /// hostname; if this is offline you have bigger problems.
    #[tokio::test]
    async fn opens_example_com_over_real_http() {
        let engine = FetchEngine::new().expect("engine builds");
        let url = Url::parse("https://example.com/").unwrap();
        let page = engine.open(&url).await.expect("fetch succeeded");
        assert_eq!(page.url().host_str(), Some("example.com"));
        let text = page.text().await.unwrap();
        assert!(
            text.contains("Example Domain"),
            "expected 'Example Domain', got {} chars: {}...",
            text.len(),
            &text[..text.len().min(100)]
        );
    }

    /// The same live fetch also produces a navigable tree: example.com has
    /// one `<h1>` so `ls /` should return exactly one row.
    #[tokio::test]
    async fn opens_example_com_and_builds_tree() {
        let engine = FetchEngine::new().expect("engine builds");
        let url = Url::parse("https://example.com/").unwrap();
        let page = engine.open(&url).await.expect("fetch succeeded");
        assert_eq!(page.tree.title, "Example Domain");
        let rows = page.tree.ls("/").expect("ls / works");
        assert!(
            rows.iter().any(|r| r.slug == "example-domain"),
            "expected an /example-domain row, got: {:?}",
            rows.iter().map(|r| &r.slug).collect::<Vec<_>>()
        );
    }
}
