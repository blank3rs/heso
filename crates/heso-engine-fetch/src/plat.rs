//! # plat
//!
//! Content-addressing for a **plat** — the static page-cartography
//! artifact heso produces. A plat's *identity* is the BLAKE3 hash of its
//! **canonical-JSON serialization**. Two heso runs that produce the same
//! plat content produce a byte-identical hash; anyone holding the plat
//! JSON can recompute the hash and verify it hasn't been tampered with.
//!
//! ## What the hash *names*
//!
//! `plat_hash` is a fingerprint of the **agent-observable surface** of a
//! page, not a byte-fingerprint of the response HTML. The canonicalizer
//! intentionally strips two classes of noise BEFORE hashing:
//!
//! 1. **Per-request entropy in attribute values.** Many sites
//!    (GitHub, Stripe, every modern SSR-with-hydration-IDs stack)
//!    inject server-generated UUIDs into element attributes like
//!    `id="icon-button-74b94e66-..."`, `aria-labelledby="tooltip-..."`,
//!    `aria-describedby="validation-..."`. Two consecutive `heso open`
//!    calls against such a page would otherwise produce different
//!    hashes for the same content. We drop a documented allowlist of
//!    *relational* attribute keys at every JSON-object level — see
//!    [`EPHEMERAL_OBJECT_KEYS`] — so the hash reflects what the agent
//!    *cares* about (tag, role, name, section, href, type, …) and not
//!    the cross-element pointers a server happens to mint per
//!    request.
//! 2. **Per-request session/state envelopes.** Fields like
//!    `inline_data`, `data_attrs` (hydration JSON blobs that often
//!    embed request IDs / sessions / build hashes), `console`,
//!    `cookies`, `scripts`, `lazy_hints`, `scroll`, `http_status`,
//!    the failure envelope (`partial`, `partial_reason`,
//!    `failed_scripts`, `console_errors_count`), and derived fields
//!    like `content_hash` / `delta` / `framework` / `forms` are
//!    pruned at every level so the hash is over the *agent-visible
//!    content surface*, not the per-run telemetry that rides
//!    alongside it.
//!
//! What stays in the hash: `url`, `title`, `description`, `tree`,
//! `actions` (with `attrs` filtered per item 1), `metadata`, `text`,
//! `linked_pages` (recursively hashed by the same rules).
//!
//! ## What the hash does NOT name
//!
//! - It is **not** a byte-fingerprint of the response HTML — see "What
//!   the hash names" above.
//! - It is **not** a network-replay anchor — that's [ADR 0008]'s
//!   recording mode (designed, not yet implemented).
//! - It is **not** a "who produced this plat" signature — that's
//!   `Receipt`'s Ed25519 territory ([ADR 0005]).
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
//! - **Ephemeral keys** ([`EPHEMERAL_OBJECT_KEYS`]) are stripped at every
//!   level before sorting (this implements the "agent-observable
//!   surface" definition above).
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
//! - Hashing identifies an *agent-observable content snapshot*. If the
//!   upstream page changes its visible content / heading structure /
//!   action graph (semantically — not just a noisy `id` attribute
//!   regenerating), the plat changes, and the hash changes — that's
//!   correct.
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

