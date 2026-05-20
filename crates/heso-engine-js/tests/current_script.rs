//! Integration tests for `document.currentScript` — the
//! WHATWG HTML §3.1.1 attribute that points at the `<script>` element
//! currently executing.
//!
//! ## Why this exists
//!
//! Modern Next.js sites ship a Turbopack-bundled runtime whose
//! externally-loaded chunks open with:
//!
//! ```js
//! (globalThis.TURBOPACK||(globalThis.TURBOPACK=[])).push([
//!     "object"==typeof document ? document.currentScript : void 0,
//!     <chunkId>, …
//! ]);
//! ```
//!
//! Once the entrypoint chunk runs, the runtime overwrites that array
//! with a `{ push: registerChunk }` proxy. `registerChunk` then reads
//! `registration[0].getAttribute("src")` to know which chunk just
//! executed. When `document.currentScript` is `undefined`, that
//! registration[0] is `void 0`, and the runtime throws
//! `"chunk path empty but not in a worker"` —
//! [`getChunkFromRegistration` in vercel/next.js
//! turbopack/crates/turbopack-ecmascript-runtime/js/src/browser/runtime/base/runtime-base.ts][1].
//! Hydration is dead on every modern Next.js site.
//!
//! [1]: https://github.com/vercel/next.js/blob/canary/turbopack/crates/turbopack-ecmascript-runtime/js/src/browser/runtime/base/runtime-base.ts
//!
//! These tests are the regression harness for the fix in
//! [`crate::scripts::set_current_script`]: each `<script src="...">`
//! executed by the script pump now observes a synthetic
//! `document.currentScript` whose `getAttribute("src")` returns the
//! raw `src` attribute (the same string the browser exposes from a
//! real `HTMLScriptElement`).
//!
//! ## What we cover
//!
//! 1. **Inline classic** scripts see a non-null `document.currentScript`
//!    whose `getAttribute("src")` returns `null` (an inline `<script>`
//!    has no `src`).
//! 2. **External classic** scripts (`<script src="...">`) see a
//!    `document.currentScript` whose `getAttribute("src")` returns the
//!    exact raw attribute string from the HTML.
//! 3. **Modules** (`<script type="module">`) keep
//!    `document.currentScript` `null` — per spec, modules never set it.
//! 4. **After the pump**, `document.currentScript` is `null` again —
//!    user JS run via `eval` after `eval_with_html_policy` sees `null`,
//!    not a stale handle.
//! 5. **Turbopack-shaped runtime** — a minimal port of the throw site
//!    from `runtime-base.ts` (`getChunkFromRegistration`). The mocked
//!    chunk body invokes the same registration pattern Turbopack uses;
//!    the test asserts the throw site does *not* fire.

use std::sync::Arc;

use heso_engine_js::{JsEngine, ScriptFetchPolicy};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a workspace-shaped `reqwest::Client` (same shape as
/// modules_integration.rs / fetch_integration.rs).
fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-current-script-tests/0.0.1")
            .build()
            .expect("client builds"),
    )
}

/// Build a JS engine in live-fetch mode with the current tokio
/// handle — required so external `<script src=...>` references can
/// resolve to the wiremock server.
fn engine_with_fetch() -> JsEngine {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    JsEngine::new_with_fetch(client, rt).expect("engine builds")
}

// ===== Test 1: currentScript on inline classic script =====

#[test]
fn inline_classic_script_sees_non_null_current_script_with_null_src() {
    // Spec: inline `<script>…</script>` makes `document.currentScript`
    // a non-null script element. The element has no `src` attribute,
    // so `getAttribute("src")` returns `null`.
    let engine = JsEngine::new().unwrap();
    let html = r#"<html><body>
        <script>
            // Snapshot what currentScript looks like from inside an
            // inline classic script.
            globalThis.__seenTagName = document.currentScript ? document.currentScript.tagName : null;
            globalThis.__seenSrcAttr = document.currentScript ? document.currentScript.getAttribute('src') : null;
            globalThis.__seenHasSrc = document.currentScript ? document.currentScript.hasAttribute('src') : null;
            globalThis.__seenIsObject = typeof document.currentScript === 'object' && document.currentScript !== null;
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            "[globalThis.__seenTagName, globalThis.__seenSrcAttr, globalThis.__seenHasSrc, globalThis.__seenIsObject]",
            ScriptFetchPolicy::Skip,
        )
        .expect("eval ok");
    assert_eq!(
        out.value,
        serde_json::json!(["SCRIPT", null, false, true]),
        "inline classic script must see a synthetic currentScript with tagName=SCRIPT and getAttribute('src')=null"
    );
}

// ===== Test 2: currentScript clears to null after pump =====

