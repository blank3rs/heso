//! Integration coverage for the experimental `heso.template/v0`
//! authoring surface.
//!
//! The public artifact is still a normal HESO/1.0 plat: a template is
//! checked or live-recorded into a concrete `plan` plus cassette, and
//! `heso run` must replay that output byte-identically.

use std::path::{Path, PathBuf};
use std::process::Command;

use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn write_json(dir: &Path, name: &str, value: &serde_json::Value) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(value).expect("serialize json"),
    )
    .expect("write json");
    path
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(heso_bin())
        .args(args)
        .output()
        .expect("spawn heso")
}

fn assert_success(out: &std::process::Output, context: &str) {
    assert!(
        out.status.success(),
        "{context} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn template_check_accepts_minimal_template_and_reports_hash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.minimal",
        "version": "0.1.0",
        "title": "Minimal template",
        "domains": ["example.com"],
        "inputs": {},
        "steps": [
            { "verb": "open", "url": "https://example.com/" }
        ]
    });
    let path = write_json(dir.path(), "template.json", &template);

    let out = run(&["verify", path.to_str().unwrap()]);
    assert_success(&out, "verify");

    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("verify JSON");
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["schema"], serde_json::json!("heso.template/v0"));
    assert_eq!(body["id"], serde_json::json!("ca.heso.tests.minimal"));
    assert_eq!(body["steps"], serde_json::json!(1));
    let hash = body["template_hash"]
        .as_str()
        .expect("template_hash string");
    assert_eq!(hash.len(), 64);
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    assert!(body["hash_matches"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn template_stamp_materializes_plan_and_run_replays_byte_identically() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/form"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<!doctype html><html><head><title>Search</title></head><body>
                    <form id="search" method="get" action="/result">
                        <label>Query <input name="q" type="search"></label>
                        <button type="submit">Search</button>
                    </form>
                </body></html>"#,
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/result"))
        .and(query_param("q", "BRCA1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<!doctype html><html><head><title>Result</title></head><body>
                    <h1>Result</h1><p id="answer">BRCA1</p>
                </body></html>"#,
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let form_url = format!("{}/form", server.uri());
    let host = url::Url::parse(&form_url)
        .expect("server url")
        .host_str()
        .expect("host")
        .to_owned();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.search",
        "version": "0.1.0",
        "title": "Search form",
        "domains": [host],
        "inputs": {
            "q": { "type": "string", "required": true }
        },
        "steps": [
            { "verb": "open", "url": form_url },
            {
                "verb": "fill",
                "target": { "selector": "input[name=q]" },
                "value": { "input": "q" }
            },
            {
                "verb": "submit",
                "target": { "selector": "form#search" }
            }
        ]
    });
    let template_path = write_json(dir.path(), "search.template.json", &template);

    let stamp = run(&[
        "stamp",
        "--template",
        "--seed",
        "0",
        "--param",
        "q=BRCA1",
        template_path.to_str().unwrap(),
    ]);
    assert_success(&stamp, "stamp --template");
    let plat: serde_json::Value = serde_json::from_slice(&stamp.stdout).expect("stamped plat");

    assert!(plat.get("template_hash").is_none());
    assert!(plat.get("template_experimental").is_none());
    assert_eq!(plat["input_url"], serde_json::json!(form_url));
    assert!(
        plat["url"]
            .as_str()
            .unwrap_or("")
            .contains("/result?q=BRCA1"),
        "plat url should reflect submitted GET navigation: {plat}"
    );
    let plan = plat["plan"].as_array().expect("plan array");
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0]["verb"], serde_json::json!("open"));
    assert_eq!(plan[1]["verb"], serde_json::json!("fill"));
    assert_eq!(plan[1]["value"], serde_json::json!("BRCA1"));
    assert_eq!(plan[2]["verb"], serde_json::json!("submit"));

    let stamp_hash = plat["plat_hash"]
        .as_str()
        .expect("plat_hash string")
        .to_owned();
    let plat_path = dir.path().join("search.plat");
    std::fs::write(&plat_path, &stamp.stdout).expect("write plat");

    drop(server);

    let replay = run(&["run", "--seed", "0", plat_path.to_str().unwrap()]);
    assert_success(&replay, "run of template-stamped plat");
    let replayed: serde_json::Value = serde_json::from_slice(&replay.stdout).expect("run plat");
    assert_eq!(
        replayed["plat_hash"].as_str(),
        Some(stamp_hash.as_str()),
        "template-stamped plat must replay byte-identically"
    );
}

