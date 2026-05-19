//! Integration tests for the PR2 / item-C surface: `fetch()` inside
//! JS, routed through the shared `reqwest::Client`. Per
//! `next-phase-plan.md` item C and ADR 0008's determinism gate.
//!
//! These tests use [`wiremock::MockServer`] for localhost HTTP
//! exchanges so the workspace `cargo test` stays hermetic, plus one
//! `data:` URL test that needs no server at all.
//!
//! The PR3 `__hesoDeepResolve` wrap (see the `engine` module docs)
//! makes both `(async () => { await fetch(...); ... })()` and
//! nested-Promise return values (`[fetch(...), fetch(...)]`, etc.)
//! serialize to their data rather than to `{}`. The
//! `nested_promise_*` and `async_iife_with_real_http_fetch_*` tests
//! pin those load-bearing behaviors.

use std::sync::Arc;

use heso_engine_js::{FetchMode, JsEngine, ScriptFetchPolicy};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Build a fresh [`reqwest::Client`] matching the rest of the
/// workspace's shape (rustls, gzip+brotli, HTTP/2, identifying as
/// `heso-engine-js-tests`).
fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-tests/0.0.1")
            .build()
            .expect("client builds"),
    )
}

/// Build a JS engine in `FetchMode::Live` with the supplied tokio
/// handle.
fn engine_with_fetch() -> JsEngine {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    JsEngine::new_with_fetch(client, rt).expect("engine builds")
}

// ===== data: URL — works without a server =====

