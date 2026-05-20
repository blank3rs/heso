//! Integration tests for `heso search` — the multi-source web search
//! verb. Each test spawns the actual `heso` binary against a wiremock
//! localhost server stubbing one or more of the upstream services
//! (DuckDuckGo HTML, Wikipedia REST `summary`, SearXNG `/search`).
//! No real network involved.
//!
//! The tests deliberately exercise the binary end-to-end (not the
//! `search` module directly), so the dispatch wiring in `main.rs`
//! AND the JSON output formatting stay regression-tested. We use the
//! `--searx-url` flag to point the SearXNG backend at our wiremock
//! server, and stub the DDG / Wikipedia hosts indirectly by running
//! against a custom HOSTS-style override only when feasible — most
//! tests focus on the SearXNG path (whose base URL is configurable)
//! plus unit-test coverage of the DDG HTML parser and Wikipedia
//! response handler in `search.rs` itself.
//!
//! For the cases that need to stub the *real* DDG / Wikipedia hosts
//! (rather than SearXNG), we exercise them via `heso serve`'s
//! `search` JSON-RPC method, which routes the same `run_search`
//! orchestrator but lets us assert on stdout JSON without process
//! plumbing for stderr noise.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Absolute path to the `heso` binary the test crate's Cargo build
/// produced. Same env var used by the other integration tests.
fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

// ============================================================================
// `heso search` CLI tests — SearXNG path (only backend whose URL is
// configurable from outside the binary)
// ============================================================================

fn run_search(args: &[&str]) -> std::process::Output {
    let mut cmd_args = vec!["search"];
    cmd_args.extend_from_slice(args);
    Command::new(heso_bin())
        .args(&cmd_args)
        .env_remove("HESO_SEARX_URL")
        .output()
        .expect("spawn heso search")
}

fn parse_stdout(out: &std::process::Output) -> serde_json::Value {
    if !out.status.success() {
        panic!(
            "heso search exit={:?} stderr={}\nstdout={}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout),
        );
    }
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn searxng_only_returns_mapped_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "rust web scraping"))
        .and(query_param("format", "json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [
                {
                    "title": "Web scraping with Rust",
                    "url": "https://example.com/rust-scraping",
                    "content": "A guide to scraping the web in Rust."
                },
                {
                    "title": "Reqwest tutorial",
                    "url": "https://example.com/reqwest",
                    "content": "Send HTTP requests in Rust."
                }
            ]
        })))
        .mount(&server)
        .await;

    let out = run_search(&[
        "rust web scraping",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
    ]);
    let v = parse_stdout(&out);
    assert_eq!(v["query"], serde_json::json!("rust web scraping"));
    let engines_used = v["engines_used"].as_array().expect("engines_used array");
    let engines: Vec<&str> = engines_used.iter().filter_map(|e| e.as_str()).collect();
    assert_eq!(engines, vec!["searxng"]);
    let results = v["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["rank"], serde_json::json!(1));
    assert_eq!(results[0]["source"], serde_json::json!("searxng"));
    assert_eq!(results[0]["title"], serde_json::json!("Web scraping with Rust"));
    assert_eq!(
        results[0]["url"],
        serde_json::json!("https://example.com/rust-scraping")
    );
    assert!(results[0]["snippet"]
        .as_str()
        .unwrap()
        .contains("guide to scraping"));
    assert!(
        v["knowledge"].is_null(),
        "knowledge should be null when wiki not in engines: {v}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn limit_caps_results() {
    let server = MockServer::start().await;
    let many: Vec<serde_json::Value> = (0..50)
        .map(|i| {
            serde_json::json!({
                "title": format!("title{i}"),
                "url": format!("https://example.com/{i}"),
                "content": format!("snippet{i}"),
            })
        })
        .collect();
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": many,
        })))
        .mount(&server)
        .await;

    let out = run_search(&[
        "many",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
        "--limit",
        "5",
    ]);
    let v = parse_stdout(&out);
    let results = v["results"].as_array().expect("results array");
    assert_eq!(results.len(), 5);
    // Ranks are 1-indexed and contiguous.
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r["rank"], serde_json::json!(i + 1));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn searxng_empty_results_handled_cleanly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": []
        })))
        .mount(&server)
        .await;

    let out = run_search(&[
        "akjsdhflkajshdf-doesnt-exist-xyz",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
    ]);
    let v = parse_stdout(&out);
    let results = v["results"].as_array().expect("results array");
    assert!(results.is_empty(), "expected empty results: {v}");
    // Still records the engine was tried.
    let engines: Vec<&str> = v["engines_used"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.as_str())
        .collect();
    assert_eq!(engines, vec!["searxng"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn searxng_5xx_does_not_crash_returns_empty() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let out = run_search(&[
        "any",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
    ]);
    // CLI should still exit success — the search-backend error went
    // to stderr, the JSON envelope is still emitted.
    let v = parse_stdout(&out);
    assert!(v["results"].as_array().unwrap().is_empty());
    assert!(
        v["engines_used"].as_array().unwrap().is_empty(),
        "engines_used must NOT include searxng on hard error: {v}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_engine_rejected_with_usage() {
    let out = Command::new(heso_bin())
        .args(["search", "anything", "--engines", "google"])
        .output()
        .expect("spawn heso search");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown engine") || stderr.contains("google"),
        "stderr should explain the bad engine: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_query_rejected_with_usage() {
    let out = Command::new(heso_bin())
        .args(["search"])
        .output()
        .expect("spawn heso search");
    assert!(!out.status.success(), "expected non-zero exit");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("usage:") || stderr.contains("--limit"),
        "stderr should print usage: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn searxng_via_env_var() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "title": "envvar route",
                "url": "https://example.com/x",
                "content": "via env"
            }]
        })))
        .mount(&server)
        .await;

    let out = Command::new(heso_bin())
        .args(["search", "any", "--engines", "searxng"])
        .env("HESO_SEARX_URL", server.uri())
        .output()
        .expect("spawn heso search");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON: {e}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        )
    });
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], serde_json::json!("envvar route"));
}