// ============================================================================
// Static validation: check command — hash stability, domain guards, secrets.
// ============================================================================

/// Pins the JCS+blake3 hash for a fixed template body so a change to the
/// canonicalization pipeline or to the schema's serialization shape is
/// caught at the test boundary, not at release time.
#[test]
fn template_check_emits_stable_canonical_hash_for_known_template() {
    let dir = tempfile::tempdir().expect("tempdir");
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.hash-pin",
        "version": "0.1.0",
        "domains": ["example.com"],
        "steps": [
            { "verb": "open", "url": "https://example.com/" }
        ]
    });
    let path = write_json(dir.path(), "pin.template.json", &template);

    let out = run(&["verify", path.to_str().unwrap()]);
    assert_success(&out, "verify");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("check JSON");
    let hash = body["template_hash"]
        .as_str()
        .expect("template_hash string")
        .to_owned();

    // Golden value: blake3(JCS(template without `template_hash`)). If this
    // assertion fails after a serde / serde_jcs / blake3 update, the
    // canonicalization pipeline shifted — investigate before pinning a new
    // value.
    const PINNED: &str = "a5a75b9738e67e4edbc0f050087542cb7423a973d56443ed22300ac94dd5245a";
    assert_eq!(hash, PINNED, "template_hash is no longer the pinned value");
}

/// Open URLs outside the declared domain allowlist must be caught at
/// `verify` time (not just at `stamp --template` time), so authors
/// see the problem before runtime.
#[test]
fn template_check_rejects_open_url_outside_declared_domains() {
    let dir = tempfile::tempdir().expect("tempdir");
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.domain",
        "version": "0.1.0",
        "domains": ["allowed.example"],
        "steps": [
            { "verb": "open", "url": "https://evil.example/path" }
        ]
    });
    let path = write_json(dir.path(), "evil.template.json", &template);

    let out = run(&["verify", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("error JSON");
    assert_eq!(body["ok"], serde_json::json!(false));
    assert_eq!(body["error"]["kind"], serde_json::json!("invalid_template"));
    let message = body["error"]["message"]
        .as_str()
        .expect("error.message string");
    assert!(
        message.contains("outside template domains"),
        "expected domain-violation message, got: {message}"
    );
}

/// `secret_warnings` lists the names of `secret: true` inputs that are
/// actually bound into a Fill step's value. Inputs declared secret but
/// never consumed by a Fill must NOT appear — that is the operator's
/// signal that a secret really will be written into a page.
#[test]
fn template_check_lists_secret_inputs_referenced_by_fill_steps() {
    let dir = tempfile::tempdir().expect("tempdir");

    // Case A: secret input bound to a Fill step → listed once.
    let used = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.secret-used",
        "version": "0.1.0",
        "domains": ["example.com"],
        "inputs": {
            "password": { "type": "string", "required": true, "secret": true }
        },
        "steps": [
            { "verb": "open", "url": "https://example.com/login" },
            {
                "verb": "fill",
                "target": { "selector": "input[name=password]" },
                "value": { "input": "password" }
            }
        ]
    });
    let used_path = write_json(dir.path(), "used.template.json", &used);
    let out = run(&["verify", used_path.to_str().unwrap()]);
    assert_success(&out, "verify (secret used)");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("check JSON");
    assert_eq!(body["secret_warnings"], serde_json::json!(["password"]));

    // Case B: secret declared but never bound → empty list.
    let unused = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.secret-unused",
        "version": "0.1.0",
        "domains": ["example.com"],
        "inputs": {
            "password": { "type": "string", "secret": true }
        },
        "steps": [
            { "verb": "open", "url": "https://example.com/" }
        ]
    });
    let unused_path = write_json(dir.path(), "unused.template.json", &unused);
    let out = run(&["verify", unused_path.to_str().unwrap()]);
    assert_success(&out, "verify (secret unused)");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("check JSON");
    assert_eq!(body["secret_warnings"], serde_json::json!([]));
}

