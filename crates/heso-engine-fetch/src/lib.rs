//! # heso-engine-fetch
//!
//! The static path of heso — the agent-native web engine. No Chromium. No
//! Node. One Rust binary. Native HTTP + HTML implementation of
//! [`heso_engine_api::EngineApi`]: `reqwest` + `scraper`, deploys
//! anywhere `heso.exe` runs.
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
pub mod cassette;
pub mod data_attrs;
pub mod explore;
pub mod inline_data;
pub mod metadata;
pub mod plat;
pub mod tree;

pub use actions::{
    extract as extract_actions, filter as filter_actions, resolve as resolve_action,
    resolve_locator, resolve_locator_from_html, ElementRef, LocatorError,
};
pub use cassette::{Cassette, CassetteMiss, Record as CassetteRecord};
pub use data_attrs::{extract as extract_data_attrs, DataAttrValue};
pub use explore::{
    linked_pages_to_json, ExploreOptions, LinkedPage, DEFAULT_LINK_CAP, HARD_LINK_CAP,
};
pub use inline_data::extract as extract_inline_data;
pub use metadata::{extract as extract_metadata, PageMetadata};
pub use plat::{
    canonical_json as plat_canonical_json, hash as plat_hash, open as plat_open,
    seal as plat_seal, verify as plat_verify, OpenOutcome as PlatOpenOutcome,
    SealedPlat, VerifyError as PlatVerifyError, EPHEMERAL_OBJECT_KEYS,
};
pub use tree::{build_tree, HtmlTree, LsRow, PwdRow, TreeError, TreeNode};
// `ResponseCookie` is defined inline below alongside `FetchPage` and is
// already public via its `pub struct` declaration. The CLI uses it to
// render a deterministic per-URL `cookies` field without re-reading the
// shared cookie jar (which is a race surface under `batch read --parallel N`).

use std::collections::HashSet;
use std::sync::Arc;

use heso_core::{Result as HesoResult, Url};
use heso_engine_api::{EngineApi, Page};
use reqwest::Client;
use reqwest_cookie_store::CookieStoreMutex;
use scraper::{ElementRef as ScraperElementRef, Html, Node};
use serde::{Deserialize, Serialize};

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

    /// Replay mode could not find a matching record in the cassette.
    /// Either the cassette was tampered, the page changed since
    /// stamping, or stamp was run without `--record`.
    #[error("{0}")]
    CassetteMiss(#[from] cassette::CassetteMiss),

    /// Cassette response-body base64 could not be decoded — corrupted
    /// cassette. Surfaces as a hard error rather than degrading to
    /// a live fetch, per ADR 0008.
    #[error("cassette decode error: {0}")]
    CassetteDecode(String),
}

impl From<Error> for heso_core::Error {
    fn from(e: Error) -> Self {
        heso_core::Error::Io(std::io::Error::other(e.to_string()))
    }
}

// ============================================================================
// CassetteMode — how the engine handles HTTP requests
// ============================================================================

/// Cassette behavior for a [`FetchEngine`]. Each variant determines
/// whether HTTP requests hit the network and whether they are
/// recorded into / served from a [`Cassette`].
///
/// Cloning a `FetchEngine` clones the `CassetteMode` too, so spawned
/// sub-fetches (the [`explore`] module, the JS engine's `fetch`
/// global) inherit the same recording/replaying behavior as the
/// parent. The recording-side cassette is shared by `Arc<Mutex<…>>`
/// so concurrent recordings from sub-fetches all land in one log.
#[derive(Debug, Clone, Default)]
pub enum CassetteMode {
    /// Live HTTP, no cassette involvement. Default for `heso open`,
    /// `heso read`, the dev-loop. This is the variant
    /// [`FetchEngine::new`] produces.
    #[default]
    Live,
    /// Live HTTP with a sidecar recorder: every request goes to the
    /// wire as in `Live`, and every (request, response) pair is
    /// appended to the shared `Cassette`. Used by `heso stamp` to
    /// produce a cassette inside the resulting plat.
    Recording(Arc<std::sync::Mutex<Cassette>>),
    /// Cassette-only: no network access at all. Every HTTP request
    /// looks up `(method, url, request-body)` in the cassette;
    /// matches return the recorded response, misses return
    /// [`Error::CassetteMiss`] so the caller can surface a clean
    /// error instead of a quiet drift. Used by `heso replay`.
    Replaying(Arc<Cassette>),
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
    /// How HTTP requests are routed — live, recording into a
    /// cassette, or playing back from one. See [`CassetteMode`].
    cassette_mode: CassetteMode,
}

