//! Live integration tests for the PR-1 form-submit surface against
//! `httpbin.org`. Gated by `#[ignore]` because the workspace
//! `cargo test` should not hit the public internet — run manually
//! with `cargo test --test form_submit_live -- --ignored --nocapture`.
//!
//! These exist to catch regressions that purely in-process mocks can
//! miss: real TLS, real redirects, real Content-Type negotiation,
//! actual multipart boundary handling by a server we don't control.
//! `httpbin.org/post` echoes the request body back as JSON, so we
//! can read the `form` (urlencoded / multipart) or `data` (text/plain)
//! field to confirm what hit the wire.

use std::sync::Arc;

use heso_engine_js::{JsEngine, JsSession, ScriptFetchPolicy};
use url::Url;

fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-tests/0.0.1")
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()
            .expect("client builds"),
    )
}

fn open_session(html: &str, url: Url) -> JsSession {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    let engine = JsEngine::new_with_fetch(client, rt).expect("engine builds");
    let (sess, _outcome) =
        JsSession::open_on_engine(engine, html, url, ScriptFetchPolicy::default())
            .expect("session opens");
    sess
}

// =====================================================================
// urlencoded POST to httpbin.org/post
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits public internet — run with --ignored"]
async fn submit_urlencoded_post_to_httpbin_echoes_form_field() {
    // Page that posts to httpbin.org/post — the same shape
    // `AGENT_FINDINGS.md` task 3 used to file the bug, so passing
    // this test is the "AGENT_FINDINGS.md task 3 unblocked" check.
    let html = r#"<!doctype html><html><body>
        <form id="f" method="post" action="https://httpbin.org/post">
            <input type="text" name="custname" value="Jane Doe">
            <input type="text" name="custtel" value="555-1234">
            <input type="text" name="custemail" value="jane@example.com">
            <button type="submit">Submit order</button>
        </form>
        </body></html>"#;
    let mut sess = open_session(html, Url::parse("https://example.com/").unwrap());
    let outcome = sess.submit("#f").expect("submit ok");

    assert_eq!(
        outcome.value["submitted"], serde_json::json!(true),
        "outcome: {:?}",
        outcome.value
    );
    assert_eq!(outcome.value["responseStatus"], serde_json::json!(200));

    // httpbin echoes the parsed form fields under `.form` (because
    // we sent urlencoded). Re-parse the response body via the
    // engine's `JSON.parse` so we don't pull a JSON dep into the
    // test binary just for this one assertion.
    let echo = sess
        .eval("JSON.parse(document.documentElement.textContent)")
        .expect("eval response body");
    let form = echo
        .value
        .get("form")
        .expect("httpbin response missing `form` key — wrong enctype?");
    assert_eq!(
        form.get("custname").and_then(|v| v.as_str()),
        Some("Jane Doe"),
        "echoed form: {form:?}"
    );
    assert_eq!(
        form.get("custtel").and_then(|v| v.as_str()),
        Some("555-1234")
    );
    assert_eq!(
        form.get("custemail").and_then(|v| v.as_str()),
        Some("jane@example.com")
    );
}

// =====================================================================
// multipart POST to httpbin.org/post
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits public internet — run with --ignored"]
async fn submit_multipart_post_to_httpbin_echoes_form_field() {
    let html = r#"<!doctype html><html><body>
        <form id="f" method="post" action="https://httpbin.org/post" enctype="multipart/form-data">
            <input type="text" name="custname" value="Jane Doe">
            <input type="text" name="comments" value="hello multipart">
            <button type="submit">Submit</button>
        </form>
        </body></html>"#;
    let mut sess = open_session(html, Url::parse("https://example.com/").unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["enctype"],
        serde_json::json!("multipart/form-data")
    );
    let echo = sess
        .eval("JSON.parse(document.documentElement.textContent)")
        .expect("eval response body");
    let form = echo.value.get("form").expect("missing form field");
    assert_eq!(
        form.get("custname").and_then(|v| v.as_str()),
        Some("Jane Doe")
    );
    assert_eq!(
        form.get("comments").and_then(|v| v.as_str()),
        Some("hello multipart")
    );
}

// =====================================================================
// GET to httpbin.org/get
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits public internet — run with --ignored"]
async fn submit_get_to_httpbin_echoes_args() {
    let html = r#"<!doctype html><html><body>
        <form id="f" method="get" action="https://httpbin.org/get">
            <input type="search" name="q" value="cats and dogs">
            <button type="submit">Search</button>
        </form>
        </body></html>"#;
    let mut sess = open_session(html, Url::parse("https://example.com/").unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(outcome.value["method"], serde_json::json!("GET"));
    let echo = sess
        .eval("JSON.parse(document.documentElement.textContent)")
        .expect("eval response body");
    let args = echo.value.get("args").expect("missing args field");
    assert_eq!(
        args.get("q").and_then(|v| v.as_str()),
        Some("cats and dogs")
    );
    // httpbin echoes the request URL — should contain our query.
    let url = echo.value.get("url").and_then(|v| v.as_str()).unwrap_or("");
    assert!(url.contains("q=cats+and+dogs"), "url: {url}");
}
