//! # heso-verify
//!
//! The standalone HESO/1.0 **Grade 0** verifier — and the single source
//! of truth for the load-bearing operations a verifier performs:
//!
//! 1. **Canonicalization** ([`canonical_bytes`]) — RFC 8785 (JCS) bytes
//!    of a plat body with the top-level `plat_hash` **and** `sig` fields
//!    removed (the hash region). [`canonical_bytes_signing`] is the
//!    sibling that strips `sig` only (the inline-signature input).
//! 2. **Content hash** ([`plat_hash`] / [`verify_plat_hash`]) — BLAKE3
//!    over those canonical bytes (HESO/1.0 §1.8).
//! 3. **Sealed-envelope verification** ([`verify_sealed_plat`]) — the
//!    §3.4 three-step check: algorithm tag, content hash, then Ed25519
//!    `verify_strict` over `SIGNING_DOMAIN ++ canonical_bytes(content)`.
//! 4. **Inline-signature verification** ([`verify_inline_signature`] /
//!    [`InlineOutcome`]) — the default sign-at-stamp `sig` block: the
//!    ordered algorithm-tag / content-hash / Ed25519 `verify_strict`
//!    check over `SIGNING_DOMAIN_INLINE ++ canonical_bytes_signing(body)`,
//!    plus [`signer_fingerprint`] for the short `heso:<hex>` signer id.
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

/// Domain-separation tag prepended to the canonical plat bytes before
/// signing an **inline** `sig` (the default sign-at-stamp path): the
/// ASCII bytes of `heso-plat-sig:v1` then one NUL. Deliberately distinct
/// from [`SIGNING_DOMAIN`] (the `seal` envelope), the receipt's
/// prefixless payload, and the fingerprint DSTs, so an inline signature
/// can never be transplanted into a `seal` envelope or vice-versa.
pub const SIGNING_DOMAIN_INLINE: &[u8] = b"heso-plat-sig:v1\0";

/// Algorithm tag carried in an inline [`Signature::algorithm`] is still
/// `Ed25519`; this is the value of the inline `sig` object's own `alg`
/// field, checked before the signature so a verifier refuses an unknown
/// inline scheme rather than treating it as Ed25519.
pub const INLINE_SIG_ALG: &str = "heso-plat-sig/v1+ed25519";

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

/// Top-level keys stripped before canonicalizing for the **hash region**
/// (HESO/1.0 §1.8): a body cannot hash its own digest (`plat_hash`) nor
/// the signature minted over it (`sig`).
const HASH_REGION_STRIP: &[&str] = &["plat_hash", "sig"];

/// The single top-level key stripped before canonicalizing for the
/// **signing input**: a body cannot sign over its own signature. The
/// signing input keeps `plat_hash`, so the inline signature transitively
/// commits to the content digest as well as `lineage` and the rest of
/// the body.
const SIGNING_INPUT_STRIP: &[&str] = &["sig"];

fn strip_top_level(value: &Value, keys: &[&str]) -> Value {
    match value {
        Value::Object(map) if keys.iter().any(|k| map.contains_key(*k)) => {
            let mut stripped = map.clone();
            for k in keys {
                stripped.remove(*k);
            }
            Value::Object(stripped)
        }
        other => other.clone(),
    }
}

fn try_canonical_bytes_with(value: &Value, strip: &[&str]) -> Result<Vec<u8>, CanonError> {
    let cleaned = strip_top_level(value, strip);
    serde_jcs::to_vec(&cleaned).map_err(|e| CanonError(e.to_string()))
}

/// Canonical-JSON bytes of `value` over the **hash region** — top-level
/// `plat_hash` and `sig` removed — the fallible form of
/// [`canonical_bytes`].
///
/// Returns [`CanonError`] when `value` carries a non-finite number that
/// RFC 8785 cannot represent. Use this on any path that canonicalizes
/// page-derived content so a malformed value becomes a structured error
/// rather than a process abort.
pub fn try_canonical_bytes(value: &Value) -> Result<Vec<u8>, CanonError> {
    try_canonical_bytes_with(value, HASH_REGION_STRIP)
}

/// Canonical-JSON bytes of `value` over the **hash region** — the exact
/// bytes [`plat_hash`] hashes and a sealed envelope signs over.
///
/// Strips the top-level `plat_hash` (a hash field cannot contain its own
/// digest) and the top-level `sig` (a body cannot hash the signature
/// minted over it). Every other field is content; nested `plat_hash`
/// values (from `linked_pages[*]`) ARE preserved so a parent plat
/// cryptographically commits to its children's hashes (Merkle-style). A
/// body carrying no `sig` hashes byte-identically to one stripping
/// `plat_hash` alone.
///
/// Infallible: every internal producer feeds finite numbers only. Paths
/// that canonicalize page-derived content use [`try_canonical_bytes`].
pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    try_canonical_bytes(value).expect("plat value canonicalizes")
}

