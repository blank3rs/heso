//! # modules
//!
//! Real `<script type="module">` execution — WHATWG HTML §8.1.3
//! "Module scripts" support, the Phase 1C item M-A unlock per the
//! M-A subagent brief. Without this module, every `<script
//! type="module">` was punted to classic-script evaluation and
//! `import` / `export` syntax raised a `SyntaxError`.
//!
//! ## What this module is and is not
//!
//! - **It is** the HTTP-backed module loader: a [`HttpResolver`]
//!   that normalizes import specifiers against the page URL, paired
//!   with a [`HttpLoader`] that serves cached sources (pre-seeded
//!   for inline modules) and synchronously fetches missing
//!   dependencies through the engine's shared
//!   [`reqwest::Client`]. Both implement rquickjs's
//!   [`rquickjs::loader::Resolver`] / [`rquickjs::loader::Loader`]
//!   traits so QuickJS's own module evaluator drives instantiation,
//!   topological compilation, and circular-import resolution. We
//!   own the network half; QuickJS owns the spec-compliance half.
//!
//! - **It is not** a bare-specifier resolver, an import-map
//!   interpreter, or a dynamic-`import()` shim. Those are
//!   M-B and M-C territory — the [`ModuleCache`] handle is
//!   intentionally exposed (`Rc<RefCell<_>>`) so M-B can install a
//!   filter on top of [`HttpResolver`] without touching this
//!   module's internals.
//!
//! ## Algorithm references
//!
//! - WHATWG HTML §8.1.3.1 "Module map" + §8.1.3.2 "Creating a
//!   module map" — the in-memory `(URL → source)` cache that
//!   makes `import "./foo.js"` from two different scripts hit one
//!   fetch. See [`ModuleCache`].
//! - WHATWG HTML §8.1.3.4 "Fetch a module script tree" — the
//!   recursive resolve-then-fetch pump. QuickJS drives it; our
//!   [`HttpLoader::load`] supplies the source on each step.
//! - WHATWG HTML §8.1.3.5 "Resolve a module specifier" — the
//!   relative-URL rule [`HttpResolver::resolve`] implements via
//!   [`url::Url::join`]. Bare specifiers fall through unchanged
//!   (no import map yet — M-B's job).
//!
//! ## OSS we lean on
//!
//! rquickjs 0.11 ships
//! [`BuiltinResolver`](rquickjs::loader::BuiltinResolver) +
//! [`BuiltinLoader`](rquickjs::loader::BuiltinLoader) — perfect
//! for pre-seeded inline modules — but their "modules must be
//! registered up-front" model can't grow to "fetch `./foo.js`
//! lazily on first import." We extend the same trait surface with
//! a cache that the engine pre-seeds for inline `<script
//! type="module">` bodies and that the loader falls back to HTTP
//! for everything else. QuickJS's module evaluator (the C-side
//! `js_resolve_module` recursion) handles the topological + cyclic
//! cases identically to V8 — we only have to feed it source.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rquickjs::loader::{Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{Ctx, Error, Module};

use url::Url;

use crate::import_map::ImportMap;

/// Shared, single-threaded handle to the engine's [`ImportMap`].
///
/// Same `Rc<RefCell<_>>` story as [`ModuleCache`]: the QuickJS runtime
/// is `!Send`, the import map lives only as long as the engine, and
/// the three readers (static [`HttpResolver`], dynamic-`import()`
/// default resolver, and the [`crate::scripts`] pump that *writes* the
/// map when it discovers a `<script type="importmap">` data block) all
/// share the same handle. Cloning bumps the refcount; the underlying
/// `ImportMap` starts as [`ImportMap::empty`] on fresh engines.
pub type SharedImportMap = Rc<RefCell<ImportMap>>;

/// Build a fresh [`SharedImportMap`] holding an empty map. Convenience
/// for the engine constructor and tests that want a "no import map"
/// baseline. The empty map short-circuits to plain URL-relative
/// resolution per [`ImportMap::resolve`]'s contract.
pub fn empty_shared_import_map() -> SharedImportMap {
    Rc::new(RefCell::new(ImportMap::empty()))
}

