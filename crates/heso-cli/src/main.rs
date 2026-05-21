//! # heso-cli
//!
//! The `heso` binary — the agent-native web engine. No Chromium. No Node.
//! One Rust binary, ~9 MB stripped, single-file deploy anywhere. See
//! [ADR 0016] for the positioning rationale.
//!
//! Every subcommand below operates on the in/out scope from ADR 0016:
//! fetch, parse, JS execution, forms, clicks, sessions, signed receipts.
//! No canvas, no WebGL, no video, no CSS layout — that's the bet.
//!
//! - `heso` — prints a banner.
//! - `heso fetch <url>` — HTTP GET via the native [`FetchEngine`], print
//!   `{ url, text }` JSON. Direct path — no planner, no trace runner. The
//!   simplest surface external agents can call.
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
//! - `heso submit <url> <@form-ref> [--field NAME=VALUE]... [--data JSON]`
//!   — Fetch `<url>`, find the form at `<@form-ref>`, optionally pre-fill
//!   its named inputs from `--field` / `--data`, dispatch the submit
//!   event, serialize per `enctype`, POST through the shared
//!   `reqwest::Client`, follow redirects, and return the response
//!   (`responseStatus`, `responseUrl`, `responseBody` ≤ 64 KB,
//!   `responseContentType`, and `responseJson` when the server sent
//!   `application/json`). One-shot: fetch + fill + submit + observe in
//!   a single CLI invocation, fixing the stateless-fill gap that
//!   `agent regression testing` filed.
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
//! - `heso search <query>` — First-class multi-source web search verb.
//!   DDG HTML + Wikipedia REST summary by default (no API keys);
//!   optional SearXNG via `--searx-url` or `HESO_SEARX_URL`. Pure HTTP
//!   + HTML parsing — no JS engine. Round-robin ranked merge across
//!   engines, dedupe by canonical URL. Wikipedia goes in the
//!   top-level `knowledge` block, not in `results`. See
//!   [`crate::search`] for the full design.
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
//! - `heso action-hash <url> [actions-json | -]` — Algorithm-derived
//!   identity for a `(URL, actions)` pair. BLAKE3 over canonical JSON of
//!   the normalized URL + a caller-supplied JSON array of actions. No
//!   fetch, no storage — same inputs always produce the same hash. Useful
//!   as a cache key or a content-free trace fingerprint.
//!
//! Per [ADR 0012], the static engine is `heso-engine-fetch`. Per [ADR 0014],
//! the JS engine is `heso-engine-js` (QuickJS via `rquickjs`, Phase 1A
//! landed). Both ship in the same binary — no Chrome dep, no Node dep.
//!
//! [ADR 0012]: ../../decisions/0012-fetch-only-native-engine.md
//! [ADR 0014]: ../../decisions/0014-bundled-quickjs-agent-dom.md
//! [ADR 0016]: ../../decisions/0016-positioning-headless-browser-for-agents.md

mod batch;
mod receipts;
mod search;
mod serve;

// Replace the system allocator with mimalloc. Windows' UCRT
// allocator is the weakest standard allocator of any major platform;
// mimalloc has near-zero init cost and outperforms it on every
// alloc-heavy path heso runs (reqwest body buffers, scraper tree,
// serde_json, canonical-JSON writes). One line, no ergonomic cost.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use heso_core::{IdentityKey, Url};
use heso_engine_api::{EngineApi, Page};
use heso_engine_fetch::{
    linked_pages_to_json, resolve_action, resolve_locator_from_html, ElementRef, ExploreOptions,
    FetchEngine, LocatorError, DEFAULT_LINK_CAP, HARD_LINK_CAP,
};
use heso_trace::{
    parse_actions, trace_fingerprint, verify_fingerprint, verify_receipt, Action,
    FingerprintOutcome, Receipt, TraceFingerprint, VerifyOutcome,
};

/// Default identity-key path used by `heso identity init` / `show` when
/// the caller doesn't pass `--path`. Lives under the gitignored
/// `heso-local-data/` directory.
const DEFAULT_IDENTITY_PATH: &str = "heso-local-data/identity.key";