/// Keys stripped from every JSON object at every level before hashing.
///
/// Two categories live here:
///
/// 1. **The hash-of-the-hash sentinel** — `plat_hash`. Embedded in the
///    plat JSON; would create a chicken-and-egg if hashed.
/// 2. **Per-request entropy / per-session telemetry** that doesn't
///    contribute to the agent-observable surface. Two subgroups:
///
///    - Element-attribute-level *relational* keys whose values are
///      typically server-minted UUIDs cross-referencing other elements
///      in the page (`id`, `aria-labelledby`, `aria-describedby`,
///      `aria-controls`, `aria-owns`, `aria-activedescendant`,
///      `for`, `nonce`). Functional aria-* attributes that reflect
///      element STATE (`aria-checked`, `aria-disabled`, `aria-expanded`,
///      `aria-hidden`, `aria-required`, `aria-pressed`, `aria-selected`)
///      are intentionally NOT stripped — they describe content, not
///      cross-element pointers.
///    - Top-level / mid-tree blob fields that carry per-request
///      payloads (`inline_data`, `data_attrs`), per-session state
///      (`console`, `cookies`, `scripts`, `lazy_hints`, `scroll`,
///      `http_status`, `framework`), failure envelope (`partial`,
///      `partial_reason`, `failed_scripts`, `console_errors_count`),
///      or fields derived FROM other fields (`content_hash`, `delta`,
///      `forms`, `text` — derived from `tree` + post-hydration HTML).
///
/// Sorted alphabetically so a `binary_search` in [`is_ephemeral`] is
/// O(log n).
///
/// Keep in sync with the doc-comment on [`hash`] — both are
/// load-bearing on the "what plat_hash names" contract.
pub const EPHEMERAL_OBJECT_KEYS: &[&str] = &[
    // Relational element attributes (per-request UUIDs in dynamic SSR).
    "aria-activedescendant",
    "aria-controls",
    "aria-describedby",
    "aria-labelledby",
    "aria-owns",
    // Derived-from-other-fields metadata; would either create hash loops
    // or duplicate already-hashed content.
    "console",
    "console_errors_count",
    "content_hash",
    "cookies",
    // Server-rendered widget JSON; often embeds UUIDs / build hashes.
    "data_attrs",
    "delta",
    "failed_scripts",
    "for",
    "forms",
    "framework",
    "http_status",
    "id",
    // Hydration JSON; often embeds requestId / sessionId / build IDs.
    "inline_data",
    "lazy_hints",
    "nonce",
    "partial",
    "partial_reason",
    // The "hash of the content" — embedded in the JSON, must not feed
    // back into itself.
    "plat_hash",
    "scripts",
    "scroll",
    "text",
];

/// `true` iff `key` is a documented [`EPHEMERAL_OBJECT_KEYS`] entry —
/// dropped from the canonical form at every level.
fn is_ephemeral(key: &str) -> bool {
    EPHEMERAL_OBJECT_KEYS.binary_search(&key).is_ok()
}

/// Hex-encoded BLAKE3 of the canonical-JSON serialization of `value`,
/// with [`EPHEMERAL_OBJECT_KEYS`] (including `plat_hash`) stripped at
/// every level before hashing. 64 hex chars (256 bits).
///
/// The hash names the **agent-observable surface** of the plat — see
/// the module-level doc-comment for the full definition. In particular,
/// two `heso open` calls against the same page can produce the same
/// hash even if the server minted different per-request UUIDs into
/// `id` / `aria-labelledby` / `aria-describedby` attributes; that's
/// the point.
pub fn hash(value: &Value) -> String {
    // Stream the canonical bytes straight into the hasher — avoids
    // building the whole canonical String just to feed it to BLAKE3.
    // Output is identical to `blake3::hash(canonical_json(v).as_bytes())`.
    let mut hasher = HasherWriter(blake3::Hasher::new());
    write_canonical(value, &mut hasher);
    hasher.0.finalize().to_hex().to_string()
}

/// `fmt::Write` adapter that funnels written UTF-8 directly into a
/// BLAKE3 hasher without buffering. The canonical bytes are
/// ASCII-and-escaped-UTF-8, so emitting via `fmt::Write` is well-defined.
struct HasherWriter(blake3::Hasher);

impl std::fmt::Write for HasherWriter {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.update(s.as_bytes());
        Ok(())
    }
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

fn write_canonical<W: std::fmt::Write>(v: &Value, out: &mut W) {
    match v {
        Value::Null => {
            let _ = out.write_str("null");
        }
        Value::Bool(b) => {
            let _ = out.write_str(if *b { "true" } else { "false" });
        }
        Value::Number(n) => {
            // `serde_json::Number`'s `Display` is canonical for the
            // shapes the engine emits (no NaN, no Infinity).
            let _ = write!(out, "{n}");
        }
        Value::String(s) => {
            // Inline JSON string escape — avoids the `serde_json::to_string`
            // per-string allocation that the previous implementation paid.
            write_json_string(out, s);
        }
        Value::Array(arr) => {
            let _ = out.write_char('[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    let _ = out.write_char(',');
                }
                write_canonical(item, out);
            }
            let _ = out.write_char(']');
        }
        Value::Object(map) => {
            // Collect keys, dropping every `EPHEMERAL_OBJECT_KEYS` entry
            // at every level (per-request UUIDs in `id` /
            // `aria-labelledby`-style relational attrs, plus
            // session/state envelope fields like `inline_data` /
            // `console` / `cookies` / `partial` that don't contribute
            // to the agent-observable surface), then sort the
            // survivors lexicographically.
            let mut keys: Vec<&String> = map.keys().filter(|k| !is_ephemeral(k.as_str())).collect();
            keys.sort();
            let _ = out.write_char('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    let _ = out.write_char(',');
                }
                write_json_string(out, key);
                let _ = out.write_char(':');
                // SAFETY: `keys` came from `map.keys()`, so the lookup
                // can't fail.
                write_canonical(&map[*key], out);
            }
            let _ = out.write_char('}');
        }
    }
}

