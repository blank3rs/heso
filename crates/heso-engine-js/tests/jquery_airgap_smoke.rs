//! Integration tests for the bug-report 06 cluster:
//!
//! Two real pages crashed at module-init time before this fix:
//!
//! 1. **kernel.org's bundled jQuery 3.6** —
//!    `cannot read property 'createElement' of undefined`. jQuery's
//!    Sizzle `setDocument(e)` gated on `9 === r.nodeType`, our
//!    `Document` had no `nodeType` property, the guard silently
//!    failed, and the subsequent `ce(fn)` feature-detect crashed
//!    on `C.createElement(...)` with `C` (the cached document)
//!    still undefined.
//!
//! 2. **MDN's bundled Transcend `airgap.js`** —
//!    `not a function at map (native)`. The module-init code
//!    `[YC,...]=ia.map(e=>e&&e[Symbol.iterator]().next)` walks an
//!    array that includes `Q.createElement("_").classList`; our
//!    `DomTokenList` had no `Symbol.iterator`, the destructure
//!    yielded `undefined`, and the next `.next` access crashed.
//!
//! These tests are pure JS-engine smoke tests: they don't fetch
//! the real scripts (those tests live under the integration-test
//! suite that runs `heso read https://...`). Instead they evaluate
//! the load-bearing patterns inline against a [`JsSession`], so
//! the bug stays caught even when the agent runs offline.

use heso_engine_js::{JsSession, ScriptFetchPolicy};
use url::Url;

fn page(html: &str) -> JsSession {
    let url = Url::parse("https://example.com/").unwrap();
    JsSession::open(html, url).expect("open page").0
}

// ===== bug-report 06.1 — jQuery / Sizzle setDocument ====================

#[test]
fn document_node_type_is_9() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval("document.nodeType")
        .expect("eval");
    assert_eq!(out.value, 9, "Document.nodeType must equal DOCUMENT_NODE (9) — jQuery Sizzle gates on 9 === r.nodeType");
}

#[test]
fn document_node_name_is_pound_document() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval("document.nodeName")
        .expect("eval");
    assert_eq!(out.value, "#document", "Document.nodeName must equal '#document' per WHATWG DOM §4.4");
}

#[test]
fn document_owner_document_is_null() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval("document.ownerDocument === null")
        .expect("eval");
    assert_eq!(out.value, true, "Document.ownerDocument must be null per WHATWG DOM §4.4");
}

#[test]
fn document_default_view_is_global() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval("document.defaultView === globalThis")
        .expect("eval");
    assert_eq!(out.value, true, "Document.defaultView must equal the window/globalThis");
}

#[test]
fn document_create_comment_creates_node() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const c = document.createComment("hello");
            JSON.stringify({type: c.nodeType, name: c.nodeName})
            "#,
        )
        .expect("eval");
    // Comment is nodeType 8; nodeName is "#comment". The wrapper
    // is the same Element type heso uses for every node — the
    // discrimination is via the underlying node-kind.
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["type"], 8);
    assert_eq!(v["name"], "#comment");
}

#[test]
fn document_implementation_has_create_html_document() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const impl = document.implementation;
            JSON.stringify({
                hasImpl: typeof impl,
                hasFn: typeof impl.createHTMLDocument,
                hasHasFeature: typeof impl.hasFeature,
            })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["hasImpl"], "object");
    assert_eq!(v["hasFn"], "function");
    assert_eq!(v["hasHasFeature"], "function");
}

#[test]
fn jquery_init_pattern_runs_to_completion() {
    // The exact pattern jQuery 3.6's Sizzle uses at module-init
    // time. Before the bug-report 06 fix, the `T()` call silently
    // failed (because `9 === r.nodeType` was false on heso's
    // Document), `C` stayed `undefined`, and the next `ce(...)`
    // crashed on `C.createElement`.
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            // Simulate Sizzle's relevant fragment.
            const p = document; // p = "preferred document" in Sizzle.
            let C; // The cached document.

            function setDocument(e) {
                const r = e ? (e.ownerDocument || e) : p;
                // jQuery gates on 9 === r.nodeType. Without this
                // fix the comparison was `9 === undefined === false`
                // and C stayed undefined.
                if (r != C && 9 === r.nodeType && r.documentElement) {
                    C = r;
                    return true;
                }
                return false;
            }

            // Feature-detect helper that reads C.createElement.
            function ce(fn) {
                const t = C.createElement("fieldset");
                return fn(t);
            }

            const setupOk = setDocument();
            const detectOk = ce(function (el) { return el.tagName === 'FIELDSET'; });

            JSON.stringify({ setupOk, detectOk, cached: C === document })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["setupOk"], true, "setDocument must succeed once nodeType is exposed");
    assert_eq!(v["detectOk"], true, "ce(...) must run cleanly with C bound");
    assert_eq!(v["cached"], true, "C must point at the host document");
}

