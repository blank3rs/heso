//! Integration coverage for the polymorphic `heso info` verb.
//!
//! Exercises the text + JSON output paths against each artifact kind,
//! and the two-arg diff mode. Mirrors `tests/plat_dev_tools.rs`'s
//! style — write fixtures off disk, drive the binary, assert on the
//! lines / fields the verb commits to.

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

fn write_json(path: &Path, value: &serde_json::Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write");
}

fn minimal_plat(dir: &Path, title: &str) -> PathBuf {
    let mut body = serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": title,
        "description": "",
        "tree": [],
        "actions": [],
    });
    let hash = heso_engine_fetch::plat_hash(&body);
    body.as_object_mut()
        .unwrap()
        .insert("plat_hash".into(), serde_json::Value::String(hash));
    let path = dir.join(format!("{title}.plat"));
    write_json(&path, &body);
    path
}

#[test]
fn info_plat_text_summary_lists_load_bearing_lines() {
    let dir = TempDir::new().unwrap();
    let plat = minimal_plat(dir.path(), "info-text");

    let out = run(&["info", plat.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "info failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "kind:           plat",
        "plat_hash:",
        "verified:       yes",
        "url:",
        "title:          info-text",
        "plan:           0 actions",
        "cassette:       0 records",
        "sealed:         no",
        "partial:        false",
    ] {
        assert!(
            stdout.contains(expected),
            "info text output missing `{expected}`. Full output:\n{stdout}"
        );
    }
}

#[test]
fn info_plat_json_format_returns_structured_object() {
    let dir = TempDir::new().unwrap();
    let plat = minimal_plat(dir.path(), "info-json");

    let out = run(&["info", "--format", "json", plat.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "info --format json failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("info --format json output is JSON");
    assert_eq!(body["kind"], serde_json::json!("plat"));
    assert_eq!(body["url"], serde_json::json!("https://example.com/"));
    assert_eq!(body["title"], serde_json::json!("info-json"));
    assert_eq!(body["verified"], serde_json::json!(true));
}

#[test]
fn info_plat_hash_only_emits_just_the_hash() {
    let dir = TempDir::new().unwrap();
    let plat = minimal_plat(dir.path(), "info-hashonly");

    let out = run(&["info", "--hash-only", plat.to_str().unwrap()]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.trim();
    assert_eq!(line.len(), 64, "hash should be 64 hex chars; got: {line}");
    assert!(line.bytes().all(|b| b.is_ascii_hexdigit()));
}

#[test]
fn info_recognizes_action_hash() {
    let dir = TempDir::new().unwrap();
    let url = heso_core::Url::parse("https://example.com/").unwrap();
    let actions = serde_json::json!([]);
    let fp = heso_trace::trace_fingerprint(&url, &actions);
    let path = dir.path().join("fp.json");
    write_json(&path, &serde_json::to_value(&fp).unwrap());

    let out = run(&["info", "--format", "json", path.to_str().unwrap()]);
    assert!(out.status.success());
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(body["kind"], serde_json::json!("action-hash"));
    assert_eq!(body["algorithm"], serde_json::json!("heso-trace-fp/v1"));
}

#[test]
fn info_recognizes_template() {
    let dir = TempDir::new().unwrap();
    let template = serde_json::json!({
        "schema": "heso.template/v0",
        "id": "ca.heso.tests.info",
        "version": "0.1.0",
        "domains": ["example.com"],
        "steps": [
            { "verb": "open", "url": "https://example.com/" }
        ]
    });
    let path = dir.path().join("tpl.json");
    write_json(&path, &template);

    let out = run(&["info", "--format", "json", path.to_str().unwrap()]);
    assert!(out.status.success(), "info failed; stderr={}", String::from_utf8_lossy(&out.stderr));
    let body: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(body["kind"], serde_json::json!("template"));
    assert_eq!(body["id"], serde_json::json!("ca.heso.tests.info"));
    assert_eq!(body["steps"], serde_json::json!(1));
}

#[test]
fn info_diff_two_plats_identical_exits_zero() {
    let dir = TempDir::new().unwrap();
    let a = minimal_plat(dir.path(), "a");
    let b = minimal_plat(dir.path(), "a");
    let out = run(&["info", a.to_str().unwrap(), b.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "diff failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("IDENTICAL"));
}

#[test]
fn info_diff_two_plats_different_exits_one() {
    let dir = TempDir::new().unwrap();
    let a = minimal_plat(dir.path(), "diff-a");
    let b = minimal_plat(dir.path(), "diff-b");
    let out = run(&["info", a.to_str().unwrap(), b.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("DIFFERENT"));
}

#[test]
fn info_rejects_diff_when_one_arg_is_not_a_plat() {
    let dir = TempDir::new().unwrap();
    let plat = minimal_plat(dir.path(), "diff-mix");
    let url = heso_core::Url::parse("https://example.com/").unwrap();
    let fp = heso_trace::trace_fingerprint(&url, &serde_json::json!([]));
    let fp_path = dir.path().join("fp.json");
    write_json(&fp_path, &serde_json::to_value(&fp).unwrap());

    let out = run(&["info", plat.to_str().unwrap(), fp_path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("diff mode only supports two plats"));
}
