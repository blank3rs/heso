//! Polymorphic `heso seal <file>`.
//!
//! Wraps a plat in an Ed25519 envelope (delegating to the same code
//! `plat-seal` uses).

use std::path::PathBuf;
use std::process::ExitCode;

use heso_core::IdentityKey;

use crate::artifact_sniffer::{detect, ArtifactKind};
use crate::DEFAULT_IDENTITY_PATH;

struct SealArgs {
    file: String,
    key_path: Option<PathBuf>,
}

fn parse_args(args: &[String]) -> Result<SealArgs, ExitCode> {
    let mut file: Option<String> = None;
    let mut key_path: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                print_usage();
                return Err(ExitCode::SUCCESS);
            }
            "--key" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--key needs a value");
                    return Err(ExitCode::from(2));
                };
                key_path = Some(PathBuf::from(v));
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
    Ok(SealArgs { file, key_path })
}

fn print_usage() {
    eprintln!("usage: heso seal <file> [--key PATH]");
    eprintln!();
    eprintln!("Wrap a plat in an Ed25519 envelope.");
}

/// `heso seal <file>` — wrap a plat in an Ed25519 envelope.
pub async fn cmd_seal(args: &[String]) -> ExitCode {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(code) => return code,
    };

    let contents = match tokio::fs::read_to_string(&parsed.file).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read `{}`: {e}", parsed.file);
            return ExitCode::from(2);
        }
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
        ArtifactKind::Plat => seal_plat(value, parsed.key_path),
        ArtifactKind::Receipt => {
            eprintln!("seal: receipt is already signed; nothing to do");
            ExitCode::from(2)
        }
        ArtifactKind::SealedPlat => {
            eprintln!("seal: input is already a sealed envelope");
            ExitCode::from(2)
        }
        ArtifactKind::ActionHash => {
            eprintln!("seal: cannot seal an action-hash fingerprint");
            ExitCode::from(2)
        }
        ArtifactKind::Template => {
            eprintln!("seal: cannot seal a template");
            ExitCode::from(2)
        }
    }
}

pub(crate) fn seal_plat(mut body: serde_json::Value, key_path: Option<PathBuf>) -> ExitCode {
    // A sealed envelope is the stronger, standalone trust unit; its
    // `content` is the canonical bare body. If the input is already
    // inline-signed, strip the top-level `sig` so the envelope signs clean
    // content rather than re-wrapping a second signature. `sig` is in the
    // hash region's strip-set, so removing it leaves `plat_hash` valid and
    // the envelope verifies.
    let stripped_sig = body
        .as_object_mut()
        .is_some_and(|obj| obj.remove("sig").is_some());
    if stripped_sig {
        eprintln!("seal: stripped the inline `sig` from the input; the envelope signs the bare body");
    }

    let path = key_path.unwrap_or_else(|| PathBuf::from(DEFAULT_IDENTITY_PATH));
    let key = match IdentityKey::load(&path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("failed to load identity at `{}`: {e}", path.display());
            eprintln!("run `heso identity init` first, or pass --key <PATH>.");
            return ExitCode::FAILURE;
        }
    };
    let sealed = match heso_engine_fetch::plat_seal_checked(&key, body) {
        Ok(s) => s,
        Err(heso_engine_fetch::PlatSealError::HashMismatch {
            embedded,
            recomputed,
        }) => {
            eprintln!(
                "seal: input plat_hash does not match its content; refusing to sign a body whose hash claim is already false"
            );
            eprintln!("  embedded:   {embedded}");
            eprintln!("  recomputed: {recomputed}");
            eprintln!("  run `heso verify` on the input to see details, or strip the stale `plat_hash` and try again");
            return ExitCode::from(2);
        }
        Err(heso_engine_fetch::PlatSealError::MalformedHashField) => {
            eprintln!("seal: input's `plat_hash` field is not a string");
            return ExitCode::from(2);
        }
    };
    match serde_json::to_string(&sealed) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to serialize sealed envelope: {e}");
            ExitCode::FAILURE
        }
    }
}
