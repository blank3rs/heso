//! Integration coverage for `heso registry <sub>` dispatch.
//!
//! Phase B adds the consolidated subcommand alongside the existing
//! top-level verbs. Tests assert the dispatcher reaches each handler;
//! the handlers' own behavior is covered by `tests/search.rs` and the
//! ecosystem tests.

use std::path::PathBuf;
use std::process::Command;

fn heso_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_heso"))
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(heso_bin())
        .args(args)
        .output()
        .expect("spawn heso")
}

#[test]
fn registry_unknown_subcommand_exits_two_with_usage() {
    let out = run(&["registry", "bogus"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unknown subcommand"),
        "expected usage on stderr, got: {stderr}"
    );
}

#[test]
fn registry_no_subcommand_exits_two_with_usage() {
    let out = run(&["registry"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("publish"));
    assert!(stderr.contains("pull"));
    assert!(stderr.contains("list"));
    assert!(stderr.contains("search"));
}

#[test]
fn registry_help_exits_zero_with_usage() {
    let out = run(&["registry", "--help"]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("publish"));
}

#[test]
fn registry_publish_missing_args_dispatches_to_ecosystem_publish() {
    // `publish` with no plat-file exits with the ecosystem-publish usage
    // error (exit 2). Confirms the dispatch reached the inner verb
    // rather than the `registry` fallback "unknown subcommand" branch.
    let out = run(&["registry", "publish"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unknown subcommand"),
        "should reach ecosystem dispatcher, got: {stderr}"
    );
}

#[test]
fn registry_pull_missing_args_dispatches_to_ecosystem_pull() {
    let out = run(&["registry", "pull"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("unknown subcommand"));
}

#[test]
fn registry_search_missing_args_dispatches_to_search_verb() {
    let out = run(&["registry", "search"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("unknown subcommand"));
}
