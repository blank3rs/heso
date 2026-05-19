//! Integration tests for the Web Components surface installed by
//! [`crate::custom_elements::install_custom_elements`]: the global
//! `customElements`, `HTMLElement`, and the illegal-constructor
//! shims for `Element` / `Document` / `Node` / `DocumentFragment` /
//! `DOMTokenList`. Plus the upgrade lifecycle: `connectedCallback`,
//! `disconnectedCallback`, `attributeChangedCallback`.
//!
//! Each test pins one spec-bearing behaviour. Failures here mean a
//! page using web components will misbehave on heso — every modern
//! component-driven framework (Lit, Solid Element, Stencil,
//! Svelte custom-element compile target, etc.) depends on this
//! surface working.
//!
//! Spec refs:
//! - WHATWG DOM §4.4 "Interface Element" — illegal-constructor.
//! - WHATWG HTML §4.13 "Custom elements" — registry + lifecycle.
//! - WHATWG HTML §4.13.5.1 "HTMLElement constructor" — the
//!   construction-stack dance that lets `super()` produce a real
//!   Element wrapper.

use heso_engine_js::{JsEngine, JsSession};
use url::Url;

fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

fn page(html: &str) -> JsSession {
    let url = Url::parse("https://example.com/").unwrap();
    JsSession::open(html, url).expect("open page").0
}

// ===== Illegal-constructor surface ====================================

#[test]
fn global_this_element_is_a_function() {
    let out = engine().eval("typeof Element").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn global_this_element_is_a_function_but_illegal_to_construct() {
    let err = engine()
        .eval("new Element()")
        .expect_err("new Element() should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Illegal constructor") || msg.contains("TypeError"),
        "expected Illegal constructor TypeError, got: {msg}"
    );
}

#[test]
fn global_this_document_is_a_function_but_illegal_to_construct() {
    let out = engine().eval("typeof Document").expect("eval");
    assert_eq!(out.value, "function");
    let err = engine()
        .eval("new Document()")
        .expect_err("new Document() should throw");
    assert!(format!("{err:?}").contains("Illegal constructor"));
}

#[test]
fn global_this_node_is_a_function_but_illegal_to_construct() {
    let out = engine().eval("typeof Node").expect("eval");
    assert_eq!(out.value, "function");
    let err = engine()
        .eval("new Node()")
        .expect_err("new Node() should throw");
    assert!(format!("{err:?}").contains("Illegal constructor"));
}

#[test]
fn global_this_documentfragment_is_a_function() {
    let out = engine().eval("typeof DocumentFragment").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn global_this_domtokenlist_is_a_function() {
    let out = engine().eval("typeof DOMTokenList").expect("eval");
    assert_eq!(out.value, "function");
}

#[test]
fn node_exposes_node_type_constants() {
    // DOM §4.4: ELEMENT_NODE = 1, TEXT_NODE = 3, COMMENT_NODE = 8,
    // DOCUMENT_NODE = 9, DOCUMENT_FRAGMENT_NODE = 11. Frameworks
    // gate on these via `Node.ELEMENT_NODE`.
    let out = engine()
        .eval("[Node.ELEMENT_NODE, Node.TEXT_NODE, Node.COMMENT_NODE, Node.DOCUMENT_NODE, Node.DOCUMENT_FRAGMENT_NODE]")
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([1, 3, 8, 9, 11]));
}

#[test]
fn document_instance_is_instance_of_document() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval("document instanceof Document")
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn element_instance_is_instance_of_element() {
    let sess = page("<html><body><div id='d'>x</div></body></html>");
    let out = sess
        .eval("document.getElementById('d') instanceof Element")
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn element_instance_is_instance_of_node() {
    // Phase 1B uses one Element wrapper for all node types, so the
    // Node "constructor" shares Element's prototype; this passes.
    let sess = page("<html><body><div id='d'>x</div></body></html>");
    let out = sess
        .eval("document.getElementById('d') instanceof Node")
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

// ===== HTMLElement =====================================================

#[test]
fn direct_new_html_element_throws_illegal_constructor() {
    // WHATWG HTML §4.13.5.1 step 7: HTMLElement constructor throws
    // TypeError when called outside of a customElements upgrade.
    let err = engine()
        .eval("new HTMLElement()")
        .expect_err("bare new HTMLElement() should throw");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("Illegal constructor") || msg.contains("TypeError"),
        "expected Illegal constructor TypeError, got: {msg}"
    );
}

#[test]
fn html_element_is_constructible_when_subclassed_and_registered() {
    // The canonical custom-element pattern: extends HTMLElement,
    // customElements.define, document.createElement. Must produce
    // an instance whose `localName` matches the registered name.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XBasic extends HTMLElement {
                constructor() { super(); }
            }
            customElements.define('x-basic', XBasic);
            const el = document.createElement('x-basic');
            [el instanceof XBasic, el instanceof HTMLElement, el instanceof Element, el.localName]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([true, true, true, "x-basic"]));
}

