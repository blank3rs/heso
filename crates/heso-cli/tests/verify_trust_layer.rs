//! Integration coverage for the `heso verify` trust layer (Workstream
//! D.4): TOFU pinning, the precedence ladder (`--signer-key` >
//! `--expect-signer` > `--trusted-keys` > TOFU), the self-signed-forgery
//! attack (§8.1), compatibility contracts (§8.4), and domain separation
//! (§8.5).
//!
//! Plats are built in-process so the tests control the signing key, then
//! `heso verify` runs as a subprocess with `current_dir` set to a
//! per-test tempdir — so the TOFU pin store lands in
//! `<tempdir>/heso-local-data/known_signers.json`, isolated from the
//! workspace and from other tests.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use heso_core::IdentityKey;
use serde_json::json;
use tempfile::TempDir;

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn run_in(cwd: &Path, args: &[&str]) -> Output {
    Command::new(heso_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn heso")
}

fn write_json(path: &Path, value: &serde_json::Value) {
    std::fs::write(path, serde_json::to_vec_pretty(value).expect("serialize")).expect("write json");
}

/// A minimal plat body (no `plat_hash`, no `sig`) carrying an explicit
/// `lineage` — the TOFU pin key — and a `title` an attacker can mutate.
fn body_with_lineage(lineage: &str, title: &str) -> serde_json::Value {
    json!({
        "input_url": "https://example.com/",
        "url": "https://example.com/",
        "title": title,
        "description": "",
        "tree": [],
        "actions": [],
        "lineage": lineage,
    })
}

/// Sign `body` inline with `key` exactly as a producer does — this
/// stamps `plat_hash` over the hash region (which keeps `lineage`) and
/// inserts the `sig`.
fn sign(key: &IdentityKey, body: serde_json::Value) -> serde_json::Value {
    heso_engine_fetch::plat::sign_inline(key, body)
}

fn stdout_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

// ============================================================================
// §8.1 — the self-signed-forgery attack (the headline)
// ============================================================================

#[test]
fn forgery_first_use_pins_then_mismatch_fails_loud() {
    let dir = TempDir::new().unwrap();
    let lineage = "site:forgery0001";

    // Honest producer signs and ships a plat.
    let honest = IdentityKey::generate();
    let honest_plat = sign(&honest, body_with_lineage(lineage, "honest"));
    let honest_path = dir.path().join("honest.plat");
    write_json(&honest_path, &honest_plat);

    // First verify: first-use pin, exit 0, signer line printed.
    let first = run_in(dir.path(), &["verify", honest_path.to_str().unwrap()]);
    assert!(
        first.status.success(),
        "first-use verify must pass; stderr={}",
        stderr_of(&first)
    );
    let out = stdout_of(&first);
    assert!(
        out.contains("OK plat") && out.contains(&format!("signer {}", honest.fingerprint())),
        "first-use must print the signer fingerprint; got: {out}"
    );
    assert!(
        out.contains("(first-use, pinned now)"),
        "first contact must be flagged first-use; got: {out}"
    );

    // Attacker edits the body, recomputes plat_hash so the integrity gate
    // passes, and re-signs with their OWN fresh key — internally valid.
    let attacker = IdentityKey::generate();
    assert_ne!(
        attacker.fingerprint(),
        honest.fingerprint(),
        "the forged key must differ"
    );
    let forged = sign(&attacker, body_with_lineage(lineage, "hijacked"));
    let forged_path = dir.path().join("forged.plat");
    write_json(&forged_path, &forged);

    // Same lineage, pin exists → SIGNER MISMATCH, exit 1, never bare OK.
    let mismatch = run_in(dir.path(), &["verify", forged_path.to_str().unwrap()]);
    assert_eq!(
        mismatch.status.code(),
        Some(1),
        "a re-signed forgery on a pinned lineage must FAIL; stdout={} stderr={}",
        stdout_of(&mismatch),
        stderr_of(&mismatch)
    );
    assert!(
        stdout_of(&mismatch).contains("FAIL plat"),
        "stdout must say FAIL plat; got: {}",
        stdout_of(&mismatch)
    );
    assert!(
        stderr_of(&mismatch).contains("SIGNER MISMATCH"),
        "stderr must name the signer mismatch; got: {}",
        stderr_of(&mismatch)
    );
    assert!(
        stderr_of(&mismatch).contains("--accept-new-signer"),
        "the mismatch must hint at the re-pin escape hatch"
    );
}

#[test]
fn forgery_fresh_machine_first_use_but_out_of_band_pinning_defeats_it() {
    // On a fresh machine (no pin) the forgery passes first-use — TOFU's
    // documented blind spot. But out-of-band pinning fully defeats it.
    let dir = TempDir::new().unwrap();
    let lineage = "site:forgery0002";

    let honest = IdentityKey::generate();
    let attacker = IdentityKey::generate();
    let forged = sign(&attacker, body_with_lineage(lineage, "hijacked"));
    let forged_path = dir.path().join("forged.plat");
    write_json(&forged_path, &forged);

    // No pin on this fresh machine → the forgery passes first-use.
    let fresh = run_in(dir.path(), &["verify", forged_path.to_str().unwrap()]);
    assert!(
        fresh.status.success(),
        "first contact with no pin passes (TOFU blind spot); stderr={}",
        stderr_of(&fresh)
    );
    assert!(stdout_of(&fresh).contains("(first-use, pinned now)"));

    // --expect-signer <honest-fp> defeats the forgery (fresh tempdir so
    // the prior pin doesn't shadow the explicit check, though explicit
    // beats TOFU regardless).
    let dir2 = TempDir::new().unwrap();
    write_json(&dir2.path().join("forged.plat"), &forged);
    let expect = run_in(
        dir2.path(),
        &[
            "verify",
            "--expect-signer",
            &honest.fingerprint(),
            dir2.path().join("forged.plat").to_str().unwrap(),
        ],
    );
    assert_eq!(
        expect.status.code(),
        Some(1),
        "--expect-signer <honest> must FAIL the forgery; stdout={} stderr={}",
        stdout_of(&expect),
        stderr_of(&expect)
    );
    assert!(
        stderr_of(&expect).contains("EXPECTED SIGNER")
            && stderr_of(&expect).contains(&honest.fingerprint())
            && stderr_of(&expect).contains(&attacker.fingerprint()),
        "stderr must show expected vs got; got: {}",
        stderr_of(&expect)
    );

    // --signer-key <honest-pubkey> defeats the forgery too.
    let dir3 = TempDir::new().unwrap();
    write_json(&dir3.path().join("forged.plat"), &forged);
    let keyfile = dir3.path().join("honest.pub");
    std::fs::write(&keyfile, honest.public_key_b64()).unwrap();
    let signer_key = run_in(
        dir3.path(),
        &[
            "verify",
            "--signer-key",
            keyfile.to_str().unwrap(),
            dir3.path().join("forged.plat").to_str().unwrap(),
        ],
    );
    assert_eq!(
        signer_key.status.code(),
        Some(1),
        "--signer-key <honest> must FAIL the forgery; stdout={} stderr={}",
        stdout_of(&signer_key),
        stderr_of(&signer_key)
    );
    assert!(stderr_of(&signer_key).contains("EXPECTED SIGNER"));
}

#[test]
fn tamper_without_resigning_fails_on_signature() {
    // Edit the body but leave the old plat_hash + sig. The hash region
    // recompute disagrees → integrity FAIL, exit 1, never OK.
    let dir = TempDir::new().unwrap();
    let honest = IdentityKey::generate();
    let mut plat = sign(&honest, body_with_lineage("site:tamper003", "honest"));
    plat["title"] = json!("hijacked"); // mutate after signing
    let path = dir.path().join("tampered.plat");
    write_json(&path, &plat);

    let out = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "tamper-without-resign must FAIL; stdout={} stderr={}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stdout_of(&out).contains("FAIL plat"));
}

#[test]
fn tamper_and_rehash_but_keep_old_sig_fails_on_signature() {
    // Edit the body AND recompute plat_hash (integrity passes), but keep
    // the now-stale signature. The signature no longer covers the new
    // signing input → INVALID, exit 1.
    let dir = TempDir::new().unwrap();
    let honest = IdentityKey::generate();
    let mut plat = sign(&honest, body_with_lineage("site:tamper004", "honest"));
    plat["title"] = json!("hijacked");
    let obj = plat.as_object_mut().unwrap();
    obj.remove("plat_hash");
    // `plat_hash` hashes the hash region, which strips both `plat_hash` and
    // `sig`, so recomputing over the object that still carries `sig` yields
    // the same digest the integrity gate expects — integrity passes, and the
    // failure surfaces purely on the stale signature below.
    let rehashed = heso_engine_fetch::plat_hash(&serde_json::Value::Object(obj.clone()));
    obj.insert("plat_hash".to_owned(), json!(rehashed));
    let path = dir.path().join("rehashed.plat");
    write_json(&path, &plat);

    let out = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout_of(&out).contains("FAIL plat"));
    assert!(
        stderr_of(&out).contains("inline signature does not verify"),
        "stderr must name the signature failure; got: {}",
        stderr_of(&out)
    );
}