#[tokio::test(flavor = "multi_thread")]
async fn fetch_data_url_resolves_to_body_via_then() {
    let engine = engine_with_fetch();
    // Use .then to stash the body into globalThis so we can observe
    // it after the engine drains its pending-fetch queue.
    let _ = engine
        .eval(
            r#"
            fetch("data:text/plain,hello").then(r => r.text()).then(t => {
                globalThis.__body = t;
            });
            "#,
        )
        .expect("schedule fetch");
    // Engine's `eval` already drained pending fetches before returning.
    let out = engine.eval("globalThis.__body").expect("observe");
    assert_eq!(out.value, serde_json::json!("hello"));
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_data_url_exposes_status_200_and_ok_true() {
    let engine = engine_with_fetch();
    let _ = engine
        .eval(
            r#"
            fetch("data:text/plain,xyz").then(r => {
                globalThis.__status = r.status;
                globalThis.__ok = r.ok;
                globalThis.__url = r.url;
            });
            "#,
        )
        .expect("schedule");
    let out = engine
        .eval("[globalThis.__status, globalThis.__ok, globalThis.__url]")
        .expect("observe");
    assert_eq!(out.value[0], 200);
    assert_eq!(out.value[1], true);
    assert_eq!(out.value[2], "data:text/plain,xyz");
}

// ===== HTTP GET via wiremock-rs =====

#[tokio::test(flavor = "multi_thread")]
async fn fetch_http_get_returns_status_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/greet"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello, agent"))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/greet", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            fetch({url:?}).then(r => {{
                globalThis.__status = r.status;
                return r.text();
            }}).then(t => {{
                globalThis.__body = t;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine
        .eval("[globalThis.__status, globalThis.__body]")
        .expect("observe");
    assert_eq!(out.value[0], 200);
    assert_eq!(out.value[1], "hello, agent");
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_http_post_with_body_sends_body_and_decodes_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(|req: &Request| {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "received": body,
                "len": body.len(),
            }))
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/echo", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            fetch({url:?}, {{ method: "POST", body: "x" }}).then(r => {{
                globalThis.__status = r.status;
                return r.json();
            }}).then(j => {{
                globalThis.__body = j;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine
        .eval("[globalThis.__status, globalThis.__body]")
        .expect("observe");
    assert_eq!(out.value[0], 201);
    assert_eq!(out.value[1]["received"], "x");
    assert_eq!(out.value[1]["len"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_post_with_json_body_sets_content_type_automatically() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .respond_with(|req: &Request| {
            let ct = req
                .headers
                .get("content-type")
                .map(|v| v.to_str().unwrap_or("").to_owned())
                .unwrap_or_default();
            let body = String::from_utf8_lossy(&req.body).into_owned();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content_type": ct,
                "body": body,
            }))
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/api", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            fetch({url:?}, {{
                method: "POST",
                body: {{ name: "alice", count: 3 }}
            }}).then(r => r.json()).then(j => {{
                globalThis.__got = j;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__got").expect("observe");
    assert_eq!(out.value["content_type"], "application/json");
    let body: serde_json::Value =
        serde_json::from_str(out.value["body"].as_str().expect("body str")).expect("body is json");
    assert_eq!(body["name"], "alice");
    assert_eq!(body["count"], 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_http_404_resolves_with_ok_false() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exists"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/nope", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            fetch({url:?}).then(r => {{
                globalThis.__status = r.status;
                globalThis.__ok = r.ok;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    // WHATWG: 4xx/5xx still resolve the promise; `ok` reflects 2xx-ness.
    let out = engine
        .eval("[globalThis.__status, globalThis.__ok]")
        .expect("observe");
    assert_eq!(out.value[0], 404);
    assert_eq!(out.value[1], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_response_headers_get_is_case_insensitive() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/hdr"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("X-Custom", "agent-shaped")
                .set_body_string("ok"),
        )
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/hdr", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            fetch({url:?}).then(r => {{
                globalThis.__lower = r.headers.get("x-custom");
                globalThis.__upper = r.headers.get("X-CUSTOM");
                globalThis.__missing = r.headers.get("missing");
                globalThis.__has_lower = r.headers.has("x-custom");
                globalThis.__has_missing = r.headers.has("nope");
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine
        .eval("[globalThis.__lower, globalThis.__upper, globalThis.__missing, globalThis.__has_lower, globalThis.__has_missing]")
        .expect("observe");
    assert_eq!(out.value[0], "agent-shaped");
    assert_eq!(out.value[1], "agent-shaped");
    assert!(out.value[2].is_null());
    assert_eq!(out.value[3], true);
    assert_eq!(out.value[4], false);
}

// ===== Determinism gate: --seed N + no cassette =====

#[tokio::test(flavor = "multi_thread")]
async fn seed_without_cassette_rejects_fetch_with_clear_error() {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    // `new_with_seed_and_fetch(seed != 0, ...)` lands in
    // `DeterministicNoCassette` per ADR 0008.
    let engine = JsEngine::new_with_seed_and_fetch(42, client, rt).expect("engine builds");
    let _ = engine
        .eval(
            r#"
            fetch("https://example.com").then(
                _ => { globalThis.__outcome = "resolved"; },
                e => { globalThis.__outcome = "rejected"; globalThis.__msg = String(e); }
            );
            "#,
        )
        .expect("schedule");
    let out = engine
        .eval("[globalThis.__outcome, globalThis.__msg]")
        .expect("observe");
    assert_eq!(out.value[0], "rejected");
    let msg = out.value[1].as_str().expect("msg is string");
    assert!(msg.contains("not in cassette"), "msg = {msg:?}");
    assert!(msg.contains("--record"), "msg = {msg:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn seed_zero_with_fetch_uses_live_path() {
    // seed = 0 is the unseeded sentinel — the CLI takes Live path for
    // it. Direct constructor call mirrors that contract.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok"))
        .respond_with(ResponseTemplate::new(200).set_body_string("live"))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/ok", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            fetch({url:?}).then(r => r.text()).then(t => {{
                globalThis.__body = t;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__body").expect("observe");
    assert_eq!(out.value, "live");
}

// ===== --js-fetch absent (default): no `fetch` global installed =====

#[tokio::test(flavor = "multi_thread")]
async fn engine_without_fetch_has_no_fetch_global() {
    let engine = JsEngine::new().expect("engine new");
    let out = engine.eval("typeof fetch").expect("eval");
    assert_eq!(out.value, "undefined");
}

// ===== top-level await: thenable returns are unwrapped via microtask pump =====

#[tokio::test(flavor = "multi_thread")]
async fn await_top_level_unwraps_fetch_via_microtask_pump() {
    // Top-level `(async () => { await fetch(...); ... })()` returns a
    // Promise; [`engine::JsEngine::eval_value_with_promise_await`]
    // registers `.then(settle)` and drains microtasks via
    // [`engine::JsEngine::run_pending_jobs`] before serializing —
    // so the user observes the resolved body, not a Promise stub.
    //
    // Earlier this case asserted the opposite (a documented PR2
    // limitation); the microtask pump that landed alongside the
    // Preact-event-delegation work makes top-level await Just Work.
    let engine = engine_with_fetch();
    let out = engine
        .eval(
            r#"
            (async () => {
                const r = await fetch("data:text/plain,async-now-pumped");
                return r.text();
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "async-now-pumped");
}

// ===== Deep-Promise unwrap (PR-3): nested Promises become resolved values =====

#[tokio::test(flavor = "multi_thread")]
async fn nested_promise_in_array_is_deep_resolved_not_serialized_as_empty_object() {
    // Pinned regression: prior to the `__hesoDeepResolve` wrap landing,
    // a Promise nested in an array passed through `JSON.stringify`
    // unchanged — and Promises have no own enumerable properties, so
    // each element serialized as `{}`. AGENT_FINDINGS Task 3 reported
    // this as "A returned Promise serializes as `{}`."
    let engine = engine_with_fetch();
    let out = engine
        .eval("[Promise.resolve(1), Promise.resolve(2), Promise.resolve(3)]")
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([1, 2, 3]));
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_promise_in_object_property_is_deep_resolved() {
    // Same shape as above but through an object property. Real agent
    // patterns produce this when packaging up partial extractions
    // (e.g. `{title: ..., body: fetch(...).then(r => r.text())}`).
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/payload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/payload", server.uri());
    let out = engine
        .eval(&format!(
            r#"({{
                title: "ok",
                body: fetch({url:?}).then(r => r.text()),
            }})"#,
            url = url,
        ))
        .expect("eval");
    assert_eq!(out.value["title"], "ok");
    assert_eq!(out.value["body"], "hello");
}

#[tokio::test(flavor = "multi_thread")]
async fn async_iife_with_real_http_fetch_json_returns_extracted_property() {
    // Pinned regression for AGENT_FINDINGS Task 3's reproducing pattern.
    // The agent wrote (in their words):
    //
    //     "Tried `eval-dom --js-fetch` with `await`/promise → fetch global
    //      exists, but the JS engine returns the script's last expression
    //      synchronously without draining the QuickJS job queue."
    //
    // The pattern below is what they should have used and now works:
    //
    //     (async () => {
    //         const r = await fetch("https://httpbin.org/get?ping=pong");
    //         const j = await r.json();
    //         return j.args.ping;   // "pong"
    //     })()
    //
    // We simulate httpbin's `/get?ping=...` shape via wiremock to keep
    // the workspace `cargo test` hermetic (no real network). The
    // engine's existing thenable-await path resolves the IIFE; this
    // test pins that real-HTTP (not just `data:` URL) doesn't regress.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/get"))
        .respond_with(|req: &wiremock::Request| {
            // Echo back the `ping` query param in an `args` object,
            // matching httpbin's response shape.
            let ping = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "ping")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "args": { "ping": ping },
                "url": req.url.to_string(),
            }))
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/get?ping=pong", server.uri());
    let out = engine
        .eval(&format!(
            r#"
            (async () => {{
                const r = await fetch({url:?});
                const j = await r.json();
                return j.args.ping;
            }})()
            "#,
            url = url,
        ))
        .expect("eval");
    assert_eq!(out.value, "pong");
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_fetches_via_array_map_unwrap_to_resolved_values() {
    // `[1, 2, 3].map(n => fetch(...).then(r => r.json()))` returns an
    // array of Promises. Pre-fix this serialized as `[{}, {}, {}]`.
    // Now each element resolves to its value because `__hesoDeepResolve`
    // walks the array.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/get"))
        .respond_with(|req: &wiremock::Request| {
            let n = req
                .url
                .query_pairs()
                .find(|(k, _)| k == "n")
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "args": { "n": n },
            }))
        })
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let base = server.uri();
    let out = engine
        .eval(&format!(
            r#"
            [1, 2, 3].map(n =>
                fetch("{base}/get?n=" + n)
                    .then(r => r.json())
                    .then(j => j.args.n)
            )
            "#,
            base = base,
        ))
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["1", "2", "3"]));
}

// ===== Page-script integration: <script src=> honored under Fetch policy =====

#[tokio::test(flavor = "multi_thread")]
async fn external_script_src_fetched_under_fetch_policy() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/inline.js"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/javascript")
                .set_body_string("globalThis.__hydrated = 'yes';"),
        )
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let script_url = format!("{}/inline.js", server.uri());
    let html = format!(r#"<html><body><script src="{script_url}"></script></body></html>"#);
    let out = engine
        .eval_with_html_policy(&html, "globalThis.__hydrated", ScriptFetchPolicy::Fetch)
        .expect("eval");
    assert_eq!(out.value, "yes");
}

