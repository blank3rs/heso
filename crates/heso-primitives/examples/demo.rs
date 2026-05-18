//! Construct a small trace, serialize it as JSON, execute each op against a
//! minimal in-process engine, and print the results.
//!
//! ```text
//! cargo run --example demo -p heso-primitives
//! ```
//!
//! Shows: the JSON shape of a trace (what receipts will carry), which ops
//! work today, which ops return [`Error::NotImplemented`] with which gating
//! task ID.

use heso_core::{Result as HesoResult, Url};
use heso_engine_api::{EngineApi, Page};
use heso_primitives::{
    execute, CatInput, CatTarget, CdInput, CdTarget, EnvPath, FindInput, FindPredicate,
    LsInput, LsTarget, PrimitiveOp, PwdInput, ScreenshotInput, WaitCondition, WaitInput,
};

/// Stand-in for the real Servo engine that's coming in M1. Just records what
/// it was asked to open and echoes a URL back.
struct DemoEngine;

struct DemoPage(Url);

impl Page for DemoPage {
    fn url(&self) -> &Url {
        &self.0
    }
    async fn text(&self) -> HesoResult<String> {
        Ok(String::from("[DemoPage placeholder text]"))
    }
}

impl EngineApi for DemoEngine {
    type Page = DemoPage;
    async fn open(&self, url: &Url) -> HesoResult<Self::Page> {
        println!("    [engine] open({url})");
        Ok(DemoPage(url.clone()))
    }
}

fn op_tag(op: &PrimitiveOp) -> String {
    serde_json::to_value(op)
        .ok()
        .and_then(|v| v["op"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "?".to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url = Url::parse("https://example.com/").expect("static URL parses");

    let trace: Vec<PrimitiveOp> = vec![
        PrimitiveOp::Cd(CdInput {
            target: CdTarget::Url { url: url.clone() },
        }),
        PrimitiveOp::Pwd(PwdInput::default()),
        PrimitiveOp::Ls(LsInput { target: LsTarget::Page }),
        PrimitiveOp::Find(FindInput {
            predicate: FindPredicate::Role { role: String::from("link") },
        }),
        PrimitiveOp::Cat(CatInput {
            target: CatTarget::Env { path: EnvPath::Cookie { name: String::from("sid") } },
        }),
        PrimitiveOp::Wait(WaitInput {
            condition: WaitCondition::Sleep { ms: 50 },
            timeout_ms: 200,
        }),
        PrimitiveOp::Screenshot(ScreenshotInput::default()),
    ];

    println!("== Trace (pretty JSON, what a receipt's `trace` field would look like) ==");
    println!(
        "{}",
        serde_json::to_string_pretty(&trace).expect("trace serializes")
    );

    let compact = serde_json::to_string(&trace).expect("trace serializes");
    println!();
    println!("== Trace (compact, {} bytes) ==", compact.len());
    println!("{compact}");

    println!();
    println!("== Execution ==");
    let engine = DemoEngine;
    for op in &trace {
        let name = op_tag(op);
        match execute(&engine, op).await {
            Ok(res) => {
                let res_json = serde_json::to_string(&res).expect("result serializes");
                println!("  {:>11} -> OK   {res_json}", name);
            }
            Err(err) => {
                println!("  {:>11} -> err  {err}", name);
            }
        }
    }
}
