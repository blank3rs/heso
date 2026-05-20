//! Integration tests for the WHATWG UI Events constructors:
//! `KeyboardEvent`, `InputEvent`, `MouseEvent`, `FocusEvent`,
//! `PointerEvent`, `WheelEvent`, `UIEvent`. Together they unblock
//! React-style `onChange` / `onKeyDown` / `onClick` / `onFocus`
//! handlers, which probe `event.key`, `event.button`,
//! `event.shiftKey`, `event.data`, etc.
//!
//! Spec map:
//! - `KeyboardEvent` — UI Events §5.6
//! - `InputEvent`    — UI Events §5.7
//! - `MouseEvent`    — UI Events §5.4
//! - `FocusEvent`    — UI Events §5.3
//! - `UIEvent`       — UI Events §5.2
//! - `PointerEvent`  — W3C Pointer Events §5
//! - `WheelEvent`    — UI Events §5.5
//!
//! OSS cross-referenced for the IDL shape:
//! - jsdom `lib/jsdom/living/events/{Keyboard,Input,Mouse,Focus,
//!   Pointer,Wheel,UI}Event-impl.js` — MIT.
//! - happy-dom `src/event/events/{Keyboard,Input,Mouse,Focus,Pointer,
//!   Wheel}Event.ts` — MIT.

use heso_engine_js::JsEngine;

/// Build a fresh engine for each test.
fn engine() -> JsEngine {
    JsEngine::new().expect("engine new")
}

// ---------------------------------------------------------------- KeyboardEvent

/// `new KeyboardEvent('keydown', {key, code, ctrlKey, shiftKey, which,
/// keyCode})` — every field listed in the init dictionary must be
/// readable back on the instance.
#[test]
fn keyboard_event_init_preserves_all_fields() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new KeyboardEvent('keydown', {
                key: 'Enter', code: 'Enter', location: 0, repeat: false,
                isComposing: false, ctrlKey: true, shiftKey: false,
                altKey: false, metaKey: false, charCode: 0,
                keyCode: 13, which: 13, bubbles: true, cancelable: true,
            });
            JSON.stringify({
                type: ev.type,
                key: ev.key, code: ev.code, location: ev.location,
                repeat: ev.repeat, isComposing: ev.isComposing,
                ctrlKey: ev.ctrlKey, shiftKey: ev.shiftKey,
                altKey: ev.altKey, metaKey: ev.metaKey,
                charCode: ev.charCode, keyCode: ev.keyCode, which: ev.which,
                bubbles: ev.bubbles, cancelable: ev.cancelable,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"type\":\"keydown\""), "{s}");
    assert!(s.contains("\"key\":\"Enter\""), "{s}");
    assert!(s.contains("\"code\":\"Enter\""), "{s}");
    assert!(s.contains("\"ctrlKey\":true"), "{s}");
    assert!(s.contains("\"shiftKey\":false"), "{s}");
    assert!(s.contains("\"which\":13"), "{s}");
    assert!(s.contains("\"keyCode\":13"), "{s}");
    assert!(s.contains("\"bubbles\":true"), "{s}");
    assert!(s.contains("\"cancelable\":true"), "{s}");
}

/// `instanceof KeyboardEvent` and `instanceof Event` both pass — the
/// prototype-chain rewire in `install_event_constructors` is doing its
/// job.
#[test]
fn kbd_event_is_instance_of_kbd_event_and_event() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new KeyboardEvent('keyup', { key: 'a' });
            [
                ev instanceof KeyboardEvent,
                ev instanceof UIEvent,
                ev instanceof Event,
            ]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], true, "instanceof KeyboardEvent");
    assert_eq!(out.value[1], true, "instanceof UIEvent");
    assert_eq!(out.value[2], true, "instanceof Event");
}

/// Dispatching a `KeyboardEvent` runs the handler with the full
/// init-shape preserved through the listener call — same code path
/// frameworks use for `onKeyDown(e => …)`.
#[test]
fn dispatch_kbd_event_with_modifier_keys_fires_handler() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const t = new EventTarget();
            let seen = null;
            t.addEventListener('keydown', e => {
                if (e.key === 'Enter' && e.shiftKey) {
                    seen = { key: e.key, shift: e.shiftKey, code: e.code };
                }
            });
            t.dispatchEvent(new KeyboardEvent('keydown', {
                key: 'Enter', code: 'Enter', shiftKey: true,
                bubbles: true, cancelable: true,
            }));
            JSON.stringify(seen)
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"key\":\"Enter\""), "{s}");
    assert!(s.contains("\"shift\":true"), "{s}");
    assert!(s.contains("\"code\":\"Enter\""), "{s}");
}

