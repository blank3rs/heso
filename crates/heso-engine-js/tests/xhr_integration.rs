//! Integration tests for `XMLHttpRequest` — closes bug-report 03 P1
//! / bug-report 01 P0 cluster. Every analytics SDK on a top-100 site
//! crashes without it.
//!
//! Uses `wiremock` to spin up a localhost mock server (same shape as
//! `fetch_integration.rs` / `current_script.rs`).

use std::sync::Arc;

use heso_engine_js::JsEngine;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-xhr-tests/0.0.1")
            .build()
            .expect("client builds"),
    )
}

fn engine_with_fetch() -> JsEngine {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    JsEngine::new_with_fetch(client, rt).expect("engine builds")
}

// ===== Constructor surface =====================================

#[tokio::test(flavor = "multi_thread")]
async fn xhr_constructor_is_a_function() {
    let engine = engine_with_fetch();
    let out = engine.eval("typeof XMLHttpRequest").expect("eval");
    assert_eq!(out.value, "function");
}

#[tokio::test(flavor = "multi_thread")]
async fn xhr_instance_is_instance_of_xml_http_request() {
    let engine = engine_with_fetch();
    let out = engine
        .eval("(new XMLHttpRequest()) instanceof XMLHttpRequest")
        .expect("eval");
    assert_eq!(out.value, true);
}

#[tokio::test(flavor = "multi_thread")]
async fn xhr_ready_state_constants_exposed() {
    let engine = engine_with_fetch();
    let out = engine
        .eval(
            r#"
            ({
                UNSENT: XMLHttpRequest.UNSENT,
                OPENED: XMLHttpRequest.OPENED,
                HEADERS_RECEIVED: XMLHttpRequest.HEADERS_RECEIVED,
                LOADING: XMLHttpRequest.LOADING,
                DONE: XMLHttpRequest.DONE,
            })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["UNSENT"], 0);
    assert_eq!(out.value["OPENED"], 1);
    assert_eq!(out.value["HEADERS_RECEIVED"], 2);
    assert_eq!(out.value["LOADING"], 3);
    assert_eq!(out.value["DONE"], 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn xhr_initial_ready_state_is_zero() {
    let engine = engine_with_fetch();
    let out = engine
        .eval("(new XMLHttpRequest()).readyState")
        .expect("eval");
    assert_eq!(out.value, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn xhr_open_advances_ready_state_to_opened() {
    let engine = engine_with_fetch();
    let out = engine
        .eval(
            r#"
            const x = new XMLHttpRequest();
            x.open("GET", "https://example.com/");
            x.readyState
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 1);
}

// ===== End-to-end GET ==========================================

#[tokio::test(flavor = "multi_thread")]
async fn xhr_get_resolves_with_onload() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/hello"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hello world"))
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/hello", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const x = new XMLHttpRequest();
            x.onload = function() {{
                globalThis.__status = x.status;
                globalThis.__text = x.responseText;
                globalThis.__readyState = x.readyState;
            }};
            x.open("GET", "{url}");
            x.send();
            "#
        ))
        .expect("schedule xhr");
    let out = engine
        .eval("[globalThis.__status, globalThis.__text, globalThis.__readyState]")
        .expect("observe");
    assert_eq!(out.value[0], 200);
    assert_eq!(out.value[1], "hello world");
    assert_eq!(out.value[2], 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn xhr_onreadystatechange_fires_at_each_transition() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/r"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/r", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const x = new XMLHttpRequest();
            globalThis.__states = [];
            x.onreadystatechange = function() {{
                globalThis.__states.push(x.readyState);
            }};
            x.open("GET", "{url}");
            x.send();
            "#
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__states").expect("observe");
    // States: OPENED(1) at open, then HEADERS_RECEIVED(2),
    // LOADING(3), DONE(4) at drain.
    let arr = out.value.as_array().expect("array");
    assert!(arr.contains(&serde_json::json!(1)), "missing OPENED");
    assert!(arr.contains(&serde_json::json!(2)), "missing HEADERS_RECEIVED");
    assert!(arr.contains(&serde_json::json!(3)), "missing LOADING");
    assert!(arr.contains(&serde_json::json!(4)), "missing DONE");
}

#[tokio::test(flavor = "multi_thread")]
async fn xhr_response_text_holds_body_text() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/t"))
        .respond_with(ResponseTemplate::new(200).set_body_string("payload-text"))
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/t", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const x = new XMLHttpRequest();
            x.onload = function() {{ globalThis.__rt = x.responseText; }};
            x.open("GET", "{url}");
            x.send();
            "#
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__rt").expect("observe");
    assert_eq!(out.value, "payload-text");
}

