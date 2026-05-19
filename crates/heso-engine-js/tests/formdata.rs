//! Integration tests for the WHATWG `FormData` constructor installed
//! by [`heso_engine_js::web_apis::install_web_apis`]. Per
//! agent regression testing F1 and "Top NEW bugs" #4. Includes the
//! end-to-end `fetch(url, {body: new FormData()})` multipart path
//! against wiremock plus an `#[ignore]`-gated live httpbin upload.

use std::sync::Arc;

use heso_engine_js::{JsEngine, JsSession};
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

fn shared_client() -> Arc<reqwest::Client> {
    Arc::new(
        reqwest::Client::builder()
            .user_agent("heso-engine-js-tests/0.0.1")
            .redirect(reqwest::redirect::Policy::limited(20))
            .build()
            .expect("client builds"),
    )
}

fn engine_with_fetch() -> JsEngine {
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    JsEngine::new_with_fetch(client, rt).expect("engine builds")
}

fn page_url() -> Url {
    Url::parse("https://example.com/").unwrap()
}

// =============================================================================
// Construction & basic ops
// =============================================================================

#[test]
fn formdata_construct_empty_no_entries() {
    let out = engine()
        .eval(
            r#"
            const f = new FormData();
            const all = [];
            for (const [k, v] of f) all.push(k);
            all.length
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 0);
}

#[test]
fn formdata_append_and_get() {
    let out = engine()
        .eval(
            r#"
            const f = new FormData();
            f.append("name", "Jane Doe");
            f.append("age", "30");
            [f.get("name"), f.get("age"), f.get("missing")]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "Jane Doe");
    assert_eq!(out.value[1], "30");
    assert!(out.value[2].is_null());
}

#[test]
fn formdata_get_all_returns_in_order() {
    let out = engine()
        .eval(
            r#"
            const f = new FormData();
            f.append("tag", "a");
            f.append("tag", "b");
            f.append("tag", "c");
            f.getAll("tag")
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["a", "b", "c"]));
}

#[test]
fn formdata_has_checks_presence() {
    let out = engine()
        .eval(
            r#"
            const f = new FormData();
            f.append("present", "y");
            [f.has("present"), f.has("absent")]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, false]));
}

#[test]
fn formdata_delete_removes_all_matching() {
    let out = engine()
        .eval(
            r#"
            const f = new FormData();
            f.append("k", "1");
            f.append("k", "2");
            f.append("other", "z");
            f.delete("k");
            [f.has("k"), f.get("other")]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], false);
    assert_eq!(out.value[1], "z");
}

#[test]
fn formdata_set_replaces_all_existing() {
    let out = engine()
        .eval(
            r#"
            const f = new FormData();
            f.append("k", "1");
            f.append("k", "2");
            f.set("k", "only");
            f.getAll("k")
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["only"]));
}

#[test]
fn formdata_constructor_is_function() {
    let out = engine().eval("typeof FormData").expect("eval");
    assert_eq!(out.value, "function");
}

// =============================================================================
// Blob/File values
// =============================================================================

#[test]
fn formdata_append_blob_value_returns_file_on_get() {
    let out = engine()
        .eval(
            r#"
            const b = new Blob(["payload"], { type: "text/plain" });
            const f = new FormData();
            f.append("upload", b, "report.txt");
            const v = f.get("upload");
            ({
                isFile: v instanceof File,
                name: v.name,
                size: v.size,
                type: v.type
            })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["isFile"], true);
    assert_eq!(out.value["name"], "report.txt");
    assert_eq!(out.value["size"], 7);
    assert_eq!(out.value["type"], "text/plain");
}

#[test]
fn formdata_append_file_preserves_name() {
    let out = engine()
        .eval(
            r#"
            const f = new File(["data"], "doc.bin", { type: "application/octet-stream" });
            const fd = new FormData();
            fd.append("file", f);
            const v = fd.get("file");
            [v instanceof File, v.name, v.size]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], true);
    assert_eq!(out.value[1], "doc.bin");
    assert_eq!(out.value[2], 4);
}

#[test]
fn formdata_iteration_yields_string_or_file_values() {
    let out = engine()
        .eval(
            r#"
            const f = new FormData();
            f.append("text", "hello");
            f.append("file", new Blob(["bytes"]), "name.bin");
            const types = [];
            for (const [name, val] of f) {
                types.push(typeof val === "string" ? "str:" + name : "blob:" + name);
            }
            types
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["str:text", "blob:file"]));
}