impl FetchEngine {
    /// Construct a new engine with sensible defaults: rustls TLS, gzip +
    /// brotli decoding, HTTP/2, follows up to 20 redirects, identifies as
    /// `heso/<version>`, and a fresh empty cookie jar wired into the
    /// `reqwest::Client` via `cookie_provider`. Cookies persist for the
    /// lifetime of this `FetchEngine` (and any clone — `Arc` semantics).
    pub fn new() -> HesoResult<Self> {
        Self::build(CassetteMode::Live)
    }

    /// Construct a `FetchEngine` whose HTTP traffic is mirrored into
    /// `cassette`. Equivalent to [`Self::new`] but every `GET` (and,
    /// once Phase 2.5 lands, every JS-side `fetch()`) records a
    /// (request, response) pair into the cassette before returning.
    ///
    /// Used by `heso stamp` to produce a plat whose cassette field
    /// can later be replayed byte-identically by `heso replay`.
    pub fn with_recording_cassette(cassette: Arc<std::sync::Mutex<Cassette>>) -> HesoResult<Self> {
        Self::build(CassetteMode::Recording(cassette))
    }

    /// Construct a `FetchEngine` that serves every HTTP request from
    /// `cassette` instead of the network. Misses surface as
    /// [`Error::CassetteMiss`]; the engine never falls through to a
    /// live fetch under Replaying mode (ADR 0008 §"Network requests"
    /// — "Hash mismatch on a request that wasn't recorded → error,
    /// not a real fetch").
    ///
    /// The `reqwest::Client` is still built (the cookie jar lives
    /// inside it and the JS engine still expects one). It's just not
    /// reached for HTTP under Replaying mode.
    pub fn with_replaying_cassette(cassette: Arc<Cassette>) -> HesoResult<Self> {
        Self::build(CassetteMode::Replaying(cassette))
    }

    /// Internal: the constructor body shared by [`Self::new`],
    /// [`Self::with_recording_cassette`], and
    /// [`Self::with_replaying_cassette`]. Centralizes the client +
    /// cookie-jar build so the three entry points stay byte-identical
    /// on the live-HTTP side.
    fn build(cassette_mode: CassetteMode) -> HesoResult<Self> {
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
        Ok(Self {
            client,
            cookie_jar,
            cassette_mode,
        })
    }

    /// Read the cassette mode the engine is operating in. Used by the
    /// CLI to introspect Recording mode for the post-run cassette
    /// extraction.
    pub fn cassette_mode(&self) -> &CassetteMode {
        &self.cassette_mode
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
        input: &str,
        opts: ExploreOptions,
    ) -> HesoResult<FetchPage> {
        let mut page = self.open_static(input).await?;
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
        let raw = self.do_http_get(url).await?;
        let html_text = String::from_utf8_lossy(&raw.body_bytes).into_owned();
        Ok((raw.final_url, html_text))
    }

    /// Internal: the original static `open` path, factored out so
    /// [`FetchEngine::open_with_explore`] can compose it without
    /// re-dispatching through the trait (which lacks an options
    /// parameter).
    async fn open_static(&self, input: &str) -> HesoResult<FetchPage> {
        let parsed = Url::parse(input).map_err(Error::from)?;
        let raw = self.do_http_get(&parsed).await?;
        let html_text = String::from_utf8_lossy(&raw.body_bytes).into_owned();
        Ok(FetchPage::from_html(
            input.to_owned(),
            raw.final_url,
            raw.http_status,
            raw.response_cookies,
            html_text,
        ))
    }

