//! Integration tests for late-upgrade re-prototyping (WHATWG HTML
//! §4.13.6.6 "upgrade an element") plus the surrounding surface
//! V5 found absent on real component-library sites: `HTMLTemplateElement`,
//! `element.dataset`, `element.insertAdjacentHTML`.
//!
//! V5 verified that against [`shoelace.style`] heso parsed 28
//! dashed-tag elements, the page's bootstrap registered 5 of them
//! against `customElements`, but **zero** of the rendered elements
//! actually upgraded (`el instanceof MyEl` returned false,
//! `constructor.name` stayed `"Element"`, custom methods came back
//! `undefined`). The cause: the upgrade walk in
//! [`crate::custom_elements`] re-prototypes the wrapper *it observes
//! during the walk*, but `document.querySelector(...)` later hands
//! the user a FRESH wrapper around the same NodeId whose
//! `[[Prototype]]` is bare `Element.prototype`.
//!
//! These tests pin the spec-correct outcome: every wrapper observed
//! after a `define()` either has the right prototype directly
//! (via lazy stamping in the wrapper-returning method wrappers) or
//! routes through `Symbol.hasInstance` on the registered ctor.
//!
//! Spec refs:
//! - WHATWG HTML §4.13.6.6 "upgrade an element"
//! - WHATWG HTML §4.12.3 "HTMLTemplateElement"
//! - WHATWG HTML §3.2.6.6 "DOMStringMap" (`dataset`)
//! - WHATWG DOM § Element-insertAdjacentHTML
//!
//! [`shoelace.style`]: https://shoelace.style/

use heso_engine_js::{JsEngine, JsSession};
use url::Url;

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

fn page(html: &str) -> JsSession {
    let url = Url::parse("https://example.com/").unwrap();
    JsSession::open(html, url).expect("open page").0
}

// ===== Late-upgrade re-prototyping ====================================
//
// The canonical bug: page parses dashed-tag elements, JS module loads,
// JS module calls `customElements.define(name, MyEl)`. The previously
// parsed elements must upgrade to `MyEl` instances — `instanceof MyEl`,
// `constructor.name`, and user methods all need to resolve correctly.

#[test]
fn late_define_promotes_existing_dashed_element() {
    // V5's verbatim reproducer against example.com. The previous
    // code fired connectedCallback but did NOT re-prototype the
    // wrapper the user got via querySelector after define().
    let sess = page("<html><body><my-thing></my-thing></body></html>");
    let out = sess
        .eval(
            r#"
            const el = document.querySelector('my-thing');
            class MyEl extends HTMLElement {
                sayHi() { return 'hi'; }
            }
            customElements.define('my-thing', MyEl);
            // After define, the existing element must upgrade. Re-
            // querying gives a FRESH wrapper — late upgrade has to
            // work on whatever wrapper user JS observes, not just
            // the one the upgrade walk saw.
            const reEl = document.querySelector('my-thing');
            JSON.stringify({
                ctorName: reEl.constructor.name,
                isInstance: reEl instanceof MyEl,
                callMethod: typeof reEl.sayHi,
                methodResult: reEl.sayHi(),
                // The original wrapper observed BEFORE define() also
                // works because Symbol.hasInstance discriminates by
                // tag name, not by prototype identity.
                preDefineInstanceOf: el instanceof MyEl,
            });
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(
            r#"{"ctorName":"MyEl","isInstance":true,"callMethod":"function","methodResult":"hi","preDefineInstanceOf":true}"#
        )
    );
}

#[test]
fn v5_verbatim_reproducer_against_example_com() {
    // V5's exact JS reproducer, run verbatim. Before this fix:
    //   ctorName = "Element", isInstance = false,
    //   callMethod = "undefined".
    // After the fix:
    //   ctorName = "MyEl",     isInstance = true,
    //   callMethod = "function".
    //
    // The fix path for each:
    //   - ctorName: Element.prototype.constructor is now a getter
    //     that dispatches by receiver's localName against the
    //     custom-element registry.
    //   - isInstance: customElements.define installs
    //     `ctor[Symbol.hasInstance]` to discriminate by tag name,
    //     so `instanceof MyEl` returns true for every wrapper of
    //     a registered tag regardless of [[Prototype]] state.
    //   - callMethod: Element.prototype's "sayHi" lookup falls
    //     through. The wrapper `el` was captured BEFORE define
    //     so its proto is still bare Element.prototype — method
    //     lookup would normally fail. We don't override method
    //     dispatch on stale wrappers (would require a Proxy on
    //     Element.prototype, which is fairly invasive); instead,
    //     the user typically re-queries after define (see
    //     `late_define_promotes_existing_dashed_element` for the
    //     post-define-query path).
    //
    // For the verbatim reproducer below, `el` is captured before
    // define. The ctorName and isInstance results are now correct;
    // callMethod stays "undefined" on the stale wrapper. The fix
    // unblocks the bigger Shoelace / Material Web pattern, which
    // is "register, then querySelector" — i.e. always observes
    // post-define wrappers.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            document.body.innerHTML = '<my-thing></my-thing>';
            const el = document.querySelector('my-thing');
            class MyEl extends HTMLElement { sayHi() { return 'hi'; } }
            customElements.define('my-thing', MyEl);
            JSON.stringify({
                ctorName: el.constructor.name,
                isInstance: el instanceof MyEl,
                callMethod: typeof el.sayHi,
            });
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(
            r#"{"ctorName":"MyEl","isInstance":true,"callMethod":"undefined"}"#
        )
    );
}

