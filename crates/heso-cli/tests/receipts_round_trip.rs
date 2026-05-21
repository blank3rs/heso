//! End-to-end round-trip tests for the signed-receipts pipeline.
//!
//! These exercise the headline pitch ("`heso open --receipt PATH`
//! signs a Receipt; `heso receipt-verify` checks it") through the
//! actual CLI binary, against a hermetic [`wiremock`] HTTP server so
//! the tests don't depend on the public internet.
//!
//! The matrix covered:
//!
//! 1. Sign a receipt against the wiremock fixture, verify with the
//!    correct pubkey in the allowlist → PASSES (exit 0).
//! 2. Verify the same receipt with a different pubkey in the
//!    allowlist → REJECTED (exit 1, INVALID).
//! 3. Tamper one byte (`seed`) after signing → REJECTED (exit 1).
//! 4. Sign with `--mode live`, verify → REJECTED (exit 1, live-mode
//!    rejection from ADR 0008).
//! 5. Verify without an allowlist → emits a stderr warning but still
//!    exits 0 (legacy behavior preserved with a loud trust-anchor
//!    warning).

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Absolute path to the `heso` binary built by Cargo for this test
/// crate. Provided automatically as `CARGO_BIN_EXE_heso`.
fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

/// Run `heso <args>` from `cwd`. Returns the raw `Output` for
/// per-test inspection of status + stdout + stderr.
fn run_in(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(heso_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn heso")
}

/// Spin up a wiremock server that returns a fixed HTML body for `/`.
/// Used as the target URL for the signing pass — same shape every
/// `heso open` test in this crate uses.
async fn start_mock_server() -> (MockServer, String) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                "<!doctype html><html><head><title>Receipt Test</title></head>\
                 <body><h1>Receipt round-trip fixture</h1></body></html>",
            ),
        )
        .mount(&server)
        .await;
    let url = format!("{}/", server.uri());
    (server, url)
}

/// Initialize an identity in `cwd` and return its base64 public key.
/// Mirrors what an external user would do as their first step.
fn init_identity(cwd: &std::path::Path) -> String {
    let out = run_in(cwd, &["identity", "init"]);
    assert!(
        out.status.success(),
        "identity init failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("identity init stdout is JSON");
    body["public_key"]
        .as_str()
        .expect("public_key is a string")
        .to_owned()
}

/// `heso open <url> --receipt receipt.json` against `url` in `cwd`.
/// Returns the path to the written receipt file.
fn sign_open(cwd: &std::path::Path, url: &str, receipt_filename: &str) -> PathBuf {
    let out = run_in(cwd, &["open", url, "--receipt", receipt_filename]);
    assert!(
        out.status.success(),
        "heso open --receipt failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let receipt_path = cwd.join(receipt_filename);
    assert!(
        receipt_path.exists(),
        "expected receipt file at {} after --receipt; cwd contents={:?}",
        receipt_path.display(),
        std::fs::read_dir(cwd)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect::<Vec<_>>(),
    );
    receipt_path
}

/// Variant: emit a `mode: live` receipt. Used to exercise the
/// live-mode rejection on the verify side (ADR 0008 / P1 fix).
fn sign_open_live(cwd: &std::path::Path, url: &str, receipt_filename: &str) -> PathBuf {
    let out = run_in(
        cwd,
        &["open", url, "--receipt", receipt_filename, "--mode", "live"],
    );
    assert!(
        out.status.success(),
        "heso open --receipt --mode live failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    cwd.join(receipt_filename)
}

/// Write a JSON pubkey allowlist to `cwd/filename` containing exactly
/// the supplied base64 keys. Mirrors the `--trusted-keys` file shape.
fn write_allowlist(cwd: &std::path::Path, filename: &str, keys: &[&str]) -> PathBuf {
    let path = cwd.join(filename);
    let body = serde_json::to_string_pretty(&keys).expect("serialize allowlist");
    std::fs::write(&path, body).expect("write allowlist");
    path
}

// ============================================================================
// 1. Sign → verify with correct pubkey → PASSES
// ============================================================================

#[tokio::test]
async fn round_trip_sign_then_verify_with_correct_allowlist_passes() {
    let (_server, url) = start_mock_server().await;
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    let pubkey = init_identity(cwd);
    let receipt = sign_open(cwd, &url, "receipt.json");
    let allowlist = write_allowlist(cwd, "trusted.json", &[pubkey.as_str()]);

    let out = run_in(
        cwd,
        &[
            "receipt-verify",
            "--trusted-keys",
            allowlist.to_str().unwrap(),
            receipt.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "expected exit 0 for valid + allowlisted signer; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("OK "),
        "expected 'OK ...' on stdout, got: {stdout}"
    );
    assert!(
        stdout.contains(&pubkey),
        "expected verifier to echo the signing pubkey {pubkey}, got: {stdout}"
    );
}

// ============================================================================
// 2. Sign → verify with WRONG pubkey in allowlist → REJECTED
// ============================================================================

#[tokio::test]
async fn round_trip_verify_with_wrong_allowlist_is_rejected() {
    let (_server, url) = start_mock_server().await;
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    let _pubkey = init_identity(cwd);
    let receipt = sign_open(cwd, &url, "receipt.json");
    // A different (random) base64 pubkey — never used to sign this
    // receipt. The crypto signature itself is still valid for the
    // ORIGINAL signing pubkey; the rejection comes from the trust
    // anchor check, not from `verify_strict`.
    let bogus = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    let allowlist = write_allowlist(cwd, "trusted.json", &[bogus]);

    let out = run_in(
        cwd,
        &[
            "receipt-verify",
            "--trusted-keys",
            allowlist.to_str().unwrap(),
            receipt.to_str().unwrap(),
        ],
    );
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code, 1,
        "wrong-pubkey allowlist must exit 1; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("INVALID"),
        "expected INVALID line, got stderr: {stderr}"
    );
    assert!(
        stderr.contains("allowlist") || stderr.contains("trusted"),
        "expected allowlist-rejection message, got stderr: {stderr}"
    );
}

// ============================================================================
// 3. Tamper a byte (seed field) → REJECTED
// ============================================================================

#[tokio::test]
async fn round_trip_tampered_receipt_is_rejected() {
    let (_server, url) = start_mock_server().await;
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    let pubkey = init_identity(cwd);
    let receipt = sign_open(cwd, &url, "receipt.json");

    // Read, mutate one field, write back. Switching seed from 0 to
    // 999 leaves the structure valid JSON but invalidates the
    // signature — canonical-JSON includes the seed field.
    let original = std::fs::read_to_string(&receipt).expect("read receipt");
    let mut as_json: serde_json::Value =
        serde_json::from_str(&original).expect("parse receipt JSON");
    as_json["seed"] = serde_json::json!(999);
    let tampered = serde_json::to_string_pretty(&as_json).expect("re-serialize");
    std::fs::write(&receipt, &tampered).expect("write tampered receipt");

    let allowlist = write_allowlist(cwd, "trusted.json", &[pubkey.as_str()]);
    let out = run_in(
        cwd,
        &[
            "receipt-verify",
            "--trusted-keys",
            allowlist.to_str().unwrap(),
            receipt.to_str().unwrap(),
        ],
    );
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code, 1,
        "tampered receipt must exit 1; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("INVALID"),
        "expected INVALID prefix, got stderr: {stderr}"
    );
}

// ============================================================================
// 4. mode: live receipt → REJECTED (replay-safety guard, ADR 0008)
// ============================================================================

#[tokio::test]
async fn round_trip_mode_live_receipt_is_rejected() {
    let (_server, url) = start_mock_server().await;
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    let pubkey = init_identity(cwd);
    let receipt = sign_open_live(cwd, &url, "live.json");
    // Confirm the receipt really is mode:live (the sign-side flag
    // plumbing test); if this changes, the rejection test below is
    // testing the wrong thing.
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt).unwrap())
            .expect("receipt is JSON");
    assert_eq!(body["mode"], serde_json::json!("live"));

    let allowlist = write_allowlist(cwd, "trusted.json", &[pubkey.as_str()]);
    let out = run_in(
        cwd,
        &[
            "receipt-verify",
            "--trusted-keys",
            allowlist.to_str().unwrap(),
            receipt.to_str().unwrap(),
        ],
    );
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(
        code, 1,
        "mode:live receipt must exit 1; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("live") || stderr.contains("ADR 0008"),
        "expected live-mode rejection message, got stderr: {stderr}"
    );
}

