//! # heso-cli
//!
//! The `heso` binary — the headless browser for the agent-relevant half of
//! the web. Native single Rust executable, no Chromium dep, no Node dep.
//! 8.1 MB stripped today (post-QuickJS bundling), single-file deploy
//! anywhere. See [ADR 0016] for the positioning rationale.
//!
//! Every subcommand below operates on the in/out scope from ADR 0016:
//! fetch, parse, JS execution (Phase 1A), forms, clicks, sessions, signed
//! receipts. No canvas, no WebGL, no video, no CSS layout — that's the bet.
//!
//! - `heso` — prints a banner.
//! - `heso fetch <url>` — HTTP GET via the native [`FetchEngine`], print
//!   `{ url, text }` JSON. Direct path — no planner, no trace runner. The
//!   simplest surface external tools (e.g. the Flue agent's `heso_fetch`
//!   tool) can call.
//! - `heso tree <url>` — Fetch + build the page tree (heading-derived
//!   sections). Print the full tree as JSON. Used by agents that want to
//!   cache the tree once and then `ls` / `cat` over it in-memory.
//! - `heso ls <url> [path]` — Fetch + list children at `path` (default `/`).
//!   Returns `{ path, entries: [LsRow, ...] }` JSON.
//! - `heso cat <url> <path|@ref>` — Polymorphic: returns `{ path, content }`
//!   for a heading-tree path, or the full `ElementRef` JSON for an action
//!   graph ref like `@e7`. Same shell verb, two address spaces.
//! - `heso find <url> [--role X] [--name SUBSTR] [--section /p]` — list
//!   interactive elements from the page's action graph. Filters compose.
//!   Returns `{ url, filters, count, matches: [ElementRef, ...] }`.
//! - `heso click <url> <@ref>` — Fetch `<url>`, resolve `<@ref>` against
//!   the action graph, dispatch a real `click` event through the DOM event
//!   model (handlers registered via `addEventListener` fire). Returns
//!   `{ url, op: "click", ref, selector, value, console, ok }`.
//! - `heso fill <url> <@ref> <value>` — Fetch `<url>`, find the input at
//!   `<@ref>`, set its `.value`, and fire both `input` and `change` events
//!   (matches real browser typing behavior). Returns the same shape as
//!   `click` with `op: "fill"`.
//! - `heso submit <url> <@form-ref>` — Fetch `<url>`, find the form at
//!   `<@form-ref>`, click its first `button[type=submit]` /
//!   `input[type=submit]` descendant. Real `reqwest::post` of the
//!   serialized form lands with sessions; today this drives only the JS
//!   side of submission.
//! - `heso meta <url>` — Fetch + extract structured metadata (JSON-LD,
//!   OpenGraph, Twitter cards, SEO meta, canonical, icons, lang). Returns
//!   the [`PageMetadata`] as JSON.
//! - `heso open <url>` — Fetch once and return the whole agent-shaped page
//!   view: `{ url, title, description, metadata, tree, actions, plat_hash }`.
//!   The single-call surface external agents prefer — one subprocess, all
//!   the pre-computed context. `plat_hash` is a BLAKE3 content fingerprint
//!   that anyone can recompute to verify the plat hasn't been tampered with.
//! - `heso open --explore-links N <url>` — Opt into **cartography V0**: after
//!   parsing the page, follow up to `--link-cap` (default 20, hard max 50)
//!   same-origin `<a href>` links and embed each fetched page's tree +
//!   metadata + actions under the new `linked_pages` field. `N` is the
//!   depth (0 = off, 1 = direct links only, 2+ = nested). Per-link errors
//!   are recorded individually; only the initial fetch failing fails the
//!   call. Useful for handing the agent a static map of a small subset of
//!   the site in one round-trip.
//! - `heso plat-hash <file>` — Compute the BLAKE3 hash of a plat JSON
//!   file (the output of `heso open`). Any embedded `plat_hash` field
//!   is IGNORED during hashing; the printed value is the hash of the
//!   rest of the content, exactly what `heso open` would have written.
//! - `heso plat-verify <file>` — Verify a plat file's embedded `plat_hash`
//!   against the recomputed hash of its content. Exit 0 = match, 1 =
//!   mismatch (tampered/corrupted), 2 = malformed (missing or
//!   non-string `plat_hash`).
//! - `heso serve` — long-running JSON-RPC 2.0 server over stdin/stdout.
//!   Framework authors (Browser Use, Stagehand, custom agents) launch
//!   ONE child process and pipe newline-delimited requests in, responses
//!   out, instead of spawning per-call. Stateful page cache by `page_id`.
//!   See [`crate::serve`].
//! - `heso run <url> <request>` — **STUB.** Today this builds a one-op
//!   trace `[Cd(url)]` and stashes `request` in `planner_id`; the request
//!   text is NOT interpreted. Returns a Receipt for a navigate-only trace.
//!   Will become the real one-tool surface once the planner (T-022) lands;
//!   until then, agents should call `heso open` and walk the page
//!   themselves, not pretend `heso run` does anything intelligent.
//!
//! Per [ADR 0012], the static engine is `heso-engine-fetch`. Per [ADR 0014],
//! the JS engine is `heso-engine-js` (QuickJS via `rquickjs`, Phase 1A
//! landed). Both ship in the same binary — no Chrome dep, no Node dep.
//!
//! [ADR 0012]: ../../decisions/0012-fetch-only-native-engine.md
//! [ADR 0014]: ../../decisions/0014-bundled-quickjs-agent-dom.md
//! [ADR 0016]: ../../decisions/0016-positioning-headless-browser-for-agents.md

mod serve;

use std::env;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use heso_core::{IdentityKey, Url};
use heso_engine_api::{EngineApi, Page};
use heso_engine_fetch::{
    linked_pages_to_json, resolve_action, ElementRef, ExploreOptions, FetchEngine,
    DEFAULT_LINK_CAP, HARD_LINK_CAP,
};
use heso_primitives::{CdInput, CdTarget, PrimitiveOp, Trace};
use heso_trace::{verify_receipt, Mode, Receipt, VerifyOutcome};
use heso_trace_exec::{run, run_signed, SessionConfig};

/// Default identity-key path. Created by `heso identity init`; auto-loaded
/// by `heso run` when present. Lives under the gitignored
/// `heso-local-data/` directory.
const DEFAULT_IDENTITY_PATH: &str = "heso-local-data/identity.key";