fn print_banner() {
    let version = env!("CARGO_PKG_VERSION");
    println!("heso {version} — the agent-native web engine. No Chromium. No Node. One Rust binary.");
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
    println!("    [--receipt PATH]               Emit a signed Receipt (Ed25519, BLAKE3) to PATH alongside stdout JSON");
    println!("    [--key PATH]                   Identity key for --receipt (default: heso-local-data/identity.key)");
    println!("    [--mode M]                     Receipt mode: deterministic (default), recording, live");
    println!("    [--seed N]                     Session seed stamped into the receipt (default 0)");
    println!("  heso read  <url>              Like `open` PLUS post-hydration text, grouped forms, cookies, console, framework sniff, scripts");
    println!("    [--include CSV]                Filter the optional surface: text,forms,cookies,console,framework,scripts (default: all)");
    println!("    [--receipt PATH] [--key PATH] [--mode M] [--seed N]   Same signed-receipt suite as `heso open`");
    println!("  heso batch [open|read] <urls...>");
    println!("                                Parallel multi-URL scraping in ONE process. Shared cookie jar + reqwest");
    println!("                                connection pool. JSON-Lines on stdout, completion-ordered. Default subverb");
    println!("                                is `open`. URLs may also come from stdin (one per line) when none are given.");
    println!("    [--parallel N]                Concurrent slots (default 8 for open / 2 for read, hard max 32)");
    println!("    [--timeout-per-url DUR]       Per-URL wall-clock cap (e.g. `5s`, `200ms`, `1m`; default 30s)");
    println!("    [--fail-fast]                 Stop on first error (default: continue, surface per-URL errors inline)");
    println!("    [--include CSV] [--js-fetch]  Passed through to `read` subverb");
    println!("                                Exit code: 0 if any succeeded, 1 if all failed, 2 on usage error");
    println!("  heso wait  <url> <condition>  Block until a page condition is satisfied (Playwright-style). Exit 0 ok / 1 timeout / 2 usage.");
    println!("    --selector-exists CSS          `document.querySelector(CSS) !== null`");
    println!("    --text-contains STRING         `document.body.textContent.includes(STRING)`");
    println!("    --url-matches REGEX            `window.location.href` matches REGEX (SPA route detection)");
    println!("    --network-idle [--idle-window DUR]   No queued fetch/timer for DUR (default 500ms; Playwright `networkidle` parity)");
    println!("    --time DUR                     Advance the deterministic virtual clock by DUR (e.g. `2s`, `750ms`)");
    println!("    [--timeout DUR]                Overall wall-clock cap (default 30s, Playwright default)");
    println!("  heso click  <url> (<@ref> | --text S | --selector CSS | --aria-label S)");
    println!("                                Fetch <url>, locate element by ref OR locator flag, dispatch a click.");
    println!("                                One-shot ergonomic: skips the `read` → scan → `click @e7` round-trip.");
    println!("  heso fill   <url> (<@ref> | --text S | --selector CSS | --aria-label S) <value>");
    println!("                                Fetch <url>, locate input by ref OR locator flag, set its .value and fire input+change.");
    println!("  heso submit <url> (<@form-ref> | --text S | --selector CSS | --aria-label S) [--field NAME=VALUE]... [--data JSON]");
    println!("                                Fetch <url>, locate form by ref OR locator flag, optionally pre-fill named inputs,");
    println!("                                dispatch submit, POST per enctype, return response body + status + parsed JSON.");
    println!("                                --field name=value     repeatable; matched by input `name` attribute.");
    println!("                                --data '{{\"k\":\"v\"}}'    JSON dict alternative; --field wins on the same name.");
    println!("                                File inputs are skipped (PR-X4 will ship FormData/Blob/File globals).");
    println!("  heso search <query>           Multi-backend web search: DDG HTML + Wikipedia summary (default).");
    println!("                                Pure HTTP + HTML — no JS engine spin-up. No API key required.");
    println!("    [--limit N]                    Max merged results (default 30, max 100). Wikipedia goes in");
    println!("                                   the top-level `knowledge` block, not in `results`.");
    println!("    [--engines ddg,wiki,searxng]   Pick subset (default ddg,wiki). Round-robin ranked merge,");
    println!("                                   dedupe by canonical URL.");
    println!("    [--searx-url URL]              Optional SearXNG base URL. Also reads HESO_SEARX_URL env.");
    println!("                                   Most public instances disable JSON output by default.");
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
    println!("                                Async patterns: the engine deep-resolves Promises in the returned value, so all of");
    println!("                                  (a) `(async () => {{ const r = await fetch(URL); return await r.json(); }})()`,");
    println!("                                  (b) `fetch(URL).then(r => r.json())`,");
    println!("                                  (c) `[fetch(URL).then(r => r.json()), fetch(URL2).then(r => r.json())]`");
    println!("                                resolve to their data before serialization. Bare side-effect reads like");
    println!("                                `globalThis.X = null; fetch(URL).then(j => globalThis.X = j); globalThis.X` will NOT");
    println!("                                work — the final expression captures `null` before the .then fires. Use shape (a).");
    println!("  heso serve                    Long-running JSON-RPC server over stdin/stdout (framework integration)");
    println!("  heso action-hash <url> [actions-json | -]");
    println!("                                Keyless, deterministic fingerprint over (URL, actions). Two");
    println!("                                strangers doing the same actions on the same site get the same");
    println!("                                hash — no key, no server, no clock. Output is a tamper-evident");
    println!("                                JSON with site_id / action_ids[] / trace_id (the headline hash).");
    println!("  heso action-hash-verify <file>");
    println!("                                Verify a saved fingerprint file (exit 0 valid, 1 invalid, 2 malformed)");
    println!("  heso replay <fingerprint.json>");
    println!("                                Re-execute every action in a saved fingerprint against the live");
    println!("                                site. Refuses tampered files. Actions must use the canonical");
    println!("                                schema ({{verb: open|click|fill|submit, ...}}). Outputs a per-step");
    println!("                                session log. Stateless: each step re-fetches; URL navigation");
    println!("                                IS tracked across steps, in-page DOM mutations are not.");
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
        eprintln!("usage: heso open [--explore-links N] [--link-cap M] [--best-effort] [--inject-script JS|@FILE]... <url>");
        return ExitCode::from(2);
    }

    // Single positional `<url>` plus optional flag pairs. Walk args
    // sequentially, accept flags in either order (before or after the
    // URL), keep behavior consistent with the other heso subcommands
    // (raw arg parsing, no `clap`).
    let mut url_arg: Option<String> = None;
    let mut explore_depth: u8 = 0;
    let mut link_cap: usize = DEFAULT_LINK_CAP;
    let mut best_effort = false;
    let mut inject_scripts: Vec<String> = Vec::new();
    let mut sign_flags = receipts::SignFlags::default();
    let mut i = 0;
    while i < args.len() {
        // `--receipt PATH` / `--key PATH` / `--mode M` / `--seed N` —
        // the receipt-sign flag suite is shared across `open` and
        // `read`, so the parsing lives in [`receipts`]. The helper
        // returns how many arg slots it consumed (0 / 1 / 2); when it
        // returns `None` the flag wasn't ours and we fall through to
        // the open-specific match below.
        match receipts::try_consume_sign_flag(args, i, &mut sign_flags) {
            Ok(Some(n)) => {
                i += n;
                continue;
            }
            Ok(None) => {}
            Err(code) => return code,
        }
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
            "--best-effort" => {
                best_effort = true;
                i += 1;
            }
            "--inject-script" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--inject-script needs a value (inline JS or @filepath)");
                    return ExitCode::from(2);
                };
                match resolve_inject_script(v) {
                    Ok(body) => inject_scripts.push(body),
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag `{other}`");
                eprintln!("usage: heso open [--explore-links N] [--link-cap M] [--best-effort] [--inject-script JS|@FILE]... [--receipt PATH [--key PATH] [--mode deterministic|recording|live] [--seed N]] <url>");
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
        eprintln!("usage: heso open [--explore-links N] [--link-cap M] [--best-effort] [--inject-script JS|@FILE]... [--receipt PATH [--key PATH] [--mode deterministic|recording|live] [--seed N]] <url>");
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
            // Hard fetch failures (DNS, connection refused, HTTP error
            // before any body returned) stay non-zero even under
            // `--best-effort`: no payload was produced, so there's
            // nothing to partially return.
            eprintln!("fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // When `--inject-script` is present, run a full JS hydration pump so
    // the injected polyfill is observable to the page's inline scripts.
    // The pump also surfaces failed_scripts + console_errors_count for
    // the structured failure envelope — same shape as the no-inject
    // path below. Without inject scripts we take the cheap
    // [`hydrate_for_failure_envelope`] path (still spins QuickJS but
    // doesn't keep the session around for post-hydrate snapshots).
    let (failed_scripts, console_errors_count, post_hydrate): (
        Vec<heso_engine_js::ScriptFailure>,
        usize,
        Option<(String, Vec<heso_engine_js::ConsoleEntry>, Option<String>)>,
    ) = if !inject_scripts.is_empty() {
        let client = engine.client();
        let cookie_jar = engine.cookie_jar();
        let rt_handle = tokio::runtime::Handle::current();
        let js_engine = match heso_engine_js::JsEngine::new_with_fetch_and_cookies(
            client, rt_handle, cookie_jar,
        ) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("failed to create JS engine: {e}");
                return ExitCode::FAILURE;
            }
        };
        let session_result = heso_engine_js::JsSession::open_on_engine_with_pre_scripts(
            js_engine,
            &page.body_html,
            page.url().clone(),
            heso_engine_js::ScriptFetchPolicy::Fetch,
            &inject_scripts,
        );
        match session_result {
            Ok((session, _outcome)) => {
                // Drain the install-time console buffer BEFORE the
                // title eval — `JsEngine::eval` clears the console
                // on entry per its "fresh per call" contract, so a
                // title pull after the drain would otherwise wipe
                // the inject + page-script console output we want
                // to surface.
                let failed = session.engine().drain_script_failures();
                let console = session.engine().drain_console();
                let console_errors = console
                    .iter()
                    .filter(|e| matches!(e.level, heso_engine_js::ConsoleLevel::Error))
                    .count();
                let post_title = match session.engine().eval("document.title") {
                    Ok(outcome) => outcome
                        .value
                        .as_str()
                        .map(str::to_owned)
                        .filter(|s| !s.trim().is_empty()),
                    Err(_) => None,
                };
                let post_html = session.document_html();
                (
                    failed,
                    console_errors,
                    Some((post_html, console, post_title)),
                )
            }
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        // Run the JS-side hydration pump so script-pump errors and
        // console.error counts are observable as part of the open envelope.
        // We swallow any hydration-step engine error (rare; alloc /
        // QuickJS internals) so the static fields still ship.
        let (failed, console_errors) =
            hydrate_for_failure_envelope(&engine, &page.body_html, page.url().clone());
        (failed, console_errors, None)
    };
    let (partial, partial_reason) =
        classify_failure_envelope(&failed_scripts, console_errors_count);
    // Without `--best-effort`, today's contract is "open always
    // returns the page even if hydration errors happen" — we keep
    // that. The new partial fields are additive; the flag only
    // becomes load-bearing on `read` / `wait` where exit-code
    // semantics change.
    let _ = best_effort;

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
    attach_failure_envelope(
        &mut body,
        partial,
        partial_reason,
        &failed_scripts,
        console_errors_count,
    );
    // When `--inject-script` ran the JS hydration pass, overlay the
    // post-hydration title (in case an injected polyfill + page script
    // mutated `document.title`) and surface the console buffer so the
    // agent sees what their inject + the page scripts logged. The body
    // text is re-extracted via the same helper `cmd_read` uses so an
    // agent who passed `--inject-script` to `heso open` gets a usable
    // post-hydration text payload alongside the action graph.
    if let Some((post_html, console, post_title)) = &post_hydrate {
        let post_text = heso_engine_fetch::extract_visible_text(post_html);
        if let Some(obj) = body.as_object_mut() {
            // Overlay the post-hydration title (the JS pass may have
            // set `document.title`, which is otherwise invisible to
            // the static `tree.title`). Only swap when the JS eval
            // returned a non-empty string.
            if let Some(t) = post_title.as_ref() {
                obj.insert(
                    "title".to_owned(),
                    serde_json::Value::String(t.clone()),
                );
            }
            obj.insert(
                "text".to_owned(),
                serde_json::Value::String(post_text),
            );
            obj.insert(
                "console".to_owned(),
                serde_json::to_value(console).unwrap_or(serde_json::Value::Null),
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
    // `--receipt PATH` (P0 fix): emit a signed [`heso_trace::Receipt`]
    // alongside the verb's normal stdout JSON. The trace is a single
    // `cd <url>` primitive — the natural intent of `heso open <url>`.
    // When the flag isn't supplied this is a no-op and the verb keeps
    // its existing behavior byte-for-byte.
    if sign_flags.is_active() {
        let trace = receipts::url_trace(&url);
        if let Err(code) = receipts::emit_signed_receipt(&engine, &trace, &sign_flags).await {
            return code;
        }
    }
    print_json(&body)
}

/// Hydrate `html` against a transient [`heso_engine_js::JsSession`]
/// purely to collect [`heso_engine_js::ScriptFailure`] entries and
/// count `console.error` calls — the two structured signals the
/// best-effort failure envelope surfaces.
///
/// On success returns `(failed_scripts, console_errors_count)`. On any
/// engine-internal error (extremely rare — runtime alloc, etc.) we
/// degrade to `(empty, 0)` so the verb's static portion still ships.
/// Per the best-effort contract the caller decides the
/// `partial`/`partial_reason` envelope; this helper only gathers raw
/// data.
///
/// The hydration shares the static-path's `reqwest::Client` and cookie
/// jar so a `<script src="//cdn">` reference still resolves through
/// the same network shim. Returned vectors are owned (no engine
/// borrow leaks) since the transient session is dropped at function
/// exit.
fn hydrate_for_failure_envelope(
    fetch_engine: &FetchEngine,
    html: &str,
    page_url: Url,
) -> (Vec<heso_engine_js::ScriptFailure>, usize) {
    let client = fetch_engine.client();
    let cookie_jar = fetch_engine.cookie_jar();
    let rt_handle = tokio::runtime::Handle::current();
    let Ok(js_engine) =
        heso_engine_js::JsEngine::new_with_fetch_and_cookies(client, rt_handle, cookie_jar)
    else {
        return (Vec::new(), 0);
    };
    let Ok((session, _outcome)) = heso_engine_js::JsSession::open_on_engine(
        js_engine,
        html,
        page_url,
        heso_engine_js::ScriptFetchPolicy::Fetch,
    ) else {
        return (Vec::new(), 0);
    };
    let failed = session.engine().drain_script_failures();
    let console = session.engine().drain_console();
    let console_errors = console
        .iter()
        .filter(|e| matches!(e.level, heso_engine_js::ConsoleLevel::Error))
        .count();
    (failed, console_errors)
}

/// Decide the `partial` + `partial_reason` for the structured-failure
/// envelope based on the captured per-script failures and the count
/// of `console.error` calls.
///
/// Vocabulary (single string, per the spec contract):
///
/// - `"ok"` — no script failures.
/// - `"script_crash"` — at least one [`heso_engine_js::ScriptFailure`]
///   with reason `script_crash` (or `importmap_parse_error`, which
///   shares the shape and is reported under the same bucket because
///   it's still a code-execution problem).
/// - `"fetch_failed"` — at least one fetch-failed entry and no
///   script_crash earlier in document order. A page with both
///   surfaces `script_crash` because that's the more actionable
///   signal (page DID run something and crashed; a fetch failure is
///   a missing prerequisite).
/// - Console-only errors with no failed scripts still report `"ok"`
///   for `partial_reason` — the agent can read
///   `console_errors_count > 0` directly. We surface only structural
///   failures here; soft signals stay informational.
pub(crate) fn classify_failure_envelope(
    failed_scripts: &[heso_engine_js::ScriptFailure],
    _console_errors_count: usize,
) -> (bool, &'static str) {
    for f in failed_scripts {
        match f.reason.as_str() {
            "script_crash" | "importmap_parse_error" => {
                return (true, "script_crash");
            }
            "fetch_failed" => {
                return (true, "fetch_failed");
            }
            _ => {}
        }
    }
    (false, "ok")
}

/// Attach the structured-failure envelope fields to `body`. Always
/// emits the fields (per the schema bump) — a clean run sees
/// `partial: false`, `partial_reason: "ok"`, `failed_scripts: []`,
/// and `console_errors_count: 0`.
pub(crate) fn attach_failure_envelope(
    body: &mut serde_json::Value,
    partial: bool,
    partial_reason: &str,
    failed_scripts: &[heso_engine_js::ScriptFailure],
    console_errors_count: usize,
) {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("partial".to_owned(), serde_json::Value::Bool(partial));
        obj.insert(
            "partial_reason".to_owned(),
            serde_json::Value::String(partial_reason.to_owned()),
        );
        obj.insert(
            "failed_scripts".to_owned(),
            serde_json::to_value(failed_scripts).unwrap_or(serde_json::Value::Array(Vec::new())),
        );
        obj.insert(
            "console_errors_count".to_owned(),
            serde_json::Value::Number(serde_json::Number::from(console_errors_count)),
        );
    }
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
/// # Async patterns
///
/// `<js>` may return a Promise (or an array / plain object containing
/// Promises); the engine's `__hesoDeepResolve` wrap awaits every
/// thenable in the returned tree before serializing. Concretely, all
/// of these now serialize to their resolved data, not `{}`:
///
/// - `(async () => { const r = await fetch(URL); return await r.json(); })()`
/// - `fetch(URL).then(r => r.json())`
/// - `[fetch(URL1).then(r => r.json()), fetch(URL2).then(r => r.json())]`
/// - `{ a: fetch(URL1).then(r => r.text()), b: 42 }`
///
/// **What still does not work:** reading a side-effected global
/// synchronously after a `.then(...)` that has not fired yet. The
/// final expression is captured at eval time, BEFORE the fetch
/// resolves; the queue drains *after* the value is captured. Example
/// of what NOT to do:
///
/// ```text
/// // BROKEN — `globalThis.__r` is read synchronously as `null`.
/// globalThis.__r = null;
/// fetch(URL).then(r => r.json()).then(j => { globalThis.__r = j; });
/// globalThis.__r
/// ```
///
/// Wrap in an async IIFE instead:
///
/// ```text
/// // WORKS — the IIFE returns a Promise the engine awaits.
/// (async () => {
///     const r = await fetch(URL);
///     return await r.json();
/// })()
/// ```
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
            // Share the *same* cookie jar reqwest's `cookie_provider`
            // is wired against (see `FetchEngine::cookie_jar`). This is
            // what makes `document.cookie` reads observe `Set-Cookie`
            // responses AND makes JS `document.cookie = ...` writes
            // travel on the next `fetch()` — login flows depend on it.
            heso_engine_js::JsEngine::new_with_fetch_and_cookies(
                client,
                rt_handle,
                fetch_engine.cookie_jar(),
            )
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

    // Set the page URL so the inline-script pump can resolve
    // relative `<script src="...">` refs against it.
    js_engine.set_base_url(Some(final_url.clone()));

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

/// `heso read <url>` — agent-facing one-call page report.
///
/// Returns a JSON envelope that's a strict superset of `heso open`:
/// the static fields (url, title, description, metadata, tree,
/// actions, plat_hash) PLUS post-hydration extras an agent typically
/// wants in one shot:
///
/// - `text` — full visible body text, scripts/styles stripped.
/// - `forms` — every `<form>` grouped with its inputs and submit
///   button (derived from the action graph; the WHATWG "successful
///   control" set).
/// - `cookies` — non-`HttpOnly` cookies visible to the page URL,
///   matching what `document.cookie` would return in a real browser
///   (per WHATWG HTML §6.1).
/// - `console` — every `console.*` entry the page's inline scripts
///   produced during hydration.
/// - `framework` — best-effort stack sniff (`next.js`, `nuxt`,
///   `astro`, `remix`, `vue`, `react`, or `vanilla`) from
///   [`crate::detect_framework`].
/// - `scripts` — `{executed, executed_with_error, external_handled,
///   skipped_non_script_type}` from the page's script-execution
///   pass; identical shape to `heso eval-dom`'s `scripts` field.
///
/// `--include` filters the optional fields. By default all of the
/// above ship; pass `--include text,actions,cookies` (etc.) to trim
/// the envelope for a smaller payload.
///
/// Eliminates the `open → find → eval-dom → eval-dom` call burn an
/// agent would otherwise do to reconstruct the same picture.
async fn cmd_read(args: &[String]) -> ExitCode {
    // Order-tolerant flag walk, same shape as `cmd_open`. Positional:
    // exactly one `<url>`. Flags:
    //   `--include CSV` (additive whitelist of optional fields)
    //   `--since <prev_content_hash>` (cross-call diff trigger —
    //   populates `delta` against the prior snapshot, or returns
    //   `delta.since_matched: false` with everything-added when no
    //   prior snapshot exists in this process)
    //   `--best-effort` (structured failure envelope + non-zero exit on
    //   script crashes)
    //   `--inject-script JS|@FILE` (repeatable: each entry runs after
    //   engine bootstrap, before page `<script>`)
    //   `--complete` (auto-scroll load loop: fire pending
    //   IntersectionObservers + click any "Load more" actions and
    //   wait for the DOM to stop changing, capped at 10 iter / 15s).
    let mut url_arg: Option<String> = None;
    let mut include_csv: Option<String> = None;
    let mut since_arg: Option<String> = None;
    let mut best_effort = false;
    let mut inject_scripts: Vec<String> = Vec::new();
    let mut complete = false;
    let mut sign_flags = receipts::SignFlags::default();
    let mut i = 0;
    while i < args.len() {
        // Receipt-sign flag suite — shared with `cmd_open`. The helper
        // returns how many arg slots it consumed; on `None` we fall
        // through to the read-specific match below.
        match receipts::try_consume_sign_flag(args, i, &mut sign_flags) {
            Ok(Some(n)) => {
                i += n;
                continue;
            }
            Ok(None) => {}
            Err(code) => return code,
        }
        match args[i].as_str() {
            "--include" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--include needs a value (comma-separated list)");
                    return ExitCode::from(2);
                };
                include_csv = Some(v.clone());
                i += 2;
            }
            "--since" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--since needs a content_hash value (e.g. blake3:abc...)");
                    return ExitCode::from(2);
                };
                since_arg = Some(v.clone());
                i += 2;
            }
            "--best-effort" => {
                best_effort = true;
                i += 1;
            }
            "--inject-script" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--inject-script needs a value (inline JS or @filepath)");
                    return ExitCode::from(2);
                };
                match resolve_inject_script(v) {
                    Ok(body) => inject_scripts.push(body),
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            }
            "--complete" | "--auto-scroll" => {
                // Both names accepted; `--complete` is the documented
                // primary, `--auto-scroll` is a friendly alias.
                complete = true;
                i += 1;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag `{other}`");
                eprintln!("usage: heso read [--include text,forms,cookies,console,framework,scripts] [--since <prev_hash>] [--best-effort] [--inject-script JS|@FILE]... [--complete] <url>");
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
        eprintln!("usage: heso read [--include ...] [--since <prev_hash>] [--best-effort] [--inject-script JS|@FILE]... [--complete] <url>");
        return ExitCode::from(2);
    };

    let url = match Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{url_str}`: {e}");
            return ExitCode::from(2);
        }
    };

    let include = parse_include_filter(include_csv.as_deref());

    let fetch_engine = match FetchEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to build fetch engine: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Static path: gives us url/title/meta/tree/actions/inline_data
    // plus the raw HTML for the JS-side hydration pass below.
    let page = match fetch_engine.open(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // JS-side hydration pass: build a JsSession against the fetched
    // HTML, run inline scripts, capture console output. The session's
    // engine shares the FetchEngine's cookie jar so `document.cookie`
    // reads observe the same Set-Cookie responses we just received.
    let client = fetch_engine.client();
    let cookie_jar = fetch_engine.cookie_jar();
    let rt_handle = tokio::runtime::Handle::current();
    let js_engine =
        match heso_engine_js::JsEngine::new_with_fetch_and_cookies(client, rt_handle, cookie_jar) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("failed to create JS engine: {e}");
                return ExitCode::FAILURE;
            }
        };
    // Under `--best-effort` we never let a hydration engine error
    // sink the verb — agent can still use the static portion of the
    // page (title/tree/actions/cookies). Without the flag, today's
    // behavior was to bail on a hydrate failure; we preserve that.
    // The `_with_pre_scripts` variant additionally surfaces
    // `--inject-script #N threw: ...` errors on stderr — the
    // structured message names the offending pre-script index.
    // `mut` so `run_auto_scroll_loop` (--complete) can pass it as
    // `&mut JsSession`.
    let (mut session, script_outcome) = match heso_engine_js::JsSession::open_on_engine_with_pre_scripts(
        js_engine,
        &page.body_html,
        page.url().clone(),
        heso_engine_js::ScriptFetchPolicy::Fetch,
        &inject_scripts,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            if best_effort {
                // Surface a synthetic failure envelope and exit 0 with
                // the static fields the static fetch already produced.
                // No DOM session means no post-hydration text/forms/
                // cookies — we still ship the static tree + plat_hash
                // so the agent has something to inspect.
                let mut body = serde_json::json!({
                    "url": page.url().as_str(),
                    "title": page.tree.title,
                    "description": page.tree.description,
                    "metadata": page.metadata,
                    "tree": page.tree,
                    "actions": page.actions,
                });
                let synthetic_failure = heso_engine_js::ScriptFailure {
                    url: None,
                    reason: "script_crash".to_owned(),
                    message: format!("hydrate failed: {e}"),
                    line: None,
                };
                let failed = vec![synthetic_failure];
                attach_failure_envelope(&mut body, true, "script_crash", &failed, 0);
                let hash = heso_engine_fetch::plat_hash(&body);
                if let Some(obj) = body.as_object_mut() {
                    obj.insert("plat_hash".to_owned(), serde_json::Value::String(hash));
                }
                return print_json(&body);
            }
            // The error's Display names the offending --inject-script
            // index when the engine flagged a pre-script throw, so a
            // bare `{e}` is informative enough here.
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let mut console = session.engine().drain_console();
    let failed_scripts = session.engine().drain_script_failures();
    let mut post_html = session.document_html();
    // Action graph the rest of the envelope speaks against. Starts as
    // the static-side extraction so plain `read` matches its previous
    // shape exactly; under `--complete` we'll re-extract after the
    // load loop so any newly-appended interactive elements get refs.
    let mut current_actions = page.actions.clone();

    // ---- lazy_hints (always emit) ----
    // Heuristics computed from the post-hydration DOM + JS-side IO
    // registry. An agent reading `more_content_likely: true` should
    // either call `read --complete` (we run the loop for them) or
    // step the page manually with `click @eN` on the surfaced
    // load-more refs.
    let mut lazy_hints = compute_lazy_hints(session.engine(), &post_html, &current_actions);

    // ---- --complete: run the load loop ----
    // The loop only runs when the heuristic detected something
    // worth loading. Otherwise we early-out with `stop_reason:
    // "no_lazy_content"` so the agent sees an honest "I checked,
    // there's nothing more here" signal.
    let scroll_summary = if complete {
        Some(run_auto_scroll_loop(
            &mut session,
            &mut lazy_hints,
            &mut current_actions,
            &mut console,
            &mut post_html,
        ))
    } else {
        None
    };

    // `console_errors_count` must be computed AFTER the load loop so
    // post-loop errors land in the best-effort envelope too.
    let console_errors_count = console
        .iter()
        .filter(|e| matches!(e.level, heso_engine_js::ConsoleLevel::Error))
        .count();

    // Build the envelope. Start from the same base as `cmd_open`,
    // then layer the agent-facing extras on top per `include`.
    let mut body = serde_json::json!({
        "url": page.url().as_str(),
        "title": page.tree.title,
        "description": page.tree.description,
        "metadata": page.metadata,
        "tree": page.tree,
        "actions": current_actions,
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
    // Always compute visible_text + forms — they feed `content_hash`
    // and the `--since` snapshot store even when the include filter
    // would have dropped them from the user-visible body. The body
    // gates them per `include`, the hash always sees them.
    // Compute against the POST-LOOP state (`current_actions` +
    // `post_html`) so `--complete` is reflected in the hash and in
    // every reader-facing field. When `--complete` is off,
    // `current_actions` == `page.actions` clone, so this is a no-op
    // change for the plain `read` path.
    let visible_text = heso_engine_fetch::extract_visible_text(&post_html);
    let forms_json = group_forms(&current_actions);

    if include.text {
        body["text"] = serde_json::Value::String(visible_text.clone());
    }
    if include.forms {
        body["forms"] = forms_json.clone();
    }
    if include.cookies {
        body["cookies"] = collect_cookies(&fetch_engine, page.url());
    }
    if include.console {
        body["console"] = serde_json::to_value(&console).unwrap_or(serde_json::Value::Null);
    }
    if include.framework {
        body["framework"] = serde_json::Value::String(detect_framework(&page));
    }
    if include.scripts {
        body["scripts"] = serde_json::json!({
            "executed": script_outcome.executed,
            "executed_with_error": script_outcome.executed_with_error,
            "external_handled": script_outcome.external_handled,
            "skipped_non_script_type": script_outcome.skipped_non_script_type,
        });
    }

    // content_hash + delta — see `ReadSnapshot` / `compute_content_hash`.
    // One-shot CLI has no snapshot store, so `--since` always yields
    // `since_matched: false` (agent treats it as "fresh page, here's
    // everything"). The serve path is where a true diff materializes.
    // Snap is built off the post-loop `current_actions` + `forms_json`
    // so `content_hash` shifts when --complete loaded more content.
    let snap = ReadSnapshot::from_parts(
        &page.tree.title,
        &visible_text,
        &current_actions,
        &forms_json,
    );
    let delta = match since_arg.as_deref() {
        Some(_prev_hash) => delta_no_prior(&snap),
        None => serde_json::Value::Null,
    };
    if let Some(obj) = body.as_object_mut() {
        obj.insert(
            "content_hash".to_owned(),
            serde_json::Value::String(snap.content_hash.clone()),
        );
        obj.insert("delta".to_owned(), delta);
    }

    // Structured-failure envelope (`partial`, `partial_reason`,
    // `failed_scripts`, `console_errors_count`). Always present per
    // the schema bump. Under `--best-effort` we additionally guarantee
    // exit 0 — the existing happy path already returns success here
    // since `JsSession::open_on_engine` succeeded.
    let (partial, partial_reason) =
        classify_failure_envelope(&failed_scripts, console_errors_count);
    attach_failure_envelope(
        &mut body,
        partial,
        partial_reason,
        &failed_scripts,
        console_errors_count,
    );
    // `best_effort` is consumed in the hydrate-failure short-circuit
    // above; on this path we always exit 0 anyway (the only way to
    // reach here is JsSession::open success).
    let _ = best_effort;

    // lazy_hints always emits; scroll only under --complete.
    body["lazy_hints"] = serde_json::to_value(&lazy_hints).unwrap_or(serde_json::Value::Null);
    if let Some(s) = scroll_summary {
        body["scroll"] = serde_json::to_value(&s).unwrap_or(serde_json::Value::Null);
    }


    // plat_hash last — same canonical form as `heso open` so an
    // agent that already trusts an `open` plat can verify a `read`
    // payload identically.
    let hash = heso_engine_fetch::plat_hash(&body);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("plat_hash".to_owned(), serde_json::Value::String(hash));
    }
    // `--receipt PATH` (P0 fix): emit a signed [`heso_trace::Receipt`]
    // alongside the stdout JSON. Same shape as `cmd_open`; the trace
    // is a single `cd <url>` primitive matching the user's intent.
    if sign_flags.is_active() {
        let trace = receipts::url_trace(&url);
        if let Err(code) =
            receipts::emit_signed_receipt(&fetch_engine, &trace, &sign_flags).await
        {
            return code;
        }
    }
    print_json(&body)
}

/// Bitfield of `read`-envelope optional fields. Defaults to "all on";
/// `--include text,actions,...` flips back to "only the listed ones".
/// Required fields (`url`, `title`, `meta`, `tree`, `actions`,
/// `plat_hash`) are always emitted — only the agent-extras are
/// gateable.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IncludeFilter {
    pub(crate) text: bool,
    pub(crate) forms: bool,
    pub(crate) cookies: bool,
    pub(crate) console: bool,
    pub(crate) framework: bool,
    pub(crate) scripts: bool,
}

impl IncludeFilter {
    pub(crate) fn all() -> Self {
        Self {
            text: true,
            forms: true,
            cookies: true,
            console: true,
            framework: true,
            scripts: true,
        }
    }
}

pub(crate) fn parse_include_filter(csv: Option<&str>) -> IncludeFilter {
    let Some(csv) = csv else {
        return IncludeFilter::all();
    };
    let mut f = IncludeFilter {
        text: false,
        forms: false,
        cookies: false,
        console: false,
        framework: false,
        scripts: false,
    };
    for token in csv.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match token {
            "text" => f.text = true,
            "forms" => f.forms = true,
            "cookies" => f.cookies = true,
            "console" => f.console = true,
            "framework" => f.framework = true,
            "scripts" => f.scripts = true,
            // Silently ignore unknown tokens — the contract is
            // "additive whitelist of the optional surface"; an agent
            // passing `actions` (which always ships) shouldn't fail.
            _ => {}
        }
    }
    f
}

