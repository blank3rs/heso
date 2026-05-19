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

// ===== Wire-up tests: dynamic import + import map end-to-end =====
//
// The tests below pin the three wires the module-loader brief
// describes:
//
//   - Wire 1: `JsEngine::new_inner` installs a default
//     `ModuleResolveFn` that bridges `globalThis.import()` to the same
//     `HttpResolver` + `ModuleCache` + `HttpFetcher` the static
//     `<script type="module">` path uses.
//   - Wire 2: the `<script type="importmap">` data block is parsed and
//     installed by the [`scripts`] pump BEFORE any module script runs,
//     so both static and dynamic imports observe it.
//   - Wire 3: this section — the integration tests proving the
//     wires actually act as one system.

// ===== Wire 1, Test 1: dynamic import works out of the box =====

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_import_resolves_via_default_resolver() {
    // Without the default resolver, this test would reject with "no
    // module loader installed" — the pre-wireup behavior of the M-C
    // shim. With Wire 1 in place, the engine fetches `./greeter.js`
    // via the same machinery a `<script type="module">` would use,
    // and the dynamic `import()` resolves to the module namespace.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/greeter.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            export const greet = (name) => "hello " + name;
            export const VERSION = "1.0.0";
            "#,
        ))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));

    // No `<script type="module">` on the page — dynamic `import()` is
    // the only entry point. Page has just a classic script that calls
    // the shim and stashes the result.
    let html = r#"<html><body>
        <script>
            globalThis.__import_outcome = globalThis.import('./greeter.js').then(
                (ns) => ({ ok: true, msg: ns.greet('world'), v: ns.VERSION }),
                (err) => ({ ok: false, msg: String((err && err.message) || err) }),
            );
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            "globalThis.__import_outcome",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(
        out.value,
        serde_json::json!({ "ok": true, "msg": "hello world", "v": "1.0.0" })
    );
}

// ===== Wire 1, Test 2: shared cache across static and dynamic =====

#[tokio::test(flavor = "multi_thread")]
async fn dynamic_import_and_static_import_share_module_cache() {
    // The load-bearing wire: a dynamic `import()` of a URL that a
    // `<script type="module">` already pulled in should hit the
    // cache, not re-fetch. Wiremock's `.expect(1)` asserts exactly
    // one HTTP request for the shared dependency.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shared.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            export const value = 7;
            "#,
        ))
        .expect(1) // <-- the load-bearing assertion
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));

    // Static module imports shared.js, then a classic script does
    // a dynamic `import('./shared.js')`. The wiremock counter sees
    // exactly one GET.
    let html = r#"<html><body>
        <script type="module">
            import { value } from "./shared.js";
            globalThis.staticValue = value;
        </script>
        <script>
            globalThis.__dyn_outcome = globalThis.import('./shared.js').then(
                (ns) => ({ ok: true, value: ns.value }),
                (err) => ({ ok: false, msg: String((err && err.message) || err) }),
            );
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            "[globalThis.staticValue, globalThis.__dyn_outcome]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    // Both observers should see the same value — and both should
    // come from the same module instance (so two evaluations of
    // `shared.js` did NOT happen).
    let arr = out.value.as_array().expect("array");
    assert_eq!(arr[0], serde_json::json!(7));
    assert_eq!(
        arr[1],
        serde_json::json!({ "ok": true, "value": 7 }),
        "dynamic import outcome mismatched: {:?}",
        arr[1]
    );

    // Confirm via the cache directly that shared.js only landed once
    // (wiremock's `.expect(1)` already pins the network side).
    let cache = engine.module_cache();
    let shared_url = format!("{}/shared.js", server.uri());
    assert!(
        cache.contains(&shared_url),
        "shared.js should be in module cache after first fetch"
    );
}

// ===== Wire 2, Test 3: import map remaps bare in module scripts =====

#[tokio::test(flavor = "multi_thread")]
async fn import_map_remaps_bare_specifier_in_module_script() {
    // The static-side payoff of Wire 2: `<script type="importmap">`
    // declares `"lodash": "/_/lodash.js"`, and `<script
    // type="module">import _ from "lodash"` hits `/_/lodash.js`
    // (not `/lodash`, which is what the pre-importmap behavior
    // would have produced). The mock only registers `/_/lodash.js`;
    // a request to `/lodash` would 404 and the test would fail.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/_/lodash.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            export default { name: "fake-lodash", version: "0.0.1" };
            "#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));

    let html = format!(
        r#"<html><body>
            <script type="importmap">
            {{
                "imports": {{
                    "lodash": "{server}/_/lodash.js"
                }}
            }}
            </script>
            <script type="module">
                import _ from "lodash";
                globalThis.lodashName = _.name;
                globalThis.lodashVersion = _.version;
            </script>
        </body></html>"#,
        server = server.uri(),
    );

    let out = engine
        .eval_with_html_policy(
            &html,
            "[globalThis.lodashName, globalThis.lodashVersion]",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(["fake-lodash", "0.0.1"]));
}

