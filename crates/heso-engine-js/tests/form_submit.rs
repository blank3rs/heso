//! Integration tests for the PR-1 form-submit surface:
//! `JsSession::submit` actually issues HTTP requests per WHATWG HTML
//! §4.10.22 — the bug `agent regression testing` filed as the single biggest
//! gap for write-shaped agent workloads.
//!
//! These tests use `wiremock::MockServer` for localhost HTTP exchanges
//! so the workspace `cargo test` stays hermetic. Each test:
//! 1. Stands up a mock server with the route(s) the form should hit.
//! 2. Builds a `JsEngine` with `FetchMode::Live` pointed at the same
//!    `reqwest::Client` the workspace uses (so the existing fetch
//!    integration tests can serve as a sanity model).
//! 3. Opens a `JsSession` on hand-written HTML and calls
//!    `session.submit("#f")`.
//! 4. Asserts both the JSON outcome from `submit` AND what the mock
//!    server received (method, content-type, body content).
//!
//! The "live" variant against `httpbin.org/post` lives in
//! `form_submit_live.rs` (gated by `#[ignore]` because the workspace
//! test run shouldn't hit the public internet).

use std::sync::Arc;

use heso_engine_js::{JsEngine, JsSession, ScriptFetchPolicy};
use url::Url;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Build a fresh `reqwest::Client`. Same shape as
/// `fetch_integration.rs::shared_client`.
fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-tests/0.0.1")
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()
            .expect("client builds"),
    )
}

/// Build a JS engine in `FetchMode::Live` with the current tokio
/// handle. Opens a `JsSession` on `html` at `url`.
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
// urlencoded POST round-trip
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_post_urlencoded_sends_correct_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .and(header(
            "content-type",
            "application/x-www-form-urlencoded",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><p id=\"r\">ok</p></body></html>",
        ))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="custname" value="Jane Doe">
            <input type="text" name="comments" value="hello &amp; world">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");

    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(outcome.value["method"], serde_json::json!("POST"));
    assert_eq!(outcome.value["responseStatus"], serde_json::json!(200));

    // Inspect the request the mock server received.
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1, "expected exactly one request");
    let req = &reqs[0];
    let body = String::from_utf8_lossy(&req.body).into_owned();
    // The HTML entity decodes to `hello & world`; the urlencoded
    // serialization should encode the space as `+` and the `&` as `%26`.
    assert!(
        body.contains("custname=Jane+Doe"),
        "body missing custname: {body}"
    );
    assert!(
        body.contains("comments=hello+%26+world"),
        "body missing comments: {body}"
    );

    // The session URL updates to the response URL.
    assert_eq!(sess.url().as_str(), format!("{}/echo", server.uri()));
}

// =====================================================================
// GET method serializes to query string
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_get_serializes_entries_into_query_string() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/search", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="GET" action="{action_url}">
            <input type="search" name="q" value="cats and dogs">
            <button type="submit">Go</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(outcome.value["method"], serde_json::json!("GET"));

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    // The mock server records the request URL post-redirect; the
    // path component carries the query.
    let url = req.url.to_string();
    assert!(url.contains("/search?q=cats+and+dogs"), "got URL: {url}");
    // GET requests must NOT have a body.
    assert!(
        req.body.is_empty(),
        "GET request must have empty body, got {} bytes",
        req.body.len()
    );
}

// =====================================================================
// GET with existing query — submission replaces it
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_get_replaces_existing_query_on_action() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/q"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    // action URL carries `?stale=1`; spec says the form data REPLACES
    // any existing query when submitting GET.
    let action_url = format!("{}/q?stale=1", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="get" action="{action_url}">
            <input type="text" name="fresh" value="yes">
            <button type="submit">Go</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let url = reqs[0].url.to_string();
    assert!(url.contains("/q?fresh=yes"), "got URL: {url}");
    assert!(!url.contains("stale"), "stale=1 should be gone: {url}");
}