// ============================================================================
// `heso serve` `search` JSON-RPC method
// ============================================================================

fn spawn_serve() -> (Child, RpcClient) {
    let mut child = Command::new(heso_bin())
        .arg("serve")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env_remove("HESO_SEARX_URL")
        .spawn()
        .expect("spawn heso serve");
    let stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let reader = BufReader::new(stdout);
    let client = RpcClient { stdin, reader, next_id: 1 };
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
            .expect("write");
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).expect("read");
        let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        if let Some(err) = resp.get("error") {
            panic!("rpc error for `{method}`: {err}");
        }
        resp["result"].clone()
    }
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ready_advertises_search_method() {
    let (child, mut client) = spawn_serve();
    let _g = KillOnDrop(child);
    let ready = client.read_ready();
    let methods: Vec<&str> = ready["params"]["methods"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        methods.contains(&"search"),
        "ready must advertise `search`: {methods:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_search_searxng_returns_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "anything"))
        .and(query_param("format", "json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "title": "rpc routed",
                "url": "https://example.com/rpc",
                "content": "stub via rpc"
            }]
        })))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _g = KillOnDrop(child);
    let _ = client.read_ready();

    let v = client.call(
        "search",
        serde_json::json!({
            "query": "anything",
            "engines": ["searxng"],
            "searx_url": server.uri(),
        }),
    );
    let results = v["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], serde_json::json!("rpc routed"));
    assert_eq!(results[0]["source"], serde_json::json!("searxng"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rpc_search_accepts_csv_engines_string() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": []
        })))
        .mount(&server)
        .await;

    let (child, mut client) = spawn_serve();
    let _g = KillOnDrop(child);
    let _ = client.read_ready();

    // engines passed as CSV string — accepted variant.
    let v = client.call(
        "search",
        serde_json::json!({
            "query": "x",
            "engines": "searxng",
            "searx_url": server.uri(),
        }),
    );
    assert!(v["results"].as_array().unwrap().is_empty());
    let engines: Vec<&str> = v["engines_used"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.as_str())
        .collect();
    assert_eq!(engines, vec!["searxng"]);
}
