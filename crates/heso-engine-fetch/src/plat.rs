//! # plat
//!
//! Content-addressing for a **plat** — the static page-cartography
//! artifact heso produces. A plat's *identity* is the BLAKE3 hash of its
//! **canonical-JSON serialization**. Two heso runs that produce the same
//! plat content produce a byte-identical hash; anyone holding the plat
//! JSON can recompute the hash and verify it hasn't been tampered with.
//!
//! ## Canonical-JSON
//!
//! The hash must be over a *byte-stable* form of the plat — same plat
//! content must always serialize to the same bytes, no matter who
//! produces it. We define our canonical form as:
//!
//! - Object keys are sorted lexicographically (recursively, depth-first).
//! - Compact: no insignificant whitespace.
//! - Standard JSON string escaping (via [`serde_json::to_string`] for the
//!   string-value subset).
//! - Numbers are emitted in `serde_json`'s default form (which preserves
//!   integer-vs-float distinction by serializing via the `Number` type).
//!
//! This is a subset of [RFC 8785 (JCS — JSON Canonicalization Scheme)]
//! sufficient for the value shapes the engine emits (no floats with
//! exponential form, no NaN/Infinity, no non-string object keys, no
//! duplicate keys). When we need full RFC 8785 conformance for
//! cross-vendor interop, we'll swap to a JCS crate; for v1 the in-tree
//! implementation is small, dependency-free, and explicit about its
//! constraints.
//!
//! ## Hash field
//!
//! When `plat_hash` is embedded in the plat JSON, it is **omitted from
//! the hash input** — otherwise you'd be hashing a hash of a hash. The
//! canonicalizer walks the value tree and skips any top-level
//! `plat_hash` key. Per-`LinkedPage` plat hashes (V2 Merkle-tree story)
//! are similarly skipped at their own level when computing.
//!
//! ## Honest scope
//!
//! - Hashing identifies a *content snapshot*. If the upstream page
//!   changes between fetches, the plat changes, and the hash changes —
//!   that's correct, the hash names "this specific view" not "this URL
//!   forever."
//! - Full network determinism (replaying the same recorded bytes from a
//!   network capture) is [ADR 0008]'s recording story — designed, not
//!   yet implemented. Not a blocker for hashing.
//! - Ed25519 signing (a second layer: "who produced this plat") is [ADR
//!   0005] territory and a clean add-on to a plat that already has a
//!   content hash. Out of scope for this module.
//!
//! [RFC 8785 (JCS — JSON Canonicalization Scheme)]: https://datatracker.ietf.org/doc/html/rfc8785
//! [ADR 0008]: ../../../decisions/0008-deterministic-execution.md
//! [ADR 0005]: ../../../decisions/0005-ed25519-identity.md

use serde_json::Value;

/// Hex-encoded BLAKE3 of the canonical-JSON serialization of `value`,
/// with any top-level `plat_hash` field omitted from the input. 64 hex
/// chars (256 bits).
pub fn hash(value: &Value) -> String {
    let canon = canonical_json(value);
    let h = blake3::hash(canon.as_bytes());
    h.to_hex().to_string()
}

/// Return the plat's canonical-JSON string (sorted keys, compact). The
/// public hashing path is [`hash`]; this is exposed primarily for tests
/// and for callers that want to see exactly what got hashed.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

/// Verify a plat: extract its embedded `plat_hash`, recompute the hash
/// over the rest of the content, return `Ok(true)` if they match.
///
/// `Err` is returned only if the input is missing `plat_hash` or it
/// isn't a string. A *mismatch* is `Ok(false)` — the call succeeded,
/// the answer is "no."
pub fn verify(plat: &Value) -> Result<bool, VerifyError> {
    let embedded = plat
        .get("plat_hash")
        .ok_or(VerifyError::MissingHashField)?
        .as_str()
        .ok_or(VerifyError::MalformedHashField)?
        .to_owned();
    let recomputed = hash(plat);
    Ok(embedded == recomputed)
}

/// Errors from [`verify`].
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// The plat JSON has no `plat_hash` field — nothing to verify against.
    #[error("plat JSON has no `plat_hash` field")]
    MissingHashField,
    /// The `plat_hash` field is present but isn't a string.
    #[error("plat JSON's `plat_hash` is not a string")]
    MalformedHashField,
}

