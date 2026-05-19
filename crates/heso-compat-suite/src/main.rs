//! # heso-compat-suite
//!
//! End-to-end compatibility + timing benchmark for heso.
//!
//! Runs a curated set of real-world site/framework targets through the
//! full engine path (fetch + html5ever parse + QuickJS execution +
//! optional `--js-fetch`), then emits a JSON report and an optional
//! markdown scorecard.
//!
//! Why a separate crate from `heso-compat-tests`:
//!
//! - `heso-compat-tests` is a CI-friendly **fetch-layer regression net**
//!   backed by recorded wiremock cassettes — it asserts that pages parse
//!   and basic invariants hold, with zero network I/O. Narrow scope.
//! - `heso-compat-suite` (this crate) is a **full-stack compatibility +
//!   timing benchmark** that hits live URLs, runs framework code, and
//!   measures total wall-clock per target. Broader scope, requires the
//!   network, not part of CI.
//!
//! ## Usage
//!
//! ```text
//! cargo run -p heso-compat-suite              # JSON to stdout
//! cargo run -p heso-compat-suite -- --markdown COMPATIBILITY.md
//! cargo run -p heso-compat-suite -- --filter wikipedia
//! ```
//!
//! Exit code: 0 on success regardless of per-target failures (failures
//! are part of the report, not a hard error). Pass `--strict` to exit
//! non-zero when any target fails.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