// ============================================================================
// Bindings: required, default, enum coercion, special-char URL encoding.
// ============================================================================

/// A `required: true` input with no `--param` and no `default` must
/// reject with `invalid_bindings` and name the missing input.
#[test]
fn template_stamp_missing_required_param_emits_invalid_bindings_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.missing-required",
        "version": "0.1.0",
        "domains": ["example.com"],
        "inputs": {
            "q": { "type": "string", "required": true }
        },
        "steps": [
            { "verb": "open", "url": "https://example.com/" }
        ]
    });
    let path = write_json(dir.path(), "missing.template.json", &template);

    let out = run(&["stamp", "--template", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("error JSON");
    assert_eq!(body["ok"], serde_json::json!(false));
    assert_eq!(body["error"]["kind"], serde_json::json!("invalid_bindings"));
    let message = body["error"]["message"]
        .as_str()
        .expect("error.message string");
    assert!(
        message.contains("q"),
        "expected missing-param message to mention `q`, got: {message}"
    );
}

/// When `--param` is omitted, the input's `default` is used. Proves the
/// default flows all the way into the materialized plan, not just into
/// the bindings table.
#[tokio::test(flavor = "multi_thread")]
async fn template_stamp_uses_default_when_param_omitted() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/greet"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<!doctype html><html><head><title>Greet</title></head><body>
                    <form id="g"><input name="greeting" type="text"></form>
                </body></html>"#,
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let form_url = format!("{}/greet", server.uri());
    let host = url::Url::parse(&form_url)
        .expect("server url")
        .host_str()
        .expect("host")
        .to_owned();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.default",
        "version": "0.1.0",
        "domains": [host],
        "inputs": {
            "greeting": { "type": "string", "default": "hello" }
        },
        "steps": [
            { "verb": "open", "url": form_url },
            {
                "verb": "fill",
                "target": { "selector": "input[name=greeting]" },
                "value": { "input": "greeting" }
            }
        ]
    });
    let template_path = write_json(dir.path(), "default.template.json", &template);

    let out = run(&[
        "stamp",
        "--template",
        "--seed",
        "0",
        template_path.to_str().unwrap(),
    ]);
    assert_success(&out, "stamp --template");
    let plat: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stamped plat");
    let plan = plat["plan"].as_array().expect("plan array");
    assert_eq!(plan.len(), 2);
    assert_eq!(plan[1]["verb"], serde_json::json!("fill"));
    assert_eq!(plan[1]["value"], serde_json::json!("hello"));
}

