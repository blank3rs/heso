//! Integration tests for the plat dev-tool subcommands:
//! `heso plat-info`, `heso plat-diff`, `heso plat-redact`.
//!
//! These do not hit the network; each test writes a hand-built plat JSON
//! to a temp file and exercises the CLI verb against it. Pins the
//! observable behaviour each command commits to (output substrings,
//! exit codes, hash invariants), not the exact formatting.

use std::path::PathBuf;
use std::process::Command;

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn write_temp(suffix: &str, body: &[u8]) -> PathBuf {
    let mut p = std::env::temp_dir();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("heso-plat-dev-{unique}-{suffix}"));
    std::fs::write(&p, body).expect("write temp");
    p
}

/// Build a minimal plat (matching HESO/1.0 §1.9 V1 fixture) with the
/// embedded plat_hash recomputed by `heso plat-hash`.
fn minimal_plat() -> serde_json::Value {
    serde_json::json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": "Example",
        "description": "",
        "tree": [],
        "actions": [],
        "plat_hash": "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf"
    })
}

#[test]
fn plat_info_emits_summary_of_a_known_plat() {
    let plat = minimal_plat();
    let path = write_temp("info-min.plat", plat.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-info", path.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert!(out.status.success(), "plat-info exit nonzero: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Pin the LINES the summary commits to, not the exact formatting.
    for expected in [
        "plat_hash:",
        "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf",
        "verified:     yes",
        "url:",
        "https://example.com/",
        "title:        Example",
        "plan:         (no plan)",
        "cassette:     (no cassette)",
        "sealed:       no",
        "partial:      false",
    ] {
        assert!(
            stdout.contains(expected),
            "plat-info output missing `{expected}`. Full output:\n{stdout}"
        );
    }

    let _ = std::fs::remove_file(&path);
}

#[test]
fn plat_info_with_no_args_prints_usage_and_exits_2() {
    let out = Command::new(heso_bin())
        .args(["plat-info"])
        .output()
        .expect("spawn heso");
    assert_eq!(out.status.code(), Some(2), "expected exit code 2 for usage error");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage: heso plat-info"), "expected usage hint, got:\n{stderr}");
}

#[test]
fn plat_diff_identical_files_says_identical_and_exits_zero() {
    let plat = minimal_plat();
    let a = write_temp("diff-identical-a.plat", plat.to_string().as_bytes());
    let b = write_temp("diff-identical-b.plat", plat.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-diff", a.to_str().unwrap(), b.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert!(
        out.status.success(),
        "expected exit 0 for identical plats; got {:?}",
        out.status.code()
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("IDENTICAL"), "expected IDENTICAL marker:\n{stdout}");

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn plat_diff_different_plats_emits_differences_and_exits_one() {
    let plat_a = minimal_plat();
    let mut plat_b = minimal_plat();
    plat_b["title"] = serde_json::json!("Different Title");
    // After mutation, the embedded plat_hash is stale; plat-diff
    // recomputes when needed, so leaving it stale is fine for this test
    // (but recompute to be honest about what a real diff target looks
    // like in the wild).
    if let Some(obj) = plat_b.as_object_mut() {
        obj.remove("plat_hash");
    }

    let a = write_temp("diff-diff-a.plat", plat_a.to_string().as_bytes());
    let b = write_temp("diff-diff-b.plat", plat_b.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-diff", a.to_str().unwrap(), b.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for different plats");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("DIFFERENT"), "expected DIFFERENT marker:\n{stdout}");
    assert!(stdout.contains("title:"), "expected title diff line:\n{stdout}");

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn plat_redact_ephemeral_field_preserves_hash() {
    // Add an ephemeral field (`cookies`) to the minimal plat. The
    // ephemeral keys list strips it before hashing, so redacting it
    // MUST leave plat_hash unchanged.
    let mut plat = minimal_plat();
    plat["cookies"] = serde_json::json!([{"name": "s", "value": "session-123"}]);
    // plat_hash field still holds the V1 hash, which is correct because
    // `cookies` was stripped during canonicalization regardless of
    // whether the field was present.
    let path = write_temp("redact-eph.plat", plat.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-redact", "cookies", path.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert!(out.status.success(), "plat-redact failed: {}", String::from_utf8_lossy(&out.stderr));

    let redacted: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("redacted output is JSON");
    assert!(
        redacted.get("cookies").is_none(),
        "`cookies` field should be removed from output"
    );
    assert_eq!(
        redacted["plat_hash"].as_str().unwrap(),
        "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf",
        "ephemeral redact must preserve plat_hash"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn plat_redact_non_ephemeral_field_warns_and_changes_hash() {
    let plat = minimal_plat();
    let path = write_temp("redact-noneph.plat", plat.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-redact", "title", path.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert!(out.status.success());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("NOT in the ephemeral keys list"),
        "expected non-ephemeral warning, got:\n{stderr}"
    );
    assert!(
        stderr.contains("plat_hash") && stderr.contains("signature is invalidated"),
        "expected hash-changes-signature-invalidated note, got:\n{stderr}"
    );

    let redacted: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("redacted output is JSON");
    assert!(redacted.get("title").is_none(), "title should be removed");
    assert_ne!(
        redacted["plat_hash"].as_str().unwrap(),
        "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf",
        "non-ephemeral redact must change plat_hash"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn plat_redact_refuses_sealed_envelope() {
    let sealed = serde_json::json!({
        "alg": "heso-plat/v1+ed25519",
        "content": minimal_plat(),
        "signature": {
            "algorithm": "Ed25519",
            "public_key": "AAAA",
            "signature": "BBBB"
        }
    });
    let path = write_temp("redact-sealed.plat", sealed.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-redact", "anything", path.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert_eq!(out.status.code(), Some(1), "expected exit 1 for sealed envelope");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sealed envelope"),
        "expected sealed-envelope refusal, got:\n{stderr}"
    );

    let _ = std::fs::remove_file(&path);
}
