//! # heso-verify
//!
//! The standalone HESO/1.0 **Grade 0** verifier — and the single source
//! of truth for the three load-bearing operations a verifier performs:
//!
//! 1. **Canonicalization** ([`canonical_bytes`]) — RFC 8785 (JCS) bytes
//!    of a plat body with the top-level `plat_hash` field removed.
//! 2. **Content hash** ([`plat_hash`] / [`verify_plat_hash`]) — BLAKE3
//!    over those canonical bytes (HESO/1.0 §1.8).
//! 3. **Sealed-envelope verification** ([`verify_sealed_plat`]) — the
//!    §3.4 three-step check: algorithm tag, content hash, then Ed25519
//!    `verify_strict` over `SIGNING_DOMAIN ++ canonical_bytes(content)`.
//!
//! ## Why this crate exists
//!
//! A HESO/1.0 verifier must be runnable with **nothing but the
//! artifacts** — no engine, no DOM, no network, no clock. So this crate
//! depends only on `serde`, `serde_json`, `serde_jcs`, `blake3`,
//! `ed25519-dalek` (verify-only, no RNG), and `base64`. It MUST NOT
//! depend on any engine / DOM / network crate, and the engine
//! (`heso-engine-fetch`) depends DOWN on this crate — never the reverse.
//! The verify path lives here, in exactly one place; producers (`seal`)
//! live in the engine.
//!
//! ## `plat_hash` is excluded at the top level only
//!
//! The plat body may carry `plat_hash` as its own embedded BLAKE3
//! digest. That field is removed before canonicalizing for hashing — a
//! hash field cannot contain its own digest. Nested objects that happen
//! to have a `plat_hash` key (e.g. a `linked_pages[*]` child plat
//! carrying its own digest) are ordinary content and hash verbatim —
//! that's the Merkle-style commitment of a parent to its children
//! (HESO/1.0 §1.8).
//!
//! [RFC 8785]: https://datatracker.ietf.org/doc/html/rfc8785

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signature as DalekSignature, VerifyingKey, PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Domain-separation tag prepended to the canonical plat bytes before
/// signing (HESO/1.0 §3.2): the ASCII bytes of `heso-plat/v1` then one
/// NUL. A bare Ed25519 signature over the canonical bytes *without* this
/// prefix MUST be rejected — it prevents transplanting a signature
/// minted for another payload shape (receipts, fingerprints, …).
pub const SIGNING_DOMAIN: &[u8] = b"heso-plat/v1\0";

/// Envelope algorithm tag (HESO/1.0 §3.3). Verifiers refuse envelopes
/// carrying any other value rather than silently treating them as
/// Ed25519.
pub const ENVELOPE_ALG: &str = "heso-plat/v1+ed25519";

/// The algorithm name embedded in a [`Signature`] envelope. Currently
/// the only supported choice.
pub const SIG_ALGORITHM: &str = "Ed25519";

// ============================================================================
// Canonicalization + content hash (HESO/1.0 §1.7, §1.8)
// ============================================================================

/// A value could not be reduced to RFC 8785 canonical bytes.
///
/// The only shape that triggers this is a JSON number that is not finite
/// (`NaN` / `±Infinity`): RFC 8785 has no representation for it, so the
/// canonicalizer rejects it. Internal producers (`seal`, the stamp/run
/// flows, the conformance vectors) never construct such a value, so they
/// use the infallible [`canonical_bytes`] / [`plat_hash`]. Callers that
/// canonicalize page-derived content reach for the `try_*` variants and
/// surface this as a structured error instead of aborting the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonError(String);

impl std::fmt::Display for CanonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value is not RFC 8785 canonicalizable: {}", self.0)
    }
}

impl std::error::Error for CanonError {}

fn strip_top_level_plat_hash(value: &Value) -> Value {
    match value {
        Value::Object(map) if map.contains_key("plat_hash") => {
            let mut stripped = map.clone();
            stripped.remove("plat_hash");
            Value::Object(stripped)
        }
        other => other.clone(),
    }
}