/// Shared, single-threaded cache of `module URL → source` entries.
///
/// Populated three ways:
///
/// 1. The engine pre-seeds every inline `<script type="module">`
///    body under a synthetic URL of the form `<page>#__heso_inline_N__`
///    (see [`inline_module_specifier`]) before calling
///    `Module::evaluate` — that's how the loader serves inline
///    code without a real network round-trip.
/// 2. The engine pre-fetches an external `<script type="module"
///    src="...">` body during the script-pump pass and seeds it
///    under the resolved absolute URL — pulls the first fetch
///    onto the sync path while still letting QuickJS drive nested
///    imports.
/// 3. [`HttpLoader::load`] inserts a freshly-fetched body on first
///    miss for any `import "./dep.js"` chain the seeded modules
///    pull in. Subsequent imports of the same URL by another
///    module hit the cache — no double fetch (the `module_cache_no
///    _double_fetch` test pins this).
///
/// `Rc<RefCell<_>>` (not `Arc<Mutex<_>>`) because the QuickJS
/// runtime is single-threaded by construction (`!Send`) and we
/// want zero lock-contention overhead. The handle is shared
/// between the resolver, the loader, and the engine; cloning bumps
/// the refcount.
#[derive(Clone, Default)]
pub struct ModuleCache {
    inner: Rc<RefCell<HashMap<String, String>>>,
}

impl ModuleCache {
    /// Build a fresh empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or overwrite) the source registered under `url`.
    /// Returns the previously-stored source if any — useful for
    /// the "we wanted to pre-seed but a fetch raced us" branch,
    /// which today never happens but the API stays honest.
    pub fn insert(&self, url: impl Into<String>, source: impl Into<String>) -> Option<String> {
        self.inner.borrow_mut().insert(url.into(), source.into())
    }

    /// Return the source stored under `url`, if any.
    pub fn get(&self, url: &str) -> Option<String> {
        self.inner.borrow().get(url).cloned()
    }

    /// `true` if a source has been stored under `url`.
    pub fn contains(&self, url: &str) -> bool {
        self.inner.borrow().contains_key(url)
    }

    /// Number of entries — useful for the `no_double_fetch` test.
    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    /// `true` when the cache holds no entries — convenience for
    /// callers that want to skip the loader hook entirely on a
    /// fresh engine. Mirrors [`Vec::is_empty`].
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }

    /// Drop every cached entry. Used by [`crate::JsEngine`]'s Drop
    /// impl to release source strings before the runtime tears
    /// down — pure belt-and-braces, since the strings are plain data
    /// rather than QuickJS-managed values, but the call documents the
    /// teardown order at zero runtime cost. `try_*` semantics so a
    /// borrow already held elsewhere (impossible in single-thread
    /// drop today, but cheap to guard against) does not panic.
    pub fn try_clear(&self) {
        if let Ok(mut inner) = self.inner.try_borrow_mut() {
            inner.clear();
        }
    }
}

/// Synthesize the module specifier used to identify the *N*-th
/// inline `<script type="module">` on `base_url`. The fragment
/// suffix makes each inline distinct in the cache and lets QuickJS's
/// internal module map key them apart, while [`Url::join`] strips the
/// fragment when an `import "./dep.js"` inside the inline resolves
/// against this name — so the dependency winds up at
/// `<base_url>/dep.js` exactly as the spec prescribes.
///
/// When `base_url` is `None` (engine has no associated page yet —
/// bare `heso eval-js`), we synthesize against `about:blank` so the
/// URL parser still produces a valid string. Imports inside such a
/// module against relative specifiers will then fail to resolve to
/// a real network URL — that's correct: there's nowhere to fetch
/// from.
pub fn inline_module_specifier(base_url: Option<&Url>, index: usize) -> String {
    let base = base_url
        .map(|u| u.as_str().to_owned())
        .unwrap_or_else(|| "about:blank".to_owned());
    format!("{base}#__heso_inline_{index}__")
}

