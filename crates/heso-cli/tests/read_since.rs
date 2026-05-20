//! Integration tests for `heso read`'s `content_hash` + `--since`
//! cross-call state-diff. Six contracts:
//!
//! 1. `heso read <url>` always emits a `content_hash` (BLAKE3) field of
//!    the form `"blake3:<64-hex>"`.
//! 2. One-shot `heso read --since <nonsense-hash>` reports
//!    `delta.since_matched: false` with every current action listed in
//!    `actions_added` (the documented "no prior snapshot, treat as
//!    fresh" branch).
//! 3. Serve session: two consecutive reads of the same URL → second
//!    call with `since: <first.content_hash>` returns
//!    `delta.since_matched: true` and every diff field empty/false.
//! 4. Serve session: read → click (DOM mutation) → read with `since`
//!    → some diff field non-empty (text or actions changed).
//! 5. Snapshot LRU: read 9 distinct URLs → URL #1's snapshot is
//!    evicted → a follow-up `read URL#1` with the old hash gets
//!    `since_matched: false`.
//! 6. Existing read_verb tests (cookies, console, forms, framework,
//!    include filter, session round-trip) continue to pass — covered
//!    by the separate `read_verb.rs` suite; we just don't break them.

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

/// A nonsense `content_hash` value with the correct shape — 64 hex
/// zeros under the `blake3:` prefix. Used to drive the
/// "since-supplied-but-no-prior-snapshot" branch of `--since`.
const NONSENSE_HASH: &str =
    "blake3:0000000000000000000000000000000000000000000000000000000000000000";

// ============================================================================
// 1. content_hash always present, well-formed
// ============================================================================

#[tokio::test]
async fn read_emits_well_formed_content_hash() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><h1>Hi</h1><a href=\"/x\">go</a></body></html>",
        ))
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &[]);
    let body = parse_stdout(&out);
    let ch = body["content_hash"]
        .as_str()
        .expect("content_hash field is a string");
    assert!(
        ch.starts_with("blake3:"),
        "expected blake3: prefix, got `{ch}`"
    );
    let hex = ch.strip_prefix("blake3:").unwrap();
    assert_eq!(hex.len(), 64, "expected 64 hex chars, got `{hex}` ({} chars)", hex.len());
    assert!(
        hex.chars().all(|c| c.is_ascii_hexdigit()),
        "non-hex chars in content_hash: `{ch}`"
    );
}

// ============================================================================
// 2. One-shot `--since <nonsense>` returns since_matched: false with all
//    actions in actions_added.
// ============================================================================

#[tokio::test]
async fn read_since_one_shot_reports_since_matched_false_with_all_actions_added() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <a href="/a">Apple</a>
                <a href="/b">Banana</a>
                <button>Cherry</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &["--since", NONSENSE_HASH]);
    let body = parse_stdout(&out);
    let delta = body
        .get("delta")
        .and_then(|d| d.as_object())
        .expect("delta object");
    assert_eq!(
        delta.get("since_matched").and_then(|v| v.as_bool()),
        Some(false),
        "since_matched should be false for one-shot --since: {body}"
    );
    let added = delta
        .get("actions_added")
        .and_then(|a| a.as_array())
        .expect("actions_added array");
    let names: Vec<&str> = added
        .iter()
        .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(names.contains(&"Apple"), "Apple missing: {names:?}");
    assert!(names.contains(&"Banana"), "Banana missing: {names:?}");
    assert!(names.contains(&"Cherry"), "Cherry missing: {names:?}");
    // No prior snapshot → no removals, no per-field changes.
    assert!(
        delta
            .get("actions_removed")
            .and_then(|x| x.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "actions_removed should be empty: {delta:?}"
    );
    assert_eq!(
        delta.get("forms_changed").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        delta.get("text_changed").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        delta.get("title_changed").and_then(|v| v.as_bool()),
        Some(false)
    );
}