// ============================================================================
// read_diff — content_hash + --since cross-call state-diff
// ============================================================================

/// A minimal frozen view of a `heso read` envelope, sufficient to:
/// (a) compute the `content_hash` deterministically, and
/// (b) diff against a later `read` to produce the `delta` field.
///
/// Stored per-URL on the `serve` session (LRU 8) so a follow-up `read`
/// with `--since <hash>` against the same URL can be compared without
/// re-fetching anything.
#[derive(Debug, Clone)]
pub(crate) struct ReadSnapshot {
    pub(crate) content_hash: String,
    pub(crate) title: String,
    pub(crate) text: String,
    /// `(ref_id, name)` pairs from the action graph, in the same order
    /// the action graph emitted them (document order). Diff treats this
    /// as a set keyed by `(ref_id, name)`.
    pub(crate) actions: Vec<(String, Option<String>)>,
    /// The post-`group_forms` JSON value. Deep-eq is enough for
    /// `forms_changed`.
    pub(crate) forms: serde_json::Value,
}

impl ReadSnapshot {
    /// Construct from the live envelope pieces.
    pub(crate) fn from_parts(
        title: &str,
        text: &str,
        actions: &[heso_engine_fetch::ElementRef],
        forms_json: &serde_json::Value,
    ) -> Self {
        let actions: Vec<(String, Option<String>)> = actions
            .iter()
            .map(|el| (el.ref_id.clone(), el.name.clone()))
            .collect();
        let content_hash = compute_content_hash(title, text, &actions, forms_json);
        Self {
            content_hash,
            title: title.to_owned(),
            text: text.to_owned(),
            actions,
            forms: forms_json.clone(),
        }
    }
}

/// BLAKE3 over a deterministic canonical byte string built from:
/// title, visible-text, actions sorted by `ref_id` then `name`, and
/// forms (sorted-by-ref with sorted-by-name inputs). Returns
/// `"blake3:<64-hex>"`.
///
/// The canonical form uses `\x01` as record separator and `\x00` as
/// field separator — neither can appear inside the inputs (HTML
/// extraction strips control bytes; ref ids are `@eN` ASCII).
pub(crate) fn compute_content_hash(
    title: &str,
    text: &str,
    actions: &[(String, Option<String>)],
    forms_json: &serde_json::Value,
) -> String {
    let mut hasher = blake3::Hasher::new();
    // 1. title
    hasher.update(b"title\x00");
    hasher.update(title.as_bytes());
    hasher.update(b"\x01");
    // 2. visible text
    hasher.update(b"text\x00");
    hasher.update(text.as_bytes());
    hasher.update(b"\x01");
    // 3. actions, sorted by (ref_id, name) — order-tolerant because the
    //    action graph order can shift with DOM mutations even when the
    //    set is the same; the agent-facing semantics is "what's
    //    actionable on this page" regardless of which order we walked.
    let mut sorted_actions: Vec<&(String, Option<String>)> = actions.iter().collect();
    sorted_actions.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.as_deref().unwrap_or("").cmp(b.1.as_deref().unwrap_or("")))
    });
    hasher.update(b"actions\x00");
    for (ref_id, name) in &sorted_actions {
        hasher.update(ref_id.as_bytes());
        hasher.update(b"\x00");
        hasher.update(name.as_deref().unwrap_or("").as_bytes());
        hasher.update(b"\x01");
    }
    // 4. forms — emit a canonical reduction: each form contributes
    //    `(ref, action, method, [(input_name, input_ref)...])`. We
    //    avoid hashing the full forms_json so that a noise-only diff
    //    in input `type` field doesn't trip content_hash.
    hasher.update(b"forms\x00");
    /// `(form.ref, form.action, form.method, sorted [(input.name, input.ref)])`.
    /// Local type alias keeps the clippy::type_complexity lint quiet
    /// without spawning a top-level type def for one call site.
    type FormKey = (String, String, String, Vec<(String, String)>);
    if let Some(arr) = forms_json.as_array() {
        let mut form_keys: Vec<FormKey> = arr
            .iter()
            .map(|f| {
                let ref_id = f.get("ref").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let action = f.get("action").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let method = f.get("method").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let inputs: Vec<(String, String)> = f
                    .get("inputs")
                    .and_then(|v| v.as_array())
                    .map(|inputs| {
                        let mut v: Vec<(String, String)> = inputs
                            .iter()
                            .map(|i| {
                                (
                                    i.get("name").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
                                    i.get("ref").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
                                )
                            })
                            .collect();
                        v.sort();
                        v
                    })
                    .unwrap_or_default();
                (ref_id, action, method, inputs)
            })
            .collect();
        form_keys.sort();
        for (ref_id, action, method, inputs) in &form_keys {
            hasher.update(ref_id.as_bytes());
            hasher.update(b"\x00");
            hasher.update(action.as_bytes());
            hasher.update(b"\x00");
            hasher.update(method.as_bytes());
            hasher.update(b"\x00");
            for (name, ref_id) in inputs {
                hasher.update(name.as_bytes());
                hasher.update(b"\x00");
                hasher.update(ref_id.as_bytes());
                hasher.update(b"\x00");
            }
            hasher.update(b"\x01");
        }
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// Compute the `delta` field by diffing `current` against `prior`.
/// All five diff slots populate:
///   - `actions_added`, `actions_removed`: shallow set-diff on `(ref, name)`.
///   - `forms_changed`, `text_changed`, `title_changed`: deep-eq booleans.
///   - `since_matched`: always `true` here (caller chose this path
///     because they found a prior snapshot).
pub(crate) fn compute_delta(
    current: &ReadSnapshot,
    prior: &ReadSnapshot,
) -> serde_json::Value {
    use std::collections::HashSet;
    let prior_set: HashSet<(&str, &str)> = prior
        .actions
        .iter()
        .map(|(r, n)| (r.as_str(), n.as_deref().unwrap_or("")))
        .collect();
    let current_set: HashSet<(&str, &str)> = current
        .actions
        .iter()
        .map(|(r, n)| (r.as_str(), n.as_deref().unwrap_or("")))
        .collect();
    let actions_added: Vec<serde_json::Value> = current
        .actions
        .iter()
        .filter(|(r, n)| !prior_set.contains(&(r.as_str(), n.as_deref().unwrap_or(""))))
        .map(|(r, n)| serde_json::json!({ "ref": r, "name": n.as_deref().unwrap_or("") }))
        .collect();
    let actions_removed: Vec<serde_json::Value> = prior
        .actions
        .iter()
        .filter(|(r, n)| !current_set.contains(&(r.as_str(), n.as_deref().unwrap_or(""))))
        .map(|(r, n)| serde_json::json!({ "ref": r, "name": n.as_deref().unwrap_or("") }))
        .collect();
    serde_json::json!({
        "since_matched": true,
        "actions_added": actions_added,
        "actions_removed": actions_removed,
        "forms_changed": current.forms != prior.forms,
        "text_changed": current.text != prior.text,
        "title_changed": current.title != prior.title,
    })
}

/// Build a `delta` for the "no prior snapshot found" branch — every
/// current action lands in `actions_added`, all flags `false`,
/// `since_matched: false`. This is what one-shot `heso read --since
/// <hash>` returns (no serve-session store to consult) AND what serve
/// returns when the supplied `since` hash didn't match any cached
/// snapshot for that URL.
pub(crate) fn delta_no_prior(current: &ReadSnapshot) -> serde_json::Value {
    let actions_added: Vec<serde_json::Value> = current
        .actions
        .iter()
        .map(|(r, n)| serde_json::json!({ "ref": r, "name": n.as_deref().unwrap_or("") }))
        .collect();
    serde_json::json!({
        "since_matched": false,
        "actions_added": actions_added,
        "actions_removed": [],
        "forms_changed": false,
        "text_changed": false,
        "title_changed": false,
    })
}

// ============================================================================
// `read` — lazy hints + auto-scroll load loop
// ============================================================================

/// One signal in `lazy_hints.load_more_actions` / `pagination_next` —
/// just `{ref, text}` so an agent can either call `read --complete` or
/// step the page manually with `click @eN`. Built from the action graph;
/// `text` is the action's accessible name (already populated by
/// [`heso_engine_fetch::actions`]).
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct LazyAction {
    #[serde(rename = "ref")]
    ref_id: String,
    text: String,
}

/// Heuristic signals that say "this page is gating content behind
/// load-on-visible / load-more / pagination." Populated unconditionally
/// in `read` output so an agent always sees them.
///
/// Field-by-field semantics:
///
/// - `intersection_observers_pending` — sum of `(observer, target)`
///   pairs registered via `IntersectionObserver.observe()` that haven't
///   been delivered an `isIntersecting: true` entry. > 0 means the
///   page is waiting on visibility to load content.
/// - `load_more_actions` — every action with an accessible name
///   matching `^(load|show|view|see)\s+(more|all|next)$` /i, or
///   `"More"` / `"Show more"` exact. Buttons AND links count.
/// - `pagination_next` — first action with `rel=next`, OR with name
///   matching `^next( ›| >|>)?$` / `^›$`. Optional.
/// - `lazy_images` — count of `<img loading="lazy">` in the post-
///   hydration HTML. >= 3 is the threshold that flips
///   `more_content_likely` on its own.
/// - `infinite_scroll_signals` — DOM class-name signals
///   (`infinite-scroll`, `virtual-list`, `lazy-load`, etc.) AND
///   `data-virtualized`-shaped attribute signals. Strings, not refs,
///   because the signal is presence-not-action.
/// - `more_content_likely` — true if any of the above evidence-based
///   signals fire. Single bit summarizing whether `read --complete`
///   would actually do something.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct LazyHints {
    intersection_observers_pending: usize,
    load_more_actions: Vec<LazyAction>,
    pagination_next: Option<LazyAction>,
    lazy_images: usize,
    infinite_scroll_signals: Vec<String>,
    more_content_likely: bool,
}

/// Compute `lazy_hints` from the post-hydration HTML, the action graph,
/// and the JS engine's IntersectionObserver registry. Pure derivation
/// over the inputs — no mutation.
pub(crate) fn compute_lazy_hints(
    engine: &heso_engine_js::JsEngine,
    post_html: &str,
    actions: &[ElementRef],
) -> LazyHints {
    let intersection_observers_pending = engine.intersection_observer_pending_count();
    let load_more_actions = find_load_more_actions(actions);
    let pagination_next = find_pagination_next(actions);
    let lazy_images = count_lazy_images(post_html);
    let infinite_scroll_signals = detect_infinite_scroll_signals(post_html);
    let more_content_likely = intersection_observers_pending > 0
        || !load_more_actions.is_empty()
        || pagination_next.is_some()
        || lazy_images >= 3
        || !infinite_scroll_signals.is_empty();
    LazyHints {
        intersection_observers_pending,
        load_more_actions,
        pagination_next,
        lazy_images,
        infinite_scroll_signals,
        more_content_likely,
    }
}

/// Case-insensitive `^(load|show|view|see)\s+(more|all|next)$` plus the
/// two exact-match conveniences `"More"` / `"Show more"` (the latter is
/// already covered by the regex but kept for clarity / future hosts).
static LOAD_MORE_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(load|show|view|see)\s+(more|all|next)$").expect("valid regex")
});

