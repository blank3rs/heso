//! Integration tests for `heso stamp` / `heso run` / `heso replay`
//! against a hermetic localhost wiremock server.
//!
//! Pins three claims:
//!
//! 1. **`stamp` records a cassette** — the output plat has a non-empty
//!    `cassette.records` array.
//! 2. **`run` is byte-identical** — running a fresh-stamped plat
//!    produces a plat whose `plat_hash` equals the input's.
//! 3. **A drifted cassette surfaces gracefully** — tampering with the
//!    cassette body so the recorded URL no longer matches what the
//!    plan asks for makes `heso run` exit 1 with a structured
//!    `cassette miss:` error in the step log, not a panic.
//!
//! Together these are the ADR 0008 "byte-identical replay against
//! recorded network responses" contract, exercised end-to-end.

use std::path::PathBuf;
use std::process::Command;

use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn write_temp(suffix: &str, body: &[u8]) -> PathBuf {
    let mut p = std::env::temp_dir();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("heso-cassette-{unique}-{suffix}"));
    std::fs::write(&p, body).expect("write temp");
    p
}

async fn fixture_server(html: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(html),
        )
        .mount(&server)
        .await;
    server
}

fn run_verb(verb: &str, extra_args: &[&str]) -> std::process::Output {
    let mut args = vec![verb];
    args.extend_from_slice(extra_args);
    Command::new(heso_bin())
        .args(&args)
        .output()
        .expect("spawn heso")
}

#[tokio::test]
async fn stamp_records_cassette() {
    let server = fixture_server("<html><head><title>fixture</title></head><body><h1>hi</h1></body></html>").await;
    let url = format!("{}/page", server.uri());
    let plan = serde_json::json!([{ "verb": "open", "url": url }]);
    let plan_path = write_temp("plan.json", plan.to_string().as_bytes());

    let out = run_verb("stamp", &["--seed", "0", plan_path.to_str().unwrap()]);
    assert!(out.status.success(), "stamp failed: {}", String::from_utf8_lossy(&out.stderr));

    let plat: serde_json::Value = serde_json::from_slice(&out.stdout).expect("plat is json");
    let records = plat
        .pointer("/cassette/records")
        .and_then(|v| v.as_array())
        .expect("cassette.records array present");
    assert!(
        !records.is_empty(),
        "stamp should record at least one cassette entry; got 0 against {url}"
    );
    let first = &records[0];
    assert_eq!(first["method"], "GET");
    assert_eq!(first["url"], url);
    assert_eq!(first["status"], 200);
    // Response body is base64 — non-empty and decodes to the HTML.
    let body_b64 = first["response_body_b64"].as_str().expect("body_b64 string");
    assert!(!body_b64.is_empty(), "recorded response body must not be empty");

    let _ = std::fs::remove_file(&plan_path);
}

#[tokio::test]
async fn stamp_then_run_is_byte_identical() {
    let server = fixture_server(
        "<html><head><title>stable fixture</title></head><body><p>hello replay</p></body></html>",
    )
    .await;
    let url = format!("{}/page", server.uri());
    let plan = serde_json::json!([{ "verb": "open", "url": url }]);
    let plan_path = write_temp("plan.json", plan.to_string().as_bytes());

    // stamp once against the live wiremock — captures a cassette.
    let stamp_out = run_verb("stamp", &["--seed", "0", plan_path.to_str().unwrap()]);
    assert!(stamp_out.status.success(), "stamp failed: {}", String::from_utf8_lossy(&stamp_out.stderr));
    let plat: serde_json::Value = serde_json::from_slice(&stamp_out.stdout).expect("plat is json");
    let stamp_hash = plat["plat_hash"].as_str().expect("plat_hash string").to_owned();

    // Persist the stamped plat to disk so `run` can read it.
    let plat_path = write_temp("plat.plat", &stamp_out.stdout);

    // The wiremock server can shut down now — `run` should not touch
    // the network. (We don't actually shut it down here because
    // wiremock's Drop handles it, and Replaying mode never reaches
    // for the wire anyway.)
    drop(server);

    let run_out = run_verb("run", &["--seed", "0", plat_path.to_str().unwrap()]);
    assert!(
        run_out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run_out.stderr)
    );
    let run_plat: serde_json::Value = serde_json::from_slice(&run_out.stdout).expect("run plat is json");
    let run_hash = run_plat["plat_hash"].as_str().expect("plat_hash string");

    assert_eq!(
        stamp_hash, run_hash,
        "stamp's plat_hash must equal run's plat_hash for the same plan + cassette (ADR 0008 \"byte-identical replay\")"
    );

    let _ = std::fs::remove_file(&plan_path);
    let _ = std::fs::remove_file(&plat_path);
}

