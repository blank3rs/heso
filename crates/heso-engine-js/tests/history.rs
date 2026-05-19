//! Integration tests for `globalThis.history` + `PopStateEvent` +
//! the `window`-level event listener surface installed by
//! [`heso_engine_js::history::install_history`].
//!
//! Each test pins ONE clause of the WHATWG History API contract that
//! a real SPA router (Next.js, React Router v6, Vue Router) gates on:
//!
//! - `pushState(state, _, url)` updates `history.state`,
//!   `history.length`, and `location.*` — but does NOT dispatch
//!   `popstate`.
//! - `replaceState(state, _, url)` updates `history.state` and
//!   `location.*` — does NOT change `history.length`, does NOT
//!   dispatch `popstate`.
//! - `history.back()` / `history.forward()` / `history.go(N)` walk
//!   the in-memory stack, restore `location.*` and `history.state`,
//!   and synchronously dispatch a `popstate` event on `window` whose
//!   `state` property mirrors the restored entry.
//! - `window.addEventListener('popstate', handler)` receives those
//!   events.
//! - `dispatchEvent(new Event('popstate'))` works as a fallback when
//!   user code synthesises its own `popstate` without using
//!   `PopStateEvent`.
//!
//! Source-of-truth for these clauses: the MDN PopStateEvent page and
//! WHATWG HTML § 7.2.7.2 (cross-checked against jsdom's
//! `History-impl.js` and happy-dom's `History.ts`).

use heso_engine_js::{JsEngine, JsSession};
use url::Url;

fn engine_at(href: &str) -> JsEngine {
    let e = JsEngine::new().expect("engine new");
    e.set_base_url(Some(Url::parse(href).expect("parse base url")));
    e
}

// ===== history shape ============================================================

#[test]
fn history_initial_length_is_one() {
    let e = engine_at("https://example.com/");
    let out = e.eval("history.length").expect("eval");
    assert_eq!(out.value, serde_json::json!(1));
}

#[test]
fn history_initial_state_is_null() {
    let e = engine_at("https://example.com/");
    let out = e.eval("history.state").expect("eval");
    assert_eq!(out.value, serde_json::json!(null));
}

#[test]
fn history_scroll_restoration_defaults_to_auto_and_accepts_manual() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            const a = history.scrollRestoration;
            history.scrollRestoration = 'manual';
            const b = history.scrollRestoration;
            // Junk values are ignored per spec.
            history.scrollRestoration = 'bogus';
            const c = history.scrollRestoration;
            [a, b, c]
            "#,
        )
        .expect("eval");
    assert_eq!(out.value[0], "auto");
    assert_eq!(out.value[1], "manual");
    assert_eq!(out.value[2], "manual");
}

// ===== pushState ==============================================================

#[test]
fn push_state_updates_history_state() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            history.pushState({route: 'about'}, '', '/about');
            history.state
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!({"route": "about"}));
}

#[test]
fn push_state_updates_location_pathname() {
    let e = engine_at("https://example.com/dashboard");
    let out = e
        .eval(
            r#"
            history.pushState(null, '', '/users/42');
            JSON.stringify({
                href: location.href,
                pathname: location.pathname,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    // `/users/42` resolves against `/dashboard` to `https://example.com/users/42`.
    assert!(
        s.contains("\"pathname\":\"/users/42\""),
        "expected pathname=/users/42, got {s}"
    );
    assert!(
        s.contains("\"href\":\"https://example.com/users/42\""),
        "expected href=https://example.com/users/42, got {s}"
    );
}

#[test]
fn push_state_increments_history_length() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            history.pushState({}, '', '/a');
            history.pushState({}, '', '/b');
            history.pushState({}, '', '/c');
            history.length
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(4));
}

#[test]
fn push_state_does_not_fire_popstate() {
    // Per MDN + WHATWG: pushState/replaceState do NOT dispatch
    // popstate. Only back()/forward()/go() do. This is the single
    // most common SPA-router footgun — if our impl fires popstate
    // on pushState, routers infinite-loop.
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            let count = 0;
            window.addEventListener('popstate', () => { count++; });
            history.pushState({}, '', '/a');
            history.pushState({}, '', '/b');
            history.replaceState({}, '', '/c');
            count
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(0));
}

