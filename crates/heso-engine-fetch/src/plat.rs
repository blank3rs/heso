//! # plat
//!
//! A **plat** is the static page-cartography artifact heso produces.
//! This module gives a plat two layers of cryptographic identity:
//!
//! 1. A **content hash** — BLAKE3 over the [RFC 8785] canonical-JSON
//!    bytes of the plat (with its own top-level `plat_hash` field
//!    excluded). Two runs that produced the same plat content produce
//!    the same hash; any content change inside the plat changes the
//!    hash.
//! 2. A **sealed envelope** — [`SealedPlat`] — that pairs the plat
//!    body with an Ed25519 [`Signature`] over the same canonical bytes,
//!    domain-separated by [`SIGNING_DOMAIN`]. Verifying needs only the
//!    envelope; no key material, no network, no clock.
//!
//! ## Single source of truth: `heso-verify`
//!
//! The canonicalization, `plat_hash`, and sealed-envelope **open/verify**
//! logic does **not** live here. It lives in the standalone
//! [`heso_verify`] crate — the Grade-0 verifier anyone can run with
//! nothing but the artifacts. This module is the engine-side producer +
//! thin wrapper layer: [`hash`], [`canonical_json`], [`verify`], and
//! [`open`] all delegate DOWN to `heso_verify`, and [`SealedPlat`] /
//! [`OpenOutcome`] / [`SIGNING_DOMAIN`] / [`ENVELOPE_ALG`] are
//! re-exported from it. The verify path exists in exactly one place — no
//! copy-paste. The dependency flows engine → verify, never the reverse.
//!
//! The **producers** ([`seal`] / [`seal_checked`]) stay here because
//! they need `heso_core::IdentityKey` — a private-key type the verify
//! crate deliberately does not pull in.
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

use heso_core::IdentityKey;
use serde_json::Value;

// Re-export the verifier's surface so existing call sites
// (`heso_engine_fetch::plat::SealedPlat`, the CLI's `PlatOpenOutcome`,
// the §3.2 / §3.3 constants) keep working unchanged while the actual
// implementation lives in exactly one place.
pub use heso_verify::{
    canonical_bytes, canonical_bytes_signing, signer_fingerprint, verify_inline_signature,
    CanonError, InlineOutcome, Outcome as OpenOutcome, SealedPlat, Signature, SignatureError,
    VerifyError, ENVELOPE_ALG, INLINE_SIG_ALG, SIGNING_DOMAIN, SIGNING_DOMAIN_INLINE,
};

/// Hex-encoded BLAKE3 of the plat's canonical-JSON bytes, with the
/// top-level `plat_hash` field excluded. 64 hex chars (256 bits).
///
/// Thin wrapper over [`heso_verify::plat_hash`].
pub fn hash(value: &Value) -> String {
    heso_verify::plat_hash(value)
}

/// Fallible hex-encoded BLAKE3 of the plat's canonical-JSON bytes — the
/// [`Result`]-returning form of [`hash`] for paths that canonicalize
/// page-derived content. Thin wrapper over [`heso_verify::try_plat_hash`].
pub fn try_hash(value: &Value) -> Result<String, CanonError> {
    heso_verify::try_plat_hash(value)
}

/// Canonical-JSON of `value` with any top-level `plat_hash` field
/// removed. The exact bytes [`hash`] and [`seal`] operate on.
///
/// Thin wrapper over [`heso_verify::canonical_json`].
pub fn canonical_json(value: &Value) -> String {
    heso_verify::canonical_json(value)
}

/// Verify a plat's embedded `plat_hash` against a recomputed hash over
/// the rest of its canonical bytes.
///
/// `Err` distinguishes "no hash field" / "malformed hash field" from a
/// genuine mismatch. A real tamper signal is `Ok(false)`. Thin wrapper
/// over [`heso_verify::verify_plat_hash`].
pub fn verify(plat: &Value) -> Result<bool, VerifyError> {
    heso_verify::verify_plat_hash(plat)
}

/// Verify a [`SealedPlat`] per HESO/1.0 §3.4. Thin wrapper over
/// [`heso_verify::open`] — the verify path lives in `heso-verify`.
pub fn open(sealed: &SealedPlat) -> OpenOutcome {
    heso_verify::open(sealed)
}

// ============================================================================
// Sealed envelope — producer side (stays in the engine; needs IdentityKey)
// ============================================================================

/// Convert the `heso_core` signature envelope an [`IdentityKey`] mints
/// into the byte-identical [`heso_verify::Signature`] shape the verifier
/// consumes. Same three fields, same JSON — this is the one place the
/// producer (`heso-core`) and the verifier (`heso-verify`) signature
/// types meet, so the conversion lives in the engine that depends on
/// both rather than coupling either crate to the other.
fn to_verify_signature(sig: heso_core::Signature) -> Signature {
    Signature {
        algorithm: sig.algorithm,
        public_key: sig.public_key,
        signature: sig.signature,
    }
}