    /// Centralized HTTP GET that all the engine's static-fetch paths
    /// (`open_static`, `fetch_text`, and `explore` via `client_ref`)
    /// route through when they touch the network. Dispatches on
    /// [`Self::cassette_mode`]:
    ///
    /// - [`CassetteMode::Live`]: hit the network as before.
    /// - [`CassetteMode::Recording`]: hit the network, then append a
    ///   record to the shared cassette.
    /// - [`CassetteMode::Replaying`]: skip the network entirely, look
    ///   up the cassette, return the recorded response. Misses
    ///   surface as [`Error::CassetteMiss`].
    async fn do_http_get(&self, url: &Url) -> HesoResult<HttpFetchResult> {
        match &self.cassette_mode {
            CassetteMode::Live => self.live_get(url).await,
            CassetteMode::Recording(cassette) => {
                let raw = self.live_get(url).await?;
                let headers: Vec<(String, String)> = raw
                    .response_headers
                    .iter()
                    .cloned()
                    .collect();
                // Lock briefly — no await held while locked.
                cassette
                    .lock()
                    .expect("cassette mutex poisoned")
                    .record(
                        "GET",
                        url.as_str(),
                        raw.final_url.as_str(),
                        &[],
                        raw.http_status,
                        headers,
                        &raw.body_bytes,
                    );
                Ok(raw)
            }
            CassetteMode::Replaying(cassette) => {
                let record = cassette.lookup("GET", url.as_str(), &[]).ok_or_else(|| {
                    Error::CassetteMiss(cassette::CassetteMiss {
                        method: "GET".to_owned(),
                        url: url.as_str().to_owned(),
                        recorded_count: cassette.len(),
                    })
                })?;
                let body_bytes = Cassette::decode_response_body(record)
                    .map_err(|e| Error::CassetteDecode(e.to_string()))?;
                let final_url = Url::parse(&record.final_url).map_err(Error::from)?;
                Ok(HttpFetchResult {
                    final_url,
                    http_status: record.status,
                    response_cookies: Vec::new(),
                    response_headers: record.response_headers.clone(),
                    body_bytes,
                })
            }
        }
    }

    /// Hit the wire via reqwest. Used by [`Self::do_http_get`]'s Live
    /// and Recording branches; never reached under Replaying.
    async fn live_get(&self, url: &Url) -> HesoResult<HttpFetchResult> {
        let response = self
            .client
            .get(url.as_str())
            .send()
            .await
            .map_err(Error::from)?;
        let final_url_str = response.url().as_str().to_owned();
        let final_url = Url::parse(&final_url_str).map_err(Error::from)?;
        let http_status = response.status().as_u16();
        let response_cookies = snapshot_response_cookies(&response);
        // Capture headers BEFORE consuming the response body — reqwest's
        // `bytes()` consumes the response, after which `.headers()` is gone.
        let response_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|s| (k.as_str().to_owned(), s.to_owned()))
            })
            .collect();
        let body_bytes = response.bytes().await.map_err(Error::from)?.to_vec();
        Ok(HttpFetchResult {
            final_url,
            http_status,
            response_cookies,
            response_headers,
            body_bytes,
        })
    }
}