// ===== Wire 2, Test 4: import map scopes work =====

#[tokio::test(flavor = "multi_thread")]
async fn import_map_scoped_specifier_in_module_script() {
    // The spec's scope mechanism: an `imports` map and a `scopes`
    // map that overrides the top-level mapping when the referrer
    // URL is under the scope prefix. Here:
    //   - `lodash` normally maps to `/_/lodash-v4.js`.
    //   - For modules under `/admin/`, `lodash` maps to
    //     `/_/lodash-v3.js`.
    // We host the entry module under `/admin/dash.js` so its
    // referrer falls into the scope; the import map's scoped entry
    // wins.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/_/lodash-v4.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"export const version = "4.0.0";"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/_/lodash-v3.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"export const version = "3.0.0";"#,
        ))
        .expect(1) // scoped match must hit v3 exactly once
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/admin/dash.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"
            import { version } from "lodash";
            globalThis.lodashVersionInScope = version;
            "#,
        ))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));

    let html = format!(
        r#"<html><body>
            <script type="importmap">
            {{
                "imports": {{
                    "lodash": "{server}/_/lodash-v4.js"
                }},
                "scopes": {{
                    "{server}/admin/": {{
                        "lodash": "{server}/_/lodash-v3.js"
                    }}
                }}
            }}
            </script>
            <script type="module" src="{server}/admin/dash.js"></script>
        </body></html>"#,
        server = server.uri(),
    );

    let out = engine
        .eval_with_html_policy(
            &html,
            "globalThis.lodashVersionInScope",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(
        out.value,
        serde_json::json!("3.0.0"),
        "scoped lodash should win over top-level imports map"
    );
}

// ===== Wire 1+2, Test 5: import map applies to dynamic import =====

#[tokio::test(flavor = "multi_thread")]
async fn import_map_applies_to_dynamic_import_too() {
    // Wire 1 and Wire 2 together: a dynamic `globalThis.import('lodash')`
    // call from a classic script must consult the page's import map
    // (same map the static path uses). Pre-wireup, a bare specifier
    // through the dynamic path would have either errored or fallen
    // through to plain URL resolution. With Wire 2, the map handles it.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/vendor/lodash.js"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"export const tag = "vendored";"#,
        ))
        .expect(1)
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse(&server.uri()).unwrap()));

    let html = format!(
        r#"<html><body>
            <script type="importmap">
            {{
                "imports": {{
                    "lodash": "{server}/vendor/lodash.js"
                }}
            }}
            </script>
            <script>
                globalThis.__lodash_outcome = globalThis.import('lodash').then(
                    (ns) => ({{ ok: true, tag: ns.tag }}),
                    (err) => ({{ ok: false, msg: String((err && err.message) || err) }}),
                );
            </script>
        </body></html>"#,
        server = server.uri(),
    );

    let out = engine
        .eval_with_html_policy(
            &html,
            "globalThis.__lodash_outcome",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(
        out.value,
        serde_json::json!({ "ok": true, "tag": "vendored" })
    );
}

// ===== Wire 1+2, Test 6: clear bare-specifier error message =====

#[tokio::test(flavor = "multi_thread")]
async fn unresolvable_bare_specifier_without_import_map_rejects() {
    // A page with no `<script type="importmap">` block and a bare
    // specifier in a `import()` call should reject with a clear
    // diagnostic that mentions both the specifier and the importmap
    // mechanism. This is the user-facing surface of the shared
    // `resolve_specifier_through_import_map` helper — agents who
    // hit this error know what to do (declare an import map, or
    // switch to a relative/absolute specifier).
    let engine = engine_with_fetch();
    engine.set_base_url(Some(Url::parse("https://example.test/").unwrap()));

    let html = r#"<html><body>
        <script>
            globalThis.__bare_outcome = globalThis.import('lodash').then(
                (ns) => ({ ok: true, msg: null, name: null }),
                (err) => ({
                    ok: false,
                    msg: String((err && err.message) || err),
                    name: err && err.name,
                }),
            );
        </script>
    </body></html>"#;
    let out = engine
        .eval_with_html_policy(
            html,
            "globalThis.__bare_outcome",
            ScriptFetchPolicy::Fetch,
        )
        .expect("eval ok");
    assert_eq!(out.value["ok"], false);
    let msg = out.value["msg"].as_str().expect("msg is string");
    // The error message must surface the specifier verbatim and
    // direct the agent at the import-map mechanism — both pieces
    // are part of the diagnostic contract.
    assert!(
        msg.contains("lodash"),
        "rejection should mention specifier; got: {msg}"
    );
    assert!(
        msg.contains("importmap"),
        "rejection should mention importmap; got: {msg}"
    );
    // TypeError shape — same as every other dynamic-import rejection.
    assert_eq!(out.value["name"], "TypeError");
}
