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

use heso_core::{IdentityError, IdentityKey, Signature, SignaturePayload, Url};
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

/// RFC 3161 trusted-timestamp anchor. Conditionally present on a
/// [`Receipt`] after notarization. When present, participates in
/// [`canonical_receipt_bytes`] under [`SignatureScope::PostAnchor`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TsaAnchor {
    /// URL of the TSA that issued the token.
    pub tsa_url: String,
    /// Hash algorithm used for `message_imprint_hex`. Canonical value: `"SHA-256"`.
    pub hash_alg: String,
    /// Hex-encoded hash over `canonical_receipt_bytes(R, PreAnchor)`.
    pub message_imprint_hex: String,
    /// Base64-encoded RFC 3161 `TimeStampToken` (DER-encoded ASN.1).
    pub token_b64: String,
    /// `genTime` from the TSA response, ISO 8601 string.
    pub gen_time: String,
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Conditionally-present RFC 3161 timestamp anchor. See [`TsaAnchor`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tsa_anchor: Option<TsaAnchor>,
    /// 64-character lowercase hex BLAKE3 hash of the plat produced by the
    /// trace run that generated this receipt. Populated by stamp/run flows
    /// when a plat is emitted alongside the receipt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub produced_plat_hash: Option<String>,
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
    /// Dispatches the signature scope on `tsa_anchor` presence so the
    /// trait impl matches what [`sign_receipt`] and [`verify_receipt`]
    /// produce.
    fn signing_payload(&self) -> Vec<u8> {
        let scope = if self.tsa_anchor.is_some() {
            SignatureScope::PostAnchor
        } else {
            SignatureScope::PreAnchor
        };
        canonical_receipt_bytes(self, scope)
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
    // Stream serde_json's emitted bytes straight into BLAKE3 — avoids
    // building a `String` that's immediately thrown away. Output is
    // identical to `blake3::hash(serde_json::to_string(trace).as_bytes())`.
    struct BlakeWriter(blake3::Hasher);
    impl std::io::Write for BlakeWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.update(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut w = BlakeWriter(blake3::Hasher::new());
    serde_json::to_writer(&mut w, trace).expect("trace serializes");
    w.0.finalize().to_hex().to_string()
}

// ============================================================================
// Trace fingerprint — keyless, deterministic identity for (URL, actions)
// ============================================================================

/// Algorithm tag baked into every [`TraceFingerprint`]. Verifiers refuse
/// fingerprints with a different tag instead of silently re-hashing under
/// the wrong rules — so v2 can ship without breaking v1 receipts.
pub const FINGERPRINT_ALGO: &str = "heso-trace-fp/v1";

const DST_SITE: &str = "heso-trace-fp/v1/site";
const DST_ACTION: &str = "heso-trace-fp/v1/action";
const DST_TRACE_INIT: &str = "heso-trace-fp/v1/trace-init";
const DST_TRACE_STEP: &str = "heso-trace-fp/v1/trace-step";

/// One step of BLAKE3 with a domain-separator prefix.
///
/// The prefix is an ASCII string identifying *which* hash this is —
/// site, action, trace-init, trace-step. Different prefixes make it
/// impossible for, say, an action hash to accidentally collide with a
/// site hash even if they were fed the same bytes. The `\0` separator is
/// safe because canonical JSON never emits a raw NUL (it escapes as
/// ` `).
fn dst_hash(domain: &str, payload: &[u8]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(b"\0");
    hasher.update(payload);
    hasher.finalize()
}

/// A structured, tamper-evident identity for a `(URL, actions)` pair.
///
/// **No keys, no clocks, no per-user state.** Two callers anywhere — on
/// different machines, with no coordination — recomputing the
/// fingerprint over the same URL and the same action sequence get the
/// same `site_id`, the same `action_ids`, and the same `trace_id`. That
/// equality is the entire point: identity is derived from *what* was
/// intended, not from *who* asked for it or *when*.
///
/// ## Why three IDs instead of one
///
/// `trace_id` is the headline — one 64-hex string that names a complete
/// `(site, action_sequence)` intent. The structural pieces underneath
/// (`site_id`, `action_ids[]`) buy three concrete properties:
///
/// - **Tamper-evidence at every layer.** A consumer holding a saved
///   [`TraceFingerprint`] can recompute every ID from `url` + `actions`.
///   If *any* byte in `url`, `actions`, `site_id`, `action_ids[i]`,
///   `trace_id`, or `canonical` has been touched, the recompute won't
///   line up. Saved files are change-detectable end-to-end without a key.
/// - **Prefix verification.** The first `k` `action_ids` together with
///   `site_id` deterministically fix the chain state after step `k`.
///   So if you publish a prefix of a trace and the chain hash after it,
///   anyone can verify that prefix without seeing the rest of the trace.
/// - **Aggregation across traces on one site.** The same `site_id`
///   appears under every fingerprint produced for that URL — useful as a
///   cache / dedup key when an agent runs many traces against one page.
///
/// ## Saving and verifying
///
/// `Serialize` / `Deserialize` are derived. Write the fingerprint to a
/// file with `serde_json::to_writer_pretty`; verify later with
/// [`verify_fingerprint`]. The verify step needs nothing but the file —
/// no key, no network, no clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceFingerprint {
    /// Algorithm tag. Compared exactly on verify; future versions get a
    /// new tag (e.g. `"heso-trace-fp/v2"`) so v1 readers refuse to
    /// silently re-hash under the wrong rules.
    pub algorithm: String,
    /// The normalized URL that went into `site_id`. See
    /// [`normalize_url_for_hash`] for the exact rules.
    pub url: String,
    /// The action array, verbatim. Callers choose the schema — heso
    /// canonicalizes the JSON (sorted keys, compact) before hashing, so
    /// two callers using the same schema get matching `action_ids`.
    pub actions: Value,
    /// BLAKE3 over the normalized URL with the site domain separator.
    /// 64 hex chars.
    pub site_id: String,
    /// One BLAKE3 per action, in the action's original index order.
    /// Each is 64 hex chars; the `i`-th element is the hash of
    /// `actions[i]`'s canonical JSON with the action domain separator.
    pub action_ids: Vec<String>,
    /// Chain hash: starts from `site_id` under the trace-init domain,
    /// then folds each `action_ids[i]` in order under the trace-step
    /// domain. 64 hex chars. The headline identity.
    pub trace_id: String,
    /// The canonical-JSON form of `{actions, url}` that backs the
    /// fingerprint. Exposed so consumers can see *exactly* what got
    /// hashed without re-deriving the canonicalization rules.
    pub canonical: String,
}

/// Compute a [`TraceFingerprint`] for a `(URL, actions)` pair.
///
/// Deterministic and keyless: same inputs, same output, byte-for-byte,
/// on any machine. See [`TraceFingerprint`] for the algorithm details
/// and the meaning of each output field.
///
/// `actions` must be a JSON array; anything else is treated as an empty
/// trace (URL-only fingerprint). This is intentional — it lets callers
/// pass `serde_json::Value::Null` to mean "no actions" without a special
/// branch.
///
/// ```
/// use heso_trace::trace_fingerprint;
/// use heso_core::Url;
/// use serde_json::json;
///
/// let url = Url::parse("https://Example.com:443/foo?b=2&a=1#frag").unwrap();
/// let actions = json!([{"verb": "click", "ref": "@e3"}]);
/// let fp1 = trace_fingerprint(&url, &actions);
///
/// // Same intent, cosmetically different URL → same trace_id.
/// let same = Url::parse("https://example.com/foo?a=1&b=2").unwrap();
/// let fp2 = trace_fingerprint(&same, &actions);
/// assert_eq!(fp1.trace_id, fp2.trace_id);
/// assert_eq!(fp1.site_id, fp2.site_id);
/// assert_eq!(fp1.action_ids, fp2.action_ids);
/// assert_eq!(fp1.trace_id.len(), 64);
/// ```
pub fn trace_fingerprint(url: &Url, actions: &Value) -> TraceFingerprint {
    let normalized = normalize_url_for_hash(url);

    let site_hash = dst_hash(DST_SITE, normalized.as_bytes());

    let empty: Vec<Value> = Vec::new();
    let action_slice: &[Value] = actions.as_array().unwrap_or(&empty);

    let action_hashes: Vec<blake3::Hash> = action_slice
        .iter()
        .map(|a| {
            let mut canon = String::new();
            write_canonical(a, &mut canon);
            dst_hash(DST_ACTION, canon.as_bytes())
        })
        .collect();

    let mut state = dst_hash(DST_TRACE_INIT, site_hash.as_bytes());
    for a_hash in &action_hashes {
        let mut hasher = blake3::Hasher::new();
        hasher.update(DST_TRACE_STEP.as_bytes());
        hasher.update(b"\0");
        hasher.update(state.as_bytes());
        hasher.update(a_hash.as_bytes());
        state = hasher.finalize();
    }

    let mut canonical = String::new();
    let payload_val = serde_json::json!({
        "actions": actions,
        "url": normalized,
    });
    write_canonical(&payload_val, &mut canonical);

    TraceFingerprint {
        algorithm: FINGERPRINT_ALGO.to_owned(),
        url: normalized,
        actions: actions.clone(),
        site_id: site_hash.to_hex().to_string(),
        action_ids: action_hashes
            .iter()
            .map(|h| h.to_hex().to_string())
            .collect(),
        trace_id: state.to_hex().to_string(),
        canonical,
    }
}

/// Outcome of [`verify_fingerprint`]. Distinguishes the categories of
/// failure a caller might want to surface differently:
/// - `Valid` — every recomputed ID matches.
/// - `Mismatch` — at least one ID disagrees with the stored value. The
///   file was tampered with, or produced from different inputs.
/// - `WrongAlgorithm` — file claims a different algorithm tag. Don't
///   silently re-hash; the rules might have changed.
/// - `Malformed` — couldn't even parse what the file claims (e.g. `url`
///   isn't a URL). Almost certainly corruption, not tampering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FingerprintOutcome {
    /// Every recomputed component matches the stored fingerprint.
    Valid,
    /// At least one component recomputed differently.
    Mismatch,
    /// The fingerprint's algorithm tag isn't one this verifier knows.
    WrongAlgorithm(String),
    /// A structural field is unparseable; can't even attempt verification.
    Malformed(&'static str),
}

/// Recompute the fingerprint from `fp.url` and `fp.actions`, compare
/// against every stored ID, return the outcome.
///
/// **Keyless.** Anyone holding the saved fingerprint can run this — no
/// secret involved. Tamper-evidence comes from the algorithm being a
/// pure function of its inputs: a single byte changed anywhere in
/// `url` / `actions` / `site_id` / `action_ids[]` / `trace_id` /
/// `canonical` causes the recompute to disagree.
///
/// ```
/// use heso_trace::{trace_fingerprint, verify_fingerprint, FingerprintOutcome};
/// use heso_core::Url;
/// use serde_json::json;
///
/// let url = Url::parse("https://example.com/").unwrap();
/// let actions = json!([{"verb": "click", "ref": "@e3"}]);
/// let fp = trace_fingerprint(&url, &actions);
///
/// assert_eq!(verify_fingerprint(&fp), FingerprintOutcome::Valid);
///
/// // Tamper with the trace_id → mismatch.
/// let mut bad = fp.clone();
/// bad.trace_id = "0".repeat(64);
/// assert_eq!(verify_fingerprint(&bad), FingerprintOutcome::Mismatch);
/// ```
pub fn verify_fingerprint(fp: &TraceFingerprint) -> FingerprintOutcome {
    if fp.algorithm != FINGERPRINT_ALGO {
        return FingerprintOutcome::WrongAlgorithm(fp.algorithm.clone());
    }
    let url = match Url::parse(&fp.url) {
        Ok(u) => u,
        Err(_) => return FingerprintOutcome::Malformed("url is not parseable"),
    };
    let recomputed = trace_fingerprint(&url, &fp.actions);
    if recomputed.site_id == fp.site_id
        && recomputed.action_ids == fp.action_ids
        && recomputed.trace_id == fp.trace_id
        && recomputed.canonical == fp.canonical
        && recomputed.url == fp.url
    {
        FingerprintOutcome::Valid
    } else {
        FingerprintOutcome::Mismatch
    }
}

// ============================================================================
// Action vocabulary — the canonical schema for replayable traces
// ============================================================================

/// The canonical, replayable action vocabulary.
///
/// JSON shape: `{"verb": "<name>", ...rest}` — a tag-dispatched enum.
/// Fingerprints whose `actions` array conforms to this schema can be
/// re-executed by `heso replay`; fingerprints with arbitrary other JSON
/// shapes still hash fine but cannot be auto-replayed.
///
/// The four verbs map 1:1 to existing heso CLI verbs (`heso fetch` /
/// `heso click` / `heso fill` / `heso submit`), so a trace is exactly
/// the same intent the agent would produce by calling those.
///
/// ## Examples
///
/// ```
/// use heso_trace::Action;
/// use serde_json::json;
///
/// let open: Action = serde_json::from_value(
///     json!({"verb": "open", "url": "https://example.com/"})
/// ).unwrap();
/// match &open {
///     Action::Open { url } => assert_eq!(url, "https://example.com/"),
///     _ => panic!(),
/// }
///
/// let click: Action = serde_json::from_value(
///     json!({"verb": "click", "ref": "@e3"})
/// ).unwrap();
/// assert_eq!(click.verb(), "click");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "lowercase")]
pub enum Action {
    /// Navigate to a URL — equivalent to typing into the address bar.
    /// Fetches + parses + replaces the active page state.
    Open {
        /// Target URL string. Parsed at replay time so the canonical
        /// shape stays a plain string.
        url: String,
    },
    /// Click an element identified by its `@ref` in the current page's
    /// action graph. If the resolved element is an `<a href>`, the
    /// click follows the link (re-fetch the target URL); otherwise it
    /// dispatches a real `click` event through the JS engine.
    Click {
        /// Element ref like `@e3`. Resolved at replay time against the
        /// page that's currently active.
        #[serde(rename = "ref")]
        target: String,
    },
    /// Set the value of an `<input>` / `<textarea>` and dispatch
    /// `input` + `change` events, matching what a real browser fires
    /// when a user types.
    Fill {
        /// Element ref like `@e7`.
        #[serde(rename = "ref")]
        target: String,
        /// New `.value`. Round-trips through `JSON.stringify` so any
        /// Unicode / quoting works.
        value: String,
    },
    /// Click the first submit-typed control inside the form at `@ref`.
    /// Same shape as a click on a submit button; modeled separately
    /// because that's how planners think about it.
    Submit {
        /// Form ref like `@form1`.
        #[serde(rename = "ref")]
        target: String,
    },
}

impl Action {
    /// The wire-level `verb` field — the same string the JSON uses.
    /// Useful for replay logs, debugging, and switch-on-verb code that
    /// doesn't want to deconstruct the enum.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::Open { .. } => "open",
            Self::Click { .. } => "click",
            Self::Fill { .. } => "fill",
            Self::Submit { .. } => "submit",
        }
    }
}