/// `^next( ›| >|>)?$` or `^›$` — case-insensitive on `next`. Picks up
/// "Next", "Next ›", "Next >", "Next>", and lone "›".
static NEXT_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"(?i)^(next( [›>]|>)?|›)$").expect("valid regex")
});

/// Find every action whose accessible `name` matches the load-more
/// regex, in document order. Buttons and links (and anything with an
/// accessible name) qualify.
fn find_load_more_actions(actions: &[ElementRef]) -> Vec<LazyAction> {
    let mut out = Vec::new();
    for a in actions {
        let Some(name) = a.name.as_deref() else { continue };
        let trimmed = name.trim();
        // Common exact-match conveniences: "More", "Show more" (also
        // caught by the regex but inexpensive to short-circuit).
        if trimmed.eq_ignore_ascii_case("more")
            || trimmed.eq_ignore_ascii_case("show more")
            || LOAD_MORE_RE.is_match(trimmed)
        {
            out.push(LazyAction {
                ref_id: a.ref_id.clone(),
                text: trimmed.to_owned(),
            });
        }
    }
    out
}

/// Find the first action with `rel=next` (HTML pagination spec) OR a
/// next-ish accessible name. Returned as `Option` — pages without
/// pagination get `None`.
fn find_pagination_next(actions: &[ElementRef]) -> Option<LazyAction> {
    for a in actions {
        // rel="next" wins over name-based fallback because it's the
        // explicit HTML link relation.
        if a.attrs
            .get("rel")
            .map(|r| {
                r.split_ascii_whitespace()
                    .any(|t| t.eq_ignore_ascii_case("next"))
            })
            .unwrap_or(false)
        {
            return Some(LazyAction {
                ref_id: a.ref_id.clone(),
                text: a.name.as_deref().unwrap_or("").trim().to_owned(),
            });
        }
    }
    for a in actions {
        let Some(name) = a.name.as_deref() else { continue };
        let trimmed = name.trim();
        if NEXT_RE.is_match(trimmed) {
            return Some(LazyAction {
                ref_id: a.ref_id.clone(),
                text: trimmed.to_owned(),
            });
        }
    }
    None
}

/// Count `<img loading="lazy">` in the post-hydration HTML. Cheap regex
/// scan — we deliberately don't re-parse the document just for one
/// number.
fn count_lazy_images(html: &str) -> usize {
    static LAZY_IMG_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?is)<img\b[^>]*\bloading\s*=\s*["']?lazy["']?"#).expect("valid regex")
    });
    LAZY_IMG_RE.find_iter(html).count()
}

/// Detect DOM-class / data-attribute signals of infinite-scroll or
/// virtual-list patterns. Returns the matched substrings (e.g.
/// `"class=infinite-scroll"`, `"data-virtualized"`). One regex scan,
/// dedup'd; order matches discovery order.
fn detect_infinite_scroll_signals(html: &str) -> Vec<String> {
    // class="...infinite-scroll..." | "virtual-list" | "lazy-load",
    // with separators `-` or `_`. Run inside class attributes.
    static CLASS_SIG_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)class\s*=\s*["'][^"']*\b((?:infinite|virtual)[-_]?(?:scroll|list)|lazy[-_]?load)\b[^"']*["']"#,
        )
        .expect("valid regex")
    });
    static DATA_VIRT_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)\bdata-virtualized\b"#).expect("valid regex")
    });
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for cap in CLASS_SIG_RE.captures_iter(html) {
        if let Some(m) = cap.get(1) {
            let token = m.as_str().to_lowercase();
            let label = format!("class={}", token);
            if seen.insert(label.clone()) {
                out.push(label);
            }
        }
    }
    if DATA_VIRT_RE.is_match(html) && seen.insert("data-virtualized".to_owned()) {
        out.push("data-virtualized".to_owned());
    }
    out
}

/// One summary record returned from [`run_auto_scroll_loop`]. Lives on
/// the `scroll` key in `read --complete` output.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ScrollSummary {
    iterations: usize,
    stop_reason: &'static str,
    elapsed_ms: u128,
    final_content_hash: String,
}

/// Hard caps for the auto-scroll loop. Both serve the same purpose
/// (don't loop forever); whichever fires first wins.
///
/// - `MAX_ITERATIONS = 10` — beyond ~10 "Load more" clicks a real
///   page is either truly infinite or has degenerated into duplicates.
///   We don't want `read --complete` to be a denial-of-service vector
///   against pages that paginate by millions of items.
/// - `MAX_ELAPSED_MS = 15_000` — 15 s of wall time. A read is meant
///   to be near-interactive; longer than this and the caller should
///   be using a different abstraction (a streaming loop, an
///   incremental crawler).
/// - `DOM_QUIET_MS = 200` — the "no mutations for X" window that
///   marks the DOM as quiet. 200 ms is the Playwright-derived
///   industry-standard quiet window for "auto-wait" loops; see
///   `https://www.browserstack.com/guide/playwright-waitforloadstate`
///   (network-idle uses 500 ms; we use 200 ms because we're not
///   waiting on network here, just on a click-handler's synchronous
///   mutations to settle).
/// - `PER_STEP_TIMEOUT_MS = 2_000` — hard cap on a single iteration's
///   quiet-wait. Bounds worst-case latency per step.
const MAX_ITERATIONS: usize = 10;
const MAX_ELAPSED_MS: u128 = 15_000;
const DOM_QUIET_MS: u64 = 200;
const PER_STEP_TIMEOUT_MS: u64 = 2_000;

/// `read --complete`'s load loop. Mutates `session` (via clicks +
/// IO flushes), updates `lazy_hints`, `actions`, `console`, and
/// `post_html` in place so the caller can serialize the final state.
///
/// Loop body, plain English:
///
/// 1. If `lazy_hints.more_content_likely` is false on entry, return
///    immediately with `stop_reason: "no_lazy_content"` and
///    `iterations: 0`. Honest signal that there's nothing to do.
/// 2. Up to `MAX_ITERATIONS` times:
///    a. Hash the current `post_html`.
///    b. Call `flush_intersection_observers()` to wake up any IO whose
///       targets were appended since last fire.
///    c. If there are surfaced "Load more" actions, click the first one.
///    d. Wait for the DOM to be quiet (no new HTML for
///       `DOM_QUIET_MS`, with a `PER_STEP_TIMEOUT_MS` ceiling).
///    e. Re-snapshot `post_html` + the action graph.
///    f. If the new hash equals the snapshot → `dom_quiet`, done.
///    g. If we hit `MAX_ITERATIONS` → `max_iterations`, done.
///    h. If we hit `MAX_ELAPSED_MS` → `timeout`, done.
///
/// Pagination ("Next" links) is INTENTIONALLY not clicked — that's a
/// page transition, a different intent than "load more on this page."
/// The hint surfaces it so the agent can choose; the loop doesn't.
pub(crate) fn run_auto_scroll_loop(
    session: &mut heso_engine_js::JsSession,
    lazy_hints: &mut LazyHints,
    actions: &mut Vec<ElementRef>,
    console: &mut Vec<heso_engine_js::ConsoleEntry>,
    post_html: &mut String,
) -> ScrollSummary {
    let start = std::time::Instant::now();
    if !lazy_hints.more_content_likely {
        return ScrollSummary {
            iterations: 0,
            stop_reason: "no_lazy_content",
            elapsed_ms: start.elapsed().as_millis(),
            final_content_hash: html_content_hash(post_html),
        };
    }

    let mut iterations = 0usize;
    let stop_reason: &'static str;
    loop {
        let elapsed_ms = start.elapsed().as_millis();
        if elapsed_ms >= MAX_ELAPSED_MS {
            stop_reason = "timeout";
            break;
        }
        let snapshot_hash = html_content_hash(post_html);

        // a) Fire any pending IntersectionObserver targets so JS that
        // gates content on visibility wakes up.
        if let Err(e) = session.engine().flush_intersection_observers() {
            // Don't kill the loop — surface the error via console
            // and move on. The pending count will tell us if anything
            // is left to do.
            eprintln!("flush IO observers failed: {e}");
        }

        // b) Click the FIRST load-more action, if any. We only click
        // one per iteration so each "Load more" handler has a clean
        // run + DOM-quiet wait. (Clicking all of them in a single
        // iteration would mask which one stopped working.)
        if let Some(la) = lazy_hints.load_more_actions.first() {
            if let Some(el) = heso_engine_fetch::resolve_action(actions, &la.ref_id) {
                if let Some(sel) = selector_for_action(el) {
                    if let Err(e) = session.click(&sel) {
                        // Same posture as IO flush: surface and
                        // continue.
                        eprintln!("auto-scroll click {} failed: {e}", la.ref_id);
                    }
                }
            }
        }

        // c) Drain JS jobs (microtasks + queued fetches) and let the
        // DOM-quiet window elapse. We do this in a tight wall-time
        // loop with `PER_STEP_TIMEOUT_MS` as the ceiling. Each pass:
        // run pending jobs; if nothing new shipped, re-snapshot and
        // check if we've held the same hash for `DOM_QUIET_MS`.
        wait_dom_quiet(session);

        // d) Re-snapshot and re-extract actions. The post-hydration
        // HTML may now include new buttons / links / inputs.
        *post_html = session.document_html();
        *actions = heso_engine_fetch::extract_actions_from_html(post_html);
        let mut new_console = session.engine().drain_console();
        console.append(&mut new_console);
        *lazy_hints = compute_lazy_hints(session.engine(), post_html, actions);

        iterations += 1;
        let new_hash = html_content_hash(post_html);
        if new_hash == snapshot_hash {
            stop_reason = "dom_quiet";
            break;
        }
        if iterations >= MAX_ITERATIONS {
            stop_reason = "max_iterations";
            break;
        }
        if start.elapsed().as_millis() >= MAX_ELAPSED_MS {
            stop_reason = "timeout";
            break;
        }
    }

    ScrollSummary {
        iterations,
        stop_reason,
        elapsed_ms: start.elapsed().as_millis(),
        final_content_hash: html_content_hash(post_html),
    }
}

/// Wait for the DOM to be quiet — no new HTML for `DOM_QUIET_MS`, with
/// a `PER_STEP_TIMEOUT_MS` ceiling. Pumps the JS engine's microtask /
/// fetch-job queue every iteration so async load-more handlers actually
/// progress.
///
/// Returns once either:
/// - the snapshot has not changed for `DOM_QUIET_MS` of wall time, or
/// - `PER_STEP_TIMEOUT_MS` total has elapsed (hard ceiling).
fn wait_dom_quiet(session: &mut heso_engine_js::JsSession) {
    let step_start = std::time::Instant::now();
    let tick = std::time::Duration::from_millis(25);
    let mut last_hash = html_content_hash(&session.document_html());
    let mut quiet_since = std::time::Instant::now();
    loop {
        let elapsed = step_start.elapsed().as_millis() as u64;
        if elapsed >= PER_STEP_TIMEOUT_MS {
            return;
        }
        // Pump pending jobs (microtasks, fetch callbacks). Errors here
        // are non-fatal — they only indicate the engine returned an
        // exception while running queued work, which the outer caller
        // will see in `console` on the next drain.
        let _ = session.engine().run_pending_jobs();
        let snapshot = session.document_html();
        let h = html_content_hash(&snapshot);
        if h != last_hash {
            last_hash = h;
            quiet_since = std::time::Instant::now();
        } else if quiet_since.elapsed().as_millis() as u64 >= DOM_QUIET_MS {
            return;
        }
        std::thread::sleep(tick);
    }
}

/// 32-bit-prefix BLAKE3 of the HTML string. We don't need the full 256
/// bits for the "did it change?" comparison; the prefix keeps the JSON
/// payload short while still effectively zero collision risk for
/// a single page-load's snapshots.
fn html_content_hash(html: &str) -> String {
    let h = blake3::hash(html.as_bytes());
    format!("blake3:{}", &h.to_hex().as_str()[..16])
}

/// Group the action-graph entries into `<form>` clusters. Each form's
/// inputs are the action-graph entries whose `section` is the form's
/// own section AND whose tag is a form control. The "submit" ref is
/// the first `button[type=submit]` / `input[type=submit]` in the form's
/// section, falling back to the first `<button>` with no explicit type.
///
/// Returns a JSON array. Each entry:
///
/// ```json
/// {
///   "ref": "@e3",
///   "action": "/login",
///   "method": "post",
///   "inputs": [{ "ref": "@e4", "tag": "input", "name": "user", "type": "text" }, ...],
///   "submit_ref": "@e5"
/// }
/// ```
pub(crate) fn group_forms(actions: &[heso_engine_fetch::ElementRef]) -> serde_json::Value {
    let mut forms = Vec::new();
    for el in actions.iter().filter(|e| e.tag == "form") {
        let action = el.attrs.get("action").cloned().unwrap_or_default();
        let method = el.attrs.get("method").cloned().unwrap_or_default();
        let mut inputs = Vec::new();
        let mut submit_ref: Option<String> = None;
        // The action graph already records `section` for every element
        // — the heading-tree path of its enclosing section. Inputs
        // INSIDE the form share the form's `section`. We also accept
        // ones nested in subsections (prefix match) so a fieldset
        // labeled `<h3>` doesn't drop its children.
        for child in actions
            .iter()
            .filter(|c| c.ref_id != el.ref_id)
            .filter(|c| starts_with_section(&c.section, &el.section))
        {
            if !is_form_control(&child.tag) {
                continue;
            }
            let is_submit = is_submit_control(child);
            let entry = serde_json::json!({
                "ref": child.ref_id,
                "tag": child.tag,
                "name": child.attrs.get("name").cloned().unwrap_or_default(),
                "type": child.attrs.get("type").cloned().unwrap_or_default(),
            });
            inputs.push(entry);
            if is_submit && submit_ref.is_none() {
                submit_ref = Some(child.ref_id.clone());
            }
        }
        let mut form = serde_json::json!({
            "ref": el.ref_id,
            "action": action,
            "method": method,
            "inputs": inputs,
        });
        if let Some(s) = submit_ref {
            form["submit_ref"] = serde_json::Value::String(s);
        }
        forms.push(form);
    }
    serde_json::Value::Array(forms)
}

fn is_form_control(tag: &str) -> bool {
    matches!(tag, "input" | "textarea" | "select" | "button")
}

/// `true` when the element is a form submission control — `<button>`
/// (default type is `submit`), `<button type="submit">`, or
/// `<input type="submit">`. Mirrors the WHATWG "submitter" fallback
/// chain ([`heso_engine_js::session::SUBMIT_DESCENDANT_FINDER_JS`]).
fn is_submit_control(el: &heso_engine_fetch::ElementRef) -> bool {
    match el.tag.as_str() {
        "button" => el
            .attrs
            .get("type")
            .map(|t| t.eq_ignore_ascii_case("submit"))
            .unwrap_or(true), // <button> default type is submit
        "input" => el
            .attrs
            .get("type")
            .map(|t| t.eq_ignore_ascii_case("submit"))
            .unwrap_or(false),
        _ => false,
    }
}

fn starts_with_section(child: &str, form: &str) -> bool {
    if form == "/" {
        // Root-level form: every section starts with `/`, so the prefix
        // match would over-match. Use the same logic as the
        // action-graph's section filter (only the form's own section).
        return child == "/";
    }
    child == form || child.starts_with(&format!("{form}/"))
}

/// Walk the fetch engine's cookie jar and emit the non-HttpOnly
/// cookies that match `url`, in the same order
/// `cookie_store::CookieStore::matches` produces. Each entry is the
/// agent-facing shape `{name, value, domain, path}` — HttpOnly is
/// dropped per WHATWG HTML §6.1 (the same filter `document.cookie`
/// applies in a real browser).
pub(crate) fn collect_cookies(engine: &FetchEngine, url: &Url) -> serde_json::Value {
    let jar = engine.cookie_jar();
    let guard = match jar.lock() {
        Ok(g) => g,
        Err(_) => return serde_json::Value::Array(Vec::new()),
    };
    let mut out = Vec::new();
    for c in guard.matches(url) {
        if matches!(c.http_only(), Some(true)) {
            continue;
        }
        out.push(serde_json::json!({
            "name": c.name(),
            "value": c.value(),
            "domain": c.domain().unwrap_or(""),
            "path": c.path().unwrap_or("/"),
        }));
    }
    serde_json::Value::Array(out)
}

