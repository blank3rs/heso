//! Detect which kind of HESO artifact a JSON value represents.
//!
//! Single source of truth for "what is this file?" used by the
//! polymorphic verbs (`heso verify`, `heso info`, `heso seal`,
//! `heso unseal`). The priority order below makes ambiguous shapes
//! resolve to the more specific kind first.

use serde_json::Value;

/// One of the five top-level HESO artifact shapes the CLI knows how to
/// inspect, verify, seal, or unseal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A bare plat object — carries `plat_hash` over its canonical bytes.
    Plat,
    /// An Ed25519-sealed plat envelope (`{alg, content, signature}`).
    SealedPlat,
    /// A signed (or unsigned) trace receipt — has `trace_hash`.
    Receipt,
    /// A keyless `(URL, actions)` fingerprint — has `trace_id` + `site_id`.
    ActionHash,
    /// An authoring template (`schema: "heso.template/v0"`).
    Template,
}

/// Error returned when the input is JSON but doesn't match any
/// recognized artifact shape.
#[derive(Debug)]
pub struct DetectError(pub String);

impl std::fmt::Display for DetectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for DetectError {}

/// Classify a top-level JSON object as a HESO artifact. The priority
/// order resolves ambiguous shapes; e.g. a receipt that also carries
/// `produced_plat_hash` matches Receipt first because `trace_hash` is
/// the receipt's primary signal.
///
/// Priority:
/// 1. `schema == "heso.template/v0"` → Template
/// 2. `trace_hash` present → Receipt
/// 3. `trace_id` AND `site_id` both present → ActionHash
/// 4. `plat_hash` present as a string → Plat
/// 5. `alg` AND `content` both present → SealedPlat
pub fn detect(value: &Value) -> Result<ArtifactKind, DetectError> {
    let obj = value
        .as_object()
        .ok_or_else(|| DetectError("artifact root is not a JSON object".to_owned()))?;

    if obj.get("schema").and_then(Value::as_str) == Some("heso.template/v0") {
        return Ok(ArtifactKind::Template);
    }
    if obj.contains_key("trace_hash") {
        return Ok(ArtifactKind::Receipt);
    }
    if obj.contains_key("trace_id") && obj.contains_key("site_id") {
        return Ok(ArtifactKind::ActionHash);
    }
    if obj.get("plat_hash").and_then(Value::as_str).is_some() {
        return Ok(ArtifactKind::Plat);
    }
    if obj.contains_key("alg") && obj.contains_key("content") {
        return Ok(ArtifactKind::SealedPlat);
    }
    Err(DetectError(
        "unrecognized artifact: expected a heso plat, sealed plat, \
         receipt, action-hash fingerprint, or template"
            .to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detect_minimal_plat() {
        let v = json!({
            "url": "https://example.com/",
            "plat_hash": "a".repeat(64),
        });
        assert_eq!(detect(&v).unwrap(), ArtifactKind::Plat);
    }

    #[test]
    fn detect_minimal_sealed_plat() {
        let v = json!({
            "alg": "heso-plat/v1+ed25519",
            "content": { "plat_hash": "a".repeat(64) },
            "signature": {
                "algorithm": "Ed25519",
                "public_key": "AAAA",
                "signature": "BBBB"
            }
        });
        assert_eq!(detect(&v).unwrap(), ArtifactKind::SealedPlat);
    }

    #[test]
    fn detect_minimal_receipt() {
        let v = json!({
            "trace": [],
            "results": [],
            "trace_hash": "a".repeat(64),
            "seed": 0,
            "mode": "deterministic",
            "cost": {"bytes": 0, "cpu_ms": 0, "wall_ms": 0, "planner_tokens": 0}
        });
        assert_eq!(detect(&v).unwrap(), ArtifactKind::Receipt);
    }

    #[test]
    fn detect_minimal_action_hash() {
        let v = json!({
            "algorithm": "heso-trace-fp/v1",
            "url": "https://example.com/",
            "actions": [],
            "site_id": "a".repeat(64),
            "action_ids": [],
            "trace_id": "b".repeat(64),
            "canonical": "[]"
        });
        assert_eq!(detect(&v).unwrap(), ArtifactKind::ActionHash);
    }

    #[test]
    fn detect_minimal_template() {
        let v = json!({
            "schema": "heso.template/v0",
            "id": "ca.heso.tests.minimal",
            "version": "0.1.0",
            "steps": []
        });
        assert_eq!(detect(&v).unwrap(), ArtifactKind::Template);
    }

    #[test]
    fn detect_arbitrary_object_errors() {
        let v = json!({ "hello": "world" });
        assert!(detect(&v).is_err());
    }

    #[test]
    fn detect_receipt_with_plat_hash_prioritizes_receipt() {
        let v = json!({
            "trace": [],
            "results": [],
            "trace_hash": "a".repeat(64),
            "produced_plat_hash": "b".repeat(64),
            "plat_hash": "c".repeat(64),
            "seed": 0,
            "mode": "deterministic",
            "cost": {"bytes": 0, "cpu_ms": 0, "wall_ms": 0, "planner_tokens": 0}
        });
        assert_eq!(detect(&v).unwrap(), ArtifactKind::Receipt);
    }

    #[test]
    fn detect_empty_object_errors() {
        let v = json!({});
        assert!(detect(&v).is_err());
    }

    #[test]
    fn detect_array_errors() {
        let v = json!([1, 2, 3]);
        let err = detect(&v).expect_err("arrays are not artifacts");
        assert!(err.to_string().contains("not a JSON object"));
    }
}