// =============================================================================
// `new FormData(form)` populates from form element
// =============================================================================

#[test]
fn formdata_from_form_element_populates_text_inputs() {
    let html = r#"<!doctype html><html><body>
        <form id="f">
            <input type="text" name="custname" value="Jane Doe">
            <input type="email" name="email" value="jane@example.com">
            <textarea name="bio">Hello</textarea>
            <select name="country"><option value="US" selected>United States</option><option value="CA">Canada</option></select>
            <input type="submit" name="go" value="Send">
            <input type="reset" name="reset_btn" value="Reset">
            <input type="text" name="ignore_disabled" value="x" disabled>
        </form>
        </body></html>"#;
    let (sess, _) = JsSession::open(html, page_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const form = document.getElementById("f");
            const fd = new FormData(form);
            ({
                custname: fd.get("custname"),
                email: fd.get("email"),
                bio: fd.get("bio"),
                country: fd.get("country"),
                hasSubmit: fd.has("go"),
                hasReset: fd.has("reset_btn"),
                hasDisabled: fd.has("ignore_disabled")
            })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["custname"], "Jane Doe");
    assert_eq!(out.value["email"], "jane@example.com");
    assert_eq!(out.value["bio"], "Hello");
    assert_eq!(out.value["country"], "US");
    // Submit/reset buttons and disabled inputs are not included.
    assert_eq!(out.value["hasSubmit"], false);
    assert_eq!(out.value["hasReset"], false);
    assert_eq!(out.value["hasDisabled"], false);
}

#[test]
fn formdata_from_form_handles_checkbox_radio() {
    let html = r#"<!doctype html><html><body>
        <form id="f">
            <input type="checkbox" name="agree" value="yes" checked>
            <input type="checkbox" name="newsletter" value="weekly">
            <input type="radio" name="size" value="s">
            <input type="radio" name="size" value="m" checked>
            <input type="radio" name="size" value="l">
        </form>
        </body></html>"#;
    let (sess, _) = JsSession::open(html, page_url()).unwrap();
    let out = sess
        .eval(
            r#"
            const fd = new FormData(document.getElementById("f"));
            ({
                agree: fd.get("agree"),
                hasNewsletter: fd.has("newsletter"),
                size: fd.get("size")
            })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["agree"], "yes");
    assert_eq!(out.value["hasNewsletter"], false);
    assert_eq!(out.value["size"], "m");
}

#[test]
fn formdata_from_non_form_throws() {
    let html = r#"<!doctype html><html><body><div id="d"></div></body></html>"#;
    let (sess, _) = JsSession::open(html, page_url()).unwrap();
    let err = sess
        .eval(
            r#"
            new FormData(document.getElementById("d"))
            "#,
        )
        .expect_err("FormData(non-form) should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("HTMLFormElement") || msg.contains("TypeError"),
        "got: {msg}"
    );
}

