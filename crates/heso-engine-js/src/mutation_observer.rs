//! WHATWG DOM § 4.3 `MutationObserver`.
//!
//! ## What this module gives you
//!
//! - `globalThis.MutationObserver` — a real constructor whose
//!   `observe(target, options)` / `disconnect()` / `takeRecords()`
//!   surface matches the spec and whose callback actually fires as a
//!   batched microtask when the observed DOM subtree mutates.
//! - `globalThis.MutationRecord` — exposed for `instanceof` checks and
//!   so feature-detection (`'MutationRecord' in globalThis`) succeeds.
//!
//! ## Why JS-side, not Rust-side
//!
//! Mirrors the design in [`crate::custom_elements`]: the lifecycle
//! hooks have to live in a wrapper around `Element.prototype.*` so they
//! see every Rust-backed DOM mutation, AND the per-method wrapping must
//! happen JS-side because most mutation entry points (`set_attribute`,
//! `set_text_content`, the `innerHTML` setter) are `#[rquickjs::class]`
//! methods on a `Class<Element>` instance and don't take a `Ctx` in
//! their signature — we can't broadcast a mutation event to a
//! Rust-side observer registry from there without a wide signature
//! rewrite touching every DOM mutation method.
//!
//! The JS-wrapper pattern has another upside: the same Element-prototype
//! wrappers `customElements` already installs for `attributeChangedCallback`
//! / `connectedCallback` are the right hook points. We layer the MO
//! dispatch onto them rather than introducing a parallel notification
//! path through Rust.
//!
//! ## Spec corners we simplify
//!
//! - **No CharacterData node distinction.** Phase 1B uses one `Element`
//!   wrapper for element + text + comment nodes (see [`crate::dom`]).
//!   `characterData` observation triggers on text-node `textContent`
//!   mutations via the existing setter, treating "text node whose
//!   `textContent` was reassigned" as the spec's CharacterData
//!   mutation. Good enough for framework reactivity bookkeeping.
//! - **No `innerHTML`-setter mutations.** Per
//!   [`crate::custom_elements`]'s same constraint: rquickjs 0.11 emits
//!   class accessors with `configurable: false`, so we can't redefine
//!   the `innerHTML` setter from JS. Pages that mutate the tree via
//!   `el.innerHTML = '...'` don't fire MO callbacks today. This
//!   matches the existing custom-elements lifecycle gap and is
//!   tracked as a shared limitation — frameworks (Lit, Stencil, Vue,
//!   Solid) route through `createElement` + `appendChild`, which IS
//!   wrapped.
//! - **`attributeNamespace` is always `null`.** Phase 1B has no real
//!   XML-namespace support (`createElementNS` ignores its namespace
//!   argument). The field is exposed for spec compatibility on the
//!   `MutationRecord` shape so framework `if (r.attributeNamespace)`
//!   branches don't crash.
//! - **Microtask batching is implemented via `Promise.resolve().then`.**
//!   Spec specifies a singleton "notify mutation observers" microtask;
//!   we approximate via a per-observer-queue scheduling flag so each
//!   observer fires its callback at most once per microtask drain.
//!   Indistinguishable from spec at the framework level.
//!
//! ## OSS cross-referenced
//!
//! - [jsdom][jd] `lib/jsdom/living/mutation-observer/MutationObserver-impl.js`
//!   and `lib/jsdom/living/helpers/mutation-observers.js` (MIT) — for
//!   the `queueMutationRecord` shape, the `attributeFilter` matching
//!   logic, and the `mutationObserverMicrotaskQueueFlag` scheduling
//!   pattern.
//! - [happy-dom][hd] `packages/happy-dom/src/mutation-observer/` (MIT)
//!   — for the observe-options validation rules
//!   (`attributeOldValue` requires `attributes`, etc.) and the
//!   `takeRecords()` queue-drain semantics.
//!
//! We don't vendor those — the algorithm is short and the spec work
//! they did was the value. Our JS-in-Rust install model is different
//! from jsdom's webidl2js codegen, and happy-dom's wrap-every-node-op
//! at the JS layer doesn't survive across the Rust/JS boundary cleanly.
//!
//! [jd]: https://github.com/jsdom/jsdom/blob/master/lib/jsdom/living/mutation-observer/MutationObserver-impl.js
//! [hd]: https://github.com/capricorn86/happy-dom/tree/master/packages/happy-dom/src/mutation-observer

