//! Signed-receipt CLI wiring.
//!
//! The signed-receipts headline pitch ("`heso open` returns a signed,
//! replayable receipt") used to be **library-only** — `heso_trace_exec::run_signed`
//! existed but no CLI verb produced one. This module is the missing
//! piece: it parses the `--receipt PATH [--key PATH] [--mode MODE]
//! [--seed N]` flag suite once, builds the [`heso_trace::Trace`] that
//! corresponds to the verb's user-facing action, drives the trace
//! through [`heso_trace_exec::run_signed`], and writes the signed
//! [`heso_trace::Receipt`] to disk alongside the verb's normal stdout
//! JSON.
//!
//! ## Design choice
//!
//! `--receipt PATH` on existing verbs (Option C from the audit) is the
//! least invasive change: the verb's stdout shape is unchanged so
//! existing pipelines keep working, and the receipt lands on disk in
//! a deterministic location the caller chose. The alternative — a
//! `record` wrapper verb (Option B) — would force callers to learn a
//! new top-level command and re-plumb their parsers; the `--sign`
//! flag-only variant (Option A) would either mutate the stdout shape
//! (breaking) or push receipts to a sibling file with a name we'd have
//! to invent (less control).
//!
//! ## Verify-side trust anchor
//!
//! [`load_trusted_keys`] reads a JSON allowlist of base64-encoded
//! Ed25519 public keys from a path (CLI: `--trusted-keys`) or env var
//! (`HESO_TRUSTED_KEYS`). When the allowlist is non-empty,
//! `receipt-verify` rejects any receipt whose signing pubkey isn't on
//! the list. When the allowlist is empty (default), verify still
//! accepts any pubkey but prints a stderr warning so the caller can't
//! silently lose the trust anchor.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use heso_core::{IdentityKey, Url};
use heso_engine_api::EngineApi;
use heso_primitives::{CdInput, CdTarget, PrimitiveOp};
use heso_trace::{Mode, Trace};
use heso_trace_exec::SessionConfig;

/// Parsed `--receipt … --key … --mode … --seed …` flag bundle.
///
/// Constructed by [`try_consume_sign_flag`]; consumed by
/// [`emit_signed_receipt`]. All fields are `Option`-ish at the CLI
/// layer — only `receipt_path` makes the verb actually emit a receipt
/// at all.
#[derive(Debug, Clone, Default)]
pub(crate) struct SignFlags {
    /// `--receipt PATH` — where to write the signed receipt JSON. When
    /// `None` the verb skips signing entirely (free-of-charge fallback).
    pub(crate) receipt_path: Option<PathBuf>,
    /// `--key PATH` — path to the Ed25519 identity key. Defaults to
    /// the same `heso-local-data/identity.key` `heso identity init` uses.
    pub(crate) key_path: Option<PathBuf>,
    /// `--mode {deterministic|recording|live}` — operating mode that
    /// gets stamped into the receipt. Default `deterministic`.
    pub(crate) mode: Option<Mode>,
    /// `--seed N` — session seed threaded into the receipt so verifiers
    /// can reproduce a deterministic run.
    pub(crate) seed: Option<u64>,
}

impl SignFlags {
    /// `true` when `--receipt PATH` was supplied. Verbs gate the
    /// trace-exec path on this — no flag means no extra work.
    pub(crate) fn is_active(&self) -> bool {
        self.receipt_path.is_some()
    }
}