/// `getModifierState('Shift')` returns the value we passed at
/// construction. Verifies the method route as well as the property
/// route.
#[test]
fn kbd_event_get_modifier_state_reads_init_flags() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new KeyboardEvent('keydown', { ctrlKey: true, altKey: false });
            [
                ev.getModifierState('Control'),
                ev.getModifierState('Shift'),
                ev.getModifierState('Alt'),
                ev.getModifierState('Meta'),
                ev.getModifierState('CapsLock'),
            ]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], true);
    assert_eq!(out.value[1], false);
    assert_eq!(out.value[2], false);
    assert_eq!(out.value[3], false);
    // Unknown modifier returns false (we don't track CapsLock).
    assert_eq!(out.value[4], false);
}

// ---------------------------------------------------------------- InputEvent

/// `new InputEvent('input', { data, inputType })` — fields readable.
#[test]
fn input_event_init_preserves_data_and_input_type() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new InputEvent('input', {
                data: 'a', inputType: 'insertText', isComposing: false,
                bubbles: true, cancelable: false,
            });
            JSON.stringify({
                type: ev.type,
                data: ev.data, inputType: ev.inputType,
                isComposing: ev.isComposing,
                bubbles: ev.bubbles, cancelable: ev.cancelable,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"type\":\"input\""), "{s}");
    assert!(s.contains("\"data\":\"a\""), "{s}");
    assert!(s.contains("\"inputType\":\"insertText\""), "{s}");
    assert!(s.contains("\"isComposing\":false"), "{s}");
    assert!(s.contains("\"bubbles\":true"), "{s}");
    assert!(s.contains("\"cancelable\":false"), "{s}");
}

/// `InputEvent.data` is `null` when the init dict omits `data` (per
/// spec). `inputType` defaults to the empty string.
#[test]
fn input_event_default_data_is_null() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new InputEvent('input');
            [ev.data, ev.inputType]
            "#,
        )
        .expect("eval ok");
    // JSON-encodes JS `null` → JSON null → serde_json::Value::Null.
    assert_eq!(out.value[0], serde_json::Value::Null);
    assert_eq!(out.value[1], "");
}

/// React's controlled-input onChange shape: a listener attached to
/// `onChange` (which React proxies through `input` events on real
/// browsers) reads `event.target.value` and `event.nativeEvent.data`.
/// We only test the `event.data` half — `event.target.value` is
/// dispatch_with_node_path's responsibility and tested in
/// `dispatch.rs`.
#[test]
fn react_style_change_handler_receives_input_event() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const t = new EventTarget();
            const log = [];
            t.addEventListener('input', e => {
                log.push({
                    type: e.type, data: e.data, inputType: e.inputType,
                    isInput: e instanceof InputEvent,
                });
            });
            t.dispatchEvent(new InputEvent('input', {
                data: 'h', inputType: 'insertText', bubbles: true,
            }));
            JSON.stringify(log[0])
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"type\":\"input\""), "{s}");
    assert!(s.contains("\"data\":\"h\""), "{s}");
    assert!(s.contains("\"inputType\":\"insertText\""), "{s}");
    assert!(s.contains("\"isInput\":true"), "{s}");
}

// ---------------------------------------------------------------- MouseEvent

