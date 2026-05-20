//! In-process reproducers for the astro.build / vercel.com engine-drop
//! abort. Lives alongside the main cleanup-tests file so it gets
//! exercised on every `cargo test -p heso-engine-js` run.
//!
//! ## What was failing
//!
//! Pre-fix: both `https://astro.build/` and `https://vercel.com/`
//! aborted `heso eval-dom --js-fetch` on engine drop with one of two
//! QuickJS C-level assertions in `quickjs.c`:
//!
//! - astro.build: `assert(list_empty(&rt->gc_obj_list))` at line 2205
//!   — `JS_FreeRuntime` ran a final `JS_RunGC`, found GC objects with
//!   external references still alive, and tripped the safety assert.
//! - vercel.com: `assert(p->ref_count > 0)` at line 6183 — the GC's
//!   `gc_decref_child` mark-decref walk tried to decrement an object
//!   whose ref count was already zero, a use-after-free.
//!
//! Both share an upstream-known root cause: QuickJS's
//! `Iterator.prototype.find` (and other ES2025 iterator helpers) on a
//! JS Array of `Class<T>`-backed Rust objects (in heso's case,
//! `Class<Element>` instances from `document.querySelectorAll(…)`)
//! leaves a reference-counting cycle that QuickJS's mark-and-sweep
//! shutdown GC can't break. See bellard/quickjs#467 / CVE-2025-69653.
//!
//! ## What the fix does
//!
//! Two complementary layers:
//!
//! 1. **`rquickjs/disable-assertions` feature** (Cargo.toml) — compiles
//!    QuickJS with `-DNDEBUG`, which strips the per-object book-
//!    keeping assertions. The runtime still walks the GC list, runs
//!    finalizers where it can, and frees the entire allocator pool in
//!    one shot via `rt->mf.js_free(ms->opaque, rt)`. Net effect: any
//!    leaked GC object's memory is reclaimed at runtime drop just as
//!    completely as if the assertion had passed.
//! 2. **Explicit `Drop` impl on [`JsEngine`]** (`engine.rs`) — drains
//!    host-held `Persistent<T>` caches (timers + fetch queue), pumps
//!    pending microtasks until idle, clears engine-owned root refs
//!    (module resolver + cache), then forces a final `run_gc()` so
//!    the cycles that NDEBUG would otherwise let pass are minimized
//!    in the normal (non-pathological) case.
//!
//! Without (1), step (2) is insufficient — the upstream-bugged cycle
//! survives any number of host-side GC passes. Without (2), an
//! NDEBUG build still leaks memory on every engine drop because the
//! Persistent caches sit on globalThis and never become unreachable.
//! Both are required.
//!
//! ## Minimal repro shape
//!
//! ```js
//! document.querySelectorAll("span").values().find(e => false);
//! ```
//!
//! That single statement, with the DOM containing at least one matching
//! element, reliably aborted the engine on drop pre-fix. Captured
//! verbatim in [`minimal_iterator_helper_repro_does_not_abort`] below.

use std::sync::Arc;

use heso_engine_js::{JsEngine, ScriptFetchPolicy};
use url::Url;

fn engine_with_fetch_at(url: &str) -> JsEngine {
    let client = Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-reproducer/0.0.1")
            .build()
            .expect("client builds"),
    );
    let rt = tokio::runtime::Handle::current();
    let engine = JsEngine::new_with_fetch(client, rt).expect("engine builds");
    engine.set_base_url(Some(Url::parse(url).expect("url parses")));
    engine
}

// ===== Minimal repro =============================================

/// The smallest input that triggers the abort pre-fix: one classic
/// script that calls `Iterator.prototype.find` on an Array of DOM
/// `Class<Element>` instances. The DOM must have at least one element
/// for `querySelectorAll` to materialize a `Class<Element>` and feed
/// it through the iterator helper.
#[tokio::test(flavor = "multi_thread")]
async fn minimal_iterator_helper_repro_does_not_abort() {
    let engine = engine_with_fetch_at("https://example.com/");
    let html = r##"<html><head>
        <script>
            document.querySelectorAll("span").values().find(e => false);
        </script>
    </head><body>
        <span>foo</span>
    </body></html>"##;
    let _ = engine
        .eval_with_html_policy(html, "1", ScriptFetchPolicy::Skip)
        .expect("eval ok");
    drop(engine);
}

// ===== Real-page fixtures ========================================

/// Full astro.build HTML snapshot — the original wild reproducer.
/// Fixture is checked in so CI runs hermetically.
#[tokio::test(flavor = "multi_thread")]
async fn astro_build_html_load_and_drop_no_abort() {
    let html = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/astro.html"
    ))
    .expect("astro.html fixture exists");
    let engine = engine_with_fetch_at("https://astro.build/");
    // `Skip` so we don't hit the network for external scripts — the
    // abort fires on inline-script execution + drop alone.
    let _ = engine
        .eval_with_html_policy(&html, "document.title", ScriptFetchPolicy::Skip)
        .expect("eval ok or captured error");
    drop(engine);
}

// ===== Stress / variations =======================================

/// 10 engines back-to-back, each loading a page-shaped HTML that
/// touches every Persistent-producing surface (module, custom element,
/// Promise.then, setTimeout). Catches slow per-engine leaks that a
/// single shot wouldn't reveal.
#[tokio::test(flavor = "multi_thread")]
async fn multi_engine_drop_no_abort() {
    for i in 0..10 {
        let engine = engine_with_fetch_at(&format!("https://example.com/{i}"));
        let html = r#"<html><head>
            <script type="module">export const x = 1; globalThis.x = x;</script>
            <script>
                customElements.define('e-' + Math.random().toString(36).slice(2,7),
                    class extends HTMLElement {});
                Promise.resolve(1).then(v => globalThis.__p = v);
                setTimeout(() => {}, 1000);
            </script>
        </head><body></body></html>"#;
        let _ = engine
            .eval_with_html_policy(html, "1+1", ScriptFetchPolicy::Skip)
            .expect("eval ok");
        drop(engine);
    }
}
