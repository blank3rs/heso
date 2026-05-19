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
use std::sync::Arc;

use rquickjs::loader::{Loader, Resolver};
use rquickjs::module::Declared;
use rquickjs::{Ctx, Error, Module};

use url::Url;

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
/// [`rquickjs::loader::Resolver`] by joining relative specifiers
/// against the importing module's URL (`base`) via
/// [`url::Url::join`], so the spec's "resolve a module specifier"
/// step (§8.1.3.5) lines up with QuickJS's own resolver protocol.
///
/// Bare specifiers (those that don't start with `./`, `../`, `/`,
/// or contain `://`) are returned unchanged. That's the M-A "do
/// the relative-import path; leave bare specifiers for M-B's
/// import map" contract — M-B can wrap this resolver and intercept
/// the bare-specifier case before delegating to us.
///
/// Errors only when both `base` and `name` are unparseable as URLs
/// — in practice that only happens if the engine never set its
/// page URL and the script attempted a relative import (which can't
/// resolve anyway). The returned [`Error::new_loading`] propagates
/// back through `Module::evaluate` so callers see a clear "loader
/// rejected this specifier" exception.
#[derive(Clone, Default)]
pub struct HttpResolver {
    // Resolver carries no state of its own today — `ModuleCache`
    // is the load-bearing handle. We keep the type a struct (not a
    // unit) so M-B can attach an import-map filter without breaking
    // the Resolver impl's `&mut self` shape.
    _marker: (),
}

impl HttpResolver {
    /// Build a fresh resolver. Stateless today; reserve the
    /// constructor so M-B can attach an import-map field later
    /// without breaking callers.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Resolver for HttpResolver {
    fn resolve(&mut self, _ctx: &Ctx<'_>, base: &str, name: &str) -> Result<String, Error> {
        // Already-absolute URL — pass through verbatim. Covers the
        // top-level `<script type="module" src="https://...">`
        // case (the engine resolves the src against the page URL
        // before calling Module::evaluate; this branch fires when
        // the user passes an absolute URL directly).
        if name.contains("://") {
            return Ok(name.to_owned());
        }

        // Bare-specifier short-circuit. Per WHATWG HTML §8.1.3.5
        // "Resolve a module specifier", a specifier that does not
        // start with `./`, `../`, or `/` is a *bare* specifier and
        // can only be resolved via an import map. We don't have
        // one yet (that's M-B). Return the bare name unchanged so
        // the loader can surface a "module not found" rather than
        // silently mapping `"lodash"` to `<base>/lodash`. M-B will
        // layer on top by intercepting this case before delegating.
        if !name.starts_with("./") && !name.starts_with("../") && !name.starts_with('/') {
            return Ok(name.to_owned());
        }

        // Relative / root-relative specifier — join against base
        // via [`url::Url::join`]. If `base` parses, this handles
        // both `./dep.js` and `/dep.js` per the spec. If the base
        // doesn't parse (no page URL was set), fall back to
        // returning the specifier as-is; the loader will then fail
        // with a clear error since there's no resolvable URL.
        if let Ok(base_url) = Url::parse(base) {
            if let Ok(joined) = base_url.join(name) {
                return Ok(joined.to_string());
            }
        }

        Ok(name.to_owned())
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
    /// store it in the cache, and return the body. Helper used by
    /// both [`Self::load`] (the cache-miss path) and by the engine's
    /// pre-fetch of an external `<script type="module" src="...">`
    /// (which keeps the first hop on the sync path before handing
    /// the rest to QuickJS).
    fn fetch_and_cache(&self, url: &str) -> Result<String, String> {
        let Some(f) = self.fetch.as_ref() else {
            return Err(format!(
                "heso: cannot fetch module `{url}` — engine has no fetch client (build with JsEngine::new_with_fetch)"
            ));
        };
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
        self.cache.insert(url.to_owned(), body.clone());
        Ok(body)
    }
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
    fn resolver_passes_bare_specifiers_unchanged() {
        // M-B is import-map territory; we keep the resolver honest
        // by leaving the bare name in place so the loader can
        // surface a clear "no source available" error.
        let rt = rquickjs::Runtime::new().unwrap();
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            let mut r = HttpResolver::new();
            assert_eq!(
                r.resolve(&ctx, "https://example.com/a.js", "lodash")
                    .unwrap(),
                "lodash"
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