#[test]
fn push_state_with_relative_query_string_resolves_against_current() {
    let e = engine_at("https://example.com/list");
    let out = e
        .eval(
            r#"
            history.pushState({}, '', '?page=2');
            JSON.stringify({
                pathname: location.pathname,
                search: location.search,
                href: location.href,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"pathname\":\"/list\""), "got {s}");
    assert!(s.contains("\"search\":\"?page=2\""), "got {s}");
    assert!(s.contains("\"href\":\"https://example.com/list?page=2\""), "got {s}");
}

#[test]
fn push_state_with_hash_only_updates_hash() {
    let e = engine_at("https://example.com/docs");
    let out = e
        .eval(
            r#"
            history.pushState({}, '', '#section-3');
            JSON.stringify({
                pathname: location.pathname,
                hash: location.hash,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"pathname\":\"/docs\""), "got {s}");
    assert!(s.contains("\"hash\":\"#section-3\""), "got {s}");
}

#[test]
fn push_state_with_undefined_url_keeps_current_url() {
    let e = engine_at("https://example.com/keep");
    let out = e
        .eval(
            r#"
            history.pushState({tag: 'first'});
            history.pushState({tag: 'second'}, '');
            JSON.stringify({
                len: history.length,
                state: history.state,
                pathname: location.pathname,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    // Both calls leave pathname unchanged (no url arg means "no
    // URL change") and add entries.
    assert!(s.contains("\"len\":3"), "got {s}");
    assert!(s.contains("\"tag\":\"second\""), "got {s}");
    assert!(s.contains("\"pathname\":\"/keep\""), "got {s}");
}

// ===== replaceState ===========================================================

#[test]
fn replace_state_updates_state_but_not_length() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            history.pushState({}, '', '/a');
            history.pushState({}, '', '/b');
            const lenBefore = history.length;
            history.replaceState({swapped: true}, '', '/b-renamed');
            JSON.stringify({
                lenBefore,
                lenAfter: history.length,
                state: history.state,
                pathname: location.pathname,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"lenBefore\":3"), "got {s}");
    assert!(s.contains("\"lenAfter\":3"), "got {s}");
    assert!(s.contains("\"swapped\":true"), "got {s}");
    assert!(s.contains("\"pathname\":\"/b-renamed\""), "got {s}");
}

// ===== back / forward =========================================================

#[test]
fn history_back_dispatches_popstate_with_restored_state() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            const seen = [];
            window.addEventListener('popstate', (ev) => {
                seen.push({state: ev.state, pathname: location.pathname});
            });
            history.pushState({route: 'a', n: 1}, '', '/a');
            history.pushState({route: 'b', n: 2}, '', '/b');
            // We're at /b (state n:2). Back goes to /a (state n:1).
            history.back();
            JSON.stringify({
                seen,
                currentPathname: location.pathname,
                currentState: history.state,
                length: history.length,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    // One popstate dispatched.
    assert!(s.contains("\"seen\":["), "got {s}");
    // popstate carries the state of the entry we navigated TO (/a, n:1).
    assert!(s.contains("\"n\":1"), "got {s}");
    assert!(s.contains("\"pathname\":\"/a\""), "got {s}");
    // location is restored.
    assert!(s.contains("\"currentPathname\":\"/a\""), "got {s}");
    // history.state matches.
    assert!(s.contains("\"currentState\":{\"route\":\"a\",\"n\":1}"), "got {s}");
    // Length unchanged.
    assert!(s.contains("\"length\":3"), "got {s}");
}

#[test]
fn history_forward_dispatches_popstate_and_walks_to_next_entry() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            const seen = [];
            window.addEventListener('popstate', (ev) => {
                seen.push({state: ev.state, pathname: location.pathname});
            });
            history.pushState({tag: 'a'}, '', '/a');
            history.pushState({tag: 'b'}, '', '/b');
            history.back();    // -> /a
            history.forward(); // -> /b
            JSON.stringify({
                seenCount: seen.length,
                last: seen[seen.length - 1],
                pathname: location.pathname,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    // back + forward = two popstate events.
    assert!(s.contains("\"seenCount\":2"), "got {s}");
    // Final pathname is /b.
    assert!(s.contains("\"pathname\":\"/b\""), "got {s}");
    // Last popstate's state matches the /b entry.
    assert!(s.contains("\"tag\":\"b\""), "got {s}");
}

#[test]
fn history_go_with_negative_delta_walks_back_multiple_entries() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            history.pushState({}, '', '/a');
            history.pushState({}, '', '/b');
            history.pushState({}, '', '/c');
            // Stack: ['/', '/a', '/b', '/c'], index=3
            history.go(-2);
            // Now at index 1 = '/a'
            JSON.stringify({pathname: location.pathname, index: history.length})
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"pathname\":\"/a\""), "got {s}");
    // Length unchanged.
    assert!(s.contains("\"index\":4"), "got {s}");
}

#[test]
fn history_back_at_start_is_a_noop() {
    let e = engine_at("https://example.com/start");
    let out = e
        .eval(
            r#"
            let count = 0;
            window.addEventListener('popstate', () => { count++; });
            // No pushState calls — we're already at the only entry.
            history.back();
            history.back();
            history.go(-100);
            JSON.stringify({count, pathname: location.pathname})
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    // No popstate fired — there's nowhere to go back to.
    assert!(s.contains("\"count\":0"), "got {s}");
    assert!(s.contains("\"pathname\":\"/start\""), "got {s}");
}

#[test]
fn history_forward_at_end_is_a_noop() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            history.pushState({}, '', '/a');
            let count = 0;
            window.addEventListener('popstate', () => { count++; });
            // At end of stack — forward should not fire popstate.
            history.forward();
            history.go(5);
            JSON.stringify({count, pathname: location.pathname})
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"count\":0"), "got {s}");
    assert!(s.contains("\"pathname\":\"/a\""), "got {s}");
}

// ===== push-after-back drops forward entries ==================================

#[test]
fn push_after_back_truncates_forward_stack() {
    // Real-browser invariant: pushing while in the middle of the
    // stack drops everything past the current index. React Router
    // assumes this — without it, `back()` after a forward push could
    // end up at the wrong entry.
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            history.pushState({}, '', '/a');
            history.pushState({}, '', '/b');
            history.pushState({}, '', '/c');
            history.back();
            history.back();
            // We're at /a. Pushing should make the stack:
            //   ['/', '/a', '/new'], not retain '/b' / '/c'.
            history.pushState({}, '', '/new');
            JSON.stringify({
                length: history.length,
                pathname: location.pathname,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    // Was 4, then we went back to /a (still 4), then push drops 2 + adds 1 = 3.
    assert!(s.contains("\"length\":3"), "got {s}");
    assert!(s.contains("\"pathname\":\"/new\""), "got {s}");
}

#[test]
fn push_after_back_cannot_forward_to_dropped_entries() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            history.pushState({tag: 'a'}, '', '/a');
            history.pushState({tag: 'b'}, '', '/b');
            history.back();
            history.pushState({tag: 'new'}, '', '/new');
            // /b should no longer be reachable via forward()
            let count = 0;
            window.addEventListener('popstate', () => { count++; });
            history.forward();
            JSON.stringify({count, pathname: location.pathname, state: history.state})
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    // No popstate — we're at the end of the truncated stack.
    assert!(s.contains("\"count\":0"), "got {s}");
    assert!(s.contains("\"pathname\":\"/new\""), "got {s}");
    assert!(s.contains("\"tag\":\"new\""), "got {s}");
}

// ===== PopStateEvent =========================================================

#[test]
fn pop_state_event_constructor_accepts_state_in_init() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            const ev = new PopStateEvent('popstate', {state: {route: 'home'}});
            JSON.stringify({type: ev.type, state: ev.state})
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"type\":\"popstate\""), "got {s}");
    assert!(s.contains("\"route\":\"home\""), "got {s}");
}

