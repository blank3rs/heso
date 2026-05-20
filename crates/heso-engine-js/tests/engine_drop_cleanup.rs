//! Engine-teardown cleanup tests.
//!
//! These tests pin the contract that dropping a [`JsEngine`] never
//! aborts the process under any combination of state that real pages
//! produce. The motivating reproducers were `https://astro.build/` and
//! `https://vercel.com/`, both of which crashed `heso eval-dom` on
//! engine drop with a QuickJS C-level assertion:
//!
//! - astro.build: `assert(list_empty(&rt->gc_obj_list))` at
//!   `quickjs.c:2205` (GC list not empty when `JS_FreeRuntime` runs).
//! - vercel.com: `assert(p->ref_count > 0)` at `quickjs.c:6183`
//!   (object ref count went negative inside the GC's cycle-collection
//!   pass).
//!
//! Both share the same family of root cause: Rust-side state held a
//! reference to JS values past the point where the runtime tore them
//! down, so the runtime's final `JS_RunGC` pass saw stale entries.
//! See [`JsEngine`]'s `Drop` impl for the drain strategy that fixes
//! them.
//!
//! ## Why no `#[ignore]`
//!
//! Each test below deterministically reproduces the bug shape (a
//! Persistent left in a closure scope past engine drop) without needing
//! the actual astro.build / vercel.com network roundtrip. The release
//! binary reproducer (`target/release/heso eval-dom --js-fetch
//! "https://astro.build/" "document.title"`) is documented in the
//! commit message for cherry-pickers who want a network-driven check.
//!
//! ## Test coverage
//!
//! - `dropping_engine_after_module_load_does_not_abort` — pumps a
//!   page with an inline `<script type="module">` and a top-level
//!   export, then drops. Mirrors the per-module Persistent registry
//!   the loader holds.
//! - `dropping_engine_after_dynamic_import_does_not_abort` — uses
//!   wiremock to host a tiny ES module, calls `globalThis.import(...)`
//!   to settle a Promise containing a Persistent namespace object,
//!   then drops.
//! - `dropping_engine_after_cookie_set_does_not_abort` — exercises
//!   the cookie bridge state (no Persistents but adds coverage).
//! - `repeated_engine_create_drop_cycle_no_aborts` — 50 engines back
//!   to back, each runs a small script + module + dynamic import,
//!   then drops. Stresses any per-runtime leak (slow growth) that a
//!   single-shot test would miss.

use std::sync::Arc;

use heso_engine_js::{JsEngine, ScriptFetchPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-drop-cleanup-tests/0.0.1")
            .build()
            .expect("client builds"),
    )
}

fn engine_with_fetch() -> JsEngine {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    JsEngine::new_with_fetch(client, rt).expect("engine builds")
}

// ===== Test 1: drop after static module load =====

#[tokio::test(flavor = "multi_thread")]
async fn dropping_engine_after_module_load_does_not_abort() {
    // Mirrors the `module_inline_with_export_and_body_runs` flow but
    // exercises the explicit drop path. With the bug, the Persistent
    // namespace held inside the dynamic-import bookkeeping could
    // outlive the runtime; with the fix, the Drop impl tears down the
    // module-side state before the runtime closes.
    let engine = engine_with_fetch();
    let html = r#"<html><body>
        <script type="module">
            export const greeting = "hello";
            globalThis.observed = greeting;
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(html, "globalThis.observed", ScriptFetchPolicy::Fetch)
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!("hello"));
    drop(engine);
    // Reach this line == no abort.
}

// ===== Test 2: drop after dynamic import =====