#[tokio::test(flavor = "multi_thread")]
async fn external_script_src_failed_fetch_writes_console_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing.js"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let script_url = format!("{}/missing.js", server.uri());
    let html = format!(r#"<html><body><script src="{script_url}"></script></body></html>"#);
    let out = engine
        .eval_with_html_policy(&html, "1+1", ScriptFetchPolicy::Fetch)
        .expect("eval");
    assert_eq!(out.value, 2);
    assert!(
        out.console.iter().any(|c| c
            .args
            .first()
            .and_then(|v| v.as_str())
            .map(|s| s.contains("missing.js") && s.contains("HTTP 404"))
            .unwrap_or(false)),
        "expected an HTTP 404 console error, got: {:?}",
        out.console
    );
}

// ===== AGENT_FINDINGS_V3 silent-null regression =====
//
// `await heso.flush()` (or any `await Promise.resolve()` / `await <non-thenable>`)
// followed by `await fetch(...)` used to return `null` instead of the fetched
// JSON. Root cause: [`JsEngine::run_pending_jobs`] drained the fetch queue
// BEFORE pumping microtasks, so when the user's async function was suspended
// at the first `await` (and no fetch was queued yet at the synchronous
// entry point), the loop observed `drained == 0` on the first iteration and
// broke. The final microtask pump then ran the user's async resumption,
// which called `fetch(...)` and queued it — but the queue was never drained
// again. The Promise nobody settled stayed pending; the engine's eval
// returned `Ok(serde_json::Value::Null)` via the
// `Thenable registered but never settled` branch — a silent failure.
//
// Fix: pump microtasks FIRST in each loop iteration of `run_pending_jobs`,
// so the user's `await`-suspended async function gets to enqueue its fetch
// BEFORE the drain step. The tests below exercise the three problematic
// shapes V3 flagged plus a couple of generalized variants.