use heso_engine_fetch::FetchEngine;
use heso_engine_js::{JsEngine, ScriptFetchPolicy};
use serde::Serialize;
use sysinfo::{get_current_pid, Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use url::Url;

/// What a target's JS probe is asserting about the page.
///
/// Probes are intentionally narrow: each one pins ONE observable
/// behavior. We're not trying to assert deep semantic equivalence
/// with Chrome — we're trying to answer "does heso get a useful
/// value out of this site/framework?" Each probe is a one-liner.
#[derive(Clone, Copy, Debug)]
enum Probe {
    /// Evaluate `js`; the stringified result must contain `needle`.
    /// Used for "extract a known title / link / heading" tests where
    /// we know exactly what should be in the page.
    Contains {
        /// JS expression to evaluate against the loaded DOM.
        js: &'static str,
        /// Substring the stringified result must contain.
        needle: &'static str,
    },
    /// Evaluate `js`; the result must be a non-empty string. Used for
    /// dynamic content where exact text changes (e.g. HN top story).
    NonEmptyString {
        /// JS expression to evaluate against the loaded DOM.
        js: &'static str,
    },
    /// Evaluate `js`; the result must be a number greater than `min`.
    /// Used for "page has at least N of X" tests (e.g. HN has at
    /// least 10 story links).
    NumberAtLeast {
        /// JS expression to evaluate; expected to return a number.
        js: &'static str,
        /// Minimum value (inclusive).
        min: i64,
    },
}

/// One row in the compatibility scorecard.
struct Target {
    /// Short human-readable name (used in the markdown table).
    name: &'static str,
    /// Bucket — "smoke", "server-rendered", "spa", "form", etc.
    category: &'static str,
    /// URL to fetch.
    url: &'static str,
    /// Whether to install the in-JS `fetch()` global. Off by default
    /// since most extraction probes work on the static HTML.
    js_fetch: bool,
    /// What the probe asserts.
    probe: Probe,
}

/// Result for one target.
#[derive(Serialize)]
struct TargetResult {
    name: String,
    category: String,
    url: String,
    /// One of: `ok`, `assertion_failed`, `fetch_error`, `js_error`.
    status: String,
    /// Total wall-clock for this target (fetch + parse + eval).
    ms_total: u128,
    /// Just the fetch leg.
    ms_fetch: u128,
    /// Just the JS eval (post-fetch).
    ms_eval: u128,
    /// This process's resident-set-size (in kilobytes) sampled
    /// **after** this target finished. Because heso does not release
    /// memory between targets, this value is monotonically
    /// non-decreasing across the run, so it answers the question
    /// "after running through these N pages, how much RAM does heso
    /// hold?" — which is exactly the number we want to back the
    /// README's "tiny idle RAM" claim. `0` means we couldn't sample
    /// (e.g. unsupported platform).
    peak_rss_kb: u64,
    /// The probe's returned value (truncated if huge).
    value: Option<serde_json::Value>,
    /// Failure message, if any.
    error: Option<String>,
}

/// Aggregate report — what gets written to JSON.
#[derive(Serialize)]
struct Report {
    results: Vec<TargetResult>,
    summary: Summary,
}

/// Pass/fail counts.
#[derive(Serialize)]
struct Summary {
    total: usize,
    passed: usize,
    failed: usize,
}

// ============================================================================
// Curated targets
// ============================================================================
//
// Selection criteria:
// - Cooperative (robots.txt-friendly, no auth, low-traffic).
// - Stable enough that the probe needle doesn't rot.
// - Each one exercises a DIFFERENT load-bearing engine path:
//   smoke, server-rendered text extraction, JS-heavy SPA, form,
//   static docs.
//
// When adding a target:
// 1. Pick a narrow probe — one expression, one assertion.
// 2. Use a needle that's part of the page's permanent identity
//    (a page title, an `id`, a brand name), not something that
//    might change daily (a headline, a price).
// 3. Add it to TARGETS below; the runner picks it up automatically.

const TARGETS: &[Target] = &[
    Target {
        name: "example.com",
        category: "smoke",
        url: "https://example.com",
        js_fetch: false,
        probe: Probe::Contains {
            js: "document.title",
            needle: "Example Domain",
        },
    },
    Target {
        name: "news.ycombinator.com",
        category: "server-rendered",
        url: "https://news.ycombinator.com",
        js_fetch: false,
        // HN's top story is always under .titleline > a; we just
        // assert the result is a non-empty string. Headline rotates
        // every few minutes so we can't pin a needle.
        probe: Probe::NonEmptyString {
            js: "document.querySelectorAll('.titleline > a')[0]?.textContent ?? ''",
        },
    },
    Target {
        name: "news.ycombinator.com (count)",
        category: "server-rendered",
        url: "https://news.ycombinator.com",
        js_fetch: false,
        probe: Probe::NumberAtLeast {
            js: "document.querySelectorAll('.titleline > a').length",
            min: 20,
        },
    },
    Target {
        name: "wikipedia.org",
        category: "server-rendered",
        url: "https://www.wikipedia.org/",
        js_fetch: false,
        probe: Probe::Contains {
            js: "document.title",
            needle: "Wikipedia",
        },
    },
    Target {
        name: "httpbin.org/html",
        category: "static",
        url: "https://httpbin.org/html",
        js_fetch: false,
        probe: Probe::Contains {
            js: "document.querySelector('h1')?.textContent ?? ''",
            needle: "Herman Melville",
        },
    },
    Target {
        name: "developer.mozilla.org div",
        category: "docs",
        url: "https://developer.mozilla.org/en-US/docs/Web/HTML/Element/div",
        js_fetch: false,
        probe: Probe::Contains {
            js: "document.title",
            needle: "<div>",
        },
    },
    Target {
        name: "rust-lang.org",
        category: "marketing",
        url: "https://www.rust-lang.org/",
        js_fetch: false,
        probe: Probe::Contains {
            js: "document.title",
            needle: "Rust",
        },
    },
    Target {
        name: "docs.rs",
        category: "docs",
        url: "https://docs.rs/serde/latest/serde/",
        js_fetch: false,
        probe: Probe::Contains {
            js: "document.title",
            needle: "serde",
        },
    },
    // TodoMVC framework targets — JS-rendered SPAs that ship a static
    // <title>TodoMVC: <Framework></title> in the HTML, so the probe is
    // robust whether or not JS hydration completes. `js_fetch: true` lets
    // the in-JS `fetch()` global resolve external <script> tags so the
    // framework code actually executes.
    //
    // TODO: follow-up — once we trust JS hydration end-to-end, add a
    // second probe per framework that asserts on the hydrated `.new-todo`
    // input or the framework's mounted DOM nodes.
    Target {
        name: "TodoMVC Preact",
        category: "spa",
        url: "https://todomvc.com/examples/preact/dist/",
        js_fetch: true,
        probe: Probe::Contains {
            js: "document.title",
            needle: "TodoMVC",
        },
    },
    Target {
        name: "TodoMVC React",
        category: "spa",
        url: "https://todomvc.com/examples/react/dist/",
        js_fetch: true,
        probe: Probe::Contains {
            js: "document.title",
            needle: "TodoMVC",
        },
    },
    Target {
        name: "TodoMVC Vue",
        category: "spa",
        url: "https://todomvc.com/examples/vue/dist/",
        js_fetch: true,
        probe: Probe::Contains {
            js: "document.title",
            needle: "TodoMVC",
        },
    },
    // ---- Heavier SPA / marketing targets ----
    //
    // These three sites ship a lot of client-side JS but each also
    // server-renders a useful `<title>`. We probe the title because it
    // is the cheapest stable signal: no hydration required, no JS
    // execution needed against the SPA bundle itself. Once we wire
    // `js_fetch: true` and a proper script pump for these, we can add
    // post-hydration probes (e.g. a known link or heading rendered by
    // React/Next).
    Target {
        name: "github.com (microsoft/playwright)",
        category: "spa",
        url: "https://github.com/microsoft/playwright",
        js_fetch: false,
        // Public repo page; title is a stable
        // `GitHub - microsoft/playwright: ...`. The slug is a
        // tighter needle than the brand alone — guards against the
        // page accidentally redirecting to a generic login wall.
        probe: Probe::Contains {
            js: "document.title",
            needle: "microsoft/playwright",
        },
    },
    Target {
        name: "stripe.com/pricing",
        category: "spa",
        url: "https://stripe.com/pricing",
        js_fetch: false,
        // Stripe's pricing page title is literally `Pricing & Fees`
        // (the brand is *not* in the `<title>`). Needle has to be
        // `Pricing` — confirmed by curl against the live page with
        // our default `heso/<version>` UA.
        probe: Probe::Contains {
            js: "document.title",
            needle: "Pricing",
        },
    },
    Target {
        name: "vercel.com",
        category: "spa",
        url: "https://vercel.com",
        js_fetch: false,
        // Next.js marketing site; title contains the brand directly.
        probe: Probe::Contains {
            js: "document.title",
            needle: "Vercel",
        },
    },
    // ---- Framework docs / SPA-router sites ----
    //
    // These exercise the same code paths but ship MORE client-side JS
    // (Next.js / VitePress / SvelteKit) and use `history.pushState` for
    // routing. The probe is still `document.title` (cheapest stable
    // signal) — what's being tested is that the page's inline scripts
    // don't throw during init now that observer ctors and pushState
    // are installed.
    Target {
        name: "react.dev",
        category: "framework-docs",
        url: "https://react.dev/",
        js_fetch: false,
        // Next.js head-rendered: `<title data-next-head>React</title>`.
        // `document.title` returns the text "React".
        probe: Probe::Contains {
            js: "document.title",
            needle: "React",
        },
    },
    Target {
        name: "vuejs.org",
        category: "framework-docs",
        url: "https://vuejs.org/",
        js_fetch: false,
        // VitePress docs site.
        probe: Probe::Contains {
            js: "document.title",
            needle: "Vue.js",
        },
    },
    Target {
        name: "svelte.dev",
        category: "framework-docs",
        url: "https://svelte.dev/",
        js_fetch: false,
        // SvelteKit. Title is "Svelte • Web development for the rest of us".
        probe: Probe::Contains {
            js: "document.title",
            needle: "Svelte",
        },
    },
    Target {
        name: "nextjs.org",
        category: "framework-docs",
        url: "https://nextjs.org/",
        js_fetch: false,
        // Next.js self-hosted on Next.js.
        probe: Probe::Contains {
            js: "document.title",
            needle: "Next.js",
        },
    },
    // ---- Feature smoke probes ----
    //
    // These point at a cheap host (example.com) but the *probe* is the
    // interesting part: an inline JS expression that exercises one of
    // the recently-shipped globals and asserts a known-good value.
    // Catches regressions in the engine itself rather than in any
    // particular site.
    Target {
        name: "feature: URLSearchParams reflects into URL",
        category: "feature",
        url: "https://example.com",
        js_fetch: false,
        // Mutating searchParams must write back through `url.toString()`.
        probe: Probe::Contains {
            js: "(() => { const u = new URL('https://x/?a=1'); u.searchParams.set('b', '2'); return u.toString(); })()",
            needle: "a=1&b=2",
        },
    },
    Target {
        name: "feature: history.pushState updates location",
        category: "feature",
        url: "https://example.com",
        js_fetch: false,
        // pushState must update location.pathname synchronously.
        probe: Probe::Contains {
            js: "(() => { history.pushState({x:1}, '', '/probe-path'); return location.pathname; })()",
            needle: "/probe-path",
        },
    },
    Target {
        name: "feature: MutationObserver init does not throw",
        category: "feature",
        url: "https://example.com",
        js_fetch: false,
        // Observer ctors are noops but must accept the spec API surface
        // (callback arg + observe/disconnect/takeRecords methods).
        probe: Probe::Contains {
            js: "(() => { const o = new MutationObserver(() => {}); o.observe(document.body, {childList: true}); o.disconnect(); return 'observer-ok'; })()",
            needle: "observer-ok",
        },
    },
];

// ============================================================================
// Runner
// ============================================================================

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut filter: Option<String> = None;
    let mut markdown_out: Option<PathBuf> = None;
    let mut strict = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--filter" => {
                i += 1;
                filter = args.get(i).cloned();
            }
            "--markdown" => {
                i += 1;
                markdown_out = args.get(i).map(PathBuf::from);
            }
            "--strict" => strict = true,
            "--help" | "-h" => {
                eprintln!("usage: heso-compat-suite [--filter SUBSTR] [--markdown PATH] [--strict]");
                return;
            }
            other => {
                eprintln!("unknown flag: {other}");
                eprintln!("usage: heso-compat-suite [--filter SUBSTR] [--markdown PATH] [--strict]");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let fetch_engine = FetchEngine::new().expect("build fetch engine");

    // Per-process RSS sampler. We use `sysinfo` because it is the de-facto
    // cross-platform process-info crate in the Rust ecosystem (active
    // upstream, no unsafe in our code path, works on Windows where the
    // user runs heso). We refresh only this process's memory entry
    // between targets — `ProcessesToUpdate::Some(&[pid])` plus
    // `ProcessRefreshKind::nothing().with_memory()` skips the
    // workspace-wide process enumeration on every sample. `0` means
    // we couldn't determine the current PID (unsupported platform),
    // in which case every target row records `peak_rss_kb = 0` rather
    // than aborting the run.
    let mut sys = System::new();
    let self_pid: Option<Pid> = get_current_pid().ok();

    let mut results: Vec<TargetResult> = Vec::with_capacity(TARGETS.len());
    for t in TARGETS {
        if let Some(f) = filter.as_deref() {
            if !t.name.contains(f) && !t.category.contains(f) {
                continue;
            }
        }
        let mut r = run_target(t, &fetch_engine).await;
        r.peak_rss_kb = sample_rss_kb(&mut sys, self_pid);
        // Stream progress so the user sees something during long runs.
        eprintln!(
            "{:6} {:>5}ms  rss={:>7}KB  {}",
            r.status,
            r.ms_total,
            r.peak_rss_kb,
            r.name,
        );
        results.push(r);
    }

    let passed = results.iter().filter(|r| r.status == "ok").count();
    let total = results.len();
    let report = Report {
        results,
        summary: Summary {
            total,
            passed,
            failed: total - passed,
        },
    };

    let json = serde_json::to_string_pretty(&report)
        .expect("serialize report");
    println!("{json}");

    if let Some(path) = markdown_out.as_deref() {
        let md = render_markdown(&report);
        if let Err(e) = std::fs::write(path, md) {
            eprintln!("failed to write markdown to {}: {e}", path.display());
            std::process::exit(1);
        }
        eprintln!("wrote {}", path.display());
    }

    if strict && report.summary.failed > 0 {
        std::process::exit(1);
    }
}