/// Raw HTTP response data captured by [`FetchEngine::do_http_get`].
/// Internal — `open_static`/`fetch_text` consume it and project
/// down to the [`FetchPage`] / `(Url, String)` shapes their callers
/// expect.
struct HttpFetchResult {
    final_url: Url,
    http_status: u16,
    response_cookies: Vec<ResponseCookie>,
    /// Response headers as `(name, value)` pairs. Used by the
    /// Recording branch to feed the cassette; not consumed by the
    /// Live branch yet (today's callers only need `response_cookies`
    /// for the `cookies` projection).
    #[allow(dead_code)]
    response_headers: Vec<(String, String)>,
    body_bytes: Vec<u8>,
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
// ResponseCookie
// ============================================================================

/// A single `Set-Cookie` header value the server returned with
/// **this** response, copied into an owned form so it survives past
/// the response body's drop point.
///
/// Captured eagerly in [`FetchEngine::open_static`] from
/// [`reqwest::Response::cookies`] so callers that want to know "what
/// cookies did *this* response set?" get a deterministic, per-task
/// answer — independent of any subsequent writes other tasks make to
/// the shared `Arc<CookieStoreMutex>`.
///
/// Trade-off: `Response::cookies()` only sees the **final** response's
/// `Set-Cookie` headers. Intermediate redirect responses' cookies
/// are written to the shared jar by reqwest but don't appear in this
/// snapshot. For the agent-facing `--include cookies` shape this is
/// the right call: the LLM wants "what cookies did the response I
/// just fetched ask me to store" rather than the full effective
/// cookie set spanning the redirect history.
///
/// The shape is intentionally `{name, value, domain, path, host_only,
/// http_only, secure}`:
///
/// - `domain` is `None` when the server's `Set-Cookie` had no `Domain=`
///   attribute. RFC 6265 §5.3 step 6 calls this the *host-only* case —
///   the cookie's effective scope is the request URL's host, not any
///   sub-domains.
/// - `host_only` is the boolean that lets a caller distinguish "the
///   server sent `Domain=`" (`host_only = false`) from "the server
///   omitted `Domain=`" (`host_only = true`) without ambiguity. Without
///   this boolean, an empty `domain` field looks the same as a missing
///   one.
/// - `http_only` is the `HttpOnly` directive — set by servers that want
///   the cookie hidden from JS `document.cookie`. Heso strips
///   HttpOnly cookies from the agent-facing JSON (matching the WHATWG
///   HTML §6.1 filter `document.cookie` applies in a real browser).
/// - `secure` is the `Secure` directive — only sent over HTTPS.
///
/// Field order matches the JSON shape `collect_cookies` emits — keep
/// them in sync.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseCookie {
    /// Cookie name (`name=value` from the `Set-Cookie` header).
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// `Domain=` attribute value — `None` when the server's `Set-Cookie`
    /// omitted `Domain=` (the "host-only" case, RFC 6265 §5.3 step 6).
    /// `host_only` is the disambiguating flag — see the struct comment.
    pub domain: Option<String>,
    /// `Path=` attribute value — defaults to `/` if the server omitted
    /// it.
    pub path: Option<String>,
    /// `true` iff the server's `Set-Cookie` had **no** `Domain=`
    /// attribute (or the attribute value was empty). RFC 6265 calls
    /// this a "host-only" cookie: the cookie's effective scope is the
    /// request URL's host, not any sub-domains.
    pub host_only: bool,
    /// `HttpOnly` directive — when `true`, the cookie is hidden from JS
    /// `document.cookie`. Heso strips HttpOnly cookies from the
    /// agent-facing JSON.
    pub http_only: bool,
    /// `Secure` directive — when `true`, the cookie only travels over
    /// HTTPS.
    pub secure: bool,
}

