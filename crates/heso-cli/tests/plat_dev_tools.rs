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

fn compute_plat_hash(value: &serde_json::Value) -> String {
    let path = write_temp("hash-input.plat", value.to_string().as_bytes());
    let out = Command::new(heso_bin())
        .args(["plat-hash", path.to_str().unwrap()])
        .output()
        .expect("spawn heso plat-hash");
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "plat-hash failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("plat-hash stdout is UTF-8")
        .trim()
        .to_owned()
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
    // Leave the embedded plat_hash stale on purpose: plat-diff must
    // compare recomputed content hashes, not trust the stored field.

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
fn plat_redact_present_field_changes_hash() {
    // Add a share-sensitive top-level field. It is emitted content, so it
    // contributes to the hash until redacted.
    let mut plat = minimal_plat();
    plat["cookies"] = serde_json::json!([{"name": "s", "value": "session-123"}]);
    let original_hash = compute_plat_hash(&plat);
    plat["plat_hash"] = serde_json::Value::String(original_hash.clone());
    let path = write_temp("redact-present.plat", plat.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-redact", "cookies", path.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert!(out.status.success(), "plat-redact failed: {}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("NEW plat_hash") && stderr.contains("signature is invalidated"),
        "expected hash-change warning, got:\n{stderr}"
    );

    let redacted: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("redacted output is JSON");
    assert!(
        redacted.get("cookies").is_none(),
        "`cookies` field should be removed from output"
    );
    assert_ne!(
        original_hash,
        redacted["plat_hash"].as_str().unwrap(),
        "redacting a present field must change plat_hash"
    );
    assert_eq!(
        redacted["plat_hash"].as_str().unwrap(),
        "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf",
        "redacted minimal plat should return to the V1 hash"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn plat_redact_present_scalar_warns_and_changes_hash() {
    let plat = minimal_plat();
    let path = write_temp("redact-noneph.plat", plat.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-redact", "title", path.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert!(out.status.success());

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("NEW plat_hash") && stderr.contains("signature is invalidated"),
        "expected hash-changes-signature-invalidated note, got:\n{stderr}"
    );

    let redacted: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("redacted output is JSON");
    assert!(redacted.get("title").is_none(), "title should be removed");
    assert_ne!(
        redacted["plat_hash"].as_str().unwrap(),
        "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf",
        "redacting a present scalar must change plat_hash"
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

// ============================================================================
// plat-seal / plat-unseal — Ed25519 envelope round-trip + adversarial cases
// ============================================================================

/// Generate a temp path for an identity key. Caller passes it to
/// `init_identity` to actually create the key on disk.
fn temp_key_path(suffix: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    p.push(format!("heso-key-{unique}-{suffix}"));
    p
}

/// Shell out to `heso identity init --path <path>` to mint a fresh
/// Ed25519 key. Cheaper than depending on `heso_core` from a CLI test —
/// keeps the test crate's dep graph at just the binary under test.
fn init_identity(path: &std::path::Path) {
    let out = Command::new(heso_bin())
        .args(["identity", "init", "--path", path.to_str().unwrap()])
        .output()
        .expect("spawn heso identity init");
    assert!(
        out.status.success(),
        "identity init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn plat_seal_roundtrip_unseal_is_valid() {
    let key = temp_key_path("seal-rt.key");
    init_identity(&key);
    let plat_path = write_temp("seal-rt.plat", minimal_plat().to_string().as_bytes());

    let sealed = Command::new(heso_bin())
        .args(["plat-seal", plat_path.to_str().unwrap(), "--key", key.to_str().unwrap()])
        .output()
        .expect("spawn plat-seal");
    assert!(
        sealed.status.success(),
        "plat-seal failed: {}",
        String::from_utf8_lossy(&sealed.stderr)
    );

    let sealed_path = write_temp("seal-rt-sealed.plat", &sealed.stdout);
    let unsealed = Command::new(heso_bin())
        .args(["plat-unseal", sealed_path.to_str().unwrap()])
        .output()
        .expect("spawn plat-unseal");
    assert!(
        unsealed.status.success(),
        "plat-unseal failed: stdout=`{}` stderr=`{}`",
        String::from_utf8_lossy(&unsealed.stdout),
        String::from_utf8_lossy(&unsealed.stderr),
    );
    let status: serde_json::Value =
        serde_json::from_slice(&unsealed.stdout).expect("unseal status is JSON");
    assert_eq!(status["status"], "valid");
    assert_eq!(status["alg"], "heso-plat/v1+ed25519");
    // public_key is non-empty base64
    assert!(
        status["public_key"].as_str().map(|s| !s.is_empty()).unwrap_or(false),
        "expected non-empty public_key in status"
    );

    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&plat_path);
    let _ = std::fs::remove_file(&sealed_path);
}

#[test]
fn plat_unseal_detects_content_tamper() {
    let key = temp_key_path("tamper.key");
    init_identity(&key);
    let plat_path = write_temp("tamper.plat", minimal_plat().to_string().as_bytes());

    let sealed = Command::new(heso_bin())
        .args(["plat-seal", plat_path.to_str().unwrap(), "--key", key.to_str().unwrap()])
        .output()
        .expect("spawn plat-seal");
    assert!(sealed.status.success());

    let mut sealed_json: serde_json::Value =
        serde_json::from_slice(&sealed.stdout).expect("sealed envelope JSON");
    sealed_json["content"]["title"] = serde_json::json!("HIJACKED");
    let sealed_path = write_temp("tamper-sealed.plat", sealed_json.to_string().as_bytes());

    let unsealed = Command::new(heso_bin())
        .args(["plat-unseal", sealed_path.to_str().unwrap()])
        .output()
        .expect("spawn plat-unseal");
    assert_eq!(
        unsealed.status.code(),
        Some(1),
        "tampered envelope must exit 1"
    );
    let stderr = String::from_utf8_lossy(&unsealed.stderr);
    assert!(
        stderr.contains("INVALID"),
        "expected INVALID marker, got:\n{stderr}"
    );

    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&plat_path);
    let _ = std::fs::remove_file(&sealed_path);
}

#[test]
fn plat_unseal_rejects_wrong_algorithm() {
    let key = temp_key_path("wrongalg.key");
    init_identity(&key);
    let plat_path = write_temp("wrongalg.plat", minimal_plat().to_string().as_bytes());

    let sealed = Command::new(heso_bin())
        .args(["plat-seal", plat_path.to_str().unwrap(), "--key", key.to_str().unwrap()])
        .output()
        .expect("spawn plat-seal");
    assert!(sealed.status.success());

    let mut sealed_json: serde_json::Value =
        serde_json::from_slice(&sealed.stdout).expect("sealed envelope JSON");
    sealed_json["alg"] = serde_json::json!("heso-plat/v999+ed25519");
    let sealed_path = write_temp("wrongalg-sealed.plat", sealed_json.to_string().as_bytes());

    let unsealed = Command::new(heso_bin())
        .args(["plat-unseal", sealed_path.to_str().unwrap()])
        .output()
        .expect("spawn plat-unseal");
    assert_eq!(
        unsealed.status.code(),
        Some(2),
        "wrong-alg envelope must exit 2"
    );
    let stderr = String::from_utf8_lossy(&unsealed.stderr);
    assert!(
        stderr.contains("WRONG ALGORITHM"),
        "expected WRONG ALGORITHM marker, got:\n{stderr}"
    );

    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&plat_path);
    let _ = std::fs::remove_file(&sealed_path);
}

#[test]
fn plat_unseal_extract_prints_inner_content() {
    let key = temp_key_path("extract.key");
    init_identity(&key);
    let plat_path = write_temp("extract.plat", minimal_plat().to_string().as_bytes());

    let sealed = Command::new(heso_bin())
        .args(["plat-seal", plat_path.to_str().unwrap(), "--key", key.to_str().unwrap()])
        .output()
        .expect("spawn plat-seal");
    let sealed_path = write_temp("extract-sealed.plat", &sealed.stdout);

    let unsealed = Command::new(heso_bin())
        .args(["plat-unseal", sealed_path.to_str().unwrap(), "--extract"])
        .output()
        .expect("spawn plat-unseal");
    assert!(unsealed.status.success());

    let content: serde_json::Value =
        serde_json::from_slice(&unsealed.stdout).expect("extracted content JSON");
    assert_eq!(content["url"], "https://example.com/");
    assert_eq!(content["title"], "Example");
    // The minted plat_hash equals the V1 fixture hash (seal canonicalizes
    // the same bytes the §1.9 vector pins).
    assert_eq!(
        content["plat_hash"].as_str().unwrap(),
        "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf"
    );

    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&plat_path);
    let _ = std::fs::remove_file(&sealed_path);
}

#[test]
fn plat_seal_refuses_pre_sealed_envelope() {
    let key = temp_key_path("double.key");
    init_identity(&key);

    let sealed = serde_json::json!({
        "alg": "heso-plat/v1+ed25519",
        "content": minimal_plat(),
        "signature": {
            "algorithm": "Ed25519",
            "public_key": "AAAA",
            "signature": "BBBB"
        }
    });
    let path = write_temp("double-sealed.plat", sealed.to_string().as_bytes());

    let out = Command::new(heso_bin())
        .args(["plat-seal", path.to_str().unwrap(), "--key", key.to_str().unwrap()])
        .output()
        .expect("spawn plat-seal");
    assert!(!out.status.success(), "expected failure for pre-sealed input");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("sealed envelope already"),
        "expected refuse-double-seal message, got:\n{stderr}"
    );

    let _ = std::fs::remove_file(&key);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn plat_seal_errors_when_key_missing() {
    let plat_path = write_temp("missingkey.plat", minimal_plat().to_string().as_bytes());
    let key = temp_key_path("never-existed.key");

    let out = Command::new(heso_bin())
        .args(["plat-seal", plat_path.to_str().unwrap(), "--key", key.to_str().unwrap()])
        .output()
        .expect("spawn plat-seal");
    assert!(!out.status.success(), "expected failure when key is missing");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("failed to load identity"),
        "expected identity-load error, got:\n{stderr}"
    );

    let _ = std::fs::remove_file(&plat_path);
}

#[test]
fn plat_seal_with_no_args_prints_usage_and_exits_2() {
    let out = Command::new(heso_bin())
        .args(["plat-seal"])
        .output()
        .expect("spawn heso");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage: heso plat-seal"));
}

#[test]
fn plat_unseal_with_no_args_prints_usage_and_exits_2() {
    let out = Command::new(heso_bin())
        .args(["plat-unseal"])
        .output()
        .expect("spawn heso");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("usage: heso plat-unseal"));
}

#[test]
fn plat_unseal_rejects_malformed_envelope() {
    // A plain plat (not wrapped) — missing `alg`/`signature` fields.
    let path = write_temp("malformed.plat", minimal_plat().to_string().as_bytes());
    let out = Command::new(heso_bin())
        .args(["plat-unseal", path.to_str().unwrap()])
        .output()
        .expect("spawn heso");
    assert_eq!(out.status.code(), Some(2), "expected exit 2 for malformed envelope");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a sealed envelope"),
        "expected malformed-envelope error, got:\n{stderr}"
    );

    let _ = std::fs::remove_file(&path);
}
