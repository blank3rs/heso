//! Integration tests for the PR-Y2 stateful JSON-RPC surface of
//! `heso serve`. Mirrors AGENT_FINDINGS_V3.md task F-X1 and the
//! "Top NEW bugs" #2 verdict — the single biggest gap V3 flagged was
//! that `heso serve` didn't expose `fill` / `click` / `submit` / `eval`
//! / `navigate`, making multi-step stateful workflows structurally
//! impossible. These tests prove the gap is closed:
//!
//! 1. **ready message** lists every new method (regression-pinning the
//!    contract V3 explicitly cited).
//! 2. **fill → eval** proves DOM mutations persist across RPC calls —
//!    the whole point of a sessioned interface.
//! 3. **eval globals persist** — `globalThis.X = 1` in one call,
//!    observable in the next.
//! 4. **click dispatches the event** — verified via a JS-side flag
//!    flipped by a listener attached in a prior `eval` call.
//! 5. **submit returns response body + parsed JSON** against a
//!    wiremock mock.
//! 6. **navigate replaces the page** — different URL → different
//!    `document.title`.
//! 7. **end-to-end multi-step flow** — open → fill → fill → submit →
//!    assert the mock server received both fields. This is the test
//!    AGENT_FINDINGS_V3 would have run if multi-step state had worked.
//!
//! Each test spawns `heso serve` as a child process (so we exercise the
//! actual binary an external agent would invoke, including the
//! line-delimited stdio framing) and drives it via stdin/stdout. A
//! single `RpcClient` helper hides the wire-format chatter so each
//! test reads as the sequence of RPC calls it actually makes.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Absolute path to the `heso` binary the test crate's Cargo build
/// produced. Same env var used by `identity_flow.rs`.
fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

/// Spawn `heso serve` and return the live child + framed RPC client.
fn spawn_serve() -> (Child, RpcClient) {
    let mut child = Command::new(heso_bin())
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heso serve");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);
    let client = RpcClient { stdin, reader, next_id: 1 };
    (child, client)
}

/// Minimal newline-delimited JSON-RPC 2.0 client over a child's stdio.
/// Auto-assigns numeric ids. `call` returns the deserialized `result`
/// (the response's `result` field), or panics on transport / RPC-error.
/// `call_expect_error` returns the `error` object instead. `read_ready`
/// drains the one-shot `ready` notification the server emits at start.
struct RpcClient {
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl RpcClient {
    /// Read and return the one-shot `ready` notification.
    fn read_ready(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read ready");
        let v: serde_json::Value =
            serde_json::from_str(line.trim()).expect("ready is JSON");
        assert_eq!(v.get("method").and_then(|m| m.as_str()), Some("ready"));
        v
    }

    /// Issue an RPC call and return the `result` value. Panics if the
    /// server replies with an `error` instead — use
    /// [`Self::call_expect_error`] for the negative cases.
    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req).unwrap();
        writeln!(self.stdin, "{line}").expect("write request");
        self.stdin.flush().expect("flush stdin");
        let mut resp_line = String::new();
        self.reader.read_line(&mut resp_line).expect("read response");
        let resp: serde_json::Value =
            serde_json::from_str(resp_line.trim()).unwrap_or_else(|e| {
                panic!("response was not JSON: {e}\nline: {resp_line}")
            });
        assert_eq!(resp.get("id"), Some(&serde_json::json!(id)));
        if let Some(err) = resp.get("error") {
            panic!("rpc error for `{method}`: {err}\nfull: {resp}");
        }
        resp.get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }

    /// Call that expects a JSON-RPC `error` reply. Returns the error
    /// object so the caller can assert on `code` / `message`.
    fn call_expect_error(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&req).unwrap())
            .expect("write request");
        self.stdin.flush().unwrap();
        let mut resp_line = String::new();
        self.reader.read_line(&mut resp_line).expect("read response");
        let resp: serde_json::Value = serde_json::from_str(resp_line.trim()).unwrap();
        resp.get("error")
            .cloned()
            .unwrap_or_else(|| panic!("expected error, got: {resp}"))
    }
}

/// Drop the child once the test is done. The Drop impl on `Child`
/// would leave a zombie; we kill explicitly so the process group
/// cleans up even on assertion failure.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ============================================================================
// Test 1 — `ready` advertises every new method
// ============================================================================