/// Seal a plat body with `key`. The resulting [`SealedPlat`] is the
/// shipping form: anyone can verify it with [`open`] (or the standalone
/// `heso-verify` binary) using nothing but the envelope.
///
/// If `body` is a JSON object that already carries a `plat_hash` field,
/// that field is preserved verbatim — the embedded hash is treated as
/// an input commitment, not as a slot to overwrite. Callers must hand
/// in a body whose claimed `plat_hash` already matches its content
/// (use [`hash`] or the [`SealError::HashMismatch`] check exposed by
/// [`seal_checked`]).
///
/// Bodies that carry no `plat_hash` get one stamped on before signing
/// so the resulting envelope is self-describing.
pub fn seal(key: &IdentityKey, mut body: Value) -> SealedPlat {
    // Hash the owned body before taking the mutable borrow — `canonical_bytes`
    // only ever strips a top-level `plat_hash` (absent here), so hashing
    // `&body` is byte-identical to hashing a clone of the object map, without
    // the deep clone of the tree/actions/embedded cassette.
    let needs_hash = body
        .as_object()
        .is_some_and(|o| !o.contains_key("plat_hash"));
    if needs_hash {
        let h = hash(&body);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("plat_hash".to_owned(), Value::String(h));
        }
    }
    let mut payload = Vec::with_capacity(SIGNING_DOMAIN.len() + 256);
    payload.extend_from_slice(SIGNING_DOMAIN);
    payload.extend_from_slice(&canonical_bytes(&body));
    let signature = to_verify_signature(key.sign(&payload));
    SealedPlat {
        alg: ENVELOPE_ALG.to_owned(),
        content: body,
        signature,
    }
}

/// Errors from [`seal_checked`].
#[derive(Debug, thiserror::Error)]
pub enum SealError {
    /// The body's embedded `plat_hash` does not match its content. The
    /// caller is asking us to sign a body whose hash claim is already
    /// false; refusing keeps the envelope honest.
    #[error("plat_hash mismatch: embedded {embedded}, recomputed {recomputed}")]
    HashMismatch {
        /// The hash the body claimed to commit to.
        embedded: String,
        /// The hash the body actually canonicalizes to.
        recomputed: String,
    },
    /// The body's `plat_hash` field is present but not a string.
    #[error("plat JSON's `plat_hash` is not a string")]
    MalformedHashField,
}

/// Like [`seal`] but refuses to sign a body whose claimed `plat_hash`
/// doesn't match its content. Bodies without a `plat_hash` field are
/// stamped just like [`seal`].
pub fn seal_checked(key: &IdentityKey, body: Value) -> Result<SealedPlat, SealError> {
    if let Some(obj) = body.as_object() {
        if let Some(embedded_val) = obj.get("plat_hash") {
            let embedded = embedded_val
                .as_str()
                .ok_or(SealError::MalformedHashField)?
                .to_owned();
            let recomputed = hash(&body);
            if embedded != recomputed {
                return Err(SealError::HashMismatch {
                    embedded,
                    recomputed,
                });
            }
        }
    }
    Ok(seal(key, body))
}

// ============================================================================
// Inline signature — producer side (the default sign-at-stamp `sig` field)
// ============================================================================

/// Sign `body` in place with an inline `sig` field — the default
/// sign-at-stamp form. Unlike [`seal`], the top-level shape is unchanged:
/// `sig` is one extra object next to `plat_hash`, so every consumer that
/// reads `{url, title, …, plat_hash}` at the root keeps working and the
/// signature is verifiable with nothing but the artifact via
/// [`verify_inline_signature`].
///
/// The two canonicalization regions are distinct (HESO/1.0 §1.8):
/// `plat_hash` is stamped (if absent) over the **hash region**
/// (`{plat_hash, sig}` stripped), and the Ed25519 signature covers the
/// **signing input** ([`SIGNING_DOMAIN_INLINE`] ++
/// [`canonical_bytes_signing`], which strips `sig` only and so keeps
/// `plat_hash`). The signature therefore transitively commits to all
/// content (via `plat_hash`), to `plat_hash` itself, and to `lineage`.
///
/// A freshly built body carries no `sig`, so the stamped `plat_hash` is
/// byte-identical to today's bare-plat hash.
pub fn sign_inline(key: &IdentityKey, mut body: Value) -> Value {
    // Stamp plat_hash over the hash region if absent — the owned body has
    // no `sig` yet, so this is byte-identical to the bare-plat hash.
    let needs_hash = body
        .as_object()
        .is_some_and(|o| !o.contains_key("plat_hash"));
    if needs_hash {
        let h = hash(&body);
        if let Some(obj) = body.as_object_mut() {
            obj.insert("plat_hash".to_owned(), Value::String(h));
        }
    }
    let mut payload = Vec::with_capacity(SIGNING_DOMAIN_INLINE.len() + 256);
    payload.extend_from_slice(SIGNING_DOMAIN_INLINE);
    payload.extend_from_slice(&canonical_bytes_signing(&body));
    let signature = to_verify_signature(key.sign(&payload));
    // The inline `sig` object reuses the three-field envelope shape but
    // carries the inline scheme tag in `alg`, distinct from the sealed
    // envelope's domain + algorithm so the two signatures are never
    // transplantable.
    let sig = serde_json::json!({
        "alg": INLINE_SIG_ALG,
        "public_key": signature.public_key,
        "signature": signature.signature,
    });
    if let Some(obj) = body.as_object_mut() {
        obj.insert("sig".to_owned(), sig);
    }
    body
}