/// Parse a fingerprint's `actions: Value` array into [`Action`]s.
///
/// Returns a list of canonical actions ready for replay. Errors point
/// to the specific index that failed and why — so a partially-bad trace
/// surfaces a clear message instead of a generic "deserialize failed."
///
/// ```
/// use heso_trace::{parse_actions, Action};
/// use serde_json::json;
///
/// let arr = json!([
///     {"verb": "open", "url": "https://example.com/"},
///     {"verb": "click", "ref": "@e3"},
/// ]);
/// let actions = parse_actions(&arr).unwrap();
/// assert_eq!(actions.len(), 2);
/// assert_eq!(actions[0].verb(), "open");
/// ```
pub fn parse_actions(actions: &Value) -> Result<Vec<Action>, ActionParseError> {
    let arr = actions.as_array().ok_or(ActionParseError::NotAnArray)?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let a: Action = serde_json::from_value(v.clone()).map_err(|e| ActionParseError::Item {
            index: i,
            source: e,
        })?;
        out.push(a);
    }
    Ok(out)
}

/// Failure modes for [`parse_actions`].
#[derive(Debug, thiserror::Error)]
pub enum ActionParseError {
    /// `actions` wasn't a JSON array at all.
    #[error("actions must be a JSON array")]
    NotAnArray,
    /// A specific action element didn't match the canonical schema.
    #[error("action[{index}] doesn't match the canonical schema: {source}")]
    Item {
        /// 0-based index into the actions array.
        index: usize,
        /// The underlying deserialize error.
        #[source]
        source: serde_json::Error,
    },
}