#[test]
fn ready_message_advertises_all_write_methods() {
    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let ready = client.read_ready();
    let methods = ready["params"]["methods"]
        .as_array()
        .expect("methods array")
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();
    for required in [
        "open", "ls", "cat", "find", "close", "ping", "fill", "click", "submit",
        "eval", "navigate",
    ] {
        assert!(
            methods.contains(&required),
            "ready did not advertise `{required}`; got: {methods:?}"
        );
    }
}

// ============================================================================
// Test 2 — fill → eval proves DOM mutation persists across RPC calls
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn fill_persists_across_rpc_calls() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <form id="f">
                    <input id="name" type="text" name="name">
                </form>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    // Open the page so the server has a session-eligible record.
    let open_result = client.call(
        "open",
        serde_json::json!({"url": format!("{}/", server.uri())}),
    );
    let page_id = open_result["page_id"].as_str().expect("page_id").to_owned();

    // Find the @ref for the textbox (the action graph normalizes
    // `<input type="text">` to `role="textbox"`).
    let find_result = client.call(
        "find",
        serde_json::json!({
            "page_id": page_id,
            "role": "textbox",
        }),
    );
    let matches = find_result["matches"].as_array().expect("matches");
    let input_ref = matches
        .iter()
        .find_map(|m| m.get("ref").and_then(|r| r.as_str()))
        .expect("at least one textbox @ref")
        .to_owned();

    // Fill the input via the new RPC method.
    let fill_result = client.call(
        "fill",
        serde_json::json!({"ref": input_ref, "value": "alice"}),
    );
    assert_eq!(fill_result["ok"], serde_json::json!(true));
    assert_eq!(
        fill_result["value"]["matched"], serde_json::json!(true),
        "fill must report matched=true; got: {fill_result}"
    );

    // Eval against the SAME session — the input value must persist.
    let eval_result = client.call(
        "eval",
        serde_json::json!({
            "js": "document.querySelector('#name').value",
        }),
    );
    assert_eq!(
        eval_result["value"], serde_json::json!("alice"),
        "fill did not persist into the next eval call; got: {eval_result}"
    );
}

// ============================================================================
// Test 3 — eval globals persist across calls
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn eval_globals_persist_across_calls() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><head><title>g</title></head><body>x</body></html>",
        ))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let open = client.call(
        "open",
        serde_json::json!({"url": format!("{}/", server.uri())}),
    );
    assert!(open["page_id"].is_string());

    // Set a global in one eval call.
    let _ = client.call("eval", serde_json::json!({"js": "globalThis.X = 42; 'set'"}));

    // Read it back in a SECOND eval call.
    let second = client.call("eval", serde_json::json!({"js": "globalThis.X"}));
    assert_eq!(
        second["value"], serde_json::json!(42),
        "globalThis.X did not persist; got: {second}"
    );
}

// ============================================================================
// Test 4 — click dispatches the event (verified via a JS-side flag)
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn click_dispatches_event_through_session() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <button id="b">go</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let open = client.call(
        "open",
        serde_json::json!({"url": format!("{}/", server.uri())}),
    );
    let page_id = open["page_id"].as_str().unwrap().to_owned();

    // Install a click listener via eval. The listener flips a global
    // flag we can observe in a later eval call.
    let _ = client.call(
        "eval",
        serde_json::json!({
            "js": "globalThis.__clicked = false; \
                   document.querySelector('#b').addEventListener('click', () => { \
                       globalThis.__clicked = true; \
                   }); 'installed'",
        }),
    );

    // Find the button's @ref.
    let find = client.call(
        "find",
        serde_json::json!({"page_id": page_id, "role": "button"}),
    );
    let button_ref = find["matches"][0]["ref"]
        .as_str()
        .expect("button @ref")
        .to_owned();

    // Click via the new RPC method.
    let click = client.call("click", serde_json::json!({"ref": button_ref}));
    assert_eq!(click["ok"], serde_json::json!(true));
    assert_eq!(
        click["value"]["matched"], serde_json::json!(true),
        "click must report matched=true; got: {click}"
    );

    // The listener should have flipped the flag.
    let observe = client.call("eval", serde_json::json!({"js": "globalThis.__clicked"}));
    assert_eq!(
        observe["value"], serde_json::json!(true),
        "click did not dispatch the event; flag still false: {observe}"
    );
}

