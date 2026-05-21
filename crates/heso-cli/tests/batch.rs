//! Integration tests for `heso batch <subverb> <urls...>` — the
//! parallel multi-URL scraping verb.
//!
//! Every test spawns the real `heso` binary against hermetic
//! wiremock-rs localhost servers (one per fixture URL or a shared
//! server with multiple mounted paths). No real network involved.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

/// Run `heso batch <args...>` with no stdin. Returns the raw `Output`
/// for the caller to inspect status + stdout + stderr.
fn run_batch(args: &[&str]) -> std::process::Output {
    let mut full = vec!["batch"];
    full.extend_from_slice(args);
    Command::new(heso_bin())
        .args(&full)
        .output()
        .expect("spawn heso batch")
}

/// Parse stdout as JSON-Lines — one `serde_json::Value` per line.
/// Skips blank lines.
fn parse_jsonl(out: &std::process::Output) -> Vec<serde_json::Value> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| {
                panic!(
                    "non-JSON line in stdout: {e}\nline: {l}\nstderr: {}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
        })
        .collect()
}

// ============================================================================
// Test 1: 3 URLs, all 200 → 3 rows ok:true, exit 0
// ============================================================================

#[tokio::test]
async fn batch_open_three_urls_all_succeed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><title>Page A</title><body>A</body>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><title>Page B</title><body>B</body>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/c"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><title>Page C</title><body>C</body>"),
        )
        .mount(&server)
        .await;

    let url_a = format!("{}/a", server.uri());
    let url_b = format!("{}/b", server.uri());
    let url_c = format!("{}/c", server.uri());
    let out = run_batch(&["open", &url_a, &url_b, &url_c]);

    assert!(
        out.status.success(),
        "expected success, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 3, "expected 3 rows, got: {rows:?}");
    for row in &rows {
        assert_eq!(row["ok"], serde_json::json!(true), "row not ok: {row}");
        assert!(row["url"].is_string());
        assert!(row["title"].is_string(), "missing title: {row}");
    }
    // All three URLs are represented (order may vary — that's the contract).
    let urls: Vec<&str> = rows.iter().filter_map(|r| r["url"].as_str()).collect();
    assert!(urls.iter().any(|u| u.ends_with("/a")), "no /a: {urls:?}");
    assert!(urls.iter().any(|u| u.ends_with("/b")), "no /b: {urls:?}");
    assert!(urls.iter().any(|u| u.ends_with("/c")), "no /c: {urls:?}");
}

// ============================================================================
// Test 2: 3 URLs, 1 returns 500 → 2 ok + 1 with `error`, exit 0
// ============================================================================

#[tokio::test]
async fn batch_open_partial_failure_exits_zero() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ok1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>ok1</title>"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/ok2"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>ok2</title>"))
        .mount(&server)
        .await;
    // reqwest does NOT treat a 500 as an `Err` by default — it returns
    // a `Response` with `.status()` 500 and `text()` still works.
    // So the `open` static path will see it as a successful fetch and
    // emit ok:true with whatever body it got. To force an error row,
    // we need a path that fails AT the transport layer — point one URL
    // at a closed port.
    let bad_url = "http://127.0.0.1:1/never-listens".to_owned();
    let url_ok1 = format!("{}/ok1", server.uri());
    let url_ok2 = format!("{}/ok2", server.uri());

    let out = run_batch(&["open", &url_ok1, &bad_url, &url_ok2]);
    assert!(
        out.status.success(),
        "expected success (any-ok); stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 3, "expected 3 rows: {rows:?}");

    let ok_count = rows
        .iter()
        .filter(|r| r["ok"] == serde_json::json!(true))
        .count();
    let err_count = rows
        .iter()
        .filter(|r| r["ok"] == serde_json::json!(false))
        .count();
    assert_eq!(ok_count, 2, "expected 2 ok rows: {rows:?}");
    assert_eq!(err_count, 1, "expected 1 err row: {rows:?}");

    let err_row = rows
        .iter()
        .find(|r| r["ok"] == serde_json::json!(false))
        .expect("err row");
    assert!(
        err_row["error"].is_string(),
        "missing error string: {err_row}"
    );
    assert!(
        err_row["url"].as_str().unwrap().contains("127.0.0.1:1"),
        "err row url wrong: {err_row}"
    );
}