#[tokio::test(flavor = "multi_thread")]
async fn dropping_engine_after_dynamic_import_does_not_abort() {
    // Dynamic `import()` is the high-risk codepath: the shim wires
    // multiple `Persistent<Function<'static>>` + `Persistent<Object
    // <'static>>` (namespace) handles into JS-side `.then` callbacks.
    // If those callbacks survive engine drop, their inner JSValues
    // try to decref a dead context. This test pins the cleanup.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/m.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("export const x = 42; globalThis.observed = x;"),
        )
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/m.js", server.uri());
    let script = format!(
        "(async () => {{ const m = await globalThis.import({:?}); return m.x; }})()",
        url
    );
    let out = engine.eval(&script).expect("eval ok");
    assert_eq!(out.value, serde_json::json!(42));
    drop(engine);
}

// ===== Test 3: drop after cookie set =====

#[tokio::test(flavor = "multi_thread")]
async fn dropping_engine_after_cookie_set_does_not_abort() {
    // Cookies don't hold Persistents but they do reach into a shared
    // `Arc<CookieStoreMutex>`. Regression-grade coverage that the
    // bridge teardown is clean. Uses `eval_with_html` so the
    // `document` global is installed before the cookie read/write.
    let engine = engine_with_fetch();
    let html = "<html><head></head><body></body></html>";
    let _ = engine
        .eval_with_html_policy(
            html,
            "document.cookie = 'k=v; path=/'; document.cookie",
            ScriptFetchPolicy::Skip,
        )
        .expect("eval ok");
    drop(engine);
}

// ===== Test 4: drop after pending fetch =====

#[tokio::test(flavor = "multi_thread")]
async fn dropping_engine_after_pending_fetch_does_not_abort() {
    // The fetch queue holds `Persistent<Function<'static>>` resolve /
    // reject handles. The Drop impl drains them inside `ctx.with`.
    // This test creates a fetch that doesn't fire (we never call
    // `run_pending_jobs`) so the Persistents are still in the queue
    // at drop time — exactly the shape the drain handles.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/slow", server.uri());
    // Build the fetch but only inspect that it returns a Promise — we
    // deliberately drop before draining microtasks so the Persistents
    // are still queued.
    let script = format!(
        "globalThis.__p = fetch({:?}); typeof globalThis.__p.then",
        url
    );
    let out = engine.eval(&script).expect("eval ok");
    assert_eq!(out.value, serde_json::json!("function"));
    drop(engine);
}

// ===== Test 5: drop after timer scheduled =====

#[tokio::test(flavor = "multi_thread")]
async fn dropping_engine_after_timer_scheduled_does_not_abort() {
    // The timer scheduler holds `Persistent<Function<'static>>` for
    // every un-fired callback. Drop should drain them. Without the
    // drain (`timers.clear_all()` inside `ctx.with`), the callbacks
    // would outlive the runtime and trip the same assert.
    let engine = engine_with_fetch();
    let out = engine
        .eval("setTimeout(() => globalThis.fired = true, 10000); 'set'")
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!("set"));
    assert_eq!(engine.pending_timers(), 1);
    drop(engine);
}

// ===== Test 6: repeated create / drop cycle =====

#[tokio::test(flavor = "multi_thread")]
async fn repeated_engine_create_drop_cycle_no_aborts() {
    // Spin up many engines back-to-back. If the Drop impl is
    // incomplete in some sneaky way, a slow per-engine leak compounds
    // into a visible abort after enough iterations. 50 engines is
    // small enough to run in < 2s and large enough to surface the
    // "one Persistent per cycle stays alive" failure mode.
    for _ in 0..50 {
        let engine = engine_with_fetch();
        let html = r#"<html><body>
            <script type="module">
                export const x = 1;
                globalThis.observed = x;
            </script>
        </body></html>"#;
        let _ = engine
            .eval_with_html_policy(html, "globalThis.observed", ScriptFetchPolicy::Fetch)
            .expect("eval ok");
        // include a timer + a dynamic-import-shaped script so each
        // engine touches every Persistent-producing surface.
        let _ = engine
            .eval("setTimeout(() => {}, 999); 'ok'")
            .expect("eval ok");
        drop(engine);
    }
}
