//! # history
//!
//! `globalThis.history` plus the `window`-level event surface that
//! SPA routers (Next.js, React Router v6, Vue Router) gate hydration
//! on: `pushState` / `replaceState` to navigate without a full page
//! load, then `popstate` on `window` when the user (or programmatic
//! `history.back()`/`forward()`/`go(N)`) traverses the in-memory
//! history stack.
//!
//! ## What this installs
//!
//! - `globalThis.history` — `pushState(state, _, url)`,
//!   `replaceState(state, _, url)`, `back()`, `forward()`, `go(delta)`,
//!   plus the `length`, `state`, and `scrollRestoration` properties.
//! - `globalThis.PopStateEvent` — `new PopStateEvent('popstate', {state})`,
//!   exposing `event.state`. Sub-classed on top of [`Event`](crate::events::Event)
//!   via the same `Object.defineProperty` shim pattern `CustomEvent`
//!   uses (see [`crate::events::install_events`]).
//! - `globalThis.addEventListener`, `removeEventListener`,
//!   `dispatchEvent` — `window.addEventListener('popstate', ...)` is the
//!   gate, so the global itself needs to be a listener surface. We
//!   reuse the same listener-record shape `EventTarget` uses
//!   (`{callback, capture, once}` in an array per type), stored under
//!   `globalThis.__listeners`. Per-eval reset is not desirable here —
//!   listeners registered by `<script>` on page-load must survive
//!   subsequent `eval` calls (e.g. `sess.eval("history.back()")`).
//!   The map is plain JS state on `globalThis` so it does survive.
//!
//! ## URL resolution
//!
//! `pushState(state, _, url)` accepts both absolute and relative URLs.
//! Resolution against the current `location.href` happens Rust-side via
//! [`url::Url::parse`] + [`url::Url::join`] (the JS engine has no
//! `URL` global). The bridge is `__hesoResolveUrl(input, base)` — a
//! Rust Func bound on `globalThis` that returns the resolved absolute
//! URL as a string.
//!
//! `__hesoSetLocation(absHref)` is the matching half — mutates the
//! existing `globalThis.location` object's `href`/`pathname`/`search`/
//! `hash`/etc. fields **in place** so cached references
//! (`const loc = window.location; ... loc.pathname`) stay live across
//! `pushState`. This differs from
//! [`crate::engine::install_location`], which replaces the whole
//! object (used only at engine init and on cross-document navigation
//! via [`crate::JsEngine::set_base_url`]).
//!
//! ## Spec contract pinned in tests (`tests/history.rs`)
//!
//! Per MDN's `PopStateEvent` page and the WHATWG HTML spec
//! (§ 7.2.7.2):
//!
//! - `pushState` updates `history.state` and `history.length` and
//!   `location.*`, but does NOT dispatch `popstate`.
//! - `replaceState` updates state and `location.*` but does NOT change
//!   `history.length` or dispatch `popstate`.
//! - `back()` / `forward()` / `go(N)` walk within the in-memory stack,
//!   restore `location.*` and `history.state` from the new entry, and
//!   dispatch a `popstate` event on `window` with the restored state.
//!
//! ## OSS reviewed (decision: build fresh)
//!
//! jsdom's `History` impl (`lib/jsdom/living/window/History-impl.js`,
//! cited in [Context7](https://github.com/jsdom/jsdom)) is the canonical
//! reference — used to confirm: (1) pushState clears forward entries,
//! (2) URL resolution uses current entry's URL as base, (3) popstate
//! does NOT fire on pushState/replaceState. happy-dom's `History`
//! (`packages/happy-dom/src/history/History.ts`) is structurally
//! similar but smaller. Both are ~200 LOC of TypeScript/JS doing what
//! this module does in <100 lines of Rust + JS bootstrap; vendoring
//! either would be heavier than writing fresh against the spec.
//!
//! Note on async: jsdom's WPT tests treat `history.back()` as async
//! (event loop turn before `popstate` fires). The task here pins
//! synchronous dispatch — matches the MDN guidance and is simpler. If
//! cross-document async ever lands we can move to a microtask without
//! breaking the in-document API.

use rquickjs::{prelude::Func, Context, Ctx, Function, Object};

use url::Url;

use crate::engine::EvalError;

