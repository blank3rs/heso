//! Integration tests for the WHATWG `Headers` constructor installed
//! by [`heso_engine_js::web_apis::install_web_apis`]. Per
//! AGENT_FINDINGS_V2.md F1 and "Top NEW bugs" #4. The fetch path
//! already used a Headers-shaped object for response.headers; this
//! suite pins the actual constructor.

use std::sync::Arc;

use heso_engine_js::JsEngine;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-tests/0.0.1")
            .build()
            .expect("client builds"),
    )
}

fn engine_with_fetch() -> JsEngine {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    JsEngine::new_with_fetch(client, rt).expect("engine builds")
}

// =============================================================================
// Construction
// =============================================================================

#[test]
fn headers_construct_empty() {
    let out = engine()
        .eval("(new Headers()).get('any')")
        .expect("eval");
    assert!(out.value.is_null());
}

#[test]
fn headers_construct_from_record() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "Content-Type": "application/json", "X-Foo": "bar" });
            [h.get("content-type"), h.get("x-foo")]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "application/json");
    assert_eq!(out.value[1], "bar");
}

#[test]
fn headers_construct_from_array_of_pairs() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers([["X-A", "1"], ["X-B", "2"]]);
            [h.get("x-a"), h.get("x-b")]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "1");
    assert_eq!(out.value[1], "2");
}

#[test]
fn headers_construct_from_another_headers_copies_entries() {
    let out = engine()
        .eval(
            r#"
            const a = new Headers({ "X-A": "alpha" });
            const b = new Headers(a);
            [b.get("x-a"), b.get("X-A")]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "alpha");
    assert_eq!(out.value[1], "alpha");
}

// =============================================================================
// Case-insensitive get/has/set
// =============================================================================

#[test]
fn headers_get_is_case_insensitive() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "Content-Type": "text/html" });
            [h.get("content-type"), h.get("CONTENT-TYPE"), h.get("Content-Type")]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "text/html");
    assert_eq!(out.value[1], "text/html");
    assert_eq!(out.value[2], "text/html");
}

#[test]
fn headers_has_is_case_insensitive() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "X-Foo": "bar" });
            [h.has("x-foo"), h.has("X-FOO"), h.has("missing")]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, true, false]));
}

#[test]
fn headers_set_replaces_existing() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers();
            h.set("Content-Type", "text/plain");
            h.set("content-type", "application/json");
            h.get("Content-Type")
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "application/json");
}

// =============================================================================
// Duplicate combining
// =============================================================================

#[test]
fn headers_append_combines_duplicates_on_get() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers();
            h.append("Set-Cookie", "a=1");
            h.append("Set-Cookie", "b=2");
            h.get("set-cookie")
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "a=1, b=2");
}

#[test]
fn headers_set_after_append_replaces_all_duplicates() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers();
            h.append("X-A", "1");
            h.append("X-A", "2");
            h.append("X-A", "3");
            h.set("x-a", "only");
            h.get("X-A")
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "only");
}

#[test]
fn headers_delete_removes_all_matching() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "X-A": "1", "X-B": "2" });
            h.append("X-A", "extra");
            h.delete("X-A");
            [h.get("X-A"), h.get("X-B")]
            "#,
        )
        .expect("eval");
    assert!(out.value[0].is_null());
    assert_eq!(out.value[1], "2");
}

// =============================================================================
// Iteration
// =============================================================================

#[test]
fn headers_entries_returns_array_of_pairs() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "X-B": "two", "X-A": "one" });
            const e = h.entries();
            [e.length, Array.isArray(e), e[0][0], e[1][0]]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], 2);
    assert_eq!(out.value[1], true);
    // Per spec: iteration is in lexicographic order of names.
    assert_eq!(out.value[2], "x-a");
    assert_eq!(out.value[3], "x-b");
}

#[test]
fn headers_keys_returns_lowercase_names() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "Content-Type": "text/html" });
            const k = h.keys();
            [k.length, k[0]]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], 1);
    assert_eq!(out.value[1], "content-type");
}

#[test]
fn headers_values_returns_values() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "X-Foo": "bar" });
            const v = h.values();
            [v.length, v[0]]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], 1);
    assert_eq!(out.value[1], "bar");
}

#[test]
fn headers_for_each_walks_entries() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "X-A": "1", "X-B": "2" });
            const seen = [];
            h.forEach((value, name) => { seen.push(`${name}=${value}`); });
            seen
            "#,
        )
        .expect("eval");
    let arr = out.value.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().any(|v| v == "x-a=1"));
    assert!(arr.iter().any(|v| v == "x-b=2"));
}

#[test]
fn headers_supports_for_of_iteration() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "X-A": "1", "X-B": "2" });
            const out = [];
            for (const [name, value] of h) {
                out.push(`${name}:${value}`);
            }
            out
            "#,
        )
        .expect("eval");
    let arr = out.value.as_array().expect("array");
    assert_eq!(arr.len(), 2);
    assert!(arr.iter().any(|v| v == "x-a:1"));
}

#[test]
fn headers_value_trimming_normalizes_whitespace() {
    let out = engine()
        .eval(
            r#"
            const h = new Headers({ "X-Foo": "  bar  " });
            h.get("x-foo")
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "bar");
}

#[test]
fn headers_constructor_is_function() {
    let out = engine().eval("typeof Headers").expect("eval");
    assert_eq!(out.value, "function");
}

// =============================================================================
// Integration with fetch
// =============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn fetch_with_headers_instance_sends_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/hi"))
        .respond_with(|req: &Request| {
            let x_custom = req
                .headers
                .get("x-custom")
                .map(|v| v.to_str().unwrap_or("").to_owned())
                .unwrap_or_default();
            let x_token = req
                .headers
                .get("x-token")
                .map(|v| v.to_str().unwrap_or("").to_owned())
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "x_custom": x_custom,
                "x_token": x_token,
            }))
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/hi", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const h = new Headers({{ "X-Custom": "agent-shaped" }});
            h.append("X-Token", "abc123");
            fetch({url:?}, {{ headers: h }}).then(r => r.json()).then(j => {{
                globalThis.__got = j;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__got").expect("observe");
    assert_eq!(out.value["x_custom"], "agent-shaped");
    assert_eq!(out.value["x_token"], "abc123");
}
