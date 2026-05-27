//! Integration coverage for the polymorphic `heso verify` verb.
//!
//! Each test builds the artifact off disk (plat / receipt / action-hash /
//! template), runs `heso verify <file>`, and asserts the exit code +
//! stdout prefix matches the per-type contract documented on
//! `cmd_verify`.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(heso_bin())
        .args(args)
        .output()
        .expect("spawn heso")
}

fn run_in(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(heso_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn heso")
}

fn write_json(path: &Path, value: &serde_json::Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write json");
}

/// Build a minimal plat with a recomputed `plat_hash`.
fn minimal_plat(dir: &Path) -> PathBuf {
    let mut body = serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": "Example",
        "description": "",
        "tree": [],
        "actions": [],
    });
    let hash = heso_engine_fetch::plat_hash(&body);
    body.as_object_mut()
        .unwrap()
        .insert("plat_hash".into(), serde_json::Value::String(hash));
    let path = dir.join("plat.plat");
    write_json(&path, &body);
    path
}

#[test]
fn verify_recognizes_plat_and_exits_zero_on_valid_hash() {
    let dir = TempDir::new().unwrap();
    let plat = minimal_plat(dir.path());

    let out = run(&["verify", plat.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "verify failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("OK plat"),
        "expected `OK plat ...`, got: {stdout}"
    );
}

#[test]
fn verify_plat_exits_one_on_tampered_hash() {
    let dir = TempDir::new().unwrap();
    let mut body = serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": "Example",
        "description": "",
        "tree": [],
        "actions": [],
    });
    body.as_object_mut()
        .unwrap()
        .insert("plat_hash".into(), "0".repeat(64).into());
    let plat = dir.path().join("bad.plat");
    write_json(&plat, &body);

    let out = run(&["verify", plat.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("FAIL plat"), "expected FAIL plat, got: {stdout}");
}

#[test]
fn verify_recognizes_action_hash_fingerprint() {
    let dir = TempDir::new().unwrap();
    let url = heso_core::Url::parse("https://example.com/").unwrap();
    let actions = serde_json::json!([]);
    let fp = heso_trace::trace_fingerprint(&url, &actions);
    let path = dir.path().join("fp.json");
    write_json(&path, &serde_json::to_value(&fp).unwrap());

    let out = run(&["verify", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "verify failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("OK action-hash"),
        "expected `OK action-hash ...`, got: {stdout}"
    );
}

#[test]
fn verify_recognizes_receipt_via_signed_path() {
    let dir = TempDir::new().unwrap();
    let key = heso_core::IdentityKey::generate();
    let trace = vec![heso_primitives::PrimitiveOp::Cd(heso_primitives::CdInput {
        target: heso_primitives::CdTarget::Url {
            url: heso_core::Url::parse("https://example.com/").unwrap(),
        },
    })];
    let mut receipt = heso_trace::Receipt {
        trace: trace.clone(),
        results: vec![],
        pages_seen: vec![],
        trace_hash: heso_trace::trace_hash(&trace),
        planner_id: "test".into(),
        seed: 0,
        mode: heso_trace::Mode::Deterministic,
        cost: heso_trace::Cost::default(),
        failed_at: None,
        error: None,
        signature: None,
        tsa_anchor: None,
        produced_plat_hash: None,
    };
    heso_trace::sign_receipt(&key, &mut receipt);
    let path = dir.path().join("receipt.json");
    std::fs::write(&path, serde_json::to_string_pretty(&receipt).unwrap()).unwrap();

    let out = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "verify failed; stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("OK receipt"),
        "expected `OK receipt ...`, got: {stdout}"
    );
}

#[test]
fn verify_recognizes_template() {
    let dir = TempDir::new().unwrap();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.verify",
        "version": "0.1.0",
        "domains": ["example.com"],
        "steps": [
            { "verb": "open", "url": "https://example.com/" }
        ]
    });
    let path = dir.path().join("t.json");
    write_json(&path, &template);

    let out = run(&["verify", path.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "verify failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("OK template"),
        "expected `OK template ...`, got: {stdout}"
    );
}

#[test]
fn verify_rejects_unrecognized_artifact() {
    let dir = TempDir::new().unwrap();
    let bogus = dir.path().join("bogus.json");
    write_json(&bogus, &serde_json::json!({ "hello": "world" }));
    let out = run(&["verify", bogus.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn verify_require_tsa_exits_two_with_not_implemented_message() {
    let dir = TempDir::new().unwrap();
    let key = heso_core::IdentityKey::generate();
    let trace = vec![heso_primitives::PrimitiveOp::Cd(heso_primitives::CdInput {
        target: heso_primitives::CdTarget::Url {
            url: heso_core::Url::parse("https://example.com/").unwrap(),
        },
    })];
    let mut receipt = heso_trace::Receipt {
        trace: trace.clone(),
        results: vec![],
        pages_seen: vec![],
        trace_hash: heso_trace::trace_hash(&trace),
        planner_id: "test".into(),
        seed: 0,
        mode: heso_trace::Mode::Deterministic,
        cost: heso_trace::Cost::default(),
        failed_at: None,
        error: None,
        signature: None,
        tsa_anchor: None,
        produced_plat_hash: None,
    };
    heso_trace::sign_receipt(&key, &mut receipt);
    let path = dir.path().join("receipt.json");
    std::fs::write(&path, serde_json::to_string_pretty(&receipt).unwrap()).unwrap();

    let out = run_in(dir.path(), &["verify", "--require-tsa", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("TSA verification not yet implemented"),
        "expected not-implemented message, got: {stderr}"
    );
}
