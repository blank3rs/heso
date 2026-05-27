//! # wait_for
//!
//! Block-until-condition primitive for [`crate::JsSession`].
//!
//! The motivating gap: an agent calling `heso click @e3` followed by
//! `heso eval-dom '...'` has no way to ask "block until the page
//! actually transitioned" / "block until `#dashboard` appears" without
//! manually pumping the engine in a polling loop. Real browser drivers
//! (Playwright, Puppeteer) ship `page.waitForSelector` /
//! `page.waitForURL` / `page.waitForLoadState('networkidle')` for
//! exactly this. We mirror the contract shape so users porting from
//! Playwright land on something familiar.
//!
//! ## Conditions
//!
//! [`WaitCondition`] enumerates the five kinds:
//!
//! - [`WaitCondition::SelectorExists`] — `document.querySelector(css)` returns non-null.
//! - [`WaitCondition::TextContains`] — `document.body.textContent.includes(needle)`.
//! - [`WaitCondition::UrlMatches`] — the engine's `base_url` matches a regex.
//! - [`WaitCondition::NetworkIdle`] — no in-flight `fetch()` for `idle_window_ms`.
//! - [`WaitCondition::TimeElapsed`] — sleep, advancing the virtual clock.
//!
//! ## Driving the loop
//!
//! The condition is checked on a tight cooperative loop that interleaves:
//!
//! 1. Pump microtasks + drain queued fetches via
//!    [`crate::JsEngine::run_pending_jobs`]. This is what lets async
//!    page code progress while we wait.
//! 2. Advance the virtual clock by `tick_ms` (default 25 ms) so any
//!    `setTimeout(..., 500)` hydration callback fires when we expect.
//! 3. Evaluate the condition.
//! 4. If satisfied → return [`WaitOutcome::ok`].
//! 5. If wall-clock timeout exceeded → return [`WaitOutcome::timeout`].
//! 6. Otherwise sleep `tick_ms` real wall time so we don't burn CPU.
//!
//! ## Determinism note
//!
//! [`WaitCondition::TimeElapsed`] advances the **virtual** clock — same
//! mechanism the deterministic-replay path uses ([ADR 0008]). It does
//! not block on wall time, so deterministic traces stay reproducible.
//! Every other condition can in principle complete instantly if the
//! page is already in the target state; wall-time is only spent when
//! the condition demands real work (a fetch round-trip, a setTimeout
//! callback firing).
//!
//! [ADR 0008]: ../../decisions/0008-determinism-by-construction.md

use std::time::{Duration, Instant};

use regex::Regex;

use crate::engine::{EvalError, JsEngine};
use crate::session::JsSession;

/// Default tick granularity — wake every 25 ms, check the condition,
/// pump microtasks, advance the virtual clock. Matches the implicit
/// granularity Playwright uses for its auto-wait polling.
pub const DEFAULT_TICK_MS: u64 = 25;

/// Default network-idle window — 500 ms of no in-flight fetches counts
/// as "idle". Matches Playwright's `networkidle` semantics
/// (500 ms with zero in-flight requests).
pub const DEFAULT_NETWORK_IDLE_WINDOW_MS: u64 = 500;

/// Default overall timeout — 30 s. Matches Playwright's
/// `page.waitForSelector` default timeout.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// The kind of condition to wait for. See module docs for semantics.
#[derive(Debug, Clone)]
pub enum WaitCondition {
    /// `document.querySelector(css) !== null`.
    SelectorExists(String),
    /// `document.body.textContent.includes(needle)`.
    TextContains(String),
    /// `window.location.href` matches the compiled regex. Matches the
    /// engine's [`crate::JsEngine::base_url`] (which tracks the current
    /// URL across `pushState` / navigate calls).
    UrlMatches(Regex),
    /// No queued fetches for `idle_window_ms` of cooperative loop time.
    NetworkIdle {
        /// Continuous quiet-window duration that counts as "idle", in ms.
        idle_window_ms: u64,
    },
    /// Advance the virtual clock by `duration_ms` (firing any
    /// `setTimeout` callbacks along the way) and return success.
    TimeElapsed {
        /// How much virtual time to advance, in ms.
        duration_ms: u64,
    },
}

impl WaitCondition {
    /// Stable string label for the condition. Used in the JSON
    /// envelope returned by [`JsSession::wait_for`] so callers can
    /// echo the condition without having to round-trip the enum.
    pub fn label(&self) -> String {
        match self {
            WaitCondition::SelectorExists(css) => format!("selector-exists {css}"),
            WaitCondition::TextContains(s) => format!("text-contains {s}"),
            WaitCondition::UrlMatches(r) => format!("url-matches {}", r.as_str()),
            WaitCondition::NetworkIdle { idle_window_ms } => {
                format!("network-idle (idle-window={idle_window_ms}ms)")
            }
            WaitCondition::TimeElapsed { duration_ms } => format!("time {duration_ms}ms"),
        }
    }
}