#[tokio::test(flavor = "multi_thread")]
async fn xhr_response_type_json_parses_into_object() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/j"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"key":"value"}"#))
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/j", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const x = new XMLHttpRequest();
            x.responseType = 'json';
            x.onload = function() {{ globalThis.__r = x.response; }};
            x.open("GET", "{url}");
            x.send();
            "#
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__r").expect("observe");
    assert_eq!(out.value, serde_json::json!({"key": "value"}));
}

// ===== POST with body + headers ==========================================

#[tokio::test(flavor = "multi_thread")]
async fn xhr_post_sends_body_and_set_request_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(|req: &Request| {
            let body = std::str::from_utf8(&req.body).unwrap_or_default();
            let ct = req
                .headers
                .get("x-test")
                .map(|v| v.to_str().unwrap_or_default().to_owned())
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_string(format!("body={body},x-test={ct}"))
        })
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/echo", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const x = new XMLHttpRequest();
            x.onload = function() {{ globalThis.__t = x.responseText; }};
            x.open("POST", "{url}");
            x.setRequestHeader("X-Test", "abc");
            x.send("payload");
            "#
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__t").expect("observe");
    assert_eq!(out.value, "body=payload,x-test=abc");
}

// ===== Error / non-2xx ==========================================

#[tokio::test(flavor = "multi_thread")]
async fn xhr_404_fires_onload_with_status_404() {
    // Spec: 4xx/5xx still completes the request (onload fires); only
    // network errors call onerror.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/missing"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/missing", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const x = new XMLHttpRequest();
            x.onload = function() {{
                globalThis.__loadStatus = x.status;
                globalThis.__loadText = x.responseText;
            }};
            x.onerror = function() {{ globalThis.__error = true; }};
            x.open("GET", "{url}");
            x.send();
            "#
        ))
        .expect("schedule");
    let out = engine
        .eval(
            "[globalThis.__loadStatus, globalThis.__loadText, !!globalThis.__error]",
        )
        .expect("observe");
    assert_eq!(out.value[0], 404);
    assert_eq!(out.value[1], "not found");
    assert_eq!(out.value[2], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn xhr_network_error_fires_onerror() {
    let engine = engine_with_fetch();
    let _ = engine
        .eval(
            // 127.0.0.1:1 — TCP connection refused.
            r#"
            const x = new XMLHttpRequest();
            x.onerror = function() { globalThis.__err = true; };
            x.onload = function() { globalThis.__loaded = true; };
            x.open("GET", "http://127.0.0.1:1/nope");
            x.send();
            "#,
        )
        .expect("schedule");
    let out = engine
        .eval("[!!globalThis.__err, !!globalThis.__loaded]")
        .expect("observe");
    assert_eq!(out.value[0], true);
    assert_eq!(out.value[1], false);
}

// ===== Headers in response ==========================================

#[tokio::test(flavor = "multi_thread")]
async fn xhr_get_response_header_returns_header_value() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/h"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-custom", "the-value")
                .set_body_string("ok"),
        )
        .mount(&server)
        .await;
    let engine = engine_with_fetch();
    let url = format!("{}/h", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const x = new XMLHttpRequest();
            x.onload = function() {{
                globalThis.__h = x.getResponseHeader('x-custom');
                globalThis.__hh = x.getResponseHeader('X-CUSTOM');
            }};
            x.open("GET", "{url}");
            x.send();
            "#
        ))
        .expect("schedule");
    let out = engine
        .eval("[globalThis.__h, globalThis.__hh]")
        .expect("observe");
    assert_eq!(out.value[0], "the-value");
    assert_eq!(out.value[1], "the-value");
}

// ===== Polyfill-detection ==========================================

#[tokio::test(flavor = "multi_thread")]
async fn xhr_prototype_patching_pattern_does_not_throw() {
    // vercel.com analytics SDK was throwing
    // "Error patching XMLHttpRequest" — the proto needs to be a
    // real object you can defineProperty on.
    let engine = engine_with_fetch();
    let out = engine
        .eval(
            r#"
            try {
                const proto = XMLHttpRequest.prototype;
                const orig = proto.send;
                proto.send = function() {
                    return orig.apply(this, arguments);
                };
                "ok"
            } catch (e) {
                e.message
            }
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}