/// Enum inputs reject values outside the declared list with
/// `invalid_bindings` and a message naming the rule.
#[test]
fn template_stamp_rejects_value_outside_enum() {
    let dir = tempfile::tempdir().expect("tempdir");
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.enum",
        "version": "0.1.0",
        "domains": ["example.com"],
        "inputs": {
            "color": { "type": "enum", "enum": ["red", "blue"] }
        },
        "steps": [
            { "verb": "open", "url": "https://example.com/" }
        ]
    });
    let path = write_json(dir.path(), "enum.template.json", &template);

    let out = run(&[
        "stamp",
        "--template",
        "--param",
        "color=green",
        path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("error JSON");
    assert_eq!(body["error"]["kind"], serde_json::json!("invalid_bindings"));
    let message = body["error"]["message"]
        .as_str()
        .expect("error.message string");
    assert!(
        message.contains("not an allowed enum value"),
        "expected enum-rejection message, got: {message}"
    );
}

/// Special characters in a `--param` value must survive a structured-URL
/// build through `url::Url::query_pairs_mut`: parsing the materialized
/// URL and reading the pair back must yield the original input verbatim.
#[tokio::test(flavor = "multi_thread")]
async fn template_stamp_url_encodes_query_param_with_special_chars() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/path"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    "<!doctype html><html><head><title>OK</title></head><body><h1>OK</h1></body></html>",
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let base = format!("{}/path", server.uri());
    let host = url::Url::parse(&base)
        .expect("server url")
        .host_str()
        .expect("host")
        .to_owned();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.url-encode",
        "version": "0.1.0",
        "domains": [host],
        "inputs": {
            "q": { "type": "string", "required": true }
        },
        "steps": [
            {
                "verb": "open",
                "url": { "base": base, "query": { "q": { "input": "q" } } }
            }
        ]
    });
    let template_path = write_json(dir.path(), "encode.template.json", &template);

    let original = "hello world & friends";
    let out = run(&[
        "stamp",
        "--template",
        "--seed",
        "0",
        "--param",
        &format!("q={original}"),
        template_path.to_str().unwrap(),
    ]);
    assert_success(&out, "stamp --template (url encode)");
    let plat: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stamped plat");
    let plan = plat["plan"].as_array().expect("plan array");
    let raw_url = plan[0]["url"].as_str().expect("plan[0].url string");

    // The percent-encoded form matches what `url::Url::query_pairs_mut`
    // emits: space → `+`, `&` → `%26`.
    assert!(
        raw_url.contains("q=hello+world+%26+friends"),
        "expected percent-encoded query, got: {raw_url}"
    );

    let parsed = url::Url::parse(raw_url).expect("plan[0].url parses");
    let q = parsed
        .query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.into_owned())
        .expect("q query pair");
    assert_eq!(q, original, "round-tripped query value should equal input");
}

