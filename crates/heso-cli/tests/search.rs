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
// `heso search` top-level CLI tests — SearXNG path (only backend whose URL
// is configurable from outside the binary, so the one we can wiremock end-
// to-end without hijacking real upstream hosts).
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
async fn searxng_5xx_surfaces_rate_limited_and_exits_nonzero() {
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
    // A 5xx that survives the retry layer is a throttle, not a silent
    // empty: the sole backend is blocked, so the process exits non-zero
    // and the envelope reports it loudly.
    assert!(
        !out.status.success(),
        "all-blocked sweep must exit non-zero: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert!(v["results"].as_array().unwrap().is_empty());
    assert!(
        v["engines_used"].as_array().unwrap().is_empty(),
        "engines_used must NOT include a blocked searxng: {v}"
    );
    let blocked: Vec<&str> = v["blocked"]
        .as_array()
        .expect("blocked array")
        .iter()
        .filter_map(|e| e.as_str())
        .collect();
    assert_eq!(blocked, vec!["searxng"]);
    let errors = v["errors"].as_array().expect("errors array");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0]["engine"], serde_json::json!("searxng"));
    assert_eq!(errors[0]["code"], serde_json::json!("rate_limited"));
    assert_eq!(errors[0]["http_status"], serde_json::json!(500));
}

#[tokio::test(flavor = "multi_thread")]
async fn searxng_429_envelope_lists_blocked_and_exits_nonzero() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let out = run_search(&[
        "any",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
    ]);
    assert!(
        !out.status.success(),
        "a 429 from the only backend must exit non-zero"
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert!(v["engines_used"].as_array().unwrap().is_empty());
    let blocked: Vec<&str> = v["blocked"]
        .as_array()
        .expect("blocked array")
        .iter()
        .filter_map(|e| e.as_str())
        .collect();
    assert_eq!(blocked, vec!["searxng"]);
    let errors = v["errors"].as_array().expect("errors array");
    assert_eq!(errors[0]["code"], serde_json::json!("rate_limited"));
    assert_eq!(errors[0]["http_status"], serde_json::json!(429));
}

