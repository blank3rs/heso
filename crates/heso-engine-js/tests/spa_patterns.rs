//! Aggressive DOM-events / session tests covering capture+bubble,
//! createElement lifecycle, listener-dedupe, mutation during dispatch,
//! event properties, timers, RNG determinism, and realistic SPA-shaped
//! patterns. The point is to find genuine bugs in dom.rs / events.rs /
//! timers.rs — failures are findings.
//!
//! Each test exercises ONE property. Names describe the property.
//!
//! See the WHATWG DOM standard for the dispatch semantics being
//! checked: <https://dom.spec.whatwg.org/#concept-event-dispatch>.

use heso_engine_js::{JsEngine, JsSession};
use url::Url;

fn u() -> Url {
    Url::parse("https://example.com/").unwrap()
}

// =====================================================================
// Capture/bubble walk
// =====================================================================

#[test]
fn capture_listener_on_root_fires_before_at_target() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.log = [];
          document.querySelector('#root').addEventListener('click',
            () => globalThis.log.push('root-cap'), { capture: true });
          document.querySelector('#b').addEventListener('click',
            () => globalThis.log.push('target'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["root-cap", "target"]));
}

#[test]
fn bubble_listener_on_root_fires_after_at_target() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.log = [];
          document.querySelector('#root').addEventListener('click',
            () => globalThis.log.push('root-bub'));
          document.querySelector('#b').addEventListener('click',
            () => globalThis.log.push('target'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["target", "root-bub"]));
}

#[test]
fn capture_and_bubble_listeners_on_same_ancestor_fire_in_both_phases() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.log = [];
          const root = document.querySelector('#root');
          root.addEventListener('click', () => globalThis.log.push('cap'), { capture: true });
          root.addEventListener('click', () => globalThis.log.push('bub'));
          document.querySelector('#b').addEventListener('click',
            () => globalThis.log.push('tgt'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["cap", "tgt", "bub"]));
}

#[test]
fn bubble_phase_skipped_when_bubbles_false() {
    // `new Event('x')` default-bubbles=false: ancestor non-capture
    // listeners must NOT fire.
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.log = [];
          document.querySelector('#root').addEventListener('x',
            () => globalThis.log.push('root'));
          document.querySelector('#b').addEventListener('x',
            () => globalThis.log.push('tgt'));
          document.querySelector('#b').dispatchEvent(new Event('x'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["tgt"]));
}

#[test]
fn stop_propagation_in_capture_prevents_at_target_and_bubble() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.log = [];
          document.querySelector('#root').addEventListener('click',
            (e) => { globalThis.log.push('cap'); e.stopPropagation(); },
            { capture: true });
          document.querySelector('#root').addEventListener('click',
            () => globalThis.log.push('bub'));
          document.querySelector('#b').addEventListener('click',
            () => globalThis.log.push('tgt'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["cap"]));
}

#[test]
fn stop_propagation_at_target_fires_all_target_listeners_but_no_bubble() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.log = [];
          document.querySelector('#root').addEventListener('click',
            () => globalThis.log.push('bub'));
          const b = document.querySelector('#b');
          b.addEventListener('click', (e) => { globalThis.log.push('t1'); e.stopPropagation(); });
          b.addEventListener('click', () => globalThis.log.push('t2'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["t1", "t2"]));
}

#[test]
fn stop_immediate_propagation_halts_same_node_listeners() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.log = [];
          document.querySelector('#root').addEventListener('click',
            () => globalThis.log.push('bub'));
          const b = document.querySelector('#b');
          b.addEventListener('click', (e) => { globalThis.log.push('t1'); e.stopImmediatePropagation(); });
          b.addEventListener('click', () => globalThis.log.push('t2'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["t1"]));
}

#[test]
fn event_phase_value_matches_phase_in_handler() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.phases = [];
          document.querySelector('#root').addEventListener('click',
            (e) => globalThis.phases.push(e.eventPhase), { capture: true });
          document.querySelector('#b').addEventListener('click',
            (e) => globalThis.phases.push(e.eventPhase));
          document.querySelector('#root').addEventListener('click',
            (e) => globalThis.phases.push(e.eventPhase));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let phases = sess.eval("globalThis.phases").unwrap();
    // CAPTURING=1, AT_TARGET=2, BUBBLING=3.
    assert_eq!(phases.value, serde_json::json!([1, 2, 3]));
}

#[test]
fn event_target_stays_pinned_during_bubble() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.tids = [];
          document.querySelector('#root').addEventListener('click',
            (e) => globalThis.tids.push(e.target.id));
          document.querySelector('#b').addEventListener('click',
            (e) => globalThis.tids.push(e.target.id));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let tids = sess.eval("globalThis.tids").unwrap();
    assert_eq!(tids.value, serde_json::json!(["b", "b"]));
}