/// Walk one CLI arg. Returns `Ok(Some(consumed))` when this arg (and
/// optionally the next one) was a receipt-sign flag (`consumed` is 1
/// or 2 — the number of arg slots to skip). Returns `Ok(None)` when
/// the arg is **not** a receipt-sign flag — caller handles it.
/// Returns `Err(ExitCode)` with the appropriate usage exit (2) when a
/// receipt-sign flag had a malformed value.
///
/// Designed to be dropped into the existing per-verb arg-walk loops:
/// the loop calls this helper first; if it returns `Some`, the loop
/// advances `i` by the returned count; if `None`, the loop falls
/// through to its own flag match.
pub(crate) fn try_consume_sign_flag(
    args: &[String],
    i: usize,
    flags: &mut SignFlags,
) -> Result<Option<usize>, ExitCode> {
    let Some(arg) = args.get(i) else {
        return Ok(None);
    };
    match arg.as_str() {
        "--receipt" => {
            let Some(v) = args.get(i + 1) else {
                eprintln!("--receipt needs a value (path to write the signed receipt to)");
                return Err(ExitCode::from(2));
            };
            flags.receipt_path = Some(PathBuf::from(v));
            Ok(Some(2))
        }
        "--key" => {
            let Some(v) = args.get(i + 1) else {
                eprintln!("--key needs a value (path to the Ed25519 identity key)");
                return Err(ExitCode::from(2));
            };
            flags.key_path = Some(PathBuf::from(v));
            Ok(Some(2))
        }
        "--mode" => {
            let Some(v) = args.get(i + 1) else {
                eprintln!("--mode needs a value (one of: deterministic, recording, live)");
                return Err(ExitCode::from(2));
            };
            let parsed = match v.as_str() {
                "deterministic" => Mode::Deterministic,
                "recording" => Mode::Recording,
                "live" => Mode::Live,
                other => {
                    eprintln!(
                        "--mode: invalid value `{other}` (expected one of: deterministic, recording, live)"
                    );
                    return Err(ExitCode::from(2));
                }
            };
            flags.mode = Some(parsed);
            Ok(Some(2))
        }
        "--seed" => {
            let Some(v) = args.get(i + 1) else {
                eprintln!("--seed needs a u64 value");
                return Err(ExitCode::from(2));
            };
            match v.parse::<u64>() {
                Ok(n) => flags.seed = Some(n),
                Err(e) => {
                    eprintln!("--seed: invalid u64 `{v}`: {e}");
                    return Err(ExitCode::from(2));
                }
            }
            Ok(Some(2))
        }
        _ => Ok(None),
    }
}

/// Default identity-key path used by sign flags when `--key` is absent.
/// Matches the default `heso identity init` writes.
const DEFAULT_IDENTITY_PATH: &str = "heso-local-data/identity.key";

/// Build the [`Trace`] that corresponds to a single "open this URL"
/// action — the natural trace for `heso open <url>` and `heso read <url>`.
/// One `cd` primitive with a URL target.
pub(crate) fn url_trace(url: &Url) -> Trace {
    vec![PrimitiveOp::Cd(CdInput {
        target: CdTarget::Url { url: url.clone() },
    })]
}

/// Drive `trace` through [`heso_trace_exec::run_signed`] using `flags`
/// for configuration, then write the signed [`heso_trace::Receipt`]
/// JSON to `flags.receipt_path`. Returns `Ok(())` on success,
/// `Err(ExitCode)` with an appropriate non-zero exit on any failure.
///
/// On success the caller's stdout JSON is unaffected — the receipt is
/// a sibling artifact.
pub(crate) async fn emit_signed_receipt<E: EngineApi>(
    engine: &E,
    trace: &Trace,
    flags: &SignFlags,
) -> Result<(), ExitCode> {
    let Some(receipt_path) = flags.receipt_path.as_ref() else {
        // The verb shouldn't have called us; bail out silently — this
        // is a programmer-error guard, not a user-facing error.
        return Ok(());
    };
    let key_path = flags
        .key_path
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_IDENTITY_PATH));
    let key = match IdentityKey::load(&key_path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!(
                "--receipt: failed to load identity key at `{}`: {e}\n\
                 (run `heso identity init` first, or pass `--key <path>`)",
                key_path.display()
            );
            return Err(ExitCode::FAILURE);
        }
    };

    let config = SessionConfig {
        seed: flags.seed.unwrap_or(0),
        mode: flags.mode.unwrap_or(Mode::Deterministic),
        planner_id: String::new(),
    };

    let receipt = heso_trace_exec::run_signed(engine, trace, &config, &key).await;

    // Pretty-printed for human eyeballing — receipts are small JSON
    // documents (one trace op for `open`/`read`) so the extra bytes
    // are negligible and the diff-friendliness pays off in agent logs.
    let body = match serde_json::to_string_pretty(&receipt) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("--receipt: failed to serialize signed receipt: {e}");
            return Err(ExitCode::FAILURE);
        }
    };

    // Create the parent directory if the caller passed something like
    // `out/run42/receipt.json` — matches the convenience the rest of
    // the CLI offers for arbitrary output paths.
    if let Some(parent) = receipt_path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "--receipt: failed to create parent dir `{}`: {e}",
                    parent.display()
                );
                return Err(ExitCode::FAILURE);
            }
        }
    }
    if let Err(e) = std::fs::write(receipt_path, &body) {
        eprintln!(
            "--receipt: failed to write `{}`: {e}",
            receipt_path.display()
        );
        return Err(ExitCode::FAILURE);
    }

    Ok(())
}

// ============================================================================
// Pubkey allowlist for `receipt-verify`
// ============================================================================