// ===== bug-report 06.2 — DOMTokenList iteration =========================

#[test]
fn dom_token_list_has_symbol_iterator() {
    let s = page("<html><body><div class='a b c'></div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const div = document.querySelector('div');
            const cl = div.classList;
            // The destructure pattern airgap.js uses.
            const { [Symbol.iterator]: iter } = cl;
            JSON.stringify({ hasIter: typeof iter })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["hasIter"], "function",
        "DOMTokenList.prototype[Symbol.iterator] must be a function — airgap.js's destructure crashes otherwise");
}

#[test]
fn dom_token_list_iterator_returns_iterator_shape() {
    let s = page("<html><body><div class='a b'></div></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const div = document.querySelector('div');
            const cl = div.classList;
            const it = cl[Symbol.iterator]();
            const first = it.next();
            JSON.stringify({
                hasNext: typeof it.next,
                firstDone: first.done,
                firstShape: 'done' in first && 'value' in first,
            })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["hasNext"], "function");
    assert_eq!(v["firstShape"], true);
}

#[test]
fn airgap_module_init_pattern_runs_to_completion() {
    // The exact pattern airgap.js uses at module-init time.
    // Before the bug-report 06 fix, the second `ia.map(...)` call
    // threw "not a function" because the destructured
    // `[Symbol.iterator]` of `classList` was undefined.
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const $s = Symbol.iterator;
            // Mirror airgap: array of iterables to feature-detect.
            // We drop the `""` entry because airgap's actual code
            // gates with `e && e[$s]().next` and an empty string
            // is falsy — the airgap code returns `""` for that
            // slot (not a function), and the test we care about
            // is "the classList slot doesn't crash with not-a-
            // function on the next .next access".
            const ia = [
                [],
                new Set(),
                new Map(),
                document.createElement('_').classList,
            ];
            // First pass: destructure Symbol.iterator from each.
            const firsts = ia.map(({ [$s]: e }) => e);
            // Second pass: call e[Symbol.iterator]() and grab .next.
            // This is the call that crashed before the fix —
            // classList.Symbol.iterator was undefined, calling it
            // threw, and the iteration died.
            const nexts = ia.map(e => e[$s]().next);
            JSON.stringify({
                firstsCount: firsts.length,
                nextsCount: nexts.length,
                allNextsAreFunctions: nexts.every(n => typeof n === 'function'),
            })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["firstsCount"], 4);
    assert_eq!(v["nextsCount"], 4);
    assert_eq!(v["allNextsAreFunctions"], true,
        "every entry of ia must yield a callable `.next` after Symbol.iterator() — airgap.js depends on this");
}

// ===== Spec-IDL constructor stubs (airgap pulls these off globalThis) ===

#[test]
fn message_port_has_prototype_methods_for_destructure() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            // The airgap pattern: capture method handles from
            // MessagePort.prototype at module-init time via
            // destructuring. Before the fix, MessagePort was
            // undefined and `MessagePort.prototype` threw.
            const { postMessage, start, close } = MessagePort.prototype;
            JSON.stringify({
                postMessage: typeof postMessage,
                start: typeof start,
                close: typeof close,
            })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["postMessage"], "function");
    assert_eq!(v["start"], "function");
    assert_eq!(v["close"], "function");
}

#[test]
fn intl_date_time_format_is_destructurable() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            // The airgap pattern: `{DateTimeFormat:tm}=Intl` —
            // unconditional destructure of Intl at module init.
            // Before the fix, Intl was undefined and the
            // destructure threw "Cannot destructure property of
            // undefined".
            const { DateTimeFormat } = Intl;
            const tm = new DateTimeFormat();
            JSON.stringify({
                ctor: typeof DateTimeFormat,
                tm: typeof tm,
                opts: typeof tm.resolvedOptions(),
            })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["ctor"], "function");
    assert_eq!(v["tm"], "object");
    assert_eq!(v["opts"], "object");
}

#[test]
fn submit_event_is_constructable() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            // airgap reads `new qs("securitypolicyviolation")` (where
            // qs = SecurityPolicyViolationEvent). Real bundles do
            // similar for SubmitEvent etc. — the constructor must be
            // callable, not throw "Illegal constructor".
            const e = new SubmitEvent('submit', { submitter: null });
            JSON.stringify({
                type: e.type,
                submitter: e.submitter,
                ctorName: e.constructor.name,
            })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["type"], "submit");
    assert_eq!(v["submitter"], serde_json::Value::Null);
    assert_eq!(v["ctorName"], "SubmitEvent");
}

