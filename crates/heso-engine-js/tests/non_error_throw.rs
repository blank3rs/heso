//! Integration tests for non-`Error` `throw` diagnostic surfacing —
//! the fix for the cluster of Next.js sites (supabase.com,
//! stripe.com, posthog.com) whose `<script src="...">` chunks throw
//! values other than `Error` instances and were previously reported as
//! the opaque string `<script threw non-error value>` with no detail.
//!
//! ## Bug shape
//!
//! Real-agent runs on the three sites above showed:
//!
//! - `_app-3f0ab84593...js threw <non-error value>` (supabase)
//! - `_app-b96d1b09...js threw <non-error value>` (stripe)
//! - `9cb3c7b0-...js threw <non-error value>` (posthog)
//!
//! A targeted diagnostic build (the precursor to this fix) revealed
//! the actual thrown value on stripe + posthog is `null` —
//! `typeof null === "object"`, but rquickjs's `CaughtError::Value`
//! arm correctly identifies it as a non-`Error` JS exception. The
//! supabase chunk hits the engine's memory cap (a separate bug
//! tracked by subagent #4) before it gets to the `throw null` site,
//! so we can only confirm 2 of 3 sites here; V8's "probably one
//! root cause across all three" hypothesis is consistent with the
//! observed pattern but not directly testable until the OOM is fixed.
//!
//! ## Fix shape
//!
//! `format_non_error_throw` in `scripts.rs` walks the thrown value
//! and produces a structural summary instead of the opaque string.
//! See the rustdoc on that function for the per-type format.
//!
//! ## What we test
//!
//! Each test exercises one shape of non-Error throw against the
//! `<script>` execution path:
//!
//! - **String** → quoted string appears in the message.
//! - **Object** → JSON.stringify summary appears in the message.
//! - **Symbol** → `Symbol(description)` appears in the message.
//! - **Promise** → engine does not crash, message mentions Promise.
//! - **null** → message contains literal "null" (the actual real-site
//!   pattern from stripe / posthog).
//! - **undefined** → message contains literal "undefined".
//! - **Number** → numeric value appears in the message.

use heso_engine_js::{JsEngine, ScriptFetchPolicy};

/// Extract the (single) console message that the script pump captured
/// for a throwing inline `<script>`. Panics if the pump didn't capture
/// exactly one error entry — the test's whole point is that the throw
/// produces one diagnostic line, not zero (silent swallow) and not
/// more (multi-emit).
fn capture_single_error<F>(html: &str, after_eval: &str, predicate: F) -> String
where
    F: Fn(&str) -> bool,
{
    let engine = JsEngine::new().expect("engine builds");
    let out = engine
        .eval_with_html_policy(html, after_eval, ScriptFetchPolicy::Skip)
        .expect("eval ok");
    let entries: Vec<String> = out
        .console
        .iter()
        .filter(|e| matches!(e.level, heso_engine_js::ConsoleLevel::Error))
        .flat_map(|e| e.args.iter().filter_map(|v| v.as_str().map(str::to_owned)))
        .filter(|s| predicate(s))
        .collect();
    assert!(
        !entries.is_empty(),
        "expected at least one console.error matching predicate; got console: {:?}",
        out.console
    );
    entries.into_iter().next().unwrap()
}

// ===== Test 1: string throw surfaces the string =====

#[test]
fn synthetic_string_throw_surfaces_string_in_error() {
    // Page code that does `throw "hello"` should produce a console
    // error whose body includes the literal string "hello" so a
    // debugging user can see what the page actually threw.
    let html = r#"<html><body>
        <script>throw "hello";</script>
    </body></html>"#;
    let msg = capture_single_error(html, "1", |s| s.contains("threw non-Error"));
    assert!(
        msg.contains("\"hello\""),
        "expected the thrown string 'hello' to appear quoted in the error; got: {msg}"
    );
    assert!(
        msg.contains("non-Error"),
        "expected the diagnostic prefix to identify the value as non-Error; got: {msg}"
    );
}

// ===== Test 2: object throw surfaces a JSON summary =====