/// Best-effort framework sniff. Inspects (in priority order):
///
/// 1. `page.inline_data` keys — Next.js ships `__NEXT_DATA__`, Nuxt
///    ships `__NUXT_DATA__`, Remix routes ship under `__remixContext`,
///    Apollo under `__APOLLO_STATE__`, etc. These are the canonical
///    SSR hydration payload names; an agent should treat them as
///    ground truth.
/// 2. Document body text + `<script src=...>` references in
///    `page.body_html` for client-only frameworks (Vue, React,
///    Svelte) that don't embed an SSR payload.
///
/// Returns one of `"next.js"`, `"nuxt"`, `"remix"`, `"astro"`,
/// `"react"`, `"vue"`, `"svelte"`, `"angular"`, or `"vanilla"` as the
/// fallback. Matches the public-signature patterns the official
/// projects ship (Next's `__NEXT_DATA__` is documented;
/// `window.__VUE__` / `window.React` are de facto signposts every
/// dev-tools extension uses).
pub(crate) fn detect_framework(page: &heso_engine_fetch::FetchPage) -> String {
    let inline = &page.inline_data;
    if inline.keys().any(|k| k == "__NEXT_DATA__") || inline.contains_key("__next_f") {
        return "next.js".to_owned();
    }
    if inline.contains_key("__NUXT__") || inline.contains_key("__NUXT_DATA__") {
        return "nuxt".to_owned();
    }
    if inline.contains_key("__remixContext") {
        return "remix".to_owned();
    }
    if inline.contains_key("__ACGH_DATA__") {
        return "apple-cms".to_owned();
    }
    // Astro embeds an `astro-island` attribute on the HTML — not in
    // inline_data but discoverable in the raw body.
    let html = &page.body_html;
    if html.contains("astro-island") || html.contains("data-astro-cid") {
        return "astro".to_owned();
    }
    if html.contains("__sveltekit") || html.contains("svelte-kit") {
        return "svelte".to_owned();
    }
    // Angular renders an `ng-version` attribute on the root element.
    if html.contains(" ng-version=") {
        return "angular".to_owned();
    }
    // Vue's hydration payload is `window.__VUE_SSR_CONTEXT__` or a
    // mount tag `<div id="app" data-server-rendered="true">`. Vue 3's
    // SFC compiler also emits `data-v-` scoped CSS class prefixes.
    if html.contains("__VUE__")
        || html.contains("__VUE_SSR_CONTEXT__")
        || html.contains("data-server-rendered=\"true\"")
        || html.contains(" data-v-")
    {
        return "vue".to_owned();
    }
    // React leaves no inline hydration payload on its own (apps that
    // use it ship one via Next/Remix above); but client-only React
    // apps tend to mount on `#root` and ship a `react.production.min.js`
    // or load via `unpkg.com/react`. Both signals are weak — we report
    // only when at least one is present.
    if html.contains("data-reactroot")
        || html.contains("react.production")
        || html.contains("react.development")
    {
        return "react".to_owned();
    }
    "vanilla".to_owned()
}

/// `heso wait <url> --selector-exists "#dashboard" [--timeout 5s]` —
/// block until a page condition is satisfied.
///
/// Five condition types:
///
/// - `--selector-exists CSS` — `document.querySelector(CSS) !== null`.
/// - `--text-contains STRING` — `document.body.textContent.includes(STRING)`.
/// - `--url-matches REGEX` — `window.location.href` matches the regex
///   (useful for SPA route changes via `pushState`).
/// - `--network-idle [--idle-window 500ms]` — no pending `fetch()`
///   for `idle_window` ms. Mirrors Playwright's `networkidle` semantics.
/// - `--time DURATION` — advance the virtual clock by `DURATION`.
///   Deterministic (no wall-time waste), so hydration-by-setTimeout
///   patterns can be advanced in trace-replay without real sleep.
///
/// Output (success):
///
/// ```json
/// { "ok": true, "elapsed_ms": 1450, "condition": "selector-exists #dashboard" }
/// ```
///
/// On timeout:
///
/// ```json
/// { "ok": false, "elapsed_ms": 5000, "condition": "...", "error": "timeout" }
/// ```
///
/// Default `--timeout` is 30 s, matching Playwright's
/// `page.waitForSelector` default. Exit code: 0 on `ok=true`, 1 on
/// timeout, 2 on usage error.
async fn cmd_wait(args: &[String]) -> ExitCode {
    let mut url_arg: Option<String> = None;
    let mut selector_exists: Option<String> = None;
    let mut text_contains: Option<String> = None;
    let mut url_matches: Option<String> = None;
    let mut network_idle = false;
    let mut idle_window: Option<u64> = None;
    let mut time_value: Option<String> = None;
    let mut timeout: Option<String> = None;
    let mut best_effort = false;
    let mut inject_scripts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--selector-exists" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--selector-exists needs a value");
                    return ExitCode::from(2);
                };
                selector_exists = Some(v.clone());
                i += 2;
            }
            "--inject-script" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--inject-script needs a value (inline JS or @filepath)");
                    return ExitCode::from(2);
                };
                match resolve_inject_script(v) {
                    Ok(body) => inject_scripts.push(body),
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            }
            "--text-contains" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--text-contains needs a value");
                    return ExitCode::from(2);
                };
                text_contains = Some(v.clone());
                i += 2;
            }
            "--url-matches" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--url-matches needs a value");
                    return ExitCode::from(2);
                };
                url_matches = Some(v.clone());
                i += 2;
            }
            "--network-idle" => {
                network_idle = true;
                i += 1;
            }
            "--idle-window" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--idle-window needs a value");
                    return ExitCode::from(2);
                };
                idle_window = match parse_duration_ms(v) {
                    Ok(ms) => Some(ms),
                    Err(e) => {
                        eprintln!("--idle-window: {e}");
                        return ExitCode::from(2);
                    }
                };
                i += 2;
            }
            "--time" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--time needs a value");
                    return ExitCode::from(2);
                };
                time_value = Some(v.clone());
                i += 2;
            }
            "--timeout" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--timeout needs a value");
                    return ExitCode::from(2);
                };
                timeout = Some(v.clone());
                i += 2;
            }
            "--best-effort" => {
                best_effort = true;
                i += 1;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag `{other}`");
                eprintln!("usage: heso wait <url> [--selector-exists CSS | --text-contains STR | --url-matches REGEX | --network-idle | --time DUR] [--timeout DUR] [--best-effort] [--inject-script JS|@FILE]...");
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

    // Build the WaitCondition. Exactly one of the five condition
    // flags must be set.
    let condition_flag_count = [
        selector_exists.is_some(),
        text_contains.is_some(),
        url_matches.is_some(),
        network_idle,
        time_value.is_some(),
    ]
    .iter()
    .filter(|b| **b)
    .count();
    if condition_flag_count != 1 {
        eprintln!(
            "heso wait: exactly one of --selector-exists / --text-contains / --url-matches / --network-idle / --time is required (got {condition_flag_count})"
        );
        return ExitCode::from(2);
    }

    let condition = if let Some(css) = selector_exists {
        heso_engine_js::WaitCondition::SelectorExists(css)
    } else if let Some(needle) = text_contains {
        heso_engine_js::WaitCondition::TextContains(needle)
    } else if let Some(pat) = url_matches {
        match regex::Regex::new(&pat) {
            Ok(re) => heso_engine_js::WaitCondition::UrlMatches(re),
            Err(e) => {
                eprintln!("--url-matches: invalid regex: {e}");
                return ExitCode::from(2);
            }
        }
    } else if network_idle {
        heso_engine_js::WaitCondition::NetworkIdle {
            idle_window_ms: idle_window.unwrap_or(heso_engine_js::wait_for::DEFAULT_NETWORK_IDLE_WINDOW_MS),
        }
    } else {
        // --time
        let duration_ms = match parse_duration_ms(time_value.as_deref().unwrap_or("")) {
            Ok(ms) => ms,
            Err(e) => {
                eprintln!("--time: {e}");
                return ExitCode::from(2);
            }
        };
        heso_engine_js::WaitCondition::TimeElapsed { duration_ms }
    };

    let timeout_ms = if let Some(s) = timeout.as_deref() {
        match parse_duration_ms(s) {
            Ok(ms) => ms,
            Err(e) => {
                eprintln!("--timeout: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        heso_engine_js::wait_for::DEFAULT_TIMEOUT_MS
    };

    let Some(url_str) = url_arg else {
        eprintln!("usage: heso wait <url> [condition] [--timeout DUR]");
        return ExitCode::from(2);
    };
    let url = match Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{url_str}`: {e}");
            return ExitCode::from(2);
        }
    };

    // Build a transient session against the URL.
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
    let client = fetch_engine.client();
    let cookie_jar = fetch_engine.cookie_jar();
    let rt_handle = tokio::runtime::Handle::current();
    let js_engine =
        match heso_engine_js::JsEngine::new_with_fetch_and_cookies(client, rt_handle, cookie_jar) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("failed to create JS engine: {e}");
                return ExitCode::FAILURE;
            }
        };
    let (session, _) = match heso_engine_js::JsSession::open_on_engine_with_pre_scripts(
        js_engine,
        &html,
        final_url,
        heso_engine_js::ScriptFetchPolicy::default(),
        &inject_scripts,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            // `e` carries the inject-script index when the failure was
            // a thrown polyfill; otherwise it's a normal hydrate error.
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    // The wait loop blocks the current task. We run it on a blocking
    // pool because it calls `std::thread::sleep` for cooperative
    // ticks; spawning it via `spawn_blocking` keeps the tokio runtime
    // responsive (an HTTP fetch from inside the JS page can still
    // drain). `JsSession` is `!Send` (QuickJS runtime), so we run
    // synchronously on the current thread instead. The tokio runtime
    // is multi-threaded, so other tasks keep moving.
    let outcome = match heso_engine_js::wait_for_on_engine(
        session.engine(),
        &condition,
        std::time::Duration::from_millis(timeout_ms),
        heso_engine_js::wait_for::DEFAULT_TICK_MS,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("wait failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Drain post-pump structured failures + console errors so the
    // wait envelope carries the same shape as `read`/`open` outputs.
    let console_after = session.engine().drain_console();
    let failed_scripts = session.engine().drain_script_failures();
    let console_errors_count = console_after
        .iter()
        .filter(|e| matches!(e.level, heso_engine_js::ConsoleLevel::Error))
        .count();

    let mut body = outcome.to_json();
    // Default partial classification follows the same rules as
    // `cmd_open` / `cmd_read` for the script-error case. Wait-timeout
    // is the wait-specific reason — overlay it AFTER so a timeout
    // dominates over a stale script-crash signal from earlier in
    // the page lifecycle (the spec is: "timeout-with-best-effort →
    // partial_reason wait_timeout").
    let (mut partial, mut partial_reason): (bool, &'static str) =
        classify_failure_envelope(&failed_scripts, console_errors_count);
    if !outcome.ok {
        partial = true;
        partial_reason = "wait_timeout";
    }
    attach_failure_envelope(
        &mut body,
        partial,
        partial_reason,
        &failed_scripts,
        console_errors_count,
    );

    // Exit-code policy:
    //   - outcome.ok          → exit 0 (always; same as today).
    //   - timeout + best-effort → exit 0 (new contract).
    //   - timeout, no best-effort → exit 1 (today's behavior).
    let exit = if outcome.ok || best_effort {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };
    let _ = print_json(&body);
    exit
}

/// Parse a duration string in the human-friendly shape Playwright
/// users will reach for: `1s` / `500ms` / `2m` / `750` (bare number =
/// milliseconds). Returns the duration as a `u64` of milliseconds.
///
/// Why not a crate: the parse is 12 lines and zero deps. `humantime`
/// or `duration-str` would add a transitive crate (and a new arg
/// shape — `5sec` vs `5 seconds`) for no expressivity win.
fn parse_duration_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("expected a duration like `500ms` / `5s` / `1m`".to_owned());
    }
    // Split into the numeric prefix + unit suffix. We accept fractional
    // seconds so `0.5s` works the same as `500ms`.
    let (num_part, unit) = {
        let mut end = 0;
        for (idx, c) in s.char_indices() {
            if c.is_ascii_digit() || c == '.' {
                end = idx + c.len_utf8();
            } else {
                break;
            }
        }
        (&s[..end], s[end..].trim().to_ascii_lowercase())
    };
    if num_part.is_empty() {
        return Err(format!("expected a number before the unit in `{s}`"));
    }
    let value: f64 = num_part
        .parse()
        .map_err(|e| format!("invalid number `{num_part}`: {e}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("duration must be a non-negative finite number, got `{s}`"));
    }
    let ms = match unit.as_str() {
        "" | "ms" => value,
        "s" | "sec" | "secs" | "seconds" => value * 1_000.0,
        "m" | "min" | "mins" | "minutes" => value * 60_000.0,
        other => return Err(format!("unknown duration unit `{other}` (use ms / s / m)")),
    };
    Ok(ms.round() as u64)
}

/// Resolve one `--inject-script <arg>` value into the JS source body to
/// evaluate before the page's own scripts run.
///
/// Accepted forms:
///
/// - `--inject-script "<inline JS>"` — `arg` is taken verbatim as the
///   script body. The common shape an agent reaches for —
///   `--inject-script "window.lunr = { Index: { load: () => ({}) } }"`.
/// - `--inject-script @<filepath>` — `arg` starts with a literal `@`;
///   the remainder is read as a filesystem path (relative to the
///   process CWD, or absolute), and the file's contents become the
///   script body. Empty path after `@` is rejected.
///
/// On `@file` failures the caller gets back a human-readable error
/// (file not found, permission denied, invalid UTF-8). No silent
/// fallback to "treat the literal `@filepath` as JS" — that would mask
/// a typo'd path as a script that throws `ReferenceError: filepath`.
///
/// No remote URLs: `--inject-script https://...` is a literal JS body
/// (it will throw — that's the point: an agent should be aware they
/// passed nonsense). The constraint is explicit in the design doc;
/// fetching remote scripts at flag-parse time would invert the trust
/// model.
pub(crate) fn resolve_inject_script(arg: &str) -> Result<String, String> {
    if let Some(rest) = arg.strip_prefix('@') {
        if rest.is_empty() {
            return Err("--inject-script: empty @path".to_owned());
        }
        std::fs::read_to_string(rest)
            .map_err(|e| format!("--inject-script: failed to read `{rest}`: {e}"))
    } else {
        Ok(arg.to_owned())
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
pub(crate) fn selector_for_action(el: &ElementRef) -> Option<String> {
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

/// Locator-flag bundle. Parsed by [`parse_locator_flags`] from CLI args
/// and consumed by [`resolve_target`] against the page's action graph.
/// `ref_id` is `Some` for the `@e<N>` ergonomic; the locator-flag fields
/// (`text`, `css_selector`, `aria_label`) are `Some` when the agent
/// passed `--text` / `--selector` / `--aria-label`. Exactly one of the
/// two modes must be supplied — [`resolve_target`] errors with usage
/// guidance otherwise.
pub(crate) struct LocatorTarget {
    pub(crate) ref_id: Option<String>,
    pub(crate) text: Option<String>,
    pub(crate) css_selector: Option<String>,
    pub(crate) aria_label: Option<String>,
}

impl LocatorTarget {
    fn is_empty(&self) -> bool {
        self.ref_id.is_none()
            && self.text.is_none()
            && self.css_selector.is_none()
            && self.aria_label.is_none()
    }

    fn has_locator_flag(&self) -> bool {
        self.text.is_some() || self.css_selector.is_some() || self.aria_label.is_some()
    }
}

/// Failure modes for [`resolve_target`]. Each variant carries enough
/// context for the CLI to render a clear, actionable error and exit
/// with the right code.
pub(crate) enum TargetError {
    /// Usage error: caller supplied no @ref AND no locator flags.
    NeitherRefNorLocator,
    /// Usage error: caller mixed `@ref` with a locator flag.
    RefAndLocatorMixed,
    /// `@ref` was supplied but no element with that id exists.
    UnknownRef(String),
    /// `--selector` was malformed — passes through [`LocatorError`].
    BadSelector { selector: String, message: String },
    /// Locator matched zero elements.
    NoMatch {
        text: Option<String>,
        css_selector: Option<String>,
        aria_label: Option<String>,
    },
    /// Locator matched more than one element. The Vec carries the
    /// candidate refs in document order so the agent can pick one.
    Ambiguous {
        text: Option<String>,
        css_selector: Option<String>,
        aria_label: Option<String>,
        candidates: Vec<ElementRef>,
    },
}

impl From<LocatorError> for TargetError {
    fn from(e: LocatorError) -> Self {
        match e {
            LocatorError::BadSelector { selector, message } => {
                TargetError::BadSelector { selector, message }
            }
        }
    }
}

/// Resolve a [`LocatorTarget`] against a page to exactly one
/// [`ElementRef`]. Returns owned values so the caller can drop the
/// [`FetchPage`] borrow before issuing the second HTTP fetch in the
/// click/fill/submit pipeline.
///
/// Resolution rules:
/// - `@ref` path: exact id lookup via [`heso_engine_fetch::resolve_action`].
/// - Locator flags path: combined AND-match via
///   [`heso_engine_fetch::resolve_locator`]. The result must be exactly
///   one element; zero or multiple matches produce a `TargetError`
///   carrying the candidate list for the agent's next call.
pub(crate) fn resolve_target(
    html: &str,
    actions: &[ElementRef],
    target: &LocatorTarget,
) -> Result<ElementRef, TargetError> {
    if target.is_empty() {
        return Err(TargetError::NeitherRefNorLocator);
    }
    if target.ref_id.is_some() && target.has_locator_flag() {
        return Err(TargetError::RefAndLocatorMixed);
    }
    if let Some(ref_str) = target.ref_id.as_deref() {
        let want = normalize_ref(ref_str);
        return match resolve_action(actions, &want) {
            Some(el) => Ok(el.clone()),
            None => Err(TargetError::UnknownRef(want)),
        };
    }

    // Locator-flag path. The `*_from_html` wrapper re-parses internally
    // and returns owned values, so the CLI stays free of `scraper` as
    // a direct dep.
    let mut matches = resolve_locator_from_html(
        html,
        actions,
        target.text.as_deref(),
        target.css_selector.as_deref(),
        target.aria_label.as_deref(),
    )?;
    match matches.len() {
        0 => Err(TargetError::NoMatch {
            text: target.text.clone(),
            css_selector: target.css_selector.clone(),
            aria_label: target.aria_label.clone(),
        }),
        1 => Ok(matches.remove(0)),
        _ => Err(TargetError::Ambiguous {
            text: target.text.clone(),
            css_selector: target.css_selector.clone(),
            aria_label: target.aria_label.clone(),
            candidates: matches,
        }),
    }
}

/// Normalize an `@ref` argument — accept both `@e7` and `e7`.
pub(crate) fn normalize_ref(s: &str) -> String {
    if let Some(stripped) = s.strip_prefix('@') {
        format!("@{stripped}")
    } else {
        format!("@{s}")
    }
}

/// Render a [`TargetError`] to stderr (with the candidate JSON when
/// ambiguous) and return the right [`ExitCode`]. Single source of
/// truth for the locator-failure user experience across all three
/// write verbs.
fn report_target_error(op_name: &str, err: TargetError) -> ExitCode {
    match err {
        TargetError::NeitherRefNorLocator => {
            eprintln!(
                "{op_name}: need either an `@e<N>` ref OR one of --text/--selector/--aria-label"
            );
            ExitCode::from(2)
        }
        TargetError::RefAndLocatorMixed => {
            eprintln!(
                "{op_name}: cannot combine an `@e<N>` ref with --text/--selector/--aria-label"
            );
            ExitCode::from(2)
        }
        TargetError::UnknownRef(want) => {
            eprintln!("no element at ref `{want}`");
            ExitCode::from(2)
        }
        TargetError::BadSelector { selector, message } => {
            eprintln!("invalid --selector `{selector}`: {message}");
            ExitCode::from(2)
        }
        TargetError::NoMatch {
            text,
            css_selector,
            aria_label,
        } => {
            eprintln!(
                "no element matched locator {}",
                format_locator(text.as_deref(), css_selector.as_deref(), aria_label.as_deref())
            );
            ExitCode::from(2)
        }
        TargetError::Ambiguous {
            text,
            css_selector,
            aria_label,
            candidates,
        } => {
            let n = candidates.len();
            eprintln!(
                "ambiguous: {n} elements matched locator {}",
                format_locator(text.as_deref(), css_selector.as_deref(), aria_label.as_deref())
            );
            eprintln!("candidates (use one of these refs):");
            for c in &candidates {
                // Single-line candidate: `<ref> <role> <tag> "<name>"`.
                // Cap snippet to 80 chars so a long button label
                // doesn't blow up terminal width.
                let name = c.name.as_deref().unwrap_or("");
                let snippet = if name.chars().count() > 80 {
                    let mut s: String = name.chars().take(80).collect();
                    s.push('…');
                    s
                } else {
                    name.to_owned()
                };
                eprintln!(
                    "  {} ({} {}) \"{}\"",
                    c.ref_id, c.role, c.tag, snippet
                );
            }
            ExitCode::from(2)
        }
    }
}

/// Render the supplied locator filters back as a `{k: "v"}`-ish blob
/// for error messages. We pass through `serde_json::to_string` so the
/// payload survives shell-quoting unambiguously.
fn format_locator(
    text: Option<&str>,
    css_selector: Option<&str>,
    aria_label: Option<&str>,
) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(3);
    if let Some(v) = text {
        parts.push(format!(
            "text: {}",
            serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_owned())
        ));
    }
    if let Some(v) = css_selector {
        parts.push(format!(
            "selector: {}",
            serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_owned())
        ));
    }
    if let Some(v) = aria_label {
        parts.push(format!(
            "aria-label: {}",
            serde_json::to_string(v).unwrap_or_else(|_| "\"\"".to_owned())
        ));
    }
    format!("{{ {} }}", parts.join(", "))
}

/// Parse the `--text` / `--selector` / `--aria-label` flag pairs and a
/// single optional `@ref` positional out of `args`. `extra` collects
/// every other positional (e.g. the URL, the `<value>` for `fill`),
/// preserving order. Used by `cmd_click` / `cmd_fill` / `cmd_submit`.
///
/// Returns `Err(ExitCode::from(2))` on flag-shape errors (missing
/// values, duplicate flags) and prints a usage line to stderr.
pub(crate) fn parse_locator_flags(
    args: &[String],
    op_name: &str,
) -> Result<(LocatorTarget, Vec<String>), ExitCode> {
    let mut target = LocatorTarget {
        ref_id: None,
        text: None,
        css_selector: None,
        aria_label: None,
    };
    let mut extra: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--text" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("{op_name}: --text needs a value");
                    return Err(ExitCode::from(2));
                };
                if target.text.is_some() {
                    eprintln!("{op_name}: --text passed more than once");
                    return Err(ExitCode::from(2));
                }
                target.text = Some(v.clone());
                i += 2;
            }
            "--selector" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("{op_name}: --selector needs a value");
                    return Err(ExitCode::from(2));
                };
                if target.css_selector.is_some() {
                    eprintln!("{op_name}: --selector passed more than once");
                    return Err(ExitCode::from(2));
                }
                target.css_selector = Some(v.clone());
                i += 2;
            }
            "--aria-label" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("{op_name}: --aria-label needs a value");
                    return Err(ExitCode::from(2));
                };
                if target.aria_label.is_some() {
                    eprintln!("{op_name}: --aria-label passed more than once");
                    return Err(ExitCode::from(2));
                }
                target.aria_label = Some(v.clone());
                i += 2;
            }
            // `@e<N>` style positional — capture as the ref. Multiple
            // `@e…` positionals are a usage error.
            other if other.starts_with('@') => {
                if target.ref_id.is_some() {
                    eprintln!("{op_name}: multiple `@ref` arguments");
                    return Err(ExitCode::from(2));
                }
                target.ref_id = Some(other.to_owned());
                i += 1;
            }
            _ => {
                extra.push(args[i].clone());
                i += 1;
            }
        }
    }
    Ok((target, extra))
}

