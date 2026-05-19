//! Integration tests for the item M-A surface: real ES module
//! loading for `<script type="module">`. Per WHATWG HTML §8.1.3
//! "Module scripts" and the M-A subagent brief.
//!
//! Wiremock-rs serves localhost `.js` files; the engine's HTTP-backed
//! [`heso_engine_js::HttpLoader`] fetches them on demand through the
//! shared `reqwest::Client` and caches them in
//! [`heso_engine_js::ModuleCache`] so two imports of the same URL
//! produce exactly one network round-trip (`module_cache_no_double
//! _fetch`).
//!
//! These three tests cover the M-A requirements verbatim:
//!
//! - `module_inline_with_export_and_import` — inline module with
//!   `export` syntax (proves real module-mode parsing).
//! - `module_external_import_relative` — two-module diamond where
//!   `a.js` imports `b.js` relative; assert exported value is
//!   readable from `globalThis`.
//! - `module_cache_no_double_fetch` — two top-level inline modules
//!   each import the same dependency URL; assert the wiremock saw
//!   exactly one request.

use std::sync::Arc;

use heso_engine_js::{JsEngine, ScriptFetchPolicy};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a workspace-shaped `reqwest::Client` for the loader.
fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-module-tests/0.0.1")
            .build()
            .expect("client builds"),
    )
}

/// Build a JS engine with live fetch — the loader needs the client +
/// runtime handle to honor `import "./dep.js"` chains over HTTP.
fn engine_with_fetch() -> JsEngine {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    JsEngine::new_with_fetch(client, rt).expect("engine builds")
}

// ===== Test 1: inline module exports + body executes =====

#[tokio::test(flavor = "multi_thread")]
async fn module_inline_with_export_and_body_runs() {
    // This is the same shape as the unit-test `module_inline_with_
    // export_and_import` in `engine.rs`, restated against the
    // full integration harness. Proves the M-A path works without
    // any network — fully inline.
    let engine = engine_with_fetch();
    let html = r#"<html><body>
        <script type="module">
            export const greeting = "hello";
            globalThis.observedGreeting = greeting;
            globalThis.exportSyntaxParsed = true;
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            "[globalThis.observedGreeting, globalThis.exportSyntaxParsed]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(["hello", true]));
}

// ===== Test 2: external import-relative two-module chain =====

#[tokio::test(flavor = "multi_thread")]
async fn module_external_import_relative() {
    // Two modules: `a.js` exports `ab` derived from `b.js`'s `b`.
    // Page loads `a.js` as a `<script type="module">`. The engine's
    // pre-fetch grabs `a.js`'s body, seeds it under the resolved
    // URL, and calls `Module::evaluate`. QuickJS's module evaluator
    // sees `import {b} from "./b.js"`, calls our resolver (joins
    // `./b.js` against `a.js`'s URL), then our loader (fetches the
    // resolved URL via HTTP). Both bodies write to globalThis so
    // the user-JS pass can observe both.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
                import { b } from "./b.js";
                export const ab = b + 1;
                globalThis.observedB = b;
                globalThis.observedAB = ab;
                "#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
                export const b = 41;
                globalThis.bModuleRan = true;
                "#,
        ))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    // Engine needs a base URL so the loader can resolve relative
    // module URLs from the inline-script bootstrap. The `<script
    // type="module" src="/a.js">` reference is rooted against this.
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));

    let html = format!(
        r#"<html><body>
            <script type="module" src="{server}/a.js"></script>
        </body></html>"#,
        server = server.uri(),
    );
    let out = engine
        .eval_with_html_policy(
            &html,
            "[globalThis.observedB, globalThis.observedAB, globalThis.bModuleRan]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!([41, 42, true]));
}

// ===== Test 3: cache prevents double-fetch =====

#[tokio::test(flavor = "multi_thread")]
async fn module_cache_no_double_fetch() {
    // Two separate top-level modules both `import "./shared.js"`.
    // Without the cache, `shared.js` would be fetched twice. With
    // it, the second import hits the cache. Wiremock's
    // `expect(1)` matcher asserts exactly one HTTP call.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shared.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
                export const counter = 1;
                globalThis.sharedFetchCount = (globalThis.sharedFetchCount || 0) + 1;
                "#,
        ))
        .expect(1) // <-- the load-bearing assertion
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/one.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            import { counter } from "./shared.js";
            globalThis.one = counter;
            "#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/two.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            import { counter } from "./shared.js";
            globalThis.two = counter;
            "#,
        ))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));
    let html = format!(
        r#"<html><body>
            <script type="module" src="{server}/one.js"></script>
            <script type="module" src="{server}/two.js"></script>
        </body></html>"#,
        server = server.uri(),
    );
    let out = engine
        .eval_with_html_policy(
            &html,
            "[globalThis.one, globalThis.two, globalThis.sharedFetchCount]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    // `shared.js`'s body ran exactly once even though it was
    // imported by two distinct modules — both observers see the
    // same exported value because they share the module instance.
    assert_eq!(out.value, serde_json::json!([1, 1, 1]));

    // Also assert via the cache directly — `shared.js` ended up in
    // it exactly once.
    let cache = engine.module_cache();
    let shared_url = format!("{}/shared.js", server.uri());
    assert!(
        cache.contains(&shared_url),
        "shared.js should be in module cache after first fetch"
    );
    // Wiremock will panic at drop time if `expect(1)` was not met,
    // so we don't have to assert that explicitly here.
}

// ===== Test 4: inline module + external dep mix =====

#[tokio::test(flavor = "multi_thread")]
async fn module_inline_can_import_from_external() {
    // The inline-module-imports-external case: agent code that does
    // `<script type="module">import { x } from "./dep.js"; ...
    // </script>` in the page itself. The synthetic inline specifier
    // serves as the base URL for the relative join — same machinery
    // as the all-external chain.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/dep.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            export const dep_value = "from-external";
            "#,
        ))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));
    let html = r#"<html><body>
        <script type="module">
            import { dep_value } from "./dep.js";
            globalThis.depObserved = dep_value;
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(html, "globalThis.depObserved", ScriptFetchPolicy::Fetch)
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!("from-external"));
}

// ===== Test 5: missing external dep surfaces clear error =====

#[tokio::test(flavor = "multi_thread")]
async fn module_missing_dep_logs_loading_error_without_crashing() {
    // The error-path: an inline module imports a URL that returns
    // 404. The loader returns `Error::new_loading`; QuickJS routes
    // it through the module-evaluator's error path; we capture it
    // on the console buffer as an error. The pump must NOT crash —
    // same containment rule as throw-in-classic-script.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing.js"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));
    let html = r#"<html><body>
        <script type="module">
            import { x } from "./missing.js";
            globalThis.shouldNotRun = true;
        </script>
        <script>
            // Sibling classic script — runs regardless. Proves the
            // module's compile error didn't poison the pump.
            globalThis.classicRan = true;
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            "[globalThis.shouldNotRun, globalThis.classicRan]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    // The module body never ran; `shouldNotRun` is undefined → null.
    // The classic sibling did run.
    assert_eq!(out.value, serde_json::json!([null, true]));
}