#[test]
fn pop_state_event_state_defaults_to_null() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            const ev = new PopStateEvent('popstate');
            ev.state
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(null));
}

#[test]
fn fallback_plain_event_popstate_via_dispatch_event_works() {
    // Per task: "Test that `dispatchEvent(new Event('popstate'))`
    // works as a fallback if PopStateEvent ctor isn't available."
    //
    // We can't actually remove PopStateEvent from globalThis (it's
    // configurable:false), so we exercise the equivalent path:
    // user code constructs a plain Event and dispatches it on window.
    // The listener must still fire, and reading `ev.state` from a
    // listener that doesn't depend on PopStateEvent should work via
    // a manually-attached property.
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            let received = null;
            window.addEventListener('popstate', (ev) => {
                received = {type: ev.type, hasState: 'state' in ev};
            });
            const ev = new Event('popstate');
            // User attaches state manually — this is the fallback shape
            // PR description calls out.
            Object.defineProperty(ev, 'state', {value: {tag: 'fb'}, enumerable: true});
            window.dispatchEvent(ev);
            JSON.stringify(received)
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"type\":\"popstate\""), "got {s}");
    assert!(s.contains("\"hasState\":true"), "got {s}");
}

// ===== window event surface ==================================================

#[test]
fn window_add_event_listener_fires_on_dispatch() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            let seen = '';
            window.addEventListener('hello', (ev) => { seen = ev.type; });
            window.dispatchEvent(new Event('hello'));
            seen
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, "hello");
}