/// Shared body for `heso click` / `heso fill` / `heso submit`. Fetches
/// `url`, resolves `ref_str` in the action graph, builds a CSS
/// selector, and hands `(html, selector)` to `op`. `op` is the
/// engine method to call — `dispatch_click`, `set_input_value`, or
/// `submit_form`. Prints the unified `{ok, url, value, console}` JSON
/// the existing eval-* commands use and returns a [`ExitCode`].
async fn run_dispatch<F>(url_arg: &str, target: &LocatorTarget, op_name: &str, op: F) -> ExitCode
where
    F: FnOnce(
        &heso_engine_js::JsEngine,
        &str,
        &str,
    ) -> Result<heso_engine_js::EvalOutcome, heso_engine_js::EvalError>,
{
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

    // We need BOTH the parsed action graph (to resolve @ref or locator
    // → selector) AND the raw HTML (to hand to the JS engine). `open()`
    // gives us actions + body_html in one call so the locator path
    // doesn't pay a second HTTP round-trip.
    let page = match engine.open(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let action = match resolve_target(&page.body_html, &page.actions, target) {
        Ok(a) => a,
        Err(e) => return report_target_error(op_name, e),
    };
    let want = action.ref_id.clone();
    let selector = match selector_for_action(&action) {
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
/// Locator flags (alternatives to `@ref`):
/// - `--text "<string>"` — case-insensitive substring match against the
///   element's accessible name (text/placeholder/value/aria-label).
/// - `--selector "<css>"` — CSS selector via `scraper::Selector`.
/// - `--aria-label "<string>"` — case-insensitive substring match
///   against the `aria-label` attribute.
///
/// Exit codes: 0 on success, 1 on fetch/JS failure, 2 on usage error,
/// unknown ref, zero locator matches, ambiguous matches (with the
/// candidate refs printed to stderr), or invalid CSS selector.
async fn cmd_click(args: &[String]) -> ExitCode {
    let (target, extra) = match parse_locator_flags(args, "click") {
        Ok(p) => p,
        Err(code) => return code,
    };
    if extra.is_empty() {
        eprintln!(
            "usage: heso click <url> (<@ref> | --text S | --selector CSS | --aria-label S)"
        );
        return ExitCode::from(2);
    }
    let url_arg = &extra[0];
    run_dispatch(url_arg, &target, "click", |eng, html, sel| {
        eng.dispatch_click(html, sel)
    })
    .await
}

/// `heso fill <url> (<@ref> | --text S | --selector CSS | --aria-label S) <value>`
/// — fetch <url>, locate the input by `@ref` OR a locator flag, set its
/// `value` to `<value>`, and dispatch first an `"input"` then a
/// `"change"` event (matching real browser behavior when a user types).
///
/// Output shape mirrors `heso click`: `{ok, op, url, ref, selector,
/// value, console}` where `value: true` indicates the selector
/// matched. Exit codes match `heso click`.
async fn cmd_fill(args: &[String]) -> ExitCode {
    let (target, extra) = match parse_locator_flags(args, "fill") {
        Ok(p) => p,
        Err(code) => return code,
    };
    if extra.len() < 2 {
        eprintln!(
            "usage: heso fill <url> (<@ref> | --text S | --selector CSS | --aria-label S) <value>"
        );
        return ExitCode::from(2);
    }
    let url_arg = extra[0].clone();
    let value = extra[1].clone();
    run_dispatch(&url_arg, &target, "fill", move |eng, html, sel| {
        eng.set_input_value(html, sel, &value)
    })
    .await
}

/// `heso submit <url> <@form-ref> [--field NAME=VALUE]... [--data JSON]`
/// — fetch <url>, locate the form at `@form-ref`, optionally pre-fill
/// its named inputs with the supplied values, and submit it per
/// [WHATWG HTML §4.10.22] — dispatch the `submit` event, serialize
/// the entry list per `enctype`, issue a real HTTP request through the
/// engine's shared `reqwest::Client`, follow redirects, and report the
/// post-redirect URL + status + response body.
///
/// Pre-PR-1 behavior dispatched a click on the submit button without
/// issuing any HTTP traffic — filed as the top write-side gap in
/// `agent regression testing`. PR-1 closed that. PR-X1 (this revision) closes
/// the next layer of the same gap that `agent regression testing` filed:
/// in V2 every CLI invocation was a fresh process, so `heso fill`'s
/// typed-in value never reached the next `heso submit` invocation. The
/// `--field NAME=VALUE` / `--data JSON` flags make submit a one-shot:
/// fetch + fill + submit + return-response in one process.
///
/// Flag shape:
///
/// - `--field name=value` — repeatable. Sets the form's input(s) with
///   `name="name"` to `value` before dispatching the submit event.
///   The first `=` splits name from value; the value can contain `=`
///   characters and arbitrary unicode (the shell escapes them as
///   usual). Inputs are matched by `name` attribute, not by `@eN` ref
///   — that's the WHATWG "successful control" key.
/// - `--data '{"k1":"v1","k2":"v2"}'` — JSON dict alternative when
///   the form has many fields. Each `(k, v)` is applied the same way
///   as a single `--field`. Values must be strings (numbers/booleans
///   are auto-stringified for ergonomics — `{"age": 32}` works).
/// - Both can be combined. When the same name appears in `--data` and
///   `--field`, the explicit `--field` wins (CLI flags override JSON).
/// - **File inputs are skipped** silently (with a `fieldsSkipped`
///   entry in the output). Full file upload is filed for a follow-up
///   PR that ships `FormData` / `Blob` / `File` globals.
///
/// Output: `{ok, op, url, ref, selector, value, console, postUrl}`.
/// `value` is the structured submission result:
///
/// - `{matched: false, submitted: false, reason: "no_form"}` —
///   selector didn't match.
/// - `{matched: true, defaultPrevented: true, submitted: false,
///   reason: "default_prevented"}` — a listener called
///   `event.preventDefault()`.
/// - `{matched: true, submitted: true, method, enctype, action,
///   responseStatus, responseUrl, responseBody, responseBodyTruncated,
///   responseContentType, responseJson?, fieldsApplied,
///   fieldsSkipped}` — the request went out, the response replaced
///   the session document, and we landed at `responseUrl`. The body
///   is truncated to 64 KB (with `responseBodyTruncated: true` when
///   so). `responseJson` is the parsed body when the server declared
///   `Content-Type: application/json` (or a `+json` suffix); omitted
///   otherwise. `fieldsApplied` lists names actually set;
///   `fieldsSkipped` lists name + reason (`"no_match"` or
///   `"file_input"`).
/// - `{matched: true, submitted: false, reason: "http_error", error}`
///   — the request failed (DNS, TLS, timeout, 5xx-then-redirect-cap,
///   etc.). The `error` field is the underlying reqwest message.
///
/// Exit codes: 0 if the request was either skipped (a real-browser
/// outcome — `preventDefault` is legitimate) or succeeded; 1 on HTTP
/// failure, 2 on usage error.
async fn cmd_submit(args: &[String]) -> ExitCode {
    // Order-tolerant walk: split `--field name=value` (repeatable),
    // `--data <json>`, and the locator flags (`--text` /
    // `--selector` / `--aria-label`) from the positionals (URL, plus
    // an optional `@ref`).
    let mut fields_cli: Vec<(String, String)> = Vec::new();
    let mut data_json: Option<String> = None;
    let mut target = LocatorTarget {
        ref_id: None,
        text: None,
        css_selector: None,
        aria_label: None,
    };
    let mut positional: Vec<String> = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--field" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--field needs a value of the form NAME=VALUE");
                    return ExitCode::from(2);
                };
                match v.split_once('=') {
                    Some((name, val)) => {
                        if name.is_empty() {
                            eprintln!("--field: empty name in `{v}`");
                            return ExitCode::from(2);
                        }
                        fields_cli.push((name.to_owned(), val.to_owned()));
                    }
                    None => {
                        eprintln!("--field: expected NAME=VALUE, got `{v}`");
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            }
            "--data" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--data needs a JSON dict value");
                    return ExitCode::from(2);
                };
                data_json = Some(v.clone());
                i += 2;
            }
            "--text" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("submit: --text needs a value");
                    return ExitCode::from(2);
                };
                if target.text.is_some() {
                    eprintln!("submit: --text passed more than once");
                    return ExitCode::from(2);
                }
                target.text = Some(v.clone());
                i += 2;
            }
            "--selector" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("submit: --selector needs a value");
                    return ExitCode::from(2);
                };
                if target.css_selector.is_some() {
                    eprintln!("submit: --selector passed more than once");
                    return ExitCode::from(2);
                }
                target.css_selector = Some(v.clone());
                i += 2;
            }
            "--aria-label" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("submit: --aria-label needs a value");
                    return ExitCode::from(2);
                };
                if target.aria_label.is_some() {
                    eprintln!("submit: --aria-label passed more than once");
                    return ExitCode::from(2);
                }
                target.aria_label = Some(v.clone());
                i += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown flag `{other}`");
                eprintln!(
                    "usage: heso submit <url> (<@form-ref> | --text S | --selector CSS | --aria-label S) [--field NAME=VALUE]... [--data JSON]"
                );
                return ExitCode::from(2);
            }
            other if other.starts_with('@') => {
                if target.ref_id.is_some() {
                    eprintln!("submit: multiple `@ref` arguments");
                    return ExitCode::from(2);
                }
                target.ref_id = Some(other.to_owned());
                i += 1;
            }
            _ => {
                positional.push(args[i].clone());
                i += 1;
            }
        }
    }
    if positional.is_empty() {
        eprintln!(
            "usage: heso submit <url> (<@form-ref> | --text S | --selector CSS | --aria-label S) [--field NAME=VALUE]... [--data JSON]"
        );
        return ExitCode::from(2);
    }

    // Parse the optional `--data` JSON dict into an ordered map. We
    // keep a Vec<(String,String)> so the apply order is deterministic
    // (matches the JSON key order, then `--field` flags in CLI order
    // override). Reject non-object roots and non-scalar values.
    let data_fields: Vec<(String, String)> = match data_json.as_deref() {
        None => Vec::new(),
        Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(serde_json::Value::Object(map)) => {
                let mut out = Vec::with_capacity(map.len());
                for (k, v) in map {
                    // Stringify scalars; reject arrays/objects for now
                    // (multi-valued field flags is a separate ergonomic
                    // call; the form-submit spec keys by `name` and
                    // each `name` is one string per successful control
                    // unless it's a `<select multiple>` — that case
                    // needs repeated `--field` flags today).
                    let s = match v {
                        serde_json::Value::String(s) => s,
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        serde_json::Value::Null => String::new(),
                        other => {
                            eprintln!(
                                "--data: value for `{k}` must be a string/number/bool/null, got {}",
                                other
                            );
                            return ExitCode::from(2);
                        }
                    };
                    out.push((k, s));
                }
                out
            }
            Ok(_) => {
                eprintln!("--data: expected a JSON object at the top level");
                return ExitCode::from(2);
            }
            Err(e) => {
                eprintln!("--data: invalid JSON: {e}");
                return ExitCode::from(2);
            }
        },
    };

    let merged = merge_submit_fields(&data_fields, &fields_cli);

    cmd_submit_inner(&positional[0], &target, &merged).await
}