#[test]
fn late_define_fires_connected_callback_in_addition_to_reprototype() {
    // Re-prototyping doesn't replace the connectedCallback firing
    // — both must happen for pre-existing connected elements.
    let sess = page("<html><body><x-both></x-both></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__log = [];
            class XBoth extends HTMLElement {
                connectedCallback() { globalThis.__log.push('connected'); }
                sayHello() { return 'hello'; }
            }
            customElements.define('x-both', XBoth);
            const el = document.querySelector('x-both');
            JSON.stringify({
                connectedFired: globalThis.__log,
                ctorName: el.constructor.name,
                callsMethod: el.sayHello(),
            });
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(
            r#"{"connectedFired":["connected"],"ctorName":"XBoth","callsMethod":"hello"}"#
        )
    );
}

#[test]
fn attribute_changed_callback_fires_after_late_upgrade_with_present_attrs() {
    // Element exists with observed attrs in static HTML, then
    // define() runs. attributeChangedCallback must fire for each
    // observed attr present at upgrade time, with oldValue=null.
    // Only OBSERVED attributes trigger the callback — `data-bar`
    // here is on the element but not in observedAttributes, so we
    // see exactly one event.
    let sess = page(
        r#"<html><body><x-preset-late data-foo="hello" data-bar="world"></x-preset-late></body></html>"#,
    );
    let out = sess
        .eval(
            r#"
            globalThis.__log = [];
            class XPresetLate extends HTMLElement {
                static get observedAttributes() { return ['data-foo']; }
                attributeChangedCallback(name, oldVal, newVal) {
                    globalThis.__log.push([name, oldVal, newVal]);
                }
            }
            customElements.define('x-preset-late', XPresetLate);
            globalThis.__log
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!([["data-foo", null, "hello"]])
    );
}

#[test]
fn append_child_of_dashed_element_upgrades_at_insertion() {
    // define() first, then create + appendChild. The appended node
    // should be a MyEl instance with its connectedCallback fired.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__connected = 0;
            class XAppend extends HTMLElement {
                connectedCallback() { globalThis.__connected += 1; }
                tag() { return 'append-' + this.localName; }
            }
            customElements.define('x-append', XAppend);
            const fresh = document.createElement('x-append');
            document.body.appendChild(fresh);
            const found = document.querySelector('x-append');
            JSON.stringify({
                freshIsInstance: fresh instanceof XAppend,
                foundIsInstance: found instanceof XAppend,
                foundCtorName: found.constructor.name,
                foundMethod: found.tag(),
                connectedCount: globalThis.__connected,
            });
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(
            r#"{"freshIsInstance":true,"foundIsInstance":true,"foundCtorName":"XAppend","foundMethod":"append-x-append","connectedCount":1}"#
        )
    );
}

#[test]
fn inner_html_setter_upgrades_new_matching_children_via_query() {
    // After `parent.innerHTML = '<my-thing></my-thing>'`, querying
    // for the new element must return an upgraded wrapper.
    //
    // Known limit (already documented in custom_elements.rs):
    // setting innerHTML directly does NOT fire connectedCallback
    // because the innerHTML setter on Element.prototype is non-
    // configurable (rquickjs accessor flag). But the wrappers
    // observed via querySelector after the assignment DO upgrade
    // because the query method itself is wrapped to lazy-stamp.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XInner extends HTMLElement {
                greet() { return 'inner'; }
            }
            customElements.define('x-inner', XInner);
            document.body.innerHTML = '<x-inner></x-inner>';
            const el = document.querySelector('x-inner');
            JSON.stringify({
                isInstance: el instanceof XInner,
                ctorName: el.constructor.name,
                callMethod: el.greet(),
            });
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(
            r#"{"isInstance":true,"ctorName":"XInner","callMethod":"inner"}"#
        )
    );
}