/// Result of a [`JsSession::wait_for`] call.
#[derive(Debug, Clone)]
pub struct WaitOutcome {
    /// Whether the condition was satisfied before timeout.
    pub ok: bool,
    /// Virtual-clock duration the wait covered, in milliseconds.
    /// Reads the same `VirtualClock` that backs `Date.now()` and
    /// `setTimeout`, so this value is byte-identical across runs in
    /// deterministic mode and is what `to_json` writes into the
    /// canonical envelope.
    pub elapsed_ms: u64,
    /// Wall-clock duration the wait actually took, in milliseconds.
    /// Real time spent in the loop. Diagnostic only — not stable
    /// across runs and so written into the envelope under a
    /// `_unsafe` suffix.
    pub wall_elapsed_ms: u64,
    /// Stable label for the condition (see [`WaitCondition::label`]).
    pub condition: String,
    /// `"timeout"` when [`Self::ok`] is `false`; `None` on success.
    pub error: Option<String>,
}

impl WaitOutcome {
    fn ok(virtual_elapsed_ms: u64, wall_elapsed: Duration, condition: &str) -> Self {
        Self {
            ok: true,
            elapsed_ms: virtual_elapsed_ms,
            wall_elapsed_ms: ms_from_duration(wall_elapsed),
            condition: condition.to_owned(),
            error: None,
        }
    }

    fn timeout(virtual_elapsed_ms: u64, wall_elapsed: Duration, condition: &str) -> Self {
        Self {
            ok: false,
            elapsed_ms: virtual_elapsed_ms,
            wall_elapsed_ms: ms_from_duration(wall_elapsed),
            condition: condition.to_owned(),
            error: Some("timeout".to_owned()),
        }
    }

    /// Render the outcome as the JSON envelope CLI/RPC callers see.
    pub fn to_json(&self) -> serde_json::Value {
        let mut body = serde_json::json!({
            "ok": self.ok,
            "elapsed_ms": self.elapsed_ms,
            "wall_elapsed_ms_unsafe": self.wall_elapsed_ms,
            "condition": self.condition,
        });
        if let Some(err) = self.error.as_deref() {
            body["error"] = serde_json::Value::String(err.to_owned());
        }
        body
    }
}