/// Snapshot the cookies **this response** set, copying owned data out
/// of the borrowed [`reqwest::cookie::Cookie`] iterator into
/// [`ResponseCookie`]s.
///
/// `response.cookies()` iterates the final response's `Set-Cookie`
/// headers — exactly the cookies the server asked for on **this**
/// fetch. Importantly, it does NOT see `Set-Cookie` headers from
/// intermediate redirect responses (reqwest discards those on the
/// final `Response` object after writing them to the shared jar);
/// callers who need redirect-chain cookies have to consult the jar
/// separately. For the agent-facing `--include cookies` shape this
/// is the correct trade-off — the LLM cares about "what cookies did
/// the response I just fetched ask me to store?", not the full
/// effective cookie set across the redirect history.
///
/// Per RFC 6265 §5.3 step 6, a cookie whose `Set-Cookie` carried no
/// `Domain=` attribute is *host-only* — its effective scope is the
/// request URL's host. `reqwest::cookie::Cookie::domain()` returns
/// `None` for the host-only case; we surface that through
/// [`ResponseCookie::host_only`] so the agent-facing JSON can render
/// `host_only: true` (and substitute the request URL's host for
/// `domain`) instead of the empty-string sentinel the previous code
/// produced.
fn snapshot_response_cookies(response: &reqwest::Response) -> Vec<ResponseCookie> {
    response
        .cookies()
        .map(|c| {
            let domain = c.domain().map(str::to_owned);
            let host_only = domain.as_deref().is_none_or(str::is_empty);
            ResponseCookie {
                name: c.name().to_owned(),
                value: c.value().to_owned(),
                domain,
                path: c.path().map(str::to_owned),
                host_only,
                http_only: c.http_only(),
                secure: c.secure(),
            }
        })
        .collect()
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
    /// Verbatim URL string the caller asked the engine to open, before
    /// any parsing or normalization. Two byte-different requests
    /// produce two byte-different `input_url`s, even when both parse
    /// to the same [`Url`] (case-folded host, default-port stripping)
    /// or both follow redirects to the same final [`url`](Self::url).
    /// Load-bearing for plat identity: every plat emitted from a
    /// [`FetchPage`] includes this string in its canonical bytes.
    pub input_url: String,
    url: Url,
    body_text: String,
    /// The raw HTML body of the response, exactly as it came over the
    /// wire (post-decompression). Populated alongside `body_text` and
    /// `actions` so callers that need to hand the same bytes to a JS
    /// engine (for `<script>` execution, DOM mutation, etc.) don't
    /// have to issue a second HTTP round-trip via [`FetchEngine::fetch_text`].
    pub body_html: String,
    /// The HTTP status code of the final response (after redirects).
    /// `200` for a clean fetch, `4xx`/`5xx` when the server returned an
    /// error page. The body is still extracted into `body_text`,
    /// `tree`, `actions`, etc. — heso's contract is "always return the
    /// payload so the agent can decide" — but the status is the honest
    /// signal callers use to distinguish "real empty page" from
    /// "server blocked us with a 403 + interstitial body."
    pub http_status: u16,
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
    /// The action sequence that produced this page, when the page was
    /// minted by replaying a plan (`heso stamp` / `heso replay`).
    /// Always [`None`] for pages produced by a single one-shot
    /// [`FetchEngine::open_with_explore`]. When [`Some`], the value is
    /// the JSON array of canonical actions exactly as it was executed;
    /// [`Self::plat_body_base`] surfaces it as the plat's `"plan"`
    /// field so the resulting plat is replayable.
    pub plan: Option<serde_json::Value>,
    /// Cookies the server set with **this specific response**, captured
    /// eagerly via [`reqwest::Response::cookies`] before any other
    /// concurrent task could land a `Set-Cookie` on the shared jar.
    /// This is the deterministic, race-free counterpart to scanning the
    /// shared `Arc<CookieStoreMutex>` at JSON-serialize time — used by
    /// the CLI's `--include cookies` to emit a per-URL snapshot that
    /// reflects what *this* response asked for, not whatever the jar
    /// happens to contain when the row gets serialized.
    ///
    /// Includes `HttpOnly` cookies; the CLI filters those out at
    /// serialize time to match the WHATWG HTML §6.1 `document.cookie`
    /// visibility rule.
    pub response_cookies: Vec<ResponseCookie>,
}

impl Page for FetchPage {
    fn url(&self) -> &Url {
        &self.url
    }