/// URL normalization used by [`trace_fingerprint`]. Public so tooling that
/// wants to display "the URL we hashed" can show the same string the
/// algorithm saw, without re-deriving the rules.
pub fn normalize_url_for_hash(url: &Url) -> String {
    let mut u = url.clone();
    u.set_fragment(None);
    if let Some(explicit) = u.port() {
        let scheme_default = match u.scheme() {
            "http" | "ws" => Some(80),
            "https" | "wss" => Some(443),
            "ftp" => Some(21),
            _ => None,
        };
        if scheme_default == Some(explicit) {
            let _ = u.set_port(None);
        }
    }
    if u.query().is_some() {
        let mut pairs: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        pairs.sort();
        {
            let mut qp = u.query_pairs_mut();
            qp.clear();
            for (k, v) in &pairs {
                qp.append_pair(k, v);
            }
        }
        if u.query() == Some("") {
            u.set_query(None);
        }
    }
    u.to_string()
}

// ============================================================================
// Canonical-JSON for signing
// ============================================================================

/// Which canonical-byte view of a [`Receipt`] is being requested.
///
/// `PreAnchor` clears both `signature` AND `tsa_anchor` to JSON null —
/// used for the TSA `messageImprint` input and for the initial signature
/// in the default (re-sign) notarize flow.
///
/// `PostAnchor` clears only `signature` to JSON null; `tsa_anchor` is
/// preserved. Used for the re-signed signature after notarize attaches
/// the anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureScope {
    /// Both `signature` and `tsa_anchor` cleared.
    PreAnchor,
    /// Only `signature` cleared; `tsa_anchor` preserved.
    PostAnchor,
}

