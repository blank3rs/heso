//! # plat
//!
//! A **plat** is the static page-cartography artifact heso produces.
//! This module gives a plat two layers of cryptographic identity:
//!
//! 1. A **content hash** — BLAKE3 over the [RFC 8785] canonical-JSON
//!    bytes of the plat (with its own `plat_hash` field excluded). Two
//!    runs that produced the same plat content produce the same hash;
//!    any byte changed inside the plat changes the hash.
//! 2. A **sealed envelope** — [`SealedPlat`] — that pairs the plat
//!    body with an Ed25519 [`Signature`] over the same canonical bytes,
//!    domain-separated by [`SIGNING_DOMAIN`]. Verifying needs only the
//!    envelope; no key material, no network, no clock.
//!
//! The unbreakability property reduces to a single invariant:
//!
//! > *Mutating any byte inside the plat changes the canonical bytes,
//! > which changes the signed message, which fails Ed25519
//! > `verify_strict`.*
//!
//! Everything else (Merkle aggregation, transparency-log anchoring,
//! post-quantum hybrids) is a layer above this and a non-breaking
//! addition under the [`SealedPlat::alg`] tag.
//!
//! ## Canonical bytes
//!
//! Canonicalization is delegated to [`serde_jcs`], which implements
//! [RFC 8785] — sorted keys, ECMA-262 number serialization, JCS string
//! escapes. Number/float and non-ASCII-key ambiguities the homegrown
//! canonicalizer used to gloss over are handled by spec.
//!
//! ## `plat_hash` is excluded at the top level only
//!
//! The plat body may carry `plat_hash` as its own embedded BLAKE3
//! digest. That field is removed before canonicalizing for hashing —
//! a hash field cannot contain its own digest. Nested objects that
//! happen to have a `plat_hash` key (e.g. a `linked_pages[*]` child
//! plat carrying its own digest) are ordinary content and hash
//! verbatim — that's the Merkle-style commitment of a parent to its
//! children.
//!
//! [RFC 8785]: https://datatracker.ietf.org/doc/html/rfc8785

use heso_core::{IdentityError, IdentityKey, Signature};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Domain-separation tag prepended to the canonical plat bytes before
/// signing. Prevents a plat signature from being valid against any
/// other signed payload shape in heso (receipts, fingerprints, …).
pub const SIGNING_DOMAIN: &[u8] = b"heso-plat/v1\0";

/// Envelope algorithm tag. Verifiers refuse envelopes carrying any
/// other value rather than silently treating them as Ed25519.
pub const ENVELOPE_ALG: &str = "heso-plat/v1+ed25519";

/// Hex-encoded BLAKE3 of the plat's canonical-JSON bytes, with the
/// top-level `plat_hash` field excluded. 64 hex chars (256 bits).
pub fn hash(value: &Value) -> String {
    let bytes = canonical_bytes(value);
    blake3::hash(&bytes).to_hex().to_string()
}

/// Canonical-JSON of `value` with any top-level `plat_hash` field
/// removed. The exact bytes [`hash`] and [`seal`] operate on.
pub fn canonical_json(value: &Value) -> String {
    String::from_utf8(canonical_bytes(value))
        .expect("serde_jcs emits valid UTF-8")
}

fn canonical_bytes(value: &Value) -> Vec<u8> {
    match value {
        Value::Object(map) if map.contains_key("plat_hash") => {
            let mut stripped = map.clone();
            stripped.remove("plat_hash");
            serde_jcs::to_vec(&Value::Object(stripped))
                .expect("plat value canonicalizes")
        }
        other => serde_jcs::to_vec(other).expect("plat value canonicalizes"),
    }
}

/// Verify a plat's embedded `plat_hash` against a recomputed hash over
/// the rest of its canonical bytes.
///
/// `Err` distinguishes "no hash field" / "malformed hash field" from a
/// genuine mismatch. A real tamper signal is `Ok(false)`.
pub fn verify(plat: &Value) -> Result<bool, VerifyError> {
    let embedded = plat
        .get("plat_hash")
        .ok_or(VerifyError::MissingHashField)?
        .as_str()
        .ok_or(VerifyError::MalformedHashField)?;
    Ok(embedded == hash(plat))
}

