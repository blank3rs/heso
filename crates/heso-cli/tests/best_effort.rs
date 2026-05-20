//! Integration tests for `--best-effort` on `heso open` / `heso read` /
//! `heso wait`. The flag turns script crashes and wait timeouts into
//! partial successes (exit 0) carrying a structured-failure envelope:
//! `partial`, `partial_reason`, `failed_scripts`, `console_errors_count`.
//!
//! Default behavior (no flag) is unchanged — these tests pin both the
//! default and the best-effort paths against hermetic wiremock fixtures
//! so a regression in either direction is caught.

use std::path::PathBuf;
use std::process::Command;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn parse_body(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

/// HTML fixture that throws synchronously during inline-script eval.
const THROW_FIXTURE: &str = r#"<!doctype html><html><head><title>Crashy</title></head><body>
    <h1>Partially Loaded</h1>
    <p>Some readable body content.</p>
    <script>throw new Error('boom');</script>
</body></html>"#;

// =====================================================================
// Test 1 — `heso open` against a throwing fixture, no flag.
// Returns the page (existing behavior) AND emits `failed_scripts`
// with the captured crash entry (new structured-failure field).
// =====================================================================
#[tokio::test]
async fn open_without_flag_surfaces_failed_scripts() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(THROW_FIXTURE))
        .mount(&server)
        .await;
    let out = Command::new(heso_bin())
        .args(["open", &server.uri()])
        .output()
        .expect("spawn heso open");
    assert!(
        out.status.success(),
        "heso open failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = parse_body(&out);
    assert_eq!(body["partial"], serde_json::json!(true), "body={body}");
    assert_eq!(
        body["partial_reason"], serde_json::json!("script_crash"),
        "body={body}"
    );
    let failed = body["failed_scripts"].as_array().expect("failed_scripts");
    assert!(!failed.is_empty(), "expected ≥1 failed_scripts: {failed:?}");
    let f0 = &failed[0];
    assert_eq!(f0["reason"], serde_json::json!("script_crash"));
    let msg = f0["message"].as_str().unwrap_or_default();
    assert!(msg.contains("boom"), "expected 'boom' in message: {msg}");
    // The page still ships its static fields (per the unchanged
    // default contract).
    assert!(body["title"].is_string());
}

// =====================================================================
// Test 2 — `heso open --best-effort` against same fixture.
// Exit 0, `partial: true`, `partial_reason: "script_crash"`.
// =====================================================================
#[tokio::test]
async fn open_with_best_effort_exits_zero_and_marks_partial() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(THROW_FIXTURE))
        .mount(&server)
        .await;
    let out = Command::new(heso_bin())
        .args(["open", "--best-effort", &server.uri()])
        .output()
        .expect("spawn heso open --best-effort");
    assert!(
        out.status.success(),
        "heso open --best-effort must exit 0 even when a script crashes; got status={:?}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let body = parse_body(&out);
    assert_eq!(body["partial"], serde_json::json!(true));
    assert_eq!(body["partial_reason"], serde_json::json!("script_crash"));
}

// =====================================================================
// Test 3 — `heso read --best-effort` against same fixture.
// Exit 0, `partial: true`, full read payload still returned.
// =====================================================================
#[tokio::test]
async fn read_with_best_effort_returns_full_payload_on_script_crash() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(THROW_FIXTURE))
        .mount(&server)
        .await;
    let out = Command::new(heso_bin())
        .args(["read", "--best-effort", &server.uri()])
        .output()
        .expect("spawn heso read --best-effort");
    assert!(
        out.status.success(),
        "heso read --best-effort must exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = parse_body(&out);
    assert_eq!(body["partial"], serde_json::json!(true));
    assert_eq!(body["partial_reason"], serde_json::json!("script_crash"));
    // Read-specific extras must still be present.
    assert!(body["text"].is_string(), "text field missing on best-effort read: {body}");
    assert!(body["framework"].is_string(), "framework field missing: {body}");
    let failed = body["failed_scripts"].as_array().expect("failed_scripts");
    assert!(!failed.is_empty(), "expected failed_scripts populated: {failed:?}");
}

