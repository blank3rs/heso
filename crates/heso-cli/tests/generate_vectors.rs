//! Generator for `generated-vectors.json` at the worktree root.
//!
//! This is the merge-seam handoff: the canonical conformance constants
//! another HESO/1.0 implementation (or the merge target) cross-checks
//! against. Every value is COMPUTED from the real public APIs
//! (`heso_engine_fetch::plat`, `heso_trace`) — nothing is hand-authored.
//! Running this test (re)writes the file:
//!
//!   cargo test -p heso-cli --test generate_vectors
//!
//! The file is intentionally checked in (it's a handoff artifact, not a
//! build output) and regenerated whenever the canonicalization, the
//! `seed` field, or the signing constants change.

use std::path::PathBuf;

use heso_core::IdentityKey;
use heso_engine_fetch::plat;
use heso_trace::{sign_receipt, trace_hash, Receipt, Trace};
use serde_json::{json, Value};

/// The fixed 32-byte seed shared by the sealed-envelope and receipt
/// vectors. All-zero so any implementation can reproduce the identity:
/// `IdentityKey::from_bytes(&[0u8; 32])`.
const FIXED_SEED: [u8; 32] = [0u8; 32];

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").unwrap();
    }
    s
}

/// The V1..V8 §1.9 plat bodies, WITH the recorded `seed` field — exactly
/// the shape `FetchPage::plat_body_base` now emits. Description strings
/// match the spec's vector names.
fn section_1_9_bodies_with_seed() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "V1",
            "minimal plat (with recorded seed)",
            json!({
                "input_url": "https://example.com/",
                "url": "https://example.com/",
                "title": "Example",
                "description": "",
                "tree": [],
                "actions": [],
                "seed": 0
            }),
        ),
        (
            "V2",
            "Merkle parent over two child plat_hashes",
            json!({
                "input_url": "https://example.com/",
                "url": "https://example.com/",
                "title": "Parent",
                "description": "",
                "tree": [],
                "actions": [],
                "linked_pages": [
                    {"url": "https://example.com/a", "plat_hash": "aaaa"},
                    {"url": "https://example.com/b", "plat_hash": "bbbb"}
                ],
                "seed": 0
            }),
        ),
        (
            "V3",
            "V1 plus populated telemetry (must hash differently)",
            json!({
                "input_url": "https://example.com/",
                "url": "https://example.com/",
                "title": "Example",
                "description": "",
                "tree": [],
                "actions": [],
                "cookies": [{"name": "s", "value": "session-123"}],
                "http_status": 200,
                "console": ["log"],
                "id": "page-uuid-7f3a2",
                "partial": false,
                "partial_reason": "ok",
                "seed": 0
            }),
        ),
        (
            "V4",
            "Unicode NFC (é = U+00E9)",
            json!({
                "input_url": "https://example.com/caf\u{00e9}",
                "url": "https://example.com/caf\u{00e9}",
                "title": "caf\u{00e9}",
                "description": "",
                "tree": [],
                "actions": [],
                "seed": 0
            }),
        ),
        (
            "V5",
            "Unicode NFD (é = U+0065 U+0301)",
            json!({
                "input_url": "https://example.com/cafe\u{0301}",
                "url": "https://example.com/cafe\u{0301}",
                "title": "cafe\u{0301}",
                "description": "",
                "tree": [],
                "actions": [],
                "seed": 0
            }),
        ),
        (
            "V6a",
            "title is empty string",
            json!({
                "input_url": "https://example.com/",
                "url": "https://example.com/",
                "title": "",
                "description": "",
                "tree": [],
                "actions": [],
                "seed": 0
            }),
        ),
        (
            "V6b",
            "title is null",
            json!({
                "input_url": "https://example.com/",
                "url": "https://example.com/",
                "title": null,
                "description": "",
                "tree": [],
                "actions": [],
                "seed": 0
            }),
        ),
        (
            "V6c",
            "title is absent",
            json!({
                "input_url": "https://example.com/",
                "url": "https://example.com/",
                "description": "",
                "tree": [],
                "actions": [],
                "seed": 0
            }),
        ),
        (
            "V8",
            "plat with a single ok step (§1.4.1)",
            json!({
                "input_url": "https://example.com/",
                "url": "https://example.com/",
                "title": "Stepped",
                "description": "",
                "tree": [],
                "actions": [],
                "plan": [{"verb": "open", "url": "https://example.com/"}],
                "steps": [{
                    "index": 0,
                    "verb": "open",
                    "action": {"verb": "open", "url": "https://example.com/"},
                    "url_before": "https://example.com/",
                    "url_after": "https://example.com/",
                    "status": "ok",
                    "observed": {"op": "open", "http_status": 200},
                    "started_at": "1970-01-01T00:00:00.000Z",
                    "finished_at": "1970-01-01T00:00:00.001Z"
                }],
                "seed": 0
            }),
        ),
    ]
}

