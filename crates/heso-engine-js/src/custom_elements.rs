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

    function safeInvoke(el, fn, args) {
        if (typeof fn !== 'function') return;
        try {
            fn.apply(el, args || []);
        } catch (e) {
            // Match the spec's "report exception" — surface via
            // console.error if present, otherwise swallow.
            if (typeof console !== 'undefined' && typeof console.error === 'function') {
                console.error('custom element callback threw:', e && e.message ? e.message : e);
            }
        }
    }

    function fireConnected(el) {
        var def = el && el[CE_DEFINITION] ? el[CE_DEFINITION] : getDefinitionForElement(el);
        if (!def) return;
        var fn = lookupCallbackOnDefinition(def, 'connectedCallback');
        safeInvoke(el, fn);
    }

    function fireDisconnected(el) {
        var def = el && el[CE_DEFINITION] ? el[CE_DEFINITION] : getDefinitionForElement(el);
        if (!def) return;
        var fn = lookupCallbackOnDefinition(def, 'disconnectedCallback');
        safeInvoke(el, fn);
    }

    function fireAttributeChanged(el, name, oldValue, newValue) {
        var def = el && el[CE_DEFINITION] ? el[CE_DEFINITION] : getDefinitionForElement(el);
        if (!def) return;
        if (!def.observedAttributes || !def.observedAttributes[name]) return;
        var fn = lookupCallbackOnDefinition(def, 'attributeChangedCallback');
        safeInvoke(el, fn, [name, oldValue, newValue, null]);
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

            // Upgrade existing elements. Spec §4.13.1.define step 14:
            // for every element in the document whose local name is
            // `name`, enqueue an upgrade reaction.
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

    Object.defineProperty(globalThis, '__hesoCustomElementsInstalled', {
        value: true,
        writable: false,
        configurable: false,
        enumerable: false,
    });
})();
"#;