// =====================================================================
// Test 4 — `heso wait --best-effort` against a static fixture with a
// selector that never appears. Exit 0, `partial: true`,
// `partial_reason: "wait_timeout"`.
// =====================================================================
#[tokio::test]
async fn wait_with_best_effort_returns_partial_on_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body><h1>Static</h1></body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = Command::new(heso_bin())
        .args([
            "wait",
            &server.uri(),
            "--selector-exists",
            ".never-appears",
            "--timeout",
            "200ms",
            "--best-effort",
        ])
        .output()
        .expect("spawn heso wait --best-effort");
    assert!(
        out.status.success(),
        "heso wait --best-effort timeout must exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = parse_body(&out);
    // The outcome is still "ok: false" inside the body — best-effort
    // doesn't lie about whether the condition was met; it only
    // changes the exit code and surfaces a partial_reason.
    assert_eq!(body["ok"], serde_json::json!(false), "body={body}");
    assert_eq!(body["partial"], serde_json::json!(true));
    assert_eq!(body["partial_reason"], serde_json::json!("wait_timeout"));
}

// =====================================================================
// Test 5 — URL that can't be fetched at all. Even `--best-effort`
// MUST surface this as a hard failure (no payload to return).
// =====================================================================
#[tokio::test]
async fn best_effort_does_not_swallow_hard_fetch_failures() {
    // Use a port that's almost certainly not listening so reqwest
    // produces a connection-refused / DNS error.
    let bad_url = "http://127.0.0.1:1/";
    let out = Command::new(heso_bin())
        .args(["read", "--best-effort", bad_url])
        .output()
        .expect("spawn heso read --best-effort against unreachable host");
    assert!(
        !out.status.success(),
        "heso read --best-effort against unreachable host must NOT exit 0 (no payload to partial-return)"
    );
}

// =====================================================================
// Test 6 — Clean run: no failures, envelope reports the trivially-
// clean shape (`partial: false`, `partial_reason: "ok"`, empty
// `failed_scripts`, `console_errors_count: 0`).
// =====================================================================
#[tokio::test]
async fn clean_run_emits_clean_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body><p>hi</p></body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = Command::new(heso_bin())
        .args(["read", &server.uri()])
        .output()
        .expect("spawn heso read");
    assert!(
        out.status.success(),
        "heso read on a clean page must exit 0; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = parse_body(&out);
    assert_eq!(body["partial"], serde_json::json!(false), "body={body}");
    assert_eq!(body["partial_reason"], serde_json::json!("ok"));
    assert_eq!(body["failed_scripts"], serde_json::json!([]));
    assert_eq!(body["console_errors_count"], serde_json::json!(0));
}

// =====================================================================
// Test 7 — `heso open` (no flag) against a clean page emits the
// clean envelope shape too. This catches accidentally-breaking the
// default branch of `cmd_open`.
// =====================================================================
#[tokio::test]
async fn open_clean_page_emits_clean_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>OK</title></head><body><p>hi</p></body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = Command::new(heso_bin())
        .args(["open", &server.uri()])
        .output()
        .expect("spawn heso open");
    assert!(out.status.success());
    let body = parse_body(&out);
    assert_eq!(body["partial"], serde_json::json!(false));
    assert_eq!(body["partial_reason"], serde_json::json!("ok"));
    assert_eq!(body["failed_scripts"], serde_json::json!([]));
    assert_eq!(body["console_errors_count"], serde_json::json!(0));
}

// =====================================================================
// Test 8 — `heso wait` without `--best-effort` on a timeout exits 1
// (unchanged behavior). The structured envelope is also present.
// =====================================================================
#[tokio::test]
async fn wait_without_best_effort_still_exits_non_zero_on_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body><p>nope</p></body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = Command::new(heso_bin())
        .args([
            "wait",
            &server.uri(),
            "--selector-exists",
            ".never-appears",
            "--timeout",
            "200ms",
        ])
        .output()
        .expect("spawn heso wait");
    assert!(
        !out.status.success(),
        "heso wait timeout without --best-effort must exit non-zero"
    );
    // But the body still has the new envelope fields populated.
    let body = parse_body(&out);
    assert_eq!(body["partial"], serde_json::json!(true), "body={body}");
    assert_eq!(body["partial_reason"], serde_json::json!("wait_timeout"));
}