use rquickjs::Context;

use crate::engine::EvalError;

/// Install the `MutationObserver` and `MutationRecord` globals on
/// `ctx.globals()`, wrap `Element.prototype.*` mutation methods so
/// they queue MutationRecords, and arrange for queued records to fire
/// each observer's callback as a batched microtask.
///
/// Must be called **after** [`crate::dom::register_classes`] (so
/// `Element.prototype` exists), **after**
/// [`crate::custom_elements::install_custom_elements`] (so this
/// module's wrappers stack OVER the custom-elements wrappers — both
/// wrappers fire on every mutation; the MO wrapper sits inside the
/// custom-elements wrapper because attribute-change semantics need
/// the post-mutation read of `getAttribute` to compute newValue), and
/// **after** the `install_browser_apis` JS bootstrap which installs
/// the noop `MutationObserver` shim that this call overwrites.
///
/// Idempotent — a one-shot sentinel inside the bootstrap skips
/// re-wrapping `Element.prototype` if called twice on the same engine.
pub(crate) fn install(context: &Context) -> Result<(), EvalError> {
    use rquickjs::CatchResultExt;
    context
        .with(|ctx| -> Result<(), EvalError> {
            ctx.eval::<(), _>(MUTATION_OBSERVER_BOOTSTRAP)
                .catch(&ctx)
                .map_err(|e| {
                    EvalError::Engine(format!("eval mutation-observer bootstrap: {e}"))
                })?;
            Ok(())
        })?;
    Ok(())
}