/// Install the `history` global, the `window`-level event listener
/// surface (`addEventListener` / `removeEventListener` /
/// `dispatchEvent` on `globalThis`), and the `PopStateEvent`
/// constructor. Idempotent on each piece — re-running re-binds the
/// Rust shims but the JS-side state under `__hesoHistory` and
/// `__listeners` survives.
///
/// MUST be called after [`crate::engine::install_location`] and
/// [`crate::events::install_events`] — depends on `location.href` for
/// the initial history entry and on `Event` for `PopStateEvent` to
/// extend.
pub fn install_history(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();

            // `__hesoResolveUrl(input, base)` — parse `input` as a URL,
            // resolving against `base` for relative refs. Returns the
            // absolute URL as a string. Throws a JS TypeError if both
            // are unparseable (the typical SPA call passes
            // `location.href` as base, which always parses).
            let resolve_url = Func::from(
                |ctx: Ctx<'_>, input: String, base: String| -> rquickjs::Result<String> {
                    // Try the input as absolute first. Relative refs
                    // fall through to `base.join(...)`.
                    if let Ok(u) = Url::parse(&input) {
                        return Ok(u.as_str().to_string());
                    }
                    match Url::parse(&base) {
                        Ok(base_url) => match base_url.join(&input) {
                            Ok(u) => Ok(u.as_str().to_string()),
                            Err(e) => Err(rquickjs::Exception::throw_type(
                                &ctx,
                                &format!("history: cannot resolve url {input:?} against base {base:?}: {e}"),
                            )),
                        },
                        Err(_) => {
                            // Base is unparseable (e.g. `about:blank`).
                            // Per spec, pushState with a relative URL
                            // against `about:blank` is a SecurityError,
                            // but in practice agent flows that touch
                            // history always have a real base URL. We
                            // throw to surface the misconfiguration.
                            Err(rquickjs::Exception::throw_type(
                                &ctx,
                                &format!("history: cannot resolve url {input:?} (base {base:?} is not parseable)"),
                            ))
                        }
                    }
                },
            );
            globals.set("__hesoResolveUrl", resolve_url)?;

            // `__hesoSetLocation(absHref)` — mutate the existing
            // `globalThis.location` object's fields in place so cached
            // references stay live. Parses `absHref` Rust-side via
            // `url::Url` (same logic as [`crate::engine::install_location`]),
            // then `loc.href = ...`, `loc.pathname = ...`, etc. via
            // direct property sets.
            //
            // No-ops silently if `globalThis.location` is undefined
            // (engine wasn't fully initialized) or if `absHref` doesn't
            // parse — there's no caller that wants to know.
            let set_location = Func::from(|ctx: Ctx<'_>, abs_href: String| -> rquickjs::Result<()> {
                let parsed = match Url::parse(&abs_href) {
                    Ok(u) => u,
                    Err(_) => return Ok(()),
                };
                let globals = ctx.globals();
                let loc: Option<Object<'_>> = globals.get("location").ok();
                let Some(loc) = loc else { return Ok(()) };

                let port = parsed.port().map(|p| p.to_string()).unwrap_or_default();
                let host = match parsed.port() {
                    Some(p) => format!("{}:{}", parsed.host_str().unwrap_or(""), p),
                    None => parsed.host_str().unwrap_or("").to_string(),
                };
                let origin = match (parsed.scheme(), parsed.host_str()) {
                    (s, Some(h)) if s == "http" || s == "https" => match parsed.port() {
                        Some(p) => format!("{}://{}:{}", s, h, p),
                        None => format!("{}://{}", s, h),
                    },
                    _ => "null".to_string(),
                };

                loc.set("href", parsed.as_str().to_string())?;
                loc.set("protocol", format!("{}:", parsed.scheme()))?;
                loc.set("host", host)?;
                loc.set("hostname", parsed.host_str().unwrap_or("").to_string())?;
                loc.set("port", port)?;
                loc.set("pathname", parsed.path().to_string())?;
                loc.set(
                    "search",
                    parsed.query().map(|q| format!("?{}", q)).unwrap_or_default(),
                )?;
                loc.set(
                    "hash",
                    parsed.fragment().map(|f| format!("#{}", f)).unwrap_or_default(),
                )?;
                loc.set("origin", origin)?;
                // Refresh toString to reflect the new href.
                let href_for_to_string = parsed.as_str().to_string();
                loc.set(
                    "toString",
                    Func::from(move || -> String { href_for_to_string.clone() }),
                )?;
                Ok(())
            });
            globals.set("__hesoSetLocation", set_location)?;

            // Pure-JS bootstrap for PopStateEvent + history + the
            // window-level event surface. Idempotent — guards against
            // re-install on subsequent calls.
            ctx.eval::<(), _>(HISTORY_BOOTSTRAP)?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install history: {e}")))?;
    Ok(())
}