#[tokio::test(flavor = "multi_thread")]
async fn await_heso_flush_then_await_fetch_resolves_to_real_response() {
    // The exact shape AGENT_FINDINGS_V3.md flagged as "silent null":
    //
    //     (async () => {
    //         await heso.flush();
    //         const v = await fetch(URL).then(r => r.json());
    //         return JSON.stringify({url: v.url});
    //     })()
    //
    // Pre-fix: `value: null, error: null, console: []`.
    // Post-fix: returns the JSON payload's `url` field.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/get"))
        .respond_with(|req: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": req.url.to_string(),
            }))
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/get?after=submit", server.uri());
    let out = engine
        .eval(&format!(
            r#"
            (async () => {{
                await heso.flush();
                const verify = await fetch({url:?}).then(r => r.json());
                return JSON.stringify({{url: verify.url}});
            }})()
            "#,
            url = url,
        ))
        .expect("eval");
    // The core regression: BEFORE the fix this returned
    // `serde_json::Value::Null` silently with no console output.
    // After the fix, the IIFE's `.return JSON.stringify(...)` produces
    // a JSON string containing the request URL the mock saw.
    assert!(
        !out.value.is_null(),
        "regression: heso.flush() + await fetch returned null silently"
    );
    let s = out.value.as_str().expect("string return");
    let parsed: serde_json::Value = serde_json::from_str(s).expect("inner JSON");
    // wiremock normalizes the host to `localhost` but echoes the path;
    // pin the path + query rather than the full URL.
    assert!(
        parsed["url"].as_str().is_some_and(|u| u.ends_with("/get?after=submit")),
        "expected URL ending with /get?after=submit, got {parsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn await_promise_resolve_then_await_fetch_resolves() {
    // Generalized: `heso.flush()` is just `Promise.resolve()` under the
    // hood, so the same trap fires for any `await Promise.resolve()`
    // followed by a fetch. Pre-fix: silent null. Post-fix: real body.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/probe"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok-after-resolve"))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/probe", server.uri());
    let out = engine
        .eval(&format!(
            r#"
            (async () => {{
                await Promise.resolve();
                const r = await fetch({url:?});
                return r.text();
            }})()
            "#,
            url = url,
        ))
        .expect("eval");
    assert_eq!(out.value, "ok-after-resolve");
}

