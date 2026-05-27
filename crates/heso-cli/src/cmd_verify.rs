//! Polymorphic `heso verify <file>`.
//!
//! Sniffs the artifact kind and dispatches to the right verifier:
//! plat, sealed plat, receipt, action-hash fingerprint, or template.
//! Exit codes follow the per-type conventions the dedicated verbs
//! already use, so a script that swaps `plat-verify` for `verify` sees
//! the same exit shape.

use std::path::PathBuf;
use std::process::ExitCode;

use heso_trace::{verify_fingerprint, verify_receipt, FingerprintOutcome, TraceFingerprint};

use crate::artifact_sniffer::{detect, ArtifactKind};
use crate::receipts;
use crate::template;

struct VerifyArgs {
    file: String,
    trusted_keys: Option<PathBuf>,
    require_tsa: bool,
    tsa_trusted_roots: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<VerifyArgs, ExitCode> {
    let mut file: Option<String> = None;
    let mut trusted_keys: Option<PathBuf> = None;
    let mut require_tsa = false;
    let mut tsa_trusted_roots: Option<PathBuf> = None;
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
    })
}

fn print_usage() {
    eprintln!("usage: heso verify [--trusted-keys PATH] [--require-tsa] [--tsa-trusted-roots PATH] <file>");
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

    let (contents, _source) = match crate::read_plat_input_or_hash(&parsed.file, "verify").await {
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
        ArtifactKind::Plat => verify_plat(&value),
        ArtifactKind::SealedPlat => verify_sealed_plat(&value),
        ArtifactKind::Receipt => {
            verify_receipt_inner(&value, &parsed.trusted_keys, parsed.require_tsa, parsed.tsa_trusted_roots.as_ref())
        }
        ArtifactKind::ActionHash => verify_action_hash(&value),
        ArtifactKind::Template => verify_template(&contents),
    }
}

fn verify_plat(value: &serde_json::Value) -> ExitCode {
    match heso_engine_fetch::plat_verify(value) {
        Ok(true) => {
            let embedded = value
                .get("plat_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            println!("OK plat {embedded}");
            ExitCode::SUCCESS
        }
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
            ExitCode::from(1)
        }
        Err(e) => {
            println!("FAIL plat");
            eprintln!("verify failed: {e}");
            ExitCode::from(2)
        }
    }
}

fn verify_sealed_plat(value: &serde_json::Value) -> ExitCode {
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
            println!(
                "OK sealed-plat {} ({})",
                sealed
                    .content
                    .get("plat_hash")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unknown)"),
                sealed.signature.public_key,
            );
            ExitCode::SUCCESS
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
                if !allow.is_empty() && !receipts::pubkey_in_allowlist(pk, allow) {
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