    async fn text(&self) -> HesoResult<String> {
        Ok(self.body_text.clone())
    }
}

impl FetchPage {
    /// Construct a [`FetchPage`] from an already-fetched HTML string —
    /// the same extraction pipeline `open_static` uses, but without the
    /// network round-trip. Callers supply `input_url` (the caller's
    /// verbatim request) and `final_url` (post-redirect / post-action).
    ///
    /// Used by `open_static` for the network path and by the replay /
    /// stamp verbs to mint a [`FetchPage`] from a post-execution DOM.
    pub fn from_html(input_url: String, final_url: Url, http_status: u16, response_cookies: Vec<ResponseCookie>, html: String) -> Self {
        let doc = Html::parse_document(&html);
        let body_text = extract_visible_text_from_doc(&doc);
        let metadata = metadata::extract(&doc);
        let tree = tree::build_tree_from_doc(&doc, &final_url);
        let actions = actions::extract(&doc);
        let inline_data = inline_data::extract(&doc);
        let data_attrs = data_attrs::extract(&doc);
        FetchPage {
            input_url,
            url: final_url,
            body_text,
            body_html: html,
            http_status,
            response_cookies,
            tree,
            metadata,
            actions,
            linked_pages: Vec::new(),
            inline_data,
            data_attrs,
            plan: None,
        }
    }

    /// Canonical opening shape of a plat body for this page. Always
    /// carries `input_url` (the caller's verbatim request) and `url`
    /// (the parsed, post-redirect URL of the page that served). Two
    /// byte-different `input_url`s produce two byte-different bodies
    /// regardless of how they normalize.
    ///
    /// Callers layer post-hydration fields, console buffers, forms,
    /// cookies, etc. on top before stamping `plat_hash`. This is the
    /// one place `input_url` enters a plat body — the type system
    /// makes it impossible to emit a plat from a [`FetchPage`] without
    /// it.
    pub fn plat_body_base(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "input_url": &self.input_url,
            "url": self.url.as_str(),
            "title": self.tree.title,
            "description": self.tree.description,
            "metadata": self.metadata,
            "tree": self.tree,
            "actions": self.actions,
            "http_status": self.http_status,
        });
        if !self.inline_data.is_empty() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "inline_data".to_owned(),
                    serde_json::to_value(&self.inline_data)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
        }
        if !self.data_attrs.is_empty() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "data_attrs".to_owned(),
                    serde_json::to_value(&self.data_attrs)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
        }
        if !self.linked_pages.is_empty() {
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "linked_pages".to_owned(),
                    linked_pages_to_json(&self.linked_pages),
                );
            }
        }
        if let Some(plan) = &self.plan {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("plan".to_owned(), plan.clone());
            }
        }
        body
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
        self.open_static(url.as_str()).await
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

/// Parse `html` and return the action graph. Convenience wrapper that
/// callers (such as `heso-cli`'s `read --complete` loop) use to
/// re-extract refs from a post-mutation DOM snapshot without having to
/// take `scraper` as a direct dependency.
///
/// Same output as calling [`actions::extract`] on a parsed document.
pub fn extract_actions_from_html(html: &str) -> Vec<ElementRef> {
    actions::extract(&Html::parse_document(html))
}

/// `true` if `html` looks like a Cloudflare / generic anti-bot
/// interstitial page rather than the real content the agent asked for.
pub fn is_bot_challenge(html: &str) -> bool {
    if html.contains("__cf_chl_opt") || html.contains("cf_chl_jschl_tk__") {
        return true;
    }
    if let Some(idx) = html.find("<title>") {
        let after = &html[idx + "<title>".len()..];
        let probe_end = after.len().min(64);
        let probe = &after[..probe_end];
        let lowered: String = probe.chars().map(|c| c.to_ascii_lowercase()).collect();
        if lowered.starts_with("just a moment") {
            return true;
        }
    }
    false
}

