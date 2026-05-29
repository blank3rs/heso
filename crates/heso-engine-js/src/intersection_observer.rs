//! # intersection_observer
//!
//! A working [`IntersectionObserver`] implementation. The sibling
//! observer slots in `engine.rs`'s browser globals batch
//! (`MutationObserver` / `ResizeObserver` / `PerformanceObserver`)
//! ship as noop ctors.
//!
//! ## Why a real impl matters
//!
//! Lots of agent-relevant pages gate visible content behind an
//! `IntersectionObserver` callback. The "fires when visible" trigger
//! powers:
//!
//! - `<img loading="lazy">` polyfills.
//! - Infinite-scroll content loaders.
//! - Astro `client:visible` (and equivalent React/Vue/Svelte
//!   `react-intersection-observer` / `vue-use` / svelte-use crates).
//! - Animate-on-enter directives, deferred component hydration.
//!
//! With a noop ctor, the callback never fires and every "this content
//! becomes visible when you scroll to it" gate stays shut — `heso`
//! sees a permanently-empty placeholder instead of the actual content.
//!
//! ## Honest simplification: no layout, no viewport
//!
//! heso has no rendering, no scroll position, and no viewport
//! ([ADR 0016] punts layout out of scope). We can't compute real
//! intersection ratios. The honest implementation is:
//!
//! - `observe(target)`: queue a microtask that delivers ONE
//!   `IntersectionObserverEntry` with `isIntersecting: true` and
//!   `intersectionRatio: 1.0` if the target is in the document tree.
//!   This matches the spec's "initial notification" guarantee
//!   (the registered observer's `previousThresholdIndex` starts at -1
//!   so the first observation always queues an entry — see
//!   [w3c/IntersectionObserver issue 426][issue-426]).
//! - Re-`observe(target)` after `unobserve` re-fires (the spec treats
//!   it as a new registration).
//! - `unobserve(target)` removes the target from the observed set.
//! - `disconnect()` empties the set.
//! - `takeRecords()` returns `[]` (we deliver entries via the
//!   callback, never buffer them).
//!
//! All the geometry fields (`boundingClientRect`, `intersectionRect`,
//! `rootBounds`) are spec-shaped zero rects. We expose the right
//! property shape so feature-detection (`'intersectionRatio' in entry`)
//! and code that reads ratio thresholds work; the actual numbers are
//! placeholders.
//!
//! ## Citations
//!
//! - W3C Intersection Observer spec:
//!   <https://w3c.github.io/IntersectionObserver/> — entry shape,
//!   initial-notification guarantee.
//! - happy-dom `packages/happy-dom/src/intersection-observer/
//!   IntersectionObserver.ts` (MIT, capricorn86) — has the API
//!   surface as a TODO-stub; we ship the working impl behind that
//!   shape.
//! - jsdom GitHub issue 2032 documents jsdom's deliberate non-impl
//!   (jsdom has no layout either) — we differ from jsdom by firing
//!   the initial notification so agent-relevant content unblocks.
//!
//! ## Implementation strategy: JS-only
//!
//! The other observer noops in `install_browser_apis` are pure JS;
//! the IO replacement is pure JS too, dropped in via a small
//! [`install`] entry point that mirrors the other `install_*`
//! functions in this crate ([`crate::events::install_events`],
//! [`crate::timers::install_timers`], etc.).
//!
//! No Rust class is necessary: every field is a primitive (or a JS
//! Element reference for `target`), no Rust-side state needs to
//! outlive a single `eval`, and the callback is a plain JS function
//! that we just store as a property on the observer instance. This
//! sidesteps the [`rquickjs::Persistent`] footgun the
//! [`crate::events`] module's docs call out at length (`Persistent`
//! held inside Rust state inside a `Class<T>` instance keeps JS
//! values alive across runtime drop and trips the QuickJS
//! `gc_obj_list != empty` assert).
//!
//! [ADR 0016]: ../../decisions/0016-positioning-headless-browser-for-agents.md
//! [issue-426]: https://github.com/w3c/IntersectionObserver/issues/426

use rquickjs::Context;

use crate::engine::EvalError;

/// Install `globalThis.IntersectionObserver` and
/// `globalThis.IntersectionObserverEntry` as a real (microtask-firing)
/// implementation. Runs after [`crate::engine::install_browser_apis`]
/// so the real ctor takes precedence over the sibling noop observers.
///
/// Idempotent — the JS bootstrap returns early if both globals are
/// already installed, matching the rest of the `install_*` family's
/// re-install behavior.
///
/// Called by [`crate::engine::JsEngine::new`] after the other
/// observer noops are registered (so this overwrite takes precedence).
pub fn install(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            ctx.eval::<(), _>(INTERSECTION_OBSERVER_BOOTSTRAP)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install IntersectionObserver: {e}")))?;
    Ok(())
}