// =====================================================================
// multipart POST round-trip
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_post_multipart_sends_correct_parts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/upload", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}" enctype="multipart/form-data">
            <input type="text" name="title" value="my doc">
            <input type="text" name="body" value="some body text">
            <button type="submit">Upload</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["enctype"],
        serde_json::json!("multipart/form-data")
    );

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    // Content-Type should be multipart/form-data with a boundary
    // chosen by reqwest. We check the prefix and that the boundary
    // appears in the body.
    let ct = req
        .headers
        .get("content-type")
        .map(|v| v.to_str().unwrap_or(""))
        .unwrap_or("");
    assert!(
        ct.starts_with("multipart/form-data; boundary="),
        "expected multipart content-type, got: {ct}"
    );
    let body = String::from_utf8_lossy(&req.body).into_owned();
    assert!(
        body.contains(r#"Content-Disposition: form-data; name="title""#),
        "missing title part: {body}"
    );
    assert!(body.contains("my doc"), "missing title value: {body}");
    assert!(
        body.contains(r#"Content-Disposition: form-data; name="body""#),
        "missing body part: {body}"
    );
    assert!(
        body.contains("some body text"),
        "missing body value: {body}"
    );
}

// =====================================================================
// text/plain enctype
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_post_text_plain_uses_crlf_pairs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/text"))
        .and(header("content-type", "text/plain"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/text", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}" enctype="text/plain">
            <input type="text" name="a" value="1">
            <input type="text" name="b" value="hello world">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));

    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    // text/plain serialization: each pair `name=value\r\n` with
    // no escaping. Spaces stay as spaces.
    assert_eq!(body, "a=1\r\nb=hello world\r\n");
}

// =====================================================================
// preventDefault on the submit event suppresses the request
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_preventdefault_on_form_suppresses_http() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="x" value="1">
            <button type="submit">Go</button>
        </form>
        <script>
          document.querySelector('#f').addEventListener('submit', (e) => {{
            e.preventDefault();
          }});
        </script>
        </body></html>"#,
    );
    let session_url = Url::parse(&server.uri()).unwrap();
    let mut sess = open_session(&html, session_url.clone());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(false));
    assert_eq!(outcome.value["defaultPrevented"], serde_json::json!(true));
    assert_eq!(outcome.value["reason"], serde_json::json!("default_prevented"));

    let reqs = server.received_requests().await.unwrap();
    assert!(
        reqs.is_empty(),
        "preventDefault on submit should suppress HTTP; got {} reqs",
        reqs.len()
    );
    // Session URL stays at the page we opened.
    assert_eq!(sess.url(), &session_url);
}

// =====================================================================
// preventDefault on the submit button click ALSO suppresses the request
// (real-browser cascade rule — a cancelled click's default action,
// which is the form submission, never runs)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_preventdefault_on_button_click_suppresses_http() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="x" value="1">
            <button type="submit" id="sb">Go</button>
        </form>
        <script>
          document.querySelector('#sb').addEventListener('click', (e) => {{
            e.preventDefault();
          }});
        </script>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(false));
    assert_eq!(outcome.value["defaultPrevented"], serde_json::json!(true));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// =====================================================================
// Default enctype: a form with no enctype attribute uses urlencoded
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_post_without_enctype_attribute_defaults_to_urlencoded() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/default"))
        .and(header(
            "content-type",
            "application/x-www-form-urlencoded",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/default", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="k" value="v">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["enctype"],
        serde_json::json!("application/x-www-form-urlencoded")
    );
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    assert_eq!(body, "k=v");
}

// =====================================================================
// Empty form: no inputs, no submit-typed activator-value either
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_empty_form_sends_empty_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/empty"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/empty", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    // Submit button has no name → it does not contribute. Body is
    // empty string.
    assert!(body.is_empty(), "expected empty body, got: {body:?}");
}