/// Run one target end-to-end: fetch → JS engine setup → probe eval →
/// classify result.
async fn run_target(t: &Target, fetch_engine: &FetchEngine) -> TargetResult {
    let url = match Url::parse(t.url) {
        Ok(u) => u,
        Err(e) => return failure(t, "fetch_error", format!("invalid URL: {e}")),
    };
    let t0 = Instant::now();

    // ---- fetch leg ----
    let fetch_start = Instant::now();
    let (final_url, html) = match fetch_engine.fetch_text(&url).await {
        Ok(pair) => pair,
        Err(e) => return failure_timed(t, "fetch_error", format!("{e}"), t0, fetch_start, None),
    };
    let ms_fetch = fetch_start.elapsed().as_millis();

    // ---- engine + eval leg ----
    let eval_start = Instant::now();
    let engine = match if t.js_fetch {
        JsEngine::new_with_fetch(fetch_engine.client(), tokio::runtime::Handle::current())
    } else {
        JsEngine::new()
    } {
        Ok(e) => e,
        Err(e) => {
            return failure_timed(
                t,
                "js_error",
                format!("engine new: {e}"),
                t0,
                fetch_start,
                Some(eval_start),
            );
        }
    };
    engine.set_base_url(Some(final_url.clone()));

    let (js_code, expected): (&str, Expected) = match t.probe {
        Probe::Contains { js, needle } => (js, Expected::Contains(needle)),
        Probe::NonEmptyString { js } => (js, Expected::NonEmptyString),
        Probe::NumberAtLeast { js, min } => (js, Expected::NumberAtLeast(min)),
    };
    let policy = if t.js_fetch {
        ScriptFetchPolicy::Fetch
    } else {
        ScriptFetchPolicy::Skip
    };
    let eval_outcome = engine.eval_with_html_capture(&html, js_code, policy);
    let ms_eval = eval_start.elapsed().as_millis();
    let ms_total = t0.elapsed().as_millis();

    let (outcome, _scripts) = match eval_outcome {
        Ok(pair) => pair,
        Err(e) => {
            return TargetResult {
                name: t.name.to_string(),
                category: t.category.to_string(),
                url: t.url.to_string(),
                status: "js_error".to_string(),
                ms_total,
                ms_fetch,
                ms_eval,
                peak_rss_kb: 0,
                value: None,
                error: Some(format!("{e:?}")),
            };
        }
    };

    let (status, error) = expected.check(&outcome.value);
    TargetResult {
        name: t.name.to_string(),
        category: t.category.to_string(),
        url: t.url.to_string(),
        status: status.to_string(),
        ms_total,
        ms_fetch,
        ms_eval,
        peak_rss_kb: 0,
        value: Some(truncate_value(outcome.value)),
        error,
    }
}