/// Map an HTTP status + body to an optional `partial_reason` token.
/// `None` means "clean 2xx"; `Some(...)` is the failure-envelope token
/// the agent surface uses: `http_403`, `http_5xx`, `bot_challenge`, ...
pub fn partial_reason_for_status(http_status: u16, body_html: &str) -> Option<String> {
    if (200..300).contains(&http_status) {
        if is_bot_challenge(body_html) {
            return Some("bot_challenge".to_owned());
        }
        return None;
    }
    if (400..500).contains(&http_status) {
        return Some(format!("http_{http_status}"));
    }
    if (500..600).contains(&http_status) {
        return Some("http_5xx".to_owned());
    }
    if (300..400).contains(&http_status) {
        return Some(format!("http_{http_status}"));
    }
    if (100..200).contains(&http_status) {
        return Some(format!("http_{http_status}"));
    }
    Some(format!("http_{http_status}"))
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

    /// Build a synthetic [`FetchPage`] without hitting the network.
    /// Two pages from the same `final_url` but different `input_url`
    /// values share every other field by construction.
    fn synthetic_page(input: &str, final_url: &str) -> FetchPage {
        let parsed = Url::parse(final_url).unwrap();
        FetchPage::from_html(input.to_owned(), parsed, 200, Vec::new(), String::new())
    }

    #[test]
    fn plat_body_base_always_carries_input_url() {
        let p = synthetic_page("https://Example.com/", "https://example.com/");
        let body = p.plat_body_base();
        assert_eq!(body["input_url"], "https://Example.com/");
        assert_eq!(body["url"], "https://example.com/");
    }

    #[test]
    fn plan_field_appears_in_plat_body_when_set() {
        let mut p = synthetic_page("https://x/", "https://x/");
        let plan_json = serde_json::json!([
            {"verb": "open", "url": "https://x/"},
            {"verb": "click", "ref": "@e0"},
        ]);
        p.plan = Some(plan_json.clone());
        let body = p.plat_body_base();
        assert_eq!(body["plan"], plan_json);
    }

    #[test]
    fn plan_field_omitted_from_plat_body_when_unset() {
        let p = synthetic_page("https://x/", "https://x/");
        let body = p.plat_body_base();
        assert!(
            body.as_object().map(|o| !o.contains_key("plan")).unwrap_or(false),
            "plan key must be absent when self.plan is None"
        );
    }

    #[test]
    fn plat_hash_changes_when_plan_changes() {
        // A plat that embeds a plan must commit to it in the hash —
        // editing the plan and forgetting to re-stamp must be detectable.
        let mut a = synthetic_page("https://x/", "https://x/");
        let mut b = synthetic_page("https://x/", "https://x/");
        a.plan = Some(serde_json::json!([{"verb": "open", "url": "https://x/"}]));
        b.plan = Some(serde_json::json!([
            {"verb": "open", "url": "https://x/"},
            {"verb": "click", "ref": "@e0"},
        ]));
        assert_ne!(plat::hash(&a.plat_body_base()), plat::hash(&b.plat_body_base()));
    }

    #[test]
    fn different_inputs_same_final_url_produce_different_plat_hashes() {
        // The headline guarantee: byte-different `input_url` ⇒
        // byte-different canonical bytes ⇒ different plat_hash, even
        // when the parsed + post-redirect `url` is identical.
        let variants = [
            "https://Example.com/",
            "https://EXAMPLE.com/",
            "https://example.com:443/",
            "HTTPS://example.com/",
            "https://example.com/",
        ];
        let mut seen = std::collections::HashMap::<String, &str>::new();
        for raw in variants {
            let body = synthetic_page(raw, "https://example.com/").plat_body_base();
            let h = plat::hash(&body);
            if let Some(prev) = seen.insert(h.clone(), raw) {
                panic!("collision: `{prev}` and `{raw}` both hash to {h}");
            }
        }
        assert_eq!(seen.len(), variants.len());
    }
}