/// Merge `--data` JSON fields with `--field NAME=VALUE` CLI flags so
/// the final apply list has `--field` winning on conflicts. Order:
/// `--data` keys first (in original JSON order, minus anything also
/// supplied via `--field`), then all `--field` flags in CLI order.
/// Last-write-wins still holds inside the JS-side apply, but pruning
/// the overridden `--data` entry keeps `fieldsApplied` clean.
pub(crate) fn merge_submit_fields(
    data_fields: &[(String, String)],
    fields_cli: &[(String, String)],
) -> Vec<(String, String)> {
    let mut merged: Vec<(String, String)> = Vec::with_capacity(data_fields.len() + fields_cli.len());
    let cli_names: std::collections::HashSet<&str> =
        fields_cli.iter().map(|(n, _)| n.as_str()).collect();
    for (n, v) in data_fields {
        if cli_names.contains(n.as_str()) {
            continue;
        }
        merged.push((n.clone(), v.clone()));
    }
    for (n, v) in fields_cli {
        merged.push((n.clone(), v.clone()));
    }
    merged
}

/// Body of [`cmd_submit`] split out so the dispatch / fetch /
/// selector-build / session-open / submit sequence is readable
/// top-to-bottom. The shape mirrors [`run_dispatch`] but takes a
/// stateful path (open a [`JsSession`], call its
/// [`heso_engine_js::JsSession::submit_with_fields`]) so the HTTP
/// response can flow back into the document AND the agent's supplied
/// `(name, value)` overrides are pre-installed on the form before the
/// submit event fires.
async fn cmd_submit_inner(
    url_arg: &str,
    target: &LocatorTarget,
    fields: &[(String, String)],
) -> ExitCode {
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

    let page = match engine.open(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("fetch failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let action = match resolve_target(&page.body_html, &page.actions, target) {
        Ok(a) => a,
        Err(e) => return report_target_error("submit", e),
    };
    let want = action.ref_id.clone();
    let selector = match selector_for_action(&action) {
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

    // Build a fetch-capable JS engine so the form submission can
    // actually go out over the wire (per PR-1). Share the same
    // `reqwest::Client` AND the same cookie jar as the static path
    // so a server's `Set-Cookie` response on the page load is sent
    // back on the form-submit `POST`, and any `document.cookie =`
    // writes the page made before submission travel on the wire.
    let client = engine.client();
    let cookie_jar = engine.cookie_jar();
    let rt_handle = tokio::runtime::Handle::current();
    let js_engine =
        match heso_engine_js::JsEngine::new_with_fetch_and_cookies(client, rt_handle, cookie_jar) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("failed to create JS engine: {e}");
                return ExitCode::FAILURE;
            }
        };

    // Open the page in a stateful session so the post-submit
    // navigation lands somewhere observable. `--seed` / determinism
    // is out of scope for PR-1; record/replay (item M) is the
    // determinism path for live writes.
    let (mut session, _open_outcome) = match heso_engine_js::JsSession::open_on_engine(
        js_engine,
        &html,
        final_url.clone(),
        heso_engine_js::ScriptFetchPolicy::default(),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("session open failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let outcome = match session.submit_with_fields(&selector, fields) {
        Ok(o) => o,
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
                "op": "submit",
                "url": final_url.to_string(),
                "ref": want,
                "selector": selector,
                "error": err_body,
            });
            let _ = serde_json::to_string_pretty(&body).map(|s| println!("{s}"));
            return ExitCode::FAILURE;
        }
    };

    let body = serde_json::json!({
        "ok": true,
        "op": "submit",
        "url": final_url.to_string(),
        "ref": want,
        "selector": selector,
        "value": outcome.value,
        "console": outcome.console,
        // Post-submit URL: when the request succeeded, this is the
        // response URL; otherwise the page we started on. Lets a
        // subsequent `heso eval-dom $POST_URL` script the result.
        "postUrl": session.url().to_string(),
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

/// `heso action-hash <url> [actions-json | -]` — derive a keyless,
/// tamper-evident fingerprint for an intended `(URL, actions)` pair.
///
/// **Two strangers doing the same actions on the same site get the same
/// hash.** Deterministic, no key, no clock, no server. See
/// [`heso_trace::trace_fingerprint`] for the algorithm (versioned
/// `heso-trace-fp/v1` — domain-separated site / action / chain steps).
///
/// Actions are a JSON array, schema-free — callers choose how to encode
/// their intent (e.g. `[{"verb":"click","ref":"@e3"}]`). Pass the array
/// inline as the second positional argument, pass `-` to read it from
/// stdin, or omit it entirely for a URL-only fingerprint.
///
/// Output: a serialized [`TraceFingerprint`] — every component (the
/// algorithm tag, normalized URL, action array, per-action `action_ids`,
/// `site_id`, and headline `trace_id`) so callers can save the JSON and
/// re-verify it later with `heso action-hash-verify`. The save is
/// tamper-evident: changing any field invalidates the recompute.
async fn cmd_action_hash(args: &[String]) -> ExitCode {
    if args.is_empty() {
        eprintln!("usage: heso action-hash <url> [actions-json | -]");
        eprintln!();
        eprintln!("Computes a keyless, deterministic fingerprint over (URL, actions).");
        eprintln!("No key, no server, no clock — two strangers doing the same actions");
        eprintln!("on the same site get the same hash.");
        eprintln!();
        eprintln!("Actions: a JSON array. Pass inline as the second arg, or `-` for stdin.");
        eprintln!("Omit it for a URL-only fingerprint (actions = []).");
        return ExitCode::from(2);
    }

    let url = match Url::parse(&args[0]) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid URL `{}`: {e}", args[0]);
            return ExitCode::from(2);
        }
    };

    let raw: Option<String> = match args.get(1).map(String::as_str) {
        None => None,
        Some("-") => {
            use std::io::Read;
            let mut buf = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
                eprintln!("failed reading stdin: {e}");
                return ExitCode::FAILURE;
            }
            Some(buf)
        }
        Some(s) => Some(s.to_owned()),
    };

    let actions: serde_json::Value = match raw {
        Some(t) => match serde_json::from_str(&t) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("actions is not valid JSON: {e}");
                return ExitCode::from(2);
            }
        },
        None => serde_json::Value::Array(Vec::new()),
    };
    if !actions.is_array() {
        eprintln!("actions must be a JSON array");
        return ExitCode::from(2);
    }

    let fp = trace_fingerprint(&url, &actions);
    let val = match serde_json::to_value(&fp) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("failed to serialize fingerprint: {e}");
            return ExitCode::FAILURE;
        }
    };
    print_json(&val)
}

/// `heso action-hash-verify <file>` — re-derive every ID in a saved
/// fingerprint file and confirm it matches.
///
/// **No key needed.** Tamper-evidence comes from the algorithm being a
/// pure function of `url` + `actions`; any drift between the stored IDs
/// and the recompute means the file was modified after it was produced.
///
/// Exit codes mirror `receipt-verify`:
/// - `0` — every component matches (`Valid`).
/// - `1` — at least one component disagrees, or the algorithm tag is
///   unknown to this version (`Mismatch` / `WrongAlgorithm`).
/// - `2` — file missing, unreadable, or not a valid fingerprint JSON
///   (`Malformed`).
async fn cmd_action_hash_verify(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: heso action-hash-verify <file>");
        return ExitCode::from(2);
    };
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("MISSING `{path}`: {e}");
            return ExitCode::from(2);
        }
    };
    let fp: TraceFingerprint = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("MALFORMED `{path}`: not a valid fingerprint JSON: {e}");
            return ExitCode::from(2);
        }
    };
    match verify_fingerprint(&fp) {
        FingerprintOutcome::Valid => {
            println!("OK {} {}", fp.algorithm, fp.trace_id);
            ExitCode::SUCCESS
        }
        FingerprintOutcome::Mismatch => {
            eprintln!(
                "INVALID `{path}`: recompute disagrees — file was modified after creation"
            );
            ExitCode::from(1)
        }
        FingerprintOutcome::WrongAlgorithm(tag) => {
            eprintln!("INVALID `{path}`: unknown algorithm tag `{tag}` (this build supports only `heso-trace-fp/v1`)");
            ExitCode::from(1)
        }
        FingerprintOutcome::Malformed(reason) => {
            eprintln!("MALFORMED `{path}`: {reason}");
            ExitCode::from(2)
        }
    }
}

// ============================================================================
// Replay — execute the actions in a fingerprint against the live site
// ============================================================================

/// `heso replay <fingerprint.json>` — re-execute every action in a saved
/// fingerprint, in order, against the live site.
///
/// **Reconstruction, with caveats.** The saved fingerprint JSON already
/// records *what* the agent intended (`url` + `actions[]`). This command
/// re-runs that intent through the same engine paths `heso click` /
/// `heso fill` / `heso submit` use today. The output is a "session
/// record" — per-step intent + outcome + URL transitions — saved as one
/// JSON document the caller can diff, archive, or hand to a downstream
/// tool.
///
/// ## What's preserved across steps
///
/// - **URL navigation.** A click on `<a href>` follows the link; the
///   next action runs against the new URL. `Open` actions navigate
///   explicitly.
///
/// ## What's NOT preserved across steps
///
/// - **In-page DOM mutations.** Each click / fill / submit re-fetches
///   the current URL and rebuilds the DOM. A `fill` that JS-handlers
///   would react to is fired, but the next step starts from a fresh
///   fetch — so if a click depends on the fill's mutation persisting,
///   that's lost. Stateful multi-action replay against one engine is a
///   follow-up.
/// - **Byte-identical results.** The live site may have changed since
///   the fingerprint was created. For byte-identical replay (record the
///   network on first run, replay against the cassette on later runs),
///   see ADR 0008 — designed, not yet implemented.
///
/// ## Refusal modes
///
/// - **Integrity:** `verify_fingerprint` runs first. A tampered file is
///   refused, exit `1`.
/// - **Schema:** actions must use the canonical [`Action`] schema
///   (`verb: open|click|fill|submit`). Schema-free fingerprints hash
///   fine but can't be auto-replayed; exit `2` with a clear message.
async fn cmd_replay(args: &[String]) -> ExitCode {
    // Parse optional `--seed N` flag (order-tolerant) and the
    // positional fingerprint path.
    let mut seed: Option<u64> = None;
    let mut path: Option<&String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                let Some(v) = args.get(i + 1) else {
                    eprintln!("--seed needs a value");
                    return ExitCode::from(2);
                };
                match v.parse::<u64>() {
                    Ok(n) => seed = Some(n),
                    Err(e) => {
                        eprintln!("--seed: invalid u64 `{v}`: {e}");
                        return ExitCode::from(2);
                    }
                }
                i += 2;
            }
            other => {
                if path.is_some() {
                    eprintln!("unexpected positional `{other}`");
                    return ExitCode::from(2);
                }
                path = Some(&args[i]);
                i += 1;
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: heso replay [--seed N] <fingerprint.json>");
        eprintln!();
        eprintln!("Re-executes every action in a saved fingerprint against the live");
        eprintln!("site. Refuses tampered files. Schema must be {{verb: open|click|fill|submit, ...}}.");
        eprintln!("--seed N seeds JsSession::open_with_seed for deterministic Math.random / crypto / timers.");
        return ExitCode::from(2);
    };
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read `{path}`: {e}");
            return ExitCode::from(2);
        }
    };
    let fp: TraceFingerprint = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("`{path}` is not a fingerprint JSON: {e}");
            return ExitCode::from(2);
        }
    };

    // Refuse to replay a fingerprint that doesn't pass its own integrity
    // check — we don't want to be a tool that re-executes tampered traces.
    match verify_fingerprint(&fp) {
        FingerprintOutcome::Valid => {}
        FingerprintOutcome::Mismatch => {
            eprintln!(
                "refusing to replay: fingerprint integrity check failed (file was modified after creation)"
            );
            return ExitCode::from(1);
        }
        FingerprintOutcome::WrongAlgorithm(tag) => {
            eprintln!("refusing to replay: unknown algorithm tag `{tag}`");
            return ExitCode::from(1);
        }
        FingerprintOutcome::Malformed(reason) => {
            eprintln!("refusing to replay: {reason}");
            return ExitCode::from(2);
        }
    }

    let actions = match parse_actions(&fp.actions) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("cannot replay: {e}");
            eprintln!();
            eprintln!("Replay handles only fingerprints whose actions use the canonical");
            eprintln!("schema: {{\"verb\": \"open|click|fill|submit\", ...}}. Hashing accepts");
            eprintln!("any JSON; replay does not.");
            return ExitCode::from(2);
        }
    };

    let mut current_url = match Url::parse(&fp.url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("fingerprint url unparseable: {e}");
            return ExitCode::FAILURE;
        }
    };

    let fetch = match FetchEngine::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("engine init failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut steps: Vec<serde_json::Value> = Vec::with_capacity(actions.len());
    let mut all_ok = true;

    // One JsSession is carried across every step so that imperative DOM
    // mutations from earlier clicks/fills/submits remain visible to
    // later ones. Initialized lazily — on first `Open`, or on the first
    // non-Open action (by fetching `current_url` and `open`-ing it).
    let mut session: Option<heso_engine_js::JsSession> = None;
    // `current_actions` is the action graph captured at the most-recent
    // navigation. It's used to resolve `@e7`-style refs. Between
    // navigations, the live in-memory DOM has been mutated by JS, so a
    // ref may point at something whose attributes have changed — that's
    // an inherent limitation of stateless refs.
    let mut current_actions: Vec<ElementRef> = Vec::new();

    for (i, action) in actions.iter().enumerate() {
        let url_before = current_url.clone();
        let res = execute_step_session(
            &fetch,
            &mut session,
            &mut current_url,
            &mut current_actions,
            action,
            seed,
        )
        .await;
        let step = match &res {
            Ok(detail) => serde_json::json!({
                "index": i,
                "verb": action.verb(),
                "action": action,
                "url_before": url_before.to_string(),
                "url_after": current_url.to_string(),
                "ok": true,
                "result": detail,
            }),
            Err(err) => serde_json::json!({
                "index": i,
                "verb": action.verb(),
                "action": action,
                "url_before": url_before.to_string(),
                "url_after": current_url.to_string(),
                "ok": false,
                "error": err,
            }),
        };
        steps.push(step);
        if res.is_err() {
            all_ok = false;
            break;
        }
    }

    let session = serde_json::json!({
        "algorithm": fp.algorithm,
        "trace_id": fp.trace_id,
        "fingerprint_valid": true,
        "start_url": fp.url,
        "final_url": current_url.to_string(),
        "steps_run": steps.len(),
        "steps_total": actions.len(),
        "ok": all_ok,
        "note": "stateful replay — one JsSession carries DOM mutations, RNG, virtual clock, and cookies (via shared reqwest::Client) across all steps. Navigation (Open, or an <a href> click) replaces the document but keeps the engine. Refs (`@e7`) are resolved against the action graph captured at the most-recent navigation — between navigations the live DOM has been mutated, so a ref may point to an element whose attributes have shifted. Submit still has the no-real-POST limitation: JsSession::submit dispatches a click on the form's submit button rather than issuing an HTTP POST. For byte-identical replay against recorded network responses, see ADR 0008 (not yet implemented).",
        "steps": steps,
    });
    if !all_ok {
        // Print the session log on stdout even on failure (the caller
        // wants to see WHICH step failed and why), but exit non-zero.
        let _ = print_json(&session);
        return ExitCode::FAILURE;
    }
    print_json(&session)
}

