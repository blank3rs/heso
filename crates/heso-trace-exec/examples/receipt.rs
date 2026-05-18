//! Run a small trace end-to-end and pretty-print the resulting [`Receipt`].
//!
//! ```text
//! cargo run --example receipt -p heso-trace-exec
//! ```
//!
//! Shows: the full receipt shape (trace + results + trace_hash + cost +
//! optional failure index/message), and confirms that `trace_hash` is
//! deterministic across two back-to-back runs of the same trace.

use heso_core::{Result as HesoResult, Url};
use heso_engine_api::{EngineApi, Page};
use heso_primitives::{
    CdInput, CdTarget, PrimitiveOp, PwdInput, ScreenshotInput,
};
use heso_trace::{Mode, Receipt};
use heso_trace_exec::{run, SessionConfig};

struct DemoEngine;
struct DemoPage(Url);

impl Page for DemoPage {
    fn url(&self) -> &Url {
        &self.0
    }
    async fn text(&self) -> HesoResult<String> {
        Ok(String::from("[DemoPage placeholder]"))
    }
}

impl EngineApi for DemoEngine {
    type Page = DemoPage;
    async fn open(&self, url: &Url) -> HesoResult<Self::Page> {
        println!("    [engine] open({url})");
        Ok(DemoPage(url.clone()))
    }
}

fn summarize(label: &str, r: &Receipt) {
    println!("== {label} ==");
    println!(
        "  status:     {}",
        if r.is_ok() { "ok" } else { "failed" }
    );
    println!("  trace len:  {}", r.trace.len());
    println!("  results:    {}", r.results.len());
    println!("  trace_hash: {}", r.trace_hash);
    println!("  seed:       {}", r.seed);
    println!("  mode:       {:?}", r.mode);
    if let Some(idx) = r.failed_at {
        println!("  failed_at:  {idx}");
        if let Some(err) = &r.error {
            println!("  error:      {err}");
        }
    }
    println!();
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let engine = DemoEngine;
    let cfg = SessionConfig {
        seed: 42,
        mode: Mode::Deterministic,
        planner_id: String::from("planner-stub"),
    };

    // ---- successful trace ----
    let ok_trace = vec![
        PrimitiveOp::Cd(CdInput {
            target: CdTarget::Url {
                url: Url::parse("https://example.com/").unwrap(),
            },
        }),
        PrimitiveOp::Cd(CdInput {
            target: CdTarget::Url {
                url: Url::parse("https://example.com/about").unwrap(),
            },
        }),
    ];
    let ok = run(&engine, &ok_trace, &cfg).await;
    summarize("Receipt: successful trace (2 cds)", &ok);

    // ---- failing trace ----
    let fail_trace = vec![
        PrimitiveOp::Cd(CdInput {
            target: CdTarget::Url {
                url: Url::parse("https://example.com/").unwrap(),
            },
        }),
        PrimitiveOp::Pwd(PwdInput::default()), // stub → NotImplemented
        PrimitiveOp::Screenshot(ScreenshotInput::default()), // never reached
    ];
    let fail = run(&engine, &fail_trace, &cfg).await;
    summarize("Receipt: trace that hits a stubbed op", &fail);

    // ---- determinism check ----
    let again = run(&engine, &ok_trace, &cfg).await;
    println!("== Determinism check ==");
    println!("  trace_hash of run 1: {}", ok.trace_hash);
    println!("  trace_hash of run 2: {}", again.trace_hash);
    println!(
        "  identical: {}",
        if ok.trace_hash == again.trace_hash {
            "yes"
        } else {
            "NO"
        }
    );
    println!();

    // ---- canonical JSON of the successful receipt ----
    let json = serde_json::to_string_pretty(&ok).expect("receipt serializes");
    println!("== Receipt (pretty JSON) ==");
    println!("{json}");
}