// ============================================================================
// Test 3: 3 URLs, ALL fail → exit 1
// ============================================================================

#[tokio::test]
async fn batch_open_all_fail_exits_one() {
    // Three closed ports — every fetch errors at the transport layer.
    let urls = [
        "http://127.0.0.1:1/x",
        "http://127.0.0.1:1/y",
        "http://127.0.0.1:1/z",
    ];
    let out = run_batch(&["open", urls[0], urls[1], urls[2]]);

    assert!(
        !out.status.success(),
        "expected failure (all errored); stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 3, "expected 3 rows: {rows:?}");
    for row in &rows {
        assert_eq!(row["ok"], serde_json::json!(false), "row not err: {row}");
        assert!(row["error"].is_string(), "missing error: {row}");
    }
}

// ============================================================================
// Test 4: --parallel 1 → output order matches input order
// ============================================================================

#[tokio::test]
async fn batch_open_parallel_one_preserves_input_order() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/first"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>first</title>"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/second"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>second</title>"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/third"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>third</title>"))
        .mount(&server)
        .await;

    let url_1 = format!("{}/first", server.uri());
    let url_2 = format!("{}/second", server.uri());
    let url_3 = format!("{}/third", server.uri());
    let out = run_batch(&["open", "--parallel", "1", &url_1, &url_2, &url_3]);
    assert!(
        out.status.success(),
        "expected success; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 3, "expected 3 rows: {rows:?}");

    // Under --parallel 1, completion order == input order.
    assert!(rows[0]["url"].as_str().unwrap().ends_with("/first"));
    assert!(rows[1]["url"].as_str().unwrap().ends_with("/second"));
    assert!(rows[2]["url"].as_str().unwrap().ends_with("/third"));
}

// ============================================================================
// Test 5: --fail-fast stops on first error → exit 1, partial output
// ============================================================================

#[tokio::test]
async fn batch_open_fail_fast_stops_on_first_error() {
    // Run with --parallel 1 + ordered input so the order of completion
    // is deterministic. First URL fails (closed port). With --fail-fast
    // the second URL should NOT be processed.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/never-reached"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>x</title>"))
        .mount(&server)
        .await;

    let bad = "http://127.0.0.1:1/fail-first".to_owned();
    let url_2 = format!("{}/never-reached", server.uri());
    let out = run_batch(&["open", "--parallel", "1", "--fail-fast", &bad, &url_2]);

    assert!(
        !out.status.success(),
        "expected failure under fail-fast; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    // Exactly one row — the failing one. The second URL should never
    // have made it to stdout.
    assert_eq!(
        rows.len(),
        1,
        "expected exactly 1 row under fail-fast: {rows:?}"
    );
    assert_eq!(rows[0]["ok"], serde_json::json!(false));
}

// ============================================================================
// Test 6: --timeout-per-url against a slow endpoint → `timeout` error
// ============================================================================

#[tokio::test]
async fn batch_open_timeout_per_url_triggers_timeout_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<title>slow</title>")
                .set_delay(std::time::Duration::from_millis(2_000)),
        )
        .mount(&server)
        .await;

    let url = format!("{}/slow", server.uri());
    let out = run_batch(&["open", "--timeout-per-url", "100ms", &url]);
    // 1-of-1 failed → exit 1
    assert!(
        !out.status.success(),
        "expected failure; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["ok"], serde_json::json!(false));
    let err = rows[0]["error"].as_str().unwrap_or("");
    assert!(
        err.contains("timeout"),
        "expected 'timeout' in error: {err}"
    );
    assert!(err.contains("100ms"), "expected duration in error: {err}");
}