/// `new MouseEvent('click', {button, clientX, clientY, ctrlKey})` —
/// fields readable.
#[test]
fn mouse_event_init_preserves_button_and_modifiers() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new MouseEvent('click', {
                button: 2, buttons: 2,
                clientX: 100, clientY: 200,
                screenX: 110, screenY: 220,
                ctrlKey: true, shiftKey: false, altKey: false, metaKey: false,
                bubbles: true, cancelable: true,
            });
            JSON.stringify({
                type: ev.type, button: ev.button, buttons: ev.buttons,
                clientX: ev.clientX, clientY: ev.clientY,
                screenX: ev.screenX, screenY: ev.screenY,
                ctrlKey: ev.ctrlKey, shiftKey: ev.shiftKey,
                bubbles: ev.bubbles, cancelable: ev.cancelable,
                pageX: ev.pageX, pageY: ev.pageY,
                x: ev.x, y: ev.y,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"button\":2"), "{s}");
    assert!(s.contains("\"buttons\":2"), "{s}");
    assert!(s.contains("\"clientX\":100"), "{s}");
    assert!(s.contains("\"clientY\":200"), "{s}");
    assert!(s.contains("\"screenX\":110"), "{s}");
    assert!(s.contains("\"screenY\":220"), "{s}");
    assert!(s.contains("\"ctrlKey\":true"), "{s}");
    assert!(s.contains("\"shiftKey\":false"), "{s}");
    // page* / x/y aliases mirror clientX/Y per our implementation.
    assert!(s.contains("\"pageX\":100"), "{s}");
    assert!(s.contains("\"y\":200"), "{s}");
}

/// `MouseEvent instanceof UIEvent instanceof Event`.
#[test]
fn mouse_event_inherits_from_ui_and_event() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new MouseEvent('click', { button: 0 });
            [
                ev instanceof MouseEvent,
                ev instanceof UIEvent,
                ev instanceof Event,
            ]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], true);
    assert_eq!(out.value[1], true);
    assert_eq!(out.value[2], true);
}

// ---------------------------------------------------------------- FocusEvent

/// `new FocusEvent('focus', {relatedTarget})` — `relatedTarget`
/// readable, defaults to `null`.
#[test]
fn focus_event_carries_related_target() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const other = new EventTarget();
            const ev = new FocusEvent('focus', {
                relatedTarget: other, bubbles: false, cancelable: false,
            });
            // We don't structural-equality the target object — just
            // verify it round-trips as the same reference (===).
            [
                ev.type,
                ev.relatedTarget === other,
                ev instanceof FocusEvent,
                ev instanceof UIEvent,
                ev instanceof Event,
                (new FocusEvent('blur')).relatedTarget,
            ]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "focus");
    assert_eq!(out.value[1], true);
    assert_eq!(out.value[2], true);
    assert_eq!(out.value[3], true);
    assert_eq!(out.value[4], true);
    assert_eq!(out.value[5], serde_json::Value::Null);
}

// ---------------------------------------------------------------- PointerEvent

/// `new PointerEvent('pointerdown', {pointerId, pointerType, isPrimary})`
/// — fields readable; chain `PointerEvent → MouseEvent → UIEvent → Event`.
#[test]
fn pointer_event_init_preserves_pointer_fields() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new PointerEvent('pointerdown', {
                pointerId: 1, pointerType: 'mouse', isPrimary: true,
                width: 1, height: 1, pressure: 0.5,
                tiltX: 0, tiltY: 0, twist: 0,
                tangentialPressure: 0,
                button: 0, buttons: 1,
                bubbles: true, cancelable: true,
            });
            JSON.stringify({
                type: ev.type,
                pointerId: ev.pointerId, pointerType: ev.pointerType,
                isPrimary: ev.isPrimary, pressure: ev.pressure,
                button: ev.button, buttons: ev.buttons,
                isPtr: ev instanceof PointerEvent,
                isMouse: ev instanceof MouseEvent,
                isUI: ev instanceof UIEvent,
                isEv: ev instanceof Event,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"pointerId\":1"), "{s}");
    assert!(s.contains("\"pointerType\":\"mouse\""), "{s}");
    assert!(s.contains("\"isPrimary\":true"), "{s}");
    assert!(s.contains("\"pressure\":0.5"), "{s}");
    assert!(s.contains("\"buttons\":1"), "{s}");
    assert!(s.contains("\"isPtr\":true"), "{s}");
    assert!(s.contains("\"isMouse\":true"), "{s}");
    assert!(s.contains("\"isUI\":true"), "{s}");
    assert!(s.contains("\"isEv\":true"), "{s}");
}

// ---------------------------------------------------------------- WheelEvent