#[tokio::test(flavor = "multi_thread")]
async fn searxng_html_when_json_requested_is_config_error() {
    let server = MockServer::start().await;
    // A public instance with JSON output disabled answers `format=json`
    // with its HTML search page — a config error, not a throttle.
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<!doctype html><html><body>searx</body></html>"),
        )
        .mount(&server)
        .await;

    let out = run_search(&[
        "any",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
    ]);
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let errors = v["errors"].as_array().expect("errors array");
    assert_eq!(errors[0]["engine"], serde_json::json!("searxng"));
    assert_eq!(errors[0]["code"], serde_json::json!("config_error"));
    let blocked: Vec<&str> = v["blocked"]
        .as_array()
        .expect("blocked array")
        .iter()
        .filter_map(|e| e.as_str())
        .collect();
    assert_eq!(blocked, vec!["searxng"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn searxng_clean_200_has_null_errors() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "title": "clean",
                "url": "https://example.com/clean",
                "content": "no errors expected"
            }]
        })))
        .mount(&server)
        .await;

    let out = run_search(&[
        "any",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
    ]);
    let v = parse_stdout(&out);
    assert!(
        v["errors"].is_null(),
        "errors must be null when every attempted backend returned results: {v}"
    );
    assert!(
        v["blocked"].is_null(),
        "blocked must be null when nothing was throttled: {v}"
    );
    let engines: Vec<&str> = v["engines_used"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e.as_str())
        .collect();
    assert_eq!(engines, vec!["searxng"]);
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
async fn unknown_engine_named_after_new_pool_still_rejected() {
    // The closed pool grew (`brave`, `marginalia`, `ddg-lite`), but a name
    // outside it must still be rejected at parse time — before any fetch,
    // so no network is touched. (The accept-side of the new names is
    // covered by the network-free `parse_engines_csv_accepts_known` unit
    // test; driving them end-to-end would hit live upstream hosts.)
    let out = Command::new(heso_bin())
        .args(["search", "q", "--engines", "ddglite"])
        .env_remove("HESO_SEARX_URL")
        .output()
        .expect("spawn heso search");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown engine"),
        "a misspelled engine must be rejected with the supported list: {stderr}"
    );
    // The error message lists the new wire names so an agent can correct.
    assert!(
        stderr.contains("ddg-lite") && stderr.contains("brave") && stderr.contains("marginalia"),
        "supported-engine hint must include the new names: {stderr}"
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

#[tokio::test(flavor = "multi_thread")]
async fn timeout_against_slow_backend_surfaces_loud_envelope() {
    // A backend that holds the connection past the per-request `--timeout`
    // must surface a loud throttle envelope (a timed-out request is a
    // retryable transport failure → `rate_limited` after retries exhaust),
    // never a silent empty. With `--timeout 1s` the inner reqwest deadline
    // fires on each attempt well before any wall-clock backstop.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_secs(30))
                .set_body_json(serde_json::json!({ "results": [] })),
        )
        .mount(&server)
        .await;

    let out = run_search(&[
        "slow-query-timeout",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
        "--timeout",
        "1s",
    ]);
    // Sole backend timed out → all-blocked sweep exits non-zero.
    assert!(
        !out.status.success(),
        "a timed-out sole backend must exit non-zero: stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert!(v["results"].as_array().unwrap().is_empty());
    assert!(
        v["engines_used"].as_array().unwrap().is_empty(),
        "a timed-out backend must NOT be listed as used: {v}"
    );
    let blocked: Vec<&str> = v["blocked"]
        .as_array()
        .expect("blocked array")
        .iter()
        .filter_map(|e| e.as_str())
        .collect();
    assert_eq!(blocked, vec!["searxng"]);
    let errors = v["errors"].as_array().expect("errors array");
    assert_eq!(errors[0]["engine"], serde_json::json!("searxng"));
    assert_eq!(errors[0]["code"], serde_json::json!("rate_limited"));
}

#[tokio::test(flavor = "multi_thread")]
async fn on_disk_cache_collapses_repeat_query_to_one_http_hit() {
    // Two identical queries in the same data directory hit the backend
    // exactly once: the first fetch writes the on-disk cache, the second
    // reads it and short-circuits the HTTP call (A.6). Both `heso`
    // subprocesses run from a shared tempdir so the cache is isolated
    // from the repo and visible across the two runs.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "title": "cached row",
                "url": "https://example.com/cached",
                "content": "served once, cached thereafter"
            }]
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let args = [
        "search",
        "cache-collapse-query",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
    ];
    let run = || {
        Command::new(heso_bin())
            .args(args)
            .current_dir(dir.path())
            .env_remove("HESO_SEARX_URL")
            .output()
            .expect("spawn heso search")
    };

    let first = run();
    assert!(first.status.success(), "first run failed");
    let v1: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(v1["results"].as_array().unwrap().len(), 1);

    let second = run();
    assert!(second.status.success(), "second run failed");
    let v2: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(v2["results"].as_array().unwrap().len(), 1);
    assert_eq!(v2["results"][0]["url"], v1["results"][0]["url"]);

    let hits = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        hits.len(),
        1,
        "the cache must collapse two identical queries to ONE HTTP hit, saw {}",
        hits.len()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cache_ttl_zero_disables_short_circuit() {
    // `HESO_SEARCH_CACHE_TTL=0` opts the process out of the cache, so two
    // identical queries hit the backend twice — proving the short-circuit
    // above is the cache, not query dedupe elsewhere.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "results": [{
                "title": "uncached",
                "url": "https://example.com/uncached",
                "content": "fetched every time"
            }]
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let args = [
        "search",
        "cache-disabled-query",
        "--engines",
        "searxng",
        "--searx-url",
        &server.uri(),
    ];
    let run = || {
        Command::new(heso_bin())
            .args(args)
            .current_dir(dir.path())
            .env_remove("HESO_SEARX_URL")
            .env("HESO_SEARCH_CACHE_TTL", "0")
            .output()
            .expect("spawn heso search")
    };
    let _ = run();
    let _ = run();
    let hits = server.received_requests().await.expect("recorded requests");
    assert_eq!(hits.len(), 2, "TTL=0 must disable the cache short-circuit");
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