// =====================================================================
// Disabled fields are excluded
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_excludes_disabled_inputs() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/post"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let action_url = format!("{}/post", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="a" value="alpha">
            <input type="text" name="b" value="beta" disabled>
            <input type="text" name="" value="nameless">
            <button type="submit">Go</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    assert_eq!(body, "a=alpha");
}

// =====================================================================
// Unchecked checkbox excluded; checked radio included
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_checkbox_and_radio_selection() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/post"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let action_url = format!("{}/post", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="checkbox" name="opt1" value="on" checked>
            <input type="checkbox" name="opt2" value="on">
            <input type="radio" name="color" value="red">
            <input type="radio" name="color" value="green" checked>
            <input type="radio" name="color" value="blue">
            <button type="submit">Go</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    // opt1 is checked → included. opt2 unchecked → excluded.
    // green is checked → included. red/blue → excluded.
    assert!(body.contains("opt1=on"));
    assert!(!body.contains("opt2"));
    assert!(body.contains("color=green"));
    assert!(!body.contains("color=red"));
    assert!(!body.contains("color=blue"));
}

// =====================================================================
// Relative action URL resolves against the session's current URL
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_relative_action_resolves_against_session_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/forms/submit"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    // Session URL has a trailing path segment; relative action
    // "submit" should resolve to /forms/submit.
    let session_url = Url::parse(&format!("{}/forms/", server.uri())).unwrap();
    let html = r#"<!doctype html><html><body>
        <form id="f" method="post" action="submit">
            <input type="text" name="k" value="v">
            <button type="submit">Go</button>
        </form>
        </body></html>"#;
    let mut sess = open_session(html, session_url);
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
}

// =====================================================================
// Missing action attribute uses the session URL
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_missing_action_uses_session_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/page"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let session_url = Url::parse(&format!("{}/page", server.uri())).unwrap();
    let html = r#"<!doctype html><html><body>
        <form id="f" method="post">
            <input type="text" name="k" value="v">
            <button type="submit">Go</button>
        </form>
        </body></html>"#;
    let mut sess = open_session(html, session_url);
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
}

// =====================================================================
// Response replaces the session document
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_response_replaces_session_document() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><div id=\"r\">welcome jane</div></body></html>",
        ))
        .mount(&server)
        .await;
    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="name" value="jane">
            <button type="submit">Go</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    // The session's document is now the response page; querying
    // it returns the response DOM, not the original form page.
    let body = sess
        .eval("document.querySelector('#r').textContent")
        .expect("eval ok");
    assert_eq!(body.value, serde_json::json!("welcome jane"));
}

// =====================================================================
// window.location.href reflects the response URL after submit
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_updates_window_location_href_to_response_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/landed"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><div>landing</div></body></html>",
        ))
        .mount(&server)
        .await;
    let action_url = format!("{}/landed", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="k" value="v">
            <button type="submit">Go</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    // location.href is installed as a globalThis-side property by
    // `set_base_url`; the navigate() inside submit() drives it.
    let loc = sess
        .eval("globalThis.location.href")
        .expect("eval location");
    assert_eq!(
        loc.value,
        serde_json::json!(format!("{}/landed", server.uri()))
    );
}

// =====================================================================
// Activator submit button with name contributes its value
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_button_with_name_contributes_value_only_as_activator() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/post"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let action_url = format!("{}/post", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="k" value="v">
            <button type="submit" name="action" value="save">Save</button>
            <button type="submit" name="action" value="delete">Delete</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    // The first submit button is the activator; its name/value
    // contribute, the second (non-activator) does not.
    assert!(body.contains("k=v"), "body: {body}");
    assert!(body.contains("action=save"), "body: {body}");
    assert!(!body.contains("action=delete"), "body: {body}");
}

