//! Integration tests for Shadow DOM per WHATWG DOM §4.8 "Shadow DOM
//! interfaces". Pinned by the V4 agent-driven probe (May 2026) that
//! discovered `customElements`, `HTMLElement`, `Element`, `Node`,
//! `DocumentFragment`, and `ShadowRoot` were all `undefined` on
//! `globalThis` — blocking every web-component framework on first
//! definition. WC-Core lands the customElements + HTMLElement +
//! lifecycle half; this file covers the Shadow DOM half:
//!
//! - `Element.prototype.attachShadow({ mode })` returns a `ShadowRoot`
//!   that extends `DocumentFragment`. Spec:
//!   <https://dom.spec.whatwg.org/#dom-element-attachshadow>.
//! - `Element.prototype.shadowRoot` returns the root for open mode,
//!   `null` for closed (spec privacy gate). Spec:
//!   <https://dom.spec.whatwg.org/#dom-element-shadowroot>.
//! - The shadow tree is isolated from the host's light tree:
//!   `host.shadowRoot.innerHTML = '…'` does NOT affect
//!   `host.childNodes`, and vice-versa. Spec: §4.8 "Shadow Trees".
//! - `shadowRoot.querySelector` is scoped to the shadow subtree, not
//!   the host's descendants.
//! - `<slot>` elements expose `assignedElements()` / `assignedNodes()`
//!   that surface light-tree children matching the slot's `name`
//!   attribute (default slot → unattributed light children). Spec:
//!   <https://dom.spec.whatwg.org/#concept-slotable-assign>.
//! - `globalThis.ShadowRoot` / `globalThis.HTMLSlotElement` exposed as
//!   constructors; `new ShadowRoot()` throws "Illegal constructor"
//!   (only `attachShadow` creates them).
//!
//! OSS cross-referenced:
//! - **happy-dom** `ShadowRoot.ts`, `Element.ts::attachShadow`,
//!   `HTMLSlotElement.ts` — MIT, the principal reference. (jsdom does
//!   not implement Shadow DOM as of 2026.)
//!
//! Tests use a plain `<div>` host because WC-Core's customElements /
//! HTMLElement land in a sibling commit; Shadow DOM mechanics are
//! independent of custom-element registration.

use heso_engine_js::JsSession;
use url::Url;

/// Convenience base URL — matches the helper shape in `anchor_href.rs`.
fn u() -> Url {
    Url::parse("https://example.com/").unwrap()
}

/// Open a fresh session over a body fragment with no scripts.
fn sess(body: &str) -> JsSession {
    let html = format!("<!doctype html><html><body>{body}</body></html>");
    let (s, _) = JsSession::open(&html, u()).expect("open session");
    s
}

// =====================================================================
// attachShadow + ShadowRoot prototype chain
// =====================================================================

