//! Smoke tests for the "trivial browser globals" cluster installed
//! by `install_browser_apis` (engine.rs) and the Document / Element
//! globals batch in dom.rs.
//!
//! Each test pins ONE property frameworks rely on. Failures here
//! mean a real-world page that uses the API will throw at runtime.
//!
//! The batch covers:
//! - `navigator` — userAgent / webdriver / language / platform
//! - `queueMicrotask(fn)`
//! - `requestAnimationFrame(cb)` / `cancelAnimationFrame(id)`
//! - `performance.now()` / `performance.timeOrigin`
//! - `atob(s)` / `btoa(s)`
//! - `matchMedia(query)`
//! - `localStorage` / `sessionStorage`
//! - `document.readyState` / `document.activeElement` /
//!   `document.cookie` / `document.contains(other)`
//! - `element.getBoundingClientRect()` / `getClientRects()` /
//!   `clientWidth` / `offsetParent` / `scrollTop` / `focus()` /
//!   `blur()` / `scrollIntoView()`

use heso_engine_js::{JsEngine, JsSession};
use url::Url;

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

fn u() -> Url {
    Url::parse("https://example.com/").unwrap()
}

// ===== navigator ======================================================

#[test]
fn navigator_user_agent_is_non_empty_and_browser_shaped() {
    let out = engine().eval("navigator.userAgent").expect("eval");
    let s = out.value.as_str().expect("userAgent is string");
    assert!(!s.is_empty(), "userAgent must not be empty");
    // Sniffers expect a "Mozilla/5.0" prefix.
    assert!(
        s.starts_with("Mozilla/5.0"),
        "userAgent should start with Mozilla/5.0; got {s:?}"
    );
    // We also identify ourselves so server operators see who's calling.
    assert!(s.contains("heso"), "userAgent should mention heso; got {s:?}");
}

#[test]
fn navigator_webdriver_is_false() {
    // anti-bot scripts gate on this; we genuinely aren't WebDriver.
    let out = engine().eval("navigator.webdriver").expect("eval");
    assert_eq!(out.value, serde_json::json!(false));
}

#[test]
fn navigator_language_and_languages() {
    let out = engine()
        .eval("[navigator.language, navigator.languages]")
        .expect("eval");
    assert_eq!(out.value[0], "en-US");
    assert_eq!(out.value[1], serde_json::json!(["en-US"]));
}

#[test]
fn navigator_on_line_and_cookie_enabled_are_true() {
    let out = engine()
        .eval("[navigator.onLine, navigator.cookieEnabled]")
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, true]));
}

#[test]
fn navigator_platform_is_non_empty_string() {
    let out = engine().eval("navigator.platform").expect("eval");
    let s = out.value.as_str().expect("platform is string");
    assert!(!s.is_empty(), "platform should not be empty");
}

// ===== queueMicrotask =================================================

#[test]
fn queue_microtask_runs_fn_before_eval_returns() {
    // Use a global counter so the promise-driven microtask's side
    // effect is observable after `eval` completes — JsEngine::eval
    // calls run_pending_jobs, which drives queued microtasks to
    // completion.
    let e = engine();
    let out = e
        .eval(
            r#"
            globalThis.qmFired = 0;
            queueMicrotask(() => { globalThis.qmFired += 1; });
            queueMicrotask(() => { globalThis.qmFired += 10; });
            globalThis.qmFired
            "#,
        )
        .expect("eval ok");
    // Synchronous return value should be 0 (microtasks haven't run
    // yet during expression evaluation, only after).
    assert_eq!(out.value, serde_json::json!(0));
    // After the microtask pump runs (which happens at the end of
    // `eval`), the next `eval` sees the side effect.
    let out2 = e.eval("globalThis.qmFired").expect("eval ok");
    assert_eq!(out2.value, serde_json::json!(11));
}

#[test]
fn queue_microtask_with_non_function_throws() {
    let err = engine()
        .eval("queueMicrotask(42)")
        .expect_err("non-function should throw");
    // Either Exception (TypeError) is fine.
    let msg = format!("{err:?}");
    assert!(
        msg.contains("TypeError") || msg.contains("not a function"),
        "expected TypeError, got: {msg}"
    );
}

// ===== requestAnimationFrame / cancelAnimationFrame ===================

#[test]
fn request_animation_frame_returns_a_number() {
    let out = engine()
        .eval("typeof requestAnimationFrame(() => {})")
        .expect("eval");
    assert_eq!(out.value, "number");
}