/// Canonical bytes of a [`Receipt`] under the requested signature scope.
/// The signature field is always cleared to JSON null; the anchor field
/// is cleared only under [`SignatureScope::PreAnchor`].
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
///
/// This is a subset of RFC 8785 (JSON Canonicalization Scheme) sufficient
/// for the value shapes a receipt emits.
///
/// [`plat`]: ../heso_engine_fetch/plat/index.html
pub fn canonical_receipt_bytes(receipt: &Receipt, scope: SignatureScope) -> Vec<u8> {
    let mut v = serde_json::to_value(receipt).expect("receipt serializes");
    if let Some(obj) = v.as_object_mut() {
        obj.insert("signature".to_owned(), Value::Null);
        if scope == SignatureScope::PreAnchor {
            obj.insert("tsa_anchor".to_owned(), Value::Null);
        }
    }
    let mut out = String::new();
    write_canonical(&v, &mut out);
    out.into_bytes()
}

/// Canonical-JSON form of a receipt with `signature` cleared, suitable as
/// the byte input to Ed25519 signing/verifying.
///
/// Thin wrapper over [`canonical_receipt_bytes`] under
/// [`SignatureScope::PreAnchor`] — receipts without a `tsa_anchor` produce
/// the same bytes either way, preserving back-compat with pre-anchor
/// receipts.
pub fn canonical_receipt_json(receipt: &Receipt) -> String {
    String::from_utf8(canonical_receipt_bytes(receipt, SignatureScope::PreAnchor))
        .expect("canonical JSON is valid UTF-8")
}