fn worktree_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/heso-cli
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("worktree root is two levels above crates/heso-cli")
        .to_path_buf()
}

#[test]
fn generate_vectors_json() {
    // ---- §1.9 plat vectors (V1..V8), regenerated WITH the seed field ----
    let plat_vectors: Vec<Value> = section_1_9_bodies_with_seed()
        .into_iter()
        .map(|(id, description, body)| {
            json!({
                "id": id,
                "description": description,
                "input_json": body,
                "canonical_bytes_hex": hex(&plat::canonical_bytes(&body)),
                "plat_hash": plat::hash(&body),
            })
        })
        .collect();

    // ---- sealed-envelope vector (fixed all-zero seed) ----
    let key = IdentityKey::from_bytes(&FIXED_SEED);
    let sealed_body = json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": "Example",
        "description": "",
        "tree": [],
        "actions": []
    });
    let sealed = plat::seal(&key, sealed_body);
    // The vector must verify under the documented verify path.
    assert!(
        matches!(plat::open(&sealed), plat::OpenOutcome::Valid),
        "sealed-envelope vector must verify Valid"
    );
    let sealed_vector = json!({
        "seed_hex": hex(&FIXED_SEED),
        "public_key_b64": key.public_key_b64(),
        "canonical_bytes_hex": hex(&plat::canonical_bytes(&sealed.content)),
        "sealed_envelope_json": serde_json::to_value(&sealed).unwrap(),
        "expected_outcome": "Valid",
    });

    // ---- receipt vector (same fixed seed) ----
    let trace: Trace = Vec::new();
    let mut receipt = Receipt {
        trace: trace.clone(),
        trace_hash: trace_hash(&trace),
        seed: 0,
        ..Default::default()
    };
    sign_receipt(&key, &mut receipt);
    assert!(
        matches!(
            heso_trace::verify_receipt(&receipt),
            heso_trace::VerifyOutcome::Valid
        ),
        "receipt vector must verify Valid"
    );
    let receipt_vector = json!({
        "seed_hex": hex(&FIXED_SEED),
        "public_key_b64": key.public_key_b64(),
        "signed_receipt_json": serde_json::to_value(&receipt).unwrap(),
        "expected_outcome": "Valid",
    });

    // ---- assemble the seam file ----
    let doc = json!({
        "_comment": "HESO/1.0 Grade-0 conformance vectors. GENERATED by \
                     `cargo test -p heso-cli --test generate_vectors` — do not hand-edit. \
                     Plat vectors V1..V8 are the §1.9 bodies WITH the recorded `seed` field \
                     (default 0); their hashes differ from the bare §1.9 spec hashes because \
                     the body gained a field.",
        "spec": "HESO/1.0",
        "signing_domain_hex": hex(plat::SIGNING_DOMAIN),
        "envelope_alg": plat::ENVELOPE_ALG,
        "plat_vectors": plat_vectors,
        "sealed_envelope_vector": sealed_vector,
        "receipt_vector": receipt_vector,
    });

    let pretty = serde_json::to_string_pretty(&doc).expect("serialize vectors") + "\n";
    let out = worktree_root().join("generated-vectors.json");
    std::fs::write(&out, &pretty).expect("write generated-vectors.json");

    // Sanity: round-trips and carries every section.
    let parsed: Value = serde_json::from_str(&pretty).expect("file is valid JSON");
    assert_eq!(parsed["signing_domain_hex"], "6865736f2d706c61742f763100");
    assert_eq!(parsed["plat_vectors"].as_array().unwrap().len(), 9);
    assert_eq!(parsed["sealed_envelope_vector"]["expected_outcome"], "Valid");
    assert_eq!(parsed["receipt_vector"]["expected_outcome"], "Valid");
    eprintln!("wrote {}", out.display());
}
