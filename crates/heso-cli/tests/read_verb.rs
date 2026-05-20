//! Integration tests for `heso read` — the one-call agent-facing
//! page report. Mirrors the `read_verb` slice of the spec for this
//! PR: text extraction, form grouping, cookie surfacing, console
//! capture, framework sniff, session-mode round-trip via `heso serve`.
//!
//! Each test spawns `heso read <url>` (or, for the session-mode
//! check, `heso serve`) against a hermetic localhost wiremock server.
//! No real network involved.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn run_read(url: &str, extra: &[&str]) -> std::process::Output {
    let mut args = vec!["read"];
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
        "heso read failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

#[tokio::test]
async fn read_returns_text_field_with_visible_content() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><h1>Hi</h1><p>Body</p><script>console.log('noise')</script></body></html>",
        ))
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &[]);
    let body = parse_stdout(&out);
    let text = body["text"].as_str().expect("text field");
    assert!(text.contains("Hi"), "expected 'Hi' in text: {text}");
    assert!(text.contains("Body"), "expected 'Body' in text: {text}");
    assert!(
        !text.contains("console.log"),
        "script content leaked into text: {text}"
    );
}

#[tokio::test]
async fn read_returns_forms_with_inputs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <form action="/login" method="post">
                    <input name="user" type="text">
                    <input name="pass" type="password">
                    <button type="submit">Go</button>
                </form>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &[]);
    let body = parse_stdout(&out);
    let forms = body["forms"].as_array().expect("forms array");
    assert_eq!(forms.len(), 1, "expected 1 form: {forms:?}");
    let form = &forms[0];
    assert_eq!(form["action"], serde_json::json!("/login"));
    assert_eq!(form["method"], serde_json::json!("post"));
    let inputs = form["inputs"].as_array().expect("inputs array");
    let names: Vec<&str> = inputs
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert!(names.contains(&"user"), "missing 'user': {names:?}");
    assert!(names.contains(&"pass"), "missing 'pass': {names:?}");
    assert!(
        form["submit_ref"].is_string(),
        "expected submit_ref string: {form}"
    );
}

#[tokio::test]
async fn read_returns_cookies_after_set() {
    let server = MockServer::start().await;
    // Server sets a `session=abc` cookie on the response. The shared
    // reqwest cookie jar (per `FetchEngine::cookie_jar`) should pick
    // it up and `read --include cookies` should surface it.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Set-Cookie", "session=abc; Path=/")
                .set_body_string("<!doctype html><html><body>ok</body></html>"),
        )
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &[]);
    let body = parse_stdout(&out);
    let cookies = body["cookies"].as_array().expect("cookies array");
    assert!(
        cookies
            .iter()
            .any(|c| c["name"] == "session" && c["value"] == "abc"),
        "expected session=abc cookie, got: {cookies:?}"
    );
}

#[tokio::test]
async fn read_returns_console_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><script>console.error('boom');</script></body></html>",
        ))
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &[]);
    let body = parse_stdout(&out);
    let console = body["console"].as_array().expect("console array");
    assert!(
        console.iter().any(|e| {
            e["level"] == "error"
                && e["args"]
                    .as_array()
                    .map(|a| a.iter().any(|x| x == "boom"))
                    .unwrap_or(false)
        }),
        "expected console.error 'boom': {console:?}"
    );
}

#[tokio::test]
async fn read_detects_next_js_framework() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><head>
                <script id="__NEXT_DATA__" type="application/json">{"props":{}}</script>
            </head><body>hi</body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &[]);
    let body = parse_stdout(&out);
    assert_eq!(
        body["framework"], serde_json::json!("next.js"),
        "framework detection failed; body={body}"
    );
}

#[tokio::test]
async fn read_include_filter_drops_unlisted_optional_fields() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><p>hello</p></body></html>",
        ))
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &["--include", "text"]);
    let body = parse_stdout(&out);
    assert!(body["text"].is_string(), "text should be present");
    assert!(body.get("forms").is_none(), "forms should be filtered out");
    assert!(body.get("cookies").is_none(), "cookies should be filtered out");
    assert!(body.get("console").is_none(), "console should be filtered out");
    assert!(body.get("framework").is_none(), "framework should be filtered out");
}

// ============================================================================
// Session-mode test — `read` against a running `heso serve` session,
// across a navigate from page A to page B.
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
async fn read_against_running_session_uses_same_state() {
    // Server has two routes that should surface different page text
    // through the `read` envelope.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><h1>PageA</h1></body></html>",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><h1>PageB</h1></body></html>",
        ))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let ready = client.read_ready();
    let methods = ready["params"]["methods"]
        .as_array()
        .expect("methods array");
    let names: Vec<&str> = methods.iter().filter_map(|m| m.as_str()).collect();
    assert!(names.contains(&"read"), "ready missing `read`: {names:?}");
    assert!(names.contains(&"wait"), "ready missing `wait`: {names:?}");

    // open /a
    let open_res = client.call(
        "open",
        serde_json::json!({ "url": format!("{}/a", server.uri()) }),
    );
    let page_id = open_res["page_id"].as_str().expect("page_id").to_owned();

    // read against page A
    let read_a = client.call("read", serde_json::json!({ "page_id": page_id }));
    let text_a = read_a["text"].as_str().unwrap_or("");
    assert!(text_a.contains("PageA"), "text_a doesn't mention PageA: {text_a}");

    // navigate to /b, then read again
    let nav = client.call(
        "navigate",
        serde_json::json!({ "url": format!("{}/b", server.uri()), "page_id": page_id }),
    );
    assert_eq!(nav["ok"], serde_json::json!(true));
    let read_b = client.call("read", serde_json::json!({ "page_id": page_id }));
    let text_b = read_b["text"].as_str().unwrap_or("");
    assert!(text_b.contains("PageB"), "text_b doesn't mention PageB: {text_b}");
    assert!(
        !text_b.contains("PageA"),
        "page A text leaked after navigate: {text_b}"
    );
}
