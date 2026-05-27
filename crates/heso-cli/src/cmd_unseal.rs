//! Polymorphic `heso unseal <file>`.
//!
//! Mirror of [`cmd_seal`] for the inverse direction. Only operates on a
//! sealed plat envelope; any other artifact kind exits 2 with a
//! type-specific message.

use std::process::ExitCode;

use crate::artifact_sniffer::{detect, ArtifactKind};

/// `heso unseal <file> [--extract]` — verify a sealed envelope. With
/// `--extract`, emit the inner plat body to stdout instead of the status
/// JSON.
pub async fn cmd_unseal(args: &[String]) -> ExitCode {
    let mut file: Option<String> = None;
    let mut extract = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            "--extract" => {
                extract = true;
                i += 1;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag `{other}`");
                print_usage();
                return ExitCode::from(2);
            }
            _ => {
                if file.is_some() {
                    eprintln!("unexpected extra argument `{}`; pass a single <file>", args[i]);
                    return ExitCode::from(2);
                }
                file = Some(args[i].clone());
                i += 1;
            }
        }
    }
    let Some(file) = file else {
        print_usage();
        return ExitCode::from(2);
    };

    let contents = match tokio::fs::read_to_string(&file).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read `{file}`: {e}");
            return ExitCode::from(2);
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("`{file}` is not valid JSON: {e}");
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
        ArtifactKind::SealedPlat => {}
        ArtifactKind::Plat => {
            eprintln!("unseal: input is a plain plat, not a sealed envelope");
            return ExitCode::from(2);
        }
        ArtifactKind::Receipt => {
            eprintln!("unseal: input is a receipt; use `heso verify` to check signatures");
            return ExitCode::from(2);
        }
        ArtifactKind::ActionHash => {
            eprintln!("unseal: action-hash fingerprints cannot be unsealed");
            return ExitCode::from(2);
        }
        ArtifactKind::Template => {
            eprintln!("unseal: templates cannot be unsealed");
            return ExitCode::from(2);
        }
    }

    let sealed: heso_engine_fetch::SealedPlat = match serde_json::from_value(value) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("`{file}` looked sealed but failed to parse: {e}");
            return ExitCode::from(2);
        }
    };

    match heso_engine_fetch::plat_open(&sealed) {
        heso_engine_fetch::PlatOpenOutcome::Valid => {
            if extract {
                match serde_json::to_string(&sealed.content) {
                    Ok(s) => {
                        println!("{s}");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("failed to serialize content: {e}");
                        ExitCode::FAILURE
                    }
                }
            } else {
                let body = serde_json::json!({
                    "status": "valid",
                    "alg": sealed.alg,
                    "public_key": sealed.signature.public_key,
                    "plat_hash": sealed.content
                        .get("plat_hash")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                });
                match serde_json::to_string(&body) {
                    Ok(s) => {
                        println!("{s}");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("failed to serialize status: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
        }
        heso_engine_fetch::PlatOpenOutcome::HashMismatch => {
            let embedded = sealed
                .content
                .get("plat_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let recomputed = heso_engine_fetch::plat_hash(&sealed.content);
            eprintln!("INVALID: content `plat_hash` does not match recomputed BLAKE3");
            eprintln!("  embedded:   {embedded}");
            eprintln!("  recomputed: {recomputed}");
            ExitCode::from(1)
        }
        heso_engine_fetch::PlatOpenOutcome::InvalidSignature(e) => {
            eprintln!("INVALID: signature does not verify: {e}");
            ExitCode::from(1)
        }
        heso_engine_fetch::PlatOpenOutcome::WrongAlgorithm(tag) => {
            eprintln!("WRONG ALGORITHM: envelope carries `{tag}`, this binary only knows `heso-plat/v1+ed25519`.");
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!("usage: heso unseal <file> [--extract]");
    eprintln!();
    eprintln!("Verify a sealed plat envelope. With --extract, also emit the inner");
    eprintln!("plat body to stdout for piping into another verb.");
}
