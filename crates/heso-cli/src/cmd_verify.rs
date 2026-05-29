//! Polymorphic `heso verify <file>`.
//!
//! Sniffs the artifact kind and dispatches to the right verifier:
//! plat, sealed plat, receipt, action-hash fingerprint, or template.
//! Exit codes follow the per-type conventions the dedicated verbs
//! already use, so a script that swaps `plat-verify` for `verify` sees
//! the same exit shape.
//!
//! ## The trust layer (signed plats / sealed plats)
//!
//! A content hash alone is not tamper-evident against a recomputing
//! forger — anyone who edits the body recomputes the hash and the
//! integrity check passes. A signature changes that, but a *self-signed*
//! signature only buys INTEGRITY ("these bytes are unchanged since
//! signing"), not AUTHENTICITY ("the *right* key signed them"): a forger
//! re-signs with their own fresh key and the signature is internally
//! valid. Authenticity exists only relative to a key the verifier already
//! trusts.
//!
//! So `verify` never prints a bare `OK` for a signed artifact. It always
//! prints a signer fingerprint and a trust-state, resolved by this
//! precedence (strongest first):
//!
//! 1. `--signer-key <path>` — the plat's pubkey MUST equal the supplied
//!    key. Fully defeats forgery.
//! 2. `--expect-signer <fp>` — the fingerprint MUST equal the supplied
//!    one. Fully defeats forgery.
//! 3. `--trusted-keys` / `HESO_TRUSTED_KEYS` allowlist — the pubkey MUST
//!    be on the list (empty supplied list = fail-closed).
//! 4. TOFU — first contact for a `lineage` pins the signer (`first-use`);
//!    a later mismatch fails loud unless `--accept-new-signer` re-pins.

use std::path::PathBuf;
use std::process::ExitCode;

use heso_trace::{verify_fingerprint, verify_receipt, FingerprintOutcome, TraceFingerprint};

use crate::artifact_sniffer::{detect, ArtifactKind};
use crate::tofu::PinStore;
use crate::{receipts, template, DEFAULT_KNOWN_SIGNERS_PATH};

struct VerifyArgs {
    file: String,
    trusted_keys: Option<PathBuf>,
    require_tsa: bool,
    tsa_trusted_roots: Option<PathBuf>,
    expect_signer: Option<String>,
    signer_key: Option<PathBuf>,
    known_signers: Option<PathBuf>,
    accept_new_signer: bool,
}

fn parse_args(args: &[String]) -> Result<VerifyArgs, ExitCode> {
    let mut file: Option<String> = None;
    let mut trusted_keys: Option<PathBuf> = None;
    let mut require_tsa = false;
    let mut tsa_trusted_roots: Option<PathBuf> = None;
    let mut expect_signer: Option<String> = None;
    let mut signer_key: Option<PathBuf> = None;
    let mut known_signers: Option<PathBuf> = None;
    let mut accept_new_signer = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                return Err(ExitCode::SUCCESS);
            }
            "--trusted-keys" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--trusted-keys needs a value (path to JSON allowlist)");
                    return Err(ExitCode::from(2));
                };
                trusted_keys = Some(PathBuf::from(v));
                i += 2;
            }
            "--require-tsa" => {
                require_tsa = true;
                i += 1;
            }
            "--tsa-trusted-roots" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--tsa-trusted-roots needs a value (path to PEM)");
                    return Err(ExitCode::from(2));
                };
                tsa_trusted_roots = Some(PathBuf::from(v));
                i += 2;
            }
            "--expect-signer" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--expect-signer needs a value (a `heso:<fp>` fingerprint)");
                    return Err(ExitCode::from(2));
                };
                expect_signer = Some(v.clone());
                i += 2;
            }
            "--signer-key" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--signer-key needs a value (path to a base64 Ed25519 public key)");
                    return Err(ExitCode::from(2));
                };
                signer_key = Some(PathBuf::from(v));
                i += 2;
            }
            "--known-signers" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--known-signers needs a value (path to the TOFU pin store)");
                    return Err(ExitCode::from(2));
                };
                known_signers = Some(PathBuf::from(v));
                i += 2;
            }
            "--accept-new-signer" => {
                accept_new_signer = true;
                i += 1;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag `{other}`");
                print_usage();
                return Err(ExitCode::from(2));
            }
            _ => {
                if file.is_some() {
                    eprintln!("unexpected extra argument `{}`; pass a single <file>", args[i]);
                    return Err(ExitCode::from(2));
                }
                file = Some(args[i].clone());
                i += 1;
            }
        }
    }
    let Some(file) = file else {
        print_usage();
        return Err(ExitCode::from(2));
    };
    Ok(VerifyArgs {
        file,
        trusted_keys,
        require_tsa,
        tsa_trusted_roots,
        expect_signer,
        signer_key,
        known_signers,
        accept_new_signer,
    })
}