// ============================================================================
// §8.3 — TOFU + precedence (integration; unit tests live in tofu.rs)
// ============================================================================

#[test]
fn second_verify_of_same_signer_is_quiet_pinned() {
    let dir = TempDir::new().unwrap();
    let honest = IdentityKey::generate();
    let plat = sign(&honest, body_with_lineage("site:repeat005", "honest"));
    let path = dir.path().join("p.plat");
    write_json(&path, &plat);

    let first = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert!(first.status.success());
    assert!(stdout_of(&first).contains("(first-use, pinned now)"));

    let second = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert!(second.status.success());
    assert!(
        stdout_of(&second).contains("(pinned)") && !stdout_of(&second).contains("first-use"),
        "second verify of the same signer must be a quiet (pinned); got: {}",
        stdout_of(&second)
    );
}

#[test]
fn accept_new_signer_repins_a_mismatch() {
    let dir = TempDir::new().unwrap();
    let lineage = "site:repin006";
    let honest = IdentityKey::generate();
    let attacker = IdentityKey::generate();

    let honest_path = dir.path().join("honest.plat");
    write_json(&honest_path, &sign(&honest, body_with_lineage(lineage, "v1")));
    assert!(run_in(dir.path(), &["verify", honest_path.to_str().unwrap()]).status.success());

    let forged_path = dir.path().join("forged.plat");
    write_json(&forged_path, &sign(&attacker, body_with_lineage(lineage, "v2")));
    // Without the flag: mismatch fail.
    assert_eq!(
        run_in(dir.path(), &["verify", forged_path.to_str().unwrap()])
            .status
            .code(),
        Some(1)
    );
    // With --accept-new-signer: re-pin succeeds.
    let repin = run_in(
        dir.path(),
        &["verify", "--accept-new-signer", forged_path.to_str().unwrap()],
    );
    assert!(
        repin.status.success(),
        "--accept-new-signer must re-pin; stderr={}",
        stderr_of(&repin)
    );
    // The new signer is now the pin: re-verifying the forged plat is quiet.
    let after = run_in(dir.path(), &["verify", forged_path.to_str().unwrap()]);
    assert!(after.status.success());
    assert!(stdout_of(&after).contains("(pinned)"));
    // And the original honest plat now MISMATCHES the new pin.
    assert_eq!(
        run_in(dir.path(), &["verify", honest_path.to_str().unwrap()])
            .status
            .code(),
        Some(1),
        "the original signer no longer matches the re-pinned signer"
    );
}

