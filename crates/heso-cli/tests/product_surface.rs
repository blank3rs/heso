//! Public product-surface guardrails.
//!
//! These tests intentionally read README/package wrapper text. heso has
//! several distribution channels, so the boring metadata and command-surface
//! claims need a cheap tripwire whenever the CLI or release version moves.

use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const ROOT_README: &str = include_str!("../../../README.md");
const PYPROJECT: &str = include_str!("../../../pyproject.toml");
const PY_INIT: &str = include_str!("../../../python/heso/__init__.py");
const NPM_PACKAGE: &str = include_str!("../../../npm/heso/package.json");
const NPM_README: &str = include_str!("../../../npm/heso/README.md");
const NPM_TYPES: &str = include_str!("../../../npm/heso/index.d.ts");
const NPM_DARWIN_ARM64: &str = include_str!("../../../npm/platforms/darwin-arm64/package.json");
const NPM_DARWIN_X64: &str = include_str!("../../../npm/platforms/darwin-x64/package.json");
const NPM_LINUX_ARM64: &str = include_str!("../../../npm/platforms/linux-arm64/package.json");
const NPM_LINUX_X64: &str = include_str!("../../../npm/platforms/linux-x64/package.json");
const NPM_WIN32_X64: &str = include_str!("../../../npm/platforms/win32-x64/package.json");

fn heso_bin() -> &'static str {
    env!("CARGO_BIN_EXE_heso")
}

#[test]
fn help_and_version_flags_are_first_class() {
    for arg in ["--help", "-h", "help"] {
        let out = Command::new(heso_bin()).arg(arg).output().unwrap();
        assert!(out.status.success(), "{arg} should exit 0");
        let stdout = String::from_utf8(out.stdout).unwrap();
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stdout.contains("Subcommands:"), "{arg} should print help");
        assert!(
            !stderr.contains("unknown subcommand"),
            "{arg} must not be reported as an unknown command"
        );
        assert!(
            !stdout.contains("  heso navigate "),
            "navigate is an RPC method under `heso serve`, not a top-level CLI verb"
        );
    }

    for arg in ["--version", "-V", "version"] {
        let out = Command::new(heso_bin()).arg(arg).output().unwrap();
        assert!(out.status.success(), "{arg} should exit 0");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            format!("heso {VERSION}")
        );
    }
}

#[test]
fn release_versions_stay_in_lockstep() {
    assert!(PYPROJECT.contains(&format!("version = \"{VERSION}\"")));
    assert!(PY_INIT.contains(&format!("__version__ = \"{VERSION}\"")));
    assert!(NPM_PACKAGE.contains(&format!("\"version\": \"{VERSION}\"")));
    for package in [
        NPM_PACKAGE,
        NPM_DARWIN_ARM64,
        NPM_DARWIN_X64,
        NPM_LINUX_ARM64,
        NPM_LINUX_X64,
        NPM_WIN32_X64,
    ] {
        assert!(package.contains(&format!("\"version\": \"{VERSION}\"")));
    }
    for platform_dep in [
        "@ixla/heso-darwin-arm64",
        "@ixla/heso-darwin-x64",
        "@ixla/heso-linux-arm64",
        "@ixla/heso-linux-x64",
        "@ixla/heso-win32-x64",
    ] {
        assert!(NPM_PACKAGE.contains(&format!("\"{platform_dep}\": \"{VERSION}\"")));
    }
    assert!(ROOT_README.contains(&format!("Shipping `v{VERSION}`")));
}

#[test]
fn install_and_platform_claims_match_release_artifacts() {
    assert!(ROOT_README.contains("heso-cli-installer.sh"));
    assert!(ROOT_README.contains("heso-cli-installer.ps1"));
    assert!(!ROOT_README.contains("heso.zip"));
    assert!(ROOT_README.contains("Requires Rust 1.90"));
    assert!(!ROOT_README.contains("Rust 1.80"));

    assert!(PYPROJECT.contains("Operating System :: MacOS"));
    assert!(PYPROJECT.contains("Operating System :: POSIX :: Linux"));
    assert!(PYPROJECT.contains("Operating System :: Microsoft :: Windows"));
}

#[test]
fn readme_top_level_verbs_resolve_in_binary() {
    // Every `heso <verb>` reference in the public README MUST resolve
    // to a real top-level subcommand. Catches the README-vs-dispatcher
    // drift that lets a verb get advertised without being wired up.
    //
    // We extract `\`heso <word>` references, filter by the curated set
    // of top-level verbs the README documents, then call each one with
    // a bogus marker flag and assert the binary doesn't reply
    // "unknown subcommand". Compound flags / RPC-only methods are
    // explicitly excluded.
    let advertised: Vec<&str> = TOP_LEVEL_VERBS_IN_README.to_vec();
    // Belt: also make sure the README still mentions each of these
    // (so this list can't silently rot if the README is restructured).
    for v in &advertised {
        let needle = format!("`heso {v}");
        assert!(
            ROOT_README.contains(&needle),
            "README no longer mentions `{needle}` — update TOP_LEVEL_VERBS_IN_README \
             or restore the reference"
        );
    }

    for verb in advertised {
        let out = Command::new(heso_bin())
            .arg(verb)
            .arg("--definitely-not-a-real-flag-xyz")
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn `heso {verb}`: {e}"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            !stderr.contains("unknown subcommand"),
            "README advertises `heso {verb}` but the binary reports it as unknown.\n\
             stderr: {stderr}\nstdout: {stdout}"
        );
    }
}

/// Curated list of top-level verbs the public README references with
/// backticks (`\`heso open\``, `\`heso search\``, …). New entries
/// added to the README MUST be added here. The
/// `readme_top_level_verbs_resolve_in_binary` test asserts the
/// dispatcher can reach each one and that the README still mentions
/// each one — preventing both directions of drift.
const TOP_LEVEL_VERBS_IN_README: &[&str] = &[
    "batch", "click", "eval-dom", "fill", "identity", "open", "read", "replay", "run", "search",
    "serve", "stamp", "submit", "verify", "wait",
];

#[test]
fn wrapper_readmes_match_language_idioms_and_cli_semantics() {
    assert!(NPM_README.contains("10.1 MB"));
    assert!(NPM_README.contains("~77 ms"));
    assert!(NPM_README.contains("~28 ms"));
    assert!(NPM_README.contains("selectorExists"));
    assert!(NPM_README.contains("bestEffort"));
    assert!(!NPM_README.contains("selector_exists"));
    assert!(!NPM_README.contains("best_effort"));

    assert!(NPM_TYPES.contains("Pure observation: no engine, no network"));
    assert!(!NPM_TYPES.contains("re-execute a plan\n * and return"));
    assert!(PY_INIT.contains("``--inject-script`` is repeatable"));
    assert!(PY_INIT.contains("key == \"inject_script\""));
}