// ============================================================================
// Test 7: cookies persist across batch URLs (--parallel 1, server A
// sets cookie, server B reads it).
// ============================================================================

#[tokio::test]
async fn batch_open_cookies_persist_across_urls() {
    let server = MockServer::start().await;
    // Step 1: `/set` issues a Set-Cookie header.
    Mock::given(method("GET"))
        .and(path("/set"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Set-Cookie", "ses=hello; Path=/")
                .set_body_string("<title>set</title>"),
        )
        .mount(&server)
        .await;
    // Step 2: `/check` returns 200 OR 401 depending on whether the
    // `ses=hello` cookie was sent. We emulate that by mounting two
    // mocks against the same path, one with a `Cookie: ses=hello`
    // header matcher and a higher priority.
    Mock::given(method("GET"))
        .and(path("/check"))
        .and(wiremock::matchers::header("cookie", "ses=hello"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>cookie-OK</title>"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/check"))
        .respond_with(ResponseTemplate::new(401).set_body_string("<title>missing-cookie</title>"))
        .mount(&server)
        .await;

    let url_set = format!("{}/set", server.uri());
    let url_check = format!("{}/check", server.uri());
    // Order matters AND we need --parallel 1 so the `set` request
    // completes before the `check` request fires.
    let out = run_batch(&["open", "--parallel", "1", &url_set, &url_check]);

    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 2, "expected 2 rows: {rows:?}");
    // The second row's title should be "cookie-OK", proving the
    // cookie travelled.
    let check_row = rows
        .iter()
        .find(|r| r["url"].as_str().unwrap_or("").ends_with("/check"))
        .expect("check row");
    let title = check_row["title"].as_str().unwrap_or("");
    assert_eq!(
        title, "cookie-OK",
        "cookie did NOT persist across batch URLs; got title={title}, row={check_row}"
    );
}

// ============================================================================
// Test 8: stdin mode — URLs read from stdin, default subverb is `open`
// ============================================================================

#[tokio::test]
async fn batch_reads_urls_from_stdin_default_subverb_open() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/p1"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>p1</title>"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/p2"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>p2</title>"))
        .mount(&server)
        .await;

    let url_1 = format!("{}/p1", server.uri());
    let url_2 = format!("{}/p2", server.uri());
    let stdin_input = format!("{url_1}\n{url_2}\n");

    let mut child = Command::new(heso_bin())
        .arg("batch")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn heso batch with stdin");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(stdin_input.as_bytes())
        .expect("write stdin");
    // Drop stdin so child sees EOF.
    drop(child.stdin.take());
    let out = child.wait_with_output().expect("wait child");

    assert!(
        out.status.success(),
        "expected success; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 2, "expected 2 rows: {rows:?}");
    for row in &rows {
        assert_eq!(row["ok"], serde_json::json!(true));
    }
}

// ============================================================================
// Test 9: invalid URL → ok:false with `invalid_url:` tag, doesn't kill batch
// ============================================================================

#[tokio::test]
async fn batch_open_invalid_url_emits_classified_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/good"))
        .respond_with(ResponseTemplate::new(200).set_body_string("<title>good</title>"))
        .mount(&server)
        .await;

    let good_url = format!("{}/good", server.uri());
    let out = run_batch(&["open", "not-a-url", &good_url]);

    // Mixed: one succeeded, exit 0.
    assert!(out.status.success(), "expected success (any-ok)");
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 2);
    let err_row = rows.iter().find(|r| r["ok"] == serde_json::json!(false));
    assert!(err_row.is_some(), "expected one err row: {rows:?}");
    let tag = err_row.unwrap()["error"].as_str().unwrap_or("");
    assert!(
        tag.starts_with("invalid_url:"),
        "expected `invalid_url:` tag, got: {tag}"
    );
}

