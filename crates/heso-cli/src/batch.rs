//! `heso batch <subverb> <urls...>` — parallel multi-URL scraping in
//! **one** process. The single biggest lever for "an agent scrapes
//! ten pages in a row" workflows: instead of N subprocess spawns each
//! paying TLS handshake + cookie-jar init + reqwest connection-pool
//! warmup, you pay all of that once and run the actual fetches
//! concurrently.
//!
//! ## Surface
//!
//! ```text
//! heso batch open url1 url2 url3
//! heso batch read url1 url2 url3
//! heso batch open url1 url2 url3 --parallel 4
//! heso batch open url1 url2 url3 --timeout-per-url 5s
//! heso batch open url1 url2 url3 --fail-fast
//! echo -e "https://a.com\nhttps://b.com" | heso batch open
//! cat urls.txt | heso batch read
//! heso batch url1 url2          # default subverb is `open`
//! ```
//!
//! ## Output
//!
//! **JSON-Lines on stdout** — one object per URL, completion-ordered:
//!
//! ```jsonl
//! {"url":"https://a.com","ok":true,"title":"...","actions":[...], ...}
//! {"url":"https://b.com","ok":false,"error":"timeout after 30s"}
//! ```
//!
//! Each object echoes the input URL as the first field so the agent
//! can correlate. Completion order is intentional — fast pages don't
//! wait on slow ones. Agents that need input order can re-sort by
//! `url` client-side.
//!
//! ## Exit codes
//!
//! - **0** — at least one URL succeeded
//! - **1** — all URLs failed (or `--fail-fast` and the first error stopped the batch)
//! - **2** — flag-parse / usage error
//!
//! ## Concurrency model
//!
//! - One shared [`FetchEngine`] per batch — single cookie jar, single
//!   reqwest connection pool. Logged-in batch scrapes work because the
//!   `Set-Cookie` from URL A is sent on the request to URL B (when run
//!   sequentially under `--parallel 1`, or whichever finishes first
//!   under higher concurrency).
//! - [`tokio::sync::Semaphore`] with `--parallel` permits caps in-flight
//!   tasks.
//! - [`tokio::sync::mpsc`] channel streams completed results back to a
//!   stdout-writing consumer task — outputs land **as they complete**,
//!   not after the whole batch finishes.
//! - For `batch read`, each task builds its own [`heso_engine_js::JsEngine`]
//!   (the engine is not cheap to clone-share across tasks; see ADR 0014).
//!   The cookie jar IS still shared — every per-task JsEngine threads
//!   the FetchEngine's `Arc<CookieStoreMutex>`. Default `--parallel 2`
//!   for `read` matches the memory budget (each engine carries a QuickJS
//!   context).
//!
//! References:
//! - Scrapy's `CONCURRENT_REQUESTS` defaults to 16 ([Scrapy settings docs](https://docs.scrapy.org/en/latest/topics/settings.html)).
//!   We default to 8 for `batch open` (conservative; users can bump),
//!   2 for `batch read` (memory-bound).
//! - `reqwest::Client` is `Arc` internally — clone-cheap, shares the
//!   connection pool by default ([reqwest::Client docs](https://docs.rs/reqwest/latest/reqwest/struct.Client.html)).
//! - `CookieStoreMutex` wrapped in `Arc` is the canonical shared-cookie
//!   pattern ([reqwest_cookie_store docs](https://docs.rs/reqwest_cookie_store/latest/reqwest_cookie_store/)).
//! - Semaphore-permits-per-task is the documented tokio idiom for
//!   bounded concurrency ([tokio::sync::Semaphore docs](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html)).