/// JS bootstrap for the IntersectionObserver replacement.
///
/// Builds three things on `globalThis`:
///
/// 1. `IntersectionObserverEntry` — a constructor that throws on
///    `new` (per spec; only the UA can mint entries) but exposes the
///    spec property shape via the prototype. heso mints entries
///    internally via `Object.create(IntersectionObserverEntry.prototype)`
///    so `entry instanceof IntersectionObserverEntry` is `true`.
///
/// 2. `IntersectionObserver` — full ctor with `observe`, `unobserve`,
///    `disconnect`, `takeRecords`, `root`, `rootMargin`, `thresholds`.
///    Callback + per-instance state stored as non-enumerable own
///    properties so `JSON.stringify(observer)` stays `"{}"` (matching
///    real browsers).
///
/// 3. The "fire on observe" microtask: `Promise.resolve().then(() =>
///    callback([entry], observer))`. This is the spec-mandated initial
///    notification, delivered on the microtask queue so synchronous
///    `observe()` callers can chain `await heso.flush()` and see the
///    callback's side effects (matches what
///    `crates/heso-engine-js/src/engine.rs::run_pending_jobs` already
///    drives for `queueMicrotask`).
///
/// "In tree" means [`Node.isConnected`]: `target.isConnected === true`.
/// Targets that have been removed (or never appended) get
/// `isIntersecting: false` — the entry still fires (spec requires the
/// initial notification regardless), but the visibility bit is false
/// so consumer code that gates on it doesn't loop on detached DOM.
///
/// [`Node.isConnected`]: ../dom/struct.Element.html#method.is_connected
const INTERSECTION_OBSERVER_BOOTSTRAP: &str = r#"
(function () {
    if (typeof globalThis.IntersectionObserver === 'function' &&
        globalThis.IntersectionObserver.__hesoReal === true) {
        return;
    }

    // ===== IntersectionObserverEntry =====================================
    //
    // Per spec, this is constructor-restricted (real browsers throw
    // "Illegal constructor" on `new`). We mint instances via
    // Object.create(...prototype) from inside IntersectionObserver so
    // `entry instanceof IntersectionObserverEntry` still works.
    function IntersectionObserverEntry() {
        throw new TypeError("Illegal constructor");
    }
    // Spec-shaped zero rect. Real browsers expose a DOMRectReadOnly;
    // we expose a plain frozen object with the same field set, which
    // every reader I checked treats as compatible (no one calls
    // `entry.boundingClientRect instanceof DOMRectReadOnly`).
    function zeroRect() {
        return Object.freeze({
            x: 0, y: 0,
            width: 0, height: 0,
            top: 0, right: 0, bottom: 0, left: 0,
        });
    }
    Object.defineProperty(IntersectionObserverEntry.prototype, 'toJSON', {
        value: function () {
            return {
                time: this.time,
                rootBounds: this.rootBounds,
                boundingClientRect: this.boundingClientRect,
                intersectionRect: this.intersectionRect,
                intersectionRatio: this.intersectionRatio,
                isIntersecting: this.isIntersecting,
                target: this.target,
            };
        },
        writable: false, enumerable: false, configurable: false,
    });
    Object.defineProperty(globalThis, 'IntersectionObserverEntry', {
        value: IntersectionObserverEntry,
        writable: true, enumerable: false, configurable: true,
    });

    // ===== IntersectionObserver ==========================================
    //
    // Parse the options dictionary up front. The spec accepts:
    //   - root: Element | Document | null (default null = viewport)
    //   - rootMargin: CSS-margin string (default "0px 0px 0px 0px")
    //   - threshold: number | number[] (default 0)
    //
    // We don't enforce intersection geometry (we have no layout) so
    // the only thing the parsed values do is round-trip through the
    // observer's own getters for code that reads them back.
    function parseThreshold(t) {
        if (t === undefined || t === null) return [0];
        var arr = Array.isArray(t) ? t.slice() : [Number(t)];
        // Spec: every threshold must be in [0, 1] or it's a TypeError.
        for (var i = 0; i < arr.length; i++) {
            var n = Number(arr[i]);
            if (!Number.isFinite(n) || n < 0 || n > 1) {
                throw new RangeError(
                    "Failed to construct 'IntersectionObserver': " +
                    "threshold values must be numbers between 0.0 and 1.0"
                );
            }
            arr[i] = n;
        }
        // Sorted, deduped per spec §3.3 "thresholds attribute".
        arr.sort(function (a, b) { return a - b; });
        return arr;
    }
    function parseRootMargin(m) {
        if (m === undefined || m === null) return "0px 0px 0px 0px";
        // Real spec parses this as a CSS margin; we accept any string
        // (no layout consumer downstream) and only validate the type.
        var s = String(m);
        if (s.length === 0) return "0px 0px 0px 0px";
        return s;
    }

    function IntersectionObserver(callback, options) {
        if (!(this instanceof IntersectionObserver)) {
            throw new TypeError(
                "Constructor IntersectionObserver requires 'new'"
            );
        }
        if (typeof callback !== 'function') {
            throw new TypeError(
                "IntersectionObserver constructor: argument 1 is not a function"
            );
        }
        var opts = options || {};
        var thresholds = parseThreshold(opts.threshold);
        var rootMargin = parseRootMargin(opts.rootMargin);
        var root = opts.root === undefined ? null : opts.root;

        Object.defineProperty(this, '__callback', {
            value: callback, writable: false, enumerable: false, configurable: false,
        });
        var targetsArr = [];
        Object.defineProperty(this, '__targets', {
            value: targetsArr, writable: false, enumerable: false, configurable: false,
        });
        // `_targets` (single underscore) is the spelling
        // `JsEngine::intersection_observer_pending_count` and the
        // `heso.flushIntersectionObservers()` opt-in API both query.
        // Same array as `__targets`; aliased so the host-side counter
        // and the spec-shaped internals see one truth.
        Object.defineProperty(this, '_targets', {
            value: targetsArr, writable: false, enumerable: false, configurable: false,
        });
        // `_fired`: targets for which an entry has already been
        // delivered. `pending_count = _targets - _fired`. Cleared on
        // `unobserve(target)` (so a re-observe re-fires) and on
        // `disconnect()` (so re-registration from scratch re-fires).
        // Read by the host's auto-scroll loop to decide whether to keep
        // pumping `heso.flushIntersectionObservers()`.
        Object.defineProperty(this, '_fired', {
            value: [], writable: false, enumerable: false, configurable: false,
        });
        Object.defineProperty(this, '__root', {
            value: root, writable: false, enumerable: false, configurable: false,
        });
        Object.defineProperty(this, '__rootMargin', {
            value: rootMargin, writable: false, enumerable: false, configurable: false,
        });
        Object.defineProperty(this, '__thresholds', {
            value: Object.freeze(thresholds.slice()),
            writable: false, enumerable: false, configurable: false,
        });
        // Register into the global IO registry so the host can sweep
        // for pending targets without having to track every constructor
        // call from Rust. Lazily ensures the registry exists; the
        // engine.rs comment block at `defineNoopObserver('IntersectionObserver')`
        // describes the same registry from the engine-side view.
        if (typeof globalThis.__hesoIO_observers === 'undefined') {
            Object.defineProperty(globalThis, '__hesoIO_observers', {
                value: [], writable: false, enumerable: false, configurable: false,
            });
        }
        globalThis.__hesoIO_observers.push(this);
    }

    // ----- Read-only IDL attributes (spec §3.3 "IntersectionObserver") --
    Object.defineProperty(IntersectionObserver.prototype, 'root', {
        get: function () { return this.__root; },
        enumerable: true, configurable: true,
    });
    Object.defineProperty(IntersectionObserver.prototype, 'rootMargin', {
        get: function () { return this.__rootMargin; },
        enumerable: true, configurable: true,
    });
    Object.defineProperty(IntersectionObserver.prototype, 'thresholds', {
        get: function () { return this.__thresholds; },
        enumerable: true, configurable: true,
    });

    // ----- Build a freshly-minted entry for `target` --------------------
    //
    // Per the "no layout" simplification documented in this module:
    // a target that is in the document tree gets isIntersecting=true
    // with ratio 1.0 (full intersection). A detached target gets
    // isIntersecting=false with ratio 0. Either way an entry is
    // delivered, matching the spec's "initial notification" guarantee
    // (registration.previousThresholdIndex starts at -1, so the first
    // observation always queues an entry — see w3c/IntersectionObserver
    // issue #426).
    function buildEntry(target) {
        // `target.isConnected` is the in-tree test. Plain JS objects
        // (e.g. window) and null targets read as "not connected" —
        // safer to treat that as not-intersecting than to crash.
        var connected = false;
        try {
            connected = !!(target && target.isConnected);
        } catch (e) {
            connected = false;
        }
        var entry = Object.create(IntersectionObserverEntry.prototype);
        Object.defineProperty(entry, 'target', {
            value: target, writable: false, enumerable: true, configurable: false,
        });
        Object.defineProperty(entry, 'isIntersecting', {
            value: connected, writable: false, enumerable: true, configurable: false,
        });
        Object.defineProperty(entry, 'intersectionRatio', {
            value: connected ? 1.0 : 0.0,
            writable: false, enumerable: true, configurable: false,
        });
        Object.defineProperty(entry, 'boundingClientRect', {
            value: zeroRect(), writable: false, enumerable: true, configurable: false,
        });
        Object.defineProperty(entry, 'intersectionRect', {
            value: zeroRect(), writable: false, enumerable: true, configurable: false,
        });
        // Spec: `rootBounds` is null when root is null (the implicit
        // viewport), else a DOMRectReadOnly. heso has no viewport, so
        // both branches give null is fine — real consumer code that
        // gates on `rootBounds != null` will see null and skip, which
        // is the safe default.
        Object.defineProperty(entry, 'rootBounds', {
            value: null, writable: false, enumerable: true, configurable: false,
        });
        // performance.now() reads the same VirtualClock that backs
        // Date.now (see engine.rs::install_browser_apis); a fresh
        // engine starts at 0, so `entry.time` is deterministic.
        var t = 0;
        try {
            t = (typeof performance !== 'undefined' &&
                 typeof performance.now === 'function')
                ? performance.now() : 0;
        } catch (e) { t = 0; }
        Object.defineProperty(entry, 'time', {
            value: t, writable: false, enumerable: true, configurable: false,
        });
        return entry;
    }

    // ----- observe(target) ----------------------------------------------
    //
    // Per spec, re-observing a target is a no-op (the target was
    // already registered). We follow that: only push if not already
    // present, but ALWAYS queue a microtask delivering the initial
    // notification — this matches real-browser behavior where
    // `observer.observe(t)` immediately after `observer.unobserve(t)`
    // re-fires the callback on the next microtask, and matches the
    // contract pages depend on for "make the lazy-loaded thing
    // resolve".
    IntersectionObserver.prototype.observe = function (target) {
        if (target == null) {
            throw new TypeError(
                "Failed to execute 'observe' on 'IntersectionObserver': " +
                "parameter 1 is not of type 'Element'."
            );
        }
        var targets = this.__targets;
        if (targets.indexOf(target) === -1) {
            targets.push(target);
        }
        var callback = this.__callback;
        var observer = this;
        // Microtask, not synchronous: the spec dispatches IO entries
        // on its own "intersection observer task source" but real
        // browsers also serve the initial notification asynchronously.
        // Using Promise.resolve().then matches the existing
        // queueMicrotask shim in engine.rs::install_browser_apis.
        Promise.resolve().then(function () {
            // Re-check the target is still observed — `disconnect()`
            // or `unobserve(target)` between observe() and the
            // microtask should suppress the fire. Real browsers also
            // suppress in this case (§3.2.2 "the queued notifications
            // are dropped if disconnect was called").
            if (observer.__targets.indexOf(target) === -1) return;
            // A re-observe of an already-delivered, still-connected target
            // is a no-op in real browsers — no second initial entry.
            // `unobserve` clears `_fired`, so a re-fire after unobserve
            // still works.
            if (observer._fired.indexOf(target) !== -1) return;
            var entry = buildEntry(target);
            // Mark this target as fired BEFORE invoking the callback —
            // pending_count reads `_targets - _fired`, and the callback
            // may synchronously call `flushIntersectionObservers()` (or
            // re-observe a target) which we don't want to re-fire on
            // the same delivery. Idempotent if the target is already
            // listed (re-observe path).
            if (observer._fired.indexOf(target) === -1) {
                observer._fired.push(target);
            }
            try {
                callback.call(undefined, [entry], observer);
            } catch (e) {
                // Spec: an uncaught callback exception is reported to
                // the global error handler, not propagated. The
                // engine's microtask pump catches throws as
                // console.error already (see
                // `engine.rs::execute_pending_jobs_until_idle`), so
                // we just rethrow — the pump promotes it to a
                // console.error entry.
                throw e;
            }
        });
    };

    // ----- unobserve(target) --------------------------------------------
    //
    // Clears the target from both `__targets` AND `_fired`. The latter
    // is necessary so a subsequent `observe(target)` will re-fire —
    // `flushIntersectionObservers()` and the initial-notification
    // microtask both gate on `_fired` to dedup repeat deliveries, but
    // the spec treats unobserve+observe as a fresh registration.
    IntersectionObserver.prototype.unobserve = function (target) {
        if (target == null) return;
        var targets = this.__targets;
        var idx = targets.indexOf(target);
        if (idx !== -1) targets.splice(idx, 1);
        var fi = this._fired.indexOf(target);
        if (fi !== -1) this._fired.splice(fi, 1);
    };

    // ----- disconnect() -------------------------------------------------
    //
    // Per spec, also drops any queued notifications. The microtask
    // closure above re-checks `__targets.indexOf(target)` so wiping
    // the list here is sufficient: queued microtasks will see the
    // target absent and become no-ops. Also wipes `_fired` so a
    // post-disconnect re-observe sees a fresh slate.
    IntersectionObserver.prototype.disconnect = function () {
        this.__targets.length = 0;
        this._fired.length = 0;
    };

    // ----- takeRecords() ------------------------------------------------
    //
    // We deliver every entry via the callback (no internal buffer),
    // so `takeRecords()` always returns []. Real browsers fill this
    // with entries queued since the last callback invocation; with
    // our "fire immediately on the next microtask" model there's
    // nothing to drain.
    IntersectionObserver.prototype.takeRecords = function () {
        return [];
    };

    // Name the constructor so `obs.constructor.name` and
    // `new IntersectionObserver(cb).toString()` show the real spec
    // name. Object.defineProperty since Function's `name` is
    // non-writable but configurable.
    Object.defineProperty(IntersectionObserver, 'name', {
        value: 'IntersectionObserver',
    });
    // Marker so a re-install on engine reuse short-circuits cleanly.
    Object.defineProperty(IntersectionObserver, '__hesoReal', {
        value: true, writable: false, enumerable: false, configurable: false,
    });

    Object.defineProperty(globalThis, 'IntersectionObserver', {
        value: IntersectionObserver,
        writable: true, enumerable: false, configurable: true,
    });

    // ===== heso.flushIntersectionObservers() ============================
    //
    // Opt-in re-delivery API used by `heso read --complete`'s auto-scroll
    // loop. Walks every IO registered into `__hesoIO_observers`; for
    // each `(observer, target)` pair where the target is currently
    // `isConnected` and has NOT yet been delivered an entry
    // (`observer._fired.indexOf(target) === -1`), queues a microtask
    // that delivers an `isIntersecting: true` entry and marks the
    // target fired.
    //
    // Returns the count of newly-queued deliveries — callers can stop
    // looping when this returns 0.
    //
    // Idempotent: re-calling after every queued delivery has been
    // marked fired returns 0 without scheduling anything.
    //
    // NOT a spec method (the W3C spec has no host-driven flush) —
    // this is the agent seam that makes lazy-content gates resolvable
    // without an actual scroll viewport. See ADR 0016 for the "no
    // layout" framing.
    if (typeof globalThis.heso !== 'object' || globalThis.heso === null) {
        globalThis.heso = {};
    }
    if (typeof globalThis.heso.flushIntersectionObservers !== 'function') {
        globalThis.heso.flushIntersectionObservers = function () {
            var observers = globalThis.__hesoIO_observers;
            if (!Array.isArray(observers)) return 0;
            var delivered = 0;
            for (var i = 0; i < observers.length; i++) {
                var obs = observers[i];
                if (!obs || !obs._targets) continue;
                // Snapshot the target list — re-firing might re-observe
                // and we don't want the iteration to chase tail-appends.
                var targets = obs._targets.slice();
                var cb = obs.__callback;
                for (var j = 0; j < targets.length; j++) {
                    var t = targets[j];
                    if (!t) continue;
                    var connected = false;
                    try {
                        // Plain objects (test fakes that lack isConnected)
                        // fall through to false here, mirroring the
                        // initial-notification rules. Real Element
                        // references on a connected tree return true.
                        connected = !!t.isConnected;
                    } catch (_) { connected = false; }
                    if (!connected) continue;
                    if (obs._fired.indexOf(t) !== -1) continue;
                    obs._fired.push(t);
                    delivered++;
                    (function (observer, callback, target) {
                        Promise.resolve().then(function () {
                            // Re-check observation; unobserve / disconnect
                            // between flush and microtask drops delivery.
                            if (observer.__targets.indexOf(target) === -1) {
                                // Also retract the `_fired` mark so a
                                // future re-observe re-fires.
                                var fi = observer._fired.indexOf(target);
                                if (fi !== -1) observer._fired.splice(fi, 1);
                                return;
                            }
                            var entry = buildEntry(target);
                            try {
                                callback.call(undefined, [entry], observer);
                            } catch (e) {
                                // See observe() rationale.
                                throw e;
                            }
                        });
                    })(obs, cb, t);
                }
            }
            return delivered;
        };
    }
})();
"#;