/// Ensure `*session` is `Some` before a non-Open action runs. If the
/// trace's first canonical action is a click/fill/submit (rather than
/// an explicit `Open`), we still need an engine + a document to dispatch
/// against — so fetch `current_url`, parse its actions, and open a fresh
/// [`JsSession`] on it. If a session already exists, this is a no-op.
async fn ensure_session(
    fetch: &FetchEngine,
    session: &mut Option<heso_engine_js::JsSession>,
    current_actions: &mut Vec<ElementRef>,
    current_url: &Url,
    seed: Option<u64>,
) -> Result<(), String> {
    if session.is_some() {
        return Ok(());
    }
    // One fetch: `body_html` is the raw bytes the JS engine wants,
    // `actions` is the action graph for `@e7` resolution. Same response.
    let page = <FetchEngine as EngineApi>::open(fetch, current_url)
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    *current_actions = page.actions;
    let (sess, _outcome) = match seed {
        Some(n) => heso_engine_js::JsSession::open_with_seed(
            &page.body_html,
            current_url.clone(),
            n,
        ),
        None => heso_engine_js::JsSession::open(&page.body_html, current_url.clone()),
    }
    .map_err(|e| format!("js session open failed: {e}"))?;
    *session = Some(sess);
    Ok(())
}

/// One step of stateful replay. Lazily initializes `session` on first
/// use, advances `current_url` / `current_actions` on every navigation
/// (`Open` or an `<a href>` click), and dispatches click/fill/submit
/// through the live [`heso_engine_js::JsSession`] — so DOM mutations
/// between steps persist (within the limits documented on
/// [`heso_engine_js::JsSession`]).
async fn execute_step_session(
    fetch: &FetchEngine,
    session: &mut Option<heso_engine_js::JsSession>,
    current_url: &mut Url,
    current_actions: &mut Vec<ElementRef>,
    action: &Action,
    seed: Option<u64>,
) -> Result<serde_json::Value, String> {
    match action {
        Action::Open { url } => {
            let new_url = Url::parse(url).map_err(|e| format!("invalid url `{url}`: {e}"))?;
            // Single fetch: `body_html` is the raw HTML for the JS
            // engine, `actions` is the action graph from the same
            // response, `url` is post-redirect.
            let page = <FetchEngine as EngineApi>::open(fetch, &new_url)
                .await
                .map_err(|e| format!("fetch failed: {e}"))?;
            *current_url = page.url().clone();
            *current_actions = page.actions.clone();
            let script_outcome = match session.as_mut() {
                None => {
                    let (sess, outcome) = match seed {
                        Some(n) => heso_engine_js::JsSession::open_with_seed(
                            &page.body_html,
                            current_url.clone(),
                            n,
                        ),
                        None => heso_engine_js::JsSession::open(
                            &page.body_html,
                            current_url.clone(),
                        ),
                    }
                    .map_err(|e| format!("js session open failed: {e}"))?;
                    *session = Some(sess);
                    outcome
                }
                Some(sess) => sess
                    .navigate(&page.body_html, current_url.clone())
                    .map_err(|e| format!("js session navigate failed: {e}"))?,
            };
            Ok(serde_json::json!({
                "op": "open",
                "navigated_to": current_url.to_string(),
                "scripts": script_outcome_json(&script_outcome),
            }))
        }
        Action::Click { target } => {
            ensure_session(fetch, session, current_actions, current_url, seed).await?;
            let want = normalize_replay_ref(target);
            let elem = resolve_action(current_actions, &want)
                .ok_or_else(|| format!("no element at ref `{want}`"))?
                .clone();

            let selector = selector_for_action(&elem)
                .ok_or_else(|| format!("no selector for ref `{want}`"))?;

            // Anchor with href: dispatch the click into JS first so SPA
            // routers (Next.js, Remix, React Router, vanilla
            // `preventDefault()` + `history.pushState`) can intercept.
            // Only when the script does NOT call preventDefault do we
            // follow up with a real navigation to the href target.
            if elem.tag == "a" && elem.attrs.contains_key("href") {
                let href = elem.attrs.get("href").cloned().unwrap_or_default();
                let sess_ref = session.as_ref().expect("session ensured above");
                let click_outcome = sess_ref
                    .click(&selector)
                    .map_err(|e| format!("js click failed: {e}"))?;
                let matched = click_outcome
                    .value
                    .get("matched")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let default_prevented = click_outcome
                    .value
                    .get("defaultPrevented")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let drift = ref_drift_field(sess_ref, &selector, &elem.tag);

                if matched && default_prevented {
                    // SPA router handled it — no real navigation.
                    let mut obj = serde_json::json!({
                        "op": "click",
                        "kind": "dom-event",
                        "ref": want,
                        "selector": selector,
                        "href": href,
                        "matched": matched,
                        "defaultPrevented": true,
                        "console": click_outcome.console,
                    });
                    if let Some(d) = drift {
                        obj.as_object_mut().unwrap().insert("ref_drift".into(), d);
                    }
                    return Ok(obj);
                }

                // Not prevented (or selector didn't match the live DOM):
                // do the real navigation. Falling through on unmatched
                // preserves the prior behavior where an anchor's href
                // always navigates.
                let target_url = current_url
                    .join(&href)
                    .map_err(|e| format!("href `{href}` is not a valid URL: {e}"))?;
                let from = current_url.to_string();
                let page = <FetchEngine as EngineApi>::open(fetch, &target_url)
                    .await
                    .map_err(|e| format!("fetch failed: {e}"))?;
                *current_url = page.url().clone();
                *current_actions = page.actions.clone();
                let sess = session.as_mut().expect("session ensured above");
                let script_outcome = sess
                    .navigate(&page.body_html, current_url.clone())
                    .map_err(|e| format!("js session navigate failed: {e}"))?;
                let mut obj = serde_json::json!({
                    "op": "click",
                    "kind": "navigation",
                    "ref": want,
                    "selector": selector,
                    "href": href,
                    "from": from,
                    "navigated_to": current_url.to_string(),
                    "matched": matched,
                    "defaultPrevented": false,
                    "console": click_outcome.console,
                    "scripts": script_outcome_json(&script_outcome),
                });
                if let Some(d) = drift {
                    obj.as_object_mut().unwrap().insert("ref_drift".into(), d);
                }
                return Ok(obj);
            }

            // Non-link click: dispatch a DOM event against the live session.
            let sess = session.as_ref().expect("session ensured above");
            let outcome = sess
                .click(&selector)
                .map_err(|e| format!("js click failed: {e}"))?;
            let matched = outcome
                .value
                .get("matched")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let default_prevented = outcome
                .value
                .get("defaultPrevented")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let drift = ref_drift_field(sess, &selector, &elem.tag);
            let mut obj = serde_json::json!({
                "op": "click",
                "kind": "dom-event",
                "ref": want,
                "selector": selector,
                "matched": matched,
                "defaultPrevented": default_prevented,
                "console": outcome.console,
            });
            if let Some(d) = drift {
                obj.as_object_mut().unwrap().insert("ref_drift".into(), d);
            }
            Ok(obj)
        }
        Action::Fill { target, value } => {
            ensure_session(fetch, session, current_actions, current_url, seed).await?;
            let want = normalize_replay_ref(target);
            let elem = resolve_action(current_actions, &want)
                .ok_or_else(|| format!("no element at ref `{want}`"))?
                .clone();
            let selector = selector_for_action(&elem)
                .ok_or_else(|| format!("no selector for ref `{want}`"))?;
            let sess = session.as_ref().expect("session ensured above");
            let outcome = sess
                .fill(&selector, value)
                .map_err(|e| format!("js fill failed: {e}"))?;
            let matched = outcome
                .value
                .get("matched")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let default_prevented = outcome
                .value
                .get("defaultPrevented")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let drift = ref_drift_field(sess, &selector, &elem.tag);
            let mut obj = serde_json::json!({
                "op": "fill",
                "ref": want,
                "selector": selector,
                "value": value,
                "matched": matched,
                "defaultPrevented": default_prevented,
                "console": outcome.console,
            });
            if let Some(d) = drift {
                obj.as_object_mut().unwrap().insert("ref_drift".into(), d);
            }
            Ok(obj)
        }
        Action::Submit { target } => {
            ensure_session(fetch, session, current_actions, current_url, seed).await?;
            let want = normalize_replay_ref(target);
            let elem = resolve_action(current_actions, &want)
                .ok_or_else(|| format!("no element at ref `{want}`"))?
                .clone();
            let selector = selector_for_action(&elem)
                .ok_or_else(|| format!("no selector for ref `{want}`"))?;
            // `submit` is `&mut self` since PR-1 — the real-HTTP path
            // can replace the session document on success. Take a
            // `&mut` borrow up front; `ref_drift_field` below only
            // needs `&` and runs after the submit returns.
            let sess = session.as_mut().expect("session ensured above");
            let outcome = sess
                .submit(&selector)
                .map_err(|e| format!("js submit failed: {e}"))?;
            let matched = outcome
                .value
                .get("matched")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let default_prevented = outcome
                .value
                .get("defaultPrevented")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let drift = ref_drift_field(sess, &selector, &elem.tag);
            let mut obj = serde_json::json!({
                "op": "submit",
                "ref": want,
                "selector": selector,
                "matched": matched,
                "defaultPrevented": default_prevented,
                "console": outcome.console,
            });
            if let Some(d) = drift {
                obj.as_object_mut().unwrap().insert("ref_drift".into(), d);
            }
            Ok(obj)
        }
    }
}

/// Project a [`heso_engine_js::ScriptOutcome`] to the JSON shape the
/// replay step embeds.
fn script_outcome_json(o: &heso_engine_js::ScriptOutcome) -> serde_json::Value {
    serde_json::json!({
        "executed": o.executed,
        "executed_with_error": o.executed_with_error,
        "external_handled": o.external_handled,
        "skipped_non_script_type": o.skipped_non_script_type,
    })
}

/// Best-effort soft-signal check: after the action graph said the
/// element at `selector` was a `<snapshot_tag>`, ask the live DOM
/// what tag actually sits at that selector now. If they disagree,
/// return a `ref_drift` JSON object; if they agree or the eval
/// fails, return None (this is non-load-bearing diagnostic data).
fn ref_drift_field(
    sess: &heso_engine_js::JsSession,
    selector: &str,
    snapshot_tag: &str,
) -> Option<serde_json::Value> {
    let selector_lit = serde_json::to_string(selector).ok()?;
    let script = format!(
        "(() => {{ const el = document.querySelector({selector_lit}); \
         return el ? el.tagName.toLowerCase() : null; }})()"
    );
    let outcome = sess.eval(&script).ok()?;
    let live_tag = outcome.value.as_str()?;
    if live_tag.eq_ignore_ascii_case(snapshot_tag) {
        None
    } else {
        Some(serde_json::json!({
            "snapshot_tag": snapshot_tag,
            "live_tag": live_tag,
        }))
    }
}

/// Accept both `@e7` and `e7` for the ref argument — matches the
/// ergonomics of `heso click` / `heso fill` / `heso submit`.
fn normalize_replay_ref(s: &str) -> String {
    if let Some(stripped) = s.strip_prefix('@') {
        format!("@{stripped}")
    } else {
        format!("@{s}")
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
        Some("read") => cmd_read(&args[1..]).await,
        Some("batch") => batch::cmd_batch(&args[1..]).await,
        Some("wait") => cmd_wait(&args[1..]).await,
        Some("plat-hash") => cmd_plat_hash(&args[1..]).await,
        Some("plat-verify") => cmd_plat_verify(&args[1..]).await,
        Some("eval-js") => cmd_eval_js(&args[1..]).await,
        Some("eval-dom") => cmd_eval_dom(&args[1..]).await,
        Some("click") => cmd_click(&args[1..]).await,
        Some("fill") => cmd_fill(&args[1..]).await,
        Some("submit") => cmd_submit(&args[1..]).await,
        Some("search") => search::cmd_search(&args[1..]).await,
        Some("serve") => serve::run().await,
        Some("action-hash") => cmd_action_hash(&args[1..]).await,
        Some("action-hash-verify") => cmd_action_hash_verify(&args[1..]).await,
        Some("replay") => cmd_replay(&args[1..]).await,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(n: &str, v: &str) -> (String, String) {
        (n.to_owned(), v.to_owned())
    }

    #[test]
    fn merge_submit_fields_data_only_keeps_order() {
        let data = vec![pair("a", "1"), pair("b", "2")];
        let merged = merge_submit_fields(&data, &[]);
        assert_eq!(merged, vec![pair("a", "1"), pair("b", "2")]);
    }

    #[test]
    fn merge_submit_fields_field_only_keeps_order() {
        let cli = vec![pair("x", "10"), pair("y", "20")];
        let merged = merge_submit_fields(&[], &cli);
        assert_eq!(merged, vec![pair("x", "10"), pair("y", "20")]);
    }

    #[test]
    fn merge_submit_fields_field_wins_over_data_on_same_name() {
        // Both supply `custname`; --field must win, and the leftover
        // --data entry should NOT appear in the merged output.
        let data = vec![pair("custname", "FROM-DATA"), pair("email", "from-data@x")];
        let cli = vec![pair("custname", "FROM-FIELD")];
        let merged = merge_submit_fields(&data, &cli);
        // `email` from data stays (no override); `custname` from data
        // is dropped; `custname` from CLI appears at the end.
        assert_eq!(
            merged,
            vec![
                pair("email", "from-data@x"),
                pair("custname", "FROM-FIELD"),
            ]
        );
    }

    #[test]
    fn merge_submit_fields_data_keys_unique_to_data_survive() {
        let data = vec![pair("a", "1"), pair("b", "2"), pair("c", "3")];
        let cli = vec![pair("b", "TWO")];
        let merged = merge_submit_fields(&data, &cli);
        // `a` and `c` survive in their original order; `b` from data
        // is dropped; `b=TWO` from CLI lands at the end.
        assert_eq!(
            merged,
            vec![
                pair("a", "1"),
                pair("c", "3"),
                pair("b", "TWO"),
            ]
        );
    }
}