fn write_canonical<W: std::fmt::Write>(v: &Value, out: &mut W) {
    match v {
        Value::Null => {
            let _ = out.write_str("null");
        }
        Value::Bool(b) => {
            let _ = out.write_str(if *b { "true" } else { "false" });
        }
        Value::Number(n) => {
            let _ = write!(out, "{n}");
        }
        Value::String(s) => {
            // Inline escape (no per-string allocation). Same output as
            // `serde_json::to_string(s)` for the value shapes receipts
            // carry.
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
            let mut keys: Vec<&String> = map.keys().collect();
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
// Signing / verifying
// ============================================================================

/// Sign `receipt` with `key`. Mutates the receipt in place: when this
/// returns, `receipt.signature` is `Some(sig)`.
///
/// Any pre-existing signature is discarded before the new one is
/// computed (the canonical form clears it anyway, so this just keeps the
/// in-memory struct consistent with what was signed).
///
/// The signature scope tracks `tsa_anchor` presence — receipts with an
/// attached anchor sign under [`SignatureScope::PostAnchor`] so the
/// anchor itself is covered; receipts without sign under
/// [`SignatureScope::PreAnchor`]. [`verify_receipt`] mirrors this
/// dispatch so the bytes match on both sides.
pub fn sign_receipt(key: &IdentityKey, receipt: &mut Receipt) {
    receipt.signature = None;
    let scope = if receipt.tsa_anchor.is_some() {
        SignatureScope::PostAnchor
    } else {
        SignatureScope::PreAnchor
    };
    let payload = canonical_receipt_bytes(receipt, scope);
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
    let scope = if receipt.tsa_anchor.is_some() {
        SignatureScope::PostAnchor
    } else {
        SignatureScope::PreAnchor
    };
    let payload = canonical_receipt_bytes(receipt, scope);
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
            pages_seen: vec![ContentHash::of(b"page")],
            trace_hash: trace_hash(&trace),
            planner_id: "planner-v0".into(),
            seed: 42,
            cost: Cost {
                bytes: 1024,
                cpu_ms: 5,
                wall_ms: 200,
                planner_tokens: 0,
            },
            ..Default::default()
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

    fn parse_url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn fingerprint_is_stable_byte_for_byte() {
        let u = parse_url("https://example.com/foo");
        let actions = serde_json::json!([{"verb": "click", "ref": "@e3"}]);
        let a = trace_fingerprint(&u, &actions);
        let b = trace_fingerprint(&u, &actions);
        assert_eq!(a, b);
        assert_eq!(a.algorithm, FINGERPRINT_ALGO);
        assert_eq!(a.site_id.len(), 64);
        assert_eq!(a.trace_id.len(), 64);
        assert_eq!(a.action_ids.len(), 1);
        assert_eq!(a.action_ids[0].len(), 64);
    }

    #[test]
    fn fingerprint_normalizes_cosmetic_url_differences() {
        let actions = serde_json::json!([]);
        // Default port, mixed case host, query reorder, fragment — all
        // should fold to the same canonical URL → same fingerprint.
        let a = parse_url("https://Example.com:443/path?b=2&a=1#frag");
        let b = parse_url("https://example.com/path?a=1&b=2");
        let fa = trace_fingerprint(&a, &actions);
        let fb = trace_fingerprint(&b, &actions);
        assert_eq!(fa.site_id, fb.site_id);
        assert_eq!(fa.trace_id, fb.trace_id);
    }

    #[test]
    fn fingerprint_distinguishes_distinct_intents() {
        let u = parse_url("https://example.com/");
        let click = serde_json::json!([{"verb": "click", "ref": "@e3"}]);
        let fill = serde_json::json!([{"verb": "fill", "ref": "@e3", "value": "hi"}]);
        assert_ne!(
            trace_fingerprint(&u, &click).trace_id,
            trace_fingerprint(&u, &fill).trace_id,
        );
    }

    #[test]
    fn fingerprint_same_actions_different_order_diverge() {
        let u = parse_url("https://example.com/");
        let ab = serde_json::json!([
            {"verb": "click", "ref": "@e1"},
            {"verb": "click", "ref": "@e2"},
        ]);
        let ba = serde_json::json!([
            {"verb": "click", "ref": "@e2"},
            {"verb": "click", "ref": "@e1"},
        ]);
        let fa = trace_fingerprint(&u, &ab);
        let fb = trace_fingerprint(&u, &ba);
        // Same action_ids exist on both, just in different order.
        let mut a_ids = fa.action_ids.clone();
        let mut b_ids = fb.action_ids.clone();
        a_ids.sort();
        b_ids.sort();
        assert_eq!(a_ids, b_ids, "the same two action_ids appear in both");
        // But the chained trace_id MUST differ — order is intent.
        assert_ne!(fa.trace_id, fb.trace_id);
    }

    #[test]
    fn fingerprint_url_only_when_actions_empty() {
        let u = parse_url("https://example.com/");
        let empty = serde_json::json!([]);
        let fp = trace_fingerprint(&u, &empty);
        assert_eq!(fp.action_ids.len(), 0);
        assert_eq!(
            fp.canonical,
            r#"{"actions":[],"url":"https://example.com/"}"#,
        );
    }

    #[test]
    fn fingerprint_action_key_order_does_not_matter() {
        let u = parse_url("https://example.com/");
        // Same logical action, keys in different order.
        let a = serde_json::json!([{"verb": "fill", "ref": "@e7", "value": "hi"}]);
        let b = serde_json::json!([{"value": "hi", "ref": "@e7", "verb": "fill"}]);
        assert_eq!(
            trace_fingerprint(&u, &a).trace_id,
            trace_fingerprint(&u, &b).trace_id,
        );
    }

    #[test]
    fn fingerprint_site_id_is_independent_of_actions() {
        let u = parse_url("https://example.com/");
        let click = serde_json::json!([{"verb": "click", "ref": "@e1"}]);
        let fill = serde_json::json!([{"verb": "fill", "ref": "@e1", "value": "x"}]);
        // Different actions, same URL → same site_id, different trace_id.
        let a = trace_fingerprint(&u, &click);
        let b = trace_fingerprint(&u, &fill);
        assert_eq!(a.site_id, b.site_id);
        assert_ne!(a.trace_id, b.trace_id);
    }

    #[test]
    fn fingerprint_verify_accepts_a_fresh_fingerprint() {
        let u = parse_url("https://example.com/foo");
        let actions = serde_json::json!([{"verb": "click", "ref": "@e3"}]);
        let fp = trace_fingerprint(&u, &actions);
        assert_eq!(verify_fingerprint(&fp), FingerprintOutcome::Valid);
    }

    #[test]
    fn fingerprint_verify_detects_tampered_trace_id() {
        let u = parse_url("https://example.com/");
        let actions = serde_json::json!([]);
        let mut fp = trace_fingerprint(&u, &actions);
        fp.trace_id = "0".repeat(64);
        assert_eq!(verify_fingerprint(&fp), FingerprintOutcome::Mismatch);
    }

    #[test]
    fn fingerprint_verify_detects_tampered_url() {
        let u = parse_url("https://example.com/");
        let actions = serde_json::json!([]);
        let mut fp = trace_fingerprint(&u, &actions);
        // Change url to a different (but parseable) site; IDs were computed
        // for the original, so recompute disagrees.
        fp.url = "https://other.example/".to_owned();
        assert_eq!(verify_fingerprint(&fp), FingerprintOutcome::Mismatch);
    }

    #[test]
    fn fingerprint_verify_detects_tampered_action() {
        let u = parse_url("https://example.com/");
        let actions = serde_json::json!([{"verb": "click", "ref": "@e3"}]);
        let mut fp = trace_fingerprint(&u, &actions);
        // Mutate the actions array on the saved fp; IDs become inconsistent.
        fp.actions = serde_json::json!([{"verb": "click", "ref": "@e99"}]);
        assert_eq!(verify_fingerprint(&fp), FingerprintOutcome::Mismatch);
    }

    #[test]
    fn fingerprint_verify_refuses_unknown_algorithm_tag() {
        let u = parse_url("https://example.com/");
        let actions = serde_json::json!([]);
        let mut fp = trace_fingerprint(&u, &actions);
        fp.algorithm = "heso-trace-fp/v999".into();
        match verify_fingerprint(&fp) {
            FingerprintOutcome::WrongAlgorithm(s) => assert_eq!(s, "heso-trace-fp/v999"),
            other => panic!("expected WrongAlgorithm, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_verify_flags_malformed_url() {
        let u = parse_url("https://example.com/");
        let actions = serde_json::json!([]);
        let mut fp = trace_fingerprint(&u, &actions);
        fp.url = "not a url".into();
        match verify_fingerprint(&fp) {
            FingerprintOutcome::Malformed(_) => {}
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_json_roundtrips_intact() {
        let u = parse_url("https://example.com/foo");
        let actions = serde_json::json!([{"verb": "fill", "ref": "@e7", "value": "hi"}]);
        let fp = trace_fingerprint(&u, &actions);
        let json = serde_json::to_string(&fp).unwrap();
        let back: TraceFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(fp, back);
        assert_eq!(verify_fingerprint(&back), FingerprintOutcome::Valid);
    }

    #[test]
    fn fingerprint_domain_separators_prevent_collision() {
        // Two payloads that happen to canonicalize to the same string
        // under different domains MUST hash to different values. A site
        // URL "foo" and an action {"foo": null} obviously don't collide
        // canonically, so we use a more direct test: derive a hash under
        // each domain over the same bytes and assert all three differ.
        let bytes = b"identical payload";
        let site = dst_hash(DST_SITE, bytes);
        let action = dst_hash(DST_ACTION, bytes);
        let init = dst_hash(DST_TRACE_INIT, bytes);
        let step = dst_hash(DST_TRACE_STEP, bytes);
        assert_ne!(site.as_bytes(), action.as_bytes());
        assert_ne!(site.as_bytes(), init.as_bytes());
        assert_ne!(site.as_bytes(), step.as_bytes());
        assert_ne!(action.as_bytes(), init.as_bytes());
        assert_ne!(action.as_bytes(), step.as_bytes());
        assert_ne!(init.as_bytes(), step.as_bytes());
    }

    #[test]
    fn action_open_serializes_with_verb_and_url() {
        let a = Action::Open {
            url: "https://example.com/".into(),
        };
        let j = serde_json::to_value(&a).unwrap();
        assert_eq!(j["verb"], "open");
        assert_eq!(j["url"], "https://example.com/");
        let back: Action = serde_json::from_value(j).unwrap();
        assert_eq!(back, a);
    }

    #[test]
    fn action_click_serializes_with_ref_field_not_target() {
        let a = Action::Click {
            target: "@e3".into(),
        };
        let j = serde_json::to_value(&a).unwrap();
        // The JSON uses `ref` (matches the rest of heso's shell vocab),
        // not the Rust field name `target`.
        assert_eq!(j["verb"], "click");
        assert_eq!(j["ref"], "@e3");
        assert!(j.get("target").is_none(), "rust field name must not leak");
    }

    #[test]
    fn action_fill_carries_value() {
        let a = Action::Fill {
            target: "@e7".into(),
            value: "hello world".into(),
        };
        let j = serde_json::to_value(&a).unwrap();
        assert_eq!(j["verb"], "fill");
        assert_eq!(j["ref"], "@e7");
        assert_eq!(j["value"], "hello world");
    }

    #[test]
    fn parse_actions_accepts_canonical_array() {
        let arr = serde_json::json!([
            {"verb": "open", "url": "https://example.com/"},
            {"verb": "click", "ref": "@e3"},
            {"verb": "fill", "ref": "@e7", "value": "x"},
            {"verb": "submit", "ref": "@form1"},
        ]);
        let actions = parse_actions(&arr).unwrap();
        assert_eq!(actions.len(), 4);
        assert_eq!(actions[0].verb(), "open");
        assert_eq!(actions[1].verb(), "click");
        assert_eq!(actions[2].verb(), "fill");
        assert_eq!(actions[3].verb(), "submit");
    }

    #[test]
    fn parse_actions_rejects_non_array() {
        let v = serde_json::json!({"verb": "click", "ref": "@e3"});
        match parse_actions(&v) {
            Err(ActionParseError::NotAnArray) => {}
            other => panic!("expected NotAnArray, got {other:?}"),
        }
    }

    #[test]
    fn parse_actions_points_to_failing_index() {
        let arr = serde_json::json!([
            {"verb": "open", "url": "https://example.com/"},
            {"verb": "fly", "ref": "@e3"}, // unknown verb
        ]);
        match parse_actions(&arr) {
            Err(ActionParseError::Item { index, .. }) => assert_eq!(index, 1),
            other => panic!("expected Item, got {other:?}"),
        }
    }

    #[test]
    fn parse_actions_rejects_fill_without_value() {
        let arr = serde_json::json!([{"verb": "fill", "ref": "@e7"}]);
        match parse_actions(&arr) {
            Err(ActionParseError::Item { index, .. }) => assert_eq!(index, 0),
            other => panic!("expected Item, got {other:?}"),
        }
    }

    #[test]
    fn fingerprint_round_trips_through_canonical_action_schema() {
        // Build a fingerprint from Action enum values; verify it hashes
        // identically to one built from equivalent raw JSON. The whole
        // point of the canonical schema is that *the schema is the
        // fingerprint*: replay and hash agree on what an action is.
        let u = parse_url("https://example.com/");
        let typed = vec![
            Action::Open {
                url: "https://example.com/".into(),
            },
            Action::Click {
                target: "@e3".into(),
            },
        ];
        let typed_json = serde_json::to_value(&typed).unwrap();
        let raw_json = serde_json::json!([
            {"verb": "open", "url": "https://example.com/"},
            {"verb": "click", "ref": "@e3"},
        ]);
        assert_eq!(
            trace_fingerprint(&u, &typed_json).trace_id,
            trace_fingerprint(&u, &raw_json).trace_id,
        );
    }

    #[test]
    fn normalize_url_strips_default_https_port() {
        let u = parse_url("https://example.com:443/path");
        assert_eq!(normalize_url_for_hash(&u), "https://example.com/path");
    }

    #[test]
    fn normalize_url_keeps_nondefault_port() {
        let u = parse_url("https://example.com:8443/path");
        assert_eq!(normalize_url_for_hash(&u), "https://example.com:8443/path");
    }

    #[test]
    fn normalize_url_drops_fragment() {
        let u = parse_url("https://example.com/foo#section");
        assert_eq!(normalize_url_for_hash(&u), "https://example.com/foo");
    }

    #[test]
    fn normalize_url_sorts_query_params() {
        let u = parse_url("https://example.com/?z=9&a=1&m=5");
        assert_eq!(
            normalize_url_for_hash(&u),
            "https://example.com/?a=1&m=5&z=9",
        );
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
        // them in lexicographic order.
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
            "\"tsa_anchor\"",
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

    // ------- tsa_anchor + produced_plat_hash -------

    fn sample_anchor() -> TsaAnchor {
        TsaAnchor {
            tsa_url: "https://tsa.example/timestamp".into(),
            hash_alg: "SHA-256".into(),
            message_imprint_hex: "a".repeat(64),
            token_b64: "AAAA".into(),
            gen_time: "2026-05-27T00:00:00Z".into(),
        }
    }

    #[test]
    fn tsa_anchor_serde_roundtrip() {
        let a = sample_anchor();
        let json = serde_json::to_string(&a).unwrap();
        let back: TsaAnchor = serde_json::from_str(&json).unwrap();
        assert_eq!(a, back);
    }

    #[test]
    fn receipt_with_tsa_anchor_serde_roundtrip() {
        let mut r = sample_receipt();
        r.tsa_anchor = Some(sample_anchor());
        let json = serde_json::to_string(&r).unwrap();
        let back: Receipt = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn receipt_omits_tsa_anchor_when_none() {
        let r = sample_receipt();
        let s = serde_json::to_string(&r).unwrap();
        assert!(
            !s.contains("\"tsa_anchor\""),
            "unanchored receipt JSON must omit tsa_anchor: {s}"
        );
    }

    #[test]
    fn receipt_omits_produced_plat_hash_when_none() {
        let r = sample_receipt();
        let s = serde_json::to_string(&r).unwrap();
        assert!(
            !s.contains("\"produced_plat_hash\""),
            "receipt JSON without plat must omit produced_plat_hash: {s}"
        );
    }

    #[test]
    fn canonical_bytes_preanchor_clears_anchor() {
        let mut r = sample_receipt();
        r.tsa_anchor = Some(sample_anchor());
        let bytes = canonical_receipt_bytes(&r, SignatureScope::PreAnchor);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.contains("\"tsa_anchor\":null"),
            "PreAnchor must null out tsa_anchor: {s}"
        );
    }

    #[test]
    fn canonical_bytes_postanchor_preserves_anchor() {
        let mut r = sample_receipt();
        r.tsa_anchor = Some(sample_anchor());
        let bytes = canonical_receipt_bytes(&r, SignatureScope::PostAnchor);
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.contains("\"tsa_url\":\"https://tsa.example/timestamp\""),
            "PostAnchor must keep the populated anchor: {s}"
        );
        assert!(
            s.contains("\"message_imprint_hex\""),
            "PostAnchor must keep anchor inner fields: {s}"
        );
        assert!(
            !s.contains("\"tsa_anchor\":null"),
            "PostAnchor must not null out tsa_anchor: {s}"
        );
    }

    #[test]
    fn canonical_bytes_both_scopes_clear_signature() {
        let key = IdentityKey::generate();
        let mut r = sample_receipt();
        r.tsa_anchor = Some(sample_anchor());
        sign_receipt(&key, &mut r);
        assert!(r.signature.is_some());
        for scope in [SignatureScope::PreAnchor, SignatureScope::PostAnchor] {
            let bytes = canonical_receipt_bytes(&r, scope);
            let s = std::str::from_utf8(&bytes).unwrap();
            assert!(
                s.contains("\"signature\":null"),
                "scope {scope:?} must null out signature: {s}"
            );
        }
    }

    #[test]
    fn produced_plat_hash_participates_in_signing() {
        let key = IdentityKey::generate();
        let mut r = sample_receipt();
        r.produced_plat_hash = Some("a".repeat(64));
        sign_receipt(&key, &mut r);
        match verify_receipt(&r) {
            VerifyOutcome::Valid => {}
            other => panic!("expected Valid pre-mutation, got {other:?}"),
        }
        r.produced_plat_hash = Some("b".repeat(64));
        match verify_receipt(&r) {
            VerifyOutcome::Invalid(_) => {}
            other => panic!("expected Invalid after mutating plat hash, got {other:?}"),
        }
    }

    #[test]
    fn sign_then_verify_roundtrip_with_anchor_uses_post_anchor_scope() {
        // When `tsa_anchor` is populated, both sign and verify must
        // dispatch to PostAnchor so the canonical bytes line up. The
        // signed payload covers the anchor itself — mutating any field
        // inside it after signing must invalidate the signature.
        let key = IdentityKey::generate();
        let mut r = sample_receipt();
        r.tsa_anchor = Some(sample_anchor());
        sign_receipt(&key, &mut r);
        match verify_receipt(&r) {
            VerifyOutcome::Valid => {}
            other => panic!("expected Valid for anchored receipt, got {other:?}"),
        }

        // Mutate a field inside the anchor — verify must reject.
        if let Some(anchor) = r.tsa_anchor.as_mut() {
            anchor.gen_time = "2099-01-01T00:00:00Z".into();
        }
        match verify_receipt(&r) {
            VerifyOutcome::Invalid(_) => {}
            other => panic!("expected Invalid after mutating anchor gen_time, got {other:?}"),
        }
    }

    #[test]
    fn anchored_signature_does_not_verify_under_preanchor_bytes() {
        // Belt-and-suspenders: when an anchored receipt is signed under
        // PostAnchor, recomputing the bytes under PreAnchor (which nulls
        // out tsa_anchor) MUST yield bytes that fail signature
        // verification — confirming the scope choice in verify_receipt
        // is load-bearing, not cosmetic.
        let key = IdentityKey::generate();
        let mut r = sample_receipt();
        r.tsa_anchor = Some(sample_anchor());
        sign_receipt(&key, &mut r);
        let sig = r.signature.as_ref().expect("just signed").clone();
        let pre_bytes = canonical_receipt_bytes(&r, SignatureScope::PreAnchor);
        assert!(
            sig.verify(&pre_bytes).is_err(),
            "PostAnchor signature must not verify against PreAnchor bytes",
        );
    }
}