fn print_banner() {
    let version = env!("CARGO_PKG_VERSION");
    println!("heso {version} — headless browser for the agent-relevant half of the web");
    println!();
    println!("Subcommands:");
    println!("  heso fetch <url>              GET a URL via the native fetch engine, print {{url, text}} JSON");
    println!("  heso tree  <url>              Fetch + build the page tree, print the full HtmlTree as JSON");
    println!("  heso ls    <url> [path]       Fetch + list children at <path> (default `/`), JSON");
    println!("  heso cat   <url> <path|@ref>  Fetch + read intro text at <path>, or the element at <@ref>");
    println!("  heso find  <url> [--role X] [--name SUBSTR] [--section /p]   List interactive elements (action graph)");
    println!("  heso meta  <url>              Fetch + extract metadata (JSON-LD, OpenGraph, SEO meta) as JSON");
    println!("  heso open  <url>              Fetch once, return {{url,title,description,metadata,tree,actions,plat_hash}} (agent-facing)");
    println!("    [--explore-links N]            Pre-fetch up to --link-cap direct (depth=1) or nested (depth>=2) same-origin links");
    println!("    [--link-cap M]                 Cap on links followed per level (default 20, hard max 50)");
    println!("  heso click  <url> <@ref>      Fetch <url>, find element at <@ref> in the action graph, dispatch a click");
    println!("  heso fill   <url> <@ref> <v>  Fetch <url>, find element at <@ref>, set its .value and fire input+change");
    println!("  heso submit <url> <@form-ref> Fetch <url>, find form at <@form-ref>, click its first submit control");
    println!("  heso plat-hash   <file>       BLAKE3 hash of a plat JSON file (content identity)");
    println!("  heso plat-verify <file>       Verify a plat file's embedded plat_hash matches its content");
    println!("  heso eval-js [--seed N] <js>  [Phase 1A — ADR 0014] Evaluate JS in a sandboxed QuickJS context; print value+console as JSON");
    println!("                                Pass `-` to read JS source from stdin. No DOM/window yet — Phase 1B.");
    println!("                                --seed N seeds Math.random / crypto.getRandomValues / crypto.randomUUID (default 0).");
    println!("  heso eval-dom [--seed N] [--js-fetch] <url> <js>");
    println!("                                [Phase 1C — ADR 0014] Fetch <url>, run every <script> in document order, then eval <js>");
    println!("                                against the post-hydration DOM. Pass `-` for <js> to read from stdin.");
    println!("                                --seed N seeds the determinism shims (default 0). Default skips <script src=...>;");
    println!("                                pass --js-fetch to install the JS `fetch()` global and honor <script src=...>");
    println!("                                via the same `reqwest::Client` used for the page load (cookies + receipts coherent).");
    println!("                                Under --seed N + --js-fetch, fetch() rejects with a clear cassette error (ADR 0008).");
    println!("  heso serve                    Long-running JSON-RPC server over stdin/stdout (framework integration)");
    println!("  heso run   <url> <request>    [STUB] navigates to <url> only; request is ignored. Planner not yet wired.");
    println!("    [--identity <path>]            Sign the receipt with the Ed25519 key at <path>; defaults to heso-local-data/identity.key when present");
    println!("  heso identity init [--path P] Generate a fresh Ed25519 identity at <path> (default: heso-local-data/identity.key)");
    println!(
        "  heso identity show [--path P] Print the base64 public key of the identity at <path>"
    );
    println!("  heso receipt-verify <file>    Verify a signed receipt (exit 0 valid, 1 invalid, 2 missing/malformed)");
    println!();
    println!("Native single binary — no Chrome, no Node, deploy anywhere.");
    println!("See state.json + decisions/0012-fetch-only-native-engine.md (static engine) and");
    println!(
        "decisions/0014-bundled-quickjs-agent-dom.md (JS engine, in progress) for the design."
    );
}

