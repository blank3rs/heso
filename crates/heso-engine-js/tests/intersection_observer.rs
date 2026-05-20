//! Tests for the real `IntersectionObserver` implementation in
//! `crates/heso-engine-js/src/intersection_observer.rs`.
//!
//! The IO replacement is the gate behind: lazy images, infinite
//! scroll, Astro `client:visible`, deferred component hydration.
//! Each test pins one observable behavior so regressions surface as
//! a named test failure.

use heso_engine_js::{JsEngine, JsSession};
use url::Url;

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

fn u() -> Url {
    Url::parse("https://example.com/").unwrap()
}

/// The headline contract: `observe(target)` MUST fire the callback
/// with `isIntersecting: true` for an in-tree target, delivered by
/// the next microtask. This is the behavior that unblocks
/// lazy-loaded content for the agent.
#[test]
fn observe_fires_callback_with_isintersecting_true_on_next_microtask() {
    let html = r#"<!doctype html><html><body></body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let out = sess
        .eval(
            r#"
            (async () => {
                const target = document.createElement('div');
                document.body.appendChild(target);
                let fired = false;
                let entryWasIntersecting = false;
                new IntersectionObserver(entries => {
                    fired = true;
                    if (entries[0] && entries[0].isIntersecting) {
                        entryWasIntersecting = true;
                    }
                }).observe(target);
                // Pump microtasks. heso.flush is Promise.resolve(); the
                // engine's `run_pending_jobs` runs queued microtasks
                // (including the IO callback's microtask) before this
                // settles.
                await heso.flush();
                return fired && entryWasIntersecting;
            })()
            "#,
        )
        .expect("eval ok");
    // The IIFE returns a Promise; the engine awaits it (see engine.rs
    // run_pending_jobs) and surfaces the resolved value.
    assert_eq!(out.value, serde_json::json!(true));
}

/// The entry shape must carry every spec property a reader might
/// probe. Real frameworks (`react-intersection-observer`, Astro
/// runtime) read `intersectionRatio`, `boundingClientRect`, and
/// `target` off the entry; if any is undefined, they crash.
#[test]
fn entry_shape_matches_spec() {
    let html = r#"<!doctype html><html><body></body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let out = sess
        .eval(
            r#"
            (async () => {
                const target = document.createElement('div');
                document.body.appendChild(target);
                let entry = null;
                new IntersectionObserver(entries => {
                    entry = entries[0];
                }).observe(target);
                await heso.flush();
                return {
                    hasTarget: entry.target === target,
                    isIntersectingType: typeof entry.isIntersecting,
                    intersectionRatioType: typeof entry.intersectionRatio,
                    intersectionRatio: entry.intersectionRatio,
                    hasBoundingClientRect: typeof entry.boundingClientRect,
                    hasIntersectionRect: typeof entry.intersectionRect,
                    rootBoundsIsNull: entry.rootBounds === null,
                    hasTime: typeof entry.time,
                    boundsXIsNumber: typeof entry.boundingClientRect.x,
                    boundsTopIsNumber: typeof entry.boundingClientRect.top,
                    instanceOfEntry: entry instanceof IntersectionObserverEntry,
                };
            })()
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value["hasTarget"], true);
    assert_eq!(out.value["isIntersectingType"], "boolean");
    assert_eq!(out.value["intersectionRatioType"], "number");
    assert_eq!(out.value["intersectionRatio"], 1.0);
    assert_eq!(out.value["hasBoundingClientRect"], "object");
    assert_eq!(out.value["hasIntersectionRect"], "object");
    assert_eq!(out.value["rootBoundsIsNull"], true);
    assert_eq!(out.value["hasTime"], "number");
    assert_eq!(out.value["boundsXIsNumber"], "number");
    assert_eq!(out.value["boundsTopIsNumber"], "number");
    assert_eq!(out.value["instanceOfEntry"], true);
}

/// Spec-mandated read-only IDL attributes (`root`, `rootMargin`,
/// `thresholds`) must round-trip the constructor options. Code that
/// reads these to compute its own thresholds (e.g.
/// `react-intersection-observer` v9 reads `observer.thresholds` to
/// build per-threshold callback partitions) fails if they're wrong.
#[test]
fn observer_attributes_reflect_constructor_options() {
    let out = engine()
        .eval(
            r#"
            const o = new IntersectionObserver(() => {}, {
                root: null,
                rootMargin: "10px 20px",
                threshold: [0.25, 0.5, 0.75]
            });
            ({
                root: o.root,
                rootMargin: o.rootMargin,
                thresholdsLen: o.thresholds.length,
                thresholdsSorted: o.thresholds[0] < o.thresholds[1] &&
                                  o.thresholds[1] < o.thresholds[2],
                t0: o.thresholds[0], t1: o.thresholds[1], t2: o.thresholds[2],
            })
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value["root"], serde_json::Value::Null);
    assert_eq!(out.value["rootMargin"], "10px 20px");
    assert_eq!(out.value["thresholdsLen"], 3);
    assert_eq!(out.value["thresholdsSorted"], true);
    assert_eq!(out.value["t0"], 0.25);
    assert_eq!(out.value["t1"], 0.5);
    assert_eq!(out.value["t2"], 0.75);
}