#[tokio::test(flavor = "multi_thread")]
async fn await_non_thenable_then_await_fetch_resolves() {
    // Even more reduced: `await <number>` suspends the async function
    // the same way as `await Promise.resolve()`, so any later
    // `await fetch(...)` triggered the same trap. Pre-fix: silent null.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/n"))
        .respond_with(ResponseTemplate::new(200).set_body_string("non-thenable-ok"))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/n", server.uri());
    let out = engine
        .eval(&format!(
            r#"
            (async () => {{
                let x = 1;
                await x;
                const r = await fetch({url:?});
                return r.text();
            }})()
            "#,
            url = url,
        ))
        .expect("eval");
    assert_eq!(out.value, "non-thenable-ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_side_effect_then_flush_then_fetch_resolves() {
    // The full AGENT_FINDINGS_V3 reproducer chain: a synchronous DOM
    // mutation + form.submit() (which itself block_on's an HTTP call
    // via __hesoFormSubmitNow), then `await heso.flush()`, then
    // `await fetch(...)`. The `f.submit()` makes the chain match the
    // verbatim repro; the silent-null is caused by `heso.flush()`,
    // not by `f.submit()`.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/anything"))
        .respond_with(ResponseTemplate::new(200).set_body_string(""))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/get"))
        .respond_with(|req: &Request| {
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "url": req.url.to_string(),
            }))
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let form_action = format!("{}/anything", server.uri());
    let verify_url = format!("{}/get?after=submit", server.uri());

    // Set a base URL so __hesoFormSubmitNow can resolve relative refs;
    // the form's `action` is absolute so this is belt-and-braces.
    engine.set_base_url(Some(url::Url::parse("https://example.com/").unwrap()));

    let html = "<html><body></body></html>";
    let out = engine
        .eval_with_html(
            html,
            &format!(
                r#"
                (async () => {{
                    document.body.innerHTML = '<form action="{form}" method="post" id="f"><input name="marker" value="v3-test"></form>';
                    const f = document.getElementById('f');
                    f.submit();
                    await heso.flush();
                    const verify = await fetch({verify:?}).then(r => r.json());
                    return JSON.stringify({{url: verify.url}});
                }})()
                "#,
                form = form_action,
                verify = verify_url,
            ),
        )
        .expect("eval");
    // The verbatim AGENT_FINDINGS_V3 reproducer: before the fix this
    // silently returned `null`. After the fix it returns the JSON
    // body the mock served.
    assert!(
        !out.value.is_null(),
        "regression: form.submit() + heso.flush() + await fetch returned null silently"
    );
    let s = out.value.as_str().expect("string return");
    let parsed: serde_json::Value = serde_json::from_str(s).expect("inner JSON");
    assert!(
        parsed["url"].as_str().is_some_and(|u| u.ends_with("/get?after=submit")),
        "expected URL ending with /get?after=submit, got {parsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn await_fetch_after_then_chain_still_resolves() {
    // Composite: a `.then` chain produces a value, an `await` suspends
    // on that chain, then a later `await fetch(...)` runs. Same trap as
    // the simple `await Promise.resolve()` shape because the suspension
    // happens on a microtask boundary in both cases.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/chained"))
        .respond_with(ResponseTemplate::new(200).set_body_string("chain-ok"))
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/chained", server.uri());
    let out = engine
        .eval(&format!(
            r#"
            (async () => {{
                const pre = await Promise.resolve("seed").then(s => s.toUpperCase());
                const r = await fetch({url:?});
                const body = await r.text();
                return pre + "/" + body;
            }})()
            "#,
            url = url,
        ))
        .expect("eval");
    assert_eq!(out.value, "SEED/chain-ok");
}

// ===== FetchMode is exported =====

#[test]
fn fetch_mode_is_exported() {
    // Compile-time check that FetchMode is reachable as a public
    // type for downstream callers. The `_` binding is enough.
    let _: Option<FetchMode> = None;
}