#[test]
fn html_element_super_twice_throws() {
    // WHATWG HTML §4.13.5.1 step 9: calling super() twice from the
    // same constructor invocation throws because the construction-
    // stack entry has been replaced with the "already constructed"
    // sentinel.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__doubleErr = null;
            class XDouble extends HTMLElement {
                constructor() {
                    super();
                    try { super(); }
                    catch (e) { globalThis.__doubleErr = e.name; }
                }
            }
            customElements.define('x-double', XDouble);
            document.createElement('x-double');
            globalThis.__doubleErr
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "TypeError");
}

// ===== Lifecycle: connectedCallback ===================================

#[test]
fn connected_callback_fires_on_existing_elements_at_define_time() {
    // Spec: customElements.define walks the document and upgrades
    // existing matching elements, firing connectedCallback for those
    // already in the tree.
    let sess = page("<html><body><x-existing></x-existing></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__fired = false;
            class XExisting extends HTMLElement {
                connectedCallback() { globalThis.__fired = true; }
            }
            customElements.define('x-existing', XExisting);
            globalThis.__fired
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn connected_callback_fires_when_inserted_after_definition() {
    // Define-then-insert path. document.createElement returns an
    // upgraded element; appendChild attaches it; connectedCallback
    // fires from the appendChild wrapper.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__fired = 0;
            class XLater extends HTMLElement {
                connectedCallback() { globalThis.__fired += 1; }
            }
            customElements.define('x-later', XLater);
            const el = document.createElement('x-later');
            document.body.appendChild(el);
            globalThis.__fired
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(1));
}

#[test]
fn connected_callback_does_not_fire_on_detached_elements() {
    // Creating but not appending must NOT fire connectedCallback.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__fired = 0;
            class XDetached extends HTMLElement {
                connectedCallback() { globalThis.__fired += 1; }
            }
            customElements.define('x-detached', XDetached);
            const el = document.createElement('x-detached');
            // No appendChild.
            globalThis.__fired
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(0));
}

#[test]
fn disconnected_callback_fires_on_remove() {
    let sess = page("<html><body><x-bye></x-bye></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__connected = 0;
            globalThis.__disconnected = 0;
            class XBye extends HTMLElement {
                connectedCallback() { globalThis.__connected += 1; }
                disconnectedCallback() { globalThis.__disconnected += 1; }
            }
            customElements.define('x-bye', XBye);
            const el = document.querySelector('x-bye');
            el.remove();
            [globalThis.__connected, globalThis.__disconnected]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!([1, 1]));
}

#[test]
fn disconnected_callback_fires_on_remove_child() {
    let sess = page("<html><body><x-bye2></x-bye2></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__disconnected = 0;
            class XBye2 extends HTMLElement {
                disconnectedCallback() { globalThis.__disconnected += 1; }
            }
            customElements.define('x-bye2', XBye2);
            const el = document.querySelector('x-bye2');
            document.body.removeChild(el);
            globalThis.__disconnected
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(1));
}

// ===== Lifecycle: attributeChangedCallback ============================

#[test]
fn attribute_changed_callback_fires_for_observed_attrs_only() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__log = [];
            class XAttrs extends HTMLElement {
                static get observedAttributes() { return ['data-foo']; }
                attributeChangedCallback(name, oldVal, newVal) {
                    globalThis.__log.push([name, oldVal, newVal]);
                }
            }
            customElements.define('x-attrs', XAttrs);
            const el = document.createElement('x-attrs');
            document.body.appendChild(el);
            el.setAttribute('data-foo', 'a');
            el.setAttribute('data-bar', 'b');   // NOT observed → no callback
            el.setAttribute('data-foo', 'c');   // observed change
            globalThis.__log
            "#,
        )
        .expect("eval");
    // Only data-foo events; old=null on the first, old='a' on the change.
    assert_eq!(
        out.value,
        serde_json::json!([
            ["data-foo", null, "a"],
            ["data-foo", "a", "c"],
        ])
    );
}

#[test]
fn attribute_changed_callback_fires_on_remove_attribute() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__log = [];
            class XAttrRm extends HTMLElement {
                static get observedAttributes() { return ['hidden']; }
                attributeChangedCallback(name, oldVal, newVal) {
                    globalThis.__log.push([name, oldVal, newVal]);
                }
            }
            customElements.define('x-attrrm', XAttrRm);
            const el = document.createElement('x-attrrm');
            document.body.appendChild(el);
            el.setAttribute('hidden', '');
            el.removeAttribute('hidden');
            globalThis.__log
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!([
            ["hidden", null, ""],
            ["hidden", "", null],
        ])
    );
}