fn print_usage() {
    eprintln!(
        "usage: heso verify [--trusted-keys PATH] [--expect-signer FP] [--signer-key PATH]\n\
        \x20                  [--known-signers PATH] [--accept-new-signer]\n\
        \x20                  [--require-tsa] [--tsa-trusted-roots PATH] <file>"
    );
    eprintln!();
    eprintln!("Polymorphic verification: detects whether <file> is a plat, sealed plat,");
    eprintln!("receipt, action-hash fingerprint, or template and runs the right check.");
}

/// `heso verify <file>` — dispatch to the right verifier based on the
/// artifact kind detected in `<file>`'s JSON shape.
pub async fn cmd_verify(args: &[String]) -> ExitCode {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let (contents, _source) = match crate::read_plat_input_with_source(&parsed.file) {
        Ok(pair) => pair,
        Err(code) => return code,
    };
    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("`{}` is not valid JSON: {e}", parsed.file);
            return ExitCode::from(2);
        }
    };
    let kind = match detect(&value) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };

    match kind {
        ArtifactKind::Plat => verify_plat(&value, &parsed),
        ArtifactKind::SealedPlat => verify_sealed_plat(&value, &parsed),
        ArtifactKind::Receipt => {
            verify_receipt_inner(&value, &parsed.trusted_keys, parsed.require_tsa, parsed.tsa_trusted_roots.as_ref())
        }
        ArtifactKind::ActionHash => verify_action_hash(&value),
        ArtifactKind::Template => verify_template(&contents),
    }
}

// ============================================================================
// Trust resolution (shared by the plat and sealed-plat branches)
// ============================================================================

/// How an explicit trust source resolved a `Valid` signature. The TOFU
/// outcomes carry the line suffix; the explicit ones share `(trusted)`.
enum TrustState {
    /// `--signer-key` / `--expect-signer` / allowlist matched.
    Trusted,
    /// TOFU: a pin already existed and matched.
    Pinned,
    /// TOFU: no pin existed — this signer was pinned just now.
    FirstUse,
    /// Signed, integrity + signature verified, but no `lineage` to pin on
    /// and no explicit trust source supplied — so TOFU was skipped and the
    /// signer was never checked against anything known. Distinct from
    /// `Trusted` so the stdout suffix never claims a trust check happened.
    Untracked,
}