#[test]
fn request_animation_frame_fires_after_advance_clock_16ms() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            globalThis.rafFired = false;
            globalThis.rafTs = null;
            requestAnimationFrame((ts) => {
                globalThis.rafFired = true;
                globalThis.rafTs = ts;
            });
            "#,
        )
        .expect("schedule ok");
    // Before any clock advance, no fire.
    assert_eq!(e.pending_timers(), 1);
    // Advance past the 16ms rAF tick.
    e.advance_clock(16).expect("advance");
    let out = e.eval("globalThis.rafFired").expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
    // Callback should receive a high-res timestamp (performance.now())
    // which after the advance equals 16.
    let ts = e.eval("globalThis.rafTs").expect("eval");
    assert_eq!(ts.value.as_f64(), Some(16.0));
}

#[test]
fn cancel_animation_frame_prevents_fire() {
    let e = engine();
    let _ = e
        .eval(
            r#"
            globalThis.rafFired = false;
            const id = requestAnimationFrame(() => { globalThis.rafFired = true; });
            cancelAnimationFrame(id);
            "#,
        )
        .expect("schedule + cancel ok");
    assert_eq!(e.pending_timers(), 0);
    e.advance_clock(1000).expect("advance");
    let out = e.eval("globalThis.rafFired").expect("eval");
    assert_eq!(out.value, serde_json::json!(false));
}

// ===== performance.now / timeOrigin ===================================

#[test]
fn performance_now_returns_number_starting_at_zero() {
    let out = engine()
        .eval("[typeof performance.now(), performance.now()]")
        .expect("eval");
    assert_eq!(out.value[0], "number");
    assert_eq!(out.value[1], serde_json::json!(0));
}

#[test]
fn performance_now_advances_after_clock_advance() {
    let e = engine();
    let out = e.eval("performance.now()").expect("eval");
    assert_eq!(out.value, serde_json::json!(0));
    e.advance_clock(250).expect("advance");
    let out = e.eval("performance.now()").expect("eval");
    assert_eq!(out.value, serde_json::json!(250));
}

#[test]
fn performance_time_origin_is_zero() {
    let out = engine().eval("performance.timeOrigin").expect("eval");
    assert_eq!(out.value, serde_json::json!(0));
}

// ===== atob / btoa ====================================================

#[test]
fn btoa_encodes_ascii() {
    let out = engine().eval("btoa('hello')").expect("eval");
    assert_eq!(out.value, "aGVsbG8=");
}

#[test]
fn atob_decodes_base64() {
    let out = engine().eval("atob('aGVsbG8=')").expect("eval");
    assert_eq!(out.value, "hello");
}

#[test]
fn atob_btoa_round_trip() {
    let out = engine()
        .eval("atob(btoa('round trip 123 !@#'))")
        .expect("eval");
    assert_eq!(out.value, "round trip 123 !@#");
}

#[test]
fn atob_invalid_input_throws() {
    let err = engine()
        .eval("atob('!!! not base64 !!!')")
        .expect_err("invalid base64 should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("InvalidCharacterError") || msg.contains("base64"),
        "expected InvalidCharacterError-shaped throw, got: {msg}"
    );
}

#[test]
fn btoa_non_latin1_throws() {
    // Code point U+0100 is out of range for btoa's binary-string contract.
    let err = engine()
        .eval("btoa('\\u0100')")
        .expect_err("non-latin1 should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("InvalidCharacterError") || msg.contains("U+00FF"),
        "expected character-range error, got: {msg}"
    );
}

// ===== matchMedia =====================================================

#[test]
fn match_media_returns_no_match_with_listener_surface() {
    let out = engine()
        .eval(
            r#"
            const m = matchMedia('(min-width: 800px)');
            ({
                matches: m.matches,
                media: m.media,
                hasAddListener: typeof m.addListener,
                hasAddEventListener: typeof m.addEventListener,
                hasRemoveEventListener: typeof m.removeEventListener,
                hasDispatchEvent: typeof m.dispatchEvent,
                onchange: m.onchange
            })
            "#,
        )
        .expect("eval");
    assert_eq!(out.value["matches"], false);
    assert_eq!(out.value["media"], "(min-width: 800px)");
    assert_eq!(out.value["hasAddListener"], "function");
    assert_eq!(out.value["hasAddEventListener"], "function");
    assert_eq!(out.value["hasRemoveEventListener"], "function");
    assert_eq!(out.value["hasDispatchEvent"], "function");
    assert!(out.value["onchange"].is_null());
}