/// Canonical-JSON bytes of `value` with any **top-level** `plat_hash`
/// field removed — the fallible form of [`canonical_bytes`].
///
/// Returns [`CanonError`] when `value` carries a non-finite number that
/// RFC 8785 cannot represent. Use this on any path that canonicalizes
/// page-derived content so a malformed value becomes a structured error
/// rather than a process abort.
pub fn try_canonical_bytes(value: &Value) -> Result<Vec<u8>, CanonError> {
    let cleaned = strip_top_level_plat_hash(value);
    serde_jcs::to_vec(&cleaned).map_err(|e| CanonError(e.to_string()))
}

/// Canonical-JSON bytes of `value` with any **top-level** `plat_hash`
/// field removed — the exact bytes [`plat_hash`] hashes and a sealed
/// envelope signs over.
///
/// Strips only the top-level `plat_hash`: a hash field cannot contain
/// its own digest. Every other field is content; nested `plat_hash`
/// values (from `linked_pages[*]`) ARE preserved so a parent plat
/// cryptographically commits to its children's hashes (Merkle-style).
///
/// Infallible: every internal producer feeds finite numbers only. Paths
/// that canonicalize page-derived content use [`try_canonical_bytes`].
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    try_canonical_bytes(value).expect("plat value canonicalizes")
}

/// Canonical-JSON of `value` (top-level `plat_hash` removed) as a UTF-8
/// string — the fallible form of [`canonical_json`].
pub fn try_canonical_json(value: &Value) -> Result<String, CanonError> {
    let bytes = try_canonical_bytes(value)?;
    Ok(String::from_utf8(bytes).expect("serde_jcs emits valid UTF-8"))
}

/// Canonical-JSON of `value` (top-level `plat_hash` removed) as a UTF-8
/// string. The string form of [`canonical_bytes`].
pub fn canonical_json(value: &Value) -> String {
    String::from_utf8(canonical_bytes(value)).expect("serde_jcs emits valid UTF-8")
}

/// Lowercase-hex BLAKE3 of the plat's canonical bytes — the fallible
/// form of [`plat_hash`].
///
/// Returns [`CanonError`] when `value` carries a non-finite number.
pub fn try_plat_hash(value: &Value) -> Result<String, CanonError> {
    Ok(blake3::hash(&try_canonical_bytes(value)?).to_hex().to_string())
}

/// Lowercase-hex BLAKE3 of the plat's canonical bytes, with the
/// top-level `plat_hash` field excluded (HESO/1.0 §1.8). 64 hex chars
/// (256 bits).
pub fn plat_hash(value: &Value) -> String {
    blake3::hash(&canonical_bytes(value)).to_hex().to_string()
}

/// Verify a plat's embedded `plat_hash` against a recomputed hash over
/// the rest of its canonical bytes.
///
/// `Err` distinguishes "no hash field" / "malformed hash field" from a
/// genuine mismatch. A real tamper signal is `Ok(false)`.
pub fn verify_plat_hash(plat: &Value) -> Result<bool, VerifyError> {
    let embedded = plat
        .get("plat_hash")
        .ok_or(VerifyError::MissingHashField)?
        .as_str()
        .ok_or(VerifyError::MalformedHashField)?;
    Ok(embedded == plat_hash(plat))
}

/// Errors from [`verify_plat_hash`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The plat has no `plat_hash` field — there is nothing to verify
    /// against.
    MissingHashField,
    /// The `plat_hash` field exists but is not a string.
    MalformedHashField,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::MissingHashField => f.write_str("plat JSON has no `plat_hash` field"),
            VerifyError::MalformedHashField => {
                f.write_str("plat JSON's `plat_hash` is not a string")
            }
        }
    }
}

impl std::error::Error for VerifyError {}

// ============================================================================
// Signature envelope (HESO/1.0 §3.1 `signature` object)
// ============================================================================

/// The on-the-wire signature envelope embedded in a [`SealedPlat`].
///
/// Byte-compatible with `heso_core::Signature` (same fields, same JSON
/// shape) — the engine signs with its `IdentityKey` and the resulting
/// signature deserializes into this type unchanged. All fields are
/// base64-encoded (standard alphabet) to keep the envelope JSON-safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    /// Always `"Ed25519"` for now. A bumpable string instead of an enum
    /// so future verifiers reading old envelopes get a clearer error.
    pub algorithm: String,
    /// Base64-encoded 32-byte Ed25519 public key.
    pub public_key: String,
    /// Base64-encoded 64-byte Ed25519 signature.
    pub signature: String,
}