// ============================================================================
// Test 10 (bug-report 04, Bug B): `batch read --include cookies --parallel N`
// emits a per-URL cookies snapshot that reflects *only this response's*
// `Set-Cookie` headers — sibling tasks finishing first cannot leak their
// cookies into our row.
//
// Fixture: two URLs, each setting a distinct cookie. With the old
// `jar.matches(url)` scan at JSON-serialize time, a row's `cookies`
// would contain both cookies whenever the sibling completed first.
// With the eager `FetchPage::response_cookies` snapshot, each row sees
// only the cookies its own response set, regardless of completion
// ordering.
// ============================================================================

#[tokio::test]
async fn batch_read_cookies_are_per_response_not_jar_snapshot() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Set-Cookie", "cook_a=A; Path=/")
                .set_body_string("<title>A</title><body>page A</body>"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/b"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("Set-Cookie", "cook_b=B; Path=/")
                .set_body_string("<title>B</title><body>page B</body>"),
        )
        .mount(&server)
        .await;

    let url_a = format!("{}/a", server.uri());
    let url_b = format!("{}/b", server.uri());
    let out = run_batch(&[
        "read",
        "--parallel",
        "4",
        "--include",
        "cookies",
        &url_a,
        &url_b,
    ]);

    assert!(
        out.status.success(),
        "expected success; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 2, "expected 2 rows: {rows:?}");

    let row_a = rows
        .iter()
        .find(|r| r["url"].as_str().unwrap_or("").ends_with("/a"))
        .expect("a row");
    let row_b = rows
        .iter()
        .find(|r| r["url"].as_str().unwrap_or("").ends_with("/b"))
        .expect("b row");

    let a_cookies = row_a["cookies"].as_array().expect("a.cookies array");
    let b_cookies = row_b["cookies"].as_array().expect("b.cookies array");
    assert_eq!(
        a_cookies.len(),
        1,
        "row A's cookies should contain only cook_a; got: {a_cookies:?}"
    );
    assert_eq!(
        b_cookies.len(),
        1,
        "row B's cookies should contain only cook_b; got: {b_cookies:?}"
    );
    assert_eq!(a_cookies[0]["name"].as_str(), Some("cook_a"));
    assert_eq!(b_cookies[0]["name"].as_str(), Some("cook_b"));
}

// ============================================================================
// Test 11 (bug-report 04, Bug B determinism): the same fixture as Test 10,
// run twice. Both runs must emit identical cookie counts per row even
// though task completion order varies under `--parallel`. This is the
// determinism property the race fix establishes.
// ============================================================================

#[tokio::test]
async fn batch_read_cookies_are_deterministic_across_runs() {
    let server = MockServer::start().await;
    for (p, name) in [("/c1", "c1"), ("/c2", "c2"), ("/c3", "c3"), ("/c4", "c4")] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header(
                        "Set-Cookie",
                        format!("{name}=v; Path=/").as_str(),
                    )
                    .set_body_string("<title>x</title>"),
            )
            .mount(&server)
            .await;
    }

    let urls: Vec<String> = (1..=4).map(|i| format!("{}/c{i}", server.uri())).collect();
    let mut args = vec!["read", "--parallel", "4", "--include", "cookies"];
    for u in &urls {
        args.push(u.as_str());
    }

    let r1 = run_batch(&args);
    assert!(r1.status.success(), "run 1 failed: {}", String::from_utf8_lossy(&r1.stderr));
    let rows_1 = parse_jsonl(&r1);

    let r2 = run_batch(&args);
    assert!(r2.status.success(), "run 2 failed: {}", String::from_utf8_lossy(&r2.stderr));
    let rows_2 = parse_jsonl(&r2);

    assert_eq!(rows_1.len(), 4);
    assert_eq!(rows_2.len(), 4);
    for rows in [&rows_1, &rows_2] {
        for row in rows {
            assert_eq!(row["ok"], serde_json::json!(true));
            let cookies = row["cookies"].as_array().expect("cookies array");
            assert_eq!(
                cookies.len(),
                1,
                "expected exactly one per-response cookie; got: {row:?}"
            );
        }
    }
}

