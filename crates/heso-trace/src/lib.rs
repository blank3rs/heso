//! # heso-trace
//!
//! Trace AST types + receipt + cost + content addressing.
//!
//! - [`Trace`], [`PrimitiveOp`], [`PrimitiveResult`] — re-exported from
//!   `heso-primitives`.
//! - [`Receipt`] — what every `heso.run` call returns under the hood. Records
//!   what was run, what came back, cost, and an optional Ed25519
//!   [`Signature`] (item H, [ADR 0005]).
//! - [`Cost`] — bytes / cpu_ms / wall_ms / planner_tokens.
//! - [`Mode`] — `deterministic` (default) / `recording` / `live` per
//!   [ADR 0008].
//! - [`ContentHash`] — BLAKE3 hex digest, used for page-hash fingerprints.
//! - [`trace_hash`] — BLAKE3 over the canonical JSON of a [`Trace`]. Two
//!   equal traces produce the same hash byte-for-byte.
//! - [`canonical_receipt_json`] — sign-it-and-stamp canonical form: the
//!   receipt's JSON with `signature: null`, sorted keys, compact.
//! - [`sign_receipt`] / [`verify_receipt`] — stamp/check the Ed25519
//!   signature on a [`Receipt`].
//!
//! **No engine dependency.** The execution that produces a [`Receipt`] lives
//! in `heso-trace-exec`. This crate is pure data + signing so consumers
//! (planners, verifiers, downstream tools) can depend on it without dragging
//! in Servo.
//!
//! [ADR 0005]: ../../decisions/0005-ed25519-identity.md
//! [ADR 0008]: ../../decisions/0008-deterministic-execution.md

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use heso_core::{IdentityError, IdentityKey, Signature, SignaturePayload};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use heso_core::Signature as ReceiptSignature;
pub use heso_primitives::{PrimitiveOp, PrimitiveResult, Trace};

/// Operating mode for one trace run. Per [ADR 0008].
///
/// [ADR 0008]: ../../decisions/0008-deterministic-execution.md
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Default. Full reproducibility — fake clock, seeded RNG, recorded
    /// network, software rendering. Two runs with the same seed + recorded
    /// inputs produce byte-identical receipts.
    #[default]
    Deterministic,
    /// Real clocks, RNG, and network. Every input is logged for later
    /// deterministic replay.
    Recording,
    /// No guarantees. Identity refuses to sign in this mode (M4).
    Live,
}

/// BLAKE3 content hash, hex-encoded.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(pub String);

impl ContentHash {
    /// Compute the BLAKE3 hash of arbitrary bytes.
    ///
    /// ```
    /// use heso_trace::ContentHash;
    /// let a = ContentHash::of(b"hello");
    /// let b = ContentHash::of(b"hello");
    /// assert_eq!(a, b);
    /// assert_eq!(a.0.len(), 64); // 32 bytes hex = 64 chars
    /// ```
    pub fn of(bytes: &[u8]) -> Self {
        Self(blake3::hash(bytes).to_hex().to_string())
    }
}

/// Cost report for one trace run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cost {
    /// Bytes downloaded across the trace (real or recorded).
    pub bytes: u64,
    /// CPU time consumed, in milliseconds.
    pub cpu_ms: u64,
    /// Wall-clock time consumed (fake clock in deterministic mode), in
    /// milliseconds.
    pub wall_ms: u64,
    /// Planner tokens consumed. Filled by the planner (M3); always 0 at this
    /// layer.
    pub planner_tokens: u64,
}

