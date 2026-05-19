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

    let mut results: Vec<TargetResult> = Vec::with_capacity(TARGETS.len());
    for t in TARGETS {
        if let Some(f) = filter.as_deref() {
            if !t.name.contains(f) && !t.category.contains(f) {
                continue;
            }
        }
        let r = run_target(t, &fetch_engine).await;
        // Stream progress so the user sees something during long runs.
        eprintln!(
            "{:6} {:>5}ms  {}",
            r.status,
            r.ms_total,
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
        value: Some(truncate_value(outcome.value)),
        error,
    }
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
/// | Site | Category | Status | Total ms | Fetch ms | Eval ms |
/// |---|---|---|---:|---:|---:|
/// | example.com | smoke | ✅ ok | 47 | 41 | 6 |
/// ```
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
        "| Site | Category | Status | Total ms | Fetch ms | Eval ms |"
    );
    let _ = writeln!(&mut out, "|---|---|---|---:|---:|---:|");
    for r in &report.results {
        let icon = if r.status == "ok" { "✅" } else { "❌" };
        let _ = writeln!(
            &mut out,
            "| {} | {} | {} {} | {} | {} | {} |",
            r.name, r.category, icon, r.status, r.ms_total, r.ms_fetch, r.ms_eval
        );
    }
    out
}
