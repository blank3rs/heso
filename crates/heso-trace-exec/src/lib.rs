//! # heso-trace-exec
//!
//! The trace runner. Walks a [`Trace`], dispatches each
//! [`heso_primitives::PrimitiveOp`] via [`heso_primitives::execute`] against
//! an [`EngineApi`] instance, and returns a [`Receipt`].
//!
//! Execution stops at the first failed primitive. The receipt records the
//! full planned trace, the per-op `results` produced up to (but not
//! including) the failure, and the index + message of the failure.
//!
//! ## Responsibilities (M2)
//!
//! - Walk the trace in order.
//! - Dispatch each op via [`heso_primitives::execute`].
//! - Build a [`Receipt`] with [`trace_hash`] over the canonical JSON of the
//!   trace.
//!
//! ## Out of scope (later milestones)
//!
//! - **Signing.** `Receipt::signed` is left empty here; M4 (`heso-identity`)
//!   adds Ed25519 signing.
//! - **Cost accounting.** `Receipt::cost` is zeroed today; the engine has to
//!   feed back byte counts + CPU time once T-013/T-017 land.
//! - **Page hashes.** `Receipt::pages_seen` is empty until the engine reports
//!   content hashes for the pages it touched (T-013).
//! - **Real determinism.** The runner is deterministic by construction (no
//!   clocks/RNG of its own), but the engine isn't yet — T-014/T-015/T-017
//!   land that. Until then, runs in [`Mode::Deterministic`] are *structurally*
//!   reproducible but not *byte-identical-engine-output* reproducible.
//!
//! [`EngineApi`]: heso_engine_api::EngineApi
//! [`trace_hash`]: heso_trace::trace_hash

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use heso_engine_api::EngineApi;
use heso_primitives::{execute, PrimitiveResult};
use heso_trace::{trace_hash, Cost, Mode, Receipt, Trace};

/// Per-session configuration the runner needs to build a [`Receipt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// Session seed. Threaded into the receipt so verifiers can reproduce.
    pub seed: u64,
    /// Operating mode.
    pub mode: Mode,
    /// Planner version ID. Empty at this layer; the planner (M3) fills it in
    /// when it constructs the SessionConfig before handing the trace to the
    /// runner.
    pub planner_id: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            mode: Mode::Deterministic,
            planner_id: String::new(),
        }
    }
}