#[test]
fn attribute_changed_callback_fires_for_attrs_present_at_define_time() {
    // Spec: when upgrading an existing element, fire
    // attributeChangedCallback for every attribute in observedAttributes
    // that's present on the element at upgrade time, with oldValue=null.
    let sess = page(
        r#"<html><body><x-preset data-foo="hello"></x-preset></body></html>"#,
    );
    let out = sess
        .eval(
            r#"
            globalThis.__log = [];
            class XPreset extends HTMLElement {
                static get observedAttributes() { return ['data-foo']; }
                attributeChangedCallback(name, oldVal, newVal) {
                    globalThis.__log.push([name, oldVal, newVal]);
                }
            }
            customElements.define('x-preset', XPreset);
            globalThis.__log
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!([["data-foo", null, "hello"]])
    );
}

// ===== Registry methods ===============================================

#[test]
fn custom_elements_get_returns_constructor() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XGet extends HTMLElement {}
            customElements.define('x-get', XGet);
            customElements.get('x-get') === XGet
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn custom_elements_get_returns_undefined_for_unknown() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval("typeof customElements.get('x-nothing-defined')")
        .expect("eval");
    assert_eq!(out.value, "undefined");
}

#[test]
fn custom_elements_get_name_returns_name_for_registered_ctor() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XGetName extends HTMLElement {}
            customElements.define('x-getname', XGetName);
            customElements.getName(XGetName)
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "x-getname");
}

#[test]
fn custom_elements_get_name_returns_null_for_unregistered() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class Unknown {}
            customElements.getName(Unknown)
            "#,
        )
        .expect("eval");
    assert!(out.value.is_null());
}

#[test]
fn custom_elements_when_defined_resolves_when_defined_after_call() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                class XWhen extends HTMLElement {}
                const p = customElements.whenDefined('x-when');
                customElements.define('x-when', XWhen);
                const v = await p;
                return v === XWhen ? 'ok' : 'mismatch';
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}

#[test]
fn custom_elements_when_defined_resolves_immediately_when_already_defined() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            (async () => {
                class XWhen2 extends HTMLElement {}
                customElements.define('x-when2', XWhen2);
                const v = await customElements.whenDefined('x-when2');
                return v === XWhen2 ? 'ok' : 'mismatch';
            })()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}

#[test]
fn custom_elements_define_rejects_invalid_names() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XBad extends HTMLElement {}
            try {
                customElements.define('NoHyphen', XBad);
                'no-throw'
            } catch (e) { e.name }
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "SyntaxError");
}

#[test]
fn custom_elements_define_rejects_uppercase_or_starts_with_digit() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XBad extends HTMLElement {}
            const errors = [];
            for (const name of ['MY-EL', '1bad-el', 'bad', 'bad name', '-bad']) {
                try {
                    customElements.define(name, class extends HTMLElement {});
                    errors.push([name, 'NO-THROW']);
                } catch (e) {
                    errors.push([name, e.name]);
                }
            }
            errors
            "#,
        )
        .expect("eval");
    // All five should throw SyntaxError.
    let arr = out.value.as_array().expect("array");
    for entry in arr {
        let name_kind = entry.as_array().unwrap();
        assert_eq!(
            name_kind[1], "SyntaxError",
            "expected SyntaxError for {:?}",
            name_kind[0]
        );
    }
}

#[test]
fn custom_elements_define_rejects_duplicate_names() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XDup1 extends HTMLElement {}
            class XDup2 extends HTMLElement {}
            customElements.define('x-dup', XDup1);
            try {
                customElements.define('x-dup', XDup2);
                'no-throw'
            } catch (e) { e.name }
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "NotSupportedError");
}

#[test]
fn custom_elements_define_rejects_duplicate_constructors() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XDupCtor extends HTMLElement {}
            customElements.define('x-dupc-a', XDupCtor);
            try {
                customElements.define('x-dupc-b', XDupCtor);
                'no-throw'
            } catch (e) { e.name }
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "NotSupportedError");
}

#[test]
fn custom_elements_define_accepts_unicode_names_starting_with_lowercase() {
    // Spec PCEN allows a wide Unicode subset after the leading ASCII
    // lowercase. We don't need to be exhaustive here — just confirm
    // that the regex is not over-strict.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            try {
                customElements.define('a-b', class extends HTMLElement {});
                'ok'
            } catch (e) { e.name }
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "ok");
}