impl Signature {
    /// Verify this signature against `payload` using
    /// `VerifyingKey::verify_strict` (which adds the "weak public key"
    /// check on top of standard Ed25519 verification — the envelope
    /// format makes weak keys an attacker-controlled input).
    pub fn verify(&self, payload: &[u8]) -> Result<(), SignatureError> {
        if self.algorithm != SIG_ALGORITHM {
            return Err(SignatureError::UnknownAlgorithm(self.algorithm.clone()));
        }
        let pk_bytes = B64
            .decode(self.public_key.as_bytes())
            .map_err(|_| SignatureError::Malformed("public_key not base64"))?;
        if pk_bytes.len() != PUBLIC_KEY_LENGTH {
            return Err(SignatureError::Malformed("public_key wrong length"));
        }
        let mut pk_arr = [0u8; PUBLIC_KEY_LENGTH];
        pk_arr.copy_from_slice(&pk_bytes);
        let vk = VerifyingKey::from_bytes(&pk_arr)
            .map_err(|_| SignatureError::Malformed("public_key not on curve"))?;

        let sig_bytes = B64
            .decode(self.signature.as_bytes())
            .map_err(|_| SignatureError::Malformed("signature not base64"))?;
        if sig_bytes.len() != SIGNATURE_LENGTH {
            return Err(SignatureError::Malformed("signature wrong length"));
        }
        let mut sig_arr = [0u8; SIGNATURE_LENGTH];
        sig_arr.copy_from_slice(&sig_bytes);
        let sig = DalekSignature::from_bytes(&sig_arr);

        vk.verify_strict(payload, &sig)
            .map_err(|_| SignatureError::VerificationFailed)
    }
}

/// Errors from [`Signature::verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureError {
    /// Algorithm string we don't recognize (expected `Ed25519`).
    UnknownAlgorithm(String),
    /// Structurally invalid envelope (bad base64, wrong length, key not
    /// on the curve).
    Malformed(&'static str),
    /// The signature did not verify against the payload.
    VerificationFailed,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureError::UnknownAlgorithm(a) => {
                write!(f, "unsupported signature algorithm `{a}` — expected Ed25519")
            }
            SignatureError::Malformed(why) => write!(f, "malformed signature envelope: {why}"),
            SignatureError::VerificationFailed => f.write_str("signature verification failed"),
        }
    }
}

impl std::error::Error for SignatureError {}

// ============================================================================
// Sealed envelope (HESO/1.0 §3)
// ============================================================================

/// A plat sealed with an Ed25519 signature over its canonical bytes
/// (HESO/1.0 §3.1).
///
/// The envelope is the unit of trust: holding a [`SealedPlat`] and a
/// HESO/1.0 verifier is sufficient to decide whether `content` was
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

/// Outcome of [`verify_sealed_plat`] / [`open`]. Three-way diagnostic to
/// mirror the verify CLI's exit-code shape (0 valid / 1 invalid /
/// 2 wrong-algorithm or mismatched hash).
#[derive(Debug)]
pub enum Outcome {
    /// Algorithm matches, embedded hash matches, signature verifies.
    Valid,
    /// Envelope carries an algorithm tag this verifier does not know.
    WrongAlgorithm(String),
    /// `content.plat_hash` does not match the recomputed BLAKE3. The
    /// signature is not even checked — the content has been mutated and
    /// `HashMismatch` is the clearer diagnostic.
    HashMismatch,
    /// Signature does not verify against the canonical content bytes.
    InvalidSignature(SignatureError),
}