#[test]
fn query_selector_all_returns_upgraded_wrappers() {
    // Stamping must also happen on each wrapper in a list returned
    // by querySelectorAll, not just on querySelector's single
    // result.
    let sess = page(
        "<html><body><x-list></x-list><x-list></x-list><x-list></x-list></body></html>",
    );
    let out = sess
        .eval(
            r#"
            class XList extends HTMLElement {
                kind() { return 'list-item'; }
            }
            customElements.define('x-list', XList);
            const all = document.querySelectorAll('x-list');
            JSON.stringify({
                count: all.length,
                allInstances: all.every(el => el instanceof XList),
                allMethods: all.every(el => typeof el.kind === 'function'),
                methodValues: all.map(el => el.kind()),
            });
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(
            r#"{"count":3,"allInstances":true,"allMethods":true,"methodValues":["list-item","list-item","list-item"]}"#
        )
    );
}

#[test]
fn instance_of_works_for_fresh_wrapper_via_symbol_hasinstance() {
    // Even when the wrapper's [[Prototype]] is the bare
    // Element.prototype (a wrapper that hasn't been touched by any
    // upgrading code path), `instanceof MyEl` must still return
    // true. The Symbol.hasInstance trap installed by define()
    // discriminates by tag name.
    let sess = page("<html><body><x-symbol></x-symbol></body></html>");
    let out = sess
        .eval(
            r#"
            class XSymbol extends HTMLElement {}
            customElements.define('x-symbol', XSymbol);
            // Reach for the element via a path that doesn't pass
            // through a stamping wrapper — directly off
            // document.body.children. Children getter is non-
            // configurable so we can't intercept it; the wrapper
            // arrives with bare Element.prototype. instanceof must
            // STILL work via Symbol.hasInstance.
            const el = document.body.children[0];
            JSON.stringify({
                isInstance: el instanceof XSymbol,
                localName: el.localName,
            });
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(r#"{"isInstance":true,"localName":"x-symbol"}"#)
    );
}

// ===== HTMLTemplateElement ============================================
//
// V5 hit `HTMLTemplateElement is not defined` on Material Web and other
// Lit-based libraries during their module evaluation phase.

#[test]
fn template_is_html_template_element() {
    let sess = page("<html><body><template id='t'><p>hi</p></template></body></html>");
    let out = sess
        .eval(
            r#"
            const t = document.querySelector('template');
            [t instanceof HTMLTemplateElement, t.localName]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, "template"]));
}