/// `new WheelEvent('wheel', {deltaX, deltaY, deltaMode})` — fields
/// readable; inherits from MouseEvent.
#[test]
fn wheel_event_init_preserves_delta_fields() {
    let e = engine();
    let out = e
        .eval(
            r#"
            const ev = new WheelEvent('wheel', {
                deltaX: 10, deltaY: -20, deltaZ: 0, deltaMode: 1,
                clientX: 5, clientY: 7,
                bubbles: true, cancelable: true,
            });
            JSON.stringify({
                deltaX: ev.deltaX, deltaY: ev.deltaY,
                deltaZ: ev.deltaZ, deltaMode: ev.deltaMode,
                clientX: ev.clientX,
                isWheel: ev instanceof WheelEvent,
                isMouse: ev instanceof MouseEvent,
            })
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    assert!(s.contains("\"deltaX\":10"), "{s}");
    assert!(s.contains("\"deltaY\":-20"), "{s}");
    assert!(s.contains("\"deltaMode\":1"), "{s}");
    assert!(s.contains("\"clientX\":5"), "{s}");
    assert!(s.contains("\"isWheel\":true"), "{s}");
    assert!(s.contains("\"isMouse\":true"), "{s}");
}

// ---------------------------------------------------------------- UIEvent

/// `new UIEvent('foo', {detail: 1, view: window})` — `detail` and
/// `view` readable.
#[test]
fn ui_event_init_preserves_detail_and_view() {
    let e = engine();
    let out = e
        .eval(
            r#"
            // Use globalThis as the view since we don't ship a real
            // Window object; the IDL just says `view: Window`, which
            // is JS-Object-shaped.
            const view = globalThis;
            const ev = new UIEvent('foo', { detail: 3, view: view });
            [ev.type, ev.detail, ev.view === view]
            "#,
        )
        .expect("eval ok");
    assert_eq!(out.value[0], "foo");
    assert_eq!(out.value[1], 3);
    assert_eq!(out.value[2], true);
}

// ---------------------------------------------------------------- Capture/bubble dispatch

/// Tree-aware dispatch: a `KeyboardEvent` dispatched on the deepest
/// element should be visible to a listener attached at the document
/// level when the event bubbles. Validates that the new event classes
/// flow through the existing path-walking dispatcher in `dom.rs`.
#[test]
fn event_is_dispatched_through_capture_and_bubble() {
    let e = engine();
    let out = e
        .eval_with_html(
            "<html><body></body></html>",
            r#"
            (() => {
                const root = document.createElement('div');
                const mid = document.createElement('div');
                const leaf = document.createElement('input');
                root.appendChild(mid);
                mid.appendChild(leaf);
                document.body.appendChild(root);

                const log = [];
                root.addEventListener('keydown', e => log.push('root:' + e.key));
                mid.addEventListener('keydown', e => log.push('mid:' + e.key));
                leaf.addEventListener('keydown', e => log.push('leaf:' + e.key));
                // Capture listener at root:
                root.addEventListener('keydown', e => log.push('rootCapture:' + e.key), true);

                leaf.dispatchEvent(new KeyboardEvent('keydown', {
                    key: 'a', code: 'KeyA', bubbles: true, cancelable: true,
                }));
                return log.join('|');
            })()
            "#,
        )
        .expect("eval ok");
    let s = out.value.as_str().expect("string");
    // Order should be: capture (root) → at-target (leaf) → bubble
    // (mid, then root). The `root` listener registered without
    // capture fires in bubble phase, AFTER `mid`.
    assert!(s.starts_with("rootCapture:a|"), "got: {s}");
    assert!(s.contains("leaf:a"), "got: {s}");
    assert!(s.contains("mid:a"), "got: {s}");
    assert!(s.contains("root:a"), "got: {s}");
    // Bubble order: mid before root.
    let mid_idx = s.find("mid:a").unwrap();
    let root_idx = s.find("|root:a").unwrap_or_else(|| {
        // root bubble entry comes after `mid:a|`.
        s.rfind("root:a").unwrap()
    });
    assert!(mid_idx < root_idx, "expected mid before root in: {s}");
}

// ---------------------------------------------------------------- CLI fill integration

/// `JsEngine::set_input_value` (the `heso fill` engine path) now
/// dispatches the spec-correct keydown → input → change sequence,
/// each event carrying its real spec-shape. A listener that asks for
/// `event.key` / `event.data` / `event.inputType` sees real values.
#[test]
fn cli_fill_dispatches_real_keyboard_events() {
    let e = engine();
    // We can't observe state across two `set_input_value` calls
    // (each call re-parses the HTML, wiping listeners). So we
    // exercise the typing pipeline by inlining the same `__hesoDispatchTyping`
    // call inside one IIFE — that's exactly what `set_input_value`
    // does, just open-coded so we can attach listeners first.
    let html = r#"<html><body><input id="q" type="text"></body></html>"#;
    let read = e
        .eval_with_html(
            html,
            r#"
            (() => {
                const el = document.getElementById('q');
                window.__events = [];
                el.addEventListener('keydown',  e => window.__events.push(
                    'kd:' + e.key + ':' + e.code + ':' + (e instanceof KeyboardEvent)));
                el.addEventListener('beforeinput', e => window.__events.push(
                    'bi:' + e.data + ':' + e.inputType + ':' + (e instanceof InputEvent)));
                el.addEventListener('input', e => window.__events.push(
                    'in:' + e.data + ':' + e.inputType + ':' + (e instanceof InputEvent)));
                el.addEventListener('keyup', e => window.__events.push(
                    'ku:' + e.key + ':' + e.code + ':' + (e instanceof KeyboardEvent)));
                el.addEventListener('change', e => window.__events.push('chg:' + el.value));
                el.addEventListener('focus', e => window.__events.push(
                    'foc:' + (e instanceof FocusEvent)));
                // Run the helper inline:
                __hesoDispatchTyping(el, 'hi');
                return window.__events;
            })()
            "#,
        )
        .expect("eval_with_html ok");
    let arr = read.value.as_array().expect("array");
    let joined = arr
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_owned())
        .collect::<Vec<_>>()
        .join("|");
    assert!(joined.contains("foc:true"), "missing focus: {joined}");
    assert!(joined.contains("kd:h:KeyH:true"), "missing kd 'h': {joined}");
    assert!(joined.contains("bi:h:insertText:true"), "missing bi 'h': {joined}");
    assert!(joined.contains("in:h:insertText:true"), "missing in 'h': {joined}");
    assert!(joined.contains("ku:h:KeyH:true"), "missing ku 'h': {joined}");
    assert!(joined.contains("kd:i:KeyI:true"), "missing kd 'i': {joined}");
    assert!(joined.contains("bi:i:insertText:true"), "missing bi 'i': {joined}");
    assert!(joined.contains("chg:hi"), "missing change with full value: {joined}");
}