/// Receipt of one trace run.
///
/// Per [ADR 0009], every `heso.run` call returns a receipt. The receipt
/// records the trace that was executed, what happened, the cost, and an
/// optional Ed25519 [`signature`](Receipt::signature) over a canonical
/// encoding of all of the above (item H, [ADR 0005]).
///
/// The unsigned receipt and the signed receipt are the same shape — the
/// `signature` field is optional. Serializing a receipt with
/// `signature == None` omits the field entirely, so unsigned receipts
/// remain backwards-compatible.
///
/// [ADR 0005]: ../../decisions/0005-ed25519-identity.md
/// [ADR 0009]: ../../decisions/0009-heso-run-single-tool.md
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// The full trace as planned.
    pub trace: Trace,
    /// Per-op results, parallel to `trace`. May be shorter than `trace.len()`
    /// if execution halted at [`Receipt::failed_at`].
    pub results: Vec<PrimitiveResult>,
    /// Content-addressed hashes of pages the engine fetched. Empty until the
    /// engine reports page hashes (post-T-013).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pages_seen: Vec<ContentHash>,
    /// BLAKE3 hash of the canonical JSON of [`Receipt::trace`]. Computed by
    /// [`trace_hash`]; lets verifiers detect tampering with the trace.
    pub trace_hash: String,
    /// Planner version ID. Filled by the planner (M3); empty at this layer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub planner_id: String,
    /// Session seed used.
    pub seed: u64,
    /// Operating mode this run used.
    pub mode: Mode,
    /// Cost report.
    pub cost: Cost,
    /// Index of the first failed op, if any. When set,
    /// `results.len() == failed_at.unwrap()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed_at: Option<usize>,
    /// Error message from the failed op, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Ed25519 signature over the canonical-JSON of this receipt with the
    /// `signature` field set to `null`. `None` when the receipt has not
    /// been signed (item H, [ADR 0005]).
    ///
    /// [ADR 0005]: ../../decisions/0005-ed25519-identity.md
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
}

impl Receipt {
    /// `true` if the trace ran to completion (every op produced a result).
    pub fn is_ok(&self) -> bool {
        self.failed_at.is_none()
    }
}

impl SignaturePayload for Receipt {
    /// Canonical-JSON of the receipt with `signature` cleared, encoded as
    /// UTF-8 bytes. The same shape verifiers use to recompute the digest.
    fn signing_payload(&self) -> Vec<u8> {
        canonical_receipt_json(self).into_bytes()
    }
}

/// Compute the BLAKE3 hash of a canonical JSON encoding of a trace.
///
/// Two equal traces produce the same hash, byte-for-byte. The encoding is
/// `serde_json`'s compact form. The hash is independent of which engine ran
/// the trace and what mode it ran in — it identifies the *intent*, not the
/// outcome.
///
/// ```
/// use heso_trace::trace_hash;
/// use heso_primitives::{PrimitiveOp, PwdInput};
///
/// let trace = vec![PrimitiveOp::Pwd(PwdInput::default())];
/// let h1 = trace_hash(&trace);
/// let h2 = trace_hash(&trace);
/// assert_eq!(h1, h2);
/// assert_eq!(h1.len(), 64); // BLAKE3 256-bit digest, hex-encoded
/// ```
pub fn trace_hash(trace: &Trace) -> String {
    let json = serde_json::to_string(trace).expect("trace serializes");
    blake3::hash(json.as_bytes()).to_hex().to_string()
}

// ============================================================================
// Canonical-JSON for signing
// ============================================================================