#[test]
fn match_media_listener_methods_do_not_throw() {
    let out = engine()
        .eval(
            r#"
            const m = matchMedia('(prefers-color-scheme: dark)');
            m.addListener(() => {});
            m.addEventListener('change', () => {});
            m.removeListener(() => {});
            m.removeEventListener('change', () => {});
            m.dispatchEvent({ type: 'change' });
            'ok'
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}

// ===== localStorage / sessionStorage ==================================

#[test]
fn local_storage_set_get_round_trip() {
    let out = engine()
        .eval(
            r#"
            localStorage.setItem('k', 'v');
            localStorage.getItem('k')
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "v");
}

#[test]
fn local_storage_survives_across_session_eval_calls() {
    let html = "<!doctype html><html><body></body></html>";
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.eval("localStorage.setItem('persistent', 'yes');")
        .unwrap();
    let out = sess
        .eval("localStorage.getItem('persistent')")
        .unwrap();
    assert_eq!(out.value, "yes");
}

#[test]
fn local_storage_get_missing_key_returns_null() {
    let out = engine()
        .eval("localStorage.getItem('nope')")
        .expect("eval");
    assert!(out.value.is_null());
}

#[test]
fn local_storage_remove_and_clear_and_length_and_key() {
    let out = engine()
        .eval(
            r#"
            localStorage.setItem('a', '1');
            localStorage.setItem('b', '2');
            localStorage.setItem('c', '3');
            const len1 = localStorage.length;
            localStorage.removeItem('b');
            const len2 = localStorage.length;
            const k0 = localStorage.key(0);
            localStorage.clear();
            const len3 = localStorage.length;
            [len1, len2, k0, len3]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], 3);
    assert_eq!(out.value[1], 2);
    assert_eq!(out.value[2], "a");
    assert_eq!(out.value[3], 0);
}

#[test]
fn local_storage_coerces_keys_and_values_to_strings() {
    let out = engine()
        .eval(
            r#"
            localStorage.setItem(42, true);
            [
                localStorage.getItem('42'),
                localStorage.getItem(42),
                typeof localStorage.getItem('42')
            ]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "true");
    assert_eq!(out.value[1], "true");
    assert_eq!(out.value[2], "string");
}

#[test]
fn session_storage_is_independent_of_local_storage() {
    let out = engine()
        .eval(
            r#"
            localStorage.setItem('shared', 'L');
            sessionStorage.setItem('shared', 'S');
            [localStorage.getItem('shared'), sessionStorage.getItem('shared')]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["L", "S"]));
}

#[test]
fn session_storage_key_method_returns_null_out_of_range() {
    let out = engine()
        .eval(
            r#"
            sessionStorage.setItem('only', 'x');
            [sessionStorage.key(0), sessionStorage.key(5), sessionStorage.key(-1)]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "only");
    assert!(out.value[1].is_null());
    assert!(out.value[2].is_null());
}

// ===== document.readyState / activeElement / cookie / contains ========

#[test]
fn document_ready_state_is_complete() {
    let html = "<html><body><p>x</p></body></html>";
    let out = engine()
        .eval_with_html(html, "document.readyState")
        .expect("eval");
    assert_eq!(out.value, "complete");
}

#[test]
fn document_active_element_is_body() {
    // activeElement should point at the body. Identity equality
    // against `document.body` doesn't hold because each getter call
    // builds a fresh Element wrapper around the same NodeId — that
    // matches `document.querySelector('body') === document.body`
    // being false too. Verify via tagName + parent identity instead.
    let html = "<html><body><p>x</p></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            r#"[
                document.activeElement.tagName,
                document.activeElement.parentElement.tagName
            ]"#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "BODY");
    assert_eq!(out.value[1], "HTML");
}

#[test]
fn document_cookie_getter_is_empty_string() {
    let html = "<html><body></body></html>";
    let out = engine()
        .eval_with_html(html, "document.cookie")
        .expect("eval");
    assert_eq!(out.value, "");
}

#[test]
fn document_cookie_setter_does_not_throw() {
    let html = "<html><body></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            "document.cookie = 'session=abc; Path=/'; document.cookie",
        )
        .expect("eval");
    // Still empty after the no-op setter.
    assert_eq!(out.value, "");
}

#[test]
fn document_contains_descendant_returns_true() {
    let html = r#"<html><body><div id="d"><span id="s">x</span></div></body></html>"#;
    let out = engine()
        .eval_with_html(
            html,
            r#"[
                document.contains(document.body),
                document.contains(document.getElementById('d')),
                document.contains(document.getElementById('s'))
            ]"#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, true, true]));
}

#[test]
fn document_contains_detached_element_returns_false() {
    let html = "<html><body></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            r#"
            const detached = document.createElement('div');
            document.contains(detached)
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(false));
}