#[test]
fn document_current_script_is_null_after_pump() {
    // Spec: outside an executing classic script body,
    // `document.currentScript` is null. Our pump's exit must clear it.
    let engine = JsEngine::new().unwrap();
    let html = r#"<html><body>
        <script>globalThis.__duringScript = document.currentScript;</script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            // After pump: the user JS arg sees null.
            "[globalThis.__duringScript !== null, document.currentScript]",
            ScriptFetchPolicy::Skip,
        )
        .expect("eval ok");
    assert_eq!(
        out.value[0], true,
        "during execution, currentScript was non-null"
    );
    assert_eq!(
        out.value[1],
        serde_json::Value::Null,
        "after pump, currentScript must be null"
    );
}

// ===== Test 3: external classic sees raw src attribute =====

#[tokio::test(flavor = "multi_thread")]
async fn external_classic_script_sees_raw_src_via_get_attribute() {
    // Spec: external `<script src="/path/chunk.js?v=1">` — the
    // executing script's `getAttribute("src")` returns the literal
    // attribute string, which is what Turbopack chunk-self-detection
    // reads via `chunk.getAttribute("src")`.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chunk.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            globalThis.__chunkTagName = document.currentScript ? document.currentScript.tagName : null;
            globalThis.__chunkSrcAttr = document.currentScript ? document.currentScript.getAttribute('src') : null;
            globalThis.__chunkHasSrc = document.currentScript ? document.currentScript.hasAttribute('src') : null;
            globalThis.__chunkAttrNames = document.currentScript ? document.currentScript.getAttributeNames() : null;
            globalThis.__chunkSrcIdl = document.currentScript ? document.currentScript.src : null;
            "#,
        ))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));

    // We use an absolute URL here so the test is independent of the
    // base-URL join logic — `getAttribute("src")` must return whatever
    // exact string appears in the HTML.
    let chunk_url = format!("{}/chunk.js", server.uri());
    let html = format!(
        r#"<html><body>
            <script src="{src}"></script>
        </body></html>"#,
        src = chunk_url,
    );
    let out = engine
        .eval_with_html_policy(
            &html,
            "[globalThis.__chunkTagName, globalThis.__chunkSrcAttr, globalThis.__chunkHasSrc, globalThis.__chunkAttrNames, globalThis.__chunkSrcIdl]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "SCRIPT");
    assert_eq!(
        out.value[1].as_str().unwrap(),
        chunk_url,
        "getAttribute('src') must return the exact raw attribute string from the HTML"
    );
    assert_eq!(out.value[2], true);
    assert_eq!(out.value[3], serde_json::json!(["src"]));
    assert_eq!(
        out.value[4].as_str().unwrap(),
        chunk_url,
        ".src IDL must return the resolved URL (matches the raw because it was already absolute)"
    );
}

// ===== Test 4: relative src — getAttribute('src') stays raw, .src resolves =====

#[tokio::test(flavor = "multi_thread")]
async fn external_classic_relative_src_get_attribute_stays_raw_but_dot_src_resolves() {
    // Browser HTMLScriptElement contract:
    //
    // - `getAttribute('src')` returns the *raw* attribute string —
    //   "/chunk.js" if the HTML wrote "/chunk.js".
    // - `.src` (IDL attribute) returns the *resolved* absolute URL —
    //   "https://host/chunk.js".
    //
    // Turbopack's `getChunkFromRegistration` reads via
    // `e.getAttribute("src")`, so the raw form is the load-bearing
    // contract.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chunk.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            globalThis.__rawSrc = document.currentScript.getAttribute('src');
            globalThis.__idlSrc = document.currentScript.src;
            "#,
        ))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let base = Url::parse(&server.uri()).unwrap();
    engine.set_base_url(Some(base.clone()));

    let html = r#"<html><body>
        <script src="/chunk.js"></script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            "[globalThis.__rawSrc, globalThis.__idlSrc]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(
        out.value[0], "/chunk.js",
        "getAttribute keeps the raw attribute"
    );
    assert_eq!(
        out.value[1].as_str().unwrap(),
        format!("{}/chunk.js", server.uri()),
        ".src IDL returns the resolved absolute URL"
    );
}

// ===== Test 5: module scripts keep currentScript null =====

#[test]
fn inline_module_script_keeps_current_script_null() {
    // Spec (HTML §3.1.1): "Returns null … if the currently executing
    //  script … is a module script."
    let engine = JsEngine::new().unwrap();
    let html = r#"<html><body>
        <script type="module">
            globalThis.__moduleSawNull = document.currentScript === null;
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(html, "globalThis.__moduleSawNull", ScriptFetchPolicy::Skip)
        .expect("eval ok");
    assert_eq!(
        out.value,
        serde_json::json!(true),
        "modules must see document.currentScript === null"
    );
}

// ===== Test 6: Turbopack-shaped runtime regression =====