fn ms_from_duration(d: Duration) -> u64 {
    // Saturating cast — a wait that exceeds ~584 million years
    // is the caller's problem, not ours.
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Drive `condition` on `engine` until it is satisfied or `timeout`
/// elapses. Caller-facing entry point is [`JsSession::wait_for`]; this
/// is factored out so the one-shot CLI path (which builds a transient
/// session) can share the same loop.
///
/// Each loop iteration: pump pending JS jobs (microtasks + queued
/// fetches), advance the virtual clock, evaluate the condition. If
/// the condition is satisfied → return ok. If wall-clock time
/// exceeded the timeout → return a timeout outcome. Otherwise sleep
/// `tick_ms` and loop.
pub fn wait_for_on_engine(
    engine: &JsEngine,
    condition: &WaitCondition,
    timeout: Duration,
    tick_ms: u64,
) -> Result<WaitOutcome, EvalError> {
    let label = condition.label();
    let virtual_start_ms = engine.virtual_now_ms();
    // Wall clock is for cancellation only — a `--timeout 30s` flag
    // means 30 s of real time before we give up, regardless of how
    // many virtual ms the engine advanced through hydration.
    let wall_start = Instant::now();

    // TimeElapsed is special: it's a deterministic clock advance, not
    // a wall-clock wait. We jump the virtual clock by the full
    // duration in one call, which lets any setTimeout-based hydration
    // fire all at once, and return immediately.
    if let WaitCondition::TimeElapsed { duration_ms } = condition {
        engine.advance_clock(*duration_ms)?;
        engine.run_pending_jobs()?;
        let virtual_elapsed = engine.virtual_now_ms().saturating_sub(virtual_start_ms);
        return Ok(WaitOutcome::ok(virtual_elapsed, wall_start.elapsed(), &label));
    }

    // For NetworkIdle we track how long the queue has been empty.
    // The condition is satisfied when the engine has had zero
    // pending fetches AND zero pending timers for a continuous
    // `idle_window_ms`. We track idleness in virtual ms so the
    // reported elapsed time is deterministic.
    let mut idle_since_virtual_ms: Option<u64> = None;

    let tick = Duration::from_millis(tick_ms.max(1));

    loop {
        // (1) Pump microtasks + queued fetches. This is what makes
        // `await fetch(...)` patterns progress while we wait.
        engine.run_pending_jobs()?;

        // (2) Advance the virtual clock so setTimeout-based hydration
        // makes forward progress. We advance by the tick on every
        // pass — this matches the "virtual time tracks wall time"
        // expectation for the wait path. (Deterministic-replay paths
        // can drive this differently via the explicit `--time`
        // condition.)
        engine.advance_clock(tick_ms.max(1))?;

        // (3) Pump again — timer callbacks queue new microtasks /
        // fetches, and we want them visible to the next check.
        engine.run_pending_jobs()?;

        // (4) Evaluate the condition.
        let satisfied = match condition {
            WaitCondition::SelectorExists(css) => check_selector_exists(engine, css)?,
            WaitCondition::TextContains(needle) => check_text_contains(engine, needle)?,
            WaitCondition::UrlMatches(re) => check_url_matches(engine, re),
            WaitCondition::NetworkIdle { idle_window_ms } => {
                let pending = engine.pending_fetches() + engine.pending_timers();
                if pending == 0 {
                    let now_ms = engine.virtual_now_ms();
                    let since = *idle_since_virtual_ms.get_or_insert(now_ms);
                    now_ms.saturating_sub(since) >= *idle_window_ms
                } else {
                    idle_since_virtual_ms = None;
                    false
                }
            }
            WaitCondition::TimeElapsed { .. } => unreachable!("handled above"),
        };

        let virtual_elapsed = engine.virtual_now_ms().saturating_sub(virtual_start_ms);

        if satisfied {
            return Ok(WaitOutcome::ok(virtual_elapsed, wall_start.elapsed(), &label));
        }

        // (5) Timeout check is wall-clock, not virtual. A 30 s wait
        // means 30 s of real time the agent / CI is willing to spend.
        let wall_elapsed = wall_start.elapsed();
        if wall_elapsed >= timeout {
            return Ok(WaitOutcome::timeout(virtual_elapsed, wall_elapsed, &label));
        }

        // (6) Sleep until the next tick. We sleep on real wall time
        // so the wait actually yields the thread back to the OS;
        // burning CPU in a busy-loop would punish hosts running
        // many parallel waits.
        std::thread::sleep(tick);
    }
}

fn check_selector_exists(engine: &JsEngine, css: &str) -> Result<bool, EvalError> {
    let css_lit = serde_json::to_string(css)
        .map_err(|e| EvalError::Engine(format!("encode selector: {e}")))?;
    let script = format!("(document.querySelector({css_lit}) !== null)");
    let outcome = engine.eval(&script)?;
    Ok(outcome.value.as_bool().unwrap_or(false))
}

fn check_text_contains(engine: &JsEngine, needle: &str) -> Result<bool, EvalError> {
    let needle_lit = serde_json::to_string(needle)
        .map_err(|e| EvalError::Engine(format!("encode text needle: {e}")))?;
    // `document.body.textContent` covers the visible-text path the
    // spec defines on Node. Empty when body is absent.
    let script = format!(
        "((document.body && document.body.textContent) || '').includes({needle_lit})"
    );
    let outcome = engine.eval(&script)?;
    Ok(outcome.value.as_bool().unwrap_or(false))
}

fn check_url_matches(engine: &JsEngine, re: &Regex) -> bool {
    // Read `globalThis.location.href` from JS rather than the
    // engine's `base_url`. `history.pushState` (per WHATWG HTML §7.7)
    // updates `location.href` in place WITHOUT firing the engine's
    // `set_base_url` — that's reserved for cross-document navigation.
    // Matching against `location.href` lets us observe SPA route
    // changes that never re-fetch.
    //
    // Falls back to the engine's `base_url` if the JS read fails
    // (engine not fully initialized, location global not installed,
    // or `location.href` is non-string).
    if let Ok(outcome) = engine.eval(
        "(typeof location !== 'undefined' && location && location.href) ? String(location.href) : ''",
    ) {
        if let Some(s) = outcome.value.as_str() {
            if !s.is_empty() {
                return re.is_match(s);
            }
        }
    }
    match engine.base_url() {
        Some(url) => re.is_match(url.as_str()),
        None => false,
    }
}

impl JsSession {
    /// Block until `condition` is satisfied or `timeout` elapses.
    ///
    /// On each tick (default 25 ms) the engine pumps pending
    /// microtasks + queued fetches, advances the virtual clock by the
    /// tick, then re-evaluates the condition.
    ///
    /// Returns the [`WaitOutcome`] verbatim — callers should inspect
    /// `outcome.ok`; only [`EvalError`] propagates if the JS engine
    /// itself fails (e.g. a malformed selector triggers a syntax
    /// error inside `querySelector`).
    ///
    /// See [`WaitCondition`] for the supported conditions and the
    /// module docs for the loop's design rationale.
    pub fn wait_for(
        &self,
        condition: &WaitCondition,
        timeout: Duration,
    ) -> Result<WaitOutcome, EvalError> {
        wait_for_on_engine(self.engine(), condition, timeout, DEFAULT_TICK_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    fn url() -> Url {
        Url::parse("https://example.com/").unwrap()
    }

    #[test]
    fn selector_exists_succeeds_when_element_already_in_dom() {
        let html = "<!doctype html><html><body><div id=ready></div></body></html>";
        let (sess, _) = JsSession::open(html, url()).unwrap();
        let outcome = sess
            .wait_for(
                &WaitCondition::SelectorExists("#ready".into()),
                Duration::from_millis(500),
            )
            .unwrap();
        assert!(outcome.ok, "outcome: {:?}", outcome);
    }

    #[test]
    fn selector_exists_times_out_when_never_appears() {
        let html = "<!doctype html><html><body></body></html>";
        let (sess, _) = JsSession::open(html, url()).unwrap();
        let outcome = sess
            .wait_for(
                &WaitCondition::SelectorExists("#never".into()),
                Duration::from_millis(150),
            )
            .unwrap();
        assert!(!outcome.ok);
        assert_eq!(outcome.error.as_deref(), Some("timeout"));
    }

    #[test]
    fn selector_exists_returns_after_settimeout_appends_element() {
        // Page schedules `<div id=ready>` to appear after 50 virtual ms.
        // The wait loop's per-tick advance_clock should fire that
        // timer well before the 1 s timeout.
        let html = r#"<!doctype html><html><body>
            <script>
              setTimeout(() => {
                const d = document.createElement('div');
                d.id = 'ready';
                document.body.appendChild(d);
              }, 50);
            </script>
        </body></html>"#;
        let (sess, _) = JsSession::open(html, url()).unwrap();
        let outcome = sess
            .wait_for(
                &WaitCondition::SelectorExists("#ready".into()),
                Duration::from_millis(2_000),
            )
            .unwrap();
        assert!(outcome.ok, "did not see #ready in time: {:?}", outcome);
    }

    #[test]
    fn text_contains_matches_visible_body_text() {
        let html = "<!doctype html><html><body><p>Welcome back, Akshay.</p></body></html>";
        let (sess, _) = JsSession::open(html, url()).unwrap();
        let outcome = sess
            .wait_for(
                &WaitCondition::TextContains("Welcome".into()),
                Duration::from_millis(500),
            )
            .unwrap();
        assert!(outcome.ok);
    }

    #[test]
    fn time_elapsed_advances_virtual_clock() {
        let html = "<!doctype html><html><body></body></html>";
        let (sess, _) = JsSession::open(html, url()).unwrap();
        let outcome = sess
            .wait_for(
                &WaitCondition::TimeElapsed { duration_ms: 1_000 },
                Duration::from_millis(5_000),
            )
            .unwrap();
        assert!(outcome.ok);
        // The virtual clock should now read >= 1000 ms.
        let now = sess.eval("Date.now()").unwrap();
        let now_ms = now.value.as_f64().unwrap_or(0.0);
        assert!(now_ms >= 1_000.0, "Date.now() = {now_ms}, want >= 1000");
    }

    #[test]
    fn url_matches_uses_engine_base_url() {
        let html = "<!doctype html><html><body></body></html>";
        let (sess, _) = JsSession::open(html, url()).unwrap();
        let re = Regex::new("/$").unwrap();
        let outcome = sess
            .wait_for(
                &WaitCondition::UrlMatches(re),
                Duration::from_millis(500),
            )
            .unwrap();
        assert!(outcome.ok);
    }

    #[test]
    fn network_idle_returns_immediately_when_no_fetches_pending() {
        let html = "<!doctype html><html><body></body></html>";
        let (sess, _) = JsSession::open(html, url()).unwrap();
        let outcome = sess
            .wait_for(
                &WaitCondition::NetworkIdle { idle_window_ms: 50 },
                Duration::from_millis(2_000),
            )
            .unwrap();
        assert!(outcome.ok, "outcome: {:?}", outcome);
    }
}