#[test]
fn template_content_returns_fragment_like_object() {
    // Per the doc-comment in custom_elements.rs, .content is the
    // template element itself; its querySelector / children /
    // cloneNode walks its own light children. Lit's
    // `template.content.cloneNode(true)` then `parent.appendChild`
    // works on this approximation because Lit's TreeWalker
    // operates on the returned node's descendants regardless of
    // node type.
    let sess = page("<html><body><template><p>inner</p></template></body></html>");
    let out = sess
        .eval(
            r#"
            const t = document.querySelector('template');
            const c = t.content;
            const p = c.querySelector('p');
            JSON.stringify({
                contentExists: c != null,
                pFound: p != null,
                pText: p ? p.textContent : null,
                childCount: c.children.length,
            });
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(r#"{"contentExists":true,"pFound":true,"pText":"inner","childCount":1}"#)
    );
}

#[test]
fn template_content_undefined_on_non_template_element() {
    // The .content getter returns undefined for non-template tags.
    // Real browsers only expose .content on HTMLTemplateElement;
    // for div / span etc. the property is unset.
    let sess = page("<html><body><div></div></body></html>");
    let out = sess
        .eval(
            r#"
            const d = document.querySelector('div');
            d.content === undefined
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn global_this_html_template_element_is_illegal_to_construct() {
    let err = engine()
        .eval("new HTMLTemplateElement()")
        .expect_err("new HTMLTemplateElement() should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Illegal constructor") || msg.contains("TypeError"),
        "expected Illegal constructor TypeError, got: {msg}"
    );
}

#[test]
fn global_this_html_template_element_is_a_function() {
    let out = engine().eval("typeof HTMLTemplateElement").expect("eval");
    assert_eq!(out.value, "function");
}

// ===== element.dataset ================================================
//
// V5 found `el.dataset` undefined; component libraries reach for it
// constantly (data-* attrs are the canonical "pass config from HTML
// to JS" channel).

#[test]
fn dataset_reads_data_attrs() {
    let sess = page(r#"<html><body><div data-foo="bar" data-num="42"></div></body></html>"#);
    let out = sess
        .eval(
            r#"
            const d = document.querySelector('div');
            [d.dataset.foo, d.dataset.num, d.dataset.missing]
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(["bar", "42", null])
    );
}

#[test]
fn dataset_writes_data_attrs() {
    let sess = page("<html><body><div></div></body></html>");
    let out = sess
        .eval(
            r#"
            const d = document.querySelector('div');
            d.dataset.greeting = 'hello';
            d.dataset.count = 5;
            [d.getAttribute('data-greeting'), d.getAttribute('data-count')]
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(["hello", "5"])
    );
}

#[test]
fn dataset_camel_case_conversion() {
    // dataset.fooBar ↔ data-foo-bar; dataset.userId ↔ data-user-id.
    let sess = page(r#"<html><body><div data-foo-bar="x" data-user-id="123"></div></body></html>"#);
    let out = sess
        .eval(
            r#"
            const d = document.querySelector('div');
            // Read via camelCase.
            const readFoo = d.dataset.fooBar;
            const readUid = d.dataset.userId;
            // Write via camelCase; verify the attribute landed as kebab.
            d.dataset.someValue = 'sv';
            const writeBack = d.getAttribute('data-some-value');
            [readFoo, readUid, writeBack]
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!(["x", "123", "sv"])
    );
}

#[test]
fn dataset_in_operator() {
    let sess = page(r#"<html><body><div data-present="y"></div></body></html>"#);
    let out = sess
        .eval(
            r#"
            const d = document.querySelector('div');
            ['present' in d.dataset, 'missing' in d.dataset]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, false]));
}

#[test]
fn dataset_delete() {
    let sess = page(r#"<html><body><div data-removeme="y"></div></body></html>"#);
    let out = sess
        .eval(
            r#"
            const d = document.querySelector('div');
            delete d.dataset.removeme;
            [d.hasAttribute('data-removeme'), d.dataset.removeme]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([false, null]));
}

#[test]
fn dataset_object_keys_enumerates_data_attrs() {
    let sess = page(
        r#"<html><body><div data-foo="a" data-bar-baz="b" class="cls" id="i"></div></body></html>"#,
    );
    let out = sess
        .eval(
            r#"
            const d = document.querySelector('div');
            const keys = Object.keys(d.dataset).sort();
            keys
            "#,
        )
        .expect("eval");
    // Only data-* keys, in camelCase form. The `class` and `id`
    // attributes are not exposed.
    assert_eq!(out.value, serde_json::json!(["barBaz", "foo"]));
}

// ===== element.insertAdjacentHTML =====================================

#[test]
fn insert_adjacent_html_beforeend() {
    let sess = page("<html><body><div id='host'><span>a</span></div></body></html>");
    let out = sess
        .eval(
            r#"
            const host = document.getElementById('host');
            host.insertAdjacentHTML('beforeend', '<span>b</span>');
            // host now has two spans.
            const kids = host.querySelectorAll('span').map(s => s.textContent);
            kids
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["a", "b"]));
}

#[test]
fn insert_adjacent_html_afterbegin() {
    let sess = page("<html><body><div id='host'><span>existing</span></div></body></html>");
    let out = sess
        .eval(
            r#"
            const host = document.getElementById('host');
            host.insertAdjacentHTML('afterbegin', '<span>first</span>');
            const kids = host.querySelectorAll('span').map(s => s.textContent);
            kids
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["first", "existing"]));
}

#[test]
fn insert_adjacent_html_beforebegin() {
    let sess = page("<html><body><div id='parent'><div id='target'></div></div></body></html>");
    let out = sess
        .eval(
            r#"
            const target = document.getElementById('target');
            target.insertAdjacentHTML('beforebegin', '<p id="pre"></p>');
            const parent = document.getElementById('parent');
            // parent's children in order: <p#pre>, <div#target>.
            parent.children.map(c => c.id)
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["pre", "target"]));
}

#[test]
fn insert_adjacent_html_afterend() {
    let sess = page("<html><body><div id='parent'><div id='target'></div></div></body></html>");
    let out = sess
        .eval(
            r#"
            const target = document.getElementById('target');
            target.insertAdjacentHTML('afterend', '<p id="post"></p>');
            const parent = document.getElementById('parent');
            parent.children.map(c => c.id)
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(["target", "post"]));
}

#[test]
fn insert_adjacent_html_throws_when_no_parent_for_beforebegin() {
    // Orphan element (created via createElement, never appendChild'd)
    // has no parent. 'beforebegin' / 'afterend' require a parent.
    let sess = page("<html><body></body></html>");
    let err = sess
        .eval(
            r#"
            const orphan = document.createElement('div');
            orphan.insertAdjacentHTML('beforebegin', '<p></p>');
            "#,
        )
        .expect_err("expected throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("beforebegin") || msg.contains("parent"),
        "expected parent-required error, got: {msg}"
    );
}

#[test]
fn insert_adjacent_html_throws_on_invalid_position() {
    let sess = page("<html><body><div></div></body></html>");
    let err = sess
        .eval(
            r#"
            document.querySelector('div').insertAdjacentHTML('middle', '<p></p>');
            "#,
        )
        .expect_err("expected throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("position") || msg.contains("middle"),
        "expected invalid-position error, got: {msg}"
    );
}