/// JS source for the real `MutationObserver` + `MutationRecord` and
/// the `Element.prototype` mutation-method wrappers.
///
/// Source-of-record references:
///
/// - WHATWG DOM § 4.3 "Mutation observers" — the algorithm for
///   "queue a mutation record" (§4.3.1) and the
///   "notify mutation observers" (§4.3.2) microtask. Our scheduling
///   approximates §4.3.2 via per-observer `Promise.resolve().then(...)`.
/// - WHATWG DOM § 4.4.1 "Mutation algorithms" — sites that produce
///   childList records: `insert`, `remove`, `append`, `replace`.
/// - WHATWG DOM § 4.4.5 "Interface Element" — `setAttributeNS`
///   produces attributes records.
const MUTATION_OBSERVER_BOOTSTRAP: &str = r#"
(function() {
    if (globalThis.__hesoMutationObserverInstalled) return;
    globalThis.__hesoMutationObserverInstalled = true;

    // ---------------------------------------------------------------
    // Observer registry.
    //
    // Each observe() call registers a (observer, target, options)
    // tuple. When any DOM mutation hook fires below, we walk the
    // registry and queue a MutationRecord into each matching
    // observer's `_pendingRecords` array. A one-shot scheduling
    // flag (`_microtaskScheduled`) prevents each observer's callback
    // from being scheduled more than once per microtask drain — the
    // spec calls this the "notify mutation observers" microtask.
    //
    // We hold observer references as plain object references (NOT
    // WeakRef) because the spec keeps an observer alive while it has
    // any registered targets; user JS that drops the only reference
    // to the observer expects subsequent disconnects via the target
    // to still work. The list is small (one or a few per page) so
    // the memory cost is negligible.
    // ---------------------------------------------------------------
    var observerRegistry = [];

    // Hidden "observed target" registry per observer. Each entry:
    //   { observer, target, options }
    // Multiple observe() calls from the same observer with different
    // targets accumulate; observe() with the SAME target replaces the
    // previous options for that (observer, target) pair (spec
    // §4.3.1 step 5).

    // ---------------------------------------------------------------
    // MutationRecord — the per-mutation record passed to callbacks.
    //
    // Spec shape (WHATWG DOM § 4.3.3):
    //   readonly attribute DOMString type;
    //   readonly attribute Node target;
    //   [SameObject] readonly attribute NodeList addedNodes;
    //   [SameObject] readonly attribute NodeList removedNodes;
    //   readonly attribute Node? previousSibling;
    //   readonly attribute Node? nextSibling;
    //   readonly attribute DOMString? attributeName;
    //   readonly attribute DOMString? attributeNamespace;
    //   readonly attribute DOMString? oldValue;
    //
    // We back the fields with plain own properties (writable for
    // the constructor's convenience; the spec calls them read-only
    // but frameworks don't write to them). Lists are plain Arrays
    // because we don't have a NodeList class; framework code reads
    // them as iterables, which Arrays satisfy.
    // ---------------------------------------------------------------
    function MutationRecord() {
        // Defaults match the spec; the constructor is called by the
        // dispatch helpers below with the relevant fields filled in.
        this.type = '';
        this.target = null;
        this.addedNodes = [];
        this.removedNodes = [];
        this.previousSibling = null;
        this.nextSibling = null;
        this.attributeName = null;
        this.attributeNamespace = null;
        this.oldValue = null;
    }
    Object.defineProperty(MutationRecord, 'name', { value: 'MutationRecord' });
    globalThis.MutationRecord = MutationRecord;

    function makeRecord(type, target, fields) {
        var r = new MutationRecord();
        r.type = type;
        r.target = target;
        if (fields) {
            if (fields.addedNodes) r.addedNodes = fields.addedNodes;
            if (fields.removedNodes) r.removedNodes = fields.removedNodes;
            if ('previousSibling' in fields) r.previousSibling = fields.previousSibling;
            if ('nextSibling' in fields) r.nextSibling = fields.nextSibling;
            if ('attributeName' in fields) r.attributeName = fields.attributeName;
            if ('attributeNamespace' in fields) r.attributeNamespace = fields.attributeNamespace;
            if ('oldValue' in fields) r.oldValue = fields.oldValue;
        }
        return r;
    }

    // ---------------------------------------------------------------
    // MutationObserver class.
    //
    // Spec surface:
    //   constructor(MutationCallback callback);
    //   undefined observe(Node target, MutationObserverInit options = {});
    //   undefined disconnect();
    //   sequence<MutationRecord> takeRecords();
    //
    // MutationObserverInit:
    //   boolean childList = false;
    //   boolean attributes;       // defaults to true iff attributeFilter
    //                             // or attributeOldValue is set
    //   boolean characterData;    // defaults to true iff
    //                             // characterDataOldValue is set
    //   boolean subtree = false;
    //   boolean attributeOldValue;
    //   boolean characterDataOldValue;
    //   sequence<DOMString> attributeFilter;
    //
    // We expose the same shape.
    // ---------------------------------------------------------------
    function MutationObserver(callback) {
        if (!(this instanceof MutationObserver)) {
            throw new TypeError("Constructor MutationObserver requires 'new'");
        }
        if (typeof callback !== 'function') {
            throw new TypeError("MutationObserver constructor: argument 1 is not a function");
        }
        // Use non-enumerable own properties so the observer is JSON-
        // safe and Object.keys(obs) returns [] (matches the IDL).
        Object.defineProperty(this, '_callback', {
            value: callback, writable: false, enumerable: false, configurable: false,
        });
        // List of {target, options} entries for this observer.
        Object.defineProperty(this, '_targets', {
            value: [], writable: false, enumerable: false, configurable: false,
        });
        // Pending records queue (drained by takeRecords() or by the
        // batched microtask).
        Object.defineProperty(this, '_pendingRecords', {
            value: [], writable: false, enumerable: false, configurable: false,
        });
        // Per-observer scheduling flag: true between "first record
        // queued this drain" and "callback fired". Reset to false at
        // the start of the callback so the next mutation queues a
        // fresh microtask. Mirrors jsdom's
        // `mutationObserverMicrotaskQueueFlag` but kept per-observer
        // for simpler scheduling.
        Object.defineProperty(this, '_microtaskScheduled', {
            value: false, writable: true, enumerable: false, configurable: false,
        });
    }
    Object.defineProperty(MutationObserver, 'name', { value: 'MutationObserver' });

    MutationObserver.prototype.observe = function observe(target, options) {
        if (target == null) {
            throw new TypeError("MutationObserver.observe: target is required");
        }
        options = options || {};

        // Normalize the options dict per spec § 4.3.1 step 1-5.
        var attributeOldValue = options.attributeOldValue === true;
        var characterDataOldValue = options.characterDataOldValue === true;
        var attributeFilter = options.attributeFilter;
        var hasAttributeFilter = Array.isArray(attributeFilter);

        // `attributes` defaults to true iff attributeOldValue OR
        // attributeFilter is set.
        var attributes;
        if (typeof options.attributes === 'boolean') {
            attributes = options.attributes;
        } else {
            attributes = attributeOldValue || hasAttributeFilter;
        }
        // `characterData` defaults to true iff characterDataOldValue
        // is set.
        var characterData;
        if (typeof options.characterData === 'boolean') {
            characterData = options.characterData;
        } else {
            characterData = characterDataOldValue;
        }
        var childList = options.childList === true;
        var subtree = options.subtree === true;

        // Spec § 4.3.1 step 3: throw TypeError if all three flags
        // (childList, attributes, characterData) are false.
        if (!childList && !attributes && !characterData) {
            throw new TypeError(
                "MutationObserver.observe: at least one of childList, attributes, " +
                "or characterData must be true"
            );
        }
        // Spec § 4.3.1 step 4: throw if attributeOldValue is true but
        // attributes is false.
        if (attributeOldValue && !attributes) {
            throw new TypeError(
                "MutationObserver.observe: attributeOldValue is set but attributes is false"
            );
        }
        // Spec § 4.3.1 step 4 cont.: throw if attributeFilter is set
        // but attributes is false.
        if (hasAttributeFilter && !attributes) {
            throw new TypeError(
                "MutationObserver.observe: attributeFilter is set but attributes is false"
            );
        }
        // Spec § 4.3.1 step 5: throw if characterDataOldValue is
        // set but characterData is false.
        if (characterDataOldValue && !characterData) {
            throw new TypeError(
                "MutationObserver.observe: characterDataOldValue is set but characterData is false"
            );
        }

        // Lowercase the attributeFilter entries — the spec is
        // case-sensitive but in practice HTML attributes are
        // lowercase, and the setAttribute wrapper lowercases the
        // name before queuing the record, so matching has to be on
        // the same casing.
        var normalizedFilter = null;
        if (hasAttributeFilter) {
            normalizedFilter = [];
            for (var i = 0; i < attributeFilter.length; i++) {
                normalizedFilter.push(String(attributeFilter[i]).toLowerCase());
            }
        }

        var normalized = {
            childList: childList,
            attributes: attributes,
            characterData: characterData,
            subtree: subtree,
            attributeOldValue: attributeOldValue,
            characterDataOldValue: characterDataOldValue,
            attributeFilter: normalizedFilter,
        };

        // Spec § 4.3.1 step 6: if there is a registered observer for
        // this target whose observer is `this`, replace its options.
        // Otherwise append a new registered observer.
        var found = false;
        for (var j = 0; j < this._targets.length; j++) {
            if (this._targets[j].target === target) {
                this._targets[j].options = normalized;
                found = true;
                break;
            }
        }
        if (!found) {
            this._targets.push({ target: target, options: normalized });
        }
        // Ensure this observer is in the global registry.
        if (observerRegistry.indexOf(this) === -1) {
            observerRegistry.push(this);
        }
    };

    MutationObserver.prototype.disconnect = function disconnect() {
        // Spec § 4.3.1 "disconnect" steps 1-2: remove all registered
        // observers AND empty the pending records queue.
        this._targets.length = 0;
        this._pendingRecords.length = 0;
        var idx = observerRegistry.indexOf(this);
        if (idx >= 0) observerRegistry.splice(idx, 1);
    };

    MutationObserver.prototype.takeRecords = function takeRecords() {
        // Spec § 4.3.1 "takeRecords": return the queued records and
        // clear the queue. Note the spec is "let records be a clone
        // of this's record queue; empty this's record queue; return
        // records." We slice then truncate.
        var records = this._pendingRecords.slice();
        this._pendingRecords.length = 0;
        return records;
    };
    Object.defineProperty(globalThis, 'MutationObserver', {
        value: MutationObserver, writable: true, configurable: true, enumerable: false,
    });

    // ---------------------------------------------------------------
    // sameOrAncestor(ancestor, node) — true if `node` is in the
    // subtree rooted at `ancestor`, OR is `ancestor` itself.
    //
    // Cross-wrapper identity is load-bearing: `document.querySelector`
    // and friends return a fresh Element wrapper around the same
    // underlying NodeId every call, so `===` between two wrappers of
    // the same node fails. The Rust-side `Node.contains(other)` method
    // on Element does the correct NodeId-keyed walk (see
    // [`crate::dom::Element::contains`]), and the DOM spec defines
    // `contains` as "is `other` an inclusive descendant of `self`",
    // which is exactly the subtree-match check the MO algorithm needs.
    // We use it here so the subtree case picks up descendants
    // produced by fresh wrappers — the only way framework code
    // routinely supplies the observed root (e.g. via parentNode walks,
    // querySelector results, or rendering pipelines that re-acquire
    // wrappers per render).
    //
    // Falls back to `===` only if either operand lacks `contains` (the
    // non-Element nodes, theoretically — Phase 1B's Element wrapper
    // covers element + text + comment, so this fallback is mostly
    // defensive).
    // ---------------------------------------------------------------
    function sameOrAncestor(ancestor, node) {
        if (!node || !ancestor) return false;
        if (ancestor === node) return true;
        if (typeof ancestor.contains === 'function') {
            try { return ancestor.contains(node); } catch (e) { return false; }
        }
        return false;
    }

    // ---------------------------------------------------------------
    // queueMutation(kind, target, fields) — queue a MutationRecord
    // into every observer that should see this mutation, and
    // schedule the notify-microtask if not already scheduled.
    //
    // `kind` is one of "childList" | "attributes" | "characterData".
    // The matching rules per spec § 4.3.2 step 3:
    //
    //   For each registered observer (obs, observedTarget, opts):
    //     if (observedTarget === target) or
    //        (opts.subtree && ancestorContains(observedTarget, target)):
    //       if kind === "attributes" && !opts.attributes: skip
    //       if kind === "attributes" && opts.attributeFilter:
    //         if filter doesn't contain fields.attributeName: skip
    //       if kind === "characterData" && !opts.characterData: skip
    //       if kind === "childList" && !opts.childList: skip
    //       build a record (cloning oldValue only when opts asks for it)
    //       push onto obs._pendingRecords
    //       if !obs._microtaskScheduled:
    //         schedule the callback via Promise.resolve().then(...)
    //         set _microtaskScheduled = true
    // ---------------------------------------------------------------
    function queueMutation(kind, target, fields) {
        if (observerRegistry.length === 0) return; // fast path
        for (var i = 0; i < observerRegistry.length; i++) {
            var obs = observerRegistry[i];
            for (var j = 0; j < obs._targets.length; j++) {
                var entry = obs._targets[j];
                var opts = entry.options;
                // Does this entry observe `target`?
                //
                // Identity check uses `===` first (cheap, works for
                // wrappers minted in the same call as observe()). For
                // cross-wrapper identity — `document.body` returning
                // a fresh wrapper each access, `parentNode` returning
                // a fresh wrapper per chain step — we use the
                // NodeId-keyed `contains` method on Element. Per the
                // DOM spec `node.contains(other)` is true iff `other`
                // is `node` itself OR an inclusive descendant. For
                // non-subtree we still need identity-only, which we
                // get by also checking the reverse direction (only
                // true when both wrappers point at the same NodeId).
                var matches = false;
                if (entry.target === target) {
                    matches = true;
                } else if (opts.subtree) {
                    matches = sameOrAncestor(entry.target, target);
                } else if (typeof entry.target.contains === 'function' &&
                           typeof target.contains === 'function') {
                    try {
                        matches = entry.target.contains(target) &&
                                  target.contains(entry.target);
                    } catch (e) { matches = false; }
                }
                if (!matches) continue;
                // Type-gate: each kind has a matching option.
                if (kind === 'attributes' && !opts.attributes) continue;
                if (kind === 'characterData' && !opts.characterData) continue;
                if (kind === 'childList' && !opts.childList) continue;
                // attributeFilter — case-folded matching (see observe()).
                if (kind === 'attributes' && opts.attributeFilter !== null) {
                    var attrName = fields && fields.attributeName;
                    if (attrName == null) continue;
                    var found = false;
                    var lname = String(attrName).toLowerCase();
                    for (var k = 0; k < opts.attributeFilter.length; k++) {
                        if (opts.attributeFilter[k] === lname) { found = true; break; }
                    }
                    if (!found) continue;
                }
                // Construct the record. Honor opts.attributeOldValue /
                // opts.characterDataOldValue — the spec says oldValue
                // is null on the queued record UNLESS the observer
                // asked for it.
                var rec_fields = {};
                if (fields) {
                    if (fields.addedNodes) rec_fields.addedNodes = fields.addedNodes;
                    if (fields.removedNodes) rec_fields.removedNodes = fields.removedNodes;
                    if ('previousSibling' in fields) rec_fields.previousSibling = fields.previousSibling;
                    if ('nextSibling' in fields) rec_fields.nextSibling = fields.nextSibling;
                    if (kind === 'attributes') {
                        rec_fields.attributeName = fields.attributeName;
                        rec_fields.attributeNamespace = fields.attributeNamespace || null;
                        if (opts.attributeOldValue) {
                            rec_fields.oldValue = fields.oldValue;
                        }
                    } else if (kind === 'characterData') {
                        if (opts.characterDataOldValue) {
                            rec_fields.oldValue = fields.oldValue;
                        }
                    }
                }
                var record = makeRecord(kind, target, rec_fields);
                obs._pendingRecords.push(record);
                if (!obs._microtaskScheduled) {
                    obs._microtaskScheduled = true;
                    // Capture per-iteration to avoid closure-over-loop-var.
                    (function(o) {
                        Promise.resolve().then(function() {
                            // Reset BEFORE firing so a mutation
                            // inside the callback can re-schedule.
                            o._microtaskScheduled = false;
                            if (o._pendingRecords.length === 0) return;
                            var batch = o._pendingRecords.slice();
                            o._pendingRecords.length = 0;
                            try {
                                // Spec: callback receives (records, observer).
                                o._callback(batch, o);
                            } catch (e) {
                                if (typeof console !== 'undefined' && console.error) {
                                    console.error('MutationObserver callback threw:',
                                        e && e.message ? e.message : e);
                                }
                            }
                        });
                    })(obs);
                }
                // Per spec one record per (observer, mutation) pair —
                // we found one matching registered observer for this
                // observer, no need to keep iterating its other
                // target entries with the same kind. Break to outer
                // loop so the next observer is considered.
                break;
            }
        }
    }
    // Expose internal queueMutation under a hidden global so future
    // Rust-side mutation paths can plug in without re-implementing
    // the wrap layer. Not part of the public surface; users should
    // not poke at this.
    Object.defineProperty(globalThis, '__hesoQueueMutation', {
        value: queueMutation, writable: false, configurable: false, enumerable: false,
    });

    // ---------------------------------------------------------------
    // Wrap Element.prototype methods so DOM mutations queue records.
    //
    // We rely on document.createElement('div') to reach
    // Element.prototype, same dance install_custom_elements uses.
    // ---------------------------------------------------------------
    if (typeof document === 'undefined') return;
    var probe = document.createElement('div');
    if (!probe) return;
    var elementProto = Object.getPrototypeOf(probe);
    if (!elementProto) return;

    // appendChild — produces a childList record with addedNodes
    // = [child]. previousSibling is the (old) lastChild before
    // append; nextSibling is null (append puts node at end).
    var origAppendChild = elementProto.appendChild;
    if (typeof origAppendChild === 'function') {
        elementProto.appendChild = function(child) {
            // Capture previousSibling BEFORE the append (spec §4.4.1).
            var prev = null;
            try { prev = this.lastChild || null; } catch (e) { /* tolerated */ }
            var result = origAppendChild.call(this, child);
            queueMutation('childList', this, {
                addedNodes: [child],
                removedNodes: [],
                previousSibling: prev,
                nextSibling: null,
            });
            return result;
        };
    }

    // insertBefore — childList record. previousSibling is the
    // node before refNode (or lastChild before insert if refNode
    // is null); nextSibling is refNode (or null).
    var origInsertBefore = elementProto.insertBefore;
    if (typeof origInsertBefore === 'function') {
        elementProto.insertBefore = function(newNode, refNode) {
            var prev = null;
            var next = null;
            try {
                if (refNode != null) {
                    next = refNode;
                    prev = refNode.previousSibling || null;
                } else {
                    prev = this.lastChild || null;
                    next = null;
                }
            } catch (e) { /* tolerated */ }
            var result = origInsertBefore.call(this, newNode, refNode);
            queueMutation('childList', this, {
                addedNodes: [newNode],
                removedNodes: [],
                previousSibling: prev,
                nextSibling: next,
            });
            return result;
        };
    }

    // removeChild — childList record. previousSibling/nextSibling
    // captured BEFORE removal because the spec says they reference
    // the pre-mutation tree position.
    var origRemoveChild = elementProto.removeChild;
    if (typeof origRemoveChild === 'function') {
        elementProto.removeChild = function(child) {
            var prev = null;
            var next = null;
            try {
                if (child != null) {
                    prev = child.previousSibling || null;
                    next = child.nextSibling || null;
                }
            } catch (e) { /* tolerated */ }
            var result = origRemoveChild.call(this, child);
            queueMutation('childList', this, {
                addedNodes: [],
                removedNodes: [child],
                previousSibling: prev,
                nextSibling: next,
            });
            return result;
        };
    }

    // setAttribute — attributes record. Capture oldValue BEFORE the
    // set so observers with attributeOldValue see the pre-mutation
    // value. Lowercase the attribute name to match the existing
    // custom-elements path (HTML attrs are ASCII-case-insensitive).
    var origSetAttribute = elementProto.setAttribute;
    if (typeof origSetAttribute === 'function') {
        elementProto.setAttribute = function(name, value) {
            var lname = String(name).toLowerCase();
            var oldValue = null;
            try {
                if (this.hasAttribute && this.hasAttribute(lname)) {
                    oldValue = this.getAttribute(lname);
                }
            } catch (e) { /* tolerated */ }
            var result = origSetAttribute.call(this, name, value);
            queueMutation('attributes', this, {
                attributeName: lname,
                attributeNamespace: null,
                oldValue: oldValue,
            });
            return result;
        };
    }

    // removeAttribute — attributes record. Only fires if the
    // attribute was actually present (matches the spec's "if
    // element had no attribute named name, do nothing" clause).
    var origRemoveAttribute = elementProto.removeAttribute;
    if (typeof origRemoveAttribute === 'function') {
        elementProto.removeAttribute = function(name) {
            var lname = String(name).toLowerCase();
            var had = false;
            var oldValue = null;
            try {
                if (this.hasAttribute && this.hasAttribute(lname)) {
                    had = true;
                    oldValue = this.getAttribute(lname);
                }
            } catch (e) { /* tolerated */ }
            var result = origRemoveAttribute.call(this, name);
            if (had) {
                queueMutation('attributes', this, {
                    attributeName: lname,
                    attributeNamespace: null,
                    oldValue: oldValue,
                });
            }
            return result;
        };
    }

    // textContent setter — characterData record on text nodes,
    // childList record on element nodes (the spec says assigning
    // textContent replaces all children with a single text node,
    // which is a childList mutation). Same configurability caveat
    // as innerHTML — rquickjs 0.11 emits class accessors with
    // configurable: false. Use try/catch so a failed redefine
    // doesn't break engine init; if it fails, textContent mutations
    // simply don't fire MO (best-effort, matches the
    // custom-elements innerHTML gap).
    try {
        var descTextContent = Object.getOwnPropertyDescriptor(elementProto, 'textContent');
        if (descTextContent && typeof descTextContent.set === 'function' && descTextContent.configurable) {
            var origTextSetter = descTextContent.set;
            var origTextGetter = descTextContent.get;
            Object.defineProperty(elementProto, 'textContent', {
                configurable: true,
                enumerable: descTextContent.enumerable,
                get: origTextGetter,
                set: function(value) {
                    var oldValue = '';
                    try {
                        if (typeof origTextGetter === 'function') {
                            oldValue = origTextGetter.call(this);
                        }
                    } catch (e) { /* tolerated */ }
                    origTextSetter.call(this, value);
                    // Text node: characterData. Element / other: childList
                    // (per spec, but we don't have a stable nodeType-3
                    // discriminator that survives the wrapper boundary
                    // cleanly across all paths — use nodeType=3 when
                    // available, fall back to childList).
                    var nt = 0;
                    try { nt = this.nodeType; } catch (e) {}
                    if (nt === 3) {
                        queueMutation('characterData', this, {
                            oldValue: oldValue,
                        });
                    } else {
                        // childList: the assignment removed all children
                        // and added a single text node. We don't have a
                        // handle to the replacement text node, so emit
                        // a synthetic record with empty added/removed —
                        // frameworks that observe childList primarily
                        // care that "something changed under here," and
                        // a re-read of `target.textContent` via the
                        // unchanged getter shows the new value.
                        queueMutation('childList', this, {
                            addedNodes: [],
                            removedNodes: [],
                            previousSibling: null,
                            nextSibling: null,
                        });
                    }
                },
            });
        }
    } catch (e) {
        // Property non-configurable (rquickjs 0.11 emits class
        // accessors that way for most properties). textContent
        // mutations don't fire MO — same limitation as the
        // innerHTML setter (see custom_elements.rs).
    }
})();
"#;