// ===== Element layout-zero stubs ======================================

#[test]
fn get_bounding_client_rect_returns_zero_rect() {
    let html = "<html><body><div id='d'>x</div></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            r#"
            const r = document.getElementById('d').getBoundingClientRect();
            ({
                x: r.x, y: r.y, width: r.width, height: r.height,
                top: r.top, right: r.right, bottom: r.bottom, left: r.left
            })
            "#,
        )
        .expect("eval");
    for key in ["x", "y", "width", "height", "top", "right", "bottom", "left"] {
        assert_eq!(out.value[key], 0, "expected 0 for {key}; got: {:?}", out.value[key]);
    }
}

#[test]
fn get_bounding_client_rect_to_json_returns_self() {
    let html = "<html><body><div id='d'></div></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            r#"
            const r = document.getElementById('d').getBoundingClientRect();
            const j = r.toJSON();
            // toJSON returns the same object, so the spread should match.
            [j.x, j.y, j.width, j.height]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([0, 0, 0, 0]));
}

#[test]
fn get_client_rects_returns_empty_array() {
    let html = "<html><body><div id='d'></div></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            r#"
            const list = document.getElementById('d').getClientRects();
            [Array.isArray(list), list.length]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, 0]));
}

#[test]
fn element_layout_dimensions_all_zero() {
    let html = "<html><body><div id='d'>x</div></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            r#"
            const e = document.getElementById('d');
            [
                e.clientWidth, e.clientHeight,
                e.offsetWidth, e.offsetHeight,
                e.offsetTop, e.offsetLeft,
                e.scrollWidth, e.scrollHeight,
                e.scrollTop, e.scrollLeft
            ]
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!([0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    );
}

#[test]
fn element_offset_parent_is_null() {
    let html = "<html><body><div id='d'></div></body></html>";
    let out = engine()
        .eval_with_html(html, "document.getElementById('d').offsetParent")
        .expect("eval");
    assert!(out.value.is_null());
}

#[test]
fn element_scroll_top_setter_is_no_op() {
    let html = "<html><body><div id='d'></div></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            r#"
            const e = document.getElementById('d');
            e.scrollTop = 100;
            e.scrollLeft = 50;
            [e.scrollTop, e.scrollLeft]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([0, 0]));
}

#[test]
fn element_focus_blur_scroll_into_view_do_not_throw() {
    let html = "<html><body><input id='i' /></body></html>";
    let out = engine()
        .eval_with_html(
            html,
            r#"
            const e = document.getElementById('i');
            e.focus();
            e.focus({preventScroll: true});
            e.blur();
            e.scrollIntoView();
            e.scrollIntoView(true);
            e.scrollIntoView({behavior: 'smooth', block: 'start'});
            'ok'
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}

// ===== heso.flush() + Promise-aware eval ==============================
//
// User JS that returns a thenable should be awaited via the microtask
// pump and serialized as the resolved value, not as a Promise-shaped
// object. This is what lets `await heso.flush()` patterns observe DOM
// mutations queued by an earlier `dispatchEvent` (e.g. a Preact
// re-render scheduled via `Promise.resolve().then(...)`).

#[test]
fn eval_resolves_top_level_promise_to_its_value() {
    // A bare `Promise.resolve(42)` at top-level should serialize as
    // 42, not as `{}` or a Promise-shaped object.
    let out = engine().eval("Promise.resolve(42)").expect("eval");
    assert_eq!(out.value, serde_json::json!(42));
}

#[test]
fn eval_resolves_async_iife_to_its_returned_value() {
    // The canonical "top-level await" pattern: wrap user code in an
    // async IIFE.  `await heso.flush()` yields to the microtask
    // queue, then the IIFE returns a value.  The Rust eval should
    // serialize that value, not the wrapper Promise.
    let out = engine()
        .eval("(async () => { await heso.flush(); return 'hello'; })()")
        .expect("eval");
    assert_eq!(out.value, serde_json::json!("hello"));
}

#[test]
fn heso_flush_lets_earlier_microtasks_run_before_resume() {
    // Spec-shaped: `Promise.resolve().then(setter)` queues a
    // microtask BEFORE `await heso.flush()`'s continuation, so by the
    // time the await resumes, the setter has already run.  This is
    // the exact pattern Preact uses to schedule re-renders.
    let out = engine()
        .eval(
            r#"(async () => {
                globalThis.__seen = 'before';
                Promise.resolve().then(() => { globalThis.__seen = 'after'; });
                await heso.flush();
                return globalThis.__seen;
            })()"#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!("after"));
}