// ============================================================================
// Test 12 (bug-report 04, Bug A): host-only cookies surface `host_only:
// true` and fill `domain` with the request URL's effective host, NOT an
// empty string.
//
// RFC 6265 §5.3 step 6 default: a `Set-Cookie` with no `Domain=`
// attribute creates a "host-only" cookie scoped to the request URL's
// host. The previous JSON emitted `"domain": ""` for these — the agent
// could not tell a host-only cookie from a domain-wide cookie that
// happened to have an empty `Domain=` (which is undefined behavior).
// ============================================================================

#[tokio::test]
async fn batch_read_cookies_emit_host_only_flag_for_no_domain_attribute() {
    let server = MockServer::start().await;
    // Two cookies on one response:
    //   `host_only_cook=H`         — no Domain attribute (host-only)
    //   `wide_cook=W; Domain=...`  — explicit Domain (domain-wide)
    //
    // Use 127.0.0.1 so the test doesn't depend on a real public suffix.
    // reqwest will accept `Domain=127.0.0.1` against a 127.0.0.1 request.
    Mock::given(method("GET"))
        .and(path("/h"))
        .respond_with(
            ResponseTemplate::new(200)
                // `append_header` (vs `insert_header`) preserves prior values
                // for the same key — required since `Set-Cookie` is the
                // canonical "one header line per cookie" case.
                .append_header("Set-Cookie", "host_only_cook=H; Path=/")
                .append_header("Set-Cookie", "wide_cook=W; Domain=127.0.0.1; Path=/")
                .set_body_string("<title>h</title>"),
        )
        .mount(&server)
        .await;

    let url = format!("{}/h", server.uri());
    let out = run_batch(&[
        "read",
        "--parallel",
        "1",
        "--include",
        "cookies",
        &url,
    ]);
    assert!(
        out.status.success(),
        "expected success; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let rows = parse_jsonl(&out);
    assert_eq!(rows.len(), 1, "expected 1 row: {rows:?}");
    let cookies = rows[0]["cookies"]
        .as_array()
        .expect("cookies array on row");
    // Two cookies expected (HttpOnly was NOT set on either).
    assert_eq!(cookies.len(), 2, "expected 2 cookies; got: {cookies:?}");

    let host_only_entry = cookies
        .iter()
        .find(|c| c["name"].as_str() == Some("host_only_cook"))
        .expect("host_only_cook in cookies array");
    let wide_entry = cookies
        .iter()
        .find(|c| c["name"].as_str() == Some("wide_cook"))
        .expect("wide_cook in cookies array");

    // host-only cookie: `host_only: true` + domain == request host.
    assert_eq!(
        host_only_entry["host_only"].as_bool(),
        Some(true),
        "expected host_only=true for cookie with no Domain attr; got: {host_only_entry:?}"
    );
    let host_only_domain = host_only_entry["domain"].as_str().unwrap_or("");
    assert!(
        !host_only_domain.is_empty(),
        "expected non-empty domain for host-only cookie (the effective host); got: {host_only_entry:?}"
    );
    assert_eq!(
        host_only_domain, "127.0.0.1",
        "expected domain == request host '127.0.0.1' for host-only cookie; got: {host_only_entry:?}"
    );

    // domain-wide cookie: `host_only: false` + domain == server's value.
    // reqwest's Cookie::domain() reports the value with a leading `.` per
    // RFC 6265 when sent without it (the spec auto-prepends), but the
    // documented behavior varies — we accept either form.
    assert_eq!(
        wide_entry["host_only"].as_bool(),
        Some(false),
        "expected host_only=false for cookie with explicit Domain attr; got: {wide_entry:?}"
    );
    let wide_domain = wide_entry["domain"].as_str().unwrap_or("");
    assert!(
        wide_domain.contains("127.0.0.1"),
        "expected domain containing '127.0.0.1' for explicit-domain cookie; got: {wide_entry:?}"
    );
}