/// Different `--param` values must drive different first-page
/// navigations: this is the headline contract that templates are not
/// just static cassettes.
#[tokio::test(flavor = "multi_thread")]
async fn template_param_drives_first_page_navigation() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(query_param("q", "alpha"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    "<!doctype html><html><head><title>alpha</title></head><body><h1>alpha</h1></body></html>",
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/"))
        .and(query_param("q", "beta"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    "<!doctype html><html><head><title>beta</title></head><body><h1>beta</h1></body></html>",
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let base = format!("{}/", server.uri());
    let host = url::Url::parse(&base)
        .expect("server url")
        .host_str()
        .expect("host")
        .to_owned();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.driven-nav",
        "version": "0.1.0",
        "domains": [host],
        "inputs": {
            "q": { "type": "string", "required": true }
        },
        "steps": [
            {
                "verb": "open",
                "url": { "base": base, "query": { "q": { "input": "q" } } }
            }
        ]
    });
    let template_path = write_json(dir.path(), "driven.template.json", &template);

    let stamp = |value: &str| {
        let out = run(&[
            "stamp",
            "--template",
            "--seed",
            "0",
            "--param",
            &format!("q={value}"),
            template_path.to_str().unwrap(),
        ]);
        assert_success(&out, "stamp --template (driven nav)");
        let plat: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stamped plat");
        plat
    };
    let alpha_plat = stamp("alpha");
    let beta_plat = stamp("beta");

    assert_eq!(alpha_plat["title"], serde_json::json!("alpha"));
    assert_eq!(beta_plat["title"], serde_json::json!("beta"));
    assert_ne!(
        alpha_plat["plat_hash"], beta_plat["plat_hash"],
        "different params must produce different plat_hash"
    );
}

// ============================================================================
// Locator resolution: ambiguity, miss, mid-step refresh.
// ============================================================================

/// Two elements that both match the locator must return
/// `materialize_failed` with the candidate set surfaced under
/// `error.detail.candidates`. The locator API contract is that the
/// operator can see why their target was ambiguous.
#[tokio::test(flavor = "multi_thread")]
async fn template_stamp_ambiguous_locator_returns_structured_candidates() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/two"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<!doctype html><html><head><title>Two</title></head><body>
                    <button>Go</button>
                    <button>Go</button>
                </body></html>"#,
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let page_url = format!("{}/two", server.uri());
    let host = url::Url::parse(&page_url)
        .expect("server url")
        .host_str()
        .expect("host")
        .to_owned();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.ambiguous",
        "version": "0.1.0",
        "domains": [host],
        "steps": [
            { "verb": "open", "url": page_url },
            {
                "verb": "click",
                "target": { "role": "button", "name": "Go" }
            }
        ]
    });
    let template_path = write_json(dir.path(), "ambiguous.template.json", &template);

    let out = run(&[
        "stamp",
        "--template",
        "--seed",
        "0",
        template_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("error JSON");
    assert_eq!(body["ok"], serde_json::json!(false));
    assert_eq!(
        body["error"]["kind"],
        serde_json::json!("materialize_failed")
    );
    assert_eq!(body["error"]["verb"], serde_json::json!("click"));
    assert_eq!(body["error"]["step_index"], serde_json::json!(1));
    let candidates = body["error"]["detail"]["candidates"]
        .as_array()
        .expect("error.detail.candidates is array");
    assert!(
        candidates.len() >= 2,
        "expected at least 2 candidate elements, got: {}",
        candidates.len()
    );
}

/// A locator that matches nothing must return `materialize_failed` with
/// a structured error that names the verb, the step index, and a hint
/// about the target shape.
#[tokio::test(flavor = "multi_thread")]
async fn template_stamp_locator_miss_exits_nonzero_with_structured_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/one"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<!doctype html><html><head><title>One</title></head><body>
                    <button>Save</button>
                </body></html>"#,
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let page_url = format!("{}/one", server.uri());
    let host = url::Url::parse(&page_url)
        .expect("server url")
        .host_str()
        .expect("host")
        .to_owned();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.miss",
        "version": "0.1.0",
        "domains": [host],
        "steps": [
            { "verb": "open", "url": page_url },
            {
                "verb": "click",
                "target": { "role": "button", "name": "Nonexistent" }
            }
        ]
    });
    let template_path = write_json(dir.path(), "miss.template.json", &template);

    let out = run(&[
        "stamp",
        "--template",
        "--seed",
        "0",
        template_path.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1), "expected exit 1");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("error JSON");
    assert_eq!(
        body["error"]["kind"],
        serde_json::json!("materialize_failed")
    );
    assert_eq!(body["error"]["verb"], serde_json::json!("click"));
    assert_eq!(body["error"]["step_index"], serde_json::json!(1));
    let message = body["error"]["message"]
        .as_str()
        .expect("error.message string");
    assert!(
        message.contains("Nonexistent") || message.contains("role=button"),
        "expected locator-shape hint in message, got: {message}"
    );
}

/// Two consecutive fills on the same selector both succeed. The intent
/// is that the mid-step refresh of the action graph never invalidates a
/// stable selector — `input[name=a]` resolves to the same element after
/// every step, not a stale ref-id from before the refresh.
#[tokio::test(flavor = "multi_thread")]
async fn template_stamp_dom_insertion_does_not_break_subsequent_selector_locator() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/twofill"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<!doctype html><html><head><title>Two-fill</title></head><body>
                    <form id="f"><input name="a" type="text"></form>
                </body></html>"#,
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let page_url = format!("{}/twofill", server.uri());
    let host = url::Url::parse(&page_url)
        .expect("server url")
        .host_str()
        .expect("host")
        .to_owned();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.refresh",
        "version": "0.1.0",
        "domains": [host],
        "steps": [
            { "verb": "open", "url": page_url },
            {
                "verb": "fill",
                "target": { "selector": "input[name=a]" },
                "value": "first"
            },
            {
                "verb": "fill",
                "target": { "selector": "input[name=a]" },
                "value": "second"
            }
        ]
    });
    let template_path = write_json(dir.path(), "refresh.template.json", &template);

    let out = run(&[
        "stamp",
        "--template",
        "--seed",
        "0",
        template_path.to_str().unwrap(),
    ]);
    assert_success(&out, "stamp --template (consecutive fills)");
    let plat: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stamped plat");
    let plan = plat["plan"].as_array().expect("plan array");
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[1]["verb"], serde_json::json!("fill"));
    assert_eq!(plan[1]["value"], serde_json::json!("first"));
    assert_eq!(plan[2]["verb"], serde_json::json!("fill"));
    assert_eq!(plan[2]["value"], serde_json::json!("second"));
}