/// Resolve trust for a `Valid` signature with fingerprint `fp`, base64
/// public key `pubkey`, and lineage `lineage` (the TOFU pin key).
///
/// Returns `Ok(TrustState)` when the signer is trusted (the caller prints
/// the matching `OK … (…)` line), or `Err(ExitCode)` after having already
/// printed `FAIL …` + the reason to stderr. The precedence is documented
/// on the module.
fn resolve_trust(
    fp: &str,
    pubkey: &str,
    lineage: Option<&str>,
    args: &VerifyArgs,
    artifact: &str,
) -> Result<TrustState, ExitCode> {
    // (1) --signer-key: the plat's pubkey MUST equal the supplied key.
    if let Some(path) = args.signer_key.as_ref() {
        let expected = match load_pubkey_b64(path) {
            Ok(pk) => pk,
            Err(e) => {
                println!("FAIL {artifact}");
                eprintln!("--signer-key: {e}");
                return Err(ExitCode::from(2));
            }
        };
        if pubkey != expected {
            let expected_fp = heso_engine_fetch::plat::signer_fingerprint(&expected)
                .unwrap_or_else(|| "heso:(unfingerprintable)".to_owned());
            println!("FAIL {artifact}");
            eprintln!("EXPECTED SIGNER {expected_fp}, GOT {fp}");
            return Err(ExitCode::from(1));
        }
        return Ok(TrustState::Trusted);
    }

    // (2) --expect-signer: the fingerprint MUST equal the supplied one.
    if let Some(expected_fp) = args.expect_signer.as_ref() {
        if fp != expected_fp {
            println!("FAIL {artifact}");
            eprintln!("EXPECTED SIGNER {expected_fp}, GOT {fp}");
            return Err(ExitCode::from(1));
        }
        return Ok(TrustState::Trusted);
    }

    // (3) --trusted-keys / HESO_TRUSTED_KEYS allowlist.
    match receipts::load_trusted_keys(args.trusted_keys.as_deref()) {
        receipts::AllowlistResult::Loaded(allow) => {
            if allow.is_empty() {
                // A supplied-but-empty allowlist is fail-closed: the user
                // bound verification to a set of signers and the set is
                // empty, so no signer can satisfy it.
                println!("FAIL {artifact}");
                eprintln!(
                    "INVALID: trusted-keys file contains zero entries — no signer can be trusted"
                );
                return Err(ExitCode::from(1));
            }
            if !receipts::pubkey_in_allowlist(pubkey, &allow) {
                println!("FAIL {artifact}");
                eprintln!("INVALID: signing pubkey `{pubkey}` is not in the trusted-keys allowlist");
                return Err(ExitCode::from(1));
            }
            return Ok(TrustState::Trusted);
        }
        receipts::AllowlistResult::Error(msg) => {
            println!("FAIL {artifact}");
            eprintln!("{msg}");
            return Err(ExitCode::from(2));
        }
        // Nothing supplied — fall through to TOFU.
        receipts::AllowlistResult::Empty => {}
    }

    // (4) TOFU. Needs a lineage to key on. A signed plat with no lineage
    // (legacy / hand-built) can't be pinned — verify integrity + signature
    // but flag that the signer can't be tracked across runs.
    let Some(lineage) = lineage else {
        eprintln!(
            "warning: signed {artifact} has no `lineage` — cannot pin its signer (TOFU is keyed \
             by lineage); integrity + signature verified, authenticity untracked. Pass \
             --expect-signer or --signer-key to bind it to a known key."
        );
        return Ok(TrustState::Untracked);
    };

    resolve_tofu(fp, pubkey, lineage, args, artifact)
}

/// The TOFU step of [`resolve_trust`]: consult the pin store keyed by
/// `lineage`. First-use pins; a match passes quietly; a mismatch fails
/// loud unless `--accept-new-signer` re-pins.
fn resolve_tofu(
    fp: &str,
    pubkey: &str,
    lineage: &str,
    args: &VerifyArgs,
    artifact: &str,
) -> Result<TrustState, ExitCode> {
    let store_path = args
        .known_signers
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_KNOWN_SIGNERS_PATH));
    let mut store = match PinStore::load(&store_path) {
        Ok(s) => s,
        Err(e) => {
            println!("FAIL {artifact}");
            eprintln!("INVALID: {e}");
            return Err(ExitCode::from(2));
        }
    };

    match store.lookup(lineage) {
        // Pin matches — quiet pass.
        Some(pin) if pin.fingerprint == fp => Ok(TrustState::Pinned),
        // Pin mismatch.
        Some(pin) => {
            let old = pin.fingerprint.clone();
            if args.accept_new_signer {
                if let Err(e) = store.repin(lineage, fp, pubkey) {
                    println!("FAIL {artifact}");
                    eprintln!("INVALID: failed to re-pin signer: {e}");
                    return Err(ExitCode::from(2));
                }
                eprintln!(
                    "note: re-pinned lineage {lineage} from {old} to {fp} (--accept-new-signer)"
                );
                Ok(TrustState::Pinned)
            } else {
                println!("FAIL {artifact}");
                eprintln!(
                    "SIGNER MISMATCH: lineage {lineage} was pinned to {old}, this {artifact} is \
                     signed by {fp} — refusing (run `heso verify --accept-new-signer` to re-pin, \
                     or --signer-key to check against a known key)"
                );
                Err(ExitCode::from(1))
            }
        }
        // First contact for this lineage — pin it.
        None => match store.pin(lineage, fp, pubkey) {
            Ok(_) => {
                eprintln!(
                    "note: first contact for lineage {lineage}; pinned signer {fp} \
                     (TOFU — a later signer change for this lineage will fail until you \
                     --accept-new-signer)"
                );
                Ok(TrustState::FirstUse)
            }
            Err(e) => {
                println!("FAIL {artifact}");
                eprintln!("INVALID: failed to pin signer: {e}");
                Err(ExitCode::from(2))
            }
        },
    }
}

