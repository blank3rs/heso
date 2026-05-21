//! Web Components: `customElements`, `HTMLElement`, and the upgrade
//! lifecycle (WHATWG HTML §4.13 "Custom elements").
//!
//! ## What this module gives you
//!
//! - `globalThis.Element` / `Document` / `DocumentFragment` / `Node` /
//!   `DomTokenList` — constructors that throw `TypeError: Illegal
//!   constructor` when called from user code, but whose `.prototype`
//!   is the real shared prototype that backs every Rust-side instance.
//!   Makes `el instanceof Element`, `doc instanceof Document`, etc.
//!   evaluate truthfully — frameworks gate on these constantly.
//!
//! - `globalThis.HTMLElement` — a subclassable constructor. Bare
//!   `new HTMLElement()` throws "Illegal constructor". `class MyEl
//!   extends HTMLElement { ... }` + `customElements.define('my-el',
//!   MyEl)` + `document.createElement('my-el')` works: the user's
//!   `super()` returns a real [`crate::dom::Element`] wrapper whose
//!   `[[Prototype]]` has been re-pointed at `MyEl.prototype`, so the
//!   user's class methods are reachable on the same object that
//!   already has every DOM method via the shared Element prototype.
//!
//! - `globalThis.customElements` — a [`CustomElementRegistry`][reg]
//!   instance with `define(name, ctor, options?)`, `get(name)`,
//!   `whenDefined(name)`, `getName(ctor)`, and `upgrade(node)`.
//!
//! - Lifecycle callbacks: `connectedCallback()` fires when the
//!   element enters the document tree (either via the define-time
//!   upgrade walk, via `document.createElement` + `appendChild`, or
//!   via `innerHTML` parsing). `disconnectedCallback()` fires when
//!   the element is removed. `attributeChangedCallback(name, old,
//!   new)` fires for each name in the class's `static get
//!   observedAttributes` list when `setAttribute` / `removeAttribute`
//!   changes it.
//!
//! ## Implementation strategy
//!
//! All the logic lives in pure JS, installed once per [`JsEngine`]
//! at construction time. The JS reads the rquickjs-managed
//! prototypes off two hidden globals
//! (`__hesoElementProto`, `__hesoDocumentProto`,
//! `__hesoDomTokenListProto`) populated from Rust right before the
//! script runs.
//!
//! ### Why JS, not Rust
//!
//! The construction-stack dance (see WHATWG HTML §4.13.4 "Upgrading
//! a custom element" steps 8-9, and §4.13.5.1 "HTMLElement
//! constructor" steps 7-9) requires `Reflect.construct` and `new.
//! target.prototype` introspection. Doing this from Rust would
//! mean re-entering JS for every `super()` call and the upgrade
//! semantics are easier to audit when they live in one inspectable
//! bootstrap string. The Rust side keeps the `Element` /
//! `Document` types and their DOM methods — JS just wraps the
//! prototype with subclassable, spec-shaped globals.
//!
//! ### Lifecycle wrapping
//!
//! `connectedCallback` / `disconnectedCallback` fire from JS-side
//! wrappers around `Element.prototype.appendChild`, `insertBefore`,
//! `removeChild`, `remove`, and the `innerHTML` setter. Each
//! wrapper:
//!
//! 1. Captures `isConnected` state before the mutation.
//! 2. Calls the original (Rust-backed) implementation.
//! 3. Walks the affected subtree and fires `connectedCallback`
//!    on any registered custom elements that transitioned from
//!    detached to connected (or vice versa for disconnected).
//!
//! `attributeChangedCallback` fires from wrappers around
//! `Element.prototype.setAttribute` and `removeAttribute`.
//!
//! ## OSS references cross-checked
//!
//! - [happy-dom][hd] `CustomElementRegistry.ts` (MIT) — for the
//!   `whenDefined` promise-bucket pattern and the `getName` reverse
//!   lookup via a parallel `WeakMap`-keyed-by-class registry.
//! - [jsdom][jd] `custom-elements.js` (MIT) — for the construction
//!   stack with "already constructed" sentinel value (HTML spec §
//!   4.13.4 / §4.13.5.1).
//! - [deno][dn] `04_global_interfaces.js` (MIT) — for the
//!   `Illegal constructor` pattern via a hidden secret-key symbol.
//!
//! We don't vendor those — the algorithms are short and the spec
//! work they did was the value. The Rust+QuickJS binding model is
//! different from happy-dom's JS-in-JS approach.
//!
//! [reg]: https://html.spec.whatwg.org/multipage/custom-elements.html#customelementregistry
//! [hd]: https://github.com/capricorn86/happy-dom/blob/master/packages/happy-dom/src/custom-element/CustomElementRegistry.ts
//! [jd]: https://github.com/jsdom/jsdom/blob/master/lib/jsdom/living/helpers/custom-elements.js
//! [dn]: https://github.com/denoland/deno/blob/main/ext/web/04_global_interfaces.js

use rquickjs::{Class, Context};

use crate::{
    dom::{Document, DomTokenList, Element},
    engine::EvalError,
};

/// Install `customElements`, `HTMLElement`, and the illegal-
/// constructor shims for `Element` / `Document` / `DocumentFragment` /
/// `Node` / `DomTokenList` on `ctx.globals()`.
///
/// Must be called **after** [`crate::dom::register_classes`] so the
/// rquickjs-managed prototypes for [`Element`], [`Document`], and
/// [`DomTokenList`] are already inserted in the runtime's prototype
/// table. Idempotent: a second call re-publishes the bootstrap (the
/// JS guards against rebinding via a sentinel).
pub(crate) fn install_custom_elements(context: &Context) -> Result<(), EvalError> {
    use rquickjs::CatchResultExt;
    context
        .with(|ctx| -> Result<(), EvalError> {
            // Pull the rquickjs-managed prototypes out of the
            // runtime opaque table and stash them on hidden
            // globals so the JS bootstrap can read them.
            //
            // `Class::<X>::prototype` lazily inserts the prototype
            // on first call (per rquickjs 0.11 source) so order
            // doesn't matter here — we get a real Object even if
            // the host hasn't created any instances yet.
            let element_proto = Class::<Element>::prototype(&ctx)
                .map_err(|e| EvalError::Engine(format!("Element prototype: {e}")))?
                .expect("Element prototype must be registered before install_custom_elements");
            let document_proto = Class::<Document>::prototype(&ctx)
                .map_err(|e| EvalError::Engine(format!("Document prototype: {e}")))?
                .expect("Document prototype must be registered before install_custom_elements");
            let domtokenlist_proto = Class::<DomTokenList>::prototype(&ctx)
                .map_err(|e| EvalError::Engine(format!("DomTokenList prototype: {e}")))?
                .expect("DomTokenList prototype must be registered before install_custom_elements");

            let globals = ctx.globals();
            globals
                .set("__hesoElementProto", element_proto)
                .map_err(|e| EvalError::Engine(format!("stash element proto: {e}")))?;
            globals
                .set("__hesoDocumentProto", document_proto)
                .map_err(|e| EvalError::Engine(format!("stash document proto: {e}")))?;
            globals
                .set("__hesoDomTokenListProto", domtokenlist_proto)
                .map_err(|e| EvalError::Engine(format!("stash domtokenlist proto: {e}")))?;

            ctx.eval::<(), _>(CUSTOM_ELEMENTS_BOOTSTRAP)
                .catch(&ctx)
                .map_err(|e| EvalError::Engine(format!("eval custom-elements bootstrap: {e}")))?;

            // Wipe the hidden globals so user JS can't poke at the
            // internal prototype objects directly. The closure over
            // them inside the bootstrap captures the references it
            // needs; the globals are no longer load-bearing.
            let _ = globals.remove("__hesoElementProto");
            let _ = globals.remove("__hesoDocumentProto");
            let _ = globals.remove("__hesoDomTokenListProto");
            Ok(())
        })?;
    Ok(())
}