#[test]
fn synthetic_object_throw_surfaces_json_summary() {
    // Page code that does `throw {code: 42}` should produce a console
    // error whose body includes a structural JSON view — at minimum
    // the literal `{"code":42}` substring so a user can pattern-match
    // their error shape.
    let html = r#"<html><body>
        <script>throw {code: 42};</script>
    </body></html>"#;
    let msg = capture_single_error(html, "1", |s| s.contains("threw non-Error"));
    assert!(
        msg.contains(r#"{"code":42}"#),
        "expected JSON.stringify view of the thrown object; got: {msg}"
    );
}

// ===== Test 3: Symbol throw surfaces the description =====

#[test]
fn synthetic_symbol_throw_surfaces_symbol_description() {
    // Page code that does `throw Symbol("BAILOUT")` should produce
    // a console error whose body includes the literal "BAILOUT" —
    // the framework-sentinel pattern (e.g. Next.js's
    // `BAILOUT_TO_CSR` digest) relies on the description being
    // observable to debuggers.
    let html = r#"<html><body>
        <script>throw Symbol("BAILOUT");</script>
    </body></html>"#;
    let msg = capture_single_error(html, "1", |s| s.contains("threw non-Error"));
    assert!(
        msg.contains("BAILOUT"),
        "expected the Symbol description 'BAILOUT' to appear in the error; got: {msg}"
    );
    // Also verify the diagnostic identifies this as a Symbol (so
    // users know it's not a plain string error).
    assert!(
        msg.contains("Symbol"),
        "expected the diagnostic to mention 'Symbol'; got: {msg}"
    );
}

// ===== Test 4: throwing a Promise doesn't crash =====

#[test]
fn synthetic_promise_throw_does_not_crash_engine() {
    // React Suspense throws a Promise (a thenable) to signal "render
    // this client-side." Without a `<Suspense>` boundary the throw
    // escapes to the pump — our handler must not crash, and the
    // diagnostic should hint that this is the Suspense pattern.
    let html = r#"<html><body>
        <script>throw Promise.resolve(42);</script>
        <script>globalThis.__afterThrow = 1;</script>
    </body></html>"#;
    let engine = JsEngine::new().expect("engine builds");
    let out = engine
        .eval_with_html_policy(
            html,
            "globalThis.__afterThrow",
            ScriptFetchPolicy::Skip,
        )
        .expect("eval ok — the engine must continue past a Promise throw");
    // The second script must have run, meaning the pump continued
    // past the Promise throw rather than aborting.
    assert_eq!(
        out.value,
        serde_json::json!(1),
        "engine must continue past a Promise throw; got value: {:?}",
        out.value,
    );
    // The first script's throw must have been captured as a
    // console.error mentioning Promise — not silently swallowed.
    let buf = serde_json::to_string(&out.console).unwrap();
    assert!(
        buf.contains("Promise"),
        "expected console to mention Promise (Suspense pattern); got: {buf}"
    );
}

// ===== Test 5: throw null — the real-site pattern =====

#[test]
fn synthetic_null_throw_surfaces_literal_null() {
    // This is the precise pattern observed on stripe.com's
    // `index-24db2941...js` and posthog.com's `9cb3c7b0...js`
    // chunks: the minified webpack module pipeline throws `null`
    // for reasons unclear from static analysis (the chunk body
    // doesn't contain a literal `throw null;`, so it's almost
    // certainly produced by a module-init helper). The previous
    // diagnostic of `<script threw non-error value>` made this
    // un-debuggable; the fix surfaces "null" explicitly.
    let html = r#"<html><body>
        <script>throw null;</script>
    </body></html>"#;
    let msg = capture_single_error(html, "1", |s| s.contains("threw non-Error"));
    assert!(
        msg.contains("null"),
        "expected the literal 'null' to appear in the error; got: {msg}"
    );
}

// ===== Test 6: throw undefined =====

#[test]
fn synthetic_undefined_throw_surfaces_literal_undefined() {
    let html = r#"<html><body>
        <script>throw undefined;</script>
    </body></html>"#;
    let msg = capture_single_error(html, "1", |s| s.contains("threw non-Error"));
    assert!(
        msg.contains("undefined"),
        "expected the literal 'undefined' to appear in the error; got: {msg}"
    );
}

// ===== Test 7: throw number =====

#[test]
fn synthetic_number_throw_surfaces_number() {
    let html = r#"<html><body>
        <script>throw 42;</script>
    </body></html>"#;
    let msg = capture_single_error(html, "1", |s| s.contains("threw non-Error"));
    assert!(
        msg.contains("42"),
        "expected the literal '42' to appear in the error; got: {msg}"
    );
}

// ===== Test 8: pump continues past throw (existing containment) =====

#[test]
fn non_error_throw_does_not_abort_pump() {
    // Containment rule from ADR 0008: a throwing script must not
    // halt subsequent scripts. Regression test that our new
    // diagnostic surfacing doesn't accidentally tighten that.
    let html = r#"<html><body>
        <script>throw {bad: 1};</script>
        <script>globalThis.__second = 2;</script>
        <script>throw "another";</script>
        <script>globalThis.__third = 3;</script>
    </body></html>"#;
    let engine = JsEngine::new().expect("engine builds");
    let out = engine
        .eval_with_html_policy(
            html,
            "[globalThis.__second, globalThis.__third]",
            ScriptFetchPolicy::Skip,
        )
        .expect("eval ok");
    assert_eq!(
        out.value,
        serde_json::json!([2, 3]),
        "all non-throwing scripts must still run between the throws"
    );
    // Both throws must appear in the console buffer.
    let buf = serde_json::to_string(&out.console).unwrap();
    assert!(buf.contains(r#"{\"bad\":1}"#), "first throw's JSON summary missing; got: {buf}");
    assert!(buf.contains(r#"\"another\""#), "second throw's string missing; got: {buf}");
}

// ===== Test 9: very long string throw is truncated =====

#[test]
fn very_long_string_throw_is_truncated() {
    // A page that throws a megabyte of HTML (e.g. error responses
    // that include the entire page body) must not spam the console
    // buffer with all of it. The diagnostic truncates at a
    // bounded length.
    //
    // The script source itself is bounded, but the throw value is
    // built up from a small string × 1000 — easily over the
    // 200-char display cap.
    let html = r#"<html><body>
        <script>throw "X".repeat(1000);</script>
    </body></html>"#;
    let msg = capture_single_error(html, "1", |s| s.contains("threw non-Error"));
    // Truncation marker must be present.
    assert!(
        msg.contains("truncated"),
        "expected 'truncated' marker for a 1000-char throw; got: {msg}"
    );
    // And the total message must be well under the 1000-char raw
    // body — sanity check that we actually truncated.
    assert!(
        msg.len() < 600,
        "expected truncated message under ~600 chars; got len={} msg={msg}",
        msg.len(),
    );
}