/// Load a base64-encoded Ed25519 public key from `path`. The file holds
/// the standard-alphabet base64 of the 32 public-key bytes (the
/// `public_key` field shape `heso identity show` prints), optionally with
/// surrounding whitespace / a trailing newline.
///
/// Validity is checked by re-deriving a fingerprint: `signer_fingerprint`
/// returns `None` for anything that isn't standard-alphabet base64 of
/// exactly 32 bytes, so a successful fingerprint proves the file holds a
/// well-formed pubkey — without pulling base64 into this crate.
fn load_pubkey_b64(path: &std::path::Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read signer-key file `{}`: {e}", path.display()))?;
    let trimmed = raw.trim().to_owned();
    if heso_engine_fetch::plat::signer_fingerprint(&trimmed).is_none() {
        return Err(format!(
            "`{}` is not a base64-encoded 32-byte Ed25519 public key \
             (expected the `public_key` value `heso identity show` prints)",
            path.display()
        ));
    }
    Ok(trimmed)
}

// ============================================================================
// Plat (bare / inline-signed)
// ============================================================================

fn verify_plat(value: &serde_json::Value, args: &VerifyArgs) -> ExitCode {
    // (a) Always recompute plat_hash first — integrity gates everything.
    match heso_engine_fetch::plat_verify(value) {
        Ok(true) => {}
        Ok(false) => {
            let embedded = value
                .get("plat_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let recomputed = heso_engine_fetch::plat_hash(value);
            println!("FAIL plat");
            eprintln!("MISMATCH");
            eprintln!("  embedded:   {embedded}");
            eprintln!("  recomputed: {recomputed}");
            return ExitCode::from(1);
        }
        Err(e) => {
            println!("FAIL plat");
            eprintln!("verify failed: {e}");
            return ExitCode::from(2);
        }
    }

    let embedded = value
        .get("plat_hash")
        .and_then(|v| v.as_str())
        .unwrap_or("(unknown)");
    let lineage = value.get("lineage").and_then(|v| v.as_str());

    // (b) / (c): branch on the presence of an inline `sig`.
    match heso_engine_fetch::plat::verify_inline_signature(value) {
        // (c) No signature: integrity-only. Print OK + a stderr warning.
        heso_engine_fetch::plat::InlineOutcome::Unsigned => {
            println!("OK plat {embedded}");
            eprintln!(
                "warning: plat is unsigned — integrity verified, authenticity unknown \
                 (signed plats are the default; this plat predates signing or was produced \
                 with --no-sign)"
            );
            ExitCode::SUCCESS
        }
        heso_engine_fetch::plat::InlineOutcome::WrongAlgorithm(tag) => {
            println!("FAIL plat");
            eprintln!(
                "WRONG ALGORITHM: inline `sig.alg` is `{tag}`, this binary only knows \
                 `{}`.",
                heso_engine_fetch::plat::INLINE_SIG_ALG
            );
            ExitCode::from(2)
        }
        // Unreachable in practice: step (a) above already recomputed the
        // same hash region with the same function `verify_inline_signature`
        // checks, so a body that passed (a) cannot land here. Kept as a
        // defensive catch-all in case the step-(a) guard is ever relaxed.
        heso_engine_fetch::plat::InlineOutcome::HashMismatch => {
            println!("FAIL plat");
            eprintln!("INVALID: inline signature does not cover this body (hash mismatch)");
            ExitCode::from(1)
        }
        heso_engine_fetch::plat::InlineOutcome::InvalidSignature(e) => {
            println!("FAIL plat");
            eprintln!("INVALID: inline signature does not verify: {e}");
            ExitCode::from(1)
        }
        // (b) Valid signature → fingerprint + trust resolution.
        heso_engine_fetch::plat::InlineOutcome::Valid { public_key } => {
            let Some(fp) = heso_engine_fetch::plat::signer_fingerprint(&public_key) else {
                println!("FAIL plat");
                eprintln!("INVALID: signer public key is not a 32-byte Ed25519 key");
                return ExitCode::from(2);
            };
            match resolve_trust(&fp, &public_key, lineage, args, "plat") {
                Ok(state) => {
                    print_ok_line("plat", embedded, &fp, state);
                    ExitCode::SUCCESS
                }
                Err(code) => code,
            }
        }
    }
}

/// Print the single stdout success line — always carries the signer
/// fingerprint and the trust-state suffix, never a bare `OK`.
fn print_ok_line(artifact: &str, hash: &str, fp: &str, state: TrustState) {
    let suffix = match state {
        TrustState::Pinned => "(pinned)",
        TrustState::FirstUse => "(first-use, pinned now)",
        TrustState::Trusted => "(trusted)",
        TrustState::Untracked => "(untracked — no lineage, TOFU skipped)",
    };
    println!("OK {artifact} {hash} signer {fp} {suffix}");
}

// ============================================================================
// Sealed plat envelope
// ============================================================================

fn verify_sealed_plat(value: &serde_json::Value, args: &VerifyArgs) -> ExitCode {
    let sealed: heso_engine_fetch::SealedPlat = match serde_json::from_value(value.clone()) {
        Ok(v) => v,
        Err(e) => {
            println!("FAIL sealed-plat");
            eprintln!("not a sealed envelope: {e}");
            eprintln!("expected JSON with `alg`, `content`, and `signature` fields.");
            return ExitCode::from(2);
        }
    };
    match heso_engine_fetch::plat_open(&sealed) {
        heso_engine_fetch::PlatOpenOutcome::Valid => {
            let hash = sealed
                .content
                .get("plat_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let lineage = sealed.content.get("lineage").and_then(|v| v.as_str());
            let pubkey = sealed.signature.public_key.clone();
            let Some(fp) = heso_engine_fetch::plat::signer_fingerprint(&pubkey) else {
                println!("FAIL sealed-plat");
                eprintln!("INVALID: signer public key is not a 32-byte Ed25519 key");
                return ExitCode::from(2);
            };
            match resolve_trust(&fp, &pubkey, lineage, args, "sealed-plat") {
                Ok(state) => {
                    print_ok_line("sealed-plat", hash, &fp, state);
                    ExitCode::SUCCESS
                }
                Err(code) => code,
            }
        }
        heso_engine_fetch::PlatOpenOutcome::HashMismatch => {
            let embedded = sealed
                .content
                .get("plat_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let recomputed = heso_engine_fetch::plat_hash(&sealed.content);
            println!("FAIL sealed-plat");
            eprintln!("INVALID: content `plat_hash` does not match recomputed BLAKE3");
            eprintln!("  embedded:   {embedded}");
            eprintln!("  recomputed: {recomputed}");
            ExitCode::from(1)
        }
        heso_engine_fetch::PlatOpenOutcome::InvalidSignature(e) => {
            println!("FAIL sealed-plat");
            eprintln!("INVALID: signature does not verify: {e}");
            ExitCode::from(1)
        }
        heso_engine_fetch::PlatOpenOutcome::WrongAlgorithm(tag) => {
            println!("FAIL sealed-plat");
            eprintln!("WRONG ALGORITHM: envelope carries `{tag}`, this binary only knows `heso-plat/v1+ed25519`.");
            ExitCode::from(2)
        }
    }
}

fn verify_receipt_inner(
    value: &serde_json::Value,
    trusted_keys: &Option<PathBuf>,
    require_tsa: bool,
    _tsa_trusted_roots: Option<&PathBuf>,
) -> ExitCode {
    if require_tsa {
        println!("FAIL receipt");
        eprintln!(
            "TSA verification not yet implemented in this build (--require-tsa). \
             Ships in a follow-up release."
        );
        return ExitCode::from(2);
    }

    let allowlist = match receipts::load_trusted_keys(trusted_keys.as_deref()) {
        receipts::AllowlistResult::Loaded(v) if v.is_empty() => {
            // A supplied allowlist with zero entries is a configuration
            // error, not a "trust anyone" wildcard. The user explicitly
            // asked to bind verification to a set of signers and that
            // set is empty, so no signer can satisfy it — fail closed.
            // Same outcome whether the empty source was `--trusted-keys`
            // or the env var (both arrive as `Loaded`).
            println!("FAIL receipt");
            eprintln!(
                "INVALID: trusted-keys file contains zero entries — no signer can be trusted"
            );
            return ExitCode::from(1);
        }
        receipts::AllowlistResult::Loaded(v) => Some(v),
        receipts::AllowlistResult::Empty => {
            eprintln!(
                "warning: no pubkey allowlist configured (pass --trusted-keys PATH or set {} \
                 to bind receipts to a known signer; verifying signatures without identity)",
                receipts::TRUSTED_KEYS_ENV
            );
            None
        }
        receipts::AllowlistResult::Error(msg) => {
            println!("FAIL receipt");
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let receipt: heso_trace::Receipt = match serde_json::from_value(value.clone()) {
        Ok(r) => r,
        Err(e) => {
            println!("FAIL receipt");
            eprintln!("not a valid Receipt JSON: {e}");
            return ExitCode::from(2);
        }
    };

    if matches!(receipt.mode, heso_trace::Mode::Live) {
        println!("FAIL receipt");
        eprintln!(
            "INVALID: receipt `mode: live` is not replay-safe — only \
             `deterministic` and `recording` receipts can be verified (live runs use \
             wall-clock time and real network, so the signature has no replay value)"
        );
        return ExitCode::from(1);
    }

    match verify_receipt(&receipt) {
        heso_trace::VerifyOutcome::Valid => {
            let pk = receipt
                .signature
                .as_ref()
                .map(|s| s.public_key.as_str())
                .unwrap_or("(unknown)");
            if let Some(allow) = allowlist.as_ref() {
                if !receipts::pubkey_in_allowlist(pk, allow) {
                    println!("FAIL receipt");
                    eprintln!(
                        "INVALID: signing pubkey `{pk}` is not in the trusted-keys allowlist"
                    );
                    return ExitCode::from(1);
                }
            }
            println!("OK receipt {pk}");
            ExitCode::SUCCESS
        }
        heso_trace::VerifyOutcome::Invalid(e) => {
            println!("FAIL receipt");
            eprintln!("INVALID: {e}");
            ExitCode::from(1)
        }
        heso_trace::VerifyOutcome::Missing => {
            println!("FAIL receipt");
            eprintln!("MISSING: receipt has no `signature` field");
            ExitCode::from(2)
        }
    }
}

fn verify_action_hash(value: &serde_json::Value) -> ExitCode {
    let fp: TraceFingerprint = match serde_json::from_value(value.clone()) {
        Ok(v) => v,
        Err(e) => {
            println!("FAIL action-hash");
            eprintln!("MALFORMED: not a valid fingerprint JSON: {e}");
            return ExitCode::from(2);
        }
    };
    match verify_fingerprint(&fp) {
        FingerprintOutcome::Valid => {
            println!("OK action-hash {} {}", fp.algorithm, fp.trace_id);
            ExitCode::SUCCESS
        }
        FingerprintOutcome::Mismatch => {
            println!("FAIL action-hash");
            eprintln!("INVALID: recompute disagrees — file was modified after creation");
            ExitCode::from(1)
        }
        FingerprintOutcome::WrongAlgorithm(tag) => {
            println!("FAIL action-hash");
            eprintln!(
                "INVALID: unknown algorithm tag `{tag}` (this build supports only `heso-trace-fp/v1`)"
            );
            ExitCode::from(1)
        }
        FingerprintOutcome::Malformed(reason) => {
            println!("FAIL action-hash");
            eprintln!("MALFORMED: {reason}");
            ExitCode::from(2)
        }
    }
}

fn verify_template(raw: &str) -> ExitCode {
    match template::validate_template_raw(raw) {
        Ok(summary) => {
            println!(
                "OK template {} ({} v{})",
                summary.template_hash, summary.id, summary.version
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            println!("FAIL template");
            eprintln!("INVALID: {e}");
            ExitCode::from(1)
        }
    }
}