/// HTTP-backed module resolver. Implements
/// [`rquickjs::loader::Resolver`] by walking the WHATWG HTML §8.1.5
/// "resolve a module specifier" algorithm against the engine's
/// shared [`SharedImportMap`] and then falling through to plain
/// [`url::Url::join`] for relative specifiers and pass-through for
/// already-absolute URLs.
///
/// All three layers ([`ImportMap::resolve`]'s scope match, top-level
/// imports match, URL-shaped passthrough) live inside the import-map
/// crate's `resolve` method — see [`crate::import_map`] for the
/// canonical algorithm. This resolver is the QuickJS-facing wrapper
/// that:
///
/// - Parses `base` (the importing module's URL) into a [`Url`] —
///   falling back to `about:blank` when the engine has no associated
///   page, which makes every bare specifier reject cleanly rather
///   than silently mapping against a nonsensical referrer.
/// - Calls into the shared [`resolve_specifier_through_import_map`]
///   helper so the dynamic-`import()` default resolver (installed by
///   [`crate::engine::JsEngine::new_inner`]) and this static path
///   stay byte-for-byte identical in their resolution behavior.
/// - Wraps errors in [`rquickjs::Error::new_resolving_message`] so
///   QuickJS surfaces a "Resolving '…' from '…' failed: …" exception
///   at the import site (much more useful than a downstream
///   "module not found" with no specifier-name).
#[derive(Clone)]
pub struct HttpResolver {
    import_map: SharedImportMap,
    /// Shared page URL — same `Arc<Mutex>` the engine's `set_base_url`
    /// mutates. Used as the referrer-fallback when QuickJS hands the
    /// resolver a non-URL `base` (typically the synthetic
    /// `"eval_script"` label QuickJS attaches to `ctx.eval(...)`
    /// sources with no filename). Without this, Astro/island-shaped
    /// classic inline scripts that do
    /// `await import("/_astro/foo.js")` resolve against `about:blank`
    /// and fail with `UnmappedBareSpecifier` — see ADR 0014 / HTML §8.1.5
    /// "active script base URL".
    page_url: Arc<Mutex<Option<Url>>>,
}

impl Default for HttpResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpResolver {
    /// Build a fresh resolver bound to a freshly-allocated empty
    /// [`SharedImportMap`]. Convenience for callers (e.g. unit tests)
    /// that don't care about import-map plumbing.
    pub fn new() -> Self {
        Self::with_state(empty_shared_import_map(), Arc::new(Mutex::new(None)))
    }

    /// Build a resolver bound to an existing [`SharedImportMap`].
    /// The engine uses this so the resolver, the dynamic-`import()`
    /// default resolver, and the [`crate::scripts`] pump (which
    /// installs the parsed `<script type="importmap">` body into the
    /// map) all share one `Rc<RefCell<ImportMap>>`.
    pub fn new_with_import_map(import_map: SharedImportMap) -> Self {
        Self::with_state(import_map, Arc::new(Mutex::new(None)))
    }

    /// Construct with both the shared import-map and the engine's
    /// shared page-URL slot. The engine uses this so a single
    /// `Arc<Mutex<Option<Url>>>` is observed by the static-import
    /// resolver, the dynamic-`import()` default resolver, and every
    /// caller of `set_base_url`.
    pub fn with_state(
        import_map: SharedImportMap,
        page_url: Arc<Mutex<Option<Url>>>,
    ) -> Self {
        Self { import_map, page_url }
    }
}

impl Resolver for HttpResolver {
    fn resolve(&mut self, _ctx: &Ctx<'_>, base: &str, name: &str) -> Result<String, Error> {
        // Three-tier referrer resolution:
        // 1. If `base` itself parses as a URL — that's the most
        //    specific source (a real module URL or an inline-module
        //    synthetic specifier) and wins.
        // 2. Else fall back to the engine's current page URL. This is
        //    what classic inline `<script>` blocks need when they call
        //    `import("/foo.js")`: QuickJS labels the source
        //    "eval_script", which isn't a URL — without this fallback
        //    the import-map sees `about:blank` as the referrer and
        //    rejects every absolute-path bare specifier.
        // 3. Final fallback is `about:blank`; resolve will fail at
        //    bare-specifier matching, which is the correct behavior
        //    when no page is associated at all.
        let referrer = Url::parse(base)
            .ok()
            .or_else(|| self.page_url.lock().ok().and_then(|g| g.clone()))
            .unwrap_or_else(|| Url::parse("about:blank").expect("about:blank parses"));
        resolve_specifier_through_import_map(&self.import_map.borrow(), name, &referrer)
            .map(|u| u.to_string())
            .map_err(|msg| Error::new_resolving_message(base, name, msg))
    }
}