// =====================================================================
// Engine without fetch client (no_fetch_client outcome)
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_without_fetch_client_returns_no_fetch_client() {
    // JsEngine::new() builds an engine without a fetch client; the
    // submit path should report "no_fetch_client" without panicking.
    let engine = JsEngine::new().expect("engine builds");
    let html = r#"<!doctype html><html><body>
        <form id="f" method="post" action="/x">
            <input type="text" name="k" value="v">
            <button type="submit">Go</button>
        </form>
        </body></html>"#;
    let (mut sess, _) =
        JsSession::open_on_engine(engine, html, Url::parse("https://example.com/").unwrap(),
            ScriptFetchPolicy::default())
            .expect("session opens");
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(false));
    assert_eq!(
        outcome.value["reason"],
        serde_json::json!("no_fetch_client")
    );
}

// =====================================================================
// Selector miss
// =====================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_unmatched_selector_reports_no_form() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let html = r#"<!doctype html><html><body><div>nothing here</div></body></html>"#;
    let mut sess = open_session(html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#nope").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(false));
    assert_eq!(outcome.value["matched"], serde_json::json!(false));
    assert_eq!(outcome.value["reason"], serde_json::json!("no_form"));
    assert!(server.received_requests().await.unwrap().is_empty());
}

// =====================================================================
// PR-X1: --field NAME=VALUE one-shot — `submit_with_fields` sets named
// inputs before serializing, response body is in the outcome, parsed
// JSON exposed when content-type is `application/json`, file inputs
// are silently skipped. Mirrors the agent regression testing Task R2 / F2
// failure modes.
// =====================================================================