// ============================================================================
// 3. Default (no `--since`) → delta is null
// ============================================================================

#[tokio::test]
async fn read_without_since_emits_null_delta() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            "<!doctype html><html><body><p>hi</p></body></html>",
        ))
        .mount(&server)
        .await;
    let out = run_read(&server.uri(), &[]);
    let body = parse_stdout(&out);
    assert!(
        body.get("delta").map(|d| d.is_null()).unwrap_or(false),
        "delta should be null when --since omitted: {body}"
    );
}

// ============================================================================
// Serve-mode RPC harness (mirrors `read_verb.rs`'s pattern).
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
        self.reader
            .read_line(&mut resp_line)
            .expect("read response");
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

// ============================================================================
// 4. Serve session: same URL twice → since matches → all diff fields
//    empty/false.
// ============================================================================

#[tokio::test]
async fn serve_read_twice_with_since_yields_clean_diff() {
    let server = MockServer::start().await;
    // Stable page — two reads should produce identical content_hashes
    // and an empty diff.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <h1>Hello</h1>
                <a href="/x">Link</a>
                <button id="b">Press</button>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ready = client.read_ready();

    let open_res = client.call("open", serde_json::json!({ "url": server.uri() }));
    let page_id = open_res["page_id"].as_str().expect("page_id").to_owned();

    let read1 = client.call("read", serde_json::json!({ "page_id": page_id }));
    let first_hash = read1["content_hash"]
        .as_str()
        .expect("content_hash on first read")
        .to_owned();

    let read2 = client.call(
        "read",
        serde_json::json!({ "page_id": page_id, "since": first_hash }),
    );
    let delta = read2["delta"].as_object().expect("delta object");
    assert_eq!(
        delta.get("since_matched").and_then(|v| v.as_bool()),
        Some(true),
        "since_matched should be true; full delta: {delta:?}\nread1.content_hash={}\nread2.content_hash={}",
        first_hash,
        read2["content_hash"].as_str().unwrap_or(""),
    );
    assert!(
        delta["actions_added"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "actions_added not empty: {delta:?}"
    );
    assert!(
        delta["actions_removed"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "actions_removed not empty: {delta:?}"
    );
    assert_eq!(delta["forms_changed"], serde_json::json!(false));
    assert_eq!(delta["text_changed"], serde_json::json!(false));
    assert_eq!(delta["title_changed"], serde_json::json!(false));
}

// ============================================================================
// 5. Serve session: read → click (DOM mutation) → read with `since` →
//    some diff is non-empty.
// ============================================================================

#[tokio::test]
async fn serve_read_after_click_mutation_shows_non_empty_diff() {
    let server = MockServer::start().await;
    // Page starts with one button. Clicking it appends a new `<a>` to
    // the DOM. The second `read --since` should see the link in
    // `actions_added` AND `text_changed: true`.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"<!doctype html><html><body>
                <button id="add">Add Link</button>
                <script>
                document.getElementById('add').addEventListener('click', () => {
                    const a = document.createElement('a');
                    a.href = '/dashboard';
                    a.textContent = 'Dashboard';
                    document.body.appendChild(a);
                });
                </script>
            </body></html>"#,
        ))
        .mount(&server)
        .await;
    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ready = client.read_ready();

    let open_res = client.call("open", serde_json::json!({ "url": server.uri() }));
    let page_id = open_res["page_id"].as_str().expect("page_id").to_owned();

    let read1 = client.call("read", serde_json::json!({ "page_id": page_id }));
    let hash1 = read1["content_hash"].as_str().expect("hash1").to_owned();

    // Find the button's ref and click it.
    let actions = read1["actions"].as_array().expect("actions array");
    let button = actions
        .iter()
        .find(|a| a.get("tag").and_then(|t| t.as_str()) == Some("button"))
        .expect("button in actions");
    let button_ref = button["ref"].as_str().expect("button ref").to_owned();
    let click_res = client.call(
        "click",
        serde_json::json!({ "page_id": page_id, "ref": button_ref }),
    );
    assert_eq!(click_res["ok"], serde_json::json!(true));

    // Second read with the prior content_hash. We expect either the
    // newly-injected anchor to surface in `actions_added` (if the
    // post-click action graph is re-extracted) OR `text_changed: true`
    // (the visible text gained "Dashboard"). At minimum, ONE of those
    // diff fields must be set — the whole point of `--since` is the
    // mutation has to surface somewhere.
    let read2 = client.call(
        "read",
        serde_json::json!({ "page_id": page_id, "since": hash1 }),
    );
    let delta = read2["delta"].as_object().expect("delta object");
    assert_eq!(
        delta.get("since_matched").and_then(|v| v.as_bool()),
        Some(true),
        "since_matched should be true after click — store keyed by URL, not by page state; delta={delta:?}",
    );
    let actions_added_non_empty = delta["actions_added"]
        .as_array()
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    let text_changed = delta["text_changed"].as_bool().unwrap_or(false);
    assert!(
        actions_added_non_empty || text_changed,
        "expected at least one diff field non-empty after DOM mutation; delta={delta:?}",
    );
}