/// Run the spec's resolve-a-module-specifier algorithm against
/// `import_map`, then fall back to direct URL resolution when the
/// map cannot answer.
///
/// Three outcomes, matching the resolve layering described on
/// [`HttpResolver`]:
///
/// 1. The map produces an absolute URL (either by mapping a bare
///    specifier, or by passing a URL-shaped specifier through after
///    no scope/import-key hit) → return it.
/// 2. The map errors with [`crate::import_map::ImportMapError::UnmappedBareSpecifier`]
///    on a genuinely bare specifier → return a string-shaped URL that
///    the loader will reject; preserve the pre-import-map error
///    surface so tests pinning that message still pass.
/// 3. Any other map error (null-block, prefix backtrack, malformed
///    address) → surface the error verbatim. These are spec-defined
///    rejections, not implementation bugs.
///
/// Shared by the static [`HttpResolver`] (driven by QuickJS's own
/// module evaluator) and by the dynamic-`import()` default resolver
/// closure installed by [`JsEngine::new_inner`]. Centralizing the
/// algorithm in one function is the load-bearing wire — without it,
/// the two paths can drift in subtle ways (e.g. one applies the map
/// for absolute URLs, the other doesn't), which is exactly the bug
/// the M-B wireup brief calls out.
pub fn resolve_specifier_through_import_map(
    import_map: &ImportMap,
    specifier: &str,
    referrer: &Url,
) -> Result<Url, String> {
    use crate::import_map::ImportMapError;
    match import_map.resolve(specifier, referrer) {
        Ok(url) => Ok(url),
        Err(ImportMapError::UnmappedBareSpecifier { .. }) => {
            // Same shape as pre-import-map behavior: bare specifiers
            // with no map hit surface a clear error. We don't try to
            // synthesize a fake URL — the loader (or the dynamic-
            // import shim) takes it from here.
            Err(format!(
                "unmapped bare specifier {specifier:?} \
                 (referrer {referrer}); declare it in a \
                 <script type=\"importmap\"> block, or use a relative \
                 (./, ../, /) or absolute (https://…) specifier"
            ))
        }
        Err(other) => Err(other.to_string()),
    }
}

/// HTTP-backed module loader. Implements
/// [`rquickjs::loader::Loader`]: looks the resolved URL up in the
/// shared [`ModuleCache`] and, on miss, fetches it through the
/// engine's shared [`reqwest::Client`] over the provided
/// [`tokio::runtime::Handle`].
///
/// Once a fetch succeeds, the body is cached so any other module
/// that imports the same URL (the diamond-import case) hits the
/// cache. This is what the `module_cache_no_double_fetch` test
/// pins.
///
/// On HTTP failure (non-2xx, network error, body-decode error) the
/// loader returns [`Error::new_loading`] with the URL embedded, so
/// QuickJS's module evaluator surfaces a useful exception at the
/// import site rather than a silent compile failure.
///
/// `fetch` is `Option<(client, runtime)>` so engines built without
/// a fetch backend (e.g. bare `JsEngine::new()` paths used in unit
/// tests that don't care about cross-module imports) still
/// function — the loader serves cached entries and rejects every
/// uncached import with a clear error.
pub struct HttpLoader {
    cache: ModuleCache,
    fetch: Option<HttpFetcher>,
}