/// Verify a [`SealedPlat`] per the HESO/1.0 §3.4 ordering:
///
/// 1. `alg` is [`ENVELOPE_ALG`], else [`Outcome::WrongAlgorithm`].
/// 2. `content.plat_hash` equals the recomputed BLAKE3 of `content`
///    (§1.8), else [`Outcome::HashMismatch`] (the body was mutated; the
///    signature check is skipped — it would fail too, but `HashMismatch`
///    is the clearer error).
/// 3. The signature verifies against [`SIGNING_DOMAIN`] ++ canonical
///    bytes of `content`, else [`Outcome::InvalidSignature`].
///
/// All three passing = [`Outcome::Valid`].
pub fn open(sealed: &SealedPlat) -> Outcome {
    if sealed.alg != ENVELOPE_ALG {
        return Outcome::WrongAlgorithm(sealed.alg.clone());
    }
    let recomputed = plat_hash(&sealed.content);
    let embedded = sealed
        .content
        .get("plat_hash")
        .and_then(Value::as_str)
        .unwrap_or("");
    if embedded != recomputed {
        return Outcome::HashMismatch;
    }
    let mut payload = Vec::with_capacity(SIGNING_DOMAIN.len() + 256);
    payload.extend_from_slice(SIGNING_DOMAIN);
    payload.extend_from_slice(&canonical_bytes(&sealed.content));
    match sealed.signature.verify(&payload) {
        Ok(()) => Outcome::Valid,
        Err(e) => Outcome::InvalidSignature(e),
    }
}

/// Verify a sealed envelope from its raw JSON bytes — the
/// nothing-but-the-artifact entry point.
///
/// Parses `bytes` as a [`SealedPlat`] and runs [`open`]. A structurally
/// invalid envelope (not JSON, or missing `alg` / `content` /
/// `signature`) surfaces as [`Outcome::InvalidSignature`] with a
/// [`SignatureError::Malformed`] reason rather than a panic, so the CLI
/// can map any failure to a non-zero exit.
pub fn verify_sealed_plat(bytes: &[u8]) -> Outcome {
    match serde_json::from_slice::<SealedPlat>(bytes) {
        Ok(sealed) => open(&sealed),
        Err(_) => Outcome::InvalidSignature(SignatureError::Malformed(
            "input is not a sealed envelope (expected `alg`, `content`, `signature`)",
        )),
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

    #[test]
    fn canonical_form_sorts_keys_recursively() {
        let a = json!({"b": 1, "a": {"y": 2, "x": 1}});
        let b = json!({"a": {"x": 1, "y": 2}, "b": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
    }

    #[test]
    fn hash_is_deterministic_and_64_hex_chars() {
        let v = sample();
        assert_eq!(plat_hash(&v), plat_hash(&v));
        assert_eq!(plat_hash(&v).len(), 64);
        assert!(plat_hash(&v).chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_excludes_top_level_plat_hash_only() {
        let bare = sample();
        let mut with_top = sample();
        with_top["plat_hash"] = json!("deadbeef");
        assert_eq!(plat_hash(&bare), plat_hash(&with_top));
    }

    #[test]
    fn hash_includes_nested_plat_hash_fields() {
        let a = json!({"url": "x", "linked_pages": [{"url": "a", "plat_hash": "aaa"}]});
        let b = json!({"url": "x", "linked_pages": [{"url": "a", "plat_hash": "bbb"}]});
        assert_ne!(plat_hash(&a), plat_hash(&b));
    }

    #[test]
    fn verify_plat_hash_round_trip() {
        let mut v = sample();
        v["plat_hash"] = json!(plat_hash(&v));
        assert!(verify_plat_hash(&v).unwrap());
        v["title"] = json!("hijacked");
        assert!(!verify_plat_hash(&v).unwrap());
    }

    #[test]
    fn verify_plat_hash_missing_and_malformed() {
        assert_eq!(
            verify_plat_hash(&sample()),
            Err(VerifyError::MissingHashField)
        );
        let mut v = sample();
        v["plat_hash"] = json!(123);
        assert_eq!(verify_plat_hash(&v), Err(VerifyError::MalformedHashField));
    }

    // ---- §1.9 vector spot-check: V1 must reproduce the pinned hash ----

    #[test]
    fn section_1_9_v1_minimal_plat() {
        let v1 = json!({
            "input_url": "https://example.com/",
            "url": "https://example.com/",
            "title": "Example",
            "description": "",
            "tree": [],
            "actions": []
        });
        assert_eq!(
            plat_hash(&v1),
            "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf"
        );
    }
}