// ============================================================================
// 6. Snapshot LRU: 9 distinct URLs read → URL #1's snapshot evicted →
//    re-reading URL #1 with the old hash returns since_matched: false.
// ============================================================================

#[tokio::test]
async fn serve_read_snapshot_lru_evicts_past_cap() {
    let server = MockServer::start().await;
    // Stand up 9 distinct routes — each renders a unique body so the
    // content_hash is different per URL.
    for i in 0..9u32 {
        let path_str = format!("/p{i}");
        let body = format!(
            "<!doctype html><html><body><h1>Page {i}</h1><a href=\"/x\">Link {i}</a></body></html>"
        );
        Mock::given(method("GET"))
            .and(path(path_str.clone()))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    }

    let (child, mut client) = spawn_serve();
    let _guard = KillOnDrop(child);
    let _ready = client.read_ready();

    // Open URL #0 first and capture its content_hash. The act of
    // `read` installs the snapshot under URL #0 at position 0 of the
    // LRU.
    let url0 = format!("{}/p0", server.uri());
    let open0 = client.call("open", serde_json::json!({ "url": &url0 }));
    let page_id0 = open0["page_id"].as_str().expect("page_id").to_owned();
    let read0 = client.call("read", serde_json::json!({ "page_id": page_id0 }));
    let hash0 = read0["content_hash"].as_str().expect("hash0").to_owned();

    // Now read 8 OTHER URLs. With SNAPSHOT_LRU_CAP=8 and URL #0
    // already in the store, after 8 more inserts the back of the LRU
    // (URL #0) must have been evicted. The store can only hold 8.
    for i in 1..9u32 {
        let url = format!("{}/p{i}", server.uri());
        let open_n = client.call("open", serde_json::json!({ "url": url }));
        let pid_n = open_n["page_id"].as_str().expect("page_id").to_owned();
        let _ = client.call("read", serde_json::json!({ "page_id": pid_n }));
    }

    // Re-open URL #0 and pass the original hash. Because URL #0's
    // snapshot was evicted, the lookup must fall through to the
    // "no prior snapshot" branch → since_matched: false.
    let open0b = client.call("open", serde_json::json!({ "url": &url0 }));
    let pid_0b = open0b["page_id"].as_str().expect("page_id").to_owned();
    let read0b = client.call(
        "read",
        serde_json::json!({ "page_id": pid_0b, "since": hash0 }),
    );
    let delta = read0b["delta"].as_object().expect("delta object");
    assert_eq!(
        delta.get("since_matched").and_then(|v| v.as_bool()),
        Some(false),
        "URL #0 should have been evicted from the LRU; delta={delta:?}",
    );
}