/// Open a URL with the default `FetchEngine`. Returns the loaded page or an
/// `ExitCode` describing how to exit the process on failure.
async fn open_or_die(url_arg: &str) -> Result<heso_engine_fetch::FetchPage, ExitCode> {
    let url = match Url::parse(url_arg) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{url_arg}`: {e}");
            return Err(ExitCode::from(2));
        }
    };
    let engine = match FetchEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to build engine: {e}");
            return Err(ExitCode::FAILURE);
        }
    };
    engine.open(&url).await.map_err(|e| {
        eprintln!("fetch failed: {e}");
        ExitCode::FAILURE
    })
}

fn print_json(value: &serde_json::Value) -> ExitCode {
    match serde_json::to_string_pretty(value) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to serialize output: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_fetch(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso fetch <url>");
        return ExitCode::from(2);
    }
    let url = match Url::parse(&args[0]) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{}`: {e}", args[0]);
            return ExitCode::from(2);
        }
    };

    let engine = match FetchEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to build engine: {e}");
            return ExitCode::FAILURE;
        }
    };

    let page = match engine.open(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let text = match page.text().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("text() failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let body = serde_json::json!({
        "url": page.url().as_str(),
        "text": text,
    });
    match serde_json::to_string_pretty(&body) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to serialize output: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_tree(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso tree <url>");
        return ExitCode::from(2);
    }
    let page = match open_or_die(&args[0]).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    match serde_json::to_value(&page.tree) {
        Ok(v) => print_json(&v),
        Err(e) => {
            eprintln!("failed to serialize tree: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_ls(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso ls <url> [path]");
        return ExitCode::from(2);
    }
    let path = args.get(1).map(String::as_str).unwrap_or("/");
    let page = match open_or_die(&args[0]).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    match page.tree.ls(path) {
        Ok(rows) => print_json(&serde_json::json!({
            "path": path,
            "entries": rows,
        })),
        Err(e) => {
            eprintln!("ls failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `heso cat <url> <path-or-ref>` — read either:
/// - a tree path like `/pricing/enterprise` → returns `{ path, content }`
///   where `content` is the section's intro text, OR
/// - an action graph ref like `@e7` → returns the full `ElementRef` JSON.
///
/// The leading `@` is the discriminator. Same shell verb, two addressable
/// vocabularies.
async fn cmd_cat(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        eprintln!("usage: heso cat <url> <path|@ref>");
        return ExitCode::from(2);
    }
    let target = &args[1];
    let page = match open_or_die(&args[0]).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    if let Some(stripped) = target.strip_prefix('@') {
        // `@e7` → look up in the action graph.
        let want = format!("@{stripped}");
        match heso_engine_fetch::resolve_action(&page.actions, &want) {
            Some(el) => match serde_json::to_value(el) {
                Ok(v) => print_json(&v),
                Err(e) => {
                    eprintln!("failed to serialize element: {e}");
                    ExitCode::FAILURE
                }
            },
            None => {
                eprintln!("no element at ref `{want}`");
                ExitCode::from(2)
            }
        }
    } else {
        match page.tree.cat(target) {
            Ok(content) => print_json(&serde_json::json!({
                "path": target,
                "content": content,
            })),
            Err(e) => {
                eprintln!("cat failed: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

/// `heso find <url> [--role X] [--name SUBSTR] [--section /path]` —
/// list interactive elements matching the filters. Returns a JSON array
/// of `ElementRef`. No filters → returns the full action graph.
///
/// Filter semantics:
/// - `--role` matches exactly (one of `link`, `button`, `textbox`,
///   `checkbox`, `radio`, `combobox`, `form`).
/// - `--name` is a case-insensitive substring match against the
///   element's accessible name.
/// - `--section` is a path prefix; `--section /pricing` returns
///   everything in `/pricing` and below (e.g. `/pricing/enterprise`).
async fn cmd_find(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso find <url> [--role X] [--name SUBSTR] [--section /path]");
        return ExitCode::from(2);
    }
    let url_arg = &args[0];

    // Walk the remaining args as `--flag value` pairs. Unknown flags →
    // usage error. Raw matching (no `clap`) keeps the CLI consistent
    // with the other heso subcommands.
    let mut role: Option<String> = None;
    let mut name: Option<String> = None;
    let mut section: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--role" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--role needs a value");
                    return ExitCode::from(2);
                };
                role = Some(v.clone());
                i += 2;
            }
            "--name" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--name needs a value");
                    return ExitCode::from(2);
                };
                name = Some(v.clone());
                i += 2;
            }
            "--section" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--section needs a value");
                    return ExitCode::from(2);
                };
                section = Some(v.clone());
                i += 2;
            }
            other => {
                eprintln!("unknown flag `{other}`");
                eprintln!("usage: heso find <url> [--role X] [--name SUBSTR] [--section /path]");
                return ExitCode::from(2);
            }
        }
    }

    let page = match open_or_die(url_arg).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    let filtered = heso_engine_fetch::filter_actions(
        &page.actions,
        role.as_deref(),
        name.as_deref(),
        section.as_deref(),
    );
    // `filter_actions` returns `Vec<&ElementRef>`; serde_json handles refs
    // transparently because `ElementRef: Serialize`.
    match serde_json::to_value(&filtered) {
        Ok(v) => print_json(&serde_json::json!({
            "url": page.url().as_str(),
            "filters": {
                "role": role,
                "name": name,
                "section": section,
            },
            "count": filtered.len(),
            "matches": v,
        })),
        Err(e) => {
            eprintln!("failed to serialize matches: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn cmd_meta(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso meta <url>");
        return ExitCode::from(2);
    }
    let page = match open_or_die(&args[0]).await {
        Ok(p) => p,
        Err(code) => return code,
    };
    match serde_json::to_value(&page.metadata) {
        Ok(v) => print_json(&v),
        Err(e) => {
            eprintln!("failed to serialize metadata: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `heso open <url>` — fetch once, return the agent-shaped payload.
///
/// Flags (must appear AFTER the URL or before — order-tolerant):
/// - `--explore-links N` — opt into cartography v0. `N=0` keeps the
///   classic behavior (no link exploration). `N=1` pre-fetches the
///   page's direct same-origin links and embeds their tree + metadata +
///   actions under `linked_pages`. `N>=2` recurses. Per-link failures
///   are captured as `linked_pages[i].error` and don't fail the call.
/// - `--link-cap M` — cap on links followed per level (default
///   [`DEFAULT_LINK_CAP`], hard max [`HARD_LINK_CAP`]).
async fn cmd_open(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso open [--explore-links N] [--link-cap M] <url>");
        return ExitCode::from(2);
    }

    // Single positional `<url>` plus optional flag pairs. Walk args
    // sequentially, accept flags in either order (before or after the
    // URL), keep behavior consistent with the other heso subcommands
    // (raw arg parsing, no `clap`).
    let mut url_arg: Option<String> = None;
    let mut explore_depth: u8 = 0;
    let mut link_cap: usize = DEFAULT_LINK_CAP;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--explore-links" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--explore-links needs a value");
                    return ExitCode::from(2);
                };
                explore_depth = match v.parse::<u8>() {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("--explore-links: invalid u8 `{v}`: {e}");
                        return ExitCode::from(2);
                    }
                };
                i += 2;
            }
            "--link-cap" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--link-cap needs a value");
                    return ExitCode::from(2);
                };
                link_cap = match v.parse::<usize>() {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("--link-cap: invalid usize `{v}`: {e}");
                        return ExitCode::from(2);
                    }
                };
                if link_cap > HARD_LINK_CAP {
                    eprintln!("--link-cap clamped from {link_cap} to hard max {HARD_LINK_CAP}");
                    link_cap = HARD_LINK_CAP;
                }
                i += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag `{other}`");
                eprintln!("usage: heso open [--explore-links N] [--link-cap M] <url>");
                return ExitCode::from(2);
            }
            _ => {
                if url_arg.is_some() {
                    eprintln!(
                        "unexpected extra argument `{}`; pass a single <url>",
                        args[i]
                    );
                    return ExitCode::from(2);
                }
                url_arg = Some(args[i].clone());
                i += 1;
            }
        }
    }

    let Some(url_str) = url_arg else {
        eprintln!("usage: heso open [--explore-links N] [--link-cap M] <url>");
        return ExitCode::from(2);
    };

    let url = match Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{url_str}`: {e}");
            return ExitCode::from(2);
        }
    };

    let engine = match FetchEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to build engine: {e}");
            return ExitCode::FAILURE;
        }
    };

    let opts = ExploreOptions {
        depth: explore_depth,
        link_cap,
    };

    let page = match engine.open_with_explore(&url, opts).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Agent-facing single payload — one subprocess gets the page URL,
    // title, description, full structured metadata, the navigable tree,
    // the action graph, and (optionally) the explored linked_pages. The
    // `plat_hash` BLAKE3 fingerprint is computed last over the canonical
    // form of everything-except-itself, so anyone holding this JSON can
    // recompute it and verify the plat hasn't been tampered with.
    let mut body = serde_json::json!({
        "url": page.url().as_str(),
        "title": page.tree.title,
        "description": page.tree.description,
        "metadata": page.metadata,
        "tree": page.tree,
        "actions": page.actions,
    });
    if !page.inline_data.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "inline_data".to_owned(),
                serde_json::to_value(&page.inline_data).unwrap_or(serde_json::Value::Null),
            );
        }
    }
    if !page.data_attrs.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "data_attrs".to_owned(),
                serde_json::to_value(&page.data_attrs).unwrap_or(serde_json::Value::Null),
            );
        }
    }
    if !page.linked_pages.is_empty() {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "linked_pages".to_owned(),
                linked_pages_to_json(&page.linked_pages),
            );
        }
    }
    // Compute plat_hash over the canonical form of `body`. The plat
    // module recursively strips any `plat_hash` field at every level
    // before hashing, so embedding it here doesn't poison the hash.
    let hash = heso_engine_fetch::plat_hash(&body);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("plat_hash".to_owned(), serde_json::Value::String(hash));
    }
    print_json(&body)
}

/// `heso plat-hash <file>` — compute the BLAKE3 hash of a plat JSON
/// file (the output of `heso open`). Useful for: caching by hash,
/// deduplication, comparing plats across machines. Prints the hex hash
/// to stdout. If the file already contains a `plat_hash` field, it is
/// IGNORED during hashing (otherwise we'd be hashing the hash); the
/// printed value is the hash of the rest of the content.
/// `heso eval-js <js>` — evaluate a JavaScript expression in a fresh
/// sandboxed QuickJS context (via `heso-engine-js`) and print the
/// result + captured console output as JSON.
///
/// Argument forms:
///
/// - `heso eval-js "1 + 2"` — JS source given inline
/// - `heso eval-js - < script.js` — JS source read from stdin
///
/// Output shape:
///
/// ```json
/// {"ok": true, "value": <json>, "console": [{"level": "log", "args": [...]}, ...]}
/// // OR
/// {"ok": false, "error": {"kind": "exception"|"thrown_value"|"engine", ...}}
/// ```
///
/// Exit codes: 0 on success, 1 on JS error, 2 on usage error. This is
/// the Phase 1A demonstration surface (per ADR 0014) — no DOM, no
/// `window`, no `<script>` on-load execution. Useful for sanity
/// testing the engine independent of any page context.
async fn cmd_eval_js(args: &[String]) -> ExitCode {
    // Walk args once and split flags from positionals so `--seed N`
    // can appear before or after `<js>`. Consistent with the rest of
    // heso's CLI (raw arg parsing, no `clap`).
    let mut seed: u64 = 0;
    let mut positional: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--seed needs a value");
                    return ExitCode::from(2);
                };
                seed = match v.parse::<u64>() {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("--seed: invalid u64 `{v}`: {e}");
                        return ExitCode::from(2);
                    }
                };
                i += 2;
            }
            other if other.starts_with("--") && other != "-" => {
                eprintln!("unknown flag `{other}`");
                eprintln!(
                    "usage: heso eval-js [--seed N] <js> | heso eval-js [--seed N] - < script.js"
                );
                return ExitCode::from(2);
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }
    if positional.is_empty() {
        eprintln!("usage: heso eval-js [--seed N] <js> | heso eval-js [--seed N] - < script.js");
        return ExitCode::from(2);
    }
    let src: String = if positional[0] == "-" {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        if let Err(e) = tokio::io::stdin().read_to_string(&mut buf).await {
            eprintln!("failed to read stdin: {e}");
            return ExitCode::FAILURE;
        }
        buf
    } else {
        positional[0].clone()
    };

    let engine = match heso_engine_js::JsEngine::new_with_seed(seed) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to create JS engine: {e}");
            return ExitCode::FAILURE;
        }
    };

    match engine.eval(&src) {
        Ok(outcome) => {
            let body = serde_json::json!({
                "ok": true,
                "value": outcome.value,
                "console": outcome.console,
            });
            match serde_json::to_string_pretty(&body) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("failed to serialize result: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let err_body = match &e {
                heso_engine_js::EvalError::Exception { message, stack } => serde_json::json!({
                    "kind": "exception",
                    "message": message,
                    "stack": stack,
                }),
                heso_engine_js::EvalError::ThrownValue { value } => serde_json::json!({
                    "kind": "thrown_value",
                    "value": value,
                }),
                heso_engine_js::EvalError::Engine(msg) => serde_json::json!({
                    "kind": "engine",
                    "message": msg,
                }),
            };
            let body = serde_json::json!({
                "ok": false,
                "error": err_body,
            });
            match serde_json::to_string_pretty(&body) {
                Ok(s) => println!("{s}"),
                Err(se) => {
                    eprintln!("failed to serialize error body: {se}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// `heso eval-dom [--js-fetch] <url> <js>` — fetch a URL, parse it,
/// install `document` as the global, run every `<script>` tag on the
/// page in document order, then evaluate `js` against the
/// post-hydration DOM. Prints `{ok, value, console, scripts}` (or
/// `{ok:false, error:{...}}`) as pretty JSON. The `scripts` object
/// surfaces the [`ScriptOutcome`] counts so callers can see how many
/// inline scripts ran, how many threw, and how many external `src=`
/// refs were touched.
///
/// Phase 1C demonstration surface (per ADR 0014). DOM mutation
/// methods, the event model, and the timer pump all work; what
/// landed in this PR is the **page-script execution pass on load**,
/// so an SSR page that hydrates by setting `document.title =`,
/// mutating `<div id="root">` children, or stashing state on
/// `globalThis` will already have done so by the time `js` runs.
///
/// Argument forms (flag is order-tolerant — may appear before or
/// after the URL):
///
/// - `heso eval-dom <url> <js>` — JS source inline (default policy:
///   external `<script src=...>` refs are skipped with a console.warn).
/// - `heso eval-dom <url> -` — JS source from stdin.
/// - `heso eval-dom --js-fetch <url> <js>` — opt-in flag: external
///   `<script src=...>` currently surfaces a `console.error`
///   explaining the fetch path is not wired yet. PR C (vendoring
///   `llrt_fetch`) will flip this branch to issue an actual GET
///   through the shared `reqwest::Client`. The flag exists in this
///   PR so downstream tooling can stage on its CLI shape.
///
/// Exit codes: 0 on success, 1 on fetch or JS error, 2 on usage.
async fn cmd_eval_dom(args: &[String]) -> ExitCode {
    // Order-tolerant flag walk: `--seed N` (with value) and
    // `--js-fetch` / `--no-js-fetch` (boolean toggles) can appear in
    // any position; positionals are `<url> <js>` in order.
    let mut seed: u64 = 0;
    let mut js_fetch = false;
    let mut positional: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--seed needs a value");
                    return ExitCode::from(2);
                };
                seed = match v.parse::<u64>() {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!("--seed: invalid u64 `{v}`: {e}");
                        return ExitCode::from(2);
                    }
                };
                i += 2;
            }
            "--js-fetch" => {
                js_fetch = true;
                i += 1;
            }
            "--no-js-fetch" => {
                js_fetch = false;
                i += 1;
            }
            other if other.starts_with("--") && other != "-" => {
                eprintln!("unknown flag `{other}`");
                eprintln!("usage: heso eval-dom [--seed N] [--js-fetch] <url> <js> | heso eval-dom [--seed N] [--js-fetch] <url> -  < script.js");
                return ExitCode::from(2);
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }
    if positional.len() < 2 {
        eprintln!("usage: heso eval-dom [--seed N] [--js-fetch] <url> <js> | heso eval-dom [--seed N] [--js-fetch] <url> -  < script.js");
        return ExitCode::from(2);
    }
    let url_arg = &positional[0];
    let js_src: String = if positional[1] == "-" {
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        if let Err(e) = tokio::io::stdin().read_to_string(&mut buf).await {
            eprintln!("failed to read stdin: {e}");
            return ExitCode::FAILURE;
        }
        buf
    } else {
        positional[1].clone()
    };

    let url = match Url::parse(url_arg) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{url_arg}`: {e}");
            return ExitCode::from(2);
        }
    };
    let fetch_engine = match FetchEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to build fetch engine: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (final_url, html) = match fetch_engine.fetch_text(&url).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Build the JS engine. When `--js-fetch` is set we install a
    // live `fetch()` global routed through the same `reqwest::Client`
    // the static path used to load the page (so cookies, TLS,
    // User-Agent stay coherent — per `next-phase-plan.md` item C and
    // the ADR 0014 Phase 2 row).
    //
    // When `--seed N` is set without a recording cassette (item M is
    // not landed yet), the in-JS `fetch()` rejects every call with a
    // clear "not in cassette" error per ADR 0008's determinism gate.
    // Seed = 0 is treated as "no seed" for this purpose (it's the
    // default for unseeded runs and shouldn't lock out live fetch).
    let js_engine_result = if js_fetch {
        let client = fetch_engine.client();
        let rt_handle = tokio::runtime::Handle::current();
        if seed != 0 {
            heso_engine_js::JsEngine::new_with_seed_and_fetch(seed, client, rt_handle)
        } else {
            heso_engine_js::JsEngine::new_with_fetch(client, rt_handle)
        }
    } else {
        heso_engine_js::JsEngine::new_with_seed(seed)
    };
    let js_engine = match js_engine_result {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to create JS engine: {e}");
            return ExitCode::FAILURE;
        }
    };

    let policy = if js_fetch {
        heso_engine_js::ScriptFetchPolicy::Fetch
    } else {
        heso_engine_js::ScriptFetchPolicy::Skip
    };

    match js_engine.eval_with_html_capture(&html, &js_src, policy) {
        Ok((outcome, script_outcome)) => {
            let body = serde_json::json!({
                "ok": true,
                "url": final_url.to_string(),
                "value": outcome.value,
                "console": outcome.console,
                "scripts": script_outcome,
            });
            match serde_json::to_string_pretty(&body) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("failed to serialize result: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let err_body = match &e {
                heso_engine_js::EvalError::Exception { message, stack } => serde_json::json!({
                    "kind": "exception",
                    "message": message,
                    "stack": stack,
                }),
                heso_engine_js::EvalError::ThrownValue { value } => serde_json::json!({
                    "kind": "thrown_value",
                    "value": value,
                }),
                heso_engine_js::EvalError::Engine(msg) => serde_json::json!({
                    "kind": "engine",
                    "message": msg,
                }),
            };
            let body = serde_json::json!({
                "ok": false,
                "url": final_url.to_string(),
                "error": err_body,
            });
            match serde_json::to_string_pretty(&body) {
                Ok(s) => println!("{s}"),
                Err(se) => {
                    eprintln!("failed to serialize error body: {se}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// Build a CSS selector that resolves an action-graph element via
/// `document.querySelector(...)`.
///
/// Strategy, in order of preference:
///
/// 1. `attrs["id"]` is present and looks like a plain identifier:
///    `#<id>`. Plain-identifier means it parses fine in CSS without
///    escaping — alphanumeric / underscore / hyphen, and doesn't
///    start with a digit. Almost every real-world id qualifies.
/// 2. Tag plus discriminating attributes: for `<a>` use
///    `a[href="..."]`; for form controls use the tag plus
///    `[type="..."][name="..."]` if both are present, falling back to
///    either alone. Quoting via `serde_json::to_string` gives us a
///    CSS-safe attribute literal (the JSON string-literal grammar is
///    a subset of what CSS accepts inside `[attr="..."]`).
/// 3. Last-resort fallback: bare tag selector + nth-of-type derived
///    from the element's position in the document. This is a best-
///    effort guess and may match the wrong element on a complex page;
///    when an action ref leaks here, the better fix is to give the
///    element a name / id upstream.
///
/// Returns `None` only if `el` lacks both a tag name AND any of the
/// fallback attrs — in practice, every action-graph entry has a tag
/// so this is unreachable.
fn selector_for_action(el: &ElementRef) -> Option<String> {
    // (1) prefer a clean id selector.
    if let Some(id) = el.attrs.get("id") {
        if !id.is_empty() && is_css_plain_ident(id) {
            return Some(format!("#{id}"));
        }
    }

    let tag = el.tag.as_str();
    if tag.is_empty() {
        return None;
    }

    // (2a) <a> with href.
    if tag == "a" {
        if let Some(href) = el.attrs.get("href") {
            return Some(format!("a[href={}]", css_attr_literal(href)));
        }
    }

    // (2b) form controls: combine type + name when present.
    if matches!(tag, "input" | "textarea" | "select" | "button") {
        let mut sel = tag.to_owned();
        if let Some(t) = el.attrs.get("type") {
            sel.push_str(&format!("[type={}]", css_attr_literal(t)));
        }
        if let Some(n) = el.attrs.get("name") {
            sel.push_str(&format!("[name={}]", css_attr_literal(n)));
        }
        // If we added any attribute, return; else fall through to (3).
        if sel.len() > tag.len() {
            return Some(sel);
        }
    }

    // (2c) <form> with action.
    if tag == "form" {
        if let Some(a) = el.attrs.get("action") {
            return Some(format!("form[action={}]", css_attr_literal(a)));
        }
    }

    // (3) bare tag. May be ambiguous on a complex page — caller
    // should plumb more attrs upstream if this becomes a real issue.
    Some(tag.to_owned())
}

/// `true` if `s` parses as a CSS identifier without escaping —
/// alphanumeric + underscore + hyphen, doesn't start with a digit or
/// a single `-` followed by a digit. Conservative; rejects valid-but-
/// fancy ids in favor of falling back to attribute matching.
fn is_css_plain_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// JSON-encode `value` to produce a CSS-safe `[attr=...]` literal.
/// Both grammars accept `"..."` with backslash-escaped quotes; using
/// `serde_json::to_string` handles the escaping uniformly. Returns
/// `"<empty>"` on the (unreachable) error case so we don't propagate
/// a String allocation failure here.
fn css_attr_literal(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// Shared body for `heso click` / `heso fill` / `heso submit`. Fetches
/// `url`, resolves `ref_str` in the action graph, builds a CSS
/// selector, and hands `(html, selector)` to `op`. `op` is the
/// engine method to call — `dispatch_click`, `set_input_value`, or
/// `submit_form`. Prints the unified `{ok, url, value, console}` JSON
/// the existing eval-* commands use and returns a [`ExitCode`].
async fn run_dispatch<F>(url_arg: &str, ref_arg: &str, op_name: &str, op: F) -> ExitCode
where
    F: FnOnce(
        &heso_engine_js::JsEngine,
        &str,
        &str,
    ) -> Result<heso_engine_js::EvalOutcome, heso_engine_js::EvalError>,
{
    // Normalize the @ref — accept both `@e7` and `e7` for ergonomics.
    let want = if let Some(stripped) = ref_arg.strip_prefix('@') {
        format!("@{stripped}")
    } else {
        format!("@{ref_arg}")
    };

    let url = match Url::parse(url_arg) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{url_arg}`: {e}");
            return ExitCode::from(2);
        }
    };
    let engine = match FetchEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to build fetch engine: {e}");
            return ExitCode::FAILURE;
        }
    };

    // We need BOTH the parsed action graph (to resolve @ref → selector)
    // AND the raw HTML (to hand to the JS engine). `open()` gives us
    // the actions; `fetch_text` gives us the HTML. Two round-trips
    // would be wasteful, so we fetch once via `open_static` (gets the
    // actions) and re-fetch the text via `fetch_text`. NOTE: this is
    // two HTTP calls — a follow-up should let `open_static` keep the
    // raw HTML on the FetchPage so we can re-use it. For PR1 the
    // duplicate fetch is acceptable: both go through the same client,
    // and the second one comes from HTTP cache for sane servers.
    let page = match engine.open(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let action = match resolve_action(&page.actions, &want) {
        Some(a) => a,
        None => {
            eprintln!("no element at ref `{want}`");
            return ExitCode::from(2);
        }
    };
    let selector = match selector_for_action(action) {
        Some(s) => s,
        None => {
            eprintln!(
                "could not build a CSS selector for `{want}` (tag={:?}, attrs={:?})",
                action.tag, action.attrs
            );
            return ExitCode::FAILURE;
        }
    };

    let (final_url, html) = match engine.fetch_text(&url).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("fetch (html) failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let js_engine = match heso_engine_js::JsEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to create JS engine: {e}");
            return ExitCode::FAILURE;
        }
    };

    match op(&js_engine, &html, &selector) {
        Ok(outcome) => {
            let body = serde_json::json!({
                "ok": true,
                "op": op_name,
                "url": final_url.to_string(),
                "ref": want,
                "selector": selector,
                "value": outcome.value,
                "console": outcome.console,
            });
            match serde_json::to_string_pretty(&body) {
                Ok(s) => println!("{s}"),
                Err(e) => {
                    eprintln!("failed to serialize result: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            let err_body = match &e {
                heso_engine_js::EvalError::Exception { message, stack } => serde_json::json!({
                    "kind": "exception",
                    "message": message,
                    "stack": stack,
                }),
                heso_engine_js::EvalError::ThrownValue { value } => serde_json::json!({
                    "kind": "thrown_value",
                    "value": value,
                }),
                heso_engine_js::EvalError::Engine(msg) => serde_json::json!({
                    "kind": "engine",
                    "message": msg,
                }),
            };
            let body = serde_json::json!({
                "ok": false,
                "op": op_name,
                "url": final_url.to_string(),
                "ref": want,
                "selector": selector,
                "error": err_body,
            });
            match serde_json::to_string_pretty(&body) {
                Ok(s) => println!("{s}"),
                Err(se) => {
                    eprintln!("failed to serialize error body: {se}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::FAILURE
        }
    }
}

/// `heso click <url> <@ref>` — fetch <url>, locate the element with
/// id `@ref` in the page's action graph, build a CSS selector from
/// its attributes, and dispatch a cancelable `"click"` event on it
/// via the QuickJS engine (per [ADR 0014]).
///
/// The selector is built in this layer (not in the engine) per the
/// PR1 plan: `selector_for_action` prefers `#id`, then falls through
/// to `tag[attr=...]` shapes, then to a bare tag. If the page hosts a
/// modern SPA, any inline `<script>` that ran during static parse is
/// NOT yet rerun — phase 1B does not execute `<script>` tags
/// (handled by PR-A of the next phase plan). For now this fires
/// click handlers that were attached during the same `eval_with_html`
/// snippet — useful for click-through behaviors a planner sets up
/// inline.
///
/// Output: `{ok, op, url, ref, selector, value, console}`. `value`
/// is `true` when the selector matched and the click was dispatched.
///
/// Exit codes: 0 on success, 1 on fetch/JS failure, 2 on usage error
/// or unknown @ref.
async fn cmd_click(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        eprintln!("usage: heso click <url> <@ref>");
        return ExitCode::from(2);
    }
    run_dispatch(&args[0], &args[1], "click", |eng, html, sel| {
        eng.dispatch_click(html, sel)
    })
    .await
}

/// `heso fill <url> <@ref> <value>` — fetch <url>, locate the input
/// at `@ref` in the action graph, set its `value` to `<value>`, and
/// dispatch first an `"input"` then a `"change"` event (matching
/// real browser behavior when a user types).
///
/// Output shape mirrors `heso click`: `{ok, op, url, ref, selector,
/// value, console}` where `value: true` indicates the selector
/// matched. Exit codes match `heso click`.
async fn cmd_fill(args: &[String]) -> ExitCode {
    if args.len() < 3 {
        eprintln!("usage: heso fill <url> <@ref> <value>");
        return ExitCode::from(2);
    }
    let value = args[2].clone();
    run_dispatch(&args[0], &args[1], "fill", move |eng, html, sel| {
        eng.set_input_value(html, sel, &value)
    })
    .await
}

/// `heso submit <url> <@form-ref>` — fetch <url>, locate the form at
/// `@form-ref`, then click its first submit-typed descendant
/// (`button[type="submit"]` / `input[type="submit"]` / bare-typed
/// `<button>`). See [`heso_engine_js::JsEngine::submit_form`] for the
/// Phase 1B limitations (no real HTTP POST; relies on JS handlers).
///
/// Output: `{ok, op, url, ref, selector, value, console}`. `value:
/// true` iff a submit control was found and clicked; `false` if the
/// form had no submit control. Exit codes match `heso click`.
async fn cmd_submit(args: &[String]) -> ExitCode {
    if args.len() < 2 {
        eprintln!("usage: heso submit <url> <@form-ref>");
        return ExitCode::from(2);
    }
    run_dispatch(&args[0], &args[1], "submit", |eng, html, sel| {
        eng.submit_form(html, sel)
    })
    .await
}

async fn cmd_plat_hash(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso plat-hash <file>");
        return ExitCode::from(2);
    }
    let file = &args[0];
    let contents = match tokio::fs::read_to_string(file).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read `{file}`: {e}");
            return ExitCode::FAILURE;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("`{file}` is not valid JSON: {e}");
            return ExitCode::FAILURE;
        }
    };
    let hash = heso_engine_fetch::plat_hash(&value);
    println!("{hash}");
    ExitCode::SUCCESS
}

/// `heso plat-verify <file>` — verify a plat JSON file's embedded
/// `plat_hash` against the recomputed hash of its content. Exits 0 if
/// they match, 1 if they don't, 2 if the input is malformed (missing or
/// non-string `plat_hash`).
async fn cmd_plat_verify(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso plat-verify <file>");
        return ExitCode::from(2);
    }
    let file = &args[0];
    let contents = match tokio::fs::read_to_string(file).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read `{file}`: {e}");
            return ExitCode::FAILURE;
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("`{file}` is not valid JSON: {e}");
            return ExitCode::FAILURE;
        }
    };
    match heso_engine_fetch::plat_verify(&value) {
        Ok(true) => {
            let embedded = value
                .get("plat_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            println!("OK {embedded}");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            let embedded = value
                .get("plat_hash")
                .and_then(|v| v.as_str())
                .unwrap_or("(unknown)");
            let recomputed = heso_engine_fetch::plat_hash(&value);
            eprintln!("MISMATCH");
            eprintln!("  embedded:   {embedded}");
            eprintln!("  recomputed: {recomputed}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("verify failed: {e}");
            ExitCode::from(2)
        }
    }
}

/// **STUB.** `heso run` is the planned single-tool surface (`heso.run(url,
/// request) → Receipt`) per ADR 0009, but the planner (T-022) is not yet
/// wired. Today this:
///   1. parses `url`,
///   2. builds a one-op trace `[Cd(url)]` — i.e. it just navigates,
///   3. stashes the user's `request` string in `planner_id` only so the
///      receipt can be correlated back; **the request is NOT interpreted**.
///
/// Optional flags:
/// - `--identity <path>` — sign the receipt with the Ed25519 key at `<path>`.
///   When omitted, `heso-local-data/identity.key` is loaded if it exists;
///   otherwise the receipt is unsigned.
///
/// A genuine `heso.run` arrives when T-022 lands. Until then, agents that
/// want page content should use `heso open` (full agent-shaped payload) or
/// `heso fetch` / `heso tree` / `heso ls` / `heso cat`.
async fn cmd_run(args: &[String]) -> ExitCode {
    // Walk the args: an `--identity <path>` flag (optional) anywhere, then
    // a positional URL, then the request string (everything else joined).
    let mut identity_path: Option<PathBuf> = None;
    let mut explicit_identity = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--identity" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--identity needs a path");
                    return ExitCode::from(2);
                };
                identity_path = Some(PathBuf::from(v));
                explicit_identity = true;
                i += 2;
            }
            other if other.starts_with("--") && other != "--" => {
                eprintln!("unknown flag `{other}`");
                eprintln!("usage: heso run [--identity <path>] <url> <request...>");
                return ExitCode::from(2);
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }
    if positional.len() < 2 {
        eprintln!("usage: heso run [--identity <path>] <url> <request...>");
        eprintln!("note: today `heso run` only navigates to <url>; the");
        eprintln!("      request text is recorded on the Receipt but not");
        eprintln!("      interpreted. Planner is T-022 (pending).");
        return ExitCode::from(2);
    }
    let url_arg = &positional[0];
    let request = positional[1..].join(" ");

    let url = match Url::parse(url_arg) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{url_arg}`: {e}");
            return ExitCode::from(2);
        }
    };

    // Identity resolution:
    //   --identity <path> explicit → must exist, hard error on miss.
    //   no flag → try DEFAULT_IDENTITY_PATH; absent is fine (unsigned).
    let resolved_identity: Option<IdentityKey> = match identity_path {
        Some(ref p) => match IdentityKey::load(p) {
            Ok(k) => Some(k),
            Err(e) => {
                eprintln!("failed to load identity at `{}`: {e}", p.display());
                return ExitCode::FAILURE;
            }
        },
        None => {
            let default_path = Path::new(DEFAULT_IDENTITY_PATH);
            if default_path.exists() {
                match IdentityKey::load(default_path) {
                    Ok(k) => Some(k),
                    Err(e) => {
                        eprintln!(
                            "default identity at `{}` exists but failed to load: {e}",
                            default_path.display()
                        );
                        return ExitCode::FAILURE;
                    }
                }
            } else {
                None
            }
        }
    };

    // Honor ADR 0008: identity must not sign in `live` mode. Today
    // `heso run` always runs in `deterministic` mode (the planner stub
    // hard-codes it), so this branch is here for when modes get a flag.
    let _ = explicit_identity; // currently informational only

    let engine = match FetchEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to build engine: {e}");
            return ExitCode::FAILURE;
        }
    };

    // M0/M1: planner v0 (T-022) isn't wired yet, so we hand-build a minimal
    // trace that just navigates to the start URL. The user's request is
    // stashed in `planner_id` so callers can correlate the receipt back to
    // what was asked. Replace this with a planner call once T-022 lands.
    let trace: Trace = vec![PrimitiveOp::Cd(CdInput {
        target: CdTarget::Url { url },
    })];

    let cfg = SessionConfig {
        seed: 0,
        mode: Mode::Deterministic,
        planner_id: format!("stub-cli:{request}"),
    };

    let receipt = match resolved_identity.as_ref() {
        Some(id) => run_signed(&engine, &trace, &cfg, id).await,
        None => run(&engine, &trace, &cfg).await,
    };
    match serde_json::to_string_pretty(&receipt) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to serialize receipt: {e}");
            ExitCode::FAILURE
        }
    }
}

// ============================================================================
// Identity subcommands (item H, ADR 0005)
// ============================================================================

/// `heso identity <sub> [args]` dispatcher.
///
/// Subcommands:
///   - `heso identity init [--path <p>]` — generate + write a new key.
///   - `heso identity show [--path <p>]` — print the base64 public key.
///
/// Default path is `heso-local-data/identity.key`. The directory is
/// already gitignored.
fn cmd_identity(args: &[String]) -> ExitCode {
    let Some(sub) = args.first() else {
        eprintln!("usage: heso identity <init|show> [--path <p>]");
        return ExitCode::from(2);
    };
    match sub.as_str() {
        "init" => cmd_identity_init(&args[1..]),
        "show" => cmd_identity_show(&args[1..]),
        other => {
            eprintln!("unknown identity subcommand: {other}");
            eprintln!("usage: heso identity <init|show> [--path <p>]");
            ExitCode::from(2)
        }
    }
}

/// Parse `[--path <p>]` from the tail args. Returns the chosen path (the
/// default if `--path` is absent) or an exit code on usage error.
fn parse_identity_path(args: &[String]) -> Result<PathBuf, ExitCode> {
    let mut path: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--path" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--path needs a value");
                    return Err(ExitCode::from(2));
                };
                path = Some(PathBuf::from(v));
                i += 2;
            }
            other => {
                eprintln!("unknown flag `{other}`");
                return Err(ExitCode::from(2));
            }
        }
    }
    Ok(path.unwrap_or_else(|| PathBuf::from(DEFAULT_IDENTITY_PATH)))
}

fn cmd_identity_init(args: &[String]) -> ExitCode {
    let path = match parse_identity_path(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    if path.exists() {
        eprintln!(
            "identity already exists at `{}` — refusing to overwrite. \
             Delete it explicitly if you want to rotate.",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    let key = IdentityKey::generate();
    if let Err(e) = key.save(&path) {
        eprintln!("failed to save identity to `{}`: {e}", path.display());
        return ExitCode::FAILURE;
    }
    // Print a small JSON envelope so callers can pipe it.
    let body = serde_json::json!({
        "path": path.display().to_string(),
        "public_key": key.public_key_b64(),
        "algorithm": "Ed25519",
    });
    match serde_json::to_string_pretty(&body) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to serialize identity envelope: {e}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_identity_show(args: &[String]) -> ExitCode {
    let path = match parse_identity_path(args) {
        Ok(p) => p,
        Err(code) => return code,
    };
    let key = match IdentityKey::load(&path) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("failed to load identity at `{}`: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let body = serde_json::json!({
        "path": path.display().to_string(),
        "public_key": key.public_key_b64(),
        "algorithm": "Ed25519",
    });
    match serde_json::to_string_pretty(&body) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("failed to serialize identity envelope: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `heso receipt-verify <file>` — read a receipt JSON, verify its
/// embedded Ed25519 signature. Exit 0 if valid, 1 if invalid
/// (tampered/wrong key), 2 if the receipt has no signature or fails to
/// parse.
async fn cmd_receipt_verify(args: &[String]) -> ExitCode {
    let Some(file) = args.first() else {
        eprintln!("usage: heso receipt-verify <file>");
        return ExitCode::from(2);
    };
    let contents = match tokio::fs::read_to_string(file).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("failed to read `{file}`: {e}");
            return ExitCode::from(2);
        }
    };
    let receipt: Receipt = match serde_json::from_str(&contents) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("`{file}` is not a valid Receipt JSON: {e}");
            return ExitCode::from(2);
        }
    };
    match verify_receipt(&receipt) {
        VerifyOutcome::Valid => {
            let pk = receipt
                .signature
                .as_ref()
                .map(|s| s.public_key.as_str())
                .unwrap_or("(unknown)");
            println!("OK {pk}");
            ExitCode::SUCCESS
        }
        VerifyOutcome::Invalid(e) => {
            eprintln!("INVALID: {e}");
            ExitCode::from(1)
        }
        VerifyOutcome::Missing => {
            eprintln!("MISSING: receipt has no `signature` field");
            ExitCode::from(2)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("fetch") => cmd_fetch(&args[1..]).await,
        Some("tree") => cmd_tree(&args[1..]).await,
        Some("ls") => cmd_ls(&args[1..]).await,
        Some("cat") => cmd_cat(&args[1..]).await,
        Some("find") => cmd_find(&args[1..]).await,
        Some("meta") => cmd_meta(&args[1..]).await,
        Some("open") => cmd_open(&args[1..]).await,
        Some("plat-hash") => cmd_plat_hash(&args[1..]).await,
        Some("plat-verify") => cmd_plat_verify(&args[1..]).await,
        Some("eval-js") => cmd_eval_js(&args[1..]).await,
        Some("eval-dom") => cmd_eval_dom(&args[1..]).await,
        Some("click") => cmd_click(&args[1..]).await,
        Some("fill") => cmd_fill(&args[1..]).await,
        Some("submit") => cmd_submit(&args[1..]).await,
        Some("serve") => serve::run().await,
        Some("run") => cmd_run(&args[1..]).await,
        Some("identity") => cmd_identity(&args[1..]),
        Some("receipt-verify") => cmd_receipt_verify(&args[1..]).await,
        Some(other) => {
            eprintln!("unknown subcommand: {other}\n");
            print_banner();
            ExitCode::from(2)
        }
        None => {
            print_banner();
            ExitCode::SUCCESS
        }
    }
}
