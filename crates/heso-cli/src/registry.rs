//! `heso registry <subcommand>` — thin dispatcher to the ecosystem and
//! search verbs. The four top-level verbs (`publish`, `pull`, `list`,
//! `search`) remain in place during the partial Path B phase; this
//! group provides the consolidated entry point.

use std::process::ExitCode;

use crate::{ecosystem, search};

/// `heso registry <subcommand> [args...]` — dispatch to the right
/// ecosystem or search verb.
pub async fn cmd_registry(args: &[String]) -> ExitCode {
    let Some(sub) = args.first() else {
        print_usage();
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    match sub.as_str() {
        "-h" | "--help" => {
            print_usage();
            ExitCode::SUCCESS
        }
        "publish" => ecosystem::cmd_publish(rest).await,
        "pull" => ecosystem::cmd_pull(rest).await,
        "list" => ecosystem::cmd_list(rest).await,
        "search" => search::cmd_search(rest).await,
        other => {
            eprintln!("registry: unknown subcommand `{other}`");
            print_usage();
            ExitCode::from(2)
        }
    }
}

fn print_usage() {
    eprintln!("usage: heso registry <publish|pull|list|search> [args...]");
    eprintln!();
    eprintln!("Subcommands:");
    eprintln!("  publish <plat-file> -d \"...\"   Upload a stamped plat to the public registry.");
    eprintln!("  pull    <plat-hash> [-o PATH]  Download a published plat by its BLAKE3 hash.");
    eprintln!("  list    [-q ...]               Browse the public plat registry.");
    eprintln!("  search  <query>                Multi-source web search (DDG + Wikipedia).");
}