use std::io::{self, BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use heso_core::Url;
use heso_engine_api::{EngineApi, Page};
use heso_engine_fetch::FetchEngine;
use tokio::sync::Semaphore;

use crate::{collect_cookies, detect_framework, group_forms, parse_include_filter, IncludeFilter};

/// Hard cap on `--parallel`. Higher = more file descriptors + more
/// in-flight TLS handshakes; 32 is generous for any realistic agent
/// workload and keeps us well clear of OS fd limits on Windows
/// (default 512 / process).
const HARD_MAX_PARALLEL: usize = 32;

/// Default `--parallel` for `batch open` (static path, cheap per
/// task — just a fetch + parse). Matches `CONCURRENT_REQUESTS_PER_DOMAIN
/// = 8` in Scrapy.
const DEFAULT_PARALLEL_OPEN: usize = 8;

/// Default `--parallel` for `batch read` (JS-hydration path, each task
/// carries a QuickJS context). 2 keeps the worst-case RSS bounded.
const DEFAULT_PARALLEL_READ: usize = 2;

/// Default per-URL timeout — matches Playwright's `actionTimeout`
/// default of 30s, which is what agent harnesses expect.
const DEFAULT_TIMEOUT_PER_URL: Duration = Duration::from_secs(30);

/// Which subverb the batch is running. Drives both the per-task body
/// builder and the default `--parallel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subverb {
    Open,
    Read,
}

impl Subverb {
    fn default_parallel(self) -> usize {
        match self {
            Self::Open => DEFAULT_PARALLEL_OPEN,
            Self::Read => DEFAULT_PARALLEL_READ,
        }
    }
}

/// Parsed CLI shape. Built by [`parse_args`], consumed by [`run_batch`].
struct BatchArgs {
    sub: Subverb,
    urls: Vec<String>,
    parallel: usize,
    timeout_per_url: Duration,
    fail_fast: bool,
    /// `--include CSV` — only meaningful for `read`. None = "all"
    /// (the same default the single-URL `heso read` uses).
    include_csv: Option<String>,
    /// `--js-fetch` — pass through to `read` mode so external
    /// `<script src=...>` runs through the same shared client.
    js_fetch: bool,
}

