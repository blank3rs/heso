//! Standalone HESO/1.0 Grade-0 verifier CLI.
//!
//! Reads a JSON artifact from a file argument or stdin, detects whether
//! it is a **sealed envelope** (`{alg, content, signature}`) or a **bare
//! plat** (carries `plat_hash`), verifies it, and exits:
//!
//! - `0` — valid (sealed envelope verifies, or bare plat's `plat_hash`
//!   matches its content).
//! - `1` — invalid signature.
//! - `2` — wrong algorithm, hash mismatch, malformed input, or a bare
//!   plat with a missing/non-string `plat_hash`.
//!
//! No engine, no network, no clock — verification needs nothing but the
//! artifact and this binary.

use std::io::Read as _;
use std::process::ExitCode;

use heso_verify::{open, verify_plat_hash, Outcome, SealedPlat};
use serde_json::Value;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print_usage();
        return ExitCode::SUCCESS;
    }
    // At most one positional: the file path. Missing / `-` means stdin.
    let path = match args.as_slice() {
        [] => None,
        [p] if p == "-" => None,
        [p] => Some(p.clone()),
        _ => {
            eprintln!("error: pass at most one <file> (or `-`/nothing for stdin)");
            print_usage();
            return ExitCode::from(2);
        }
    };

    let raw = match read_input(path.as_deref()) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let value: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: input is not valid JSON: {e}");
            return ExitCode::from(2);
        }
    };

    // Sealed envelope wins the tie: `{alg, content, signature}`. Anything
    // else carrying a `plat_hash` is treated as a bare plat.
    if is_sealed_envelope(&value) {
        verify_sealed(&value)
    } else {
        verify_bare_plat(&value)
    }
}

fn is_sealed_envelope(value: &Value) -> bool {
    value
        .as_object()
        .map(|o| o.contains_key("alg") && o.contains_key("content") && o.contains_key("signature"))
        .unwrap_or(false)
}

fn verify_sealed(value: &Value) -> ExitCode {
    let sealed: SealedPlat = match serde_json::from_value(value.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("FAIL sealed-plat");
            eprintln!("error: not a well-formed sealed envelope: {e}");
            return ExitCode::from(2);
        }
    };
    match open(&sealed) {
        Outcome::Valid => {
            println!(
                "OK sealed-plat {} ({})",
                sealed
                    .content
                    .get("plat_hash")
                    .and_then(Value::as_str)
                    .unwrap_or("(unknown)"),
                sealed.signature.public_key,
            );
            ExitCode::SUCCESS
        }
        Outcome::HashMismatch => {
            let embedded = sealed
                .content
                .get("plat_hash")
                .and_then(Value::as_str)
                .unwrap_or("(unknown)");
            let recomputed = heso_verify::plat_hash(&sealed.content);
            println!("FAIL sealed-plat");
            eprintln!("HASH MISMATCH: content `plat_hash` does not match recomputed BLAKE3");
            eprintln!("  embedded:   {embedded}");
            eprintln!("  recomputed: {recomputed}");
            ExitCode::from(2)
        }
        Outcome::WrongAlgorithm(tag) => {
            println!("FAIL sealed-plat");
            eprintln!(
                "WRONG ALGORITHM: envelope carries `{tag}`, this verifier only knows \
                 `heso-plat/v1+ed25519`"
            );
            ExitCode::from(2)
        }
        Outcome::InvalidSignature(e) => {
            println!("FAIL sealed-plat");
            eprintln!("INVALID SIGNATURE: {e}");
            ExitCode::from(1)
        }
    }
}

fn verify_bare_plat(value: &Value) -> ExitCode {
    match verify_plat_hash(value) {
        Ok(true) => {
            let embedded = value
                .get("plat_hash")
                .and_then(Value::as_str)
                .unwrap_or("(unknown)");
            println!("OK plat {embedded}");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            let embedded = value
                .get("plat_hash")
                .and_then(Value::as_str)
                .unwrap_or("(unknown)");
            let recomputed = heso_verify::plat_hash(value);
            println!("FAIL plat");
            eprintln!("HASH MISMATCH");
            eprintln!("  embedded:   {embedded}");
            eprintln!("  recomputed: {recomputed}");
            ExitCode::from(2)
        }
        Err(e) => {
            println!("FAIL plat");
            eprintln!(
                "error: {e} — input is neither a sealed envelope nor a hashable plat \
                 (expected `alg`+`content`+`signature`, or a string `plat_hash`)"
            );
            ExitCode::from(2)
        }
    }
}

fn read_input(path: Option<&str>) -> Result<Vec<u8>, String> {
    match path {
        Some(p) => std::fs::read(p).map_err(|e| format!("cannot read `{p}`: {e}")),
        None => {
            let mut buf = Vec::new();
            std::io::stdin()
                .read_to_end(&mut buf)
                .map_err(|e| format!("cannot read stdin: {e}"))?;
            Ok(buf)
        }
    }
}

fn print_usage() {
    eprintln!("usage: heso-verify [<file>]");
    eprintln!();
    eprintln!("Verify a HESO/1.0 sealed envelope or bare plat. Reads <file>, or stdin when");
    eprintln!("<file> is omitted or `-`.");
    eprintln!();
    eprintln!("exit codes:");
    eprintln!("  0  valid");
    eprintln!("  1  invalid signature");
    eprintln!("  2  wrong algorithm, hash mismatch, or malformed input");
}