/// Run a trace and return a [`Receipt`].
///
/// Stops at the first failed primitive. The receipt records:
/// - the full trace
/// - per-op `results` up to (but excluding) the failed op
/// - `failed_at = Some(index)` of the failed op
/// - `error = Some(message)` of the failed op
///
/// `Receipt::trace_hash` is BLAKE3 over the trace's canonical JSON.
///
/// No signing (M4). `pages_seen` empty until the engine reports content
/// hashes (T-013). `cost` zeroed until the engine threads cost data through.
pub async fn run<E: EngineApi>(engine: &E, trace: &Trace, config: &SessionConfig) -> Receipt {
    let mut results: Vec<PrimitiveResult> = Vec::with_capacity(trace.len());
    let mut failed_at: Option<usize> = None;
    let mut error: Option<String> = None;

    for (i, op) in trace.iter().enumerate() {
        match execute(engine, op).await {
            Ok(r) => results.push(r),
            Err(e) => {
                failed_at = Some(i);
                error = Some(e.to_string());
                break;
            }
        }
    }

    Receipt {
        trace: trace.clone(),
        results,
        pages_seen: Vec::new(),
        trace_hash: trace_hash(trace),
        planner_id: config.planner_id.clone(),
        seed: config.seed,
        mode: config.mode,
        cost: Cost::default(),
        failed_at,
        error,
        signed: String::new(),
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use heso_core::{Result as HesoResult, Url};
    use heso_engine_api::Page;
    use heso_primitives::{
        CdInput, CdTarget, PrimitiveOp, PwdInput, ScreenshotInput,
    };

    struct DummyEngine;
    struct DummyPage(Url);

    impl Page for DummyPage {
        fn url(&self) -> &Url {
            &self.0
        }
        async fn text(&self) -> HesoResult<String> {
            Err(heso_core::Error::NotImplemented("DummyPage::text"))
        }
    }

    impl EngineApi for DummyEngine {
        type Page = DummyPage;
        async fn open(&self, url: &Url) -> HesoResult<Self::Page> {
            Ok(DummyPage(url.clone()))
        }
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn cd(url: &str) -> PrimitiveOp {
        PrimitiveOp::Cd(CdInput { target: CdTarget::Url { url: u(url) } })
    }

    #[tokio::test]
    async fn run_a_single_cd_succeeds_and_records_result() {
        let trace = vec![cd("https://example.com/")];
        let r = run(&DummyEngine, &trace, &SessionConfig::default()).await;

        assert!(r.is_ok(), "expected ok receipt, got {r:?}");
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.failed_at, None);
        assert!(r.error.is_none());
        assert_eq!(r.trace.len(), 1);
        assert_eq!(r.trace_hash.len(), 64);
        assert_eq!(r.mode, Mode::Deterministic);
    }

    #[tokio::test]
    async fn run_stops_at_first_failed_op_and_records_partial_results() {
        let trace = vec![
            cd("https://example.com/"),
            PrimitiveOp::Pwd(PwdInput::default()), // stubbed → NotImplemented
            cd("https://example.org/"),            // never reached
        ];
        let r = run(&DummyEngine, &trace, &SessionConfig::default()).await;

        assert!(!r.is_ok());
        assert_eq!(r.failed_at, Some(1));
        assert_eq!(r.results.len(), 1, "only the first cd produced a result");
        let err = r.error.as_deref().unwrap();
        assert!(err.contains("pwd"), "error mentions failing op: {err}");
        assert!(err.contains("T-013"), "error names the gating task: {err}");
        // Full trace is preserved even though execution halted.
        assert_eq!(r.trace.len(), 3);
    }

    #[tokio::test]
    async fn trace_hash_in_receipt_is_stable_across_runs() {
        let trace = vec![cd("https://example.com/")];
        let r1 = run(&DummyEngine, &trace, &SessionConfig::default()).await;
        let r2 = run(&DummyEngine, &trace, &SessionConfig::default()).await;
        assert_eq!(r1.trace_hash, r2.trace_hash);
    }

    #[tokio::test]
    async fn trace_hash_in_receipt_differs_for_different_traces() {
        let r1 = run(&DummyEngine, &vec![cd("https://example.com/")], &SessionConfig::default()).await;
        let r2 = run(&DummyEngine, &vec![cd("https://example.org/")], &SessionConfig::default()).await;
        assert_ne!(r1.trace_hash, r2.trace_hash);
    }

    #[tokio::test]
    async fn session_config_threads_seed_planner_and_mode_into_receipt() {
        let trace = vec![cd("https://example.com/")];
        let cfg = SessionConfig {
            seed: 1234,
            mode: Mode::Recording,
            planner_id: "planner-v0.1".into(),
        };
        let r = run(&DummyEngine, &trace, &cfg).await;
        assert_eq!(r.seed, 1234);
        assert_eq!(r.mode, Mode::Recording);
        assert_eq!(r.planner_id, "planner-v0.1");
    }

    #[tokio::test]
    async fn receipt_serializes_then_deserializes_intact() {
        let trace = vec![
            cd("https://example.com/"),
            PrimitiveOp::Screenshot(ScreenshotInput::default()),
        ];
        let r = run(&DummyEngine, &trace, &SessionConfig::default()).await;

        let json = serde_json::to_string(&r).expect("receipt serializes");
        let back: Receipt = serde_json::from_str(&json).expect("receipt deserializes");
        assert_eq!(r, back);
    }

    #[tokio::test]
    async fn empty_trace_produces_ok_empty_receipt() {
        let trace: Trace = vec![];
        let r = run(&DummyEngine, &trace, &SessionConfig::default()).await;
        assert!(r.is_ok());
        assert_eq!(r.results.len(), 0);
        assert_eq!(r.trace.len(), 0);
    }
}