/// One row of output. Serialised one-per-line on stdout. `ok=false`
/// rows carry an `error` string in a tag-classified shape
/// (`timeout`, `dns`, `connection_refused`, `http_5xx`, …) so the
/// agent can branch on retryability without parsing English.
#[derive(Debug, serde::Serialize)]
struct BatchRow {
    url: String,
    ok: bool,
    /// Present when `ok = true` — the same JSON shape `heso open` /
    /// `heso read` would emit for this URL, minus the `url` field
    /// (which already lives at the top level of the row).
    #[serde(skip_serializing_if = "Option::is_none", flatten)]
    payload: Option<serde_json::Value>,
    /// Present when `ok = false` — a short classified error string.
    /// Format: `"<kind>: <detail>"` where `<kind>` is one of
    /// `timeout`, `invalid_url`, `dns`, `connection_refused`,
    /// `tls`, `http_<status>`, `engine`, `js_hydrate`, `fetch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Top-level entry. Parses args, validates, dispatches to [`run_batch`].
/// Returns:
/// - `ExitCode::from(2)` on flag-parse error
/// - `ExitCode::SUCCESS` if at least one URL succeeded
/// - `ExitCode::FAILURE` if all URLs failed (or `--fail-fast` aborted
///   before any success)
pub(crate) async fn cmd_batch(args: &[String]) -> ExitCode {
    let parsed = match parse_args(args).await {
        Ok(p) => p,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };
    run_batch(parsed).await
}

/// Walk the argv. Subverb is optional and defaults to `open`. Flags
/// can appear before/after positionals, same as the rest of the heso
/// CLI. URLs come from positionals; if none AND stdin is not a TTY,
/// read one URL per line from stdin.
async fn parse_args(args: &[String]) -> Result<BatchArgs, String> {
    let usage = "usage: heso batch [open|read] <urls...> [--parallel N] [--timeout DUR | --timeout-per-url DUR] \
                 [--fail-fast] [--include CSV] [--js-fetch]\n\
                 stdin mode: cat urls.txt | heso batch [open|read]";

    let mut sub: Option<Subverb> = None;
    let mut urls: Vec<String> = Vec::new();
    let mut parallel: Option<usize> = None;
    let mut timeout_per_url = DEFAULT_TIMEOUT_PER_URL;
    let mut fail_fast = false;
    let mut include_csv: Option<String> = None;
    let mut js_fetch = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--parallel" => {
                let Some(v) = args.get(i + 1) else {
                    return Err(format!("--parallel needs a value\n{usage}"));
                };
                let n: usize = v
                    .parse()
                    .map_err(|e| format!("--parallel: invalid usize `{v}`: {e}\n{usage}"))?;
                if n == 0 {
                    return Err(format!("--parallel must be >= 1\n{usage}"));
                }
                parallel = Some(n.min(HARD_MAX_PARALLEL));
                if n > HARD_MAX_PARALLEL {
                    eprintln!("--parallel clamped from {n} to hard max {HARD_MAX_PARALLEL}");
                }
                i += 2;
            }
            "--timeout-per-url" | "--timeout" => {
                // `--timeout-per-url` is the original, explicit name. `--timeout`
                // is the global flag the other network verbs accept; in batch
                // the only timeout dimension that makes sense is per-URL
                // (a global batch budget would silently kill mid-stream work
                // when one slow URL ate all the time), so the two names are
                // aliases. When both appear, the latter wins — the user's
                // most recent intent.
                let flag = args[i].clone();
                let Some(v) = args.get(i + 1) else {
                    return Err(format!("{flag} needs a value\n{usage}"));
                };
                timeout_per_url =
                    parse_duration(v).map_err(|e| format!("{flag}: {e}\n{usage}"))?;
                i += 2;
            }
            "--fail-fast" => {
                fail_fast = true;
                i += 1;
            }
            "--continue-on-error" => {
                // Already the default — explicit opt-in is a no-op.
                fail_fast = false;
                i += 1;
            }
            "--include" => {
                let Some(v) = args.get(i + 1) else {
                    return Err(format!("--include needs a value\n{usage}"));
                };
                include_csv = Some(v.clone());
                i += 2;
            }
            "--js-fetch" => {
                js_fetch = true;
                i += 1;
            }
            other if other.starts_with("--") => {
                return Err(format!("unknown flag `{other}`\n{usage}"));
            }
            // Positional. The FIRST positional may be a subverb keyword;
            // otherwise it's a URL and the subverb defaults to `open`.
            _ => {
                let token = &args[i];
                if sub.is_none() && urls.is_empty() {
                    match token.as_str() {
                        "open" => {
                            sub = Some(Subverb::Open);
                            i += 1;
                            continue;
                        }
                        "read" => {
                            sub = Some(Subverb::Read);
                            i += 1;
                            continue;
                        }
                        _ => {}
                    }
                }
                urls.push(token.clone());
                i += 1;
            }
        }
    }

    let sub = sub.unwrap_or(Subverb::Open);

    // Stdin-mode: no positional URLs AND stdin isn't a TTY → read
    // newline-delimited URLs. Skip blank lines and `#`-comments.
    if urls.is_empty() && !io::stdin().is_terminal() {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => return Err(format!("stdin read failed: {e}\n{usage}")),
            };
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            urls.push(trimmed.to_owned());
        }
    }

    if urls.is_empty() {
        return Err(format!("no URLs given\n{usage}"));
    }

    let parallel = parallel.unwrap_or_else(|| sub.default_parallel());

    Ok(BatchArgs {
        sub,
        urls,
        parallel,
        timeout_per_url,
        fail_fast,
        include_csv,
        js_fetch,
    })
}