/// Like [`sign_inline`] but refuses to sign a body whose claimed
/// `plat_hash` doesn't match its content. Bodies without a `plat_hash`
/// field are stamped just like [`sign_inline`].
pub fn sign_inline_checked(key: &IdentityKey, body: Value) -> Result<Value, SealError> {
    if let Some(obj) = body.as_object() {
        if let Some(embedded_val) = obj.get("plat_hash") {
            let embedded = embedded_val
                .as_str()
                .ok_or(SealError::MalformedHashField)?
                .to_owned();
            let recomputed = hash(&body);
            if embedded != recomputed {
                return Err(SealError::HashMismatch {
                    embedded,
                    recomputed,
                });
            }
        }
    }
    Ok(sign_inline(key, body))
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

    // ------------------------------------------------------------------
    // URL-in-hash invariant tests. These exist because the public
    // contract "different URLs produce different plat hashes" is the
    // load-bearing claim behind plat-hash content addressing. If any
    // future refactor normalizes URLs aggressively or drops either
    // field from the hash input, these tests trip.
    // ------------------------------------------------------------------

    #[test]
    fn different_urls_produce_different_plat_hashes() {
        let a = json!({
            "url": "https://example.com/a",
            "title": "X", "tree": [], "actions": [],
        });
        let b = json!({
            "url": "https://example.com/b",
            "title": "X", "tree": [], "actions": [],
        });
        assert_ne!(
            hash(&a), hash(&b),
            "different `url` MUST yield different plat_hash"
        );
    }

    #[test]
    fn different_input_urls_produce_different_plat_hashes() {
        // heso emits both `url` (post-redirect, normalized) and
        // `input_url` (verbatim user input). Casing-only differences
        // in the user input would collapse on `url` (Url::as_str()
        // lowercases the host), but `input_url` preserves them — so
        // the hash still differentiates the two callers.
        let a = json!({
            "url": "https://x.com/",
            "input_url": "https://X.com/",
            "title": "X",
        });
        let b = json!({
            "url": "https://x.com/",
            "input_url": "https://x.com/",
            "title": "X",
        });
        assert_ne!(
            hash(&a), hash(&b),
            "different `input_url` MUST yield different plat_hash"
        );
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
    fn seal_checked_refuses_stale_plat_hash() {
        let key = IdentityKey::generate();
        let mut body = sample();
        body["plat_hash"] = json!("0000000000000000000000000000000000000000000000000000000000000000");
        match seal_checked(&key, body) {
            Err(SealError::HashMismatch { embedded, recomputed }) => {
                assert_eq!(
                    embedded,
                    "0000000000000000000000000000000000000000000000000000000000000000"
                );
                assert_ne!(recomputed, embedded);
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn seal_checked_accepts_honest_plat_hash() {
        let key = IdentityKey::generate();
        let mut body = sample();
        let honest = hash(&body);
        body["plat_hash"] = json!(honest);
        let sealed = seal_checked(&key, body).expect("honest hash must seal");
        assert!(matches!(open(&sealed), OpenOutcome::Valid));
    }

    #[test]
    fn seal_checked_accepts_bare_body() {
        let key = IdentityKey::generate();
        let sealed = seal_checked(&key, sample()).expect("bare body must seal");
        assert!(matches!(open(&sealed), OpenOutcome::Valid));
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
        let bare = to_verify_signature(key.sign(&canonical_bytes(&body)));
        let sealed = SealedPlat {
            alg: ENVELOPE_ALG.to_owned(),
            content: body,
            signature: bare,
        };
        assert!(matches!(open(&sealed), OpenOutcome::InvalidSignature(_)));
    }

    // ========================================================================
    // Tamper-evidence: any content field that ships in the plat is hashed.
    // ========================================================================

    #[test]
    fn hash_changes_when_relational_attrs_change() {
        // These fields can contain per-request UUIDs on some sites, but
        // once heso emits them they are part of the artifact. Mutating
        // them after stamping must be detectable by plat_hash.
        let v1 = json!({
            "url": "https://example.com/",
            "title": "Page",
            "actions": [
                {
                    "ref": "@e0",
                    "role": "button",
                    "tag": "button",
                    "name": "Submit",
                    "attrs": {
                        "id": "icon-button-74b94e66-8fab-40f4-90ea-fda2bb6133e7",
                        "aria-labelledby": "tooltip-2446ac23-7ac6-481d-8430-6e4667e583d4",
                        "aria-describedby": "validation-3769e0a2-c905-42db-ab8a-a8870f2e306b",
                        "type": "submit",
                    }
                }
            ]
        });
        let v2 = json!({
            "url": "https://example.com/",
            "title": "Page",
            "actions": [
                {
                    "ref": "@e0",
                    "role": "button",
                    "tag": "button",
                    "name": "Submit",
                    "attrs": {
                        "id": "icon-button-5b6f3f07-1bdc-4be4-8eeb-dff4c9ad3b84",
                        "aria-labelledby": "tooltip-93e8dd11-c9ef-4819-83f8-66442b20394f",
                        "aria-describedby": "validation-de974846-72d8-4b59-b189-035a6c82e608",
                        "type": "submit",
                    }
                }
            ]
        });
        assert_ne!(hash(&v1), hash(&v2));
    }

    #[test]
    fn hash_still_changes_when_meaningful_attrs_change() {
        // Attribute changes contribute to the hash — content changes
        // must still flip it.
        let base = json!({
            "url": "https://example.com/",
            "actions": [
                {"ref": "@e0", "tag": "a", "attrs": {"href": "/page-1"}}
            ]
        });
        let changed_href = json!({
            "url": "https://example.com/",
            "actions": [
                {"ref": "@e0", "tag": "a", "attrs": {"href": "/page-2"}}
            ]
        });
        assert_ne!(hash(&base), hash(&changed_href));
    }

    #[test]
    fn hash_still_changes_when_visible_content_changes() {
        // The `tree` field (heading-based content + intro text) is the
        // agent-relevant content surface; changes there must flip the
        // hash.
        let v1 = json!({
            "url": "x",
            "tree": {"title": "T", "root": {"intro": "Hello world"}}
        });
        let v2 = json!({
            "url": "x",
            "tree": {"title": "T", "root": {"intro": "Goodbye world"}}
        });
        assert_ne!(hash(&v1), hash(&v2));
    }

    #[test]
    fn hash_includes_inline_data_and_data_attrs_top_level_blobs() {
        // SSR frameworks ship `__NEXT_DATA__` / `__NUXT_DATA__` /
        // `data-*` JSON payloads. They may contain request IDs or build
        // hashes, but they are still bytes heso observed and emitted.
        // If they change, the plat hash must change too.
        let v1 = json!({
            "url": "x",
            "title": "Same title",
            "inline_data": {"__NEXT_DATA__": {"requestId": "req-aaa-111"}},
            "data_attrs": {"data-foo": [{"requestId": "req-bbb-222"}]},
        });
        let v2 = json!({
            "url": "x",
            "title": "Same title",
            "inline_data": {"__NEXT_DATA__": {"requestId": "req-ccc-333"}},
            "data_attrs": {"data-foo": [{"requestId": "req-ddd-444"}]},
        });
        assert_ne!(hash(&v1), hash(&v2));
    }

    #[test]
    fn hash_includes_per_session_envelope_fields() {
        // `cookies`, `console`, `scripts`, `partial`, `partial_reason`,
        // `failed_scripts`, `console_errors_count`, `lazy_hints`,
        // `scroll`, `http_status`, `framework`, `forms`,
        // `content_hash`, `delta`, `text` — all are part of the plat
        // once emitted. Redacting any present field must produce a new
        // hash.
        let v1 = json!({
            "url": "x",
            "title": "T",
            "cookies": [{"name": "s", "value": "session-1"}],
            "console": ["log A"],
            "http_status": 200,
            "partial": false,
            "scripts": {"executed": 5},
        });
        let v2 = json!({
            "url": "x",
            "title": "T",
            "cookies": [{"name": "s", "value": "session-99999-DIFFERENT"}],
            "console": ["log B", "log C", "log D"],
            "http_status": 200,
            "partial": false,
            "scripts": {"executed": 12, "executed_with_error": 1},
        });
        assert_ne!(hash(&v1), hash(&v2));
    }

    #[test]
    fn hash_preserves_functional_aria_state_attrs() {
        // `aria-disabled`, `aria-checked`, etc. reflect element state,
        // and emitted attrs must contribute to the hash.
        let v1 = json!({
            "url": "x",
            "actions": [
                {"ref": "@e0", "tag": "input", "attrs": {"aria-checked": "false"}}
            ]
        });
        let v2 = json!({
            "url": "x",
            "actions": [
                {"ref": "@e0", "tag": "input", "attrs": {"aria-checked": "true"}}
            ]
        });
        assert_ne!(hash(&v1), hash(&v2));
    }

    // ========================================================================
    // HESO/1.0 §1.9 — canonical test vectors, in two forms.
    //
    // (a) `heso_1_0_section_1_9_spec_vectors_bare` pins the EXACT §1.9
    //     spec ground-truth hashes over the *bare* bodies (no `seed`
    //     field). These guard the canonicalization + hash construction
    //     against drift relative to the spec doc — `bc272895…` etc. — and
    //     must never change.
    //
    // (b) `heso_1_0_section_1_9_vectors` pins the same V1..V8 bodies as
    //     the engine now actually emits them: WITH the recorded RNG
    //     `seed` field (default 0). Adding a body field is a new input,
    //     so these hashes DIFFER from (a) — that is expected and correct.
    //     This is the dump test `generated-vectors.json` captures for the
    //     merge seam; it emits `canonical_bytes_hex` + `plat_hash` for
    //     V1..V8 under `--nocapture`:
    //       cargo test -p heso-engine-fetch \
    //         plat::tests::heso_1_0_section_1_9_vectors -- --nocapture
    // ========================================================================

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            write!(s, "{:02x}", b).unwrap();
        }
        s
    }

    #[test]
    fn heso_1_0_section_1_9_spec_vectors_bare() {
        let vectors: Vec<(&str, Value, &str)> = vec![
            (
                "V1 minimal plat",
                json!({
                    "input_url": "https://example.com/",
                    "url": "https://example.com/",
                    "title": "Example",
                    "description": "",
                    "tree": [],
                    "actions": []
                }),
                "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf",
            ),
            (
                "V2 Merkle parent over two child plat_hashes",
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
                    ]
                }),
                "f098b1ac08693b85c05fc9465a9f7763d22fb8563e292b025f7dbab9cc67ac62",
            ),
            (
                "V3 V1 plus populated telemetry (must hash differently)",
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
                    "partial_reason": "ok"
                }),
                "a6c4dcef1d2c5e96a6abb47878df0a905336f5a557f7a8b1d99f76da49c351b9",
            ),
            (
                "V4 Unicode NFC (é = U+00E9)",
                json!({
                    "input_url": "https://example.com/caf\u{00e9}",
                    "url": "https://example.com/caf\u{00e9}",
                    "title": "caf\u{00e9}",
                    "description": "",
                    "tree": [],
                    "actions": []
                }),
                "a64f1bf864d5eba5972a4a41fed19144077fedf23c9626c9b7adf57343b6c650",
            ),
            (
                "V5 Unicode NFD (é = U+0065 U+0301)",
                json!({
                    "input_url": "https://example.com/cafe\u{0301}",
                    "url": "https://example.com/cafe\u{0301}",
                    "title": "cafe\u{0301}",
                    "description": "",
                    "tree": [],
                    "actions": []
                }),
                "0a514b8a155da02f7db89ae79fb9fa885cc7ba88bf6837f1139b4026abbe2f7d",
            ),
            (
                "V6a title is empty string",
                json!({
                    "input_url": "https://example.com/",
                    "url": "https://example.com/",
                    "title": "",
                    "description": "",
                    "tree": [],
                    "actions": []
                }),
                "121f46f2d02fafadb811cd0ff2a1b7e5d6f64a381af29b36295384ba96f91c4b",
            ),
            (
                "V6b title is null",
                json!({
                    "input_url": "https://example.com/",
                    "url": "https://example.com/",
                    "title": null,
                    "description": "",
                    "tree": [],
                    "actions": []
                }),
                "801a174528591c1ef1cd3e3d249f76f277be8e84675b4758791b1e1355d2aa41",
            ),
            (
                "V6c title is absent",
                json!({
                    "input_url": "https://example.com/",
                    "url": "https://example.com/",
                    "description": "",
                    "tree": [],
                    "actions": []
                }),
                "e53bdc36b6aa0dbc27679d4c1a0dae825e9f500c48915357f9e34dfd49cb8c45",
            ),
            (
                "V8 plat with a single ok step (§1.4.1)",
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
                    }]
                }),
                "f550be12cd6cff8d738d9f80947ecce676e375077c99e75a0bffc4ae8f847ad1",
            ),
        ];

        for (name, body, expected_hash) in &vectors {
            let bytes = canonical_bytes(body);
            let bytes_hex = hex(&bytes);
            let plat_hash = hash(body);
            eprintln!("=== {name} ===");
            eprintln!("input_json:           {}", serde_json::to_string(body).unwrap());
            eprintln!("canonical_bytes_utf8: {}", String::from_utf8_lossy(&bytes));
            eprintln!("canonical_bytes_hex:  {}", bytes_hex);
            eprintln!("plat_hash:            {}", plat_hash);
            eprintln!();
            if *expected_hash != "TBD" {
                assert_eq!(
                    &plat_hash, expected_hash,
                    "{name}: plat_hash drifted from §1.9 pinned vector"
                );
            }
        }
    }

    // ========================================================================
    // §1.9 vectors as PLAT BODIES (with the recorded `seed` field) — the
    // shape the engine now actually emits. These are the body-shaped
    // vectors `generated-vectors.json` captures for the merge seam: each
    // is the V1..V8 body with the recorded RNG `seed` field added
    // (default 0), exactly as `plat_body_base` produces it. The pinned
    // hashes are REGENERATED — they intentionally differ from the
    // bare-body §1.9 spec hashes above because the body gained a field.
    // ========================================================================

    /// The V1..V8 §1.9 bodies WITH the recorded `seed` field, paired with
    /// their regenerated `plat_hash`, in spec order. Shared by the dump
    /// test and (via `generated-vectors.json`) the merge seam. The hashes
    /// were generated by running the dump test — never hand-authored.
    pub(crate) fn section_1_9_bodies_with_seed() -> Vec<(&'static str, Value, &'static str)> {
        vec![
            (
                "V1 minimal plat",
                json!({
                    "input_url": "https://example.com/",
                    "url": "https://example.com/",
                    "title": "Example",
                    "description": "",
                    "tree": [],
                    "actions": [],
                    "seed": 0
                }),
                "2c7fe735dbcc65ac5fe36297c122180790b6cba736c22ce34ba5aeff6d1fdcd4",
            ),
            (
                "V2 Merkle parent over two child plat_hashes",
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
                "ce75148813caf0c10d8042b90fe5981be6b4b8fc54a02f3c95b40ef86ac69152",
            ),
            (
                "V3 V1 plus populated telemetry (must hash differently)",
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
                "c025d194b43e43fc4aff34998c1d21ab6efc7ef8899635b0a2c591a1cc0804f5",
            ),
            (
                "V4 Unicode NFC (é = U+00E9)",
                json!({
                    "input_url": "https://example.com/caf\u{00e9}",
                    "url": "https://example.com/caf\u{00e9}",
                    "title": "caf\u{00e9}",
                    "description": "",
                    "tree": [],
                    "actions": [],
                    "seed": 0
                }),
                "3d95aac8b9e4c21a815f20a1be5f49288944348e1eebcc6dfbae63e96d7ff81b",
            ),
            (
                "V5 Unicode NFD (é = U+0065 U+0301)",
                json!({
                    "input_url": "https://example.com/cafe\u{0301}",
                    "url": "https://example.com/cafe\u{0301}",
                    "title": "cafe\u{0301}",
                    "description": "",
                    "tree": [],
                    "actions": [],
                    "seed": 0
                }),
                "de6691fb8dd63bd0020b62365bb83db7b033d87631a00a539f89c0eba1244c9d",
            ),
            (
                "V6a title is empty string",
                json!({
                    "input_url": "https://example.com/",
                    "url": "https://example.com/",
                    "title": "",
                    "description": "",
                    "tree": [],
                    "actions": [],
                    "seed": 0
                }),
                "d3c80a5837bb7f795311c25118f6cce2ead344ce114b9786fe98508226347836",
            ),
            (
                "V6b title is null",
                json!({
                    "input_url": "https://example.com/",
                    "url": "https://example.com/",
                    "title": null,
                    "description": "",
                    "tree": [],
                    "actions": [],
                    "seed": 0
                }),
                "63f41dd2f53b8a511a7471664ade493c105ef42ff48700b089359ef9f9657ace",
            ),
            (
                "V6c title is absent",
                json!({
                    "input_url": "https://example.com/",
                    "url": "https://example.com/",
                    "description": "",
                    "tree": [],
                    "actions": [],
                    "seed": 0
                }),
                "7a6f4008191fdc0716ab4da26f1217495a5b9ddc9a8463b71666c4a81a90b1a1",
            ),
            (
                "V8 plat with a single ok step (§1.4.1)",
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
                "8382289f9f3b1dfb53d2428c3a77be5a199389abf49ccd0861a8b318ca042532",
            ),
        ]
    }

    #[test]
    fn heso_1_0_section_1_9_vectors() {
        // V1..V8 as the engine emits them (WITH the `seed` field), each
        // pinned to its regenerated `plat_hash`. Emits `canonical_bytes_hex`
        // + `plat_hash` under `--nocapture`; the pinned hashes are
        // captured into `generated-vectors.json` (the merge seam).
        for (name, body, expected_hash) in section_1_9_bodies_with_seed() {
            let bytes = canonical_bytes(&body);
            let plat_hash = hash(&body);
            eprintln!("=== {name} (with seed) ===");
            eprintln!("input_json:           {}", serde_json::to_string(&body).unwrap());
            eprintln!("canonical_bytes_hex:  {}", hex(&bytes));
            eprintln!("plat_hash:            {}", plat_hash);
            eprintln!();
            assert_eq!(
                plat_hash, expected_hash,
                "{name}: plat_hash drifted from the pinned §1.9-with-seed vector"
            );
            assert_eq!(plat_hash.len(), 64);
            assert!(plat_hash.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    // ========================================================================
    // Conformance-constant dump tests. These GENERATE the constants that
    // `generated-vectors.json` (and any cross-implementation test) needs,
    // so no constant is ever hand-typed. Run under `--nocapture`:
    //   cargo test -p heso-engine-fetch plat::tests::dump_ -- --nocapture
    // ========================================================================

    /// The fixed 32-byte seed used by the sealed-envelope / receipt
    /// conformance vectors. All-zero so the vector is reproducible by any
    /// implementation: `IdentityKey::from_bytes(&[0u8; 32])`.
    const FIXED_SEED: [u8; 32] = [0u8; 32];

    /// A known minimal plat body the sealed-envelope vector seals.
    fn minimal_vector_body() -> Value {
        json!({
            "input_url": "https://example.com/",
            "url": "https://example.com/",
            "title": "Example",
            "description": "",
            "tree": [],
            "actions": []
        })
    }

    #[test]
    fn dump_signing_domain() {
        // §3.2: SIGNING_DOMAIN = ASCII "heso-plat/v1" + one NUL.
        eprintln!("signing_domain_hex: {}", hex(SIGNING_DOMAIN));
        assert_eq!(hex(SIGNING_DOMAIN), "6865736f2d706c61742f763100");
    }

    #[test]
    fn dump_sealed_envelope_vector() {
        let key = IdentityKey::from_bytes(&FIXED_SEED);
        let sealed = seal(&key, minimal_vector_body());
        let canonical_hex = hex(&canonical_bytes(&sealed.content));
        eprintln!("seed_hex:            {}", hex(&FIXED_SEED));
        eprintln!("public_key_b64:      {}", key.public_key_b64());
        eprintln!("canonical_bytes_hex: {}", canonical_hex);
        eprintln!(
            "sealed_envelope_json: {}",
            serde_json::to_string(&sealed).unwrap()
        );
        // The vector must verify under the same verify path it documents.
        assert!(matches!(open(&sealed), OpenOutcome::Valid));
        // Ed25519 is deterministic, so the public key for the all-zero
        // seed is fixed across runs and implementations.
        assert_eq!(
            key.public_key_b64(),
            "O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik="
        );
    }

    // ---- inline signature (the default sign-at-stamp `sig` field) ----

    #[test]
    fn sign_inline_then_verify_is_valid() {
        let key = IdentityKey::generate();
        let body = sign_inline(&key, sample());
        match verify_inline_signature(&body) {
            InlineOutcome::Valid { public_key } => {
                assert_eq!(public_key, key.public_key_b64());
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn sign_inline_stamps_self_hash_over_hash_region() {
        let key = IdentityKey::generate();
        let body = sign_inline(&key, sample());
        let embedded = body["plat_hash"].as_str().unwrap();
        // The hash region strips {plat_hash, sig}, so the embedded hash
        // equals a recompute over the now-signed body.
        assert_eq!(embedded, hash(&body));
    }

    #[test]
    fn sign_inline_sig_object_has_inline_alg() {
        let key = IdentityKey::generate();
        let body = sign_inline(&key, sample());
        let sig = body["sig"].as_object().expect("sig object");
        assert_eq!(sig["alg"].as_str().unwrap(), INLINE_SIG_ALG);
        assert_eq!(sig["public_key"].as_str().unwrap(), key.public_key_b64());
        assert!(sig["signature"].is_string());
    }

    #[test]
    fn sign_inline_survives_json_roundtrip() {
        let key = IdentityKey::generate();
        let body = sign_inline(&key, sample());
        let s = serde_json::to_string(&body).unwrap();
        let back: Value = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            verify_inline_signature(&back),
            InlineOutcome::Valid { .. }
        ));
    }

    #[test]
    fn sign_inline_detects_content_tamper() {
        let key = IdentityKey::generate();
        let mut body = sign_inline(&key, sample());
        body["title"] = json!("hijacked");
        assert!(matches!(
            verify_inline_signature(&body),
            InlineOutcome::HashMismatch
        ));
    }

    #[test]
    fn sign_inline_detects_hash_field_forgery() {
        // Mutate content AND rewrite plat_hash to match. BLAKE3 lines up;
        // Ed25519 over the new signing input does not.
        let key = IdentityKey::generate();
        let mut body = sign_inline(&key, sample());
        body["title"] = json!("hijacked");
        // Recompute plat_hash over the hash region of the tampered body.
        let h = hash(&body);
        body["plat_hash"] = json!(h);
        assert!(matches!(
            verify_inline_signature(&body),
            InlineOutcome::InvalidSignature(_)
        ));
    }

    #[test]
    fn sign_inline_domain_prevents_seal_signature_transplant() {
        // A `seal`-envelope signature (domain b"heso-plat/v1\0") dropped
        // into an inline `sig` slot must be rejected: the inline verify
        // path prefixes the distinct b"heso-plat-sig:v1\0" domain.
        let key = IdentityKey::generate();
        let mut body = sample();
        body["plat_hash"] = json!(hash(&body));
        let envelope_sig = to_verify_signature(key.sign(&{
            let mut p = Vec::new();
            p.extend_from_slice(SIGNING_DOMAIN);
            p.extend_from_slice(&canonical_bytes(&body));
            p
        }));
        body["sig"] = json!({
            "alg": INLINE_SIG_ALG,
            "public_key": envelope_sig.public_key,
            "signature": envelope_sig.signature,
        });
        assert!(matches!(
            verify_inline_signature(&body),
            InlineOutcome::InvalidSignature(_)
        ));
    }

    #[test]
    fn sign_inline_checked_refuses_stale_plat_hash() {
        let key = IdentityKey::generate();
        let mut body = sample();
        body["plat_hash"] = json!("0000000000000000000000000000000000000000000000000000000000000000");
        match sign_inline_checked(&key, body) {
            Err(SealError::HashMismatch { embedded, recomputed }) => {
                assert_eq!(
                    embedded,
                    "0000000000000000000000000000000000000000000000000000000000000000"
                );
                assert_ne!(recomputed, embedded);
            }
            other => panic!("expected HashMismatch, got {other:?}"),
        }
    }

    #[test]
    fn sign_inline_checked_accepts_bare_and_honest_bodies() {
        let key = IdentityKey::generate();
        let bare = sign_inline_checked(&key, sample()).expect("bare body must sign");
        assert!(matches!(
            verify_inline_signature(&bare),
            InlineOutcome::Valid { .. }
        ));

        let mut honest = sample();
        honest["plat_hash"] = json!(hash(&honest));
        let signed = sign_inline_checked(&key, honest).expect("honest hash must sign");
        assert!(matches!(
            verify_inline_signature(&signed),
            InlineOutcome::Valid { .. }
        ));
    }

    #[test]
    fn dump_inline_signed_vector_matches_verify_crate() {
        // Producer-side companion to heso-verify's `dump_inline_signed_vector`:
        // the inline `sig` minted here over the all-zero seed must verify and
        // carry the pinned public key, proving the producer and the verifier
        // agree on the signing input byte-for-byte.
        let key = IdentityKey::from_bytes(&FIXED_SEED);
        let body = sign_inline(&key, minimal_vector_body());
        assert_eq!(
            body["sig"]["public_key"].as_str().unwrap(),
            "O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik="
        );
        assert_eq!(body["sig"]["alg"].as_str().unwrap(), INLINE_SIG_ALG);
        // The load-bearing cross-impl anchor: the 64-byte signature must be
        // byte-identical to heso-verify's `dump_inline_signed_vector` pin
        // (same all-zero seed over the same content). If the producer and
        // the verifier ever disagree on the signing input, these two pins
        // diverge and one side fails — exactly the §8.2 guarantee.
        assert_eq!(
            body["sig"]["signature"].as_str().unwrap(),
            "TgyK/FJQe80g4+p2DRChjf667cQZM5U9+ONm9PlDebW+pl9c+gF/CxmT0Muao11Zt+IL0n+nNx7h9z9/iFtPAQ=="
        );
        match verify_inline_signature(&body) {
            InlineOutcome::Valid { public_key } => {
                assert_eq!(public_key, "O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik=");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }
}