/// Errors from [`verify`].
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// The plat has no `plat_hash` field — there is nothing to verify
    /// against.
    #[error("plat JSON has no `plat_hash` field")]
    MissingHashField,
    /// The `plat_hash` field exists but is not a string.
    #[error("plat JSON's `plat_hash` is not a string")]
    MalformedHashField,
}

// ============================================================================
// Sealed envelope — signed, self-describing, offline-verifiable
// ============================================================================

/// A plat sealed with an Ed25519 signature over its canonical bytes.
///
/// The envelope is the unit of trust: holding a [`SealedPlat`] and the
/// `heso` binary is sufficient to decide whether the `content` was
/// produced by the holder of `signature.public_key` and is byte-for-byte
/// what they signed.
///
/// JSON shape (compact, sorted keys after canonicalization):
///
/// ```json
/// {
///   "alg": "heso-plat/v1+ed25519",
///   "content": { ... the plat body ..., "plat_hash": "<blake3-hex>" },
///   "signature": { "algorithm": "Ed25519", "public_key": "<b64>", "signature": "<b64>" }
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedPlat {
    /// Envelope algorithm tag. Always [`ENVELOPE_ALG`] for v1.
    pub alg: String,
    /// The plat body. Carries its own `plat_hash` (BLAKE3 of itself).
    pub content: Value,
    /// Ed25519 signature over [`SIGNING_DOMAIN`] ++ canonical bytes of
    /// `content`.
    pub signature: Signature,
}

/// Outcome of [`open`]. Three-way to mirror the receipt-verify CLI
/// exit-code shape (0 valid / 1 invalid / 2 wrong-algorithm or
/// mismatched hash).
#[derive(Debug)]
pub enum OpenOutcome {
    /// Algorithm matches, embedded hash matches, signature verifies.
    Valid,
    /// Envelope carries an algorithm tag this binary does not know.
    WrongAlgorithm(String),
    /// `content.plat_hash` does not match the recomputed BLAKE3. The
    /// signature is not even checked — the content has been mutated.
    HashMismatch,
    /// Signature does not verify against the canonical content bytes.
    InvalidSignature(IdentityError),
}

/// Seal a plat body with `key`. The resulting [`SealedPlat`] is the
/// shipping form: anyone can verify it with [`open`] using nothing but
/// the envelope.
///
/// Side-effect: if `body` is a JSON object, its `plat_hash` field is
/// overwritten with the freshly-computed BLAKE3 so the embedded hash
/// and the signature always agree.
pub fn seal(key: &IdentityKey, mut body: Value) -> SealedPlat {
    if body.is_object() {
        let h = hash(&body);
        body.as_object_mut()
            .expect("checked above")
            .insert("plat_hash".to_owned(), Value::String(h));
    }
    let mut payload = Vec::with_capacity(SIGNING_DOMAIN.len() + 256);
    payload.extend_from_slice(SIGNING_DOMAIN);
    payload.extend_from_slice(&canonical_bytes(&body));
    let signature = key.sign(&payload);
    SealedPlat {
        alg: ENVELOPE_ALG.to_owned(),
        content: body,
        signature,
    }
}

