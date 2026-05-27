//! Integration coverage for `heso refresh <plat>` — drift detection.
//!
//! Pins four claims:
//!
//! 1. **Unchanged site → no drift.** Stamping a plan against a wiremock
//!    server twice (once to mint the plat, once via `refresh`) yields
//!    matching `plat_hash` values. Exit 0.
//! 2. **Changed site → drift.** Swapping the wiremock response between
//!    stamp and refresh flips `plat_hash`. Exit 1, stderr signals
//!    drift, the JSON `diff` block records that the plan stayed
//!    identical (only the cassette diverged).
//! 3. **Missing `plan` field → structured input error.** A plat that
//!    was minted by single-URL `heso open` instead of `heso stamp`
//!    can't be refreshed. Exit 2, stdout has `{ok: false, error.kind:
//!    "no_plan"}`.
//! 4. **Unreachable site → structured failure, no panic.** A plat whose
//!    target server is no longer up exits 2 with a structured error
//!    body and a clean stderr (no `panicked` / backtrace markers).

use std::path::{Path, PathBuf};
use std::process::Command;

use wiremock::matchers::{method, path as wm_path};
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

fn write_bytes(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write bytes");
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

/// Stamp a single-page `open` plan against the server's `/page` route
/// and return the parsed plat plus the path it was written to.
async fn stamp_open_plan(server: &MockServer, dir: &Path) -> (serde_json::Value, PathBuf) {
    let url = format!("{}/page", server.uri());
    let plan = serde_json::json!([{ "verb": "open", "url": url }]);
    let plan_path = write_json(dir, "plan.json", &plan);
    let out = run(&["stamp", "--seed", "0", plan_path.to_str().unwrap()]);
    assert_success(&out, "stamp");
    let plat: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stamp emits JSON");
    let plat_path = write_bytes(dir, "stamped.plat", &out.stdout);
    (plat, plat_path)
}

#[tokio::test]
async fn refresh_unchanged_site_exits_0_drifted_false() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    "<html><head><title>Hello</title></head><body>Content</body></html>",
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let (plat, plat_path) = stamp_open_plan(&server, dir.path()).await;
    let input_hash = plat["plat_hash"].as_str().expect("plat_hash").to_owned();

    let out = run(&["refresh", "--seed", "0", plat_path.to_str().unwrap()]);
    assert_success(&out, "refresh");
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("refresh emits JSON");
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["drifted"], serde_json::json!(false));
    assert_eq!(
        body["input_plat_hash"].as_str(),
        Some(input_hash.as_str()),
        "input_plat_hash must echo the source plat's hash"
    );
    assert_eq!(
        body["live_plat_hash"].as_str(),
        Some(input_hash.as_str()),
        "live_plat_hash must equal the stamped hash when the site is unchanged"
    );
    assert!(
        body.get("diff").is_none(),
        "no-drift response must omit the `diff` block"
    );
}

#[tokio::test]
async fn refresh_changed_site_exits_1_drifted_true() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    "<html><head><title>Hello</title></head><body>Content</body></html>",
                ),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let (plat, plat_path) = stamp_open_plan(&server, dir.path()).await;
    let input_hash = plat["plat_hash"].as_str().expect("plat_hash").to_owned();

    // Override the existing mock with a higher-priority response that
    // returns different HTML — refresh re-fetches and should see drift.
    Mock::given(method("GET"))
        .and(wm_path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string(
                    "<html><head><title>CHANGED</title></head><body>Different</body></html>",
                ),
        )
        .with_priority(1)
        .mount(&server)
        .await;

    let out = run(&["refresh", "--seed", "0", plat_path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "drift must exit 1\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("drift response is JSON");
    assert_eq!(body["ok"], serde_json::json!(true));
    assert_eq!(body["drifted"], serde_json::json!(true));
    assert_eq!(body["input_plat_hash"].as_str(), Some(input_hash.as_str()));
    let live_hash = body["live_plat_hash"]
        .as_str()
        .expect("live_plat_hash string");
    assert_ne!(
        live_hash, input_hash,
        "live_plat_hash must change when the cassette diverges"
    );
    assert_eq!(
        body["diff"]["plan_identical"],
        serde_json::json!(true),
        "the plan didn't change — only the cassette did"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("drift detected"),
        "stderr should announce drift, got: {stderr}"
    );
}

#[tokio::test]
async fn refresh_plat_without_plan_exits_2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let plat = serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "actions": [],
        "tree": [],
        "title": "",
        "description": "",
        "plat_hash": "0".repeat(64),
    });
    let plat_path = write_json(dir.path(), "no-plan.plat", &plat);

    let out = run(&["refresh", plat_path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "a plat with no `plan` field must exit 2\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("error response is JSON");
    assert_eq!(body["ok"], serde_json::json!(false));
    assert_eq!(body["error"]["kind"], serde_json::json!("no_plan"));
}

#[test]
fn refresh_unreachable_site_exits_2_no_panic() {
    // Hand-build a plat whose plan points at a port nothing listens on.
    // Port 1 is privileged and unbound on every platform — `connect`
    // rejects with ECONNREFUSED, so the re-stamp's `Open` must surface
    // a structured error rather than producing a fresh plat.
    let dir = tempfile::tempdir().expect("tempdir");
    let dead_url = "http://127.0.0.1:1/";
    let plat = serde_json::json!({
        "input_url": dead_url,
        "url": dead_url,
        "title": "",
        "description": "",
        "tree": [],
        "actions": [],
        "plan": [{ "verb": "open", "url": dead_url }],
        "plat_hash": "0".repeat(64),
    });
    let plat_path = write_json(dir.path(), "unreachable.plat", &plat);

    let out = run(&["refresh", "--seed", "0", plat_path.to_str().unwrap()]);
    let code = out.status.code();
    assert_eq!(
        code,
        Some(2),
        "an unreachable site should yield exit 2 (stamp failure surfaces as input error)\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("error response is JSON");
    assert_eq!(body["ok"], serde_json::json!(false));
    let kind = body["error"]["kind"]
        .as_str()
        .expect("error.kind string");
    assert!(
        kind == "stamp_failed" || kind == "stamp_partial",
        "error.kind must be a stamp_* variant, got `{kind}`"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("panicked") && !stderr.contains("RUST_BACKTRACE"),
        "stderr must not contain a panic trace, got: {stderr}"
    );
}