/// Parse `5s`, `200ms`, `1m`, `750us` into a [`Duration`]. The same
/// shape `heso wait --timeout` accepts. Unitless input is rejected —
/// the agent should always be explicit about time.
fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num_part, unit) = if let Some(rest) = s.strip_suffix("ms") {
        (rest, "ms")
    } else if let Some(rest) = s.strip_suffix("us") {
        (rest, "us")
    } else if let Some(rest) = s.strip_suffix('s') {
        (rest, "s")
    } else if let Some(rest) = s.strip_suffix('m') {
        (rest, "m")
    } else {
        return Err(format!(
            "invalid duration `{s}`: need a unit (ms, us, s, m), e.g. `5s`"
        ));
    };
    let n: u64 = num_part
        .parse()
        .map_err(|e| format!("invalid duration number `{num_part}` in `{s}`: {e}"))?;
    let d = match unit {
        "ms" => Duration::from_millis(n),
        "us" => Duration::from_micros(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n * 60),
        _ => unreachable!(),
    };
    Ok(d)
}

/// Drive the batch: build one shared [`FetchEngine`], spawn N tasks
/// (semaphore-bounded), stream each task's row to stdout as it
/// completes. Returns the appropriate `ExitCode`.
async fn run_batch(args: BatchArgs) -> ExitCode {
    let engine = match FetchEngine::new() {
        Ok(e) => Arc::new(e),
        Err(e) => {
            eprintln!("failed to build fetch engine: {e}");
            return ExitCode::FAILURE;
        }
    };
    let semaphore = Arc::new(Semaphore::new(args.parallel));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BatchRow>();
    let include = parse_include_filter(args.include_csv.as_deref());

    // Two concurrent halves:
    //
    // (1) Dispatcher: spawn one task per URL. Crucially, the loop awaits
    //     `acquire_owned()` BEFORE spawning the next task, so spawn(N)
    //     cannot start before spawn(N-1) has grabbed its permit. That
    //     gives us input-order dispatch even under `--parallel 1`
    //     (each URL fully completes before the next is even spawned),
    //     which is what makes cookie chains work — a `Set-Cookie` from
    //     URL A reaches the shared jar before URL B's request is built.
    // (2) Consumer: drain `rx` as rows complete, write each one to
    //     stdout immediately (streaming output, NOT batched at end).
    //
    // We run both halves via `tokio::join!` so they overlap: tasks
    // finishing during dispatch can already be written to stdout while
    // later URLs are still in flight.
    let urls = args.urls.clone();
    let sub = args.sub;
    let timeout_per_url = args.timeout_per_url;
    let js_fetch = args.js_fetch;
    let fail_fast = args.fail_fast;

    // The dispatcher owns its own `tx` clone, drops it when done; the
    // separate handle Vec is shared via `Arc<Mutex<...>>` so the
    // consumer can `.abort()` outstanding tasks on fail-fast / broken
    // pipe.
    let handles: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> =
        Arc::new(std::sync::Mutex::new(Vec::with_capacity(urls.len())));
    let handles_for_dispatch = handles.clone();
    let semaphore_for_dispatch = semaphore.clone();
    let engine_for_dispatch = engine.clone();
    let dispatch_tx = tx.clone();
    let dispatcher = tokio::spawn(async move {
        for url_str in urls {
            let permit = semaphore_for_dispatch
                .clone()
                .acquire_owned()
                .await
                .expect("semaphore closed");
            // Permit-acquisition happened; spawn the worker.
            let engine = engine_for_dispatch.clone();
            let tx = dispatch_tx.clone();
            let handle = tokio::spawn(async move {
                let row = run_one(sub, &engine, url_str, timeout_per_url, include, js_fetch).await;
                drop(permit);
                let _ = tx.send(row);
            });
            handles_for_dispatch
                .lock()
                .expect("handles mutex poisoned")
                .push(handle);
        }
        // Drop the dispatcher's tx clone — once every worker also
        // drops, the consumer's `rx.recv()` returns None.
    });
    drop(tx);

    // Stdout consumer half. Tracks success count + fail-fast trigger.
    let mut any_ok = false;
    let mut any_err = false;
    let mut stop = false;
    {
        let mut stdout = io::stdout().lock();
        while let Some(row) = rx.recv().await {
            if row.ok {
                any_ok = true;
            } else {
                any_err = true;
            }
            // Each row is a SINGLE LINE — `serde_json::to_string`
            // (NOT pretty) produces no embedded newlines.
            match serde_json::to_string(&row) {
                Ok(s) => {
                    if writeln!(stdout, "{s}").is_err() {
                        // Broken pipe — downstream consumer is gone.
                        stop = true;
                        break;
                    }
                }
                Err(e) => {
                    eprintln!("failed to serialize row for {}: {e}", row.url);
                }
            }
            let _ = stdout.flush();
            if fail_fast && !row.ok {
                stop = true;
                break;
            }
        }
    }

    if stop {
        // Abort the dispatcher AND any outstanding worker tasks.
        dispatcher.abort();
        let inner = handles.lock().expect("handles mutex poisoned");
        for h in inner.iter() {
            h.abort();
        }
        // Don't bother awaiting — the process is about to exit anyway.
    } else {
        // Let the dispatcher finish spawning, then drain any in-flight
        // workers so the runtime exits cleanly. `rx.recv()` returning
        // None already implies all senders dropped, so all worker
        // tasks have finished sending — but they may not yet have
        // RETURNED. Join them.
        let _ = dispatcher.await;
        let inner_handles = std::mem::take(&mut *handles.lock().expect("handles mutex poisoned"));
        for h in inner_handles {
            let _ = h.await;
        }
    }

    if !any_ok && any_err {
        ExitCode::FAILURE
    } else if !any_ok && !any_err {
        // No URLs processed at all — shouldn't happen (we validated
        // urls.len() > 0 in parse_args) but be defensive.
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// Run one URL. Always returns a [`BatchRow`] — no error escapes back
/// to the caller. The timeout wraps the whole per-task body so a slow
/// or hung URL can't block the slot forever.
async fn run_one(
    sub: Subverb,
    engine: &Arc<FetchEngine>,
    url_str: String,
    timeout_per_url: Duration,
    include: IncludeFilter,
    js_fetch: bool,
) -> BatchRow {
    let url = match Url::parse(&url_str) {
        Ok(u) => u,
        Err(e) => {
            return BatchRow {
                url: url_str,
                ok: false,
                payload: None,
                error: Some(format!("invalid_url: {e}")),
            };
        }
    };

    let task = async {
        match sub {
            Subverb::Open => run_open_for_url(engine, &url).await,
            Subverb::Read => run_read_for_url(engine, &url, include, js_fetch).await,
        }
    };

    match tokio::time::timeout(timeout_per_url, task).await {
        Ok(Ok(payload)) => {
            // `ok` reflects whether the verb's primary objective
            // succeeded — i.e. a usable page came back. A 5xx or
            // `partial: true` shape is not "ok" even though the fetch
            // wire didn't panic. Mirrors single-URL `heso open` semantics.
            let payload_ok = payload
                .get("partial")
                .and_then(|v| v.as_bool())
                .map(|p| !p)
                .unwrap_or(true);
            BatchRow {
                url: url.to_string(),
                ok: payload_ok,
                payload: Some(payload),
                error: None,
            }
        }
        Ok(Err(err)) => BatchRow {
            url: url.to_string(),
            ok: false,
            payload: None,
            error: Some(err),
        },
        Err(_) => BatchRow {
            url: url.to_string(),
            ok: false,
            payload: None,
            error: Some(format!(
                "timeout: after {}",
                format_duration(timeout_per_url)
            )),
        },
    }
}

/// `heso open` for one URL, returning the JSON payload (no `url`
/// field — that's added by [`run_one`] at the row level). Returns
/// `Err(String)` with a classified error tag on failure.
async fn run_open_for_url(
    engine: &Arc<FetchEngine>,
    url: &Url,
) -> Result<serde_json::Value, String> {
    let page = engine
        .open(url)
        .await
        .map_err(|e| classify_fetch_error(&e.to_string()))?;
    Ok(build_open_payload_with_envelope(&page))
}

/// `heso read` for one URL — fetch + JS hydration. One [`heso_engine_js::JsEngine`]
/// per task; sharing engines across tasks is not safe (each context
/// is single-threaded). The COOKIE JAR is still shared via the same
/// `Arc<CookieStoreMutex>` reqwest writes into, so `document.cookie`
/// reads observe `Set-Cookie` responses from other batch URLs.
async fn run_read_for_url(
    engine: &Arc<FetchEngine>,
    url: &Url,
    include: IncludeFilter,
    js_fetch: bool,
) -> Result<serde_json::Value, String> {
    let page = engine
        .open(url)
        .await
        .map_err(|e| classify_fetch_error(&e.to_string()))?;

    let client = engine.client();
    let cookie_jar = engine.cookie_jar();
    let rt_handle = tokio::runtime::Handle::current();
    let js_engine = if js_fetch {
        heso_engine_js::JsEngine::new_with_fetch_and_cookies(client, rt_handle, cookie_jar)
            .map_err(|e| format!("engine: {e}"))?
    } else {
        heso_engine_js::JsEngine::new().map_err(|e| format!("engine: {e}"))?
    };
    let script_policy = if js_fetch {
        heso_engine_js::ScriptFetchPolicy::Fetch
    } else {
        heso_engine_js::ScriptFetchPolicy::Skip
    };
    let (session, script_outcome) = heso_engine_js::JsSession::open_on_engine(
        js_engine,
        &page.body_html,
        page.url().clone(),
        script_policy,
    )
    .map_err(|e| format!("js_hydrate: {e}"))?;
    let console = session.engine().drain_console();
    let failed_scripts = session.engine().drain_script_failures();
    let console_errors_count = console
        .iter()
        .filter(|e| matches!(e.level, heso_engine_js::ConsoleLevel::Error))
        .count();
    let post_html = session.document_html();

    let mut body = build_open_payload_without_hash(&page);
    if include.text {
        let text = heso_engine_fetch::extract_visible_text(&post_html);
        body["text"] = serde_json::Value::String(text);
    }
    if include.forms {
        body["forms"] = group_forms(&page.actions);
    }
    if include.cookies {
        body["cookies"] = collect_cookies(&page);
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
    let (js_partial, js_reason) = crate::classify_failure_envelope(&failed_scripts);
    let (partial, partial_reason) = crate::apply_http_truthfulness(
        js_partial,
        js_reason,
        page.http_status,
        &page.body_html,
    );
    let (partial, partial_reason) = crate::apply_extraction_truthfulness(
        partial,
        partial_reason,
        &page.tree.title,
        page.actions.len(),
        page.tree.root.children.len(),
        &failed_scripts,
    );
    crate::attach_failure_envelope(
        &mut body,
        partial,
        &partial_reason,
        &failed_scripts,
        console_errors_count,
    );
    // plat_hash last — over the same canonical form `heso read` uses.
    let hash = heso_engine_fetch::plat_hash(&body);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("plat_hash".to_owned(), serde_json::Value::String(hash));
    }
    Ok(body)
}

/// Build the `heso open` JSON payload (with `plat_hash` AND the
/// structured-failure envelope) for a fetched page. Mirrors the
/// single-URL `cmd_open` output minus the top-level `url` field
/// (which the batch row carries). HTTP-side classification only —
/// `batch open` skips the JS hydration pass that single `open` does
/// for `failed_scripts`/`console_errors_count` because that would
/// rebuild a QuickJS context per URL.
fn build_open_payload_with_envelope(page: &heso_engine_fetch::FetchPage) -> serde_json::Value {
    let mut body = page.plat_body_base();
    let partial_reason = heso_engine_fetch::partial_reason_for_status(
        page.http_status,
        &page.body_html,
    );
    let (partial, reason): (bool, String) = match partial_reason {
        Some(r) => (true, r),
        None => (false, "ok".to_owned()),
    };
    crate::attach_failure_envelope(&mut body, partial, &reason, &[], 0);
    let hash = heso_engine_fetch::plat_hash(&body);
    if let Some(obj) = body.as_object_mut() {
        obj.insert("plat_hash".to_owned(), serde_json::Value::String(hash));
    }
    body
}

/// Skips the final `plat_hash` stamp — the `read` path needs to layer
/// extra fields on first, then compute the hash over the full result.
fn build_open_payload_without_hash(page: &heso_engine_fetch::FetchPage) -> serde_json::Value {
    page.plat_body_base()
}

/// Map a fetch-engine error string onto one of the classified tags
/// the batch row exposes. Best-effort — the goal is for an agent to
/// branch on retryability (`timeout` / `dns` / `tls` / `http_5xx`)
/// without parsing English. Unknown shapes fall through to `fetch:`.
fn classify_fetch_error(e: &str) -> String {
    let lower = e.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        format!("timeout: {e}")
    } else if lower.contains("dns") || lower.contains("resolve dns") {
        format!("dns: {e}")
    } else if lower.contains("connection refused") {
        format!("connection_refused: {e}")
    } else if lower.contains("tls") || lower.contains("certificate") {
        format!("tls: {e}")
    } else if let Some(status) = extract_status_code(&lower) {
        format!("http_{status}: {e}")
    } else {
        format!("fetch: {e}")
    }
}

/// Best-effort extraction of an HTTP status code from a reqwest error
/// string. Reqwest's `Display` for status errors looks like
/// `"HTTP status client error (404 Not Found) for url ..."` or
/// `"HTTP status server error (500 ...) for url ..."`.
fn extract_status_code(lower: &str) -> Option<u16> {
    // Cheap pattern: `(NNN ` where NNN is 3 ascii digits.
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i + 4 < bytes.len() {
        if bytes[i] == b'('
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
            && bytes[i + 4] == b' '
        {
            let s = std::str::from_utf8(&bytes[i + 1..i + 4]).ok()?;
            return s.parse().ok();
        }
        i += 1;
    }
    None
}

/// Render a [`Duration`] in the same shape [`parse_duration`] accepts.
/// `100ms`, `5s`, `1m`. Always uses the largest unit that gives a whole
/// number — matches the user's input flavor when round-tripping.
fn format_duration(d: Duration) -> String {
    let total_ms = d.as_millis();
    if total_ms >= 60_000 && total_ms.is_multiple_of(60_000) {
        format!("{}m", total_ms / 60_000)
    } else if total_ms >= 1_000 && total_ms.is_multiple_of(1_000) {
        format!("{}s", total_ms / 1_000)
    } else {
        format!("{total_ms}ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_units() {
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("200ms").unwrap(), Duration::from_millis(200));
        assert_eq!(parse_duration("1m").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_duration("750us").unwrap(), Duration::from_micros(750));
        assert!(parse_duration("100").is_err());
        assert!(parse_duration("five").is_err());
    }

    #[test]
    fn format_duration_round_trip() {
        assert_eq!(format_duration(Duration::from_secs(30)), "30s");
        assert_eq!(format_duration(Duration::from_millis(100)), "100ms");
        assert_eq!(format_duration(Duration::from_secs(120)), "2m");
    }

    #[test]
    fn classify_extracts_http_status() {
        let s = "HTTP status client error (404 Not Found) for url (http://x/)";
        let tag = classify_fetch_error(s);
        assert!(tag.starts_with("http_404:"), "got: {tag}");
    }

    #[test]
    fn classify_falls_through_to_fetch() {
        let tag = classify_fetch_error("something weird");
        assert!(tag.starts_with("fetch:"), "got: {tag}");
    }
}
