//! Integration tests for the `--text` / `--selector` / `--aria-label`
//! locator flags on the `heso click`, `heso fill`, and `heso submit`
//! verbs. Drives the release binary against a hermetic wiremock server
//! so we exercise the full CLI parse → fetch → locator-resolve →
//! engine-dispatch path the same way an agent would.
//!
//! Background: before these flags, a typical agent had to do
//! `read → scan actions → find ref of "Submit" → click @e7`. The flags
//! collapse that into one call, eliminating two round-trips per write.

use std::path::PathBuf;
use std::process::Command;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn run(verb: &str, args: &[&str]) -> std::process::Output {
    let mut all = vec![verb];
    all.extend_from_slice(args);
    Command::new(heso_bin())
        .args(&all)
        .output()
        .expect("spawn heso")
}

fn assert_ok(out: &std::process::Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "heso failed (exit {:?}): stdout={} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn click_by_text_finds_submit_button() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <button id="b">Submit</button>
                <button id="c">Cancel</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = run("click", &[&server.uri(), "--text", "Submit"]);
    let body = assert_ok(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["op"], serde_json::json!("click"));
    // First interactive element is @e0 (the Submit button).
    assert_eq!(body["ref"], serde_json::json!("@e0"));
    assert_eq!(body["selector"], serde_json::json!("#b"));
    // Click doesn't take a string to write; `value` is null. The
    // engine's "selector matched?" boolean is folded into `ok`, and
    // the structured engine payload lives under `result`.
    assert_eq!(body["value"], serde_json::Value::Null);
    assert_eq!(body["element_id"], serde_json::json!("b"));
    assert_eq!(body["result"], serde_json::json!(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn click_by_selector_resolves_to_button() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <button id="a">First</button>
                <button id="primary" class="primary">Primary</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = run("click", &[&server.uri(), "--selector", "button.primary"]);
    let body = assert_ok(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    // The primary button is the second interactive element → @e1.
    assert_eq!(body["ref"], serde_json::json!("@e1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn click_by_aria_label_matches_substring_case_insensitive() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <button aria-label="Close dialog">x</button>
                <button aria-label="Open menu">m</button>
                <button>No aria here</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = run("click", &[&server.uri(), "--aria-label", "MENU"]);
    let body = assert_ok(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    // The "Open menu" button is @e1 (second interactive element).
    assert_eq!(body["ref"], serde_json::json!("@e1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn click_at_ref_path_still_works() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <button>First</button>
                <button>Second</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    // Existing @ref path must NOT regress.
    let out = run("click", &[&server.uri(), "@e1"]);
    let body = assert_ok(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["ref"], serde_json::json!("@e1"));
}

#[tokio::test(flavor = "multi_thread")]
async fn click_zero_match_returns_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><html><body><button>Go</button></body></html>"),
        )
        .mount(&server)
        .await;

    let out = run("click", &[&server.uri(), "--text", "Nope"]);
    assert!(!out.status.success(), "expected non-zero exit");
    assert_eq!(out.status.code(), Some(2), "expected usage exit code 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no element matched locator"),
        "expected no-match message, got stderr: {stderr}"
    );
    assert!(stderr.contains("text:"), "expected text: in stderr: {stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn click_ambiguous_lists_candidates_to_stderr() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <button>Edit</button>
                <button>Edit</button>
                <button>Delete</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = run("click", &[&server.uri(), "--text", "Edit"]);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ambiguous: 2 elements matched"),
        "expected ambiguous message, got: {stderr}"
    );
    // Candidate refs must be visible so the agent can pick one.
    assert!(stderr.contains("@e0"), "expected @e0 in candidates: {stderr}");
    assert!(stderr.contains("@e1"), "expected @e1 in candidates: {stderr}");
}

#[tokio::test(flavor = "multi_thread")]
async fn click_ref_and_locator_together_is_usage_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><html><body><button>Go</button></body></html>"),
        )
        .mount(&server)
        .await;

    let out = run("click", &[&server.uri(), "@e0", "--text", "Go"]);
    assert!(!out.status.success());
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot combine"),
        "expected cannot-combine error, got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_by_selector_sets_input_value() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <input name="q" type="search" placeholder="Search">
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = run(
        "fill",
        &[&server.uri(), "--selector", "input[name=q]", "rust"],
    );
    let body = assert_ok(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["op"], serde_json::json!("fill"));
    // `value` is the literal string the verb wrote (the bytes you
    // passed), not a success boolean. The engine's selector-matched
    // flag rides on `result`.
    assert_eq!(body["value"], serde_json::json!("rust"));
    assert_eq!(body["result"], serde_json::json!(true));
}

#[tokio::test(flavor = "multi_thread")]
async fn fill_by_text_matches_placeholder() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <input name="q" type="search" placeholder="Search the web">
                <input name="other" type="text" placeholder="Other field">
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    // The accessible-name for an input prefers placeholder → `--text search`
    // matches the first input.
    let out = run("fill", &[&server.uri(), "--text", "search", "rust"]);
    let body = assert_ok(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    // `value` carries the literal written string under the new
    // envelope; the engine's matched-flag now lives on `result`.
    assert_eq!(body["value"], serde_json::json!("rust"));
    assert_eq!(body["result"], serde_json::json!(true));
    assert_eq!(body["ref"], serde_json::json!("@e0"));
}

#[tokio::test(flavor = "multi_thread")]
async fn submit_by_selector_posts_form() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/form"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <form id="login" method="post" action="/echo">
                    <input name="user" type="text" value="alice">
                    <button type="submit">Send</button>
                </form>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/echo"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "got": "alice"})),
        )
        .mount(&server)
        .await;

    let url = format!("{}/form", server.uri());
    let out = run("submit", &[&url, "--selector", "form#login"]);
    let body = assert_ok(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["op"], serde_json::json!("submit"));
    // `value` is null for submit; the structured form-submission
    // outcome lives under `result`.
    assert_eq!(body["value"], serde_json::Value::Null);
    assert_eq!(body["result"]["submitted"], serde_json::json!(true));
    assert_eq!(body["result"]["responseJson"]["got"], serde_json::json!("alice"));
    assert_eq!(body["element_id"], serde_json::json!("login"));
}