/// Canonical-JSON bytes of `value` over the **signing input** — top-level
/// `sig` removed, `plat_hash` KEPT — the fallible form of
/// [`canonical_bytes_signing`].
///
/// Returns [`CanonError`] when `value` carries a non-finite number that
/// RFC 8785 cannot represent.
pub fn try_canonical_bytes_signing(value: &Value) -> Result<Vec<u8>, CanonError> {
    try_canonical_bytes_with(value, SIGNING_INPUT_STRIP)
}

/// Canonical-JSON bytes of `value` over the **signing input** — the exact
/// bytes an inline `sig` is computed over after the
/// [`SIGNING_DOMAIN_INLINE`] prefix.
///
/// Strips only the top-level `sig` (a body cannot sign its own
/// signature). `plat_hash` is KEPT, so the inline signature commits to
/// the content digest, to `plat_hash` itself, and to `lineage` — the
/// inline analogue of how a sealed envelope signs over content that
/// carries `plat_hash`.
///
/// Infallible: every internal producer feeds finite numbers only. Paths
/// that canonicalize page-derived content use
/// [`try_canonical_bytes_signing`].
pub fn canonical_bytes_signing(value: &Value) -> Vec<u8> {
    try_canonical_bytes_signing(value).expect("plat value canonicalizes")
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

/// Short fingerprint of an Ed25519 public key, rendered `heso:<32-hex>`.
///
/// `blake3` of the raw 32 public-key bytes, first 16 bytes, lowercase
/// hex. Stable across machines and cheap to compare out-of-band. Lives
/// here so the Grade-0 verifier can print a signer fingerprint with
/// nothing but the artifact; `heso_core::IdentityKey::fingerprint` calls
/// through.
///
/// Returns `None` when `pubkey_b64` is not standard-alphabet base64 of
/// exactly 32 bytes.
pub fn signer_fingerprint(pubkey_b64: &str) -> Option<String> {
    let bytes = B64.decode(pubkey_b64.as_bytes()).ok()?;
    if bytes.len() != PUBLIC_KEY_LENGTH {
        return None;
    }
    let digest = blake3::hash(&bytes);
    Some(format!("heso:{}", hex_lower(&digest.as_bytes()[..16])))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("writing to a String never fails");
    }
    s
}

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
// Inline signature (the default sign-at-stamp `sig` field)
// ============================================================================

/// Outcome of [`verify_inline_signature`]. Mirrors [`Outcome`] (the
/// sealed-envelope shape) and adds [`InlineOutcome::Unsigned`] for a body
/// that carries no `sig` field at all.
#[derive(Debug)]
pub enum InlineOutcome {
    /// `sig.alg` matches, the embedded `plat_hash` matches the recomputed
    /// hash region, and the inline signature verifies. Carries the
    /// base64 public key so the caller can fingerprint the signer.
    Valid {
        /// Base64-encoded (standard alphabet) signer public key.
        public_key: String,
    },
    /// The body has no top-level `sig` — there is nothing to verify. The
    /// caller falls back to integrity-only (`plat_hash`) checking.
    Unsigned,
    /// The `sig` object carries an inner `alg` this verifier does not
    /// know (expected [`INLINE_SIG_ALG`]).
    WrongAlgorithm(String),
    /// The embedded `plat_hash` does not match the recomputed BLAKE3 of
    /// the hash region. The signature is not even checked — the content
    /// has been mutated and `HashMismatch` is the clearer diagnostic.
    HashMismatch,
    /// The inline signature does not verify against the signing input.
    InvalidSignature(SignatureError),
}