/// Env var name for the pubkey allowlist path. Read when the
/// `--trusted-keys` CLI flag isn't supplied.
pub(crate) const TRUSTED_KEYS_ENV: &str = "HESO_TRUSTED_KEYS";

/// Outcome of resolving a `--trusted-keys` allowlist source. The CLI
/// only treats an explicitly-supplied source that fails to load as
/// fatal — when nothing is supplied at all, [`AllowlistResult::Empty`]
/// lets the caller keep the legacy "any-pubkey OK" behavior with a
/// stderr warning.
#[derive(Debug)]
pub(crate) enum AllowlistResult {
    /// User supplied a source (CLI flag or env var) and the keys
    /// loaded successfully. Holds the parsed allowlist.
    Loaded(Vec<String>),
    /// No source supplied. `receipt-verify` will emit a stderr
    /// warning and accept any signature.
    Empty,
    /// User supplied a source but loading failed. The caller exits
    /// non-zero with a clear error — silently downgrading to
    /// "no allowlist" would defeat the point of the flag.
    Error(String),
}

/// Read a JSON pubkey allowlist from `path`. The file shape is either:
///
/// - A bare array of base64-encoded public-key strings:
///   `["pk1Base64==", "pk2Base64=="]`
/// - Or an object with a `keys` field of the same shape:
///   `{"keys": ["pk1Base64==", "pk2Base64=="]}`
///
/// The object form leaves room to add fields (e.g. labels, expiry)
/// without breaking the array shape.
fn read_allowlist_file(path: &Path) -> Result<Vec<String>, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read trusted-keys file `{}`: {e}", path.display()))?;
    parse_allowlist_json(&contents)
        .map_err(|e| format!("trusted-keys file `{}` is malformed: {e}", path.display()))
}

/// Parse the JSON body of an allowlist file. Accepts either the bare
/// array `["pk1", "pk2"]` or `{"keys": ["pk1", "pk2"]}`.
pub(crate) fn parse_allowlist_json(json: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| format!("not valid JSON: {e}"))?;
    let arr = match &value {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(obj) => match obj.get("keys") {
            Some(serde_json::Value::Array(a)) => a,
            _ => {
                return Err(
                    "expected an array of base64 pubkey strings, or `{\"keys\": [...]}` object"
                        .to_owned(),
                );
            }
        },
        _ => {
            return Err(
                "expected an array of base64 pubkey strings, or `{\"keys\": [...]}` object"
                    .to_owned(),
            );
        }
    };
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        match v.as_str() {
            Some(s) if !s.trim().is_empty() => out.push(s.trim().to_owned()),
            Some(_) => return Err(format!("entry [{i}] is an empty string")),
            None => return Err(format!("entry [{i}] is not a string")),
        }
    }
    Ok(out)
}

/// Resolve the pubkey allowlist for `receipt-verify`. Precedence:
///
/// 1. `--trusted-keys PATH` (when `flag_value` is `Some`).
/// 2. `HESO_TRUSTED_KEYS=PATH` env var.
/// 3. Nothing → [`AllowlistResult::Empty`].
pub(crate) fn load_trusted_keys(flag_value: Option<&Path>) -> AllowlistResult {
    if let Some(path) = flag_value {
        return match read_allowlist_file(path) {
            Ok(keys) => AllowlistResult::Loaded(keys),
            Err(e) => AllowlistResult::Error(e),
        };
    }
    if let Ok(env_path) = std::env::var(TRUSTED_KEYS_ENV) {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() {
            return match read_allowlist_file(Path::new(trimmed)) {
                Ok(keys) => AllowlistResult::Loaded(keys),
                Err(e) => AllowlistResult::Error(e),
            };
        }
    }
    AllowlistResult::Empty
}