/// Emit `s` as a JSON-escaped string literal directly into `out`, no
/// intermediate `String` allocation. Same escape rules as
/// `serde_json::to_string(s)` for the value shapes plats carry
/// (ASCII control bytes escape to `\\u00XX`; `"` and `\\` are escaped;
/// `\n`, `\r`, `\t`, `\f`, `\b` get their short form; everything else
/// passes through verbatim).
fn write_json_string<W: std::fmt::Write>(out: &mut W, s: &str) {
    let _ = out.write_char('"');
    for c in s.chars() {
        match c {
            '"' => {
                let _ = out.write_str("\\\"");
            }
            '\\' => {
                let _ = out.write_str("\\\\");
            }
            '\n' => {
                let _ = out.write_str("\\n");
            }
            '\r' => {
                let _ = out.write_str("\\r");
            }
            '\t' => {
                let _ = out.write_str("\\t");
            }
            '\x08' => {
                let _ = out.write_str("\\b");
            }
            '\x0c' => {
                let _ = out.write_str("\\f");
            }
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                let _ = out.write_char(c);
            }
        }
    }
    let _ = out.write_char('"');
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

    // ========================================================================
    // bug-report 05-C: agent-observable hash stability against per-request
    // server-side UUIDs (GitHub-style CSP nonces, request IDs, build hashes
    // embedded in `id` / `aria-labelledby` / `aria-describedby` attributes).
    // ========================================================================

    #[test]
    fn hash_stable_when_only_ephemeral_relational_attrs_change() {
        // Identical agent-observable surface; only `id`,
        // `aria-labelledby`, `aria-describedby` carry per-request UUIDs.
        // These are the exact attribute keys agent 5 observed leaking
        // server-side entropy into `plat_hash` for github.com pages.
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
        assert_eq!(hash(&v1), hash(&v2));
    }

    #[test]
    fn hash_still_changes_when_meaningful_attrs_change() {
        // Non-ephemeral attrs (`href`, `type`, `name`, `value`) DO
        // contribute to the hash — content changes must still flip it.
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
    fn hash_drops_inline_data_and_data_attrs_top_level_blobs() {
        // SSR frameworks ship `__NEXT_DATA__` / `__NUXT_DATA__` /
        // `data-*` JSON payloads that frequently embed requestId /
        // sessionId / build hashes. Two plats with the same
        // agent-visible surface but different hydration payloads must
        // hash the same.
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
        assert_eq!(hash(&v1), hash(&v2));
    }

    #[test]
    fn hash_drops_per_session_envelope_fields() {
        // `cookies`, `console`, `scripts`, `partial`, `partial_reason`,
        // `failed_scripts`, `console_errors_count`, `lazy_hints`,
        // `scroll`, `http_status`, `framework`, `forms`,
        // `content_hash`, `delta`, `text` — all per-session telemetry
        // / derived fields that don't contribute to the
        // agent-observable surface.
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
        assert_eq!(hash(&v1), hash(&v2));
    }

    #[test]
    fn hash_preserves_functional_aria_state_attrs() {
        // `aria-disabled`, `aria-checked`, etc. reflect element STATE
        // (not relational pointers) — these are part of the
        // agent-observable surface and must contribute to the hash.
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

    #[test]
    fn ephemeral_object_keys_is_sorted_so_binary_search_works() {
        // `is_ephemeral` uses `binary_search`; the list MUST be sorted.
        // Guard against future maintenance accidentally inserting an
        // out-of-order entry that would silently break the filter for
        // entries after it.
        let mut copy: Vec<&str> = EPHEMERAL_OBJECT_KEYS.to_vec();
        copy.sort();
        assert_eq!(
            copy.as_slice(),
            EPHEMERAL_OBJECT_KEYS,
            "EPHEMERAL_OBJECT_KEYS must be sorted alphabetically"
        );
    }
}