// ============================================================================
// Test 5 — submit returns response body + parsed JSON
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_returns_response_body_and_parsed_json() {
    let server = MockServer::start().await;
    // Form page.
    Mock::given(method("GET"))
        .and(path("/form"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            // Build the action URL inline so we don't need an outer
            // mutable closure over `server.uri()`.
            r#"<!doctype html><html><body>
                <form id="f" method="post" action="/echo">
                    <input type="text" name="who" value="bob">
                    <button type="submit">Send</button>
                </form>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    // Mock echo endpoint that replies with JSON. `set_body_json`
    // sets the `content-type: application/json` header automatically
    // (vs. `set_body_string` which lands on text/plain) — that's the
    // signal `submit_with_fields` uses to parse responseJson.
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "got": "bob"})),
        )
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let open = client.call(
        "open",
        serde_json::json!({"url": format!("{}/form", server.uri())}),
    );
    let page_id = open["page_id"].as_str().unwrap().to_owned();

    // Find the form's @ref.
    let find = client.call(
        "find",
        serde_json::json!({"page_id": page_id, "role": "form"}),
    );
    let form_ref = find["matches"][0]["ref"]
        .as_str()
        .expect("form @ref")
        .to_owned();

    // Submit.
    let submit = client.call("submit", serde_json::json!({"ref": form_ref}));
    assert_eq!(submit["ok"], serde_json::json!(true));
    assert_eq!(
        submit["value"]["submitted"], serde_json::json!(true),
        "submit must succeed; got: {submit}"
    );
    assert_eq!(submit["value"]["responseStatus"], serde_json::json!(200));
    let body = submit["value"]["responseBody"]
        .as_str()
        .expect("responseBody is a string");
    assert!(
        body.contains(r#""ok":true"#) && body.contains(r#""got":"bob""#),
        "responseBody mismatch: {body}"
    );
    // The JSON content-type triggers responseJson parsing.
    assert_eq!(
        submit["value"]["responseJson"]["ok"], serde_json::json!(true),
        "responseJson missing or wrong shape: {submit}"
    );
    assert_eq!(submit["value"]["responseJson"]["got"], serde_json::json!("bob"));
}

// ============================================================================
// Test 6 — navigate replaces the page
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn navigate_replaces_page_with_new_title() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/first"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><head><title>FIRST</title></head><body>1</body></html>",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/second"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><head><title>SECOND</title></head><body>2</body></html>",
        ))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let open = client.call(
        "open",
        serde_json::json!({"url": format!("{}/first", server.uri())}),
    );
    let page_id = open["page_id"].as_str().unwrap().to_owned();

    let title1 = client.call("eval", serde_json::json!({"js": "document.title"}));
    assert_eq!(title1["value"], serde_json::json!("FIRST"));

    // Navigate the SAME session to the second URL.
    let nav = client.call(
        "navigate",
        serde_json::json!({"url": format!("{}/second", server.uri()), "page_id": page_id}),
    );
    assert_eq!(nav["ok"], serde_json::json!(true));
    assert!(
        nav["url"].as_str().unwrap().ends_with("/second"),
        "navigate didn't move to /second; got: {nav}"
    );

    let title2 = client.call("eval", serde_json::json!({"js": "document.title"}));
    assert_eq!(
        title2["value"], serde_json::json!("SECOND"),
        "navigate did not replace the document; title still FIRST: {title2}"
    );
}