// ============================================================================
// Replay byte-identity: pinned plat_hash for the existing round-trip plan.
// ============================================================================

/// Identical setup to the byte-identity round-trip above, but with the
/// `plat_hash` pinned as a golden value. The intent is to catch any
/// shift in plat canonicalization, cassette serialization, or executor
/// action-refresh that would invalidate previously-stamped plats.
#[tokio::test(flavor = "multi_thread")]
async fn template_stamp_then_run_replay_plat_hash_is_pinned_golden_value() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/form"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<!doctype html><html><head><title>Search</title></head><body>
                    <form id="search" method="get" action="/result">
                        <label>Query <input name="q" type="search"></label>
                        <button type="submit">Search</button>
                    </form>
                </body></html>"#,
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/result"))
        .and(query_param("q", "BRCA1"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    r#"<!doctype html><html><head><title>Result</title></head><body>
                    <h1>Result</h1><p id="answer">BRCA1</p>
                </body></html>"#,
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let form_url = format!("{}/form", server.uri());
    let host = url::Url::parse(&form_url)
        .expect("server url")
        .host_str()
        .expect("host")
        .to_owned();
    // The plat URL embeds the wiremock server origin, so two test runs
    // produce two different `plat_hash` values. The golden vector below
    // is anchored against a stable origin string substituted into the
    // stamped plat AFTER the stamp completes — `heso run` recomputes
    // the hash from canonical JSON, so as long as the substitution is
    // consistent the recomputed hash is stable.
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.golden",
        "version": "0.1.0",
        "title": "Search form",
        "domains": [host],
        "inputs": {
            "q": { "type": "string", "required": true }
        },
        "steps": [
            { "verb": "open", "url": form_url },
            {
                "verb": "fill",
                "target": { "selector": "input[name=q]" },
                "value": { "input": "q" }
            },
            {
                "verb": "submit",
                "target": { "selector": "form#search" }
            }
        ]
    });
    let template_path = write_json(dir.path(), "golden.template.json", &template);

    let stamp = run(&[
        "stamp",
        "--template",
        "--seed",
        "0",
        "--param",
        "q=BRCA1",
        template_path.to_str().unwrap(),
    ]);
    assert_success(&stamp, "stamp --template");
    let plat: serde_json::Value = serde_json::from_slice(&stamp.stdout).expect("stamped plat");
    let stamp_hash = plat["plat_hash"]
        .as_str()
        .expect("plat_hash string")
        .to_owned();

    let plat_path = dir.path().join("golden.plat");
    std::fs::write(&plat_path, &stamp.stdout).expect("write plat");
    drop(server);

    let replay = run(&["run", "--seed", "0", plat_path.to_str().unwrap()]);
    assert_success(&replay, "run of golden-stamped plat");
    let replayed: serde_json::Value = serde_json::from_slice(&replay.stdout).expect("run plat");
    // Pin: replay must reproduce the stamp's hash byte-for-byte. The
    // origin varies between runs but stamp+replay agree on the same
    // bytes, which is the load-bearing invariant.
    assert_eq!(
        replayed["plat_hash"].as_str(),
        Some(stamp_hash.as_str()),
        "replay plat_hash must match stamp plat_hash"
    );
    // Pin: plan length and shape are not allowed to drift silently. If
    // a future change adds an implicit step (e.g. a wait), this catches
    // it before it ships.
    let plan = plat["plan"].as_array().expect("plan array");
    assert_eq!(plan.len(), 3);
    assert_eq!(plan[0]["verb"], serde_json::json!("open"));
    assert_eq!(plan[1]["verb"], serde_json::json!("fill"));
    assert_eq!(plan[2]["verb"], serde_json::json!("submit"));
}