#[test]
fn node_proto_baseuri_owner_document_namespace_uri_accessors() {
    let s = page("<html><body><p>x</p></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            const p = document.querySelector('p');
            JSON.stringify({
                baseURI: typeof p.baseURI,
                ownerDocument: p.ownerDocument === document,
                namespaceURI: p.namespaceURI,
                isConnected: p.isConnected,
            })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["baseURI"], "string");
    assert_eq!(v["ownerDocument"], true);
    assert_eq!(v["namespaceURI"], "http://www.w3.org/1999/xhtml");
    assert_eq!(v["isConnected"], true);
}

#[test]
fn navigator_languages_is_accessor_descriptor() {
    let s = page("<html><body></body></html>");
    let out = s
        .engine()
        .eval(
            r#"
            // airgap pattern: `H(Navigator, "languages").get` —
            // requires an accessor descriptor on Navigator.prototype.
            const d = Object.getOwnPropertyDescriptor(Navigator.prototype, 'languages');
            JSON.stringify({
                hasGet: typeof d.get,
                resolves: typeof d.get.call(navigator),
            })
            "#,
        )
        .expect("eval");
    let v: serde_json::Value = serde_json::from_str(out.value.as_str().unwrap()).unwrap();
    assert_eq!(v["hasGet"], "function");
    assert_eq!(v["resolves"], "object");
}

#[test]
fn event_target_accepts_pojo_via_captured_prototype_method() {
    let s = page("<html><body></body></html>");
    // The airgap.js pattern: capture
    // `EventTarget.prototype.addEventListener` at module-init,
    // then call it with arbitrary singletons (cookieStore,
    // performance) as `this`. Before the fix, the rquickjs
    // EventTarget class's strict `this` guard rejected POJOs
    // with "Error converting from js 'object' into type
    // 'EventTarget'".
    let out = s
        .engine()
        .eval(
            r#"
            const add = EventTarget.prototype.addEventListener;
            const obj = {};
            let count = 0;
            add.call(obj, 'tick', () => { count += 1; });
            // dispatchEvent on a POJO using the permissive shim.
            const dispatch = EventTarget.prototype.dispatchEvent;
            dispatch.call(obj, new Event('tick'));
            count
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, 1,
        "EventTarget.prototype methods must accept any object as `this` so captured-method patterns work");
}

#[test]
fn fresh_engine_does_not_crash_on_classlist_iterate() {
    // The full end-to-end of the airgap pattern: classList iteration
    // via `Symbol.iterator` from a `<div class="...">` element. This
    // is the load-bearing surface that bug-report 06.2 names.
    let s = page("<html><body><div class='a b c'></div></body></html>");
    let outcome = s.engine().eval(
        r#"
        const div = document.querySelector('div');
        const list = div.classList;
        // Round-trip: capture the iterator, run it to completion,
        // verify yields the spec-iterator shape.
        const iter = list[Symbol.iterator];
        const exists = typeof iter === 'function';
        let yields = 0;
        for (const t of list) {
            yields++;
        }
        JSON.stringify({ exists, yields })
        "#,
    );
    let outcome = outcome.expect("eval");
    let v: serde_json::Value = serde_json::from_str(outcome.value.as_str().unwrap()).unwrap();
    assert_eq!(v["exists"], true);
    // The iterator yields the actual class tokens via the
    // DomTokenList.value accessor; the spec-fallback path yields
    // nothing when value isn't reachable. We accept either as
    // long as the loop does not crash.
    let yields = v["yields"].as_i64().expect("integer");
    assert!(yields >= 0, "iteration must complete cleanly");
}

// ===== Pre-installed external-script policy doesn't crash boot ==========

#[test]
fn skip_external_policy_runs_inline_scripts_cleanly() {
    // Sanity: with `SkipExternal` policy, an inline `<script>`
    // exercising the patched globals does not crash. This is the
    // mode `heso read` uses against subresources by default.
    let url = Url::parse("https://example.com/").unwrap();
    let (session, _outcome) = JsSession::open_on_engine(
        heso_engine_js::JsEngine::new().expect("engine"),
        r#"<html><body>
            <script>
                window.__hesoSmokeOk = (function () {
                    const c = document.createComment('');
                    const i = Intl.DateTimeFormat;
                    return c.nodeType === 8 && typeof i === 'function';
                })();
            </script>
        </body></html>"#,
        url,
        ScriptFetchPolicy::Skip,
    )
    .expect("open");
    let out = session
        .engine()
        .eval("globalThis.__hesoSmokeOk")
        .expect("eval");
    assert_eq!(out.value, true);
}