#[test]
fn eval_propagates_promise_rejection_as_thrown_value() {
    // A rejected promise at top-level should surface as
    // `EvalError::ThrownValue`, not as a silently-null result.
    let err = engine()
        .eval("Promise.reject('boom')")
        .expect_err("expected rejection to propagate");
    match err {
        heso_engine_js::EvalError::ThrownValue { value } => {
            assert_eq!(value, serde_json::json!("boom"));
        }
        other => panic!("expected ThrownValue; got {other:?}"),
    }
}

#[test]
fn heso_flush_is_one_microtask_checkpoint_per_call() {
    // Spec-shaped contract: each `await heso.flush()` is exactly one
    // microtask checkpoint.  If a microtask handler queues another
    // microtask, that second one runs in the NEXT checkpoint, not
    // the current one — same as in real browsers and Node.
    //
    // The pump loop in `execute_pending_jobs_until_idle` does run
    // chained microtasks during a single pump call, but the await's
    // continuation is queued BEFORE the inner .then, so the
    // continuation observes only the outer tick (FIFO ordering of
    // microtasks queued at the same checkpoint).
    let out = engine()
        .eval(
            r#"(async () => {
                globalThis.__ticks = 0;
                Promise.resolve().then(() => {
                    globalThis.__ticks += 1;
                    Promise.resolve().then(() => { globalThis.__ticks += 1; });
                });
                await heso.flush();
                const after_one = globalThis.__ticks;
                await heso.flush();
                const after_two = globalThis.__ticks;
                return [after_one, after_two];
            })()"#,
        )
        .expect("eval");
    // Two flushes => two checkpoints => both ticks visible.
    assert_eq!(out.value, serde_json::json!([1, 2]));
}

#[test]
fn preact_shaped_dispatch_then_render_is_visible_after_flush() {
    // The end-to-end scenario this whole change exists for:
    // - DOM listener fires on dispatchEvent
    // - Listener queues a "re-render" as a microtask (Preact pattern)
    // - User code awaits a microtask checkpoint, then reads the DOM
    // - The reads see the rendered mutation
    //
    // This pins the contract without needing the full Preact bundle.
    let html = r#"<!doctype html><html><body>
        <input id="i" />
        <ul id="list"></ul>
        <script>
            // No `e.key` check because KeyboardEvent ctor isn't wired
            // yet; the plain Event('keydown') we dispatch from the
            // test is enough to drive the same code shape.
            document.getElementById('i').addEventListener('keydown', (e) => {
                const val = e.target.value;
                e.target.value = '';
                // "Preact-style" deferred render via microtask.
                Promise.resolve().then(() => {
                    const li = document.createElement('li');
                    li.textContent = val;
                    document.getElementById('list').appendChild(li);
                });
            });
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let out = sess
        .eval(
            r#"(async () => {
                const i = document.getElementById('i');
                i.value = 'buy milk';
                i.dispatchEvent(new Event('keydown'));
                await heso.flush();
                return document.getElementById('list').innerHTML;
            })()"#,
        )
        .expect("eval");
    // Without microtask drain this would be empty.  With `await
    // heso.flush()` (implicit via the awaited top-level promise),
    // the <li> is rendered before the read.
    let html_str = out.value.as_str().expect("innerHTML is string");
    // Trim is paranoia — engine may add no whitespace, but if it
    // ever adds e.g. a trailing newline the asserts shouldn't fail.
    let trimmed = html_str.trim();
    assert!(
        trimmed.contains("buy milk"),
        "expected list to contain rendered item; got innerHTML = {trimmed:?}"
    );
    assert!(
        trimmed.starts_with("<li") && trimmed.ends_with("</li>"),
        "expected single <li>; got {trimmed:?}"
    );
}

#[test]
fn dispatch_without_flush_still_sees_synchronous_handler_effects() {
    // Regression guard: synchronous handler effects (e.g. setting
    // a property in place) must still be visible without any await,
    // because we don't want the Promise-await path to change sync
    // semantics for callers who didn't opt in.
    let html = r#"<!doctype html><html><body>
        <input id="i" />
        <script>
            document.getElementById('i').addEventListener('keydown', (e) => {
                e.target.value = '';
            });
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.eval(
        r#"
        const i = document.getElementById('i');
        i.value = 'x';
        i.dispatchEvent(new Event('keydown'));
        "#,
    )
    .expect("eval");
    let out = sess
        .eval("document.getElementById('i').value")
        .expect("eval value");
    assert_eq!(out.value, serde_json::json!(""));
}