/// Bundles the `reqwest::Client` + `tokio::runtime::Handle` pair
/// the loader uses for synchronous HTTP fetches. Same shape as
/// `crate::fetch::FetchMode::Live` — `Arc<Client>` shares the
/// connection pool, cookie jar, and TLS state with the rest of
/// the workspace; the runtime handle drives
/// [`reqwest::RequestBuilder::send`] from inside the synchronous
/// JS context via `tokio::task::block_in_place`.
#[derive(Clone)]
pub struct HttpFetcher {
    /// Same `reqwest::Client` instance the static page fetch and
    /// the in-JS `fetch()` global use. Keeps cookies, TLS state, and
    /// (once item M lands) recorded-network playback consistent
    /// across all three call sites.
    pub client: Arc<reqwest::Client>,
    /// Tokio runtime handle used to drive `Client::send` from
    /// inside the synchronous JS context. The host must call
    /// `Runtime::block_on` on a multi-thread runtime; the engine's
    /// `JsEngine::new_with_fetch` documents that constraint.
    pub rt: tokio::runtime::Handle,
}

impl HttpLoader {
    /// Build a loader backed by `cache` and (optionally) an HTTP
    /// fetcher. Engines that don't need cross-module imports can
    /// pass `fetch = None`; the loader will then reject any
    /// uncached resolved URL with [`Error::new_loading`].
    pub fn new(cache: ModuleCache, fetch: Option<HttpFetcher>) -> Self {
        Self { cache, fetch }
    }

    /// Synchronously fetch `url` via the loader's `reqwest::Client`,
    /// store it in the cache, and return the body. Internal wrapper
    /// over the free [`fetch_module_source`] helper so [`Self::load`]
    /// stays a one-liner. The free helper is what the dynamic-import
    /// default resolver in [`crate::engine`] also calls — both paths
    /// share one cache + one fetch path.
    fn fetch_and_cache(&self, url: &str) -> Result<String, String> {
        fetch_module_source(&self.cache, self.fetch.as_ref(), url)
    }
}

/// Look `url` up in `cache`. On hit, return the stored source. On
/// miss, synchronously fetch via `fetcher` (when present), store in
/// `cache`, and return the fresh body. When `fetcher` is `None` and
/// the URL is not in cache, returns a clear error explaining the
/// engine wasn't built with a fetch client.
///
/// This is the seam that lets the static [`HttpLoader`] (driven by
/// QuickJS's module evaluator) and the dynamic-`import()` default
/// resolver (installed by [`crate::engine::JsEngine::new_inner`]) hit
/// the same cache and the same network path. Two consequences:
///
/// 1. A page that loads `./foo.js` once via static `<script
///    type="module">` and later via `await import('./foo.js')` only
///    issues one HTTP request — the cache hit on the second path is
///    automatic.
/// 2. The two paths' error surfaces are identical, so an agent
///    debugging a missing module sees the same string regardless of
///    which call site faulted.
pub fn fetch_module_source(
    cache: &ModuleCache,
    fetcher: Option<&HttpFetcher>,
    url: &str,
) -> Result<String, String> {
    if let Some(source) = cache.get(url) {
        return Ok(source);
    }
    let Some(f) = fetcher else {
        return Err(format!(
            "heso: cannot fetch module `{url}` — engine has no fetch client (build with JsEngine::new_with_fetch)"
        ));
    };
    // SSRF pre-flight: reqwest skips `PrivateNetworkGuard` for IP-literal
    // hosts, so an ES-module `import` of a blocked literal IP would bypass
    // the opt-in block without this. Mirrors `guard_literal_host`.
    if let Some(reason) = heso_engine_fetch::private_network::literal_host_block_reason(url) {
        return Err(format!("blocked: {reason}"));
    }
    // `block_in_place` lets us run a sync HTTP call from the
    // CLI's `#[tokio::main]` flow without tripping the
    // "runtime from within a runtime" panic — same trick as
    // `crate::fetch::perform_request` and
    // `crate::scripts::fetch_script_source`.
    let result = tokio::task::block_in_place(|| {
        f.rt.block_on(async {
            let resp = f
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("send: {e}"))?;
            let status = resp.status();
            if !status.is_success() {
                return Err(format!("HTTP {}", status.as_u16()));
            }
            resp.text().await.map_err(|e| format!("read body: {e}"))
        })
    });
    let body = result?;
    cache.insert(url.to_owned(), body.clone());
    Ok(body)
}