/// Canonical-JSON form of a receipt with `signature` cleared, suitable as
/// the byte input to Ed25519 signing/verifying.
///
/// Canonicalization rules (same as the [`plat`] module — chosen so two
/// implementations produce identical bytes):
///
/// - Object keys sorted lexicographically (recursively, depth-first).
/// - Compact: no insignificant whitespace.
/// - Strings escaped via `serde_json::to_string` for the string-value
///   subset (handles `\"`, `\\`, `\n`, `\t`, `\uXXXX`, etc.).
/// - Numbers via `serde_json::Number`'s `Display`, which preserves the
///   integer-vs-float distinction.
/// - The `signature` field is forced to `null` on the receipt object
///   itself before canonicalizing. The "sign it and stamp it" pattern:
///   sign over the receipt-without-signature, then write the signature
///   back into the same struct.
///
/// This is a subset of RFC 8785 (JSON Canonicalization Scheme) sufficient
/// for the value shapes a receipt emits. If we ever need full RFC 8785
/// conformance for cross-vendor interop we can swap in a JCS crate; for
/// v1 the in-tree implementation is small, dependency-free, and explicit
/// about its constraints.
///
/// [`plat`]: ../heso_engine_fetch/plat/index.html
pub fn canonical_receipt_json(receipt: &Receipt) -> String {
    let mut v = serde_json::to_value(receipt).expect("receipt serializes");
    // Force `signature` to JSON `null` on the top-level object. That gives
    // a single canonical "unsigned" shape regardless of whether the input
    // is a fresh (no field) or already-signed (Some(...)) receipt.
    if let Some(obj) = v.as_object_mut() {
        obj.insert("signature".to_owned(), Value::Null);
    }
    let mut out = String::new();
    write_canonical(&v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => {
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
// Signing / verifying
// ============================================================================

/// Sign `receipt` with `key`. Mutates the receipt in place: when this
/// returns, `receipt.signature` is `Some(sig)`.
///
/// Any pre-existing signature is discarded before the new one is
/// computed (the canonical form clears it anyway, so this just keeps the
/// in-memory struct consistent with what was signed).
pub fn sign_receipt(key: &IdentityKey, receipt: &mut Receipt) {
    receipt.signature = None;
    let payload = canonical_receipt_json(receipt).into_bytes();
    let sig = key.sign(&payload);
    receipt.signature = Some(sig);
}

/// Verify a receipt's embedded signature against its canonical form.
/// Returns:
///
/// - [`VerifyOutcome::Valid`] — the signature is present, the algorithm
///   matches, the public key + signature decode, and the verification
///   succeeds against the canonical receipt-without-signature bytes.
/// - [`VerifyOutcome::Missing`] — the receipt has no `signature` field.
/// - [`VerifyOutcome::Invalid(_)`] — the signature is present but
///   doesn't verify (tampered receipt, wrong key, malformed envelope).
pub fn verify_receipt(receipt: &Receipt) -> VerifyOutcome {
    let Some(sig) = receipt.signature.as_ref() else {
        return VerifyOutcome::Missing;
    };
    // Recompute canonical form with `signature` cleared.
    let mut probe = receipt.clone();
    probe.signature = None;
    let payload = canonical_receipt_json(&probe).into_bytes();
    match sig.verify(&payload) {
        Ok(()) => VerifyOutcome::Valid,
        Err(e) => VerifyOutcome::Invalid(e),
    }
}

/// Result of [`verify_receipt`]. Three-way for the CLI exit-code shape
/// the `receipt-verify` subcommand needs (0 valid / 1 invalid / 2
/// missing-or-malformed).
#[derive(Debug)]
pub enum VerifyOutcome {
    /// Signature present and verifies.
    Valid,
    /// Receipt has no `signature` field at all.
    Missing,
    /// Signature is present but verification failed.
    Invalid(IdentityError),
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use heso_primitives::{CdInput, CdTarget, PwdInput};

    fn url(s: &str) -> heso_primitives::Trace {
        vec![
            PrimitiveOp::Cd(CdInput {
                target: CdTarget::Url {
                    url: heso_core::Url::parse(s).unwrap(),
                },
            }),
            PrimitiveOp::Pwd(PwdInput::default()),
        ]
    }

    fn sample_receipt() -> Receipt {
        let trace = url("https://example.com/");
        Receipt {
            trace: trace.clone(),
            results: vec![],
            pages_seen: vec![ContentHash::of(b"page")],
            trace_hash: trace_hash(&trace),
            planner_id: "planner-v0".into(),
            seed: 42,
            mode: Mode::Deterministic,
            cost: Cost {
                bytes: 1024,
                cpu_ms: 5,
                wall_ms: 200,
                planner_tokens: 0,
            },
            failed_at: None,
            error: None,
            signature: None,
        }
    }

    #[test]
    fn mode_default_is_deterministic() {
        assert_eq!(Mode::default(), Mode::Deterministic);
    }

    #[test]
    fn mode_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(Mode::Deterministic).unwrap(),
            "deterministic"
        );
        assert_eq!(serde_json::to_value(Mode::Recording).unwrap(), "recording");
        assert_eq!(serde_json::to_value(Mode::Live).unwrap(), "live");
    }

    #[test]
    fn content_hash_is_64_hex_chars_and_deterministic() {
        let a = ContentHash::of(b"some bytes");
        let b = ContentHash::of(b"some bytes");
        assert_eq!(a, b);
        assert_eq!(a.0.len(), 64);
        assert!(a
            .0
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn content_hash_differs_for_different_inputs() {
        assert_ne!(ContentHash::of(b"a"), ContentHash::of(b"b"));
    }

    #[test]
    fn trace_hash_is_stable() {
        let t = url("https://example.com/");
        let h1 = trace_hash(&t);
        let h2 = trace_hash(&t);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn trace_hash_differs_for_different_traces() {
        let a = url("https://example.com/");
        let b = url("https://example.org/");
        assert_ne!(trace_hash(&a), trace_hash(&b));
    }

    #[test]
    fn receipt_roundtrips_through_json() {
        let mut receipt = sample_receipt();
        receipt.failed_at = Some(3);
        receipt.error = Some("oops".into());
        let json = serde_json::to_string(&receipt).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(receipt, back);
    }

    #[test]
    fn receipt_is_ok_when_no_failure() {
        let r = sample_receipt();
        assert!(r.is_ok());
    }

    #[test]
    fn receipt_is_not_ok_when_failed() {
        let mut r = sample_receipt();
        r.failed_at = Some(2);
        r.error = Some("err".into());
        assert!(!r.is_ok());
    }

    #[test]
    fn unsigned_receipt_omits_signature_field_from_json() {
        let r = sample_receipt();
        let s = serde_json::to_string(&r).unwrap();
        assert!(
            !s.contains("\"signature\""),
            "unsigned receipt JSON must omit signature field: {s}"
        );
    }

    // ------- canonical form -------

    #[test]
    fn canonical_form_clears_signature_field() {
        let mut r = sample_receipt();
        let unsigned = canonical_receipt_json(&r);

        // Now sign and recompute — the canonical form must be byte-identical
        // because canonicalization clears the signature before hashing.
        let key = IdentityKey::generate();
        sign_receipt(&key, &mut r);
        assert!(r.signature.is_some());
        let signed = canonical_receipt_json(&r);
        assert_eq!(unsigned, signed,
            "canonical form must clear signature; got\n  unsigned: {unsigned}\n  signed:   {signed}");
    }

    #[test]
    fn canonical_form_is_deterministic_for_equal_receipts() {
        let a = sample_receipt();
        let b = sample_receipt();
        assert_eq!(canonical_receipt_json(&a), canonical_receipt_json(&b));
    }

    #[test]
    fn canonical_form_does_not_pretty_print() {
        let r = sample_receipt();
        let c = canonical_receipt_json(&r);
        assert!(!c.contains('\n'));
        assert!(!c.contains("  "));
    }

    #[test]
    fn canonical_form_sorts_keys_at_top_level() {
        // The serialized receipt has many fields; canonical form must list
        // them in lexicographic order. Quick check: `cost` precedes
        // `error` precedes `failed_at` precedes `mode` precedes `pages_seen`
        // precedes `planner_id` precedes `results` precedes `seed`
        // precedes `signature` precedes `trace` precedes `trace_hash`.
        let mut r = sample_receipt();
        r.error = Some("e".into());
        r.failed_at = Some(1);
        let c = canonical_receipt_json(&r);
        let positions = [
            "\"cost\"",
            "\"error\"",
            "\"failed_at\"",
            "\"mode\"",
            "\"pages_seen\"",
            "\"planner_id\"",
            "\"results\"",
            "\"seed\"",
            "\"signature\"",
            "\"trace\"",
            "\"trace_hash\"",
        ];
        let mut prev = 0usize;
        for needle in positions {
            let idx = c
                .find(needle)
                .unwrap_or_else(|| panic!("missing key {needle} in canonical form"));
            assert!(
                idx > prev || prev == 0,
                "key {needle} at {idx} expected after position {prev} in {c}"
            );
            prev = idx;
        }
    }

    // ------- sign / verify roundtrip -------

    #[test]
    fn sign_then_verify_roundtrip_succeeds() {
        let key = IdentityKey::generate();
        let mut r = sample_receipt();
        sign_receipt(&key, &mut r);
        assert!(r.signature.is_some());
        match verify_receipt(&r) {
            VerifyOutcome::Valid => {}
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn verify_missing_signature_returns_missing() {
        let r = sample_receipt();
        match verify_receipt(&r) {
            VerifyOutcome::Missing => {}
            other => panic!("expected Missing, got {other:?}"),
        }
    }

    #[test]
    fn signature_verifies_after_json_roundtrip() {
        // Signing must survive serialize → deserialize, since the CLI is
        // going to write JSON and a separate process is going to read it.
        let key = IdentityKey::generate();
        let mut r = sample_receipt();
        sign_receipt(&key, &mut r);
        let json = serde_json::to_string(&r).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        match verify_receipt(&back) {
            VerifyOutcome::Valid => {}
            other => panic!("expected Valid after roundtrip, got {other:?}"),
        }
    }

    #[test]
    fn tampering_with_trace_hash_invalidates_signature() {
        let key = IdentityKey::generate();
        let mut r = sample_receipt();
        sign_receipt(&key, &mut r);
        // Mutate one byte of trace_hash.
        let mut chars: Vec<char> = r.trace_hash.chars().collect();
        chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
        r.trace_hash = chars.into_iter().collect();
        match verify_receipt(&r) {
            VerifyOutcome::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn tampering_with_seed_invalidates_signature() {
        let key = IdentityKey::generate();
        let mut r = sample_receipt();
        sign_receipt(&key, &mut r);
        r.seed = r.seed.wrapping_add(1);
        match verify_receipt(&r) {
            VerifyOutcome::Invalid(_) => {}
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn re_signing_overwrites_old_signature() {
        let k1 = IdentityKey::generate();
        let k2 = IdentityKey::generate();
        let mut r = sample_receipt();
        sign_receipt(&k1, &mut r);
        let pk1 = r.signature.as_ref().unwrap().public_key.clone();
        sign_receipt(&k2, &mut r);
        let pk2 = r.signature.as_ref().unwrap().public_key.clone();
        assert_ne!(pk1, pk2, "re-signing must rewrite the signature");
        match verify_receipt(&r) {
            VerifyOutcome::Valid => {}
            other => panic!("expected Valid after re-sign, got {other:?}"),
        }
    }

    #[test]
    fn canonical_form_is_invariant_under_object_key_shuffles() {
        // Property: two semantically-equal receipts produce byte-identical
        // canonical bytes regardless of the *construction* order of any
        // embedded objects.
        //
        // We test this at the canonicalizer level by routing a receipt
        // through `serde_json::Value` two ways: once via the natural
        // `to_value(&receipt)` path, and once via a hand-constructed Value
        // with all object key orderings reversed at every level. Both must
        // produce the same canonical bytes — which is the same guarantee a
        // signature relies on.
        let r = sample_receipt();
        let v_natural = serde_json::to_value(&r).unwrap();
        let v_shuffled = reverse_object_keys(v_natural.clone());

        // The two Values are NOT structurally equal as Rust types if the
        // backing maps preserve order; but their canonical JSON must be.
        let mut a = String::new();
        let mut b = String::new();
        super::write_canonical(&v_natural, &mut a);
        super::write_canonical(&v_shuffled, &mut b);
        assert_eq!(
            a, b,
            "canonical form must not depend on object-key insertion order"
        );
    }

    /// Walk a `Value`, returning a copy with every object's keys
    /// inserted in reverse order. With `serde_json`'s
    /// `preserve_order = off` (the default), maps are `BTreeMap` and the
    /// canonical form is naturally stable; with `preserve_order = on`,
    /// this exercise actually shuffles. Either way, the canonical
    /// writer's `keys.sort()` defends the property.
    fn reverse_object_keys(v: Value) -> Value {
        match v {
            Value::Array(items) => {
                Value::Array(items.into_iter().map(reverse_object_keys).collect())
            }
            Value::Object(map) => {
                let mut entries: Vec<_> = map.into_iter().collect();
                entries.reverse();
                let mut out = serde_json::Map::new();
                for (k, child) in entries {
                    out.insert(k, reverse_object_keys(child));
                }
                Value::Object(out)
            }
            other => other,
        }
    }
}