#[test]
fn event_current_target_changes_per_node() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><div id="mid"><button id="b">b</button></div></div>
        <script>
          globalThis.cts = [];
          document.querySelector('#root').addEventListener('click',
            (e) => globalThis.cts.push(e.currentTarget.id));
          document.querySelector('#mid').addEventListener('click',
            (e) => globalThis.cts.push(e.currentTarget.id));
          document.querySelector('#b').addEventListener('click',
            (e) => globalThis.cts.push(e.currentTarget.id));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let cts = sess.eval("globalThis.cts").unwrap();
    assert_eq!(cts.value, serde_json::json!(["b", "mid", "root"]));
}

#[test]
fn deeply_nested_bubble_walks_all_ancestors_in_order() {
    let html = r#"<!doctype html><html><body>
        <div id="a1"><div id="a2"><div id="a3"><div id="a4"><div id="a5"><div id="a6"><div id="a7"><button id="b">b</button></div></div></div></div></div></div></div>
        <script>
          globalThis.log = [];
          for (const id of ['a1','a2','a3','a4','a5','a6','a7']) {
            document.querySelector('#'+id).addEventListener('click',
              (e) => globalThis.log.push(e.currentTarget.id));
          }
          document.querySelector('#b').addEventListener('click',
            () => globalThis.log.push('b'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(
        log.value,
        serde_json::json!(["b", "a7", "a6", "a5", "a4", "a3", "a2", "a1"])
    );
}

// =====================================================================
// createElement / mutation
// =====================================================================

#[test]
fn create_element_setattribute_then_query_finds_it() {
    let html = r#"<!doctype html><html><body><div id="root"></div></body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.eval(r#"
        const el = document.createElement('span');
        el.setAttribute('id', 'made');
        el.setAttribute('data-x', '7');
        document.querySelector('#root').appendChild(el);
    "#).unwrap();
    let found = sess
        .eval("document.querySelector('#made').getAttribute('data-x')")
        .unwrap();
    assert_eq!(found.value, serde_json::json!("7"));
}

#[test]
fn sibling_listeners_do_not_cross_fire() {
    let html = r#"<!doctype html><html><body>
        <button id="a">a</button>
        <button id="b">b</button>
        <script>
          globalThis.log = [];
          document.querySelector('#a').addEventListener('click', () => globalThis.log.push('a'));
          document.querySelector('#b').addEventListener('click', () => globalThis.log.push('b'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#a").unwrap();
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["a"]));
}

#[test]
fn removed_child_is_not_queryable() {
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          const root = document.querySelector('#root');
          root.removeChild(document.querySelector('#b'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let exists = sess.eval("document.querySelector('#b') === null").unwrap();
    assert_eq!(exists.value, serde_json::json!(true));
}

#[test]
fn appended_grandchild_bubbles_to_ancestor_via_delegation() {
    let html = r#"<!doctype html><html><body>
        <div id="outer"></div>
        <div id="out">none</div>
        <script>
          document.querySelector('#outer').addEventListener('click', (e) => {
            document.querySelector('#out').textContent = 'caught:' + e.target.id;
          });
          const inner = document.createElement('div');
          inner.id = 'inner';
          const btn = document.createElement('button');
          btn.id = 'dyn';
          inner.appendChild(btn);
          document.querySelector('#outer').appendChild(inner);
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#dyn").unwrap();
    let out = sess.eval("document.querySelector('#out').textContent").unwrap();
    assert_eq!(out.value, serde_json::json!("caught:dyn"));
}

// =====================================================================
// Listener lifecycle
// =====================================================================

#[test]
fn same_callback_added_twice_with_same_capture_fires_once() {
    // DOM spec: addEventListener dedupes (type, callback, capture).
    let html = r#"<!doctype html><html><body>
        <button id="b">b</button>
        <script>
          globalThis.n = 0;
          const fn = () => { globalThis.n++; };
          const el = document.querySelector('#b');
          el.addEventListener('click', fn);
          el.addEventListener('click', fn);
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let n = sess.eval("globalThis.n").unwrap();
    assert_eq!(n.value, serde_json::json!(1));
}

#[test]
fn same_callback_with_different_capture_flags_fires_twice() {
    // Spec: (callback, capture=true) and (callback, capture=false) are
    // distinct listener entries.
    let html = r#"<!doctype html><html><body>
        <div id="root"><button id="b">b</button></div>
        <script>
          globalThis.n = 0;
          const fn = () => { globalThis.n++; };
          const root = document.querySelector('#root');
          root.addEventListener('click', fn, { capture: true });
          root.addEventListener('click', fn, { capture: false });
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let n = sess.eval("globalThis.n").unwrap();
    assert_eq!(n.value, serde_json::json!(2));
}

#[test]
fn listener_added_during_dispatch_is_not_invoked_in_current_dispatch() {
    // Spec: snapshot at dispatch start. Newly-added listeners during a
    // dispatch fire only on subsequent dispatches.
    let html = r#"<!doctype html><html><body>
        <button id="b">b</button>
        <script>
          globalThis.log = [];
          const el = document.querySelector('#b');
          el.addEventListener('click', () => {
            globalThis.log.push('A');
            el.addEventListener('click', () => globalThis.log.push('B'));
          });
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let after_first = sess.eval("globalThis.log.slice()").unwrap();
    assert_eq!(after_first.value, serde_json::json!(["A"]));
    sess.click("#b").unwrap();
    let after_second = sess.eval("globalThis.log").unwrap();
    assert_eq!(after_second.value, serde_json::json!(["A", "A", "B"]));
}

#[test]
fn many_add_remove_cycles_leave_no_residual_listeners() {
    let html = r#"<!doctype html><html><body>
        <button id="b">b</button>
        <script>
          globalThis.n = 0;
          const el = document.querySelector('#b');
          for (let i = 0; i < 100; i++) {
            const fn = () => { globalThis.n++; };
            el.addEventListener('click', fn);
            el.removeEventListener('click', fn);
          }
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let n = sess.eval("globalThis.n").unwrap();
    assert_eq!(n.value, serde_json::json!(0));
}

// =====================================================================
// Event object properties
// =====================================================================

#[test]
fn event_type_matches_dispatched_string() {
    let html = r#"<!doctype html><html><body>
        <button id="b">b</button>
        <script>
          globalThis.t = null;
          document.querySelector('#b').addEventListener('custom-xyz',
            (e) => { globalThis.t = e.type; });
          document.querySelector('#b').dispatchEvent(new Event('custom-xyz'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let t = sess.eval("globalThis.t").unwrap();
    assert_eq!(t.value, serde_json::json!("custom-xyz"));
}

#[test]
fn custom_event_detail_propagates_to_handler() {
    let html = r#"<!doctype html><html><body>
        <button id="b">b</button>
        <script>
          globalThis.d = null;
          document.querySelector('#b').addEventListener('x',
            (e) => { globalThis.d = e.detail; });
          document.querySelector('#b').dispatchEvent(
            new CustomEvent('x', { detail: { a: 1, b: 'hi' } }));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let d = sess.eval("globalThis.d").unwrap();
    assert_eq!(d.value, serde_json::json!({"a": 1, "b": "hi"}));
}

#[test]
fn preventdefault_on_non_cancelable_event_is_noop() {
    let html = r#"<!doctype html><html><body>
        <button id="b">b</button>
        <script>
          globalThis.dp = null;
          document.querySelector('#b').addEventListener('x', (e) => {
            e.preventDefault();
            globalThis.dp = e.defaultPrevented;
          });
          // cancelable defaults to false
          document.querySelector('#b').dispatchEvent(new Event('x'));
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    let dp = sess.eval("globalThis.dp").unwrap();
    assert_eq!(dp.value, serde_json::json!(false));
}

// =====================================================================
// Timers
// =====================================================================

#[test]
fn two_timeouts_fire_in_delay_order() {
    let e = JsEngine::new().unwrap();
    e.eval(r#"
        globalThis.log = [];
        setTimeout(() => globalThis.log.push('late'), 100);
        setTimeout(() => globalThis.log.push('early'), 10);
    "#).unwrap();
    e.advance_clock(200).unwrap();
    let log = e.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["early", "late"]));
}

#[test]
fn cleartimeout_cancels_pending_callback() {
    let e = JsEngine::new().unwrap();
    e.eval(r#"
        globalThis.fired = false;
        const id = setTimeout(() => { globalThis.fired = true; }, 50);
        clearTimeout(id);
    "#).unwrap();
    e.advance_clock(500).unwrap();
    let fired = e.eval("globalThis.fired").unwrap();
    assert_eq!(fired.value, serde_json::json!(false));
}

#[test]
fn setinterval_fires_multiple_times_then_clearinterval_stops() {
    let e = JsEngine::new().unwrap();
    e.eval(r#"
        globalThis.n = 0;
        globalThis.id = setInterval(() => { globalThis.n++; }, 10);
    "#).unwrap();
    e.advance_clock(55).unwrap();
    let after1 = e.eval("globalThis.n").unwrap();
    assert_eq!(after1.value, serde_json::json!(5));
    e.eval("clearInterval(globalThis.id)").unwrap();
    e.advance_clock(100).unwrap();
    let after2 = e.eval("globalThis.n").unwrap();
    assert_eq!(after2.value, serde_json::json!(5));
}

#[test]
fn nested_settimeout_chain_fires_after_enough_advance() {
    let e = JsEngine::new().unwrap();
    e.eval(r#"
        globalThis.done = false;
        setTimeout(() => {
          setTimeout(() => { globalThis.done = true; }, 10);
        }, 10);
    "#).unwrap();
    e.advance_clock(100).unwrap();
    let done = e.eval("globalThis.done").unwrap();
    assert_eq!(done.value, serde_json::json!(true));
}

// =====================================================================
// RNG determinism
// =====================================================================

#[test]
fn same_seed_produces_same_math_random_sequence() {
    let e1 = JsEngine::new_with_seed(42).unwrap();
    let e2 = JsEngine::new_with_seed(42).unwrap();
    let s1 = e1.eval("[Math.random(), Math.random(), Math.random()]").unwrap();
    let s2 = e2.eval("[Math.random(), Math.random(), Math.random()]").unwrap();
    assert_eq!(s1.value, s2.value);
}

#[test]
fn crypto_randomuuid_is_v4_shape() {
    let e = JsEngine::new_with_seed(7).unwrap();
    let id = e.eval("crypto.randomUUID()").unwrap();
    let s = id.value.as_str().unwrap().to_owned();
    // 8-4-4-4-12 with version nibble '4' and variant nibble in [89ab].
    assert_eq!(s.len(), 36, "uuid len wrong: {s}");
    let bytes = s.as_bytes();
    assert_eq!(bytes[8], b'-');
    assert_eq!(bytes[13], b'-');
    assert_eq!(bytes[18], b'-');
    assert_eq!(bytes[23], b'-');
    assert_eq!(bytes[14] as char, '4', "version nibble not 4: {s}");
    let variant = bytes[19] as char;
    assert!(
        matches!(variant, '8' | '9' | 'a' | 'b'),
        "variant nibble not in 89ab: {s}"
    );
}

// =====================================================================
// SPA-shaped realism
// =====================================================================

#[test]
fn body_delegated_listener_identifies_each_child_button() {
    let html = r#"<!doctype html><html><body>
        <button id="b1">1</button>
        <button id="b2">2</button>
        <button id="b3">3</button>
        <button id="b4">4</button>
        <button id="b5">5</button>
        <div id="out"></div>
        <script>
          globalThis.log = [];
          document.body.addEventListener('click', (e) => {
            if (e.target.id) globalThis.log.push(e.target.id);
          });
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    for id in ["b3", "b1", "b5", "b2", "b4"] {
        sess.click(&format!("#{id}")).unwrap();
    }
    let log = sess.eval("globalThis.log").unwrap();
    assert_eq!(log.value, serde_json::json!(["b3", "b1", "b5", "b2", "b4"]));
}

#[test]
fn counter_app_inc_dec_reset_all_via_delegation() {
    let html = r#"<!doctype html><html><body>
        <button id="inc">+</button>
        <button id="dec">-</button>
        <button id="reset">0</button>
        <span id="v">0</span>
        <script>
          let n = 0;
          document.body.addEventListener('click', (e) => {
            if (e.target.id === 'inc') n++;
            else if (e.target.id === 'dec') n--;
            else if (e.target.id === 'reset') n = 0;
            else return;
            document.querySelector('#v').textContent = String(n);
          });
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#inc").unwrap();
    sess.click("#inc").unwrap();
    sess.click("#inc").unwrap();
    sess.click("#dec").unwrap();
    let v1 = sess.eval("document.querySelector('#v').textContent").unwrap();
    assert_eq!(v1.value, serde_json::json!("2"));
    sess.click("#reset").unwrap();
    let v2 = sess.eval("document.querySelector('#v').textContent").unwrap();
    assert_eq!(v2.value, serde_json::json!("0"));
}

#[test]
fn aria_expanded_toggles_each_click_via_getattribute() {
    let html = r#"<!doctype html><html><body>
        <button id="b" aria-expanded="false">b</button>
        <script>
          document.querySelector('#b').addEventListener('click', (e) => {
            const cur = e.currentTarget.getAttribute('aria-expanded');
            e.currentTarget.setAttribute('aria-expanded',
              cur === 'true' ? 'false' : 'true');
          });
        </script>
    </body></html>"#;
    let (sess, _) = JsSession::open(html, u()).unwrap();
    sess.click("#b").unwrap();
    let a1 = sess
        .eval("document.querySelector('#b').getAttribute('aria-expanded')")
        .unwrap();
    assert_eq!(a1.value, serde_json::json!("true"));
    sess.click("#b").unwrap();
    let a2 = sess
        .eval("document.querySelector('#b').getAttribute('aria-expanded')")
        .unwrap();
    assert_eq!(a2.value, serde_json::json!("false"));
}