// ============================================================================
// Test 7 — End-to-end multi-step flow (the F-X1 task)
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn end_to_end_multi_step_open_fill_fill_submit() {
    // The shape that AGENT_FINDINGS_V3.md F-X1 was trying to run, now
    // working end-to-end. Two text fields filled via two `fill` calls
    // against the SAME session; `submit` sends both values to a mock
    // /post endpoint; we assert the request body the mock received
    // carries both fields.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/forms/post"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <form id="f" method="post" action="/post">
                    <input id="custname" type="text" name="custname">
                    <input id="custemail" type="text" name="custemail">
                    <button type="submit">Order</button>
                </form>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/post"))
        .respond_with(ResponseTemplate::new(200).set_body_string("received"))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    // 1) open
    let open = client.call(
        "open",
        serde_json::json!({"url": format!("{}/forms/post", server.uri())}),
    );
    let page_id = open["page_id"].as_str().unwrap().to_owned();

    // 2) find both text inputs and the form via @e refs (the action
    //    graph normalizes `<input type="text">` to role="textbox").
    let find = client.call(
        "find",
        serde_json::json!({"page_id": page_id, "role": "textbox"}),
    );
    let inputs = find["matches"].as_array().expect("matches array");
    // Resolve each @ref to its input name so the test is robust to
    // future @e numbering changes.
    let mut ref_by_name: std::collections::HashMap<String, String> = Default::default();
    for el in inputs {
        let name = el["attrs"]["name"].as_str().unwrap_or("").to_owned();
        let r = el["ref"].as_str().unwrap_or("").to_owned();
        if !name.is_empty() && !r.is_empty() {
            ref_by_name.insert(name, r);
        }
    }
    let custname_ref = ref_by_name.get("custname").cloned().expect("custname ref");
    let custemail_ref = ref_by_name.get("custemail").cloned().expect("custemail ref");

    let form_find = client.call(
        "find",
        serde_json::json!({"page_id": page_id, "role": "form"}),
    );
    let form_ref = form_find["matches"][0]["ref"]
        .as_str()
        .expect("form ref")
        .to_owned();

    // 3) fill custname → alice
    let f1 = client.call(
        "fill",
        serde_json::json!({"ref": custname_ref, "value": "alice"}),
    );
    assert_eq!(f1["ok"], serde_json::json!(true));
    assert_eq!(f1["value"]["matched"], serde_json::json!(true));

    // 4) fill custemail → alice@example.com — proving the second fill
    //    didn't wipe the first.
    let f2 = client.call(
        "fill",
        serde_json::json!({"ref": custemail_ref, "value": "alice@example.com"}),
    );
    assert_eq!(f2["ok"], serde_json::json!(true));
    assert_eq!(f2["value"]["matched"], serde_json::json!(true));

    // Cross-check via eval: both inputs still hold their typed values.
    let snapshot = client.call(
        "eval",
        serde_json::json!({
            "js": "[document.querySelector('#custname').value, \
                   document.querySelector('#custemail').value]",
        }),
    );
    assert_eq!(
        snapshot["value"],
        serde_json::json!(["alice", "alice@example.com"]),
        "DOM state lost between fills: {snapshot}"
    );

    // 5) submit
    let submit = client.call("submit", serde_json::json!({"ref": form_ref}));
    assert_eq!(submit["ok"], serde_json::json!(true));
    assert_eq!(
        submit["value"]["submitted"], serde_json::json!(true),
        "submit didn't succeed: {submit}"
    );

    // 6) verify the mock server saw both fields in the request body.
    let reqs = server.received_requests().await.unwrap();
    let posts: Vec<_> = reqs
        .iter()
        .filter(|r| r.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(posts.len(), 1, "expected exactly one POST, got {}", posts.len());
    let body = String::from_utf8_lossy(&posts[0].body).into_owned();
    // urlencoded body — spaces are `+`, `@` is `%40`, etc.
    assert!(
        body.contains("custname=alice"),
        "POST body missing custname=alice: {body}"
    );
    assert!(
        body.contains("custemail=alice") && body.contains("example.com"),
        "POST body missing custemail: {body}"
    );
}

// ============================================================================
// Test 8 — submit with `field` overrides (one-shot path still works)
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn submit_with_field_overrides() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/form"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <form id="f" method="post" action="/p">
                    <input type="text" name="who" value="default">
                    <button type="submit">Send</button>
                </form>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/p"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let open = client.call(
        "open",
        serde_json::json!({"url": format!("{}/form", server.uri())}),
    );
    let page_id = open["page_id"].as_str().unwrap().to_owned();
    let find = client.call(
        "find",
        serde_json::json!({"page_id": page_id, "role": "form"}),
    );
    let form_ref = find["matches"][0]["ref"].as_str().unwrap().to_owned();

    let submit = client.call(
        "submit",
        serde_json::json!({
            "ref": form_ref,
            "field": {"who": "OVERRIDE"},
        }),
    );
    assert_eq!(submit["value"]["submitted"], serde_json::json!(true));

    let reqs = server.received_requests().await.unwrap();
    let post = reqs.iter().find(|r| r.method == wiremock::http::Method::POST).expect("a POST");
    let body = String::from_utf8_lossy(&post.body).into_owned();
    assert!(
        body.contains("who=OVERRIDE"),
        "field override not applied: {body}"
    );
}

