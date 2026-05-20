//! Integration tests for `--inject-script`: the agent's pre-page
//! polyfill injection hook on `heso open` / `read` / `wait` (CLI) and
//! `open` / `read` / `wait` (JSON-RPC).
//!
//! The contract this exercises:
//!
//! 1. Inline `--inject-script "<JS>"` runs BEFORE any page `<script>`,
//!    so a page-side `<script>` that reads `window.MY_POLYFILL` sees
//!    the injected stub.
//! 2. Multiple `--inject-script` flags preserve order — the first
//!    runs before the second, so `--inject-script "window.A=1"
//!    --inject-script "window.B=window.A+1"` ends with `B === 2`.
//! 3. `--inject-script @<filepath>` reads the JS body from disk
//!    (relative or absolute path).
//! 4. A throwing injected script hard-fails the verb with a clear
//!    error message naming the 1-based inject index, NOT a silent
//!    swallow.
//! 5. The `read` verb plumbs `--inject-script` identically.
//! 6. Over JSON-RPC, `inject_scripts: [string]` on `open` makes the
//!    polyfill visible to a follow-up `eval` against the same
//!    `page_id`.
//!
//! Each test runs against a hermetic wiremock localhost server. No
//! real network involved.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use tempfile::NamedTempFile;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn run_open(url: &str, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<&str> = vec!["open"];
    args.extend_from_slice(extra);
    args.push(url);
    Command::new(heso_bin())
        .args(&args)
        .output()
        .expect("spawn heso open")
}

fn run_read(url: &str, extra: &[&str]) -> std::process::Output {
    let mut args: Vec<&str> = vec!["read"];
    args.extend_from_slice(extra);
    args.push(url);
    Command::new(heso_bin())
        .args(&args)
        .output()
        .expect("spawn heso read")
}

fn parse_stdout(out: &std::process::Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "heso failed: status={:?}\nstdout={}\nstderr={}",
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

// ============================================================================
// Test 1 — Inline injection: `--inject-script "window.MY_POLYFILL = 42"`
// is visible to a page-side `<script>` that reads it.
// ============================================================================

#[tokio::test]
async fn inject_script_inline_is_visible_to_page_scripts() {
    let server = MockServer::start().await;
    // The page's inline script reads `window.MY_POLYFILL` (which the
    // inject sets to 42) and writes the truthiness into `document.title`.
    // If the inject ran AFTER this script (broken contract), title would
    // stay "default".
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>default</title></head><body>
                <script>
                    document.title = (window.MY_POLYFILL === 42) ? "INJECTED" : "MISSED";
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = run_open(
        &server.uri(),
        &["--inject-script", "window.MY_POLYFILL = 42"],
    );
    let body = parse_stdout(&out);
    assert_eq!(
        body["title"], serde_json::json!("INJECTED"),
        "inject did not run before page script: body={body}"
    );
}

// ============================================================================
// Test 2 — Order preservation: `--inject-script "window.A=1"
// --inject-script "window.B=window.A+1"` leaves B === 2.
// ============================================================================

#[tokio::test]
async fn inject_scripts_run_in_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>x</title></head><body>
                <script>
                    document.title = (window.A === 1 && window.B === 2) ? "OK" : "WRONG";
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = run_open(
        &server.uri(),
        &[
            "--inject-script",
            "window.A = 1",
            "--inject-script",
            "window.B = window.A + 1",
        ],
    );
    let body = parse_stdout(&out);
    assert_eq!(
        body["title"], serde_json::json!("OK"),
        "inject scripts ran out of order: body={body}"
    );
}

// ============================================================================
// Test 3 — `@filepath` form reads JS from disk.
// ============================================================================

#[tokio::test]
async fn inject_script_at_file_reads_js_from_disk() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>x</title></head><body>
                <script>
                    document.title = (window.FROMFILE === 'yes') ? "FROMFILE" : "NO";
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    // Persist the temp file under `into_temp_path` so we control its
    // lifecycle — drop after the child finishes.
    let mut tmp = NamedTempFile::new().expect("tempfile");
    use std::io::Write as _;
    tmp.write_all(b"window.FROMFILE = 'yes';\n").expect("write tmp");
    let path = tmp.path().to_str().expect("utf8 tmp path").to_owned();
    let arg = format!("@{path}");

    let out = run_open(&server.uri(), &["--inject-script", &arg]);
    let body = parse_stdout(&out);
    assert_eq!(
        body["title"], serde_json::json!("FROMFILE"),
        "@filepath inject did not run: body={body}"
    );
}

// ============================================================================
// Test 4 — A throwing injected script hard-fails with a clear error.
// ============================================================================

#[tokio::test]
async fn inject_script_throw_hard_fails_with_clear_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><html><body>ok</body></html>"),
        )
        .mount(&server)
        .await;

    // Two injects: the first is fine, the second throws. Verifies that
    // we name the 1-based index of the offender, not the first one.
    let out = run_open(
        &server.uri(),
        &[
            "--inject-script",
            "window.A = 1",
            "--inject-script",
            "throw new Error('boom')",
        ],
    );
    assert!(
        !out.status.success(),
        "expected non-zero exit on injected throw: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--inject-script #2"),
        "stderr should name the failing inject (#2): {stderr}"
    );
    assert!(
        stderr.contains("boom"),
        "stderr should include the thrown message: {stderr}"
    );
}

// ============================================================================
// Test 5 — `heso read --inject-script ...` exercises the same plumbing.
// ============================================================================

#[tokio::test]
async fn inject_script_works_on_read_verb() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head><title>x</title></head><body>
                <p id="msg"></p>
                <script>
                    const m = document.getElementById('msg');
                    if (m) { m.textContent = (window.READPATCH === 'ok') ? 'patched' : 'missed'; }
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;

    let out = run_read(
        &server.uri(),
        &["--include", "text", "--inject-script", "window.READPATCH = 'ok'"],
    );
    let body = parse_stdout(&out);
    let text = body["text"].as_str().unwrap_or("");
    assert!(
        text.contains("patched"),
        "post-hydration text should reflect the inject: {text}"
    );
    assert!(
        !text.contains("missed"),
        "inject ran AFTER page script (or not at all): {text}"
    );
}

// ============================================================================
// Test 6 — JSON-RPC: `inject_scripts` on `open` survives to a
// follow-up `eval` against the same `page_id`.
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
    fn read_ready(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read ready");
        serde_json::from_str(line.trim()).expect("ready is JSON")
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
        let resp: serde_json::Value = serde_json::from_str(resp_line.trim())
            .unwrap_or_else(|e| panic!("response was not JSON: {e}\nline: {resp_line}"));
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
async fn jsonrpc_open_with_inject_scripts_persists_to_eval() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><html><body>hi</body></html>"),
        )
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ready = client.read_ready();

    let open_res = client.call(
        "open",
        serde_json::json!({
            "url": server.uri(),
            "inject_scripts": ["window.X = 1"],
        }),
    );
    let page_id = open_res["page_id"].as_str().expect("page_id").to_owned();

    // The eval builds the session lazily (this is the first
    // session-touching call), so the inject_scripts stashed on the
    // PageRecord should land before any page script ran.
    let eval_res = client.call(
        "eval",
        serde_json::json!({
            "js": "globalThis.X",
            "page_id": page_id,
        }),
    );
    assert_eq!(
        eval_res["value"], serde_json::json!(1),
        "inject_scripts did not propagate to session-bound eval: {eval_res}"
    );
}