/// Reset the JS-side history stack to a single entry at `href`.
/// Called by [`crate::JsEngine::set_base_url`] on cross-document
/// navigation so a fresh page starts with a one-entry history (matches
/// what real browsers do — each document navigation creates a new
/// session history entry; in-document `pushState` calls add more).
///
/// Idempotent if `__hesoHistory` doesn't exist yet (engine still
/// initializing). Silently no-ops in that case.
pub fn reset_history(context: &Context, href: &str) -> Result<(), EvalError> {
    let href = href.to_owned();
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            // Skip if history hasn't been installed yet (first
            // install_location call, before install_history).
            let history_obj: Option<Object<'_>> = globals.get("__hesoHistory").ok();
            if history_obj.is_none() {
                return Ok(());
            }
            // Call the JS-side reset helper. It's exposed because doing
            // the `entries.length = 1; entries[0] = ...` mutation from
            // Rust is more verbose than from JS.
            let reset_fn: Option<Function<'_>> = globals.get("__hesoResetHistory").ok();
            let Some(reset_fn) = reset_fn else { return Ok(()) };
            let _: () = reset_fn.call((href,))?;
            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("reset history: {e}")))?;
    Ok(())
}

/// JS bootstrap installed by [`install_history`]. Idempotent — guards
/// the install of each piece behind a `typeof` check so a re-call
/// re-binds the Rust shims (`__hesoResolveUrl`, `__hesoSetLocation`)
/// without clobbering live listener storage or the history stack.
const HISTORY_BOOTSTRAP: &str = r#"
(function() {
    'use strict';

    // ---------- window-level event surface ----------
    //
    // `globalThis.__listeners` shape: { [eventType]: [{callback, capture, once}, ...] }.
    // Same as the EventTarget impl in events.rs — kept structurally
    // compatible so future code that wants to share dispatch logic
    // between the two surfaces doesn't have to translate.

    function __hesoNormalizeOpts(options) {
        if (options === true) return {capture: true, once: false};
        if (typeof options === 'object' && options !== null) {
            return {
                capture: !!options.capture,
                once: !!options.once,
            };
        }
        return {capture: false, once: false};
    }

    function __hesoGetListenerMap() {
        if (!globalThis.__listeners) {
            Object.defineProperty(globalThis, '__listeners', {
                value: Object.create(null),
                writable: false,
                enumerable: false,
                configurable: false,
            });
        }
        return globalThis.__listeners;
    }

    if (typeof globalThis.addEventListener !== 'function') {
        globalThis.addEventListener = function(type, callback, options) {
            if (typeof callback !== 'function') return;
            type = String(type);
            const opts = __hesoNormalizeOpts(options);
            const map = __hesoGetListenerMap();
            if (!map[type]) map[type] = [];
            // De-dupe by (callback, capture) per WHATWG spec.
            const list = map[type];
            for (let i = 0; i < list.length; i++) {
                if (list[i].callback === callback && list[i].capture === opts.capture) {
                    return;
                }
            }
            list.push({callback: callback, capture: opts.capture, once: opts.once});
        };
    }

    if (typeof globalThis.removeEventListener !== 'function') {
        globalThis.removeEventListener = function(type, callback, options) {
            type = String(type);
            const opts = __hesoNormalizeOpts(options);
            const map = __hesoGetListenerMap();
            const list = map[type];
            if (!list) return;
            for (let i = 0; i < list.length; i++) {
                if (list[i].callback === callback && list[i].capture === opts.capture) {
                    list.splice(i, 1);
                    return;
                }
            }
        };
    }

    if (typeof globalThis.dispatchEvent !== 'function') {
        globalThis.dispatchEvent = function(event) {
            if (!event || typeof event.type !== 'string') {
                throw new TypeError('dispatchEvent: argument is not an Event');
            }
            const map = __hesoGetListenerMap();
            const list = map[event.type];
            if (!list || list.length === 0) {
                return !(event.cancelable && event.defaultPrevented);
            }
            // Snapshot — listener may mutate the live list during
            // dispatch; spec says we iterate the pre-dispatch set.
            const snapshot = list.slice();
            const toRemove = [];
            for (let i = 0; i < snapshot.length; i++) {
                const rec = snapshot[i];
                try {
                    rec.callback.call(globalThis, event);
                } catch (err) {
                    if (typeof console !== 'undefined' && console.error) {
                        console.error('window listener exception:', err);
                    }
                }
                if (rec.once) toRemove.push(rec);
            }
            if (toRemove.length > 0) {
                for (const rec of toRemove) {
                    const idx = list.indexOf(rec);
                    if (idx >= 0) list.splice(idx, 1);
                }
            }
            return !(event.cancelable && event.defaultPrevented);
        };
    }

    // ---------- PopStateEvent ----------
    //
    // Spec shape: `new PopStateEvent(type, {state, bubbles, cancelable})`.
    // `state` is exposed as a getter that returns the construction-time
    // value. Implemented by wrapping `Event` and attaching `state` via
    // `Object.defineProperty` — same pattern `CustomEvent` uses to
    // attach `detail` (see events.rs::install_events).
    if (typeof globalThis.PopStateEvent !== 'function') {
        const _Event = globalThis.Event;
        if (typeof _Event === 'function') {
            globalThis.PopStateEvent = function PopStateEvent(type, init) {
                const ev = new _Event(type, init);
                const state = (init && typeof init === 'object' && 'state' in init)
                    ? init.state
                    : null;
                Object.defineProperty(ev, 'state', {
                    value: state,
                    writable: false,
                    enumerable: true,
                    configurable: false,
                });
                return ev;
            };
        }
    }

    // ---------- History ----------
    //
    // `globalThis.__hesoHistory` shape:
    //   {
    //     entries: [{state: any, url: string}, ...],
    //     index: number,
    //     scrollRestoration: 'auto' | 'manual',
    //   }
    //
    // Initialized once with one entry pointing at the current
    // location.href. The host's `set_base_url` calls
    // `__hesoResetHistory` on cross-document navigation to replace the
    // stack with a fresh one-entry stack at the new URL.

    function __hesoInitialHref() {
        if (globalThis.location && typeof globalThis.location.href === 'string') {
            return globalThis.location.href;
        }
        return 'about:blank';
    }

    if (typeof globalThis.__hesoHistory === 'undefined') {
        Object.defineProperty(globalThis, '__hesoHistory', {
            value: {
                entries: [{state: null, url: __hesoInitialHref()}],
                index: 0,
                scrollRestoration: 'auto',
            },
            writable: true,
            enumerable: false,
            configurable: true,
        });
    }

    // Reset hook called from Rust on cross-document navigation
    // (`set_base_url`). Idempotent — re-installs the JS function but
    // doesn't touch the live stack unless the caller invokes it.
    globalThis.__hesoResetHistory = function(href) {
        const h = globalThis.__hesoHistory;
        if (!h) return;
        h.entries = [{state: null, url: String(href)}];
        h.index = 0;
        // scrollRestoration is a per-document setting; reset to default.
        h.scrollRestoration = 'auto';
    };

    function __hesoNavigateInStack(newIndex) {
        const h = globalThis.__hesoHistory;
        if (newIndex < 0 || newIndex >= h.entries.length) return;
        if (newIndex === h.index) return;
        h.index = newIndex;
        const entry = h.entries[newIndex];
        // Update location.* in place so cached references stay valid.
        __hesoSetLocation(entry.url);
        // Dispatch popstate on window. Construct via PopStateEvent if
        // available (the common path), else fall back to a plain Event
        // with a `state` property attached — covers the "before
        // install_events runs" hypothetical and any future engine
        // that drops PopStateEvent.
        let ev;
        if (typeof globalThis.PopStateEvent === 'function') {
            ev = new globalThis.PopStateEvent('popstate', {state: entry.state});
        } else {
            ev = new globalThis.Event('popstate');
            Object.defineProperty(ev, 'state', {
                value: entry.state,
                writable: false,
                enumerable: true,
                configurable: false,
            });
        }
        globalThis.dispatchEvent(ev);
    }

    if (typeof globalThis.history === 'undefined') {
        const historyObj = Object.create(null);
        Object.defineProperty(historyObj, 'length', {
            get: function() { return globalThis.__hesoHistory.entries.length; },
            enumerable: true,
            configurable: false,
        });
        Object.defineProperty(historyObj, 'state', {
            get: function() {
                const h = globalThis.__hesoHistory;
                return h.entries[h.index].state;
            },
            enumerable: true,
            configurable: false,
        });
        Object.defineProperty(historyObj, 'scrollRestoration', {
            get: function() { return globalThis.__hesoHistory.scrollRestoration; },
            set: function(v) {
                v = String(v);
                if (v === 'auto' || v === 'manual') {
                    globalThis.__hesoHistory.scrollRestoration = v;
                }
            },
            enumerable: true,
            configurable: false,
        });

        function __hesoResolveAgainstCurrent(url) {
            const h = globalThis.__hesoHistory;
            const base = h.entries[h.index].url;
            // Treat undefined / null as "no URL change" — keep the
            // current entry's URL. This matches the spec: the `url`
            // argument is optional and defaults to the current URL.
            if (url === undefined || url === null) return base;
            return __hesoResolveUrl(String(url), base);
        }

        historyObj.pushState = function(state, _title, url) {
            const h = globalThis.__hesoHistory;
            const resolved = __hesoResolveAgainstCurrent(url);
            // Discard any forward entries — pushing replaces them.
            // Real browsers' "joint session history" rule.
            if (h.entries.length > h.index + 1) {
                h.entries.length = h.index + 1;
            }
            h.entries.push({
                state: state === undefined ? null : state,
                url: resolved,
            });
            h.index = h.entries.length - 1;
            __hesoSetLocation(resolved);
            // Per spec: no popstate event. The router that called
            // pushState already knows it just changed routes.
        };

        historyObj.replaceState = function(state, _title, url) {
            const h = globalThis.__hesoHistory;
            const resolved = __hesoResolveAgainstCurrent(url);
            h.entries[h.index] = {
                state: state === undefined ? null : state,
                url: resolved,
            };
            __hesoSetLocation(resolved);
            // Per spec: no popstate event. No length change either.
        };

        historyObj.back = function() {
            __hesoNavigateInStack(globalThis.__hesoHistory.index - 1);
        };

        historyObj.forward = function() {
            __hesoNavigateInStack(globalThis.__hesoHistory.index + 1);
        };

        historyObj.go = function(delta) {
            // `go()` and `go(0)` reload in real browsers; heso doesn't
            // wire navigation re-execution from JS, so we no-op
            // (matches our location.reload() stub). Non-zero deltas
            // walk the stack.
            const n = (delta === undefined ? 0 : Number(delta));
            if (!isFinite(n) || (n | 0) === 0) return;
            __hesoNavigateInStack(globalThis.__hesoHistory.index + (n | 0));
        };

        Object.defineProperty(globalThis, 'history', {
            value: historyObj,
            writable: false,
            enumerable: true,
            configurable: false,
        });
    }
})();
"#;