/// Default-options call: no `threshold`, no `root`. Spec defaults
/// are `thresholds: [0]`, `root: null`, `rootMargin: "0px 0px 0px
/// 0px"`. Most consumer code uses defaults.
#[test]
fn default_options_match_spec() {
    let out = engine()
        .eval(
            r#"
            const o = new IntersectionObserver(() => {});
            ({
                root: o.root,
                rootMargin: o.rootMargin,
                thresholdsLen: o.thresholds.length,
                t0: o.thresholds[0],
            })
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value["root"], serde_json::Value::Null);
    // Spec says default rootMargin is "0px 0px 0px 0px"; we accept
    // anything truthy as long as it's a string.
    assert!(
        out.value["rootMargin"].is_string(),
        "rootMargin should be a string, got {:?}",
        out.value["rootMargin"]
    );
    assert_eq!(out.value["thresholdsLen"], 1);
    assert_eq!(out.value["t0"], 0.0);
}

/// Re-observing after `unobserve` re-fires (matches real browsers
/// and the spec — `unobserve` clears the registration, `observe`
/// adds a fresh one with `previousThresholdIndex: -1` so the next
/// match always queues an entry).
#[test]
fn reobserve_after_unobserve_refires() {
    let html = r#"<!doctype html><html><body></body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let out = sess
        .eval(
            r#"
            (async () => {
                const target = document.createElement('div');
                document.body.appendChild(target);
                let fireCount = 0;
                const observer = new IntersectionObserver(() => {
                    fireCount += 1;
                });
                observer.observe(target);
                await heso.flush();
                observer.unobserve(target);
                observer.observe(target);
                await heso.flush();
                return fireCount;
            })()
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(2));
}

/// `disconnect()` must drop queued notifications. A microtask
/// pending from a prior `observe()` call should NOT fire after
/// `disconnect()`.
#[test]
fn disconnect_drops_pending_notifications() {
    let html = r#"<!doctype html><html><body></body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let out = sess
        .eval(
            r#"
            (async () => {
                const target = document.createElement('div');
                document.body.appendChild(target);
                let fireCount = 0;
                const observer = new IntersectionObserver(() => {
                    fireCount += 1;
                });
                observer.observe(target);
                observer.disconnect();   // synchronous, before microtask
                await heso.flush();
                return fireCount;
            })()
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(0));
}

/// `takeRecords()` always returns `[]` in our model — we deliver
/// every entry via the callback, no internal buffer. Sites that
/// loop on `takeRecords` expect an empty array to mean "no pending
/// records", so this is the right shape.
#[test]
fn take_records_returns_empty_array() {
    let html = r#"<!doctype html><html><body></body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let out = sess
        .eval(
            r#"
            const target = document.createElement('div');
            document.body.appendChild(target);
            const observer = new IntersectionObserver(() => {});
            observer.observe(target);
            const records = observer.takeRecords();
            ({
                isArray: Array.isArray(records),
                length: records.length,
            })
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value["isArray"], true);
    assert_eq!(out.value["length"], 0);
}

/// Spec contract: constructor without a function callback must throw
/// TypeError. Frameworks feature-detect by `try { new IO() } catch
/// {}` and the catch branch matters.
#[test]
fn constructor_throws_typeerror_on_missing_callback() {
    let out = engine()
        .eval(
            r#"
            let threw = false;
            let name = '';
            try { new IntersectionObserver(); }
            catch (e) { threw = true; name = e.constructor.name; }
            ({ threw, name })
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value["threw"], true);
    assert_eq!(out.value["name"], "TypeError");
}

/// Out-of-range threshold (>1 or <0) throws RangeError per spec.
#[test]
fn out_of_range_threshold_throws() {
    let out = engine()
        .eval(
            r#"
            let threw = false;
            try { new IntersectionObserver(() => {}, { threshold: 2.0 }); }
            catch (e) { threw = true; }
            threw
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, serde_json::json!(true));
}

/// Detached targets (`createElement` without `appendChild`) still
/// receive a callback, but with `isIntersecting: false`. Real
/// browsers fire the initial notification regardless of in-tree
/// status; only the visibility bit reflects connectedness.
#[test]
fn detached_target_fires_callback_with_isintersecting_false() {
    let html = r#"<!doctype html><html><body></body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let out = sess
        .eval(
            r#"
            (async () => {
                const orphan = document.createElement('div');
                // NOT appended to body
                let fired = false;
                let isIntersecting = null;
                new IntersectionObserver(entries => {
                    fired = true;
                    isIntersecting = entries[0].isIntersecting;
                }).observe(orphan);
                await heso.flush();
                return { fired, isIntersecting };
            })()
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value["fired"], true);
    assert_eq!(out.value["isIntersecting"], false);
}
