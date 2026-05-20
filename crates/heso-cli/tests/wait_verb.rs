//! Integration tests for `heso wait` — block-until-condition.
//! Mirrors Playwright's `page.waitForSelector` / `waitForURL` /
//! `waitForLoadState('networkidle')` contracts.
//!
//! Each test spawns `heso wait` as a subprocess against a hermetic
//! wiremock localhost server. Where the condition exercises virtual
//! time (`--time`, setTimeout-based hydration), the wait loop's
//! per-tick `advance_clock` advance is what makes the page-side
//! `setTimeout` fire; no real wall-clock sleep is necessary beyond
//! the cooperative tick.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn run_wait(url: &str, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["wait"];
    args.push(url);
    args.extend_from_slice(extra);
    Command::new(heso_bin())
        .args(&args)
        .output()
        .expect("spawn heso wait")
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

// ============================================================================
// Test 1 — `--selector-exists` returns when an element appears
// ============================================================================

#[tokio::test]
async fn wait_selector_exists_returns_when_element_appears() {
    let server = MockServer::start().await;
    // Page uses setTimeout to insert `<div id="ready">` after 50ms.
    // The wait loop's per-tick virtual-clock advance is what fires
    // the timeout (no real sleep needed).
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <script>
                    setTimeout(() => {
                        const d = document.createElement('div');
                        d.id = 'ready';
                        document.body.appendChild(d);
                    }, 50);
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = run_wait(
        &server.uri(),
        &["--selector-exists", "#ready", "--timeout", "5s"],
    );
    let body = parse_body(&out);
    assert!(
        out.status.success(),
        "heso wait failed: status={:?}\nstdout={}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(body["ok"], serde_json::json!(true), "body={body}");
    assert_eq!(
        body["condition"], serde_json::json!("selector-exists #ready"),
        "condition mismatch: {body}"
    );
}

// ============================================================================
// Test 2 — `--selector-exists` times out
// ============================================================================

#[tokio::test]
async fn wait_selector_exists_times_out_when_never_appears() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><html><body>empty</body></html>"),
        )
        .mount(&server)
        .await;
    let out = run_wait(
        &server.uri(),
        &["--selector-exists", "#never", "--timeout", "500ms"],
    );
    // Exit code is 1 on timeout (success path is 0).
    assert!(!out.status.success(), "expected failure on timeout");
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(false));
    assert_eq!(body["error"], serde_json::json!("timeout"));
}

// ============================================================================
// Test 3 — `--text-contains` finds visible body text
// ============================================================================

#[tokio::test]
async fn wait_text_contains_works() {
    let server = MockServer::start().await;
    // Page schedules a text mutation via setTimeout.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <div id="msg">loading…</div>
                <script>
                    setTimeout(() => {
                        document.querySelector('#msg').textContent = 'Welcome back';
                    }, 100);
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = run_wait(
        &server.uri(),
        &["--text-contains", "Welcome", "--timeout", "5s"],
    );
    assert!(out.status.success(), "wait failed: stderr={}", String::from_utf8_lossy(&out.stderr));
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
}

// ============================================================================
// Test 4 — `--url-matches` for SPA navigation via pushState
// ============================================================================

#[tokio::test]
async fn wait_url_matches_for_spa_navigation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <script>
                    setTimeout(() => {
                        history.pushState({}, '', '/dashboard');
                    }, 50);
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = run_wait(
        &server.uri(),
        &["--url-matches", "/dashboard", "--timeout", "5s"],
    );
    assert!(out.status.success(), "wait failed: stderr={}", String::from_utf8_lossy(&out.stderr));
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(true), "body={body}");
}

// ============================================================================
// Test 5 — `--network-idle` returns after no fetches are pending
// ============================================================================

#[tokio::test]
async fn wait_network_idle_returns_when_no_fetches_pending() {
    let server = MockServer::start().await;
    // Page is static, no in-flight fetches at all. Network-idle
    // should resolve as soon as the idle-window has elapsed.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><html><body>quiet</body></html>"),
        )
        .mount(&server)
        .await;
    let out = run_wait(
        &server.uri(),
        &[
            "--network-idle",
            "--idle-window",
            "100ms",
            "--timeout",
            "5s",
        ],
    );
    assert!(out.status.success(), "wait failed: stderr={}", String::from_utf8_lossy(&out.stderr));
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(true), "body={body}");
}

// ============================================================================
// Test 6 — `--time` advances virtual clock
// ============================================================================

#[tokio::test]
async fn wait_time_advances_virtual_clock() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><html><body></body></html>"),
        )
        .mount(&server)
        .await;
    // Advance 1 s of virtual time. The wait should return ok with a
    // small wall-clock elapsed (no real sleep — `TimeElapsed` is
    // deterministic, not wall-time-blocking).
    let out = run_wait(&server.uri(), &["--time", "1s"]);
    assert!(out.status.success(), "wait failed: stderr={}", String::from_utf8_lossy(&out.stderr));
    let body = parse_body(&out);
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["condition"], serde_json::json!("time 1000ms"));
}

// ============================================================================
// Session-mode test — `wait` against a `heso serve` page_id, with a
// setTimeout-scheduled DOM mutation that the loop's per-tick clock
// advance pumps through.
// ============================================================================

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
    let client = RpcClient {
        stdin,
        reader,
        next_id: 1,
    };
    (child, client)
}

struct RpcClient {
    stdin: ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl RpcClient {
    fn read_ready(&mut self) {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read ready");
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
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
        let resp: serde_json::Value =
            serde_json::from_str(resp_line.trim()).unwrap_or_else(|e| {
                panic!("response was not JSON: {e}\nline: {resp_line}")
            });
        if let Some(err) = resp.get("error") {
            panic!("rpc error for `{method}`: {err}\nfull: {resp}");
        }
        resp.get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn wait_against_session_returns_after_settimeout_appends_element() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <script>
                    setTimeout(() => {
                        const d = document.createElement('div');
                        d.id = 'ready';
                        document.body.appendChild(d);
                    }, 100);
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    client.read_ready();

    let open_res = client.call("open", serde_json::json!({ "url": server.uri() }));
    let page_id = open_res["page_id"].as_str().unwrap().to_owned();

    let wait_res = client.call(
        "wait",
        serde_json::json!({
            "page_id": page_id,
            "selector_exists": "#ready",
            "timeout_ms": 5_000,
        }),
    );
    assert_eq!(
        wait_res["ok"], serde_json::json!(true),
        "wait did not resolve: {wait_res}"
    );
    assert_eq!(
        wait_res["condition"], serde_json::json!("selector-exists #ready")
    );
}