// ============================================================================
// Test 9 — calling a write verb before any `open` returns a clear error
// ============================================================================

#[test]
fn write_verb_without_active_page_errors_clearly() {
    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let err = client.call_expect_error(
        "eval",
        serde_json::json!({"js": "1 + 1"}),
    );
    // INTERNAL_ERROR (-32603) is what the dispatcher uses for handler
    // errors — the message must guide the agent to `open`.
    assert_eq!(err["code"], serde_json::json!(-32603));
    let msg = err["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("no active page"),
        "expected `no active page` hint, got: {msg}"
    );
}

// ============================================================================
// Test 10 — bad params surface the underlying decode error
// ============================================================================

#[test]
fn fill_with_missing_ref_returns_bad_params() {
    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let err = client.call_expect_error(
        "fill",
        // Missing `ref` field entirely.
        serde_json::json!({"value": "x"}),
    );
    assert_eq!(err["code"], serde_json::json!(-32603));
    let msg = err["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("bad params"),
        "expected `bad params` prefix, got: {msg}"
    );
}

// ============================================================================
// Test 11 — `cat`/`ls`/`find` still work alongside the new verbs (the
// read methods must not regress).
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn read_methods_still_work_alongside_write_verbs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><head><title>R</title></head><body><h1>head</h1><p>x</p></body></html>",
        ))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let open = client.call(
        "open",
        serde_json::json!({"url": format!("{}/", server.uri())}),
    );
    let page_id = open["page_id"].as_str().unwrap().to_owned();

    // ls works.
    let ls = client.call("ls", serde_json::json!({"page_id": page_id, "path": "/"}));
    assert!(ls["entries"].is_array());

    // After eval mutates the DOM, the cached FetchPage (used by ls/cat
    // / find) STILL shows the pre-eval content — read methods snapshot
    // at open time. This is the documented behavior.
    let _ = client.call(
        "eval",
        serde_json::json!({"js": "document.body.innerHTML = '<p>after</p>'; 'mutated'"}),
    );
    // ls / cat should still reflect the pre-eval state.
    let ls2 = client.call("ls", serde_json::json!({"page_id": page_id, "path": "/"}));
    assert!(ls2["entries"].is_array(), "ls still works after eval");

    // ping is the cheapest health check.
    let pong = client.call("ping", serde_json::json!({}));
    assert_eq!(pong, serde_json::json!("pong"));
}

// ============================================================================
// Test 12 — closing a page clears the `last_page_id` so subsequent
// pageless write calls fail loudly instead of silently dangling.
// ============================================================================

#[tokio::test(flavor = "multi_thread")]
async fn close_clears_default_page_id() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body>x</body></html>",
        ))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ = client.read_ready();

    let open = client.call(
        "open",
        serde_json::json!({"url": format!("{}/", server.uri())}),
    );
    let page_id = open["page_id"].as_str().unwrap().to_owned();

    let close = client.call("close", serde_json::json!({"page_id": page_id}));
    assert_eq!(close["closed"], serde_json::json!(true));

    // After close, pageless eval has no default to fall back to.
    let err = client.call_expect_error("eval", serde_json::json!({"js": "1"}));
    let msg = err["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("no active page"),
        "expected stale-default cleared, got: {msg}"
    );
}

// ============================================================================
// Test 13 — a request line that's bad JSON returns -32700 parse_error
// instead of taking down the server.
// ============================================================================

#[test]
fn parse_error_on_bad_request_does_not_kill_server() {
    let (mut child, mut client) = spawn_serve();
    let _ = client.read_ready();

    // Write a non-JSON line directly.
    writeln!(client.stdin, "not valid json {{").unwrap();
    client.stdin.flush().unwrap();
    // Read the response — should be a parse-error.
    let mut line = String::new();
    client.reader.read_line(&mut line).unwrap();
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["error"]["code"], serde_json::json!(-32700));

    // The server must still be alive — ping it.
    let pong = client.call("ping", serde_json::json!({}));
    assert_eq!(pong, serde_json::json!("pong"));

    // Tidy up.
    let _ = child.kill();
    let _ = child.wait();
}
