//! Integration coverage for the polymorphic `heso seal` verb.
//!
//! `seal <plat>` produces a sealed envelope. Receipts, sealed
//! envelopes, action-hash fingerprints, and templates each return a
//! dedicated refusal exit.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn run_in(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(heso_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn heso")
}

fn write_json(path: &Path, value: &serde_json::Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write");
}

fn minimal_plat(dir: &Path) -> PathBuf {
    let mut body = serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": "Seal",
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
fn seal_plat_produces_a_sealed_envelope() {
    let dir = TempDir::new().unwrap();
    // Initialize an identity in the temp dir so the default key path
    // resolves cleanly under `current_dir`.
    let init = run_in(dir.path(), &["identity", "init"]);
    assert!(init.status.success());

    let plat = minimal_plat(dir.path());
    let out = run_in(dir.path(), &["seal", plat.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "seal failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let envelope: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("seal stdout is JSON");
    assert_eq!(
        envelope["alg"],
        serde_json::json!("heso-plat/v1+ed25519")
    );
    assert!(envelope.get("content").is_some(), "envelope has content");
    assert!(envelope.get("signature").is_some(), "envelope has signature");
}

#[test]
fn seal_refuses_action_hash_fingerprint() {
    let dir = TempDir::new().unwrap();
    let url = heso_core::Url::parse("https://example.com/").unwrap();
    let fp = heso_trace::trace_fingerprint(&url, &serde_json::json!([]));
    let path = dir.path().join("fp.json");
    write_json(&path, &serde_json::to_value(&fp).unwrap());

    let out = run_in(dir.path(), &["seal", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot seal an action-hash"));
}

#[test]
fn seal_refuses_template() {
    let dir = TempDir::new().unwrap();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.seal-refuse",
        "version": "0.1.0",
        "domains": ["example.com"],
        "steps": [
            { "verb": "open", "url": "https://example.com/" }
        ]
    });
    let path = dir.path().join("tpl.json");
    write_json(&path, &template);

    let out = run_in(dir.path(), &["seal", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot seal a template"));
}

#[test]
fn unseal_extract_returns_inner_plat() {
    let dir = TempDir::new().unwrap();
    let init = run_in(dir.path(), &["identity", "init"]);
    assert!(init.status.success());

    let plat = minimal_plat(dir.path());
    let seal = run_in(dir.path(), &["seal", plat.to_str().unwrap()]);
    assert!(seal.status.success());
    let sealed_path = dir.path().join("sealed.json");
    std::fs::write(&sealed_path, &seal.stdout).unwrap();

    let out = run_in(
        dir.path(),
        &["unseal", "--extract", sealed_path.to_str().unwrap()],
    );
    assert!(
        out.status.success(),
        "unseal --extract failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("unseal --extract stdout is JSON");
    assert_eq!(body["url"], serde_json::json!("https://example.com/"));
    assert_eq!(body["title"], serde_json::json!("Seal"));
}