/// JS bootstrap for the web-components surface. Runs once per
/// engine. Reads the rquickjs-managed prototypes off the
/// `__hesoElementProto` / `__hesoDocumentProto` /
/// `__hesoDomTokenListProto` hidden globals (set by
/// [`install_custom_elements`] and removed right after).
///
/// Source-of-record references:
///
/// - WHATWG DOM § 4.4 "Interface Element" — the constructor of
///   `Element` is illegal to call from user code.
/// - WHATWG HTML § 4.13 "Custom elements" — the registry, the
///   valid-name regex, the upgrade algorithm, and the lifecycle
///   callbacks.
/// - WHATWG HTML § 4.13.5.1 "HTMLElement constructor" — the
///   construction-stack dance that lets `super()` return the
///   pre-allocated Element from the upgrade pass.
const CUSTOM_ELEMENTS_BOOTSTRAP: &str = r#"
(function() {
    if (globalThis.__hesoCustomElementsInstalled) return;

    // ---------------------------------------------------------------
    // Capture the rquickjs-managed prototypes BEFORE we expose
    // anything: the Rust caller installs them under hidden globals
    // and wipes them immediately after this script returns.
    // ---------------------------------------------------------------
    var elementProto = globalThis.__hesoElementProto;
    var documentProto = globalThis.__hesoDocumentProto;
    var domTokenListProto = globalThis.__hesoDomTokenListProto;
    if (!elementProto || !documentProto || !domTokenListProto) {
        throw new Error('heso: custom-elements bootstrap missing prototype handles');
    }

    // ---------------------------------------------------------------
    // `Illegal constructor` factory.
    //
    // Per WHATWG (DOM §4.4 "Interface Element", HTML §4.13.5.1
    // "HTMLElement constructor" step 1), calling these constructors
    // from user code must throw a TypeError. We expose a function
    // whose `.prototype` is the actual shared prototype so
    // `instanceof` works. The function body throws on direct call.
    // ---------------------------------------------------------------
    function makeIllegalConstructor(name, proto) {
        var fn = function() {
            throw new TypeError(name + ': Illegal constructor');
        };
        // `.prototype` is normally non-configurable on a fn ctor; we
        // overwrite via Object.defineProperty so `instanceof` walks
        // into the real Rust-side prototype.
        Object.defineProperty(fn, 'prototype', {
            value: proto,
            writable: false,
            configurable: false,
            enumerable: false,
        });
        Object.defineProperty(fn, 'name', {
            value: name,
            configurable: true,
        });
        // Spec: `Element.prototype.constructor === Element`. We
        // can't generally write to a prototype owned by the Rust
        // side, but Object.defineProperty on a non-frozen object
        // works. Wrap in try/catch in case the proto is sealed.
        try {
            Object.defineProperty(proto, 'constructor', {
                value: fn,
                writable: true,
                configurable: true,
                enumerable: false,
            });
        } catch (e) { /* ignore — `instanceof` still works */ }
        return fn;
    }

    // ---------------------------------------------------------------
    // Node — common ancestor for Element / Document / DocumentFragment
    // / Text / Comment. heso doesn't separate these into distinct
    // Rust types (Phase 1B keeps one Element wrapper for every node
    // type — see dom.rs); the JS-side Node constructor is a stub
    // whose `.prototype` is shared with Element so `node instanceof
    // Node` works for every node-typed wrapper.
    //
    // Spec: https://dom.spec.whatwg.org/#interface-node
    // ---------------------------------------------------------------
    var Node = makeIllegalConstructor('Node', elementProto);
    // Node-type constants per DOM §4.4.
    var NODE_CONSTANTS = {
        ELEMENT_NODE: 1,
        ATTRIBUTE_NODE: 2,
        TEXT_NODE: 3,
        CDATA_SECTION_NODE: 4,
        ENTITY_REFERENCE_NODE: 5,
        ENTITY_NODE: 6,
        PROCESSING_INSTRUCTION_NODE: 7,
        COMMENT_NODE: 8,
        DOCUMENT_NODE: 9,
        DOCUMENT_TYPE_NODE: 10,
        DOCUMENT_FRAGMENT_NODE: 11,
        NOTATION_NODE: 12,
    };
    for (var k in NODE_CONSTANTS) {
        Object.defineProperty(Node, k, {
            value: NODE_CONSTANTS[k],
            writable: false,
            enumerable: true,
            configurable: false,
        });
    }
    globalThis.Node = Node;

    // ---------------------------------------------------------------
    // Element — shares the rquickjs Element prototype.
    // ---------------------------------------------------------------
    var Element = makeIllegalConstructor('Element', elementProto);
    globalThis.Element = Element;

    // ---------------------------------------------------------------
    // Document — shares the rquickjs Document prototype.
    // ---------------------------------------------------------------
    var Document = makeIllegalConstructor('Document', documentProto);
    globalThis.Document = Document;

    // ---------------------------------------------------------------
    // DocumentFragment — Phase 1B doesn't have a separate Rust type
    // for fragments, so we share the Element prototype. Real
    // documents do round-trip parsing through fragments
    // (innerHTML setter uses one internally), and frameworks
    // feature-detect via `instanceof DocumentFragment`. Giving them
    // a constructor whose `.prototype` is in the Element chain
    // makes the feature-detect succeed for any node that has been
    // attached to a fragment-shaped subtree. heso uses the same
    // wrapper for both, so we trade off strict instanceof precision
    // for surface coverage — frameworks call `.appendChild` on
    // both, never branch on the difference.
    //
    // Bare `new DocumentFragment()` is legal per WHATWG DOM (§4.7).
    // We provide a callable form that returns a real orphan node.
    //
    // Idempotent: the `dom` module's Shadow DOM bootstrap installs its
    // own DocumentFragment first and chains
    // `ShadowRoot.prototype.__proto__ = DocumentFragment.prototype`. If
    // we overwrite it here, the chain breaks and `root instanceof
    // DocumentFragment` returns false for shadow roots. Skip when the
    // global is already populated.
    // ---------------------------------------------------------------
    if (typeof globalThis.DocumentFragment === 'undefined') {
        var DocumentFragment = function DocumentFragment() {
            if (typeof document === 'undefined' || !document.createDocumentFragment) {
                throw new TypeError('DocumentFragment: no document available');
            }
            return document.createDocumentFragment();
        };
        Object.defineProperty(DocumentFragment, 'prototype', {
            value: elementProto,
            writable: false,
            configurable: false,
            enumerable: false,
        });
        Object.defineProperty(DocumentFragment, 'name', { value: 'DocumentFragment', configurable: true });
        globalThis.DocumentFragment = DocumentFragment;
    }

    // ---------------------------------------------------------------
    // DOMTokenList — shares the rquickjs DomTokenList prototype.
    // The spec name is DOMTokenList (all caps for the acronym); the
    // Rust class registers under the camelCase form. Bind both.
    // ---------------------------------------------------------------
    var DOMTokenList = makeIllegalConstructor('DOMTokenList', domTokenListProto);
    globalThis.DOMTokenList = DOMTokenList;
    if (typeof globalThis.DomTokenList === 'undefined') {
        // Belt-and-suspenders: a few framework feature-detects look
        // for the camelCase spelling that rquickjs emitted under
        // older codepaths. Keeping both names eliminates a foot-gun.
        globalThis.DomTokenList = DOMTokenList;
    }

    // ---------------------------------------------------------------
    // HTMLTemplateElement
    //
    // Spec: WHATWG HTML §4.12.3 "interface HTMLTemplateElement". Bare
    // `new HTMLTemplateElement()` throws "Illegal constructor". The
    // interface gates `el instanceof HTMLTemplateElement` for any
    // `<template>` Element — Lit / Material Web / shoelace feature-
    // detect this surface heavily during boot, and a ReferenceError
    // on the global halts module evaluation.
    //
    // Phase 1B doesn't have a separate Rust type for templates, so
    // the constructor's `.prototype` is the shared Element prototype
    // and `Symbol.hasInstance` discriminates by tag name. The
    // load-bearing surface is:
    //   - `template instanceof HTMLTemplateElement`
    //   - `template.content` returns something with
    //     `.querySelector`, `.children`, `.cloneNode(true)`.
    //
    // `.content` punt: per WHATWG HTML §4.12.3 the property returns
    // a `DocumentFragment` holding the template's children
    // (off-document). heso doesn't have a real off-document
    // DocumentFragment node, so `.content` returns the template
    // element itself. That preserves the load-bearing methods
    // (`querySelector`, `children`, `firstElementChild`,
    // `cloneNode(true)`) without splitting the children out into a
    // distinct fragment node. Lit's pattern
    // `document.importNode(template.content, true)` doesn't apply
    // (heso has no `importNode`), but `template.content.cloneNode(true)`
    // works because the cloned root carries the parsed children.
    // ---------------------------------------------------------------
    var HTMLTemplateElement = makeIllegalConstructor('HTMLTemplateElement', elementProto);
    try {
        Object.defineProperty(HTMLTemplateElement, Symbol.hasInstance, {
            value: function(instance) {
                if (!instance || typeof instance !== 'object') return false;
                try {
                    var ln = instance.localName;
                    return typeof ln === 'string' && ln.toLowerCase() === 'template';
                } catch (e) { return false; }
            },
            writable: false,
            enumerable: false,
            configurable: true,
        });
    } catch (e) { /* leave default proto-chain instanceof */ }
    globalThis.HTMLTemplateElement = HTMLTemplateElement;

    // ---------------------------------------------------------------
    // HTML*Element subclass family — bug-report 03 P1 / bug-report 01
    // P0 cluster.
    //
    // Real-world repros:
    //   - linear.app:  webpack chunk does `instanceof HTMLScriptElement`
    //                  during bootstrap to detect its own runtime
    //   - docs.rs/serde: `instanceof HTMLLinkElement` from a navigation
    //                    shim
    //   - cloudflare.com: hero-video hydration uses
    //                    `instanceof HTMLVideoElement`
    //   - slack.com:  lazy-loader uses `instanceof HTMLImageElement`
    //   - theguardian.com: framework code uses `instanceof NodeList`
    //
    // Each constructor:
    //   1. Throws `TypeError: Illegal constructor` on direct `new`
    //      (`makeIllegalConstructor`).
    //   2. Shares `Element.prototype` so `el instanceof HTMLXxxElement`
    //      walks the prototype chain successfully when fast paths via
    //      `hasInstance` don't trip.
    //   3. Overrides `Symbol.hasInstance` to return true when the
    //      tested object's `tagName` matches one of the spec's mapped
    //      HTML elements (e.g. HTMLAnchorElement matches `<a>`,
    //      HTMLInputElement matches `<input>`, HTMLImageElement
    //      matches `<img>`).
    //
    // Why `hasInstance` instead of separate prototypes: heso has one
    // Rust-side Element type (phase 1B punt). Separate prototype
    // chains would require either splitting that into multiple
    // `#[rquickjs::class]` types or re-pointing `__proto__` per node
    // — both bigger lifts than needed for the load-bearing surface,
    // which is *just* the instanceof check.
    //
    // Spec refs:
    //   <https://html.spec.whatwg.org/multipage/dom.html#interface-htmlelement>
    //   <https://html.spec.whatwg.org/multipage/sections.html#htmldivelement>
    //   …one row per WHATWG HTML interface section.
    // ---------------------------------------------------------------
    //
    // tag-name -> constructor-name map. Source: HTML spec § elements.
    // Each entry: [interface-name, tag-name (lowercase, the spec's
    // localName for the element)]. For interfaces that match multiple
    // tags (HTMLTableSectionElement → thead/tbody/tfoot; HTMLAnchor /
    // HTMLAreaElement single each; HTMLQuoteElement → blockquote/q;
    // HTMLTableCellElement → td/th), we pass a list.
    var HTML_INTERFACES = [
        ['HTMLDivElement',       ['div']],
        ['HTMLSpanElement',      ['span']],
        ['HTMLAnchorElement',    ['a']],
        ['HTMLAreaElement',      ['area']],
        ['HTMLButtonElement',    ['button']],
        ['HTMLInputElement',     ['input']],
        ['HTMLTextAreaElement',  ['textarea']],
        ['HTMLSelectElement',    ['select']],
        ['HTMLOptionElement',    ['option']],
        ['HTMLOptGroupElement',  ['optgroup']],
        ['HTMLLabelElement',     ['label']],
        ['HTMLFormElement',      ['form']],
        ['HTMLFieldSetElement',  ['fieldset']],
        ['HTMLLegendElement',    ['legend']],
        ['HTMLOutputElement',    ['output']],
        ['HTMLProgressElement',  ['progress']],
        ['HTMLMeterElement',     ['meter']],
        ['HTMLDataListElement',  ['datalist']],
        ['HTMLImageElement',     ['img']],
        ['HTMLPictureElement',   ['picture']],
        ['HTMLSourceElement',    ['source']],
        ['HTMLVideoElement',     ['video']],
        ['HTMLAudioElement',     ['audio']],
        ['HTMLMediaElement',     ['video', 'audio']], // abstract base for video+audio
        ['HTMLCanvasElement',    ['canvas']],
        ['HTMLIFrameElement',    ['iframe']],
        ['HTMLEmbedElement',     ['embed']],
        ['HTMLObjectElement',    ['object']],
        ['HTMLParamElement',     ['param']],
        ['HTMLScriptElement',    ['script']],
        ['HTMLStyleElement',     ['style']],
        ['HTMLLinkElement',      ['link']],
        ['HTMLMetaElement',      ['meta']],
        ['HTMLTitleElement',     ['title']],
        ['HTMLBaseElement',      ['base']],
        ['HTMLHeadElement',      ['head']],
        ['HTMLBodyElement',      ['body']],
        ['HTMLHtmlElement',      ['html']],
        ['HTMLUListElement',     ['ul']],
        ['HTMLOListElement',     ['ol']],
        ['HTMLLIElement',        ['li']],
        ['HTMLDListElement',     ['dl']],
        ['HTMLParagraphElement', ['p']],
        ['HTMLPreElement',       ['pre']],
        ['HTMLQuoteElement',     ['blockquote', 'q']],
        ['HTMLHRElement',        ['hr']],
        ['HTMLBRElement',        ['br']],
        ['HTMLHeadingElement',   ['h1', 'h2', 'h3', 'h4', 'h5', 'h6']],
        ['HTMLTableElement',     ['table']],
        ['HTMLTableRowElement',  ['tr']],
        ['HTMLTableCellElement', ['td', 'th']],
        ['HTMLTableSectionElement', ['thead', 'tbody', 'tfoot']],
        ['HTMLTableColElement',  ['col', 'colgroup']],
        ['HTMLTableCaptionElement', ['caption']],
        ['HTMLDialogElement',    ['dialog']],
        ['HTMLDetailsElement',   ['details']],
        ['HTMLMenuElement',      ['menu']],
        ['HTMLMapElement',       ['map']],
        ['HTMLLegendElement',    ['legend']],
        ['HTMLModElement',       ['ins', 'del']],
        ['HTMLTimeElement',      ['time']],
        ['HTMLDataElement',      ['data']],
        ['HTMLTrackElement',     ['track']],
    ];

    function defineHtmlSubclass(name, tagNamesLower) {
        // Avoid clobbering pre-existing globals (HTMLTemplateElement,
        // HTMLSlotElement are installed elsewhere with their own
        // hasInstance shape; idempotent re-install gates on existence).
        if (typeof globalThis[name] !== 'undefined') return;
        var ctor = makeIllegalConstructor(name, elementProto);
        try {
            Object.defineProperty(ctor, Symbol.hasInstance, {
                value: function(instance) {
                    if (!instance || typeof instance !== 'object') return false;
                    try {
                        // The Rust-side Element.tagName returns
                        // uppercase; localName is lowercase. Use
                        // localName for the comparison so spec
                        // discrimination matches WHATWG's per-element
                        // mappings (HTMLAnchorElement → "a", etc.).
                        var ln = instance.localName;
                        if (typeof ln !== 'string') return false;
                        var lnLower = ln.toLowerCase();
                        for (var i = 0; i < tagNamesLower.length; i++) {
                            if (tagNamesLower[i] === lnLower) return true;
                        }
                        return false;
                    } catch (e) { return false; }
                },
                writable: false,
                enumerable: false,
                configurable: true,
            });
        } catch (e) { /* leave default proto-chain instanceof */ }
        globalThis[name] = ctor;
    }

    for (var hi = 0; hi < HTML_INTERFACES.length; hi++) {
        defineHtmlSubclass(HTML_INTERFACES[hi][0], HTML_INTERFACES[hi][1]);
    }

    // NodeList — bug-report 01 P0 (cluster: HTMLLinkElement etc.).
    // theguardian.com hits `instanceof NodeList`. The interface is
    // technically a separate type from Array (querySelectorAll returns
    // a NodeList in real browsers), but our `querySelectorAll` returns
    // a plain array. Expose an `Illegal constructor` shim + a
    // `Symbol.hasInstance` that accepts any array-like object with
    // numeric `length`, so the guarded `if (x instanceof NodeList)`
    // branches don't ReferenceError.
    if (typeof globalThis.NodeList === 'undefined') {
        var NodeList = makeIllegalConstructor('NodeList', Object.create(Object.prototype));
        try {
            Object.defineProperty(NodeList, Symbol.hasInstance, {
                value: function(instance) {
                    if (instance == null) return false;
                    // Real Arrays returned from querySelectorAll.
                    if (Array.isArray(instance)) return true;
                    // Array-likes with numeric length.
                    return typeof instance === 'object'
                        && typeof instance.length === 'number';
                },
                writable: false,
                enumerable: false,
                configurable: true,
            });
        } catch (e) { /* */ }
        globalThis.NodeList = NodeList;
    }

    // HTMLCollection — same logic as NodeList; the .scripts /
    // .forms / .images / .links accessors return plain arrays.
    if (typeof globalThis.HTMLCollection === 'undefined') {
        var HTMLCollection = makeIllegalConstructor(
            'HTMLCollection',
            Object.create(Object.prototype)
        );
        try {
            Object.defineProperty(HTMLCollection, Symbol.hasInstance, {
                value: function(instance) {
                    if (instance == null) return false;
                    if (Array.isArray(instance)) return true;
                    return typeof instance === 'object'
                        && typeof instance.length === 'number';
                },
                writable: false,
                enumerable: false,
                configurable: true,
            });
        } catch (e) { /* */ }
        globalThis.HTMLCollection = HTMLCollection;
    }

    // ===============================================================
    // HTMLElement + customElements registry
    // ===============================================================

    // -----------------------------------------------------------------
    // Construction stack — WHATWG HTML §4.13 "concept-custom-element-
    // definition-construction-stack". Each entry is either an
    // already-created Element wrapper (pushed by the upgrade /
    // createElement path) or a sentinel string marking "this entry
    // has already been consumed by super()". A user who does
    // `super(); super();` should see the second `super()` throw, per
    // the spec's "already constructed marker" rule.
    // -----------------------------------------------------------------
    var ALREADY_CONSTRUCTED = { _heso: 'already_constructed' };
    var constructionStack = [];

    // -----------------------------------------------------------------
    // The HTMLElement constructor itself. Spec algorithm:
    //
    //   1. If NewTarget is undefined, throw TypeError.
    //   2. Let definition be the entry on the construction stack
    //      whose constructor === NewTarget. If none, throw TypeError
    //      (no entry → user called `new HTMLElement()` directly).
    //   3. If definition has been consumed, throw TypeError.
    //   4. Mark the entry as consumed and return the underlying
    //      Element. JS then reparents the returned object to
    //      NewTarget.prototype because we're returning it from a
    //      constructor with `new` — that's the ES spec's
    //      "return object" override.
    // -----------------------------------------------------------------
    function HTMLElement() {
        if (new.target === undefined) {
            throw new TypeError('HTMLElement: Illegal constructor (must be called with new)');
        }
        if (constructionStack.length === 0) {
            // Bare `new HTMLElement()` (or `new MyClass()` without
            // first being define()'d) — no Element waiting to be
            // adopted. WHATWG HTML §4.13.5.1 step 7: throw TypeError.
            throw new TypeError('HTMLElement: Illegal constructor');
        }
        var topIndex = constructionStack.length - 1;
        var top = constructionStack[topIndex];
        if (top === ALREADY_CONSTRUCTED) {
            // The user's class called super() twice. WHATWG HTML
            // §4.13.5.1 step 9: throw TypeError.
            throw new TypeError(
                "HTMLElement: this constructor has already been called (super() twice?)"
            );
        }
        // Mark consumed. The actual pop happens in the caller (define
        // / createElement) so a throwing user constructor leaves a
        // tombstone the caller can clean up.
        constructionStack[topIndex] = ALREADY_CONSTRUCTED;
        // Reparent to NewTarget.prototype (e.g. MyEl.prototype) so
        // user methods like connectedCallback resolve.
        try {
            Object.setPrototypeOf(top, new.target.prototype);
        } catch (e) { /* same proto already; no-op */ }
        return top;
    }
    // `HTMLElement.prototype` is itself a fresh object whose
    // [[Prototype]] is Element.prototype, so the chain is
    // MyEl.prototype → HTMLElement.prototype → Element.prototype →
    // ... → Object.prototype. That lets HTMLElement-specific
    // methods (none yet, but a placeholder for future surface) live
    // on HTMLElement.prototype without bleeding into bare Element
    // instances, while still letting `instanceof Element` succeed.
    var htmlElementProto = Object.create(elementProto);
    Object.defineProperty(htmlElementProto, 'constructor', {
        value: HTMLElement,
        writable: true,
        configurable: true,
        enumerable: false,
    });
    Object.defineProperty(HTMLElement, 'prototype', {
        value: htmlElementProto,
        writable: false,
        configurable: false,
        enumerable: false,
    });
    Object.defineProperty(HTMLElement, 'name', { value: 'HTMLElement', configurable: true });
    globalThis.HTMLElement = HTMLElement;

    // -----------------------------------------------------------------
    // Valid-custom-element-name check.
    //
    // WHATWG HTML §4.13.2 "Custom element name":
    //  - Must start with [a-z]
    //  - Must contain a U+002D HYPHEN-MINUS
    //  - All chars must be from the PCEN-char set (letters, digits,
    //    a small allow-list of punctuation, and a chunk of Unicode
    //    BMP ranges)
    //  - Must not be one of the reserved names.
    //
    // The exact PCEN regex is a copy of the happy-dom encoding
    // (capricorn86/happy-dom/src/custom-element/CustomElementUtility.ts);
    // they encode the spec's character classes directly. We treat
    // the regex as a literal here so the result matches happy-dom
    // and jsdom for every plausible name.
    // -----------------------------------------------------------------
    var PCEN_CHAR =
        '[-_.]|[0-9]|[a-z]|·|[À-Ö]|[Ø-ö]' +
        '|[ø-ͽ]|[Ϳ-῿]' +
        '|[‌-‍]|[‿-⁀]|[⁰-↏]' +
        '|[Ⰰ-⿯]|[、-퟿]' +
        '|[豈-﷏]|[ﷰ-�]';
    var PCEN_REGEXP = new RegExp('^[a-z](' + PCEN_CHAR + ')*-(' + PCEN_CHAR + ')*$');
    var RESERVED_NAMES = {
        'annotation-xml': true,
        'color-profile': true,
        'font-face': true,
        'font-face-src': true,
        'font-face-uri': true,
        'font-face-format': true,
        'font-face-name': true,
        'missing-glyph': true,
    };
    function isValidCustomElementName(name) {
        if (typeof name !== 'string') return false;
        if (RESERVED_NAMES[name]) return false;
        return PCEN_REGEXP.test(name);
    }

    // -----------------------------------------------------------------
    // Registry — backed by two parallel Maps:
    //
    //  - nameToDef: 'my-el' → { ctor, observedAttributes, ... }
    //  - ctorToName: WeakMap<class, name> for getName() reverse lookup
    //
    // `whenDefined(name)` returns a Promise resolved when `name` is
    // registered. We track callbacks under a separate Map so a name
    // resolved before its first define() can be ready immediately.
    // -----------------------------------------------------------------
    var nameToDef = Object.create(null);
    var ctorToName = new WeakMap();
    var whenDefinedCallbacks = Object.create(null);

    function getDefinitionForName(name) {
        return nameToDef[name] || null;
    }

    function getDefinitionForElement(el) {
        if (!el || typeof el.tagName !== 'string') return null;
        var lname = (el.localName || el.tagName).toLowerCase();
        return getDefinitionForName(lname);
    }

    // -----------------------------------------------------------------
    // Construct an instance of a user class on top of an existing
    // Element wrapper. Spec: WHATWG HTML §4.13.4 "Upgrade an element"
    // steps 8-9 plus §4.13.5.1.
    //
    // 1. Push `el` onto the construction stack.
    // 2. Call `Reflect.construct(ctor, [], ctor)`. Inside the user's
    //    `class MyEl extends HTMLElement { ... }`, `super()` invokes
    //    our HTMLElement function which pops the stack entry and
    //    returns it. JS-spec semantics: when a derived constructor
    //    returns an object, that object becomes `this` (so the rest
    //    of the user constructor body operates on the same Element).
    // 3. The result is the Element wrapper itself, now with
    //    [[Prototype]] === ctor.prototype.
    //
    // If the user's constructor throws, we still pop the stack so a
    // subsequent define() / createElement isn't left with a stale
    // entry, and we re-throw so the caller can decide whether to
    // swallow (define() upgrade walk) or surface (createElement).
    // -----------------------------------------------------------------
    function constructUpgradeOnto(el, ctor) {
        constructionStack.push(el);
        try {
            var result = Reflect.construct(ctor, [], ctor);
            // Per spec, the construction result MUST be the same
            // element. If user's constructor returned something
            // else (extremely uncommon, but possible), throw.
            if (result !== el) {
                throw new TypeError(
                    "customElements: user constructor returned a different object than the element being upgraded"
                );
            }
        } finally {
            constructionStack.pop();
        }
        return el;
    }

    // -----------------------------------------------------------------
    // Mark an element as "upgraded" so we don't double-upgrade if the
    // page parses then re-walks. We stash a non-enumerable flag on
    // the element wrapper that points at the definition.
    //
    // Note that document.querySelector('my-el') returns a FRESH
    // Element wrapper every call (see dom.rs PROP_NODE_LISTENERS
    // discussion) — so the flag rides on whatever wrapper happens
    // to be in play. We re-stamp on each upgrade attempt. The
    // ctorToName map gives us the durable "is this name registered"
    // check; the wrapper flag just prevents wasted work on a
    // wrapper we've already touched in this call.
    // -----------------------------------------------------------------
    var CE_DEFINITION = '__hesoCEDefinition';
    var CE_UPGRADED = '__hesoCEUpgraded';

    function markUpgraded(el, def) {
        try {
            Object.defineProperty(el, CE_DEFINITION, {
                value: def,
                writable: true,
                configurable: true,
                enumerable: false,
            });
            Object.defineProperty(el, CE_UPGRADED, {
                value: true,
                writable: true,
                configurable: true,
                enumerable: false,
            });
        } catch (e) { /* ignore */ }
    }

    function isUpgraded(el) {
        return el && el[CE_UPGRADED] === true;
    }

    // -----------------------------------------------------------------
    // Lazy prototype stamping for fresh wrappers.
    //
    // Why this exists: `document.querySelector(...)` and friends each
    // produce a brand-new JS wrapper around the same underlying
    // dom_query NodeId every call. WHATWG HTML §4.13.6.6 "upgrade an
    // element" step 8 changes the element's prototype to the
    // definition's interface prototype — but on heso that "element"
    // is the JS wrapper, and a different wrapper for the same node
    // would still have the bare `Element.prototype` chain. Without
    // stamping the prototype at materialisation, the third Shoelace
    // bootstrap test (`el instanceof SlButton` after the page has
    // <sl-button> elements present at parse time and the Shoelace
    // module is loaded after) returns false on every wrapper the
    // page hands to user JS, even though the registry knows the
    // mapping.
    //
    // `stampPrototypeIfRegistered(el)` is idempotent and cheap:
    // if the localName matches a registered definition AND the
    // wrapper's current [[Prototype]] differs, set it. Methods we
    // care about (`querySelector`, `querySelectorAll`, etc.) call
    // this on every wrapper they return.
    //
    // Reference: jsdom's `upgradeElement` runs once at first
    // observation thanks to stable wrapper identity (each NodeId
    // has exactly one JS wrapper for life). happy-dom takes the
    // same approach. heso's fresh-per-call wrapper model means we
    // have to stamp lazily; the work is trivial (one prototype
    // walk + one setPrototypeOf) so the cost amortizes.
    // -----------------------------------------------------------------
    function stampPrototypeIfRegistered(el) {
        if (!el || typeof el.localName !== 'string') return el;
        var lname = el.localName.toLowerCase();
        var def = nameToDef[lname];
        if (!def) return el;
        var target = def.ctor && def.ctor.prototype;
        if (!target) return el;
        try {
            var current = Object.getPrototypeOf(el);
            if (current !== target) {
                Object.setPrototypeOf(el, target);
            }
        } catch (e) { /* ignore — fall through with the original */ }
        return el;
    }

    function stampArrayPrototypes(arr) {
        if (!arr || typeof arr.length !== 'number') return arr;
        for (var i = 0; i < arr.length; i++) {
            stampPrototypeIfRegistered(arr[i]);
        }
        return arr;
    }

    // -----------------------------------------------------------------
    // Lifecycle invocation helpers.
    //
    // Heso quirk: every `document.querySelector(...)` returns a FRESH
    // [`Element`] wrapper for the underlying [`dom_query::Node`] —
    // wrapper identity isn't stable across calls. (See dom.rs
    // PROP_NODE_LISTENERS for the same trick on the events side.)
    // That means a wrapper observed at insert / remove time
    // generally does NOT have the user's class prototype on it; only
    // the wrapper returned by `document.createElement(name)` (which
    // we explicitly upgrade) has [[Prototype]] === ctor.prototype.
    //
    // To make lifecycle callbacks reliable, we look up the
    // user-defined method via the registered constructor's
    // prototype directly, then call it with `this` bound to the
    // wrapper we observed. The wrapper still backs the same node,
    // so any DOM ops the user's callback does land on the right
    // node — only the method-lookup needs the user-prototype
    // detour.
    //
    // Calls into user code MUST be defensively try/caught: a throw
    // from connectedCallback shouldn't halt parsing or block the
    // next callback in the same upgrade walk. (Spec
    // §4.13.4.3.1.invoke-custom-element-reactions step 4: "If this
    // throws an exception, then report the exception".)
    // -----------------------------------------------------------------
    function lookupCallbackOnDefinition(def, name) {
        if (!def || !def.ctor || !def.ctor.prototype) return null;
        try {
            var fn = def.ctor.prototype[name];
            return typeof fn === 'function' ? fn : null;
        } catch (e) { return null; }
    }

    function safeInvoke(el, fn, args, callbackName) {
        if (typeof fn !== 'function') return;
        try {
            fn.apply(el, args || []);
        } catch (e) {
            // Match the spec's "report exception" — surface via
            // console.error if present, otherwise swallow.
            if (typeof console !== 'undefined' && typeof console.error === 'function') {
                var tag = '';
                try { tag = el && el.tagName ? String(el.tagName).toLowerCase() : ''; } catch (_) {}
                var label = '<' + (tag || '?') + '>.' + (callbackName || 'callback');
                var name = (e && e.name) || 'Error';
                var msg  = (e && e.message != null) ? String(e.message) : '';
                var stack = (e && e.stack) ? String(e.stack) : '';
                console.error(label + ' threw: ' + name + ': ' + msg + '\n' + stack);
            }
        }
    }

    function fireConnected(el) {
        var def = el && el[CE_DEFINITION] ? el[CE_DEFINITION] : getDefinitionForElement(el);
        if (!def) return;
        var fn = lookupCallbackOnDefinition(def, 'connectedCallback');
        safeInvoke(el, fn, null, 'connectedCallback');
    }

    function fireDisconnected(el) {
        var def = el && el[CE_DEFINITION] ? el[CE_DEFINITION] : getDefinitionForElement(el);
        if (!def) return;
        var fn = lookupCallbackOnDefinition(def, 'disconnectedCallback');
        safeInvoke(el, fn, null, 'disconnectedCallback');
    }

    function fireAttributeChanged(el, name, oldValue, newValue) {
        var def = el && el[CE_DEFINITION] ? el[CE_DEFINITION] : getDefinitionForElement(el);
        if (!def) return;
        if (!def.observedAttributes || !def.observedAttributes[name]) return;
        var fn = lookupCallbackOnDefinition(def, 'attributeChangedCallback');
        safeInvoke(el, fn, [name, oldValue, newValue, null], 'attributeChangedCallback');
    }

    // -----------------------------------------------------------------
    // Is `el` connected to the document tree?
    //
    // `document.contains(el)` is the spec test (DOM §4.4.6
    // "Node.isConnected"). We re-implement against the underlying
    // .parentNode chain because the wrapper identity isn't stable
    // (document.contains takes whatever wrapper the caller gave us
    // and walks its parents — those re-resolve correctly because
    // every wrapper is backed by the same parse tree).
    // -----------------------------------------------------------------
    function isConnected(el) {
        if (!el || typeof document === 'undefined') return false;
        try {
            return document.contains(el);
        } catch (e) {
            return false;
        }
    }

    // -----------------------------------------------------------------
    // Walk a subtree calling `cb` on every element whose tag is a
    // registered custom element. Used by:
    //  - define() to fire connectedCallback on existing elements
    //  - the appendChild / insertBefore wrappers to fire
    //    connectedCallback on newly-attached subtrees
    //  - the removeChild / remove wrappers to fire
    //    disconnectedCallback on newly-detached subtrees
    //  - upgrade() to upgrade descendants
    //
    // We walk via .children rather than querySelectorAll because
    // querySelectorAll requires a CSS-valid selector and custom
    // element names can technically include chars that confuse the
    // parser (the PCEN ranges include Unicode chars outside the
    // ASCII identifier set).
    // -----------------------------------------------------------------
    function walkElementsDescending(root, cb) {
        if (!root) return;
        try {
            cb(root);
        } catch (e) { /* keep walking on user-cb throws */ }
        var kids;
        try { kids = root.children; } catch (e) { kids = null; }
        if (!kids) return;
        for (var i = 0; i < kids.length; i++) {
            walkElementsDescending(kids[i], cb);
        }
    }

    // -----------------------------------------------------------------
    // CustomElementRegistry — the `globalThis.customElements`
    // instance. WHATWG HTML §4.13.1.
    //
    // We expose a singleton; real browsers also have a singleton
    // per Window. Heso has one Window.
    // -----------------------------------------------------------------
    var registry = {
        define: function(name, ctor, options) {
            if (typeof name !== 'string') {
                throw new TypeError("customElements.define: name must be a string");
            }
            if (typeof ctor !== 'function') {
                throw new TypeError("customElements.define: constructor must be a function");
            }
            if (!isValidCustomElementName(name)) {
                // Spec: throw a "SyntaxError" DOMException.
                // We don't have DOMException-with-name access from
                // here without taking a dep on the Rust DOMException
                // type, so we use a SyntaxError-shaped Error whose
                // `.name` is 'SyntaxError'.
                var e = new SyntaxError(
                    "customElements.define: '" + name + "' is not a valid custom element name"
                );
                e.name = 'SyntaxError';
                throw e;
            }
            if (nameToDef[name]) {
                var e = new Error(
                    "customElements.define: the name '" + name + "' has already been used with this registry"
                );
                e.name = 'NotSupportedError';
                throw e;
            }
            if (ctorToName.has(ctor)) {
                var e = new Error(
                    "customElements.define: this constructor has already been used with this registry"
                );
                e.name = 'NotSupportedError';
                throw e;
            }
            // Read observedAttributes off the class. Spec §4.13.3
            // "Element definition" step 8.
            var observed = Object.create(null);
            var rawObserved = null;
            try { rawObserved = ctor.observedAttributes; } catch (e) {}
            if (Array.isArray(rawObserved)) {
                for (var i = 0; i < rawObserved.length; i++) {
                    observed[String(rawObserved[i]).toLowerCase()] = true;
                }
            }
            var def = {
                name: name,
                ctor: ctor,
                observedAttributes: observed,
                extendsTag: options && typeof options.extends === 'string'
                    ? options.extends.toLowerCase() : null,
            };
            nameToDef[name] = def;
            ctorToName.set(ctor, name);

            // Install Symbol.hasInstance on the user's class so that
            // `el instanceof MyEl` returns true for ANY wrapper whose
            // `localName` matches the registered name, regardless of
            // the wrapper's current [[Prototype]].
            //
            // Why: every `document.querySelector('my-el')` produces a
            // fresh wrapper around the same NodeId. Even after the
            // upgrade walk re-prototypes one wrapper, a later
            // querySelector hands back a wrapper whose proto is still
            // bare `Element.prototype`. The native `instanceof`
            // walks proto chains and would miss. WHATWG HTML
            // §4.13.6.6 ("upgrade an element") implicitly assumes
            // stable wrapper identity (the spec talks about "the
            // element" as one object); heso's wrapper-per-query
            // model means we additionally route through
            // `Symbol.hasInstance` for the spec-conformant outcome.
            //
            // OSS reference: jsdom doesn't need this because its
            // wrappers are stable; happy-dom does the same trick
            // for its WindowBrowserSettings-gated class detection.
            try {
                Object.defineProperty(ctor, Symbol.hasInstance, {
                    value: function(instance) {
                        if (!instance || typeof instance !== 'object') return false;
                        try {
                            var ln = instance.localName;
                            if (typeof ln !== 'string') return false;
                            return ln.toLowerCase() === name;
                        } catch (e) { return false; }
                    },
                    writable: false,
                    enumerable: false,
                    configurable: true,
                });
            } catch (e) {
                // Some classes lock Symbol.hasInstance via their own
                // descriptor (rare in practice). Fall back to the
                // default prototype-chain instanceof; the lazy
                // stamping below covers the most common path.
            }

            // Upgrade existing elements. Spec §4.13.1.define step 14:
            // for every element in the document whose local name is
            // `name`, enqueue an upgrade reaction. Per WHATWG HTML
            // §4.13.6.6 ("upgrade an element") step 8, the upgrade
            // changes the element's [[Prototype]] to the definition's
            // interface prototype — this is what `constructUpgradeOnto`
            // accomplishes (via Reflect.construct + the
            // HTMLElement constructor's setPrototypeOf inside).
            //
            // We walk the doc once, upgrade, then fire
            // connectedCallback on those that are in the tree.
            if (typeof document !== 'undefined' && document.documentElement) {
                var toUpgrade = [];
                walkElementsDescending(document.documentElement, function(el) {
                    if (!el || typeof el.localName !== 'string') return;
                    if (el.localName.toLowerCase() !== name) return;
                    toUpgrade.push(el);
                });
                for (var j = 0; j < toUpgrade.length; j++) {
                    var el = toUpgrade[j];
                    try {
                        constructUpgradeOnto(el, ctor);
                        markUpgraded(el, def);
                    } catch (e) {
                        if (typeof console !== 'undefined' && console.error) {
                            console.error('customElements.define upgrade threw:', e && e.message ? e.message : e);
                        }
                        continue;
                    }
                    // Fire attributeChangedCallback for each observed
                    // attr present at define time. Spec §4.13.4 step
                    // 5: enqueue an attributeChangedCallback for each
                    // attribute in the element's attribute list.
                    for (var attrName in observed) {
                        if (el.hasAttribute && el.hasAttribute(attrName)) {
                            var val = el.getAttribute(attrName);
                            fireAttributeChanged(el, attrName, null, val);
                        }
                    }
                    // Connected pass: only fire if already in tree.
                    if (isConnected(el)) {
                        fireConnected(el);
                    }
                }
            }

            // Resolve whenDefined promises. Spec §4.13.1.whenDefined
            // step 4: when `name` is added, resolve the promise.
            var callbacks = whenDefinedCallbacks[name];
            if (callbacks) {
                delete whenDefinedCallbacks[name];
                for (var k = 0; k < callbacks.length; k++) {
                    try { callbacks[k](ctor); } catch (e) {}
                }
            }
        },

        get: function(name) {
            var def = nameToDef[name];
            return def ? def.ctor : undefined;
        },

        getName: function(ctor) {
            if (typeof ctor !== 'function') return null;
            var name = ctorToName.get(ctor);
            return name || null;
        },

        whenDefined: function(name) {
            if (typeof name !== 'string') {
                return Promise.reject(new TypeError(
                    "customElements.whenDefined: name must be a string"
                ));
            }
            if (!isValidCustomElementName(name)) {
                var e = new SyntaxError(
                    "customElements.whenDefined: '" + name + "' is not a valid custom element name"
                );
                e.name = 'SyntaxError';
                return Promise.reject(e);
            }
            var def = nameToDef[name];
            if (def) return Promise.resolve(def.ctor);
            return new Promise(function(resolve) {
                if (!whenDefinedCallbacks[name]) whenDefinedCallbacks[name] = [];
                whenDefinedCallbacks[name].push(resolve);
            });
        },

        upgrade: function(root) {
            // Spec §4.13.1.upgrade: walk descendants and call
            // tryUpgradeElement on each. If `root` itself is a
            // candidate, upgrade it too.
            if (!root) return;
            walkElementsDescending(root, function(el) {
                if (isUpgraded(el)) return;
                var def = getDefinitionForElement(el);
                if (!def) return;
                try {
                    constructUpgradeOnto(el, def.ctor);
                    markUpgraded(el, def);
                } catch (e) { /* per spec, swallow */ }
            });
        },
    };

    Object.defineProperty(globalThis, 'customElements', {
        value: registry,
        writable: false,
        configurable: false,
        enumerable: true,
    });

    // ===============================================================
    // Lifecycle wrapping: appendChild / insertBefore / removeChild /
    // remove / innerHTML setter / setAttribute / removeAttribute.
    //
    // Each wrapper preserves the original Rust-backed behavior and
    // adds the lifecycle pass. Wrappers go on Element.prototype
    // (and Document.prototype where applicable).
    // ===============================================================

    // appendChild
    var origAppendChild = elementProto.appendChild;
    if (typeof origAppendChild === 'function') {
        elementProto.appendChild = function(child) {
            var connectedBefore = isConnected(child);
            var result = origAppendChild.call(this, child);
            var connectedAfter = isConnected(child);
            if (!connectedBefore && connectedAfter) {
                walkElementsDescending(child, function(el) {
                    var def = getDefinitionForElement(el);
                    if (!def) return;
                    if (!isUpgraded(el)) {
                        try {
                            constructUpgradeOnto(el, def.ctor);
                            markUpgraded(el, def);
                            for (var attrName in def.observedAttributes) {
                                if (el.hasAttribute && el.hasAttribute(attrName)) {
                                    fireAttributeChanged(el, attrName, null, el.getAttribute(attrName));
                                }
                            }
                        } catch (e) { return; }
                    }
                    fireConnected(el);
                });
            }
            return result;
        };
    }

    // insertBefore
    var origInsertBefore = elementProto.insertBefore;
    if (typeof origInsertBefore === 'function') {
        elementProto.insertBefore = function(newNode, refNode) {
            var connectedBefore = isConnected(newNode);
            var result = origInsertBefore.call(this, newNode, refNode);
            var connectedAfter = isConnected(newNode);
            if (!connectedBefore && connectedAfter) {
                walkElementsDescending(newNode, function(el) {
                    var def = getDefinitionForElement(el);
                    if (!def) return;
                    if (!isUpgraded(el)) {
                        try {
                            constructUpgradeOnto(el, def.ctor);
                            markUpgraded(el, def);
                            for (var attrName in def.observedAttributes) {
                                if (el.hasAttribute && el.hasAttribute(attrName)) {
                                    fireAttributeChanged(el, attrName, null, el.getAttribute(attrName));
                                }
                            }
                        } catch (e) { return; }
                    }
                    fireConnected(el);
                });
            }
            return result;
        };
    }

    // removeChild
    var origRemoveChild = elementProto.removeChild;
    if (typeof origRemoveChild === 'function') {
        elementProto.removeChild = function(child) {
            // Collect the soon-to-be-detached subtree's custom elements
            // BEFORE the detach, so their lifecycle fires reference the
            // pre-detach state.
            var toFire = [];
            if (isConnected(child)) {
                walkElementsDescending(child, function(el) {
                    var def = getDefinitionForElement(el);
                    if (def) toFire.push(el);
                });
            }
            var result = origRemoveChild.call(this, child);
            for (var i = 0; i < toFire.length; i++) {
                fireDisconnected(toFire[i]);
            }
            return result;
        };
    }

    // remove
    var origRemove = elementProto.remove;
    if (typeof origRemove === 'function') {
        elementProto.remove = function() {
            var toFire = [];
            if (isConnected(this)) {
                walkElementsDescending(this, function(el) {
                    var def = getDefinitionForElement(el);
                    if (def) toFire.push(el);
                });
            }
            var result = origRemove.call(this);
            for (var i = 0; i < toFire.length; i++) {
                fireDisconnected(toFire[i]);
            }
            return result;
        };
    }

    // NOTE: innerHTML lifecycle wrapping intentionally omitted.
    // rquickjs 0.11 emits class accessors with the configurable flag
    // unset (see methods/accessor.rs in the macro crate), so
    // `Object.defineProperty(elementProto, 'innerHTML', ...)` throws
    // "property is not configurable". Pages that set innerHTML to a
    // string containing custom elements get a Rust-side parse but no
    // `connectedCallback` / upgrade pass. As a workaround, call
    // `customElements.upgrade(parent)` after assignment, or use
    // createElement + appendChild (which IS wrapped). Tracked as a
    // known limit; production frameworks (Lit, Solid, Stencil) all
    // route through createElement + the document.createTreeWalker
    // path that hits our appendChild wrapper.

    // setAttribute — fires attributeChangedCallback if the attr is
    // in observedAttributes.
    var origSetAttribute = elementProto.setAttribute;
    if (typeof origSetAttribute === 'function') {
        elementProto.setAttribute = function(name, value) {
            var lname = String(name).toLowerCase();
            var oldValue = null;
            try {
                if (this.hasAttribute && this.hasAttribute(lname)) {
                    oldValue = this.getAttribute(lname);
                }
            } catch (e) {}
            var result = origSetAttribute.call(this, name, value);
            var newValue;
            try {
                newValue = this.getAttribute(lname);
            } catch (e) { newValue = null; }
            if (oldValue !== newValue) {
                fireAttributeChanged(this, lname, oldValue, newValue);
            }
            return result;
        };
    }

    // removeAttribute
    var origRemoveAttribute = elementProto.removeAttribute;
    if (typeof origRemoveAttribute === 'function') {
        elementProto.removeAttribute = function(name) {
            var lname = String(name).toLowerCase();
            var oldValue = null;
            try {
                if (this.hasAttribute && this.hasAttribute(lname)) {
                    oldValue = this.getAttribute(lname);
                }
            } catch (e) {}
            var result = origRemoveAttribute.call(this, name);
            if (oldValue !== null) {
                fireAttributeChanged(this, lname, oldValue, null);
            }
            return result;
        };
    }

    // document.createElement — when the tag has a registered
    // definition, instantiate via constructUpgradeOnto so the user's
    // class hierarchy is set up on the returned element. Spec
    // §4.13.1.create-element step 5.
    //
    // Hooks onto Document.prototype so all document instances pick
    // up the wrapping. The original method lives on the prototype
    // (per Class::define behaviour), so we wrap it there.
    var origCreateElement = documentProto.createElement;
    if (typeof origCreateElement === 'function') {
        documentProto.createElement = function(name) {
            var el = origCreateElement.call(this, name);
            if (typeof name !== 'string') return el;
            var def = nameToDef[name.toLowerCase()];
            if (!def) return el;
            try {
                constructUpgradeOnto(el, def.ctor);
                markUpgraded(el, def);
            } catch (e) {
                // Per spec, creation continues with an unupgraded
                // element if the constructor throws. We surface via
                // console.error so the agent sees the failure.
                if (typeof console !== 'undefined' && console.error) {
                    console.error("customElements: constructor threw during createElement:", e && e.message ? e.message : e);
                }
            }
            return el;
        };
    }

    // ===============================================================
    // Lazy prototype stamping on wrapper-returning methods.
    //
    // Every `document.querySelector(...)` / `querySelectorAll(...)` /
    // `getElementById(...)` / `getElementsByTagName(...)` and their
    // Element-side counterparts produces a FRESH wrapper around the
    // same NodeId. The upgrade walk in `define()` only stamps the
    // prototype on the wrappers it saw during the walk; subsequent
    // fresh wrappers (the ones the page actually hands to user JS)
    // arrive with bare `Element.prototype`. We hook each
    // wrapper-returning method to call `stampPrototypeIfRegistered`
    // on the result so the user's class methods, `constructor`, and
    // the prototype-chain `instanceof` all work for late-defined
    // tags. Methods (as opposed to getters) are configurable per
    // rquickjs 0.11's macro/methods/method.rs (each method gets
    // `.writable().configurable()` flags), so we can override them.
    // Getters like `parentNode` / `children` / `firstElementChild`
    // remain non-configurable; their wrappers ride the
    // `Symbol.hasInstance` path on user ctors instead.
    // ===============================================================

    // One-time warn dedup so a missing prototype method surfaces once
    // in console instead of silently no-op'ing. Lesson from v0.0.5/0.0.6:
    // wrapReturnsElement was the silent grave for `Element.prototype.X`
    // gaps — Catalyst's connectedCallback calls would throw deep inside
    // user bundles with "not a function" and no breadcrumb pointing
    // back at heso. Surface the gap at install time instead.
    var __missingProtoMethodWarned = {};
    function warnMissingProtoMethod(protoName, method) {
        try {
            if (typeof console === 'undefined' || !console.warn) return;
            var key = protoName + '.' + method;
            if (__missingProtoMethodWarned[key]) return;
            __missingProtoMethodWarned[key] = true;
            console.warn('heso: ' + protoName + '.prototype.' + method
                + ' is not implemented; lifecycle code may throw');
        } catch (_) {}
    }

    function wrapReturnsElement(proto, method) {
        if (!proto) return;
        var orig = proto[method];
        if (typeof orig !== 'function') {
            warnMissingProtoMethod(proto === documentProto ? 'Document' : 'Element', method);
            return;
        }
        proto[method] = function() {
            var result = orig.apply(this, arguments);
            return stampPrototypeIfRegistered(result);
        };
    }

    function wrapReturnsElementArray(proto, method) {
        if (!proto) return;
        var orig = proto[method];
        if (typeof orig !== 'function') {
            warnMissingProtoMethod(proto === documentProto ? 'Document' : 'Element', method);
            return;
        }
        proto[method] = function() {
            var result = orig.apply(this, arguments);
            return stampArrayPrototypes(result);
        };
    }

    // Document.prototype query methods.
    wrapReturnsElement(documentProto, 'querySelector');
    wrapReturnsElementArray(documentProto, 'querySelectorAll');
    wrapReturnsElement(documentProto, 'getElementById');
    wrapReturnsElementArray(documentProto, 'getElementsByTagName');
    wrapReturnsElementArray(documentProto, 'getElementsByClassName');
    wrapReturnsElementArray(documentProto, 'getElementsByName');

    // Element.prototype query methods (descendant queries).
    wrapReturnsElement(elementProto, 'querySelector');
    wrapReturnsElementArray(elementProto, 'querySelectorAll');
    wrapReturnsElement(elementProto, 'closest');
    wrapReturnsElementArray(elementProto, 'getElementsByTagName');
    wrapReturnsElementArray(elementProto, 'getElementsByClassName');

    // cloneNode returns a fresh wrapper — stamp it if the cloned
    // element's tag is registered.
    wrapReturnsElement(elementProto, 'cloneNode');

    // ===============================================================
    // ParentNode / ChildNode mixins — DOM Std §4.2.6 / §4.2.7.
    //
    // These are missing on heso's Rust-side Element/Document and the
    // rspack popover-polyfill GitHub embeds (oddbird/popover-polyfill)
    // calls them at the top level of every page that has Catalyst
    // custom elements: `e.head.prepend(styleNode)` and `e.prepend(node)`
    // for shadow roots, then `e.adoptedStyleSheets = [...]` in the
    // CSSStyleSheet branch. Without these mixins, the polyfill throws
    // `TypeError: not a function` and every Catalyst connectedCallback
    // on the page cascades from the same broken entry-point.
    //
    // Variadic args accept `(Node | string)*`: strings become text
    // nodes via `document.createTextNode`; Nodes are inserted with the
    // existing `appendChild` / `insertBefore` plumbing so the slot-
    // change dispatch and listener-registry invariants from the Rust
    // side fire unchanged.
    // ===============================================================
    function toNodes(args) {
        var out = [];
        for (var i = 0; i < args.length; i++) {
            var a = args[i];
            if (a == null) continue;
            if (typeof a === 'string') {
                out.push(document.createTextNode(a));
            } else {
                out.push(a);
            }
        }
        return out;
    }
    function installParentNodeMixin(proto, label) {
        if (!proto) return;
        if (typeof proto.append !== 'function') {
            proto.append = function() {
                var nodes = toNodes(arguments);
                for (var i = 0; i < nodes.length; i++) this.appendChild(nodes[i]);
            };
        }
        if (typeof proto.prepend !== 'function') {
            proto.prepend = function() {
                var nodes = toNodes(arguments);
                var first = this.firstChild;
                for (var i = 0; i < nodes.length; i++) {
                    if (first) this.insertBefore(nodes[i], first);
                    else this.appendChild(nodes[i]);
                }
            };
        }
        if (typeof proto.replaceChildren !== 'function') {
            proto.replaceChildren = function() {
                while (this.firstChild) this.removeChild(this.firstChild);
                var nodes = toNodes(arguments);
                for (var i = 0; i < nodes.length; i++) this.appendChild(nodes[i]);
            };
        }
    }
    installParentNodeMixin(elementProto, 'Element');
    installParentNodeMixin(documentProto, 'Document');

    if (typeof elementProto.before !== 'function') {
        elementProto.before = function() {
            var parent = this.parentNode;
            if (!parent) return;
            var nodes = toNodes(arguments);
            for (var i = 0; i < nodes.length; i++) parent.insertBefore(nodes[i], this);
        };
    }
    if (typeof elementProto.after !== 'function') {
        elementProto.after = function() {
            var parent = this.parentNode;
            if (!parent) return;
            var ref = this.nextSibling;
            var nodes = toNodes(arguments);
            for (var i = 0; i < nodes.length; i++) {
                if (ref) parent.insertBefore(nodes[i], ref);
                else parent.appendChild(nodes[i]);
            }
        };
    }
    if (typeof elementProto.remove !== 'function') {
        elementProto.remove = function() {
            var parent = this.parentNode;
            if (parent) parent.removeChild(this);
        };
    }
    if (typeof elementProto.replaceWith !== 'function') {
        elementProto.replaceWith = function() {
            var parent = this.parentNode;
            if (!parent) return;
            var nodes = toNodes(arguments);
            var ref = this.nextSibling;
            parent.removeChild(this);
            for (var i = 0; i < nodes.length; i++) {
                if (ref) parent.insertBefore(nodes[i], ref);
                else parent.appendChild(nodes[i]);
            }
        };
    }

    // ===============================================================
    // Tag-gated prototype methods that live on a specific HTML element
    // subclass but ship on the shared Element prototype here (heso has
    // one prototype object for all elements, so subclass-specific
    // methods guard on `this.tagName`).
    // ===============================================================

    // HTMLSelectElement.prototype.add(element, before?) — HTML §4.10.7.
    // docs.python.org switchers.js calls select.add(option) to wire its
    // version picker.
    if (typeof elementProto.add !== 'function') {
        elementProto.add = function(element, before) {
            var tag = (this.tagName || '').toLowerCase();
            if (tag !== 'select' && tag !== 'optgroup' && tag !== 'datalist') {
                throw new TypeError('Element.add: receiver is not <select>/<optgroup>/<datalist>');
            }
            if (before == null) return this.appendChild(element);
            if (typeof before === 'number') {
                var children = this.children;
                var ref = (children && children[before]) || null;
                return ref ? this.insertBefore(element, ref) : this.appendChild(element);
            }
            return this.insertBefore(element, before);
        };
    }

    // HTMLCanvasElement.prototype.getContext(type, attrs?) — HTML §4.12.5.
    // anthropic.com (Webflow Lottie) and apple.com (ac-target.js GPU
    // fingerprint) both call canvas.getContext("2d"/"webgl"). heso
    // does not render, so we return inert no-op contexts whose methods
    // are present (so chained calls don't throw) but do nothing.
    if (typeof elementProto.getContext !== 'function') {
        var canvasContext2D = null;
        var canvasContextGL = null;
        function makeContext2D() {
            if (canvasContext2D) return canvasContext2D;
            var noop = function() {};
            canvasContext2D = {
                canvas: null,
                fillStyle: '#000', strokeStyle: '#000',
                font: '10px sans-serif',
                globalAlpha: 1, globalCompositeOperation: 'source-over',
                lineWidth: 1, lineCap: 'butt', lineJoin: 'miter',
                miterLimit: 10, lineDashOffset: 0,
                textAlign: 'start', textBaseline: 'alphabetic',
                direction: 'inherit',
                imageSmoothingEnabled: true, imageSmoothingQuality: 'low',
                shadowBlur: 0, shadowColor: 'rgba(0,0,0,0)',
                shadowOffsetX: 0, shadowOffsetY: 0,
                fillRect: noop, strokeRect: noop, clearRect: noop,
                fillText: noop, strokeText: noop,
                measureText: function(t) { return { width: (t == null ? 0 : String(t).length * 6) }; },
                beginPath: noop, closePath: noop, moveTo: noop, lineTo: noop,
                rect: noop, arc: noop, arcTo: noop, bezierCurveTo: noop,
                quadraticCurveTo: noop, ellipse: noop,
                stroke: noop, fill: noop, clip: noop,
                save: noop, restore: noop,
                scale: noop, rotate: noop, translate: noop,
                transform: noop, setTransform: noop, resetTransform: noop,
                createLinearGradient: function() { return { addColorStop: noop }; },
                createRadialGradient: function() { return { addColorStop: noop }; },
                createConicGradient: function() { return { addColorStop: noop }; },
                createPattern: function() { return null; },
                drawImage: noop, putImageData: noop, getImageData: function(_x, _y, w, h) {
                    var len = Math.max(1, (w|0) * (h|0)) * 4;
                    return { data: new Uint8ClampedArray(len), width: w|0, height: h|0 };
                },
                createImageData: function(w, h) {
                    var len = Math.max(1, (w|0) * (h|0)) * 4;
                    return { data: new Uint8ClampedArray(len), width: w|0, height: h|0 };
                },
                setLineDash: noop, getLineDash: function() { return []; },
                isPointInPath: function() { return false; },
                isPointInStroke: function() { return false; },
            };
            return canvasContext2D;
        }
        function makeContextGL() {
            if (canvasContextGL) return canvasContextGL;
            canvasContextGL = {
                canvas: null,
                drawingBufferWidth: 0, drawingBufferHeight: 0,
                getExtension: function() { return null; },
                getParameter: function() { return null; },
                getSupportedExtensions: function() { return []; },
                getContextAttributes: function() { return null; },
                createShader: function() { return null; },
                createProgram: function() { return null; },
                createBuffer: function() { return null; },
                createTexture: function() { return null; },
            };
            return canvasContextGL;
        }
        elementProto.getContext = function(type) {
            var tag = (this.tagName || '').toLowerCase();
            if (tag !== 'canvas') return null;
            if (type === '2d') return makeContext2D();
            if (type === 'webgl' || type === 'experimental-webgl' || type === 'webgl2') return makeContextGL();
            return null;
        };
    }

    // ===============================================================
    // Element.prototype.constructor as a tag-dispatched getter.
    //
    // Why: even when a fresh wrapper's [[Prototype]] hasn't been
    // re-pointed at the user's class prototype (e.g. a wrapper
    // captured BEFORE define() but accessed AFTER), `el.constructor`
    // and `el.constructor.name` need to return the registered ctor.
    // Real browsers achieve this via stable wrapper identity (one
    // wrapper per node, upgraded once). heso's fresh-per-query model
    // breaks that, so we route `.constructor` through a getter that
    // checks the receiver's localName against the registry.
    //
    // `makeIllegalConstructor` set `Element.prototype.constructor`
    // as a configurable data property pointing at the `Element`
    // illegal-ctor function. We re-define it here as an accessor
    // that returns the registered ctor for tags in the registry,
    // and falls back to `Element` otherwise. Spec-conformance: the
    // own/inherited-property layout differs from a real browser
    // (where `el.constructor` resolves on the user's prototype
    // directly) but the observable values are identical for
    // `el.constructor === MyEl` and `el.constructor.name === "MyEl"`.
    //
    // Since this getter only changes the observable value of
    // `el.constructor` (not e.g. `el.appendChild`), pre-existing
    // tests that assert `el.constructor === Element` for bare
    // elements still hold: an `<div>` has no entry in `nameToDef`,
    // so the getter falls through to `Element`.
    // ===============================================================
    var defaultElementCtor = Element;
    try {
        Object.defineProperty(elementProto, 'constructor', {
            get: function() {
                try {
                    if (this && typeof this.localName === 'string') {
                        var def = nameToDef[this.localName.toLowerCase()];
                        if (def && def.ctor) return def.ctor;
                    }
                } catch (e) {}
                return defaultElementCtor;
            },
            // No setter: `el.constructor = ...` becomes a no-op,
            // matching the spec (constructor is not writable on a
            // typical interface).
            configurable: true,
            enumerable: false,
        });
    } catch (e) {
        if (typeof console !== 'undefined' && console.warn) {
            console.warn('heso: failed to install constructor dispatcher:', e && e.message ? e.message : e);
        }
    }

    // createDocumentFragment shim — Phase 1B doesn't have a Rust
    // DocumentFragment type. We approximate via a detached <template>
    // wrapper: createElement('template') gives an orphan element
    // that can hold children, supports appendChild, and survives the
    // queryselector + serializer path. Frameworks that branch on
    // `frag instanceof DocumentFragment` then `frag.appendChild` get
    // a working object; strict-mode users who require the spec node
    // type get the wrong nodeType (1 instead of 11), which is a
    // known limitation tracked in the module docstring.
    if (typeof documentProto.createDocumentFragment !== 'function') {
        documentProto.createDocumentFragment = function() {
            // Use a real <template>; its `.content` would be the
            // fragment in a real browser, but here we return the
            // <template> itself because the Rust side doesn't split
            // content out. Frameworks call .appendChild on the
            // return value, which works because <template> is just
            // another orphan Element.
            var frag = this.createElement('template');
            return frag;
        };
    }

    // ===============================================================
    // template.content — DocumentFragment-shaped view over the
    // template's children. WHATWG HTML §4.12.3 specifies a real
    // off-document fragment; heso punts to a lazy "materialised
    // copy" approach because dom_query / html5ever stash a parsed
    // `<template>`'s children in an associated document fragment
    // that `dom_query::NodeRef` does NOT expose to Rust callers
    // (verified against dom_query 0.28's `NodeRef` API surface — no
    // `template_content` / `content_document` accessor). Walking the
    // template via `.children` / `.querySelector` therefore sees an
    // empty subtree, even though `t.outerHTML` correctly serializes
    // the inner markup (the html5ever serializer special-cases
    // `<template>` and inlines the fragment).
    //
    // Workaround: lazily build a sibling element ("content holder")
    // populated by parsing the template's outerHTML and stripping
    // the wrapper. Cache it as a non-enumerable own-property on the
    // template wrapper so repeated `.content` access from the same
    // wrapper returns the same object. (Across fresh wrappers the
    // cache misses — but Lit always reuses one template wrapper for
    // many renders, so the per-render cost is one parse at first
    // touch.)
    //
    // The load-bearing surface for Lit / Material Web / shoelace:
    //   - `template.content.querySelector(...)` / `.children`
    //   - `template.content.cloneNode(true)` — copies the holder
    //     element with its parsed children. Lit's TreeWalker then
    //     iterates descendants of the clone, which works because
    //     the clone preserves the child node structure.
    //
    // Known limit (still a punt): the holder is an Element wrapper
    // with nodeType=1, not a real DocumentFragment with nodeType=11.
    // Strict feature-detects that branch on
    // `content.nodeType === 11` get the wrong answer; we accept the
    // mismatch because the practical Lit / Material Web path uses
    // `content.firstElementChild` / `content.children` / `.cloneNode`
    // without sniffing nodeType. A future fix can promote the
    // holder to a real DocumentFragment node once heso's Rust DOM
    // grows fragment-typed nodes.
    //
    // Installed as a configurable getter on Element.prototype; a
    // generic Element gets `undefined` (real browsers only expose
    // `.content` on HTMLTemplateElement, never on bare Element).
    // ===============================================================
    var TEMPLATE_CONTENT_HOLDER = '__hesoTemplateContentHolder';

    function buildTemplateContentHolder(template) {
        var outer = '';
        try { outer = template.outerHTML || ''; } catch (e) { outer = ''; }
        // Strip the outer <template[ attrs]>...</template> wrapper.
        // html5ever's serializer always emits the closing tag, so
        // a regex that matches the open + close pair is sufficient
        // for well-formed templates. Edge case: a template with
        // nested templates would have multiple closing tags; the
        // greedy match accommodates that (we drop the OUTERMOST
        // wrapper only).
        var inner = outer;
        var openEnd = inner.indexOf('>');
        if (openEnd >= 0) {
            inner = inner.slice(openEnd + 1);
        }
        var closeStart = inner.lastIndexOf('</template>');
        if (closeStart >= 0) {
            inner = inner.slice(0, closeStart);
        }
        // Build a holder element and parse the inner markup into it.
        // 'template-content-holder' avoids any chance of colliding
        // with a registered custom element name (registered names
        // can't contain double hyphens at the spec-validation level,
        // but defensively keep one anyway).
        var holder;
        try {
            holder = (typeof document !== 'undefined')
                ? document.createElement('template-content-holder')
                : null;
        } catch (e) { holder = null; }
        if (holder) {
            try { holder.innerHTML = inner; } catch (e) {}
        }
        return holder;
    }

    try {
        Object.defineProperty(elementProto, 'content', {
            get: function() {
                try {
                    if (typeof this.localName !== 'string'
                        || this.localName.toLowerCase() !== 'template') {
                        return undefined;
                    }
                } catch (e) { return undefined; }
                // Cache the holder on the wrapper to keep repeated
                // accesses cheap (the wrapper itself is fresh per
                // query, so the cache is per-wrapper, not per-node;
                // Lit reuses one wrapper for many renders).
                if (this[TEMPLATE_CONTENT_HOLDER]) {
                    return this[TEMPLATE_CONTENT_HOLDER];
                }
                var holder = buildTemplateContentHolder(this);
                try {
                    Object.defineProperty(this, TEMPLATE_CONTENT_HOLDER, {
                        value: holder,
                        writable: true,
                        configurable: true,
                        enumerable: false,
                    });
                } catch (e) { /* fresh wrapper next time */ }
                return holder;
            },
            configurable: true,
            enumerable: false,
        });
    } catch (e) {
        // If a Rust-side `content` accessor ever lands on the
        // Element class, this becomes a redefine-on-non-configurable
        // throw — log and continue without the shim.
        if (typeof console !== 'undefined' && console.warn) {
            console.warn('heso: failed to install template.content shim:', e && e.message ? e.message : e);
        }
    }

    // ===============================================================
    // element.dataset — DOMStringMap over `data-*` attributes.
    //
    // WHATWG HTML §3.2.6.6 "DOMStringMap":
    //   - `el.dataset.foo` ↔ `el.getAttribute('data-foo')`
    //   - `el.dataset.fooBar` ↔ `el.getAttribute('data-foo-bar')`
    //     (camelCase ↔ kebab-case conversion: lowercase letter
    //      preceded by a "-" becomes uppercase; uppercase letter
    //      becomes "-" + lowercase letter)
    //   - `'foo' in el.dataset` checks attribute presence
    //   - `delete el.dataset.foo` removes the attribute
    //
    // Installed as a configurable getter that returns a per-call
    // Proxy. The Proxy traps `get` / `set` / `has` / `deleteProperty`
    // / `ownKeys` / `getOwnPropertyDescriptor` (the last two so
    // `Object.keys(el.dataset)` and spread `{...el.dataset}` work).
    //
    // Per-call construction: the Proxy holds a closure over `this`
    // (the element wrapper). Real browsers cache the DOMStringMap
    // per element; heso's fresh-wrapper-per-query model would defeat
    // any caching scheme keyed by JS identity, so we build on
    // demand. Cost: one Proxy allocation per `el.dataset` access.
    // Negligible at typical framework call frequency.
    //
    // OSS reference: jsdom's DOMStringMap impl is a Proxy with the
    // same five traps, modulo their use of WebIDL-generated key
    // validation. happy-dom uses a plain object with on-demand
    // recompute via getters — simpler, but doesn't support
    // arbitrary `data-x-y` keys without enumerating. Proxy wins.
    // ===============================================================
    function datasetKeyToAttr(key) {
        // camelCase → kebab-case: "fooBar" → "foo-bar". Per the
        // spec the rule is "for each uppercase letter, insert '-'
        // before it and lowercase it". If the key contains
        // characters that aren't valid in an attribute name
        // (whitespace, "=", etc.), real browsers throw SyntaxError;
        // we return a safe-ish placeholder so user code gets a
        // null read rather than a thrown property access.
        var out = '';
        for (var i = 0; i < key.length; i++) {
            var c = key.charCodeAt(i);
            if (c >= 65 && c <= 90) { /* A-Z */
                out += '-' + key.charAt(i).toLowerCase();
            } else {
                out += key.charAt(i);
            }
        }
        return 'data-' + out;
    }

    function attrToDatasetKey(attr) {
        // kebab-case → camelCase: "data-foo-bar" → "fooBar". Drop
        // the "data-" prefix, then for each "-x" (x = lowercase
        // ascii letter), drop the "-" and uppercase x. Other dashes
        // are passed through (spec allows e.g. "data-3d").
        if (!attr || attr.indexOf('data-') !== 0) return null;
        var rest = attr.slice(5);
        var out = '';
        var i = 0;
        while (i < rest.length) {
            var ch = rest.charAt(i);
            if (ch === '-' && i + 1 < rest.length) {
                var next = rest.charAt(i + 1);
                var code = rest.charCodeAt(i + 1);
                if (code >= 97 && code <= 122) { /* a-z */
                    out += next.toUpperCase();
                    i += 2;
                    continue;
                }
            }
            out += ch;
            i += 1;
        }
        return out;
    }

    function makeDatasetProxy(el) {
        var target = Object.create(null);
        return new Proxy(target, {
            get: function(_t, key) {
                if (typeof key !== 'string') return undefined;
                var attr = datasetKeyToAttr(key);
                try {
                    if (el.hasAttribute && el.hasAttribute(attr)) {
                        return el.getAttribute(attr);
                    }
                } catch (e) {}
                return undefined;
            },
            set: function(_t, key, value) {
                if (typeof key !== 'string') return true;
                var attr = datasetKeyToAttr(key);
                try {
                    if (el.setAttribute) {
                        el.setAttribute(attr, value == null ? '' : String(value));
                    }
                } catch (e) {}
                return true;
            },
            has: function(_t, key) {
                if (typeof key !== 'string') return false;
                var attr = datasetKeyToAttr(key);
                try {
                    return !!(el.hasAttribute && el.hasAttribute(attr));
                } catch (e) { return false; }
            },
            deleteProperty: function(_t, key) {
                if (typeof key !== 'string') return true;
                var attr = datasetKeyToAttr(key);
                try {
                    if (el.removeAttribute) el.removeAttribute(attr);
                } catch (e) {}
                return true;
            },
            ownKeys: function(_t) {
                // Walk the element's attribute list for `data-*` keys.
                // heso doesn't expose `el.attributes` as a NamedNodeMap
                // directly, but `el.getAttributeNames()` (if present)
                // returns the same list. Fall back to an empty array
                // so iteration is well-defined.
                var keys = [];
                var seen = Object.create(null);
                try {
                    if (typeof el.getAttributeNames === 'function') {
                        var attrs = el.getAttributeNames();
                        for (var i = 0; i < attrs.length; i++) {
                            var key = attrToDatasetKey(attrs[i]);
                            if (key && !seen[key]) {
                                seen[key] = true;
                                keys.push(key);
                            }
                        }
                    }
                } catch (e) {}
                return keys;
            },
            getOwnPropertyDescriptor: function(_t, key) {
                if (typeof key !== 'string') return undefined;
                var attr = datasetKeyToAttr(key);
                try {
                    if (el.hasAttribute && el.hasAttribute(attr)) {
                        return {
                            value: el.getAttribute(attr),
                            writable: true,
                            enumerable: true,
                            configurable: true,
                        };
                    }
                } catch (e) {}
                return undefined;
            },
        });
    }

    try {
        Object.defineProperty(elementProto, 'dataset', {
            get: function() {
                return makeDatasetProxy(this);
            },
            configurable: true,
            enumerable: false,
        });
    } catch (e) {
        if (typeof console !== 'undefined' && console.warn) {
            console.warn('heso: failed to install dataset shim:', e && e.message ? e.message : e);
        }
    }

    Object.defineProperty(globalThis, '__hesoCustomElementsInstalled', {
        value: true,
        writable: false,
        configurable: false,
        enumerable: false,
    });
})();
"#;