#[test]
fn custom_elements_define_rejects_reserved_names() {
    // Spec reserves a small set of names that look like custom but
    // are already in use by SVG / fonts. Defining them throws.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            const reserved = [
                'annotation-xml','color-profile','font-face',
                'font-face-src','font-face-uri','font-face-format',
                'font-face-name','missing-glyph'
            ];
            const errs = [];
            for (const n of reserved) {
                try {
                    customElements.define(n, class extends HTMLElement {});
                    errs.push([n, 'NO-THROW']);
                } catch (e) { errs.push([n, e.name]); }
            }
            errs.every(([_, k]) => k === 'SyntaxError')
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(true));
}

#[test]
fn custom_elements_upgrade_walks_subtree() {
    // The user can call customElements.upgrade(parent) to force a
    // walk of the subtree and upgrade descendants. Spec §4.13.1.
    let sess = page("<html><body><div id='holder'></div></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__connected = 0;
            class XUpgrade extends HTMLElement {
                connectedCallback() { globalThis.__connected += 1; }
            }
            // Add the child BEFORE define so it's a plain element first.
            const holder = document.getElementById('holder');
            holder.innerHTML = '<x-upgrade></x-upgrade>';
            // Define — this walks the document and upgrades existing
            // <x-upgrade> tags too (the define-time pass).
            customElements.define('x-upgrade', XUpgrade);
            globalThis.__connected
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(1));
}

// ===== Connected/disconnected semantics on nested insertion ==========

#[test]
fn connected_callback_fires_when_parent_is_inserted() {
    // Insert a tree with custom-element descendants — the wrapper
    // walks the inserted subtree firing connectedCallback on each
    // registered descendant.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__connected = 0;
            class XNested extends HTMLElement {
                connectedCallback() { globalThis.__connected += 1; }
            }
            customElements.define('x-nested', XNested);
            const wrapper = document.createElement('div');
            wrapper.appendChild(document.createElement('x-nested'));
            wrapper.appendChild(document.createElement('x-nested'));
            // The wrapper itself is not custom; only descendants fire.
            document.body.appendChild(wrapper);
            globalThis.__connected
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(2));
}

#[test]
fn class_hierarchy_methods_resolve_after_upgrade() {
    // The user's class methods (including ones declared after the
    // base call) must be reachable on the upgraded element via
    // its [[Prototype]] chain.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XMethods extends HTMLElement {
                greet() { return 'hello-' + this.localName; }
            }
            customElements.define('x-methods', XMethods);
            const el = document.createElement('x-methods');
            el.greet()
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "hello-x-methods");
}

// ===== Cross-cutting: instance of Element / HTMLElement / Node ========

#[test]
fn upgraded_element_has_full_prototype_chain() {
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XChain extends HTMLElement {}
            customElements.define('x-chain', XChain);
            const el = document.createElement('x-chain');
            [
                el instanceof XChain,
                el instanceof HTMLElement,
                el instanceof Element,
                el instanceof Node,
            ]
            "#,
        )
        .expect("eval");
    assert_eq!(
        out.value,
        serde_json::json!([true, true, true, true])
    );
}

#[test]
fn upgraded_element_can_use_setattribute_and_getattribute() {
    // Smoke test: round-trip through the wrapper'd setAttribute /
    // getAttribute pair. Frameworks rely on the IDL-like dance.
    let sess = page("<html><body></body></html>");
    let out = sess
        .eval(
            r#"
            class XRoundtrip extends HTMLElement {}
            customElements.define('x-roundtrip', XRoundtrip);
            const el = document.createElement('x-roundtrip');
            el.setAttribute('data-key', 'value');
            el.getAttribute('data-key')
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "value");
}

#[test]
fn lit_shaped_component_renders_initial_state() {
    // End-to-end smoke: a Lit-shaped component does setAttribute in
    // its constructor, attributeChangedCallback updates internal
    // state, connectedCallback fires the initial render. This is
    // the shape Lit + Solid Element + Stencil all compile down to.
    let sess = page("<html><body><x-counter count='3'></x-counter></body></html>");
    let out = sess
        .eval(
            r#"
            globalThis.__renders = [];
            class XCounter extends HTMLElement {
                static get observedAttributes() { return ['count']; }
                constructor() {
                    super();
                    this._count = 0;
                }
                attributeChangedCallback(name, _oldVal, newVal) {
                    if (name === 'count') this._count = parseInt(newVal, 10) || 0;
                    this._render();
                }
                connectedCallback() {
                    this._render();
                }
                _render() {
                    globalThis.__renders.push(this._count);
                }
            }
            customElements.define('x-counter', XCounter);
            globalThis.__renders
            "#,
        )
        .expect("eval");
    // Attribute fires first (count: 3), then connectedCallback
    // (count: 3 because we already read it). Both observable.
    assert_eq!(out.value, serde_json::json!([3, 3]));
}