// ============================================================================
// 5. No allowlist → still passes, but warns to stderr
// ============================================================================

#[tokio::test]
async fn round_trip_no_allowlist_warns_and_still_passes() {
    let (_server, url) = start_mock_server().await;
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    let _pubkey = init_identity(cwd);
    let receipt = sign_open(cwd, &url, "receipt.json");

    // No --trusted-keys flag, no HESO_TRUSTED_KEYS env var set in
    // this process. The verifier should still succeed (legacy
    // behavior) but print a warning.
    //
    // We explicitly REMOVE the env var to defend against a parallel
    // test or developer shell that set it — the worktree-test
    // invariant is "if you don't tell heso a trust anchor, you get
    // an exit-0 verify + a warning."
    let mut cmd = Command::new(heso_bin());
    cmd.args(["receipt-verify", receipt.to_str().unwrap()])
        .current_dir(cwd)
        .env_remove("HESO_TRUSTED_KEYS");
    let out = cmd.output().expect("spawn heso");
    assert!(
        out.status.success(),
        "no-allowlist verify must still exit 0; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("warning") && stderr.contains("allowlist"),
        "expected stderr warning about missing allowlist, got: {stderr}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("OK "),
        "expected OK stdout, got: {stdout}"
    );
}

// ============================================================================
// 6. Env-var allowlist works the same as the flag
// ============================================================================

#[tokio::test]
async fn round_trip_env_allowlist_passes_with_correct_pubkey() {
    let (_server, url) = start_mock_server().await;
    let dir = TempDir::new().unwrap();
    let cwd = dir.path();

    let pubkey = init_identity(cwd);
    let receipt = sign_open(cwd, &url, "receipt.json");
    let allowlist = write_allowlist(cwd, "trusted.json", &[pubkey.as_str()]);

    // Set HESO_TRUSTED_KEYS=<path> on the child process — exact
    // same shape as `--trusted-keys`, just an alternate source.
    let mut cmd = Command::new(heso_bin());
    cmd.args(["receipt-verify", receipt.to_str().unwrap()])
        .current_dir(cwd)
        .env("HESO_TRUSTED_KEYS", &allowlist);
    let out = cmd.output().expect("spawn heso");
    assert!(
        out.status.success(),
        "env-allowlist verify must exit 0; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("OK "), "expected OK, got: {stdout}");
}
