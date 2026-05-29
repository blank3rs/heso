//! Integration tests for default tamper-evidence on CLI-produced plats
//! (Workstream D.3): every producer (`open`, `read`, `stamp`, `run`)
//! stamps a `lineage` pin key and an inline `sig` by default, and
//! `--no-sign` re-emits today's bare plat byte-for-byte.
//!
//! These run the real `heso` binary as a subprocess against hermetic
//! `data:` URLs (no network) and isolate the default identity key in a
//! per-test tempdir so a fresh signing key is generated in-band without
//! racing the shared workspace key.

use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

/// Start a hermetic localhost server serving a stable page at `/page` —
/// the `data:` URL plan executor can't be replayed through `stamp`/`run`,
/// so the cassette-replay tests need a real (but local) HTTP origin.
async fn fixture_server() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(wm_path("/page"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/html; charset=utf-8")
                .set_body_string("<html><head><title>sig fixture</title></head><body><h1>hi</h1></body></html>"),
        )
        .mount(&server)
        .await;
    server
}

/// Run `heso <args>` with `cwd` as the working directory, so the default
/// `heso-local-data/identity.key` lands inside the test's tempdir.
fn run_in(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(heso_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn heso")
}

fn parse_ok(out: &std::process::Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "heso exited non-zero: stderr={}\nstdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    serde_json::from_slice(&out.stdout).expect("stdout is JSON")
}

const PAGE: &str = "data:text/html,<h1>hi</h1>";

/// A plat captured from the `heso open <PAGE>` binary BEFORE default
/// signing landed — the regression anchor. `heso open --no-sign <PAGE>`
/// must reproduce these exact bytes (pretty-printed JSON + trailing
/// newline from `println!`), proving the bare path is byte-identical to a
/// pre-signing plat.
const PRE_D_BARE_OPEN_GOLDEN: &str = "{\n  \"actions\": [],\n  \"console_errors_count\": 0,\n  \"description\": null,\n  \"failed_scripts\": [],\n  \"http_status\": 200,\n  \"input_url\": \"data:text/html,<h1>hi</h1>\",\n  \"metadata\": {},\n  \"partial\": false,\n  \"partial_reason\": \"ok\",\n  \"plat_hash\": \"8133f737db272370d22dfe4ab10dd05015185f5927c6d215538975e1bc75dca7\",\n  \"seed\": 0,\n  \"title\": \"\",\n  \"tree\": {\n    \"description\": null,\n    \"root\": {\n      \"byte_count\": 0,\n      \"child_count\": 1,\n      \"children\": [\n        {\n          \"byte_count\": 0,\n          \"child_count\": 0,\n          \"children\": [],\n          \"heading\": \"hi\",\n          \"intro\": \"\",\n          \"level\": 1,\n          \"path\": \"/hi\",\n          \"slug\": \"hi\",\n          \"summary\": \"hi\"\n        }\n      ],\n      \"heading\": null,\n      \"intro\": \"\",\n      \"level\": 0,\n      \"path\": \"/\",\n      \"slug\": \"\",\n      \"summary\": \"\"\n    },\n    \"title\": \"\",\n    \"url\": \"data:text/html,<h1>hi</h1>\"\n  },\n  \"url\": \"data:text/html,<h1>hi</h1>\"\n}\n";

#[test]
fn open_signs_by_default_with_sig_and_lineage() {
    let dir = TempDir::new().unwrap();
    let plat = parse_ok(&run_in(dir.path(), &["open", PAGE]));

    let sig = plat["sig"].as_object().expect("default open carries a `sig`");
    assert_eq!(
        sig["alg"].as_str(),
        Some("heso-plat-sig/v1+ed25519"),
        "inline sig uses the v1 ed25519 algorithm tag"
    );
    assert_eq!(
        sig["public_key"].as_str().map(str::len),
        Some(44),
        "public_key is base64 of 32 bytes"
    );
    assert_eq!(
        sig["signature"].as_str().map(str::len),
        Some(88),
        "signature is base64 of 64 bytes"
    );

    let lineage = plat["lineage"].as_str().expect("default open carries `lineage`");
    let hex = lineage.strip_prefix("site:").expect("lineage is `site:`-prefixed");
    assert_eq!(hex.len(), 32, "lineage is blake3[..16] => 32 hex chars");

    // A default signed plat still verifies through the verify verb
    // (integrity at minimum; the trust layer is exercised elsewhere).
    let plat_path = dir.path().join("signed.plat");
    std::fs::write(&plat_path, serde_json::to_vec_pretty(&plat).unwrap()).unwrap();
    let verify = run_in(dir.path(), &["verify", plat_path.to_str().unwrap()]);
    assert!(
        verify.status.success(),
        "verify of a default signed plat must pass: {}",
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(
        String::from_utf8_lossy(&verify.stdout).contains("OK"),
        "verify output must contain OK for grep-compat"
    );
}

#[test]
fn no_sign_open_is_byte_identical_to_a_bare_plat() {
    let dir = TempDir::new().unwrap();
    let bare = run_in(dir.path(), &["open", "--no-sign", PAGE]);
    let plat = parse_ok(&bare);

    assert!(
        plat.get("sig").is_none(),
        "--no-sign must omit `sig` entirely (absent, not null)"
    );
    assert!(
        plat.get("lineage").is_none(),
        "--no-sign without an explicit --lineage must omit `lineage` so the \
         output is byte-identical to a pre-signing bare plat"
    );

    // The regression anchor: --no-sign emits exactly the historical bare
    // shape. The required fields are present; no signing artifacts leak.
    assert!(plat["plat_hash"].as_str().is_some_and(|h| h.len() == 64));
    assert_eq!(plat["input_url"].as_str(), Some(PAGE));

    // A bare --no-sign run must NOT have created a signing identity — the
    // bare path never touches the key store.
    let key_path = dir.path().join("heso-local-data").join("identity.key");
    assert!(
        !key_path.exists(),
        "--no-sign must not generate a default identity"
    );

    // Byte-for-byte regression anchor against a plat captured from the
    // pre-signing binary. On Windows the OS line ending is the only
    // permitted difference, so normalize CRLF before comparing.
    let stdout = String::from_utf8(bare.stdout).expect("stdout is utf8");
    let normalized = stdout.replace("\r\n", "\n");
    assert_eq!(
        normalized, PRE_D_BARE_OPEN_GOLDEN,
        "--no-sign output must be byte-identical to the pre-signing plat"
    );
}

#[test]
fn no_sign_with_explicit_lineage_carries_the_label_without_a_sig() {
    let dir = TempDir::new().unwrap();
    let plat = parse_ok(&run_in(
        dir.path(),
        &["open", "--no-sign", "--lineage", "site:crawl-batch-1", PAGE],
    ));
    assert!(plat.get("sig").is_none(), "still unsigned");
    assert_eq!(
        plat["lineage"].as_str(),
        Some("site:crawl-batch-1"),
        "an explicit --lineage under --no-sign is honored"
    );
}

#[test]
fn read_signs_by_default() {
    let dir = TempDir::new().unwrap();
    let plat = parse_ok(&run_in(dir.path(), &["read", PAGE]));
    assert_eq!(
        plat["sig"]["alg"].as_str(),
        Some("heso-plat-sig/v1+ed25519")
    );
    assert!(plat["lineage"].as_str().is_some_and(|l| l.starts_with("site:")));
}

#[test]
fn read_no_sign_is_bare() {
    let dir = TempDir::new().unwrap();
    let plat = parse_ok(&run_in(dir.path(), &["read", "--no-sign", PAGE]));
    assert!(plat.get("sig").is_none());
    assert!(plat.get("lineage").is_none());
}

#[test]
fn open_and_no_sign_open_differ_only_by_sig_and_lineage() {
    // Same page, same seed: the signed plat is the bare plat plus exactly
    // `sig` + `lineage` (and the lineage-shifted plat_hash). This pins the
    // "additive, surgical" shape change.
    let dir = TempDir::new().unwrap();
    let mut signed = parse_ok(&run_in(dir.path(), &["open", PAGE]));
    let bare = parse_ok(&run_in(dir.path(), &["open", "--no-sign", PAGE]));

    let s = signed.as_object_mut().unwrap();
    let removed_sig = s.remove("sig").is_some();
    let removed_lineage = s.remove("lineage").is_some();
    assert!(removed_sig && removed_lineage);
    // After stripping sig+lineage, the only remaining difference is the
    // plat_hash (lineage is in the hash region). Overwrite it with the
    // bare hash and assert the rest is byte-identical.
    s.insert(
        "plat_hash".to_owned(),
        bare["plat_hash"].clone(),
    );
    assert_eq!(
        signed, bare,
        "signed plat minus {{sig, lineage}} (and its lineage-shifted hash) \
         equals the bare plat"
    );
}

/// Stamp a signed plat against `server`'s `/page` route, returning the
/// parsed plat. The default key lands in `dir`.
fn stamp_signed(server: &MockServer, dir: &Path) -> serde_json::Value {
    let url = format!("{}/page", server.uri());
    let plan = serde_json::json!([{ "verb": "open", "url": url }]);
    let plan_path = dir.join("plan.json");
    std::fs::write(&plan_path, serde_json::to_vec(&plan).unwrap()).unwrap();
    let stamped = parse_ok(&run_in(
        dir,
        &["stamp", "--seed", "0", plan_path.to_str().unwrap()],
    ));
    assert!(stamped.get("sig").is_some(), "stamp signs by default");
    assert!(
        stamped.get("cassette").is_some(),
        "stamp records a cassette for replay"
    );
    stamped
}

/// Mutate `title` and re-stamp `plat_hash` so the BLAKE3 integrity gate
/// passes, but leave the now-stale `sig` in place. `sig` is stripped from
/// the hash region, so recomputing via the same engine crate the binary
/// uses yields the value the integrity gate accepts.
fn forge_tamper_rehash_keep_sig(mut plat: serde_json::Value) -> serde_json::Value {
    plat["title"] = serde_json::Value::String("tampered title".into());
    let obj = plat.as_object_mut().unwrap();
    obj.remove("plat_hash");
    let recomputed = heso_engine_fetch::plat_hash(&serde_json::Value::Object(obj.clone()));
    obj.insert("plat_hash".to_owned(), serde_json::Value::String(recomputed));
    plat
}

#[tokio::test]
async fn run_refuses_an_input_whose_sig_no_longer_verifies() {
    // The launder-through-replay defense: take a signed plat, mutate a
    // content field AND recompute its plat_hash so the integrity gate
    // would pass — but leave the now-stale `sig` in place. `run` must
    // verify the inline signature and refuse, never minting a fresh plat
    // that launders the tamper.
    let dir = TempDir::new().unwrap();
    let server = fixture_server().await;
    let stamped = stamp_signed(&server, dir.path());
    drop(server);

    let forged = forge_tamper_rehash_keep_sig(stamped);
    let forged_path = dir.path().join("forged.plat");
    std::fs::write(&forged_path, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();

    let out = run_in(dir.path(), &["run", "--seed", "0", forged_path.to_str().unwrap()]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "run must refuse a tampered-but-rehashed plat whose inline sig is \
         now invalid\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid inline signature"),
        "stderr must name the signature failure; got: {stderr}"
    );
}

#[tokio::test]
async fn run_no_verify_input_skips_the_inline_signature_gate() {
    // The escape hatch still works: `--no-verify-input` skips BOTH the
    // hash gate and the inline-signature gate.
    let dir = TempDir::new().unwrap();
    let server = fixture_server().await;
    let stamped = stamp_signed(&server, dir.path());
    drop(server);

    let forged = forge_tamper_rehash_keep_sig(stamped);
    let forged_path = dir.path().join("forged.plat");
    std::fs::write(&forged_path, serde_json::to_vec_pretty(&forged).unwrap()).unwrap();

    let out = run_in(
        dir.path(),
        &[
            "run",
            "--no-verify-input",
            "--seed",
            "0",
            forged_path.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "--no-verify-input must replay a tampered plat\nstderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}