// ============================================================================
// Canonicalizer
// ============================================================================

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
            // Delegate to serde_json for JSON-string escaping — it handles
            // \", \\, \n, \t, \uXXXX, etc. correctly.
            let escaped = serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned());
            out.push_str(&escaped);
        }
        Value::Array(arr) => {
            out.push('[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            // Skip the `plat_hash` field at every level — we hash the
            // content the field describes, not the field itself.
            keys.retain(|k| k.as_str() != "plat_hash");
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                let escaped = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_owned());
                out.push_str(&escaped);
                out.push(':');
                // SAFETY: `keys` came from `map.keys()`, so the lookup
                // can't fail.
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_form_sorts_object_keys() {
        let a = json!({"b": 1, "a": 2, "c": 3});
        let b = json!({"c": 3, "a": 2, "b": 1});
        // Same content, different insertion order → same canonical.
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_json(&a), r#"{"a":2,"b":1,"c":3}"#);
    }

    #[test]
    fn canonical_form_sorts_nested_objects() {
        let v = json!({
            "outer_b": {"y": 1, "x": 2},
            "outer_a": {"z": [3, 4], "w": {"j": null, "i": true}}
        });
        let canon = canonical_json(&v);
        // Outer keys sorted: a comes before b. Inner keys sorted.
        let expected = r#"{"outer_a":{"w":{"i":true,"j":null},"z":[3,4]},"outer_b":{"x":2,"y":1}}"#;
        assert_eq!(canon, expected);
    }

    #[test]
    fn canonical_form_preserves_array_order() {
        // Arrays have inherent order; canonical form must NOT sort.
        let v = json!([3, 1, 2]);
        assert_eq!(canonical_json(&v), "[3,1,2]");
    }

    #[test]
    fn canonical_form_escapes_strings() {
        let v = json!({"k": "line1\nline2\t\"quoted\""});
        let canon = canonical_json(&v);
        // The escaped form should round-trip through serde_json.
        let reparsed: Value = serde_json::from_str(&canon).unwrap();
        assert_eq!(reparsed, v);
    }

    #[test]
    fn hash_is_deterministic_and_64_hex_chars() {
        let v = json!({"a": 1, "b": [2, 3]});
        let h1 = hash(&v);
        let h2 = hash(&v);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_is_insertion_order_independent() {
        // Same content, different insertion order → same hash.
        let v1 = json!({"a": 1, "b": 2});
        let v2 = json!({"b": 2, "a": 1});
        assert_eq!(hash(&v1), hash(&v2));
    }

    #[test]
    fn hash_changes_when_content_changes() {
        let v1 = json!({"a": 1});
        let v2 = json!({"a": 2});
        let v3 = json!({"a": 1, "extra": "field"});
        assert_ne!(hash(&v1), hash(&v2));
        assert_ne!(hash(&v1), hash(&v3));
    }

    #[test]
    fn hash_field_is_excluded_from_input() {
        // The `plat_hash` field on the value is NOT hashed (otherwise we'd
        // be hashing the hash). Adding or changing it must not change the
        // computed hash.
        let v_no_hash = json!({"data": [1, 2, 3]});
        let v_with_hash = json!({"data": [1, 2, 3], "plat_hash": "deadbeef"});
        let v_with_diff_hash = json!({"data": [1, 2, 3], "plat_hash": "cafef00d"});
        assert_eq!(hash(&v_no_hash), hash(&v_with_hash));
        assert_eq!(hash(&v_no_hash), hash(&v_with_diff_hash));
    }

    #[test]
    fn verify_round_trip_passes() {
        let mut v = json!({"data": [1, 2, 3], "url": "https://example.com"});
        let h = hash(&v);
        v["plat_hash"] = json!(h);
        assert!(verify(&v).expect("verify succeeded"));
    }

    #[test]
    fn verify_detects_tampering() {
        // Compute a hash, embed it, then tamper with the content. Verify
        // must return Ok(false).
        let mut v = json!({"data": [1, 2, 3]});
        let h = hash(&v);
        v["plat_hash"] = json!(h);
        // Tamper.
        v["data"] = json!([1, 2, 999]);
        assert!(!verify(&v).expect("verify succeeded"));
    }

    #[test]
    fn verify_missing_hash_is_an_error() {
        let v = json!({"data": [1, 2, 3]});
        match verify(&v) {
            Err(VerifyError::MissingHashField) => {}
            other => panic!("expected MissingHashField, got {other:?}"),
        }
    }

    #[test]
    fn verify_malformed_hash_field_is_an_error() {
        let v = json!({"data": [1], "plat_hash": 42});
        match verify(&v) {
            Err(VerifyError::MalformedHashField) => {}
            other => panic!("expected MalformedHashField, got {other:?}"),
        }
    }

    #[test]
    fn nested_plat_hash_fields_are_also_skipped() {
        // A plat with linked_pages, each of which has its own plat_hash
        // (the V2 Merkle-tree shape), must hash the same as a plat with
        // those inner plat_hash fields removed — because we strip
        // plat_hash at EVERY level, not just the top.
        let with_inner = json!({
            "url": "x",
            "linked_pages": [
                {"url": "a", "plat_hash": "aaa"},
                {"url": "b", "plat_hash": "bbb"}
            ]
        });
        let without_inner = json!({
            "url": "x",
            "linked_pages": [{"url": "a"}, {"url": "b"}]
        });
        assert_eq!(hash(&with_inner), hash(&without_inner));
    }
}