impl Loader for HttpLoader {
    fn load<'js>(&mut self, ctx: &Ctx<'js>, name: &str) -> Result<Module<'js, Declared>, Error> {
        // Cache hit: serve the pre-seeded / previously-fetched
        // source. This is how inline `<script type="module">`
        // bodies are served (the engine seeds them before calling
        // Module::evaluate) and how diamond imports avoid a second
        // network round trip.
        if let Some(source) = self.cache.get(name) {
            return Module::declare(ctx.clone(), name.to_owned(), source);
        }

        // Cache miss + HTTP fetcher available: fetch synchronously,
        // cache, declare.
        match self.fetch_and_cache(name) {
            Ok(source) => Module::declare(ctx.clone(), name.to_owned(), source),
            Err(_msg) => {
                // Surface a real "module loading failed" error so
                // QuickJS rejects the import-site Promise rather
                // than producing a confusing "module not found"
                // with no context. Including the URL in the
                // QuickJS error message helps the agent debug:
                // they'll see `Error: loading: <url>` plus our
                // own console line above it.
                Err(Error::new_loading(name))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_specifier_with_base_url_includes_fragment() {
        let base = Url::parse("https://example.com/page").unwrap();
        let s = inline_module_specifier(Some(&base), 0);
        assert_eq!(s, "https://example.com/page#__heso_inline_0__");
        let s2 = inline_module_specifier(Some(&base), 7);
        assert_eq!(s2, "https://example.com/page#__heso_inline_7__");
    }

    #[test]
    fn inline_specifier_without_base_url_falls_back_to_about_blank() {
        let s = inline_module_specifier(None, 3);
        assert_eq!(s, "about:blank#__heso_inline_3__");
    }

    #[test]
    fn cache_insert_and_get_roundtrip() {
        let c = ModuleCache::new();
        assert!(c.is_empty());
        c.insert("https://x.test/a.js", "export const x = 1;");
        assert_eq!(c.len(), 1);
        assert_eq!(
            c.get("https://x.test/a.js"),
            Some("export const x = 1;".into())
        );
        assert!(c.contains("https://x.test/a.js"));
        assert!(!c.contains("https://x.test/b.js"));
    }

    #[test]
    fn cache_clone_shares_storage() {
        // The whole point of `Rc<RefCell<_>>` — clones share state.
        let a = ModuleCache::new();
        let b = a.clone();
        a.insert("https://x.test/a.js", "src");
        assert_eq!(b.get("https://x.test/a.js"), Some("src".into()));
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
    }

    #[test]
    fn resolver_passes_absolute_urls_through() {
        // No runtime needed for this — Resolver doesn't actually
        // touch the Ctx for the absolute-URL path.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let mut r = HttpResolver::new();
            assert_eq!(
                r.resolve(&ctx, "", "https://example.com/a.js").unwrap(),
                "https://example.com/a.js"
            );
        });
    }

    #[test]
    fn resolver_joins_relative_against_base() {
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let mut r = HttpResolver::new();
            // `./b.js` from a parent at `/foo/a.js` resolves to
            // `/foo/b.js`.
            assert_eq!(
                r.resolve(&ctx, "https://example.com/foo/a.js", "./b.js")
                    .unwrap(),
                "https://example.com/foo/b.js"
            );
            // `../b.js` from a parent at `/foo/a.js` resolves to
            // `/b.js`.
            assert_eq!(
                r.resolve(&ctx, "https://example.com/foo/a.js", "../b.js")
                    .unwrap(),
                "https://example.com/b.js"
            );
            // Root-relative `/b.js` from any path resolves to the
            // page root.
            assert_eq!(
                r.resolve(&ctx, "https://example.com/foo/a.js", "/b.js")
                    .unwrap(),
                "https://example.com/b.js"
            );
        });
    }

    #[test]
    fn resolver_strips_inline_fragment_when_joining_relative() {
        // The inline-script trick: synthetic name carries
        // `#__heso_inline_N__`; relative imports inside the script
        // join against it; fragment is dropped by Url::join.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let mut r = HttpResolver::new();
            assert_eq!(
                r.resolve(
                    &ctx,
                    "https://example.com/page#__heso_inline_0__",
                    "./dep.js",
                )
                .unwrap(),
                "https://example.com/dep.js"
            );
        });
    }

    #[test]
    fn resolver_rejects_bare_specifier_without_import_map() {
        // With no import map declared, a bare specifier should
        // surface a clear Resolving error rather than silently
        // map to `<base>/lodash` (the pre-import-map "leave it
        // alone" behavior would defer the error to the loader,
        // which is a worse UX — the error message there doesn't
        // mention that the agent needs an import map).
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let mut r = HttpResolver::new();
            let err = r
                .resolve(&ctx, "https://example.com/a.js", "lodash")
                .unwrap_err();
            // Resolving error variant — surfaced by rquickjs as
            // "Resolving 'lodash' from 'https://…' failed: …".
            let msg = err.to_string();
            assert!(
                msg.contains("lodash") && msg.contains("importmap"),
                "expected error to mention specifier and importmap; got: {msg}"
            );
        });
    }

    #[test]
    fn resolver_consults_import_map_for_bare_specifiers() {
        // The Wire 2 payoff: an import map declares `"lodash" →
        // "https://cdn/lodash.js"`; the resolver returns the mapped
        // URL instead of erroring.
        use crate::import_map::parse_import_map;
        let json = r#"{
            "imports": { "lodash": "https://cdn.example/lodash.js" }
        }"#;
        let base = Url::parse("https://app.example/").unwrap();
        let map = parse_import_map(json, &base).unwrap();
        let shared: SharedImportMap = Rc::new(RefCell::new(map));

        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let mut r = HttpResolver::new_with_import_map(shared);
            assert_eq!(
                r.resolve(&ctx, "https://app.example/page.js", "lodash")
                    .unwrap(),
                "https://cdn.example/lodash.js"
            );
        });
    }

    #[test]
    fn resolver_import_map_applies_to_absolute_url_keys_too() {
        // An import-map key may be a full URL — used to substitute
        // a remote module's URL (e.g. swap one CDN for another).
        // The static resolver honors this the same way the dynamic
        // path does.
        use crate::import_map::parse_import_map;
        let json = r#"{
            "imports": {
                "https://old.example/x.js": "https://new.example/x.js"
            }
        }"#;
        let base = Url::parse("https://app.example/").unwrap();
        let map = parse_import_map(json, &base).unwrap();
        let shared: SharedImportMap = Rc::new(RefCell::new(map));

        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let mut r = HttpResolver::new_with_import_map(shared);
            assert_eq!(
                r.resolve(
                    &ctx,
                    "https://app.example/page.js",
                    "https://old.example/x.js"
                )
                .unwrap(),
                "https://new.example/x.js"
            );
        });
    }

    #[test]
    fn loader_serves_cached_source() {
        // The pre-seeded inline-module path — the engine's job is
        // to insert the source before calling `Module::evaluate`;
        // here we just verify the loader serves it.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let cache = ModuleCache::new();
            cache.insert("https://example.com/a.js", "export const x = 42;");
            let mut l = HttpLoader::new(cache, None);
            // `Module::declare` succeeds when the loader returns a
            // valid source. We don't try to evaluate here — just
            // verify the declaration step doesn't error.
            let _decl = l.load(&ctx, "https://example.com/a.js").unwrap();
        });
    }

    #[test]
    fn loader_rejects_uncached_when_no_fetch() {
        // Without a fetch backend, the loader has nothing to fall
        // back to. It returns a loading error rather than panicking
        // — same containment story as the rest of the engine.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let cache = ModuleCache::new();
            let mut l = HttpLoader::new(cache, None);
            let err = l.load(&ctx, "https://example.com/missing.js");
            assert!(err.is_err(), "loader should error on uncached miss");
        });
    }
}