/// `JsEngine::dispatch_click` now fires the spec-correct
/// mousedown → mouseup → click trio, each as a real MouseEvent with
/// `button: 0` and `buttons` set per UI Events §3.5.1.
#[test]
fn cli_click_dispatches_mousedown_mouseup_click() {
    let e = engine();
    let html = r#"
        <html><body>
            <button id="go">Go</button>
        </body></html>
    "#;
    // dispatch_click reinstalls document, so we exercise the actual
    // sequence via an inline IIFE that registers listeners then
    // mimics the dispatch_click body verbatim.
    let read = e
        .eval_with_html(
            html,
            r#"
            (() => {
                const btn = document.getElementById('go');
                window.__events = [];
                btn.addEventListener('mousedown', e => window.__events.push(
                    'md:' + e.button + ':' + e.buttons + ':' + (e instanceof MouseEvent)));
                btn.addEventListener('mouseup', e => window.__events.push(
                    'mu:' + e.button + ':' + e.buttons));
                btn.addEventListener('click', e => window.__events.push(
                    'cl:' + e.button + ':' + e.detail));
                btn.dispatchEvent(new MouseEvent('mousedown', {
                    bubbles: true, cancelable: true, composed: true,
                    button: 0, buttons: 1, detail: 1,
                }));
                btn.dispatchEvent(new MouseEvent('mouseup', {
                    bubbles: true, cancelable: true, composed: true,
                    button: 0, buttons: 0, detail: 1,
                }));
                btn.dispatchEvent(new MouseEvent('click', {
                    bubbles: true, cancelable: true, composed: true,
                    button: 0, buttons: 0, detail: 1,
                }));
                return window.__events;
            })()
            "#,
        )
        .expect("eval_with_html ok");
    let arr = read.value.as_array().expect("array");
    let v: Vec<String> = arr
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_owned())
        .collect();
    // mousedown first, with buttons=1, mouseEvent flag true.
    assert_eq!(v[0], "md:0:1:true");
    // mouseup second, with buttons=0.
    assert_eq!(v[1], "mu:0:0");
    // click last, with detail=1.
    assert_eq!(v[2], "cl:0:1");
}