#[cfg(test)]
mod tests {
    use crate::engine::JsEngine;

    /// Engine helper — Default uses `JsEngine::new()` which is enough
    /// for these unit tests. Integration coverage lives in
    /// `tests/history.rs`.
    fn engine() -> JsEngine {
        JsEngine::default()
    }

    #[test]
    fn history_global_exists_with_required_shape() {
        let e = engine();
        let out = e
            .eval(
                r#"
                JSON.stringify({
                    has_history: typeof globalThis.history,
                    has_pushState: typeof history.pushState,
                    has_replaceState: typeof history.replaceState,
                    has_back: typeof history.back,
                    has_forward: typeof history.forward,
                    has_go: typeof history.go,
                    has_length: typeof history.length,
                })
                "#,
            )
            .expect("eval");
        let s = out.value.as_str().expect("string");
        // Sanity — failing here means install_history didn't run.
        assert!(s.contains("\"has_history\":\"object\""), "got {s}");
        assert!(s.contains("\"has_pushState\":\"function\""), "got {s}");
        assert!(s.contains("\"has_replaceState\":\"function\""), "got {s}");
        assert!(s.contains("\"has_back\":\"function\""), "got {s}");
        assert!(s.contains("\"has_forward\":\"function\""), "got {s}");
        assert!(s.contains("\"has_go\":\"function\""), "got {s}");
        assert!(s.contains("\"has_length\":\"number\""), "got {s}");
    }

    #[test]
    fn window_event_surface_exists() {
        let e = engine();
        let out = e
            .eval(
                r#"
                JSON.stringify({
                    aEL: typeof window.addEventListener,
                    rEL: typeof window.removeEventListener,
                    dE: typeof window.dispatchEvent,
                    pSE: typeof PopStateEvent,
                })
                "#,
            )
            .expect("eval");
        let s = out.value.as_str().expect("string");
        assert!(s.contains("\"aEL\":\"function\""), "got {s}");
        assert!(s.contains("\"rEL\":\"function\""), "got {s}");
        assert!(s.contains("\"dE\":\"function\""), "got {s}");
        assert!(s.contains("\"pSE\":\"function\""), "got {s}");
    }
}
