//! Integration coverage for `heso replay --plan`.
//!
//! `heso replay --plan` extracts the `plan` array from a plat. Without
//! the flag, `replay` emits the step log.

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

#[test]
fn replay_plan_emits_plan_array() {
    let dir = TempDir::new().unwrap();
    let plan = serde_json::json!([
        { "verb": "open", "url": "https://example.com/" },
        { "verb": "click", "ref": "@e1" }
    ]);
    let mut body = serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": "Plan Flag",
        "description": "",
        "tree": [],
        "actions": [],
        "plan": plan.clone(),
        "steps": []
    });
    let hash = heso_engine_fetch::plat_hash(&body);
    body.as_object_mut()
        .unwrap()
        .insert("plat_hash".into(), serde_json::Value::String(hash));
    let plat = dir.path().join("p.plat");
    write_json(&plat, &body);

    let replay = run(&["replay", "--plan", plat.to_str().unwrap()]);
    assert!(
        replay.status.success(),
        "replay --plan failed; stderr={}",
        String::from_utf8_lossy(&replay.stderr)
    );
    let replay_body: serde_json::Value =
        serde_json::from_slice(&replay.stdout).expect("replay --plan stdout is JSON");
    assert_eq!(replay_body, plan, "replay --plan should match the embedded plan");
}

#[test]
fn replay_plan_errors_when_plan_absent() {
    let dir = TempDir::new().unwrap();
    let mut body = serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": "No Plan",
        "description": "",
        "tree": [],
        "actions": [],
    });
    let hash = heso_engine_fetch::plat_hash(&body);
    body.as_object_mut()
        .unwrap()
        .insert("plat_hash".into(), serde_json::Value::String(hash));
    let plat = dir.path().join("noplan.plat");
    write_json(&plat, &body);

    let out = run(&["replay", "--plan", plat.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("has no `plan` field"));
}

#[test]
fn replay_without_flag_still_emits_steps() {
    let dir = TempDir::new().unwrap();
    let mut body = serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": "Steps",
        "description": "",
        "tree": [],
        "actions": [],
        "plan": [],
        "steps": [
            { "index": 0, "verb": "open", "ok": true }
        ]
    });
    let hash = heso_engine_fetch::plat_hash(&body);
    body.as_object_mut()
        .unwrap()
        .insert("plat_hash".into(), serde_json::Value::String(hash));
    let plat = dir.path().join("withsteps.plat");
    write_json(&plat, &body);

    let out = run(&["replay", plat.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "replay failed; stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert!(parsed.get("steps").is_some(), "replay must still emit steps when --plan is absent");
}