#[tokio::test(flavor = "multi_thread")]
async fn turbopack_shaped_runtime_chunk_registration_does_not_throw() {
    // This is the load-bearing scenario: a mocked chunk that mimics
    // the exact runtime contract from vercel/next.js
    // turbopack/crates/turbopack-ecmascript-runtime/js/src/browser/runtime/base/runtime-base.ts
    // (`getChunkFromRegistration`). The first chunk installs the
    // registry + the registerChunk hook; the second chunk pushes its
    // own currentScript. Before the fix, the second chunk threw
    // "chunk path empty but not in a worker" because
    // `document.currentScript` was `undefined`.
    let server = MockServer::start().await;

    // Chunk A — the "runtime" chunk. Installs the global TURBOPACK
    // registry, then swaps in a real `push` hook that mimics
    // `registerChunk` from the Turbopack runtime: extracts the src
    // via `e.getAttribute("src")`, records it.
    Mock::given(method("GET"))
        .and(path("/runtime.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            // Mirror Turbopack: `globalThis.TURBOPACK` starts as a plain
            // array (chunks queued before the runtime arrived push to
            // it), then the runtime promotes it to a `{ push: registerChunk }`
            // proxy and drains the pending queue. We skip the drain
            // (no pending chunks before this one) — the next chunk
            // will push directly into the proxy.
            function registerChunk(registration) {
                // Mirror getChunkFromRegistration from
                // runtime-base.ts: registration[0] is either a string
                // chunk path, a <script>-like element (extract via
                // getAttribute('src')), or falsy (worker / undefined).
                var e = registration[0];
                var chunk;
                if (typeof e === 'string') {
                    chunk = e;
                } else if (e) {
                    chunk = { src: e.getAttribute('src') };
                } else if (typeof TURBOPACK_NEXT_CHUNK_URLS !== 'undefined') {
                    chunk = { src: TURBOPACK_NEXT_CHUNK_URLS.pop() };
                } else {
                    throw new Error('chunk path empty but not in a worker');
                }
                globalThis.__registeredChunks = globalThis.__registeredChunks || [];
                globalThis.__registeredChunks.push(chunk);
            }
            // The runtime chunk's own self-registration (same pattern
            // as feature chunks: the "first push" before the proxy is
            // installed lands on the plain array).
            (globalThis.TURBOPACK = globalThis.TURBOPACK || []).push([
                "object" == typeof document ? document.currentScript : void 0,
                'runtime-id',
                function(){}
            ]);
            // Promote to the registerChunk-backed proxy and drain
            // the pre-runtime queue.
            var chunksToRegister = globalThis.TURBOPACK;
            globalThis.TURBOPACK = { push: registerChunk };
            chunksToRegister.forEach(registerChunk);
            "#,
        ))
        .mount(&server)
        .await;

    // Chunk B — a feature chunk. Pushes its own currentScript-derived
    // registration after the runtime is up. The throw site is
    // `registerChunk` reading registration[0].
    Mock::given(method("GET"))
        .and(path("/chunk-b.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            (globalThis.TURBOPACK || (globalThis.TURBOPACK = [])).push([
                "object" == typeof document ? document.currentScript : void 0,
                'chunk-b-id',
                function(){}
            ]);
            "#,
        ))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));

    let html = format!(
        r#"<html><body>
            <script src="{server}/runtime.js"></script>
            <script src="{server}/chunk-b.js"></script>
        </body></html>"#,
        server = server.uri(),
    );

    let out = engine
        .eval_with_html_policy(
            &html,
            // Returns the recorded chunk srcs so we can assert what
            // landed. Crucially: this only returns if neither chunk
            // threw.
            "globalThis.__registeredChunks ? globalThis.__registeredChunks.map(c => typeof c === 'string' ? c : c.src) : null",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");

    // Both chunks must have registered without throwing. The raw src
    // attribute is what each chunk's currentScript exposes via
    // getAttribute('src'), which `registerChunk` then wraps as
    // `{ src: <raw> }`.
    let registered = out.value.as_array().expect("got an array").clone();
    assert_eq!(
        registered.len(),
        2,
        "both chunks must register; before the fix the second one threw 'chunk path empty but not in a worker'"
    );
    assert_eq!(
        registered[0].as_str().unwrap(),
        format!("{}/runtime.js", server.uri()),
        "runtime chunk must self-register via document.currentScript.getAttribute('src')"
    );
    assert_eq!(
        registered[1].as_str().unwrap(),
        format!("{}/chunk-b.js", server.uri()),
        "feature chunk must self-register via document.currentScript.getAttribute('src')"
    );

    // And there must be no console errors mentioning the throw — even
    // if the script handler caught it, the buffer would carry the
    // error message.
    let buf_str = serde_json::to_string(&out.console).unwrap_or_default();
    assert!(
        !buf_str.contains("chunk path empty but not in a worker"),
        "no chunk must throw the Turbopack self-detection error; got console: {buf_str}"
    );
}