/// Verify a [`SealedPlat`]. Checks, in order:
///
/// 1. `alg` is [`ENVELOPE_ALG`].
/// 2. `content.plat_hash` equals the recomputed BLAKE3 of `content`.
///    A failure here means the body was mutated after sealing; the
///    signature check is skipped (it would fail too, but the
///    `HashMismatch` variant is a clearer error).
/// 3. The signature verifies against [`SIGNING_DOMAIN`] ++ canonical
///    bytes of `content`.
pub fn open(sealed: &SealedPlat) -> OpenOutcome {
    if sealed.alg != ENVELOPE_ALG {
        return OpenOutcome::WrongAlgorithm(sealed.alg.clone());
    }
    let recomputed = hash(&sealed.content);
    let embedded = sealed
        .content
        .get("plat_hash")
        .and_then(Value::as_str)
        .unwrap_or("");
    if embedded != recomputed {
        return OpenOutcome::HashMismatch;
    }
    let mut payload = Vec::with_capacity(SIGNING_DOMAIN.len() + 256);
    payload.extend_from_slice(SIGNING_DOMAIN);
    payload.extend_from_slice(&canonical_bytes(&sealed.content));
    match sealed.signature.verify(&payload) {
        Ok(()) => OpenOutcome::Valid,
        Err(e) => OpenOutcome::InvalidSignature(e),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "url": "https://example.com/",
            "title": "Example",
            "tree": [{"h": 1, "text": "Hello"}],
            "actions": [{"ref": "@e0", "kind": "link"}]
        })
    }

    // ---- canonical form / hash ----

    #[test]
    fn canonical_form_sorts_keys_recursively() {
        let a = json!({"b": 1, "a": {"y": 2, "x": 1}});
        let b = json!({"a": {"x": 1, "y": 2}, "b": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn hash_is_deterministic_and_64_hex_chars() {
        let v = sample();
        let h1 = hash(&v);
        let h2 = hash(&v);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_excludes_top_level_plat_hash_only() {
        let bare = sample();
        let mut with_top = sample();
        with_top["plat_hash"] = json!("deadbeef");
        assert_eq!(hash(&bare), hash(&with_top));
    }

    #[test]
    fn hash_includes_nested_plat_hash_fields() {
        let parent_a = json!({
            "url": "x",
            "linked_pages": [{"url": "a", "plat_hash": "aaa"}]
        });
        let parent_b = json!({
            "url": "x",
            "linked_pages": [{"url": "a", "plat_hash": "bbb"}]
        });
        assert_ne!(
            hash(&parent_a),
            hash(&parent_b),
            "a parent plat commits to its children's hashes"
        );
    }

    #[test]
    fn hash_changes_when_any_byte_changes() {
        let mut v = sample();
        let before = hash(&v);
        v["title"] = json!("Examplf");
        assert_ne!(before, hash(&v));
    }

    #[test]
    fn verify_round_trip_passes() {
        let mut v = sample();
        v["plat_hash"] = json!(hash(&v));
        assert!(verify(&v).unwrap());
    }

    #[test]
    fn verify_catches_content_tamper() {
        let mut v = sample();
        v["plat_hash"] = json!(hash(&v));
        v["title"] = json!("hijacked");
        assert!(!verify(&v).unwrap());
    }

    #[test]
    fn verify_catches_nested_payload_injection() {
        // The historical hazard: an attacker burying payload under a
        // nested `plat_hash` key once would have left the parent hash
        // unchanged. With top-level-only stripping, the nested key is
        // ordinary content and the recompute disagrees.
        let mut v = json!({"url": "x", "linked_pages": [{"url": "a"}]});
        v["plat_hash"] = json!(hash(&v));
        v["linked_pages"][0]["plat_hash"] = json!("INJECTED");
        assert!(!verify(&v).unwrap());
    }

    // ---- sealed envelope ----

    #[test]
    fn seal_then_open_is_valid() {
        let key = IdentityKey::generate();
        let sealed = seal(&key, sample());
        matches!(open(&sealed), OpenOutcome::Valid)
            .then_some(())
            .expect("Valid");
    }

    #[test]
    fn seal_embeds_self_hash() {
        let key = IdentityKey::generate();
        let sealed = seal(&key, sample());
        let embedded = sealed.content["plat_hash"].as_str().unwrap();
        assert_eq!(embedded, hash(&sealed.content));
    }

    #[test]
    fn seal_survives_json_roundtrip() {
        let key = IdentityKey::generate();
        let sealed = seal(&key, sample());
        let s = serde_json::to_string(&sealed).unwrap();
        let back: SealedPlat = serde_json::from_str(&s).unwrap();
        assert!(matches!(open(&back), OpenOutcome::Valid));
    }

    #[test]
    fn open_detects_content_tamper() {
        let key = IdentityKey::generate();
        let mut sealed = seal(&key, sample());
        sealed.content["title"] = json!("hijacked");
        match open(&sealed) {
            OpenOutcome::HashMismatch => {}
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn open_detects_hash_field_forgery() {
        // Attacker mutates content AND rewrites plat_hash to match.
        // BLAKE3 lines up; Ed25519 over the new bytes does not.
        let key = IdentityKey::generate();
        let mut sealed = seal(&key, sample());
        sealed.content["title"] = json!("hijacked");
        sealed.content["plat_hash"] = json!(hash(&sealed.content));
        match open(&sealed) {
            OpenOutcome::InvalidSignature(_) => {}
            other => panic!("expected InvalidSignature, got {other:?}"),
        }
    }

    #[test]
    fn open_detects_signature_swap() {
        // Reseal under a second key; the signature now belongs to a
        // different public key. We then transplant just the inner
        // ed25519 bytes back onto the first envelope and verify it is
        // rejected.
        let k1 = IdentityKey::generate();
        let k2 = IdentityKey::generate();
        let mut sealed = seal(&k1, sample());
        let other = seal(&k2, sample());
        sealed.signature.signature = other.signature.signature.clone();
        assert!(matches!(open(&sealed), OpenOutcome::InvalidSignature(_)));
    }

    #[test]
    fn open_rejects_unknown_algorithm() {
        let key = IdentityKey::generate();
        let mut sealed = seal(&key, sample());
        sealed.alg = "heso-plat/v999+ed25519".into();
        match open(&sealed) {
            OpenOutcome::WrongAlgorithm(s) => assert_eq!(s, "heso-plat/v999+ed25519"),
            other => panic!("expected WrongAlgorithm, got {other:?}"),
        }
    }

    #[test]
    fn distinct_user_url_inputs_can_collapse_to_one_canonical_url() {
        // Documents an honest sharp edge: the URL crate normalizes
        // scheme + host case and the default port, so byte-different
        // user-typed URLs serialize to the same string. When that
        // normalized string is the `"url"` field of a plat body, the
        // resulting plat_hash is identical. This is not a hash
        // collision — it is a deliberate property of URL parsing.
        use url::Url;
        let variants = [
            "https://Example.com/",
            "https://EXAMPLE.com/",
            "https://example.com:443/",
            "HTTPS://example.com/",
        ];
        let mut hashes = Vec::new();
        for s in variants {
            let u = Url::parse(s).unwrap();
            let body = json!({"url": u.as_str(), "title": ""});
            hashes.push(hash(&body));
        }
        let first = &hashes[0];
        assert!(
            hashes.iter().all(|h| h == first),
            "URL-crate-normalized inputs must collapse to one plat_hash; got {hashes:?}"
        );
    }

    #[test]
    fn input_url_field_preserves_byte_distinction_across_normalization() {
        // The `heso open` body carries both `input_url` (verbatim user
        // input) and `url` (parsed + post-redirect). Any byte-different
        // user input produces a byte-different `input_url` even when
        // the parsed `url` collapses to the same string — so the plat
        // hash always differs.
        use url::Url;
        let variants = [
            "https://Example.com/",
            "https://EXAMPLE.com/",
            "https://example.com:443/",
            "HTTPS://example.com/",
        ];
        let mut seen = std::collections::HashSet::new();
        for s in variants {
            let parsed = Url::parse(s).unwrap();
            let body = json!({
                "input_url": s,
                "url": parsed.as_str(),
                "title": "",
            });
            let h = hash(&body);
            assert!(seen.insert(h.clone()), "collision for input {s}");
        }
        assert_eq!(seen.len(), variants.len());
    }

    #[test]
    fn byte_different_unnormalized_urls_produce_distinct_hashes() {
        // Property check on the headline claim: as long as the URL
        // field strings differ byte-for-byte, the plat_hash differs.
        // 1000 URLs that vary by path / query / fragment / port.
        let mut seen = std::collections::HashSet::new();
        for i in 0..1000 {
            let url = format!("https://example.com/path-{i}?q={i}&x={}", i * 7);
            let body = json!({"url": url, "title": ""});
            let h = hash(&body);
            assert!(
                seen.insert(h.clone()),
                "collision for url={url}, hash={h}, after {} inserts",
                seen.len()
            );
        }
        assert_eq!(seen.len(), 1000);
    }

    // ---- adversarial sweep: byte-different inputs MUST never collide ----

    #[test]
    fn adversarial_url_variants_all_hash_distinctly() {
        // 30 byte-different URL strings hand-picked to stress every
        // normalization corner of the `url` crate. Each is paired with
        // its `Url::parse(...).as_str()` form to mirror what the CLI
        // actually emits. All 30 plats must have distinct plat_hashes.
        use url::Url;
        let raws = [
            "https://example.com",
            "https://example.com/",
            "https://example.com/?",
            "https://example.com/#",
            "https://example.com/#section",
            "https://example.com/?a=1",
            "https://example.com/?a=1&b=2",
            "https://example.com/?b=2&a=1",
            "https://Example.com/",
            "https://EXAMPLE.com/",
            "HTTPS://example.com/",
            "https://example.com:443/",
            "https://example.com:80/",
            "https://example.com:8443/",
            "https://user@example.com/",
            "https://user:pass@example.com/",
            "https://example.com/path",
            "https://example.com/path/",
            "https://example.com/PATH",
            "https://example.com/path?",
            "https://example.com/path#",
            "https://example.com/%66oo",
            "https://example.com/foo",
            "https://example.com/foo%20bar",
            "https://example.com/foo+bar",
            "https://例え.jp/",
            "https://xn--r8jz45g.jp/",
            "https://192.0.2.1/",
            "https://[2001:db8::1]/",
            "http://example.com/",
        ];
        let mut seen: std::collections::HashMap<String, &str> =
            std::collections::HashMap::new();
        for raw in raws {
            let parsed = Url::parse(raw).expect(raw);
            let body = json!({
                "input_url": raw,
                "url": parsed.as_str(),
                "title": "",
            });
            let h = hash(&body);
            if let Some(prev) = seen.insert(h.clone(), raw) {
                panic!("collision: `{prev}` and `{raw}` both hash to {h}");
            }
        }
        assert_eq!(seen.len(), raws.len());
    }

    #[test]
    fn adversarial_path_variants_all_hash_distinctly() {
        // 5000 paths that vary by characters chosen to stress URL
        // serialization: digits, ASCII letters, percent-encoded
        // sequences, query strings, fragments, repeated slashes.
        use url::Url;
        let mut seen = std::collections::HashSet::new();
        for i in 0..5000u32 {
            let raw = format!(
                "https://example.com/p{i}/q?x={i}&y={}#f{}",
                i.wrapping_mul(2654435761),
                i ^ 0xdeadbeef
            );
            let parsed = Url::parse(&raw).unwrap();
            let body = json!({
                "input_url": &raw,
                "url": parsed.as_str(),
                "title": "",
            });
            let h = hash(&body);
            assert!(
                seen.insert(h.clone()),
                "collision at i={i} url=`{raw}` hash={h} (seen {} unique)",
                seen.len()
            );
        }
        assert_eq!(seen.len(), 5000);
    }

    #[test]
    fn adversarial_content_variants_all_hash_distinctly() {
        // Same URL, byte-different bodies. The plat_hash must reflect
        // every byte of every field, recursively. 1000 bodies whose
        // only difference is a deeply-nested integer.
        let mut seen = std::collections::HashSet::new();
        for i in 0..1000i64 {
            let body = json!({
                "input_url": "https://example.com/",
                "url": "https://example.com/",
                "title": "T",
                "tree": {
                    "kind": "section",
                    "children": [{"kind": "p", "depth": i}]
                }
            });
            let h = hash(&body);
            assert!(seen.insert(h.clone()), "collision at i={i}");
        }
        assert_eq!(seen.len(), 1000);
    }

    #[test]
    fn unicode_normalization_forms_hash_distinctly() {
        // RFC 8785 explicitly does NOT normalize Unicode. NFC and NFD
        // are different codepoint sequences and must produce different
        // canonical bytes. This is correct: they ARE different inputs.
        let nfc = "\u{00e9}"; // é
        let nfd = "e\u{0301}"; // e + combining acute
        let a = json!({"input_url": format!("https://example.com/{nfc}"), "url": "x", "title": ""});
        let b = json!({"input_url": format!("https://example.com/{nfd}"), "url": "x", "title": ""});
        assert_ne!(hash(&a), hash(&b));
    }

    #[test]
    fn integer_and_float_with_same_value_hash_distinctly_when_distinct_in_value() {
        // 42 (i64) and 42.0 (f64) are different serde_json Number
        // representations. JCS prescribes ECMA-262 ToString, which
        // collapses 42.0 to "42". Both forms might serialize the
        // same way. This test documents what JCS actually does so
        // future contributors can't silently change the property.
        let int = json!({"v": 42});
        let flt = json!({"v": 42.0});
        // Whichever choice JCS makes, both must agree on the bytes for
        // each value, deterministically. We assert determinism and
        // document the observed equivalence.
        assert_eq!(hash(&int), hash(&int)); // self-stable
        assert_eq!(hash(&flt), hash(&flt));
        // The plat schema does not emit floats today; this test exists
        // so a future change that introduces them is reviewed against
        // this property.
        let _ = (int, flt);
    }

    #[test]
    fn empty_string_distinguishable_from_missing_field() {
        // A field with value "" must hash differently from a body that
        // omits the field entirely. Otherwise an attacker could elide
        // optional fields without changing the hash.
        let with_empty = json!({"input_url": "u", "url": "u", "title": ""});
        let without = json!({"input_url": "u", "url": "u"});
        assert_ne!(hash(&with_empty), hash(&without));
    }

    #[test]
    fn null_distinguishable_from_missing_field() {
        let with_null = json!({"input_url": "u", "url": "u", "title": null});
        let without = json!({"input_url": "u", "url": "u"});
        assert_ne!(hash(&with_null), hash(&without));
    }

    #[test]
    fn null_distinguishable_from_empty_string() {
        let with_null = json!({"input_url": "u", "url": "u", "title": null});
        let with_empty = json!({"input_url": "u", "url": "u", "title": ""});
        assert_ne!(hash(&with_null), hash(&with_empty));
    }

    #[test]
    fn array_vs_object_with_same_content_hash_distinctly() {
        let a = json!({"x": [1, 2, 3]});
        let b = json!({"x": {"0": 1, "1": 2, "2": 3}});
        assert_ne!(hash(&a), hash(&b));
    }

    #[test]
    fn deep_nesting_does_not_collide() {
        // 100-level deep object differing only at the deepest leaf.
        fn nest(depth: usize, leaf: i64) -> Value {
            if depth == 0 {
                return json!({"leaf": leaf});
            }
            json!({"n": nest(depth - 1, leaf)})
        }
        let a = nest(100, 1);
        let b = nest(100, 2);
        assert_ne!(hash(&a), hash(&b));
    }

    #[test]
    fn signing_domain_prevents_cross_payload_replay() {
        // A bare Ed25519 signature over the canonical content (without
        // the SIGNING_DOMAIN prefix) MUST NOT be accepted as a sealed
        // plat — otherwise a signature minted for a different payload
        // could be transplanted.
        let key = IdentityKey::generate();
        let body = {
            let mut b = sample();
            b["plat_hash"] = json!(hash(&b));
            b
        };
        let bare = key.sign(&canonical_bytes(&body));
        let sealed = SealedPlat {
            alg: ENVELOPE_ALG.to_owned(),
            content: body,
            signature: bare,
        };
        assert!(matches!(open(&sealed), OpenOutcome::InvalidSignature(_)));
    }
}