// =============================================================================
// Multipart fetch integration (via wiremock)
// =============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn fetch_formdata_sends_multipart_content_type_with_boundary() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
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
                "body_len": body.len(),
            }))
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/upload", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const fd = new FormData();
            fd.append("name", "Jane");
            fd.append("comment", "hello world");
            fetch({url:?}, {{ method: "POST", body: fd }}).then(r => r.json()).then(j => {{
                globalThis.__got = j;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__got").expect("observe");
    let ct = out.value["content_type"]
        .as_str()
        .expect("content_type is string");
    // reqwest's multipart sets a multipart/form-data Content-Type with
    // a generated boundary; both must be present.
    assert!(
        ct.starts_with("multipart/form-data") && ct.contains("boundary="),
        "Content-Type must include boundary; got: {ct:?}"
    );
    // The serialized body should contain both fields.
    let body = out.value["body"].as_str().expect("body is string");
    assert!(body.contains("name=\"name\""), "missing name field: {body}");
    assert!(
        body.contains("name=\"comment\""),
        "missing comment field: {body}"
    );
    assert!(body.contains("Jane"), "missing 'Jane' value: {body}");
    assert!(
        body.contains("hello world"),
        "missing 'hello world' value: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_formdata_with_blob_part_sends_file_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/upload"))
        .respond_with(|req: &Request| {
            let body = String::from_utf8_lossy(&req.body).into_owned();
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/upload", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const fd = new FormData();
            fd.append("file", new Blob(["agent-payload"], {{ type: "text/plain" }}), "report.txt");
            fetch({url:?}, {{ method: "POST", body: fd }}).then(r => r.text()).then(t => {{
                globalThis.__body = t;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__body").expect("observe");
    let body = out.value.as_str().expect("body is string");
    // The multipart body should contain the filename, the file's MIME
    // type, and the content bytes.
    assert!(
        body.contains("filename=\"report.txt\""),
        "missing filename: {body}"
    );
    assert!(
        body.contains("Content-Type: text/plain"),
        "missing content-type for part: {body}"
    );
    assert!(
        body.contains("agent-payload"),
        "missing file content: {body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_formdata_caller_set_content_type_is_dropped() {
    // When the caller passes a Content-Type header alongside a
    // FormData body, the multipart serializer's boundary-bearing
    // Content-Type wins (because a stale CT without the boundary
    // breaks parsing on the server side).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/u"))
        .respond_with(|req: &Request| {
            let ct = req
                .headers
                .get("content-type")
                .map(|v| v.to_str().unwrap_or("").to_owned())
                .unwrap_or_default();
            ResponseTemplate::new(200).set_body_string(ct)
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/u", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const fd = new FormData();
            fd.append("x", "y");
            fetch({url:?}, {{
                method: "POST",
                body: fd,
                headers: {{ "Content-Type": "application/json" }}
            }}).then(r => r.text()).then(t => {{
                globalThis.__ct = t;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__ct").expect("observe");
    let ct = out.value.as_str().expect("ct is string");
    assert!(
        ct.starts_with("multipart/form-data") && ct.contains("boundary="),
        "expected multipart with boundary; got: {ct:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_blob_body_sets_content_type_from_blob_mime() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/data"))
        .respond_with(|req: &Request| {
            let ct = req
                .headers
                .get("content-type")
                .map(|v| v.to_str().unwrap_or("").to_owned())
                .unwrap_or_default();
            let body = String::from_utf8_lossy(&req.body).into_owned();
            ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ct": ct,
                "body": body,
            }))
        })
        .mount(&server)
        .await;

    let engine = engine_with_fetch();
    let url = format!("{}/data", server.uri());
    let _ = engine
        .eval(&format!(
            r#"
            const b = new Blob(["{{\"key\": \"value\"}}"], {{ type: "application/json" }});
            fetch({url:?}, {{ method: "POST", body: b }}).then(r => r.json()).then(j => {{
                globalThis.__got = j;
            }});
            "#,
            url = url,
        ))
        .expect("schedule");
    let out = engine.eval("globalThis.__got").expect("observe");
    assert_eq!(out.value["ct"], "application/json");
    assert_eq!(out.value["body"], "{\"key\": \"value\"}");
}

// =============================================================================
// Live httpbin upload (requires public internet)
// =============================================================================

#[tokio::test(flavor = "multi_thread")]
#[ignore = "hits public internet — run with --ignored"]
async fn formdata_upload_blob_via_fetch_to_httpbin_round_trips_content() {
    // The end-to-end "agent uploads a file" scenario. httpbin echoes
    // multipart bodies into the `files` field of its JSON response,
    // keyed by the part name.
    let client = shared_client();
    let rt = tokio::runtime::Handle::current();
    let engine = JsEngine::new_with_fetch(client, rt).expect("engine builds");

    let _ = engine
        .eval(
            r#"
            const fd = new FormData();
            fd.append("description", "agent-shaped upload");
            fd.append("upload", new Blob(["agent-bytes-payload"], { type: "text/plain" }), "agent.txt");
            fetch("https://httpbin.org/post", {
                method: "POST",
                body: fd
            }).then(r => {
                globalThis.__status = r.status;
                return r.json();
            }).then(j => {
                globalThis.__got = j;
            });
            "#,
        )
        .expect("schedule");
    let out = engine
        .eval("[globalThis.__status, globalThis.__got]")
        .expect("observe");
    assert_eq!(out.value[0], 200, "expected 200; got: {:?}", out.value[0]);
    let echo = &out.value[1];
    // httpbin returns:
    //   {
    //     "form": { "description": "agent-shaped upload" },
    //     "files": { "upload": "agent-bytes-payload" },
    //     ...
    //   }
    assert_eq!(
        echo["form"]["description"], "agent-shaped upload",
        "form description not echoed; got: {echo:?}"
    );
    assert_eq!(
        echo["files"]["upload"], "agent-bytes-payload",
        "uploaded file content not echoed; got: {echo:?}"
    );
}