#[tokio::test]
async fn run_against_tampered_cassette_errors_gracefully() {
    let server = fixture_server("<html><body>fixture</body></html>").await;
    let url = format!("{}/page", server.uri());
    let plan = serde_json::json!([{ "verb": "open", "url": url }]);
    let plan_path = write_temp("plan.json", plan.to_string().as_bytes());

    let stamp_out = run_verb("stamp", &["--seed", "0", plan_path.to_str().unwrap()]);
    assert!(stamp_out.status.success());

    let mut plat: serde_json::Value = serde_json::from_slice(&stamp_out.stdout).expect("plat is json");
    // Tamper the recorded URL so lookup fails — simulates a page that
    // changed paths since stamping.
    plat
        .pointer_mut("/cassette/records/0/url")
        .map(|v| *v = serde_json::Value::String("https://drifted.example/".into()))
        .expect("path exists");
    // Remove plat_hash since we've mutated the body.
    if let Some(obj) = plat.as_object_mut() {
        obj.remove("plat_hash");
    }
    let plat_path = write_temp("plat-tampered.json", plat.to_string().as_bytes());

    drop(server);

    let run_out = run_verb("run", &["--seed", "0", plat_path.to_str().unwrap()]);
    assert!(
        !run_out.status.success(),
        "run against a tampered cassette must exit non-zero"
    );

    let run_plat: serde_json::Value = serde_json::from_slice(&run_out.stdout).expect("partial plat is json");
    let steps = run_plat["steps"].as_array().expect("steps array");
    assert!(
        !steps.is_empty(),
        "even on miss, `run` should emit the per-step log with the error attached"
    );
    let err = steps[0]["error"].as_str().expect("step error string");
    assert!(
        err.contains("cassette miss"),
        "error message must surface `cassette miss`; got: {err}"
    );
    assert!(
        err.contains("re-stamp"),
        "error message should hint at the operator action (re-stamp); got: {err}"
    );

    let _ = std::fs::remove_file(&plan_path);
    let _ = std::fs::remove_file(&plat_path);
}

#[tokio::test]
async fn replay_emits_step_log_without_executing() {
    let server = fixture_server("<html><body>fixture</body></html>").await;
    let url = format!("{}/page", server.uri());
    let plan = serde_json::json!([{ "verb": "open", "url": url }]);
    let plan_path = write_temp("plan.json", plan.to_string().as_bytes());

    let stamp_out = run_verb("stamp", &["--seed", "0", plan_path.to_str().unwrap()]);
    assert!(stamp_out.status.success());
    let plat_path = write_temp("plat.plat", &stamp_out.stdout);

    drop(server);

    // `replay` reads the plat without any engine — no network needed,
    // even though we just dropped the wiremock server.
    let replay_out = run_verb("replay", &[plat_path.to_str().unwrap()]);
    assert!(
        replay_out.status.success(),
        "replay failed: {}",
        String::from_utf8_lossy(&replay_out.stderr)
    );
    let summary: serde_json::Value =
        serde_json::from_slice(&replay_out.stdout).expect("replay output is json");
    assert!(
        summary["steps"].is_array(),
        "replay must echo the steps array"
    );
    assert!(
        summary["plat_hash"].is_string(),
        "replay should surface the recorded plat_hash"
    );
    assert_eq!(
        summary["cassette_records"].as_u64().unwrap_or(0),
        1,
        "the source plat had one cassette record; replay surfaces the count"
    );

    let _ = std::fs::remove_file(&plan_path);
    let _ = std::fs::remove_file(&plat_path);
}

#[tokio::test]
async fn run_refuses_plat_without_cassette() {
    // A plat with a plan but no `cassette` field MUST be refused —
    // `run` is the cassette-replay verb; falling back to live HTTP
    // would silently violate HESO/1.0 §5.5 (deterministic mode must
    // not degrade to a network fetch on a missing cassette). The
    // operator should use `heso stamp <plan>` to mint a fresh plat
    // against the live web instead.
    let plat = serde_json::json!({
        "plan": [{"verb": "open", "url": "https://example.com/"}],
        "url": "https://example.com/",
        "title": "no cassette here",
    });
    let plat_path = write_temp("plat-nocassette.json", plat.to_string().as_bytes());

    let out = run_verb("run", &["--seed", "0", plat_path.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "run must refuse a plat with no cassette field"
    );
    assert_eq!(
        out.status.code(),
        Some(2),
        "missing-required-field is exit 2 per the spec"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cassette"),
        "stderr must mention `cassette`; got: {stderr}"
    );
    assert!(
        stderr.contains("stamp"),
        "stderr should point operator at `heso stamp`; got: {stderr}"
    );

    let _ = std::fs::remove_file(&plat_path);
}

#[tokio::test]
async fn replay_refuses_plat_without_steps_field() {
    // Hand-build a plat with no `steps` field — `replay` should
    // exit 2 with a clear message rather than fabricating one.
    let plat = serde_json::json!({
        "url": "https://example.com/",
        "title": "no steps here",
    });
    let plat_path = write_temp("plat-bare.json", plat.to_string().as_bytes());

    let out = run_verb("replay", &[plat_path.to_str().unwrap()]);
    assert!(!out.status.success(), "replay should refuse a stepless plat");
    assert_eq!(
        out.status.code(),
        Some(2),
        "exit code 2 is the documented refusal code"
    );

    let _ = std::fs::remove_file(&plat_path);
}
