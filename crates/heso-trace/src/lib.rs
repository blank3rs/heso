//! # heso-trace
//!
//! Trace AST types + receipt + cost + content addressing.
//!
//! - [`Trace`], [`PrimitiveOp`], [`PrimitiveResult`] — re-exported from
//!   `heso-primitives`.
//! - [`Receipt`] — what every `heso.run` call returns under the hood. Records
//!   what was run, what came back, cost, and (M4+) a signature.
//! - [`Cost`] — bytes / cpu_ms / wall_ms / planner_tokens.
//! - [`Mode`] — `deterministic` (default) / `recording` / `live` per
//!   [ADR 0008].
//! - [`ContentHash`] — BLAKE3 hex digest, used for page-hash fingerprints.
//! - [`trace_hash`] — BLAKE3 over the canonical JSON of a [`Trace`]. Two
//!   equal traces produce the same hash byte-for-byte.
//!
//! **No engine dependency.** The execution that produces a [`Receipt`] lives
//! in `heso-trace-exec`. This crate is pure data so consumers (planners,
//! verifiers, downstream tools) can depend on it without dragging in Servo.
//!
//! [ADR 0008]: ../../decisions/0008-deterministic-execution.md

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use serde::{Deserialize, Serialize};

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
/// records the trace that was executed, what happened, the cost, and (in
/// M4+) a signature over a canonical encoding of all of the above.
///
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
    /// Ed25519 signature over a canonical encoding of all of the above.
    /// Empty until M4 lands the signing layer.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signed: String,
}

impl Receipt {
    /// `true` if the trace ran to completion (every op produced a result).
    pub fn is_ok(&self) -> bool {
        self.failed_at.is_none()
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

    #[test]
    fn mode_default_is_deterministic() {
        assert_eq!(Mode::default(), Mode::Deterministic);
    }

    #[test]
    fn mode_serializes_lowercase() {
        assert_eq!(serde_json::to_value(Mode::Deterministic).unwrap(), "deterministic");
        assert_eq!(serde_json::to_value(Mode::Recording).unwrap(), "recording");
        assert_eq!(serde_json::to_value(Mode::Live).unwrap(), "live");
    }

    #[test]
    fn content_hash_is_64_hex_chars_and_deterministic() {
        let a = ContentHash::of(b"some bytes");
        let b = ContentHash::of(b"some bytes");
        assert_eq!(a, b);
        assert_eq!(a.0.len(), 64);
        assert!(a.0.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
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
        let trace = url("https://example.com/");
        let receipt = Receipt {
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
            failed_at: Some(3),
            error: Some("oops".into()),
            signed: String::new(),
        };
        let json = serde_json::to_string(&receipt).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(receipt, back);
    }

    #[test]
    fn receipt_is_ok_when_no_failure() {
        let trace = url("https://example.com/");
        let r = Receipt {
            trace,
            results: vec![],
            pages_seen: vec![],
            trace_hash: String::new(),
            planner_id: String::new(),
            seed: 0,
            mode: Mode::Deterministic,
            cost: Cost::default(),
            failed_at: None,
            error: None,
            signed: String::new(),
        };
        assert!(r.is_ok());
    }

    #[test]
    fn receipt_is_not_ok_when_failed() {
        let trace = url("https://example.com/");
        let r = Receipt {
            trace,
            results: vec![],
            pages_seen: vec![],
            trace_hash: String::new(),
            planner_id: String::new(),
            seed: 0,
            mode: Mode::Deterministic,
            cost: Cost::default(),
            failed_at: Some(2),
            error: Some("err".into()),
            signed: String::new(),
        };
        assert!(!r.is_ok());
    }
}