/// Sample this process's resident-set-size, in kilobytes.
///
/// Refreshes only **our own** process entry (not every PID on the
/// system) and only the memory field — keeps the per-target cost in
/// the sub-millisecond range. Returns `0` if we couldn't determine
/// the current PID at startup (e.g. unsupported platform) or if the
/// refresh couldn't see the process for some reason.
fn sample_rss_kb(sys: &mut System, pid: Option<Pid>) -> u64 {
    let Some(pid) = pid else { return 0 };
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    // `Process::memory()` is documented to return bytes; cast to KB.
    sys.process(pid).map(|p| p.memory() / 1024).unwrap_or(0)
}

/// Inlined version of the probe assertion that doesn't carry the `js`
/// string (so it can be moved out of the `Target` after we've started
/// the eval).
enum Expected {
    Contains(&'static str),
    NonEmptyString,
    NumberAtLeast(i64),
}

impl Expected {
    fn check(&self, val: &serde_json::Value) -> (&'static str, Option<String>) {
        match self {
            Expected::Contains(needle) => {
                let s = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                if s.contains(needle) {
                    ("ok", None)
                } else {
                    (
                        "assertion_failed",
                        Some(format!("value did not contain {needle:?}: got {s:?}")),
                    )
                }
            }
            Expected::NonEmptyString => match val {
                serde_json::Value::String(s) if !s.is_empty() => ("ok", None),
                other => (
                    "assertion_failed",
                    Some(format!("expected non-empty string; got {other}")),
                ),
            },
            Expected::NumberAtLeast(min) => match val.as_i64() {
                Some(n) if n >= *min => ("ok", None),
                Some(n) => (
                    "assertion_failed",
                    Some(format!("expected >= {min}; got {n}")),
                ),
                None => (
                    "assertion_failed",
                    Some(format!("expected number; got {val}")),
                ),
            },
        }
    }
}

fn failure(t: &Target, status: &str, msg: String) -> TargetResult {
    TargetResult {
        name: t.name.to_string(),
        category: t.category.to_string(),
        url: t.url.to_string(),
        status: status.to_string(),
        ms_total: 0,
        ms_fetch: 0,
        ms_eval: 0,
        peak_rss_kb: 0,
        value: None,
        error: Some(msg),
    }
}

fn failure_timed(
    t: &Target,
    status: &str,
    msg: String,
    t0: Instant,
    fetch_start: Instant,
    eval_start: Option<Instant>,
) -> TargetResult {
    let ms_total = t0.elapsed().as_millis();
    let ms_fetch = fetch_start.elapsed().as_millis();
    let ms_eval = eval_start.map(|s| s.elapsed().as_millis()).unwrap_or(0);
    TargetResult {
        name: t.name.to_string(),
        category: t.category.to_string(),
        url: t.url.to_string(),
        status: status.to_string(),
        ms_total,
        ms_fetch,
        ms_eval,
        peak_rss_kb: 0,
        value: None,
        error: Some(msg),
    }
}

/// Trim long string values so the JSON report stays human-readable.
fn truncate_value(v: serde_json::Value) -> serde_json::Value {
    const MAX: usize = 240;
    match v {
        serde_json::Value::String(s) if s.len() > MAX => {
            let mut t = s.chars().take(MAX).collect::<String>();
            t.push_str("…");
            serde_json::Value::String(t)
        }
        other => other,
    }
}

/// Render a markdown scorecard. Used when `--markdown PATH` is passed.
///
/// Shape:
///
/// ```markdown
/// # heso compatibility scorecard
///
/// | Site | Category | Status | Total ms | Fetch ms | Eval ms | Peak RSS KB |
/// |---|---|---|---:|---:|---:|---:|
/// | example.com | smoke | ✅ ok | 47 | 41 | 6 | 24560 |
/// ```
///
/// The `Peak RSS KB` column is sampled after each target finishes. heso
/// does not release memory between targets, so the column is
/// monotonically non-decreasing — the last row's value is the peak
/// resident-set-size across the whole suite. This is the number the
/// README's "tiny idle RAM" claim should be compared against.
fn render_markdown(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "# heso compatibility scorecard");
    let _ = writeln!(&mut out);
    let _ = writeln!(
        &mut out,
        "Generated by `heso-compat-suite`. {} / {} targets ok.",
        report.summary.passed, report.summary.total
    );
    let _ = writeln!(&mut out);
    let _ = writeln!(
        &mut out,
        "`Peak RSS KB` is this process's resident-set-size sampled after each target. heso does not release memory between targets, so values are monotonically non-decreasing across the run."
    );
    let _ = writeln!(&mut out);
    let _ = writeln!(
        &mut out,
        "| Site | Category | Status | Total ms | Fetch ms | Eval ms | Peak RSS KB |"
    );
    let _ = writeln!(&mut out, "|---|---|---|---:|---:|---:|---:|");
    for r in &report.results {
        let icon = if r.status == "ok" { "✅" } else { "❌" };
        let _ = writeln!(
            &mut out,
            "| {} | {} | {} {} | {} | {} | {} | {} |",
            r.name,
            r.category,
            icon,
            r.status,
            r.ms_total,
            r.ms_fetch,
            r.ms_eval,
            r.peak_rss_kb,
        );
    }
    out
}