/// Verify a plat body's inline `sig` per the same ordering the sealed
/// envelope uses (mirrors [`open`]):
///
/// 1. No top-level `sig` → [`InlineOutcome::Unsigned`].
/// 2. `sig.alg` is [`INLINE_SIG_ALG`], else
///    [`InlineOutcome::WrongAlgorithm`] (algorithm-before-signature).
/// 3. `plat_hash` equals the recomputed BLAKE3 over the hash region
///    ([`canonical_bytes`]), else [`InlineOutcome::HashMismatch`] (the
///    signature is NOT checked — the clearer diagnostic).
/// 4. The inline `Signature` verifies against [`SIGNING_DOMAIN_INLINE`]
///    ++ [`canonical_bytes_signing`] of `body` via `verify_strict`, else
///    [`InlineOutcome::InvalidSignature`].
///
/// All passing = [`InlineOutcome::Valid`] carrying the signer public key.
pub fn verify_inline_signature(body: &Value) -> InlineOutcome {
    let sig_obj = match body.get("sig") {
        Some(Value::Object(map)) => map,
        Some(_) => {
            return InlineOutcome::InvalidSignature(SignatureError::Malformed(
                "inline `sig` is not an object",
            ))
        }
        None => return InlineOutcome::Unsigned,
    };

    let alg = sig_obj.get("alg").and_then(Value::as_str).unwrap_or("");
    // (1) algorithm-before-signature: refuse an unknown inline scheme.
    if alg != INLINE_SIG_ALG {
        return InlineOutcome::WrongAlgorithm(alg.to_owned());
    }

    let public_key = match sig_obj.get("public_key").and_then(Value::as_str) {
        Some(pk) => pk.to_owned(),
        None => {
            return InlineOutcome::InvalidSignature(SignatureError::Malformed(
                "inline `sig` missing `public_key`",
            ))
        }
    };
    let signature = match sig_obj.get("signature").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => {
            return InlineOutcome::InvalidSignature(SignatureError::Malformed(
                "inline `sig` missing `signature`",
            ))
        }
    };

    // (2) recompute plat_hash over the hash region; mismatch is the
    // clearer diagnostic, so the signature is not checked yet.
    let recomputed = plat_hash(body);
    let embedded = body.get("plat_hash").and_then(Value::as_str).unwrap_or("");
    if embedded != recomputed {
        return InlineOutcome::HashMismatch;
    }

    // (3) verify_strict over the domain-separated signing input. The
    // inline scheme tag lived in `sig.alg` and was checked above; the
    // underlying primitive is Ed25519, so we build the verify-only
    // envelope (whose `algorithm` is `Ed25519`) from the same key +
    // signature bytes and run the shared `verify_strict` path.
    let mut payload = Vec::with_capacity(SIGNING_DOMAIN_INLINE.len() + 256);
    payload.extend_from_slice(SIGNING_DOMAIN_INLINE);
    payload.extend_from_slice(&canonical_bytes_signing(body));
    let envelope = Signature {
        algorithm: SIG_ALGORITHM.to_owned(),
        public_key: public_key.clone(),
        signature,
    };
    match envelope.verify(&payload) {
        Ok(()) => InlineOutcome::Valid { public_key },
        Err(e) => InlineOutcome::InvalidSignature(e),
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

    // ========================================================================
    // Inline signature — strip-sets, golden vectors (§8.2), domain
    // separation (§8.5).
    //
    // The all-zero FIXED_SEED makes the signed vector reproducible by any
    // implementation: `SigningKey::from_bytes(&[0u8; 32])`, public key
    // `O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik=`. Ed25519 is
    // deterministic, so the signature is fixed across runs.
    // ========================================================================

    use ed25519_dalek::{Signer as _, SigningKey};

    const FIXED_SEED: [u8; 32] = [0u8; 32];

    fn to_hex(bytes: &[u8]) -> String {
        hex_lower(bytes)
    }

    /// The body the inline-signature vector signs: minimal, no `sig` yet,
    /// `plat_hash` stamped over the hash region (which strips `sig`, of
    /// which there is none yet).
    fn minimal_signed_body() -> Value {
        let mut body = json!({
            "input_url": "https://example.com/",
            "url": "https://example.com/",
            "title": "Example",
            "description": "",
            "tree": [],
            "actions": []
        });
        body["plat_hash"] = json!(plat_hash(&body));
        body
    }

    /// Mint the inline `sig` object over `body` exactly as the producer
    /// will: `SIGNING_DOMAIN_INLINE ++ canonical_bytes_signing(body)`,
    /// inner `alg = INLINE_SIG_ALG`. Test-local so heso-verify keeps no
    /// runtime signing capability.
    fn sign_inline_for_test(seed: &[u8; 32], body: &Value) -> Value {
        let sk = SigningKey::from_bytes(seed);
        let mut payload = Vec::new();
        payload.extend_from_slice(SIGNING_DOMAIN_INLINE);
        payload.extend_from_slice(&canonical_bytes_signing(body));
        let sig = sk.sign(&payload);
        json!({
            "alg": INLINE_SIG_ALG,
            "public_key": B64.encode(sk.verifying_key().to_bytes()),
            "signature": B64.encode(sig.to_bytes()),
        })
    }

    #[test]
    fn dump_inline_signing_domain() {
        // §8.2: SIGNING_DOMAIN_INLINE = ASCII "heso-plat-sig:v1" + one NUL.
        eprintln!("inline_signing_domain_hex: {}", to_hex(SIGNING_DOMAIN_INLINE));
        assert_eq!(to_hex(SIGNING_DOMAIN_INLINE), "6865736f2d706c61742d7369673a763100");
    }

    #[test]
    fn hash_region_and_signing_input_differ_only_by_plat_hash() {
        // The hash region strips {plat_hash, sig}; the signing input
        // strips {sig} only. On a body with a plat_hash and no sig, the
        // two regions differ by exactly the plat_hash member.
        let body = minimal_signed_body();
        let hash_region: Value =
            serde_json::from_slice(&canonical_bytes(&body)).unwrap();
        let signing_input: Value =
            serde_json::from_slice(&canonical_bytes_signing(&body)).unwrap();
        assert!(hash_region.get("plat_hash").is_none());
        assert_eq!(
            signing_input.get("plat_hash").and_then(Value::as_str),
            Some(body["plat_hash"].as_str().unwrap())
        );
        // Re-stripping plat_hash from the signing input recovers the hash
        // region byte-for-byte.
        assert_eq!(
            canonical_bytes(&signing_input),
            canonical_bytes(&body),
            "signing input minus plat_hash == hash region"
        );
    }

    #[test]
    fn dump_inline_signed_vector() {
        let mut body = minimal_signed_body();
        let sig = sign_inline_for_test(&FIXED_SEED, &body);
        body["sig"] = sig.clone();

        let hash_region_hex = to_hex(&canonical_bytes(&body));
        let signing_input_hex = to_hex(&canonical_bytes_signing(&body));
        let mut signing_bytes = Vec::new();
        signing_bytes.extend_from_slice(SIGNING_DOMAIN_INLINE);
        signing_bytes.extend_from_slice(&canonical_bytes_signing(&body));

        eprintln!("seed_hex:            {}", to_hex(&FIXED_SEED));
        eprintln!("public_key_b64:      {}", sig["public_key"].as_str().unwrap());
        eprintln!("canonical_bytes_hex (hash region):     {hash_region_hex}");
        eprintln!("canonical_bytes_signing_hex:           {signing_input_hex}");
        eprintln!("signing_input_hex (domain ++ signing): {}", to_hex(&signing_bytes));
        eprintln!("sig_object_json:     {}", serde_json::to_string(&sig).unwrap());

        // Pinned: the public key for the all-zero seed (shared with the
        // sealed-envelope / receipt vectors).
        assert_eq!(
            sig["public_key"].as_str().unwrap(),
            "O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik="
        );
        // Pinned: the 64-byte Ed25519 signature itself — the load-bearing
        // cross-impl anchor (§8.2's #1 risk). A fork that mis-canonicalizes
        // the signing input would self-sign + self-verify cleanly yet land
        // on a different signature; pinning the bytes is the only check that
        // catches that. The producer companion
        // (`dump_inline_signed_vector_matches_verify_crate` in
        // heso-engine-fetch) pins the identical value over the same seed +
        // body, so the two anchors must agree byte-for-byte.
        assert_eq!(
            sig["signature"].as_str().unwrap(),
            "TgyK/FJQe80g4+p2DRChjf667cQZM5U9+ONm9PlDebW+pl9c+gF/CxmT0Muao11Zt+IL0n+nNx7h9z9/iFtPAQ=="
        );
        // The hash region strips {plat_hash, sig}, so it reduces to the
        // bare minimal body — identical to the §1.9 V1 canonical bytes,
        // pinned by `section_1_9_v1_minimal_plat` to `bc272895…`.
        assert_eq!(
            to_hex(blake3::hash(canonical_bytes(&body).as_slice()).as_bytes()),
            "bc272895d75d0d780e6304e2cbd15a7a67819a3909c1aa5c51f7b5bbb28abccf",
            "hash region of the signed vector must equal the §1.9 V1 bare body"
        );
        // The signing input strips {sig} only, so it carries plat_hash;
        // the hash region does not. The signing input is therefore longer
        // and embeds the pinned V1 digest as a member.
        assert!(signing_input_hex.len() > hash_region_hex.len());
        assert!(signing_input_hex
            .contains(&to_hex(b"\"plat_hash\":\"bc272895")[..]));
        // The vector verifies under the documented inline verify path.
        match verify_inline_signature(&body) {
            InlineOutcome::Valid { public_key } => {
                assert_eq!(public_key, "O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik=");
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn inline_unsigned_body_reports_unsigned() {
        let body = minimal_signed_body();
        assert!(matches!(
            verify_inline_signature(&body),
            InlineOutcome::Unsigned
        ));
    }

    #[test]
    fn inline_wrong_algorithm_before_signature() {
        let mut body = minimal_signed_body();
        let mut sig = sign_inline_for_test(&FIXED_SEED, &body);
        sig["alg"] = json!("heso-plat-sig/v999+ed25519");
        body["sig"] = sig;
        match verify_inline_signature(&body) {
            InlineOutcome::WrongAlgorithm(a) => assert_eq!(a, "heso-plat-sig/v999+ed25519"),
            other => panic!("expected WrongAlgorithm, got {other:?}"),
        }
    }

    #[test]
    fn inline_hash_mismatch_when_content_mutated() {
        // Attacker edits content but leaves the old plat_hash + sig. The
        // recomputed hash region disagrees, and the signature is NOT
        // checked — HashMismatch is the clearer diagnostic.
        let mut body = minimal_signed_body();
        body["sig"] = sign_inline_for_test(&FIXED_SEED, &body);
        body["title"] = json!("hijacked");
        assert!(matches!(
            verify_inline_signature(&body),
            InlineOutcome::HashMismatch
        ));
    }

    #[test]
    fn inline_invalid_signature_when_resigned_hash_but_wrong_sig() {
        // Attacker edits content AND recomputes plat_hash so the hash
        // region matches, but the old signature no longer covers the new
        // signing input.
        let mut body = minimal_signed_body();
        body["sig"] = sign_inline_for_test(&FIXED_SEED, &body);
        body["title"] = json!("hijacked");
        body["plat_hash"] = json!(plat_hash(&body));
        assert!(matches!(
            verify_inline_signature(&body),
            InlineOutcome::InvalidSignature(_)
        ));
    }

    #[test]
    fn inline_domain_separation_rejects_prefixless_signature() {
        // §8.5: a bare Ed25519 signature over canonical_bytes_signing
        // WITHOUT the SIGNING_DOMAIN_INLINE prefix MUST be rejected — it
        // would otherwise allow transplanting a signature minted for a
        // different (prefixless) payload shape.
        let mut body = minimal_signed_body();
        let sk = SigningKey::from_bytes(&FIXED_SEED);
        let bare = sk.sign(&canonical_bytes_signing(&body));
        body["sig"] = json!({
            "alg": INLINE_SIG_ALG,
            "public_key": B64.encode(sk.verifying_key().to_bytes()),
            "signature": B64.encode(bare.to_bytes()),
        });
        assert!(matches!(
            verify_inline_signature(&body),
            InlineOutcome::InvalidSignature(_)
        ));
    }

    #[test]
    fn inline_domain_separation_rejects_seal_domain_signature() {
        // §8.5: a signature minted under the seal envelope's
        // SIGNING_DOMAIN must not verify as an inline sig (distinct
        // domains). Sign the signing input under SIGNING_DOMAIN, present
        // it as an inline sig — rejected.
        let mut body = minimal_signed_body();
        let sk = SigningKey::from_bytes(&FIXED_SEED);
        let mut payload = Vec::new();
        payload.extend_from_slice(SIGNING_DOMAIN);
        payload.extend_from_slice(&canonical_bytes_signing(&body));
        let cross = sk.sign(&payload);
        body["sig"] = json!({
            "alg": INLINE_SIG_ALG,
            "public_key": B64.encode(sk.verifying_key().to_bytes()),
            "signature": B64.encode(cross.to_bytes()),
        });
        assert!(matches!(
            verify_inline_signature(&body),
            InlineOutcome::InvalidSignature(_)
        ));
    }

    #[test]
    fn signer_fingerprint_is_heso_prefixed_32_hex() {
        let pk = "O2onvM62pC1io6jQKm8Nc2UyFXcd4kOmOsBIoYtZ2ik=";
        let fp = signer_fingerprint(pk).expect("valid pubkey");
        assert!(fp.starts_with("heso:"));
        let hex = &fp["heso:".len()..];
        assert_eq!(hex.len(), 32);
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        // Deterministic over the all-zero-seed public key.
        assert_eq!(fp, signer_fingerprint(pk).unwrap());
    }

    #[test]
    fn signer_fingerprint_rejects_malformed_pubkey() {
        assert_eq!(signer_fingerprint("not base64!!!"), None);
        // Valid base64 but wrong length.
        assert_eq!(signer_fingerprint(&B64.encode([0u8; 16])), None);
    }
}