/// Check whether `pubkey` (base64 of the 32-byte Ed25519 public key,
/// matching the format `Signature::public_key` stores) appears in
/// `allowlist`. Both sides are compared as exact-byte strings; the
/// allowlist parser already trimmed surrounding whitespace at load
/// time.
pub(crate) fn pubkey_in_allowlist(pubkey: &str, allowlist: &[String]) -> bool {
    allowlist.iter().any(|k| k == pubkey)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_trace_has_one_cd_op() {
        let u = Url::parse("https://example.com/").unwrap();
        let t = url_trace(&u);
        assert_eq!(t.len(), 1);
        match &t[0] {
            PrimitiveOp::Cd(CdInput {
                target: CdTarget::Url { url },
            }) => {
                assert_eq!(url.as_str(), "https://example.com/");
            }
            other => panic!("expected Cd(Url), got {other:?}"),
        }
    }

    #[test]
    fn try_consume_recognizes_receipt_flag() {
        let args = vec!["--receipt".to_owned(), "/tmp/r.json".to_owned()];
        let mut flags = SignFlags::default();
        let consumed = try_consume_sign_flag(&args, 0, &mut flags).expect("ok");
        assert_eq!(consumed, Some(2));
        assert_eq!(flags.receipt_path.as_deref(), Some(Path::new("/tmp/r.json")));
        assert!(flags.is_active());
    }

    #[test]
    fn try_consume_returns_none_for_unknown() {
        let args = vec!["--something-else".to_owned()];
        let mut flags = SignFlags::default();
        let consumed = try_consume_sign_flag(&args, 0, &mut flags).expect("ok");
        assert_eq!(consumed, None);
        assert!(!flags.is_active());
    }

    #[test]
    fn try_consume_parses_mode() {
        let args = vec!["--mode".to_owned(), "recording".to_owned()];
        let mut flags = SignFlags::default();
        let consumed = try_consume_sign_flag(&args, 0, &mut flags).expect("ok");
        assert_eq!(consumed, Some(2));
        assert_eq!(flags.mode, Some(Mode::Recording));
    }

    #[test]
    fn try_consume_rejects_bad_mode() {
        let args = vec!["--mode".to_owned(), "bogus".to_owned()];
        let mut flags = SignFlags::default();
        let err = try_consume_sign_flag(&args, 0, &mut flags).expect_err("rejects");
        assert_eq!(format!("{err:?}"), format!("{:?}", ExitCode::from(2)));
    }

    #[test]
    fn try_consume_parses_seed() {
        let args = vec!["--seed".to_owned(), "42".to_owned()];
        let mut flags = SignFlags::default();
        let consumed = try_consume_sign_flag(&args, 0, &mut flags).expect("ok");
        assert_eq!(consumed, Some(2));
        assert_eq!(flags.seed, Some(42));
    }

    #[test]
    fn try_consume_rejects_bad_seed() {
        let args = vec!["--seed".to_owned(), "not-a-number".to_owned()];
        let mut flags = SignFlags::default();
        let err = try_consume_sign_flag(&args, 0, &mut flags).expect_err("rejects");
        assert_eq!(format!("{err:?}"), format!("{:?}", ExitCode::from(2)));
    }

    // --- allowlist tests ---

    #[test]
    fn parse_array_form() {
        let json = r#"["aaa=", "bbb="]"#;
        let out = parse_allowlist_json(json).expect("parses");
        assert_eq!(out, vec!["aaa=".to_owned(), "bbb=".to_owned()]);
    }

    #[test]
    fn parse_object_form() {
        let json = r#"{"keys": ["xyz="]}"#;
        let out = parse_allowlist_json(json).expect("parses");
        assert_eq!(out, vec!["xyz=".to_owned()]);
    }

    #[test]
    fn parse_rejects_non_string_entry() {
        let json = r#"["aaa=", 42]"#;
        let err = parse_allowlist_json(json).expect_err("rejects");
        assert!(err.contains("[1]"), "got: {err}");
    }

    #[test]
    fn parse_rejects_empty_string_entry() {
        let json = r#"["aaa=", ""]"#;
        let err = parse_allowlist_json(json).expect_err("rejects");
        assert!(err.contains("[1]"), "got: {err}");
    }

    #[test]
    fn parse_rejects_bare_string() {
        let json = r#""just a string""#;
        let err = parse_allowlist_json(json).expect_err("rejects");
        assert!(err.contains("array"), "got: {err}");
    }

    #[test]
    fn pubkey_match_is_exact() {
        let allow = vec!["alpha".to_owned(), "beta".to_owned()];
        assert!(pubkey_in_allowlist("alpha", &allow));
        assert!(pubkey_in_allowlist("beta", &allow));
        assert!(!pubkey_in_allowlist("gamma", &allow));
        assert!(!pubkey_in_allowlist("ALPHA", &allow), "case-sensitive");
    }

    // Note: `load_trusted_keys(None)` env-var behavior is covered by
    // the round-trip integration tests in
    // `tests/receipts_round_trip.rs`, which control the env via
    // `Command::env`/`env_remove` per child process and avoid
    // mutating the test process's env at all. The Edition-2024
    // `unsafe` requirement on `set_var`/`remove_var` is forbidden in
    // this crate (`#![forbid(unsafe_code)]`).
}