#[test]
fn attach_shadow_returns_shadow_root_extending_document_fragment() {
    // §4.8 attachShadow: returns a `ShadowRoot` that is a
    // `DocumentFragment`. Both `instanceof` checks must pass.
    let s = sess(r#"<div id="host"></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            JSON.stringify({
                isShadowRoot: root instanceof ShadowRoot,
                isFragment: root instanceof DocumentFragment,
                mode: root.mode,
                hostIsEl: root.host === host,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string result");
    assert!(s.contains("\"isShadowRoot\":true"), "got: {s}");
    assert!(s.contains("\"isFragment\":true"), "got: {s}");
    assert!(s.contains("\"mode\":\"open\""), "got: {s}");
    assert!(s.contains("\"hostIsEl\":true"), "got: {s}");
}

#[test]
fn attach_shadow_twice_throws_not_supported_error() {
    // §4.8: "If this is a shadow host, then throw a NotSupportedError
    // DOMException." A second attachShadow on the same element is a
    // hard error, not a silent reuse.
    let s = sess(r#"<div id="host"></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            host.attachShadow({ mode: 'open' });
            let threw = false;
            let name = '';
            try {
                host.attachShadow({ mode: 'open' });
            } catch (e) {
                threw = true;
                name = e && e.name ? e.name : '';
            }
            JSON.stringify({ threw, name })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string result");
    assert!(s.contains("\"threw\":true"), "got: {s}");
    assert!(s.contains("\"name\":\"NotSupportedError\""), "got: {s}");
}

// =====================================================================
// Element.shadowRoot — open vs closed visibility
// =====================================================================

#[test]
fn element_shadow_root_returns_root_when_mode_open() {
    // §4.8 shadowRoot getter: returns the shadow root iff mode is
    // 'open'. The most common pattern (and the one every web-component
    // tutorial uses) opens the root.
    let s = sess(r#"<div id="host"></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            host.shadowRoot === root
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value, true);
}

#[test]
fn element_shadow_root_returns_null_when_mode_closed() {
    // §4.8 shadowRoot getter: returns null for closed mode. This is
    // the spec privacy gate — closed roots are accessible only via
    // the internal handle returned by attachShadow.
    let s = sess(r#"<div id="host"></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'closed' });
            JSON.stringify({
                externalRoot: host.shadowRoot,
                internalRootDefined: typeof root !== 'undefined',
                internalRootIsShadow: root instanceof ShadowRoot,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string result");
    // host.shadowRoot must serialize to null for closed.
    assert!(s.contains("\"externalRoot\":null"), "got: {s}");
    assert!(s.contains("\"internalRootDefined\":true"), "got: {s}");
    assert!(s.contains("\"internalRootIsShadow\":true"), "got: {s}");
}

// =====================================================================
// Light / shadow tree isolation
// =====================================================================

#[test]
fn shadow_root_inner_html_does_not_affect_host_light_tree() {
    // §4.8 Shadow Trees: the shadow tree is a separate subtree.
    // Writing to shadowRoot.innerHTML must not mutate host.childNodes;
    // querying shadowRoot.querySelector must find the new node.
    let s = sess(r#"<div id="host"></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            root.innerHTML = '<p id="x">shadow</p>';
            const lightCount = host.childNodes.length;
            const shadowP = root.querySelector('p');
            JSON.stringify({
                lightCount,
                shadowPText: shadowP ? shadowP.textContent : null,
                shadowPId: shadowP ? shadowP.id : null,
                shadowInner: root.innerHTML,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"lightCount\":0"), "got: {s}");
    assert!(s.contains("\"shadowPText\":\"shadow\""), "got: {s}");
    assert!(s.contains("\"shadowPId\":\"x\""), "got: {s}");
    assert!(s.contains("<p"), "shadow innerHTML reflects mutation: {s}");
}

#[test]
fn host_inner_html_does_not_affect_shadow_tree() {
    // Inverse of the previous test: writing to host.innerHTML
    // replaces light-tree children, but the shadow tree is untouched.
    let s = sess(r#"<div id="host"></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            root.innerHTML = '<p id="shadow-p">shadow</p>';
            host.innerHTML = '<span id="light-s">light</span>';
            JSON.stringify({
                lightChildCount: host.children.length,
                lightFirstId: host.children[0] ? host.children[0].id : null,
                shadowFirstId: root.children[0] ? root.children[0].id : null,
                shadowQuery: root.querySelector('p') ? 'found' : 'none',
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"lightChildCount\":1"), "got: {s}");
    assert!(s.contains("\"lightFirstId\":\"light-s\""), "got: {s}");
    assert!(s.contains("\"shadowFirstId\":\"shadow-p\""), "got: {s}");
    assert!(s.contains("\"shadowQuery\":\"found\""), "got: {s}");
}

#[test]
fn shadow_root_query_selector_is_scoped_to_shadow_tree() {
    // The "scoped query" property is what makes Shadow DOM useful for
    // encapsulation. A `<p>` in the light tree must NOT be returned
    // by `shadowRoot.querySelector('p')` if there's no matching `<p>`
    // in the shadow tree.
    let s = sess(r#"<div id="host"><p id="light-p">in light</p></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            // Only one <p> exists in the shadow tree.
            root.innerHTML = '<span id="shadow-span">shadow</span>';
            JSON.stringify({
                lightP: host.querySelector('p') ? host.querySelector('p').id : null,
                shadowP: root.querySelector('p'),  // null
                shadowSpan: root.querySelector('span') ? root.querySelector('span').id : null,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"lightP\":\"light-p\""), "got: {s}");
    // shadowRoot.querySelector('p') must NOT find the light-tree <p>.
    assert!(s.contains("\"shadowP\":null"), "got: {s}");
    assert!(s.contains("\"shadowSpan\":\"shadow-span\""), "got: {s}");
}

// =====================================================================
// Slot assignment
// =====================================================================

#[test]
fn default_slot_assigns_unattributed_light_children() {
    // §4.8 slot assignment: a `<slot>` without a `name` attribute (or
    // with name="") collects every light-tree child of the host that
    // does NOT have a `slot=` attribute.
    let s = sess(r#"<div id="host"><p>light A</p><span>light B</span></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            root.innerHTML = '<div><slot></slot></div>';
            const slot = root.querySelector('slot');
            const assigned = slot.assignedElements();
            JSON.stringify({
                assignedCount: assigned.length,
                tags: assigned.map(e => e.tagName),
                slotIsHtmlSlot: slot instanceof HTMLSlotElement,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"assignedCount\":2"), "got: {s}");
    assert!(s.contains("\"tags\":[\"P\",\"SPAN\"]"), "got: {s}");
    assert!(s.contains("\"slotIsHtmlSlot\":true"), "got: {s}");
}

#[test]
fn named_slot_assigns_only_matching_light_children() {
    // §4.8: `<slot name="foo">` collects light-tree children with
    // matching `slot="foo"`. Non-matching children stay unassigned.
    let s = sess(
        r#"<div id="host">
            <p slot="title">my title</p>
            <p slot="body">my body</p>
            <p>unattributed</p>
        </div>"#,
    );
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            root.innerHTML = '<header><slot name="title"></slot></header>'
                           + '<main><slot></slot></main>'
                           + '<footer><slot name="body"></slot></footer>';
            const slots = Array.from(root.querySelectorAll('slot'));
            const result = slots.map(s => ({
                name: s.name || '(default)',
                assigned: s.assignedElements().map(e => e.textContent.trim()),
            }));
            JSON.stringify(result)
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"name\":\"title\""), "got: {s}");
    assert!(s.contains("\"my title\""), "got: {s}");
    assert!(s.contains("\"name\":\"body\""), "got: {s}");
    assert!(s.contains("\"my body\""), "got: {s}");
    assert!(s.contains("\"name\":\"(default)\""), "got: {s}");
    assert!(s.contains("\"unattributed\""), "got: {s}");
}

#[test]
fn slot_change_event_fires_when_assignment_changes() {
    // §4.8: "signal a slot change" runs on mutation. The slotchange
    // event fires on the slot when a new child is assigned. We fire
    // synchronously (best-effort; real browsers queue at microtask).
    let s = sess(r#"<div id="host"></div>"#);
    let out = s
        .eval(
            r#"
            const host = document.getElementById('host');
            const root = host.attachShadow({ mode: 'open' });
            root.innerHTML = '<slot></slot>';
            const slot = root.querySelector('slot');
            globalThis.slotChangeCount = 0;
            slot.addEventListener('slotchange', () => {
                globalThis.slotChangeCount += 1;
            });
            const newKid = document.createElement('p');
            newKid.textContent = 'new';
            host.appendChild(newKid);
            globalThis.slotChangeCount
            "#,
        )
        .expect("eval ok");
    assert!(
        out.value.as_u64().unwrap_or(0) >= 1,
        "slotchange should fire at least once after appendChild; got {:?}",
        out.value
    );
}

// =====================================================================
// Global constructors
// =====================================================================

#[test]
fn global_this_shadow_root_throws_illegal_constructor() {
    // Per WHATWG: `ShadowRoot` is exposed as a constructor for
    // `instanceof` checks but `new ShadowRoot()` throws. Real
    // browsers throw a TypeError "Illegal constructor".
    let s = sess("");
    let out = s
        .eval(
            r#"
            let threw = false;
            let msg = '';
            try {
                new ShadowRoot();
            } catch (e) {
                threw = true;
                msg = String(e && e.message ? e.message : e);
            }
            JSON.stringify({ threw, illegal: msg.toLowerCase().includes('illegal') })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"threw\":true"), "got: {s}");
    assert!(s.contains("\"illegal\":true"), "got: {s}");
}

#[test]
fn global_this_html_slot_element_is_exposed() {
    // `HTMLSlotElement` must be a defined constructor on the global
    // so framework `instanceof` checks succeed. We don't allow
    // `new HTMLSlotElement()`; the type only exists for the
    // instanceof side.
    let s = sess("");
    let out = s
        .eval("typeof HTMLSlotElement === 'function'")
        .expect("eval ok");
    assert_eq!(out.value, true);
}

#[test]
fn global_this_document_fragment_is_exposed() {
    // `DocumentFragment` must also be a defined constructor so the
    // `instanceof DocumentFragment` check on `ShadowRoot` works.
    let s = sess("");
    let out = s
        .eval("typeof DocumentFragment === 'function'")
        .expect("eval ok");
    assert_eq!(out.value, true);
}