#[test]
fn window_remove_event_listener_stops_dispatch() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            let count = 0;
            const fn = () => { count++; };
            window.addEventListener('beat', fn);
            window.dispatchEvent(new Event('beat'));
            window.removeEventListener('beat', fn);
            window.dispatchEvent(new Event('beat'));
            count
            "#,
        )
        .expect("eval");
    // First dispatch counted, second didn't.
    assert_eq!(out.value, serde_json::json!(1));
}

#[test]
fn window_add_event_listener_dedupes_duplicate_registration() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            let count = 0;
            const fn = () => { count++; };
            window.addEventListener('x', fn);
            window.addEventListener('x', fn);  // duplicate
            window.dispatchEvent(new Event('x'));
            count
            "#,
        )
        .expect("eval");
    // Only one invocation despite two adds.
    assert_eq!(out.value, serde_json::json!(1));
}

#[test]
fn window_add_event_listener_once_auto_removes() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            let count = 0;
            window.addEventListener('y', () => { count++; }, {once: true});
            window.dispatchEvent(new Event('y'));
            window.dispatchEvent(new Event('y'));
            count
            "#,
        )
        .expect("eval");
    assert_eq!(out.value, serde_json::json!(1));
}

// ===== reset-on-navigate ====================================================

#[test]
fn cross_document_navigation_resets_history() {
    // `set_base_url` is the host-side cross-document navigation
    // hook. After it, history should look fresh (length=1, state=null,
    // location reflects the new URL).
    let e = engine_at("https://example.com/page-one");
    let _ = e
        .eval(
            r#"
            history.pushState({}, '', '/a');
            history.pushState({}, '', '/b');
            "#,
        )
        .expect("eval");
    e.set_base_url(Some(Url::parse("https://other.example/fresh").unwrap()));
    let out = e
        .eval(
            r#"
            JSON.stringify({
                length: history.length,
                state: history.state,
                href: location.href,
                pathname: location.pathname,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"length\":1"), "got {s}");
    assert!(s.contains("\"state\":null"), "got {s}");
    assert!(s.contains("\"pathname\":\"/fresh\""), "got {s}");
    assert!(s.contains("\"href\":\"https://other.example/fresh\""), "got {s}");
}

// ===== multi-listener semantics ============================================

#[test]
fn multiple_popstate_listeners_all_fire_on_back() {
    let e = engine_at("https://example.com/");
    let out = e
        .eval(
            r#"
            const log = [];
            window.addEventListener('popstate', () => { log.push('a'); });
            window.addEventListener('popstate', () => { log.push('b'); });
            window.addEventListener('popstate', () => { log.push('c'); });
            history.pushState({}, '', '/x');
            history.back();
            log.join(',')
            "#,
        )
        .expect("eval");
    // All three listeners fire once, in registration order.
    assert_eq!(out.value, "a,b,c");
}

// ===== location identity =====================================================

#[test]
fn cached_location_reference_sees_push_state_updates() {
    // SPA invariant: routers often do `const loc = window.location`
    // once at module-load and re-read `loc.pathname` on every render.
    // pushState must mutate fields in place — replacing the location
    // object would orphan the cached reference and silently break
    // every read after the first navigation.
    let e = engine_at("https://example.com/initial");
    let out = e
        .eval(
            r#"
            // Cache reference once.
            const loc = window.location;
            const before = loc.pathname;
            history.pushState({}, '', '/changed');
            const after = loc.pathname;
            // Same JS object identity, freshly-updated pathname.
            JSON.stringify({
                before, after,
                sameIdentity: (loc === window.location),
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"before\":\"/initial\""), "got {s}");
    assert!(s.contains("\"after\":\"/changed\""), "got {s}");
    assert!(s.contains("\"sameIdentity\":true"), "got {s}");
}

// ===== JsSession (HTML + script load) ======================================

#[test]
fn jssession_load_with_inline_router_observes_pop_state_after_back() {
    // End-to-end: load a page with a router-style script that wires
    // popstate, then drive history via a separate eval(). Exercises
    // both the script-on-load path (set_base_url -> install_history
    // -> reset_history) and listener survival across eval boundaries.
    let html = r#"<!doctype html><html><body>
        <div id="route">/</div>
        <script>
            window.addEventListener('popstate', (ev) => {
                document.getElementById('route').textContent = location.pathname;
            });
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, Url::parse("https://app.example/").unwrap())
        .expect("session open");

    // Drive history.
    let _ = sess
        .eval(
            r#"
            history.pushState({}, '', '/users');
            history.pushState({}, '', '/users/42');
            history.back();
            "#,
        )
        .expect("eval");

    let out = sess
        .eval("document.getElementById('route').textContent")
        .expect("eval read");
    // popstate handler should have updated the textContent to /users
    // (the entry we landed on after back from /users/42).
    assert_eq!(out.value, "/users");
}

// ===== integration: realistic SPA router pattern ===========================

#[test]
fn realistic_router_pattern_back_then_forward_walks_correctly() {
    // A small router that records every popstate. Mirrors the shape
    // React Router / Vue Router code uses: register one popstate
    // handler at mount, then call pushState whenever the user
    // clicks an in-app link.
    let e = engine_at("https://app.example/");
    let out = e
        .eval(
            r#"
            const visited = [];
            // Initial entry — routers usually pushState on mount.
            history.replaceState({page: 'home'}, '', '/');
            window.addEventListener('popstate', (ev) => {
                visited.push({
                    state: ev.state,
                    pathname: location.pathname,
                });
            });

            // User navigates around.
            history.pushState({page: 'about'}, '', '/about');
            history.pushState({page: 'docs'}, '', '/docs');
            history.pushState({page: 'docs/api'}, '', '/docs/api');

            // User clicks back twice.
            history.back();
            history.back();

            // User clicks forward.
            history.forward();

            JSON.stringify({
                visitedCount: visited.length,
                visitedPaths: visited.map(v => v.pathname),
                visitedStates: visited.map(v => v.state && v.state.page),
                finalPathname: location.pathname,
                finalState: history.state && history.state.page,
                length: history.length,
            })
            "#,
        )
        .expect("eval");
    let s = out.value.as_str().expect("string");
    // Three popstate events: back, back, forward.
    assert!(s.contains("\"visitedCount\":3"), "got {s}");
    // Path sequence: /docs (back from /docs/api), /about (back from /docs), /docs (forward).
    assert!(s.contains("\"visitedPaths\":[\"/docs\",\"/about\",\"/docs\"]"), "got {s}");
    assert!(s.contains("\"visitedStates\":[\"docs\",\"about\",\"docs\"]"), "got {s}");
    // After back-back-forward we're at /docs.
    assert!(s.contains("\"finalPathname\":\"/docs\""), "got {s}");
    assert!(s.contains("\"finalState\":\"docs\""), "got {s}");
    // Length: home + about + docs + docs/api = 4. Back/forward don't change length.
    assert!(s.contains("\"length\":4"), "got {s}");
}