/// Helper: build `(name, value)` overrides from `&[(name, value)]`
/// string-literal pairs without all the `.to_owned()` noise at the
/// call site.
fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_with_fields_overrides_default_input_value() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    // The form's default value for `custname` is "DEFAULT" — the
    // override should replace it before the entry list is serialized.
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="custname" value="DEFAULT">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let overrides = fields(&[("custname", "Jane Doe")]);
    let outcome = sess
        .submit_with_fields("#f", &overrides)
        .expect("submit ok");

    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["fieldsApplied"],
        serde_json::json!(["custname"])
    );

    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    // The override won — body contains the supplied value, not the
    // DOM default.
    assert!(
        body.contains("custname=Jane+Doe"),
        "body should carry override, got: {body}"
    );
    assert!(
        !body.contains("DEFAULT"),
        "DEFAULT should be gone, got: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_with_fields_handles_multiple_overrides() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="custname" value="">
            <input type="text" name="custemail" value="">
            <textarea name="comments"></textarea>
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let overrides = fields(&[
        ("custname", "Jane Doe"),
        ("custemail", "j@x.com"),
        ("comments", "hello world"),
    ]);
    let outcome = sess
        .submit_with_fields("#f", &overrides)
        .expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    let applied = outcome.value["fieldsApplied"]
        .as_array()
        .expect("fieldsApplied is an array");
    assert_eq!(applied.len(), 3, "fieldsApplied: {applied:?}");

    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    assert!(body.contains("custname=Jane+Doe"), "{body}");
    assert!(body.contains("custemail=j%40x.com"), "{body}");
    assert!(body.contains("comments=hello+world"), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_with_fields_skips_missing_names_silently() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="real" value="">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let overrides = fields(&[("real", "hi"), ("ghost", "nope")]);
    let outcome = sess
        .submit_with_fields("#f", &overrides)
        .expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    // `real` was applied; `ghost` is in skipped with reason=no_match.
    assert_eq!(
        outcome.value["fieldsApplied"],
        serde_json::json!(["real"])
    );
    let skipped = outcome.value["fieldsSkipped"]
        .as_array()
        .expect("fieldsSkipped");
    assert_eq!(skipped.len(), 1, "skipped: {skipped:?}");
    assert_eq!(skipped[0]["name"], serde_json::json!("ghost"));
    assert_eq!(skipped[0]["reason"], serde_json::json!("no_match"));
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_with_fields_file_input_is_skipped() {
    // PR-X4 territory: file inputs can't have their value set via a
    // string today. We record them in `fieldsSkipped` with reason
    // `"file_input"` so an agent / CLI can warn the user.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/upload", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}" enctype="multipart/form-data">
            <input type="text" name="title" value="">
            <input type="file" name="upload">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let overrides = fields(&[("title", "agent-x1"), ("upload", "/etc/passwd")]);
    let outcome = sess
        .submit_with_fields("#f", &overrides)
        .expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["fieldsApplied"],
        serde_json::json!(["title"])
    );
    let skipped = outcome.value["fieldsSkipped"]
        .as_array()
        .expect("fieldsSkipped");
    assert_eq!(skipped.len(), 1, "skipped: {skipped:?}");
    assert_eq!(skipped[0]["name"], serde_json::json!("upload"));
    assert_eq!(skipped[0]["reason"], serde_json::json!("file_input"));

    let reqs = server.received_requests().await.unwrap();
    // The upload field is part of the multipart body but carries no
    // bytes (PR-1's existing limit) — we just verify that the
    // override didn't leak `/etc/passwd` into the upload value.
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    assert!(body.contains(r#"name="title""#), "{body}");
    assert!(body.contains("agent-x1"), "{body}");
    assert!(!body.contains("/etc/passwd"), "must not leak: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_with_fields_overrides_radio_and_checkbox() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="radio" name="color" value="red">
            <input type="radio" name="color" value="green" checked>
            <input type="radio" name="color" value="blue">
            <input type="checkbox" name="agree" value="yes">
            <button type="submit">Go</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    // Switch color from `green` (default-checked) to `blue`; also
    // check the `agree` checkbox.
    let overrides = fields(&[("color", "blue"), ("agree", "yes")]);
    let outcome = sess
        .submit_with_fields("#f", &overrides)
        .expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    let reqs = server.received_requests().await.unwrap();
    let body = String::from_utf8_lossy(&reqs[0].body).into_owned();
    assert!(body.contains("color=blue"), "body: {body}");
    assert!(!body.contains("color=green"), "body: {body}");
    assert!(body.contains("agree=yes"), "body: {body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_response_body_is_in_outcome_for_urlencoded_post() {
    // The submit outcome now carries `responseBody` and
    // `responseContentType` so an agent can observe what the server
    // echoed back — matches agent regression testing task R2's second
    // gap ("no body field, no echo").
    //
    // We use `set_body_raw(body, mime)` (not `set_body_string` then
    // `insert_header`) so wiremock writes our exact content-type
    // instead of the default `text/plain` it picks for string bodies.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("you sent: k=v", "text/plain; charset=utf-8"),
        )
        .mount(&server)
        .await;

    let action_url = format!("{}/echo", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="k" value="v">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["responseBody"],
        serde_json::json!("you sent: k=v")
    );
    assert_eq!(
        outcome.value["responseContentType"],
        serde_json::json!("text/plain; charset=utf-8")
    );
    assert_eq!(
        outcome.value["responseBodyTruncated"],
        serde_json::json!(false)
    );
    // Not JSON → no responseJson field.
    assert!(outcome.value.get("responseJson").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_response_json_is_parsed_when_content_type_is_json() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                r#"{"ok": true, "id": 42, "echo": {"k": "v"}}"#,
                "application/json",
            ),
        )
        .mount(&server)
        .await;
    let action_url = format!("{}/api", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <input type="text" name="k" value="v">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    // responseJson is the parsed value — agent can drill into it
    // without an extra JSON.parse round-trip.
    assert_eq!(outcome.value["responseJson"]["ok"], serde_json::json!(true));
    assert_eq!(outcome.value["responseJson"]["id"], serde_json::json!(42));
    assert_eq!(
        outcome.value["responseJson"]["echo"]["k"],
        serde_json::json!("v")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_response_json_handles_vendor_plus_json_suffix() {
    // `application/vnd.api+json` is a "structured syntax suffix" —
    // per IANA, the trailing `+json` declares the payload as JSON.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/jsonapi"))
        .respond_with(
            ResponseTemplate::new(201).set_body_raw(
                r#"{"data": {"id": "1", "type": "agents"}}"#,
                "application/vnd.api+json",
            ),
        )
        .mount(&server)
        .await;
    let action_url = format!("{}/jsonapi", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["responseJson"]["data"]["type"],
        serde_json::json!("agents")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_response_json_omitted_when_body_is_not_valid_json() {
    // Server lies in its content-type — JSON parse should fail and
    // the field should be omitted rather than erroring.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/lie"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw("this is definitely not json", "application/json"),
        )
        .mount(&server)
        .await;
    let action_url = format!("{}/lie", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["responseBody"],
        serde_json::json!("this is definitely not json")
    );
    // The body was unparseable — responseJson is absent (not null,
    // not an error — just missing). Agents that test for the field
    // can fall back to responseBody.
    assert!(
        outcome.value.get("responseJson").is_none(),
        "responseJson should be absent on parse failure, got: {:?}",
        outcome.value
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_response_body_is_truncated_above_64_kib() {
    // Build a response body larger than the 64 KiB cap; the outcome
    // should carry a truncated copy + the truncated flag.
    let big_payload = "X".repeat(70 * 1024); // 71680 bytes > 65536
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(big_payload.clone(), "text/plain"))
        .mount(&server)
        .await;
    let action_url = format!("{}/big", server.uri());
    let html = format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}">
            <button type="submit">Send</button>
        </form>
        </body></html>"#,
    );
    let mut sess = open_session(&html, Url::parse(&server.uri()).unwrap());
    let outcome = sess.submit("#f").expect("submit ok");
    assert_eq!(outcome.value["submitted"], serde_json::json!(true));
    assert_eq!(
        outcome.value["responseBodyTruncated"],
        serde_json::json!(true)
    );
    let body_field = outcome.value["responseBody"]
        .as_str()
        .expect("responseBody is a string");
    assert!(
        body_field.len() <= 64 * 1024,
        "truncated body should be ≤ 64 KiB, got {} bytes",
        body_field.len()
    );
    assert!(
        body_field.len() >= 64 * 1024 - 4,
        "truncated body should be very close to the cap, got {} bytes",
        body_field.len()
    );
}

// =====================================================================
// Multipart cassette: distinct field values produce distinct
// request_body_b64 keys (the boundary is deterministic across calls)
// =====================================================================

/// HTML scaffold for a multipart form with one text field that varies
/// per test setup.
fn multipart_form_html(action_url: &str, field_value: &str) -> String {
    format!(
        r#"<!doctype html><html><body>
        <form id="f" method="post" action="{action_url}" enctype="multipart/form-data">
            <input type="text" name="title" value="{field_value}">
            <button type="submit">Upload</button>
        </form>
        </body></html>"#,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn multipart_request_body_uses_deterministic_boundary() {
    // Two POSTs to the same URL with different field values must land
    // in the cassette as two distinguishable records — `request_body`
    // is the disambiguator, so the wire bytes have to differ. A
    // random per-request boundary would defeat this: the bytes would
    // differ on the boundary alone, which still distinguishes records,
    // but more critically replay can't find them again. Pinning the
    // boundary makes both record-time and lookup-time bytes line up.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let action_url = format!("{}/upload", server.uri());

    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    let cassette = std::sync::Arc::new(std::sync::Mutex::new(
        heso_engine_fetch::Cassette::new(),
    ));

    // First POST: title=alpha.
    {
        let engine = JsEngine::new_with_recording_cassette(
            0,
            client.clone(),
            rt.clone(),
            cassette.clone(),
            None,
        )
        .expect("engine builds");
        let html = multipart_form_html(&action_url, "alpha");
        let (mut sess, _) = JsSession::open_on_engine(
            engine,
            &html,
            Url::parse(&server.uri()).unwrap(),
            ScriptFetchPolicy::default(),
        )
        .expect("session opens");
        sess.submit("#f").expect("alpha submit ok");
    }
    // Second POST: title=beta.
    {
        let engine = JsEngine::new_with_recording_cassette(
            0,
            client.clone(),
            rt.clone(),
            cassette.clone(),
            None,
        )
        .expect("engine builds");
        let html = multipart_form_html(&action_url, "beta");
        let (mut sess, _) = JsSession::open_on_engine(
            engine,
            &html,
            Url::parse(&server.uri()).unwrap(),
            ScriptFetchPolicy::default(),
        )
        .expect("session opens");
        sess.submit("#f").expect("beta submit ok");
    }

    let recorded = cassette.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2, "expected two recorded multipart POSTs");
    let a = &recorded.records[0];
    let b = &recorded.records[1];
    assert_eq!(a.method, "POST");
    assert_eq!(b.method, "POST");
    assert_eq!(a.url, action_url);
    assert_eq!(b.url, action_url);
    assert_ne!(
        a.request_body_b64, b.request_body_b64,
        "distinct field values must produce distinct request_body bytes; \
         got both records carrying body `{}`",
        a.request_body_b64
    );
    assert!(
        !a.request_body_b64.is_empty(),
        "multipart cassette record must carry the wire bytes, not an empty body"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn multipart_cassette_replay_matches_request_body() {
    // Record two distinct multipart POSTs against the live mock, then
    // replay each one and verify the response page came from the
    // matching record (not the other one). Confirms the lookup walk
    // `(method, url, request_body)` distinguishes records that share
    // method + URL.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                "<!doctype html><html><body><div id=\"r\">response-A</div></body></html>",
            ),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                "<!doctype html><html><body><div id=\"r\">response-B</div></body></html>",
            ),
        )
        .mount(&server)
        .await;
    let action_url = format!("{}/echo", server.uri());

    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    let cassette = std::sync::Arc::new(std::sync::Mutex::new(
        heso_engine_fetch::Cassette::new(),
    ));

    for value in ["X", "Y"] {
        let engine = JsEngine::new_with_recording_cassette(
            0,
            client.clone(),
            rt.clone(),
            cassette.clone(),
            None,
        )
        .expect("engine builds");
        let html = multipart_form_html(&action_url, value);
        let (mut sess, _) = JsSession::open_on_engine(
            engine,
            &html,
            Url::parse(&server.uri()).unwrap(),
            ScriptFetchPolicy::default(),
        )
        .expect("session opens");
        sess.submit("#f").expect("record submit ok");
    }

    let recorded = cassette.lock().unwrap().clone();
    assert_eq!(recorded.len(), 2, "expected two recorded POSTs");
    let response_a = String::from_utf8(
        heso_engine_fetch::Cassette::decode_response_body(&recorded.records[0]).expect("decode 0"),
    )
    .expect("utf8");
    let response_b = String::from_utf8(
        heso_engine_fetch::Cassette::decode_response_body(&recorded.records[1]).expect("decode 1"),
    )
    .expect("utf8");
    assert!(
        response_a.contains("response-A"),
        "first record should carry response-A, got: {response_a}"
    );
    assert!(
        response_b.contains("response-B"),
        "second record should carry response-B, got: {response_b}"
    );

    // Replay each value against the same cassette and confirm the
    // matching record's body comes back through the session.
    let cassette_ro = std::sync::Arc::new(recorded);
    drop(server);

    for (value, expected_marker) in [("X", "response-A"), ("Y", "response-B")] {
        let engine = JsEngine::new_with_replaying_cassette(0, cassette_ro.clone(), None)
            .expect("engine builds");
        let html = multipart_form_html(&action_url, value);
        let (mut sess, _) = JsSession::open_on_engine(
            engine,
            &html,
            Url::parse(&action_url).unwrap(),
            ScriptFetchPolicy::default(),
        )
        .expect("session opens");
        let outcome = sess.submit("#f").expect("replay submit ok");
        let body = outcome.value["responseBody"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        assert!(
            body.contains(expected_marker),
            "replay for value `{value}` should match `{expected_marker}`; got: {body}"
        );
    }
}