#[test]
fn precedence_signer_key_beats_expect_signer_and_tofu() {
    // --signer-key is strongest. Supply a mismatching --signer-key
    // alongside a MATCHING --expect-signer; the signer-key check wins and
    // fails, proving precedence (1) > (2).
    let dir = TempDir::new().unwrap();
    let honest = IdentityKey::generate();
    let other = IdentityKey::generate();
    let plat = sign(&honest, body_with_lineage("site:prec007", "x"));
    let path = dir.path().join("p.plat");
    write_json(&path, &plat);

    let keyfile = dir.path().join("other.pub");
    std::fs::write(&keyfile, other.public_key_b64()).unwrap();

    let out = run_in(
        dir.path(),
        &[
            "verify",
            "--signer-key",
            keyfile.to_str().unwrap(),
            "--expect-signer",
            &honest.fingerprint(), // would PASS on its own
            path.to_str().unwrap(),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "--signer-key (mismatch) must override a matching --expect-signer; stdout={} stderr={}",
        stdout_of(&out),
        stderr_of(&out)
    );
}

#[test]
fn precedence_expect_signer_beats_trusted_keys_and_tofu() {
    // A MATCHING --expect-signer plus an allowlist that EXCLUDES the
    // signer: --expect-signer (2) wins over --trusted-keys (3), so it
    // passes (trusted), and no TOFU pin is consulted.
    let dir = TempDir::new().unwrap();
    let honest = IdentityKey::generate();
    let plat = sign(&honest, body_with_lineage("site:prec008", "x"));
    let path = dir.path().join("p.plat");
    write_json(&path, &plat);

    // Allowlist with a different key — would FAIL allowlist on its own.
    let other = IdentityKey::generate();
    let allow = dir.path().join("allow.json");
    write_json(&allow, &json!([other.public_key_b64()]));

    let out = run_in(
        dir.path(),
        &[
            "verify",
            "--expect-signer",
            &honest.fingerprint(),
            "--trusted-keys",
            allow.to_str().unwrap(),
            path.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "--expect-signer (match) must win over an excluding allowlist; stdout={} stderr={}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stdout_of(&out).contains("(trusted)"));

    // The FAIL direction: a MISMATCHING --expect-signer plus an allowlist
    // that DOES include the real signer must still fail. If (2) did not gate
    // before (3), the matching allowlist would short-circuit to a pass — so
    // this proves --expect-signer truly precedes --trusted-keys both ways.
    let allow_inclusive = dir.path().join("allow_inclusive.json");
    write_json(&allow_inclusive, &json!([honest.public_key_b64()]));
    let fail = run_in(
        dir.path(),
        &[
            "verify",
            "--expect-signer",
            &other.fingerprint(), // wrong fp — must FAIL
            "--trusted-keys",
            allow_inclusive.to_str().unwrap(), // would PASS on its own
            path.to_str().unwrap(),
        ],
    );
    assert_eq!(
        fail.status.code(),
        Some(1),
        "a mismatching --expect-signer must FAIL even with an inclusive allowlist; \
         stdout={} stderr={}",
        stdout_of(&fail),
        stderr_of(&fail)
    );
    assert!(
        stderr_of(&fail).contains("EXPECTED SIGNER"),
        "the failure must be the expect-signer check, not the allowlist; got: {}",
        stderr_of(&fail)
    );
}

#[test]
fn trusted_keys_allowlist_gates_the_plat_branch() {
    // The allowlist (3) applies to plats now, not just receipts.
    let dir = TempDir::new().unwrap();
    let honest = IdentityKey::generate();
    let plat = sign(&honest, body_with_lineage("site:allow009", "x"));
    let path = dir.path().join("p.plat");
    write_json(&path, &plat);

    // In the allowlist → trusted.
    let allow_ok = dir.path().join("allow_ok.json");
    write_json(&allow_ok, &json!([honest.public_key_b64()]));
    let ok = run_in(
        dir.path(),
        &["verify", "--trusted-keys", allow_ok.to_str().unwrap(), path.to_str().unwrap()],
    );
    assert!(ok.status.success(), "stderr={}", stderr_of(&ok));
    assert!(stdout_of(&ok).contains("(trusted)"));

    // Not in the allowlist → fail.
    let allow_no = dir.path().join("allow_no.json");
    write_json(&allow_no, &json!([IdentityKey::generate().public_key_b64()]));
    let no = run_in(
        dir.path(),
        &["verify", "--trusted-keys", allow_no.to_str().unwrap(), path.to_str().unwrap()],
    );
    assert_eq!(no.status.code(), Some(1));
    assert!(stderr_of(&no).contains("not in the trusted-keys allowlist"));
}

#[test]
fn empty_trusted_keys_is_fail_closed_on_the_plat_branch() {
    let dir = TempDir::new().unwrap();
    let honest = IdentityKey::generate();
    let plat = sign(&honest, body_with_lineage("site:empty010", "x"));
    let path = dir.path().join("p.plat");
    write_json(&path, &plat);

    let empty = dir.path().join("empty.json");
    write_json(&empty, &json!([]));
    let out = run_in(
        dir.path(),
        &["verify", "--trusted-keys", empty.to_str().unwrap(), path.to_str().unwrap()],
    );
    assert_eq!(
        out.status.code(),
        Some(1),
        "an explicitly-supplied empty allowlist is fail-closed; stdout={} stderr={}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stderr_of(&out).contains("zero entries"));
}

// ============================================================================
// §8.4 — compatibility / output contracts
// ============================================================================

#[test]
fn unsigned_legacy_plat_warns_but_passes_integrity() {
    // A plat with no `sig` (legacy / --no-sign) verifies on integrity and
    // exits 0, with a stderr WARNING that it's unsigned. The stdout line
    // still contains OK for grep-compat.
    let dir = TempDir::new().unwrap();
    let mut body = body_with_lineage("site:legacy011", "x");
    body.as_object_mut().unwrap().remove("lineage"); // bare legacy shape
    let h = heso_engine_fetch::plat_hash(&body);
    body.as_object_mut().unwrap().insert("plat_hash".into(), json!(h));
    let path = dir.path().join("legacy.plat");
    write_json(&path, &body);

    let out = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert!(out.status.success(), "stderr={}", stderr_of(&out));
    assert!(
        stdout_of(&out).contains("OK plat"),
        "unsigned plat still prints OK (grep-compat); got: {}",
        stdout_of(&out)
    );
    assert!(
        stderr_of(&out).contains("unsigned") && stderr_of(&out).contains("authenticity unknown"),
        "must warn that the plat is unsigned; got: {}",
        stderr_of(&out)
    );
    // No pin store is created for an unsigned plat.
    assert!(
        !dir
            .path()
            .join("heso-local-data")
            .join("known_signers.json")
            .exists(),
        "an unsigned plat must not write a TOFU pin"
    );
}

#[test]
fn signed_plat_verify_output_contains_ok_for_grep_compat() {
    let dir = TempDir::new().unwrap();
    let honest = IdentityKey::generate();
    let plat = sign(&honest, body_with_lineage("site:grep012", "x"));
    let path = dir.path().join("p.plat");
    write_json(&path, &plat);

    let out = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert!(out.status.success());
    // Single stdout line, contains OK, and the signer fingerprint.
    let stdout = stdout_of(&out);
    let line_count = stdout.lines().filter(|l| !l.trim().is_empty()).count();
    assert_eq!(line_count, 1, "exactly one stdout line; got: {stdout:?}");
    assert!(stdout.contains("OK plat"));
    assert!(stdout.contains(&format!("signer {}", honest.fingerprint())));
}

#[test]
fn seal_strips_inline_sig_and_produces_a_valid_envelope() {
    // §8.4 + §6: `heso seal` on an inline-signed plat strips `sig` and the
    // envelope still verifies (now via the sealed-plat trust path).
    let dir = TempDir::new().unwrap();
    // The sealing identity must exist at the default path.
    assert!(run_in(dir.path(), &["identity", "init"]).status.success());

    let signer = IdentityKey::generate();
    let plat = sign(&signer, body_with_lineage("site:seal013", "x"));
    assert!(plat.get("sig").is_some(), "input is inline-signed");
    let plat_path = dir.path().join("signed.plat");
    write_json(&plat_path, &plat);

    let seal = run_in(dir.path(), &["seal", plat_path.to_str().unwrap()]);
    assert!(seal.status.success(), "seal failed: {}", stderr_of(&seal));
    assert!(
        stderr_of(&seal).contains("stripped the inline `sig`"),
        "seal must note it stripped the inline sig; got: {}",
        stderr_of(&seal)
    );
    let envelope: serde_json::Value = serde_json::from_slice(&seal.stdout).expect("seal is JSON");
    assert!(
        envelope["content"].get("sig").is_none(),
        "the sealed envelope's content must not carry the stripped inline sig"
    );

    // The envelope verifies through the sealed-plat branch.
    let sealed_path = dir.path().join("sealed.json");
    std::fs::write(&sealed_path, &seal.stdout).unwrap();
    let verify = run_in(dir.path(), &["verify", sealed_path.to_str().unwrap()]);
    assert!(
        verify.status.success(),
        "sealed envelope must verify; stdout={} stderr={}",
        stdout_of(&verify),
        stderr_of(&verify)
    );
    assert!(stdout_of(&verify).contains("OK sealed-plat"));
}

#[test]
fn sealed_plat_branch_prints_a_fingerprint_and_consults_trust() {
    // The sealed-plat branch now resolves trust like the inline branch:
    // a fingerprint is printed, and --expect-signer can fail it.
    let dir = TempDir::new().unwrap();
    let key = IdentityKey::generate();
    // Build a sealed envelope in-process so we know the signer key.
    let body = body_with_lineage("site:sealed014", "x");
    let h = heso_engine_fetch::plat_hash(&body);
    let mut content = body;
    content.as_object_mut().unwrap().insert("plat_hash".into(), json!(h));
    let sealed = heso_engine_fetch::plat_seal(&key, content);
    let sealed_value = serde_json::to_value(&sealed).unwrap();
    let path = dir.path().join("sealed.json");
    write_json(&path, &sealed_value);

    // Default: first-use pin, prints the fingerprint.
    let ok = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert!(ok.status.success(), "stderr={}", stderr_of(&ok));
    assert!(
        stdout_of(&ok).contains(&format!("signer {}", key.fingerprint())),
        "sealed-plat must print a signer fingerprint, not the full pubkey; got: {}",
        stdout_of(&ok)
    );

    // --expect-signer with the wrong fp fails the sealed plat.
    let dir2 = TempDir::new().unwrap();
    write_json(&dir2.path().join("sealed.json"), &sealed_value);
    let wrong = run_in(
        dir2.path(),
        &[
            "verify",
            "--expect-signer",
            "heso:00000000000000000000000000000000",
            dir2.path().join("sealed.json").to_str().unwrap(),
        ],
    );
    assert_eq!(
        wrong.status.code(),
        Some(1),
        "a wrong --expect-signer must fail the sealed plat; stdout={} stderr={}",
        stdout_of(&wrong),
        stderr_of(&wrong)
    );
    assert!(stderr_of(&wrong).contains("EXPECTED SIGNER"));
}

// ============================================================================
// §8.5 — domain separation / transplant rejection (through the CLI)
// ============================================================================

#[test]
fn wrong_inline_alg_exits_two() {
    // An inline `sig` carrying an unknown `alg` is refused
    // algorithm-before-signature with exit 2 (wrong-algorithm), not 1.
    let dir = TempDir::new().unwrap();
    let key = IdentityKey::generate();
    let mut plat = sign(&key, body_with_lineage("site:alg015", "x"));
    plat["sig"]["alg"] = json!("heso-plat-sig/v999+ed25519");
    let path = dir.path().join("p.plat");
    write_json(&path, &plat);

    let out = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(2),
        "an unknown inline alg is a wrong-algorithm (exit 2); stdout={} stderr={}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stdout_of(&out).contains("FAIL plat"));
    assert!(stderr_of(&out).contains("WRONG ALGORITHM"));
}

#[test]
fn seal_domain_signature_transplanted_into_inline_sig_is_rejected() {
    // §8.5: a signature minted under the `seal` envelope's domain, dropped
    // into an inline `sig` slot (with the inline alg tag), must be
    // rejected by `verify` — distinct domains make the two signatures
    // non-transplantable.
    let dir = TempDir::new().unwrap();
    let key = IdentityKey::generate();

    // Build the body + plat_hash, then seal it to get a seal-domain sig.
    let body = body_with_lineage("site:xplant016", "x");
    let h = heso_engine_fetch::plat_hash(&body);
    let mut content = body.clone();
    content.as_object_mut().unwrap().insert("plat_hash".into(), json!(h));
    let sealed = heso_engine_fetch::plat_seal(&key, content.clone());

    // Transplant the seal-domain signature into an inline `sig` on the
    // same (bare) body.
    let mut forged = content;
    forged.as_object_mut().unwrap().insert(
        "sig".to_owned(),
        json!({
            "alg": heso_engine_fetch::plat::INLINE_SIG_ALG,
            "public_key": sealed.signature.public_key,
            "signature": sealed.signature.signature,
        }),
    );
    let path = dir.path().join("xplant.plat");
    write_json(&path, &forged);

    let out = run_in(dir.path(), &["verify", path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "a transplanted seal-domain signature must fail the inline check; stdout={} stderr={}",
        stdout_of(&out),
        stderr_of(&out)
    );
    assert!(stdout_of(&out).contains("FAIL plat"));
    assert!(stderr_of(&out).contains("inline signature does not verify"));
}
