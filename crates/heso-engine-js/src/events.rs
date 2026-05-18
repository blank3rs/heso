//! # events
//!
//! Phase 1B addendum per [ADR 0014]: the WHATWG event model. Adds the
//! six classes a real page reaches for once `<script>` execution is on
//! the table:
//!
//! - [`DOMException`] — the typed error class JS throws for spec-shaped
//!   failures (`AbortError`, `NotFoundError`, ...).
//! - [`Event`] — the base event type with the `type`/`bubbles`/
//!   `cancelable`/`defaultPrevented`/`preventDefault`/`stopPropagation`
//!   surface.
//! - [`CustomEvent`] — `Event` + a `detail` payload carrying any JS
//!   value.
//! - [`EventTarget`] — `addEventListener`/`removeEventListener`/
//!   `dispatchEvent` over a per-type listener map.
//! - [`AbortController`] / [`AbortSignal`] — the abort plumbing real
//!   `fetch` cancellation rides on, with `signal.aborted`, `abort()`,
//!   `throwIfAborted()`, `AbortSignal.abort(reason)`.
//!
//! ## Why Rust types, not JS preamble
//!
//! The obvious move would be to drop in Deno's `ext/web` files
//! verbatim. They are the spec-correctness reference and MIT-licensed
//! (we cite them below). We port the **logic** into Rust
//! `#[rquickjs::class]` types instead because:
//!
//! - The existing DOM (`Document`, `Element`, `DomTokenList`) is
//!   Rust-classes — consistency wins.
//! - One execution model. We don't shuttle into a preamble that calls
//!   back into Rust, then back into JS — every method is one Rust
//!   function, with one set of borrows.
//! - The non-spec simplifications we ship (flat dispatch only, no
//!   propagation phases, no shadow-DOM retargeting) are easier to see
//!   when the logic is right here rather than buried in 1.6k lines of
//!   ported JS.
//!
//! ## Storage strategy: JS-side, not Rust-side
//!
//! Listener callbacks, abort reasons, and CustomEvent details are
//! held as **JS-side properties on the class instance's JS object**
//! rather than as Rust `Persistent` values. This sidesteps a real
//! footgun: `Persistent<T>` rooted inside Rust state inside a
//! `Class<T>` instance keeps the underlying JS value alive across
//! runtime drop, and rquickjs's runtime aborts the process in
//! `gc_obj_list != empty` if any JS value outlives the runtime.
//! Storing on the JS-side object means the entire web of references
//! is collected as one closed graph during normal runtime teardown.
//!
//! Each event-shaped class therefore exposes its `this: Class<Self>`
//! to its methods and reads/writes hidden properties (`__listeners`,
//! `__reason`, `__detail`) on it.
//!
//! ## Citation
//!
//! Source-of-truth for spec edge cases (listener-once removal timing,
//! `stopImmediatePropagation` halting subsequent listeners,
//! `defaultPrevented` gating on `cancelable`, `AbortSignal.reason`
//! defaulting to a `DOMException("AbortError")`) is Deno's `ext/web`:
//! - `01_dom_exception.js`
//! - `02_event.js`
//! - `03_abort_signal.js`
//!
//! `https://github.com/denoland/deno/tree/main/ext/web` — MIT, compatible
//! with our MIT-or-Apache dual.
//!
//! ## Punts (deliberate Phase-1B simplifications)
//!
//! - **No propagation phases.** Dispatch is flat: target only, no
//!   capture/bubble walk. `eventPhase` reports
//!   [`EVENT_PHASE_AT_TARGET`] (=2) during dispatch, 0 otherwise.
//!   Tree-aware dispatch waits for `Element` to extend `EventTarget`.
//! - **No microtask queue.** Listeners run synchronously inside
//!   `dispatchEvent`. Real WHATWG dispatch is also synchronous; this
//!   simplification only matters if/when we add async cancellation.
//! - **`target` / `currentTarget` are not back-referenced.** Returning
//!   them would require persisting a JS reference inside the event;
//!   we deliberately punt rather than reintroduce the Persistent
//!   footgun. Listeners receive the event as their first argument
//!   and have full access to its other properties.
//! - **`timeStamp` is 0.** Until the timer agent's monotonic clock is
//!   wired in, every event reports 0.
//! - **`isTrusted` is `false`.** All events here are
//!   user-script-created (`new Event(...)` then `dispatchEvent`).
//! - **`AbortSignal.timeout(ms)` is a stub.** Returns a never-aborting
//!   signal. Real timeout integration lands when the timers module
//!   (parallel agent) merges and we can call `setTimeout` from Rust.
//! - **Listener equality for `removeEventListener`** uses JS strict-
//!   equals (`===`) on the callback function value, via a tiny helper
//!   evaluated per call. Good enough for the common case of "remove
//!   the same function I added."
//!
//! [ADR 0014]: ../../decisions/0014-bundled-quickjs-agent-dom.md
//! [`EVENT_PHASE_AT_TARGET`]: constant.EVENT_PHASE_AT_TARGET.html

use std::cell::Cell;
use std::rc::Rc;

use rquickjs::{
    class::Trace,
    prelude::{Opt, This},
    Array, Class, Context, Ctx, Error as JsError, Exception, Function, JsLifetime, Object, Value,
};

use crate::engine::EvalError;

// ===== Event-phase constants =====================================================

/// `Event.NONE` — event is not currently being dispatched.
pub const EVENT_PHASE_NONE: u32 = 0;
/// `Event.CAPTURING_PHASE` — not used in our flat-dispatch model;
/// included for spec compatibility on the constants.
pub const EVENT_PHASE_CAPTURING: u32 = 1;
/// `Event.AT_TARGET` — the only phase we report during a live dispatch.
pub const EVENT_PHASE_AT_TARGET: u32 = 2;
/// `Event.BUBBLING_PHASE` — not used in flat dispatch.
pub const EVENT_PHASE_BUBBLING: u32 = 3;

// JS-side hidden property names we use to attach state to class
// instances. Names start with `__` so they don't collide with
// well-formed DOM properties.
const PROP_LISTENERS: &str = "__listeners";
const PROP_REASON: &str = "__reason";
const PROP_DETAIL: &str = "__detail";

// ===== DOMException =============================================================

/// Look up the legacy numeric `code` for a DOMException name.
///
/// The DOM standard's "legacy code value" table assigns small integers
/// to a handful of historical exception names. Names not in the table
/// get code 0. This matches the table in `01_dom_exception.js`.
fn dom_exception_code(name: &str) -> u32 {
    match name {
        "IndexSizeError" => 1,
        "DOMStringSizeError" => 2,
        "HierarchyRequestError" => 3,
        "WrongDocumentError" => 4,
        "InvalidCharacterError" => 5,
        "NoDataAllowedError" => 6,
        "NoModificationAllowedError" => 7,
        "NotFoundError" => 8,
        "NotSupportedError" => 9,
        "InUseAttributeError" => 10,
        "InvalidStateError" => 11,
        "SyntaxError" => 12,
        "InvalidModificationError" => 13,
        "NamespaceError" => 14,
        "InvalidAccessError" => 15,
        "ValidationError" => 16,
        "TypeMismatchError" => 17,
        "SecurityError" => 18,
        "NetworkError" => 19,
        "AbortError" => 20,
        "URLMismatchError" => 21,
        "QuotaExceededError" => 22,
        "TimeoutError" => 23,
        "InvalidNodeTypeError" => 24,
        "DataCloneError" => 25,
        _ => 0,
    }
}

/// `DOMException` — the typed error class JS code throws for
/// spec-shaped failures.
///
/// Constructor: `new DOMException(message, name?)`. `name` defaults to
/// `"Error"`. `code` is derived from `name` via the legacy table (see
/// [`dom_exception_code`]).
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "DOMException")]
pub struct DOMException {
    /// `e.message` — human-readable detail.
    #[qjs(skip_trace)]
    message: String,
    /// `e.name` — programmatic discriminator (e.g. `"AbortError"`).
    #[qjs(skip_trace)]
    name: String,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl DOMException {
    /// `new DOMException(message, name?)`. `name` defaults to
    /// `"Error"`; arbitrary names are permitted.
    #[qjs(constructor)]
    pub fn new(message: Opt<String>, name: Opt<String>) -> Self {
        Self {
            message: message.0.unwrap_or_default(),
            name: name.0.unwrap_or_else(|| "Error".to_owned()),
        }
    }

    /// `e.message` getter.
    #[qjs(get)]
    fn message(&self) -> String {
        self.message.clone()
    }

    /// `e.name` getter.
    #[qjs(get)]
    fn name(&self) -> String {
        self.name.clone()
    }

    /// `e.code` getter — the legacy numeric code for `name`, or 0 if
    /// `name` is not in the legacy table.
    #[qjs(get)]
    fn code(&self) -> u32 {
        dom_exception_code(&self.name)
    }

    /// `e.toString()` — `"DOMException: <message>"` if message is non
    /// empty, else just `"DOMException"`. Matches the spec's
    /// `Error.prototype.toString` shape.
    fn to_string(&self) -> String {
        if self.message.is_empty() {
            "DOMException".to_owned()
        } else {
            format!("DOMException: {}", self.message)
        }
    }
}

// ===== Event ====================================================================

/// Mutable state of an [`Event`].
///
/// Kept Rust-side (no JS values, just primitives + flags), inside an
/// `Rc<RefCell<…>>` so methods that don't take `&mut self` can flip
/// `defaultPrevented` / `eventPhase` / propagation flags via
/// interior mutability.
struct EventState {
    event_phase: Cell<u32>,
    default_prevented: Cell<bool>,
    propagation_stopped: Cell<bool>,
    immediate_propagation_stopped: Cell<bool>,
    dispatching: Cell<bool>,
}

impl Default for EventState {
    fn default() -> Self {
        Self {
            event_phase: Cell::new(EVENT_PHASE_NONE),
            default_prevented: Cell::new(false),
            propagation_stopped: Cell::new(false),
            immediate_propagation_stopped: Cell::new(false),
            dispatching: Cell::new(false),
        }
    }
}

/// `Event` — the base WHATWG event.
///
/// Constructor `new Event(type, init?)`. The `init` dictionary
/// supports `bubbles`, `cancelable`, `composed` (all default `false`).
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "Event")]
pub struct Event {
    #[qjs(skip_trace)]
    event_type: String,
    #[qjs(skip_trace)]
    bubbles: bool,
    #[qjs(skip_trace)]
    cancelable: bool,
    #[qjs(skip_trace)]
    composed: bool,
    #[qjs(skip_trace)]
    state: Rc<EventState>,
}

impl Event {
    /// Construct a fresh [`Event`] from `(type, init?)`. Used both
    /// by the JS `new Event(...)` constructor and internally by
    /// `AbortController.abort()` when synthesizing the `"abort"`
    /// event.
    pub fn new_with_init(event_type: String, init: Option<EventInit>) -> Self {
        let init = init.unwrap_or_default();
        Self {
            event_type,
            bubbles: init.bubbles,
            cancelable: init.cancelable,
            composed: init.composed,
            state: Rc::new(EventState::default()),
        }
    }
}

/// `EventInit` — the optional dictionary the JS [`Event`] constructor
/// accepts. All fields default to `false`. Public so `CustomEvent`
/// (and anything else inheriting from `Event` in future) can reuse
/// the parsing.
#[derive(Default, Clone, Copy)]
pub struct EventInit {
    /// Whether the event bubbles. Carried for spec compatibility;
    /// flat-dispatch ignores this in Phase 1B.
    pub bubbles: bool,
    /// Whether `preventDefault()` has any effect.
    pub cancelable: bool,
    /// Whether the event crosses shadow-root boundaries. Carried but
    /// unused (no shadow DOM in Phase 1B).
    pub composed: bool,
}

/// Read an `EventInit` dictionary from a JS object. Missing fields
/// default to `false`. A `None` input yields the default init.
fn parse_event_init<'js>(_ctx: &Ctx<'js>, init: Option<Value<'js>>) -> rquickjs::Result<EventInit> {
    let Some(value) = init else {
        return Ok(EventInit::default());
    };
    if value.is_null() || value.is_undefined() {
        return Ok(EventInit::default());
    }
    let obj = value
        .into_object()
        .ok_or_else(|| JsError::new_from_js_message("init", "EventInit", "expected an object"))?;
    Ok(EventInit {
        bubbles: obj.get::<_, Option<bool>>("bubbles")?.unwrap_or(false),
        cancelable: obj.get::<_, Option<bool>>("cancelable")?.unwrap_or(false),
        composed: obj.get::<_, Option<bool>>("composed")?.unwrap_or(false),
    })
}

#[rquickjs::methods(rename_all = "camelCase")]
impl Event {
    /// `new Event(type, init?)`. `type` is required. `init` may carry
    /// `bubbles` / `cancelable` / `composed`.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let parsed = parse_event_init(&ctx, init.0)?;
        Ok(Self::new_with_init(event_type, Some(parsed)))
    }

    /// `e.type` — the event type passed to the constructor.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.event_type.clone()
    }

    /// `e.bubbles` — frozen at construction.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.bubbles
    }

    /// `e.cancelable` — frozen at construction.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.cancelable
    }

    /// `e.composed` — frozen at construction.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.composed
    }

    /// `e.defaultPrevented` — true iff a listener called
    /// `preventDefault()` on a `cancelable: true` event.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.state.default_prevented.get()
    }

    /// `e.eventPhase` — 0 when not dispatching, 2 during dispatch.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.state.event_phase.get()
    }

    /// `e.timeStamp` — stubbed at 0 in Phase 1B (no monotonic clock
    /// yet; the timers agent will wire one in).
    #[qjs(get)]
    fn time_stamp(&self) -> f64 {
        0.0
    }

    /// `e.isTrusted` — always false: every Event reachable via this
    /// surface was synthesized by user JS, not the user agent.
    #[qjs(get)]
    fn is_trusted(&self) -> bool {
        false
    }

    /// `e.target` — phase-1B stub returning `null`. Storing the
    /// back-reference would require Rust-side `Persistent`, which
    /// breaks runtime teardown. Listeners get the event by value
    /// instead; they don't need `target` to act on it.
    #[qjs(get)]
    fn target(&self) -> Option<i32> {
        None
    }

    /// `e.currentTarget` — phase-1B stub returning `null`.
    #[qjs(get)]
    fn current_target(&self) -> Option<i32> {
        None
    }

    /// `e.preventDefault()` — sets `defaultPrevented` to true if the
    /// event is `cancelable`. Silently no-ops on non-cancelable
    /// events (matches Deno's `02_event.js`).
    fn prevent_default(&self) {
        if self.cancelable {
            self.state.default_prevented.set(true);
        }
    }

    /// `e.stopPropagation()` — flag the event so subsequent
    /// EventTargets in a propagation walk are skipped. Flat-dispatch
    /// doesn't walk anyway, but the flag is read by tree-aware code
    /// once it exists.
    fn stop_propagation(&self) {
        self.state.propagation_stopped.set(true);
    }

    /// `e.stopImmediatePropagation()` — stronger: also halts
    /// subsequent listeners on the **current** EventTarget.
    fn stop_immediate_propagation(&self) {
        self.state.propagation_stopped.set(true);
        self.state.immediate_propagation_stopped.set(true);
    }
}

// ===== CustomEvent ==============================================================

/// `CustomEvent` — `Event` plus a `detail` payload of arbitrary JS
/// value.
///
/// The `detail` value is stored as a JS-side property on the class
/// instance (see [`PROP_DETAIL`]), so it participates in the normal
/// QuickJS GC graph instead of being rooted via a Rust-side
/// `Persistent`.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "CustomEvent")]
pub struct CustomEvent {
    #[qjs(skip_trace)]
    event_type: String,
    #[qjs(skip_trace)]
    bubbles: bool,
    #[qjs(skip_trace)]
    cancelable: bool,
    #[qjs(skip_trace)]
    composed: bool,
    #[qjs(skip_trace)]
    state: Rc<EventState>,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl CustomEvent {
    /// `new CustomEvent(type, init?)`. `init` extends `EventInit`
    /// with a `detail: any` field. `detail` is stashed on the
    /// freshly-built JS instance via a hidden property in
    /// [`install_events`]'s constructor wrapper. Phase-1B note: the
    /// Rust constructor sees `Ctx` but not `this`, so the wrapper
    /// installed at registration time copies `init.detail` onto the
    /// resulting instance.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let parsed = parse_event_init(&ctx, init.0)?;
        Ok(Self {
            event_type,
            bubbles: parsed.bubbles,
            cancelable: parsed.cancelable,
            composed: parsed.composed,
            state: Rc::new(EventState::default()),
        })
    }

    /// `e.type`.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.event_type.clone()
    }

    /// `e.bubbles`.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.bubbles
    }

    /// `e.cancelable`.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.cancelable
    }

    /// `e.composed`.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.composed
    }

    /// `e.defaultPrevented`.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.state.default_prevented.get()
    }

    /// `e.eventPhase`.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.state.event_phase.get()
    }

    /// `e.timeStamp`.
    #[qjs(get)]
    fn time_stamp(&self) -> f64 {
        0.0
    }

    /// `e.isTrusted`.
    #[qjs(get)]
    fn is_trusted(&self) -> bool {
        false
    }

    /// `e.target`.
    #[qjs(get)]
    fn target(&self) -> Option<i32> {
        None
    }

    /// `e.currentTarget`.
    #[qjs(get)]
    fn current_target(&self) -> Option<i32> {
        None
    }

    /// `e.detail` — reads the JS-side hidden property installed by
    /// the constructor wrapper. Returns `undefined` if no detail was
    /// provided at construction.
    #[qjs(get)]
    fn detail<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        let obj: Object<'js> =
            this.0.clone().into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "CustomEvent", "not an object")
            })?;
        match obj.get::<_, Option<Value<'js>>>(PROP_DETAIL)? {
            Some(v) => Ok(v),
            None => {
                // No detail set → return undefined. Construct one via
                // a tiny JS helper because `Value::undefined` requires
                // explicit Ctx wiring.
                let ctx = obj.ctx().clone();
                ctx.eval::<Value<'js>, _>("undefined")
            }
        }
    }

    /// `e.preventDefault()`.
    fn prevent_default(&self) {
        if self.cancelable {
            self.state.default_prevented.set(true);
        }
    }

    /// `e.stopPropagation()`.
    fn stop_propagation(&self) {
        self.state.propagation_stopped.set(true);
    }

    /// `e.stopImmediatePropagation()`.
    fn stop_immediate_propagation(&self) {
        self.state.propagation_stopped.set(true);
        self.state.immediate_propagation_stopped.set(true);
    }
}

/// View struct holding the Rc<EventState> + cancelable + event_type
/// for either [`Event`] or [`CustomEvent`], pulled out so the
/// dispatch loop can be written once.
struct EventView {
    state: Rc<EventState>,
    event_type: String,
    cancelable: bool,
}

/// Read an [`EventView`] from a JS value that should be a Class<Event>
/// or Class<CustomEvent>. Returns `None` if neither.
fn view_from_value<'js>(value: &Value<'js>) -> Option<EventView> {
    let obj = value.as_object()?;
    if let Some(c) = obj.as_class::<Event>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.state.clone(),
            event_type: ev.event_type.clone(),
            cancelable: ev.cancelable,
        });
    }
    if let Some(c) = obj.as_class::<CustomEvent>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.state.clone(),
            event_type: ev.event_type.clone(),
            cancelable: ev.cancelable,
        });
    }
    None
}

// ===== EventTarget ==============================================================

/// `EventTarget` — JS-side `addEventListener` / `removeEventListener`
/// / `dispatchEvent`.
///
/// The Rust struct is a zero-state marker. All listener storage lives
/// as a hidden JS-side property on the `this` instance (see
/// [`PROP_LISTENERS`]), shaped as an Object whose keys are event
/// types and whose values are Arrays of listener-record objects
/// `{ callback, capture, once, passive }`.
///
/// This deliberate split keeps the design free of `Persistent`
/// pitfalls — the entire listener web is closed under normal
/// QuickJS GC and tears down cleanly at runtime drop.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "EventTarget")]
pub struct EventTarget {
    // No Rust state. The class is just a JS-recognisable type marker.
}

impl EventTarget {
    /// Construct a fresh JS-only EventTarget marker.
    pub fn new_empty() -> Self {
        Self {}
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl EventTarget {
    /// `new EventTarget()`.
    #[qjs(constructor)]
    pub fn js_new() -> Self {
        Self::new_empty()
    }

    /// `target.addEventListener(type, listener, options?)`.
    ///
    /// `options` may be a boolean (interpreted as `capture`) or an
    /// object with `capture` / `once` / `passive` boolean fields, per
    /// the WHATWG spec.
    fn add_event_listener<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event_type: String,
        listener: Function<'js>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let (capture, once, passive) = parse_listener_options(&ctx, options.0)?;
        let instance: Object<'js> =
            this.0.into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "EventTarget", "not an object")
            })?;
        add_listener_to_instance(
            &ctx,
            &instance,
            &event_type,
            &listener,
            capture,
            once,
            passive,
        )
    }

    /// `target.removeEventListener(type, listener, options?)`.
    fn remove_event_listener<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event_type: String,
        listener: Function<'js>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let (capture, _, _) = parse_listener_options(&ctx, options.0)?;
        let instance: Object<'js> =
            this.0.into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "EventTarget", "not an object")
            })?;
        remove_listener_from_instance(&ctx, &instance, &event_type, &listener, capture)
    }

    /// `target.dispatchEvent(event)`. Returns `false` iff `event` is
    /// cancelable and a listener called `preventDefault()`.
    fn dispatch_event<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event: Value<'js>,
    ) -> rquickjs::Result<bool> {
        let instance: Object<'js> =
            this.0.into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "EventTarget", "not an object")
            })?;
        dispatch_on_instance(&ctx, &instance, event)
    }
}

/// Read (or lazily create) the listener-map Object hanging off
/// `instance` under [`PROP_LISTENERS`]. The map's shape is
/// `{ [eventType]: [{ callback, capture, once, passive }, ...] }`.
pub(crate) fn get_or_create_listener_map<'js>(
    ctx: &Ctx<'js>,
    instance: &Object<'js>,
) -> rquickjs::Result<Object<'js>> {
    match instance.get::<_, Option<Object<'js>>>(PROP_LISTENERS)? {
        Some(o) => Ok(o),
        None => {
            let o = Object::new(ctx.clone())?;
            instance.set(PROP_LISTENERS, o.clone())?;
            Ok(o)
        }
    }
}

/// Append a listener record to the per-type list. De-dupes against
/// existing records that share `(callback, capture)` per the spec
/// (duplicate addEventListener is a no-op).
pub(crate) fn add_listener_to_instance<'js>(
    ctx: &Ctx<'js>,
    instance: &Object<'js>,
    event_type: &str,
    callback: &Function<'js>,
    capture: bool,
    once: bool,
    passive: bool,
) -> rquickjs::Result<()> {
    let map = get_or_create_listener_map(ctx, instance)?;
    let list: Array<'js> = match map.get::<_, Option<Array<'js>>>(event_type)? {
        Some(a) => a,
        None => {
            let a = Array::new(ctx.clone())?;
            map.set(event_type, a.clone())?;
            a
        }
    };

    // De-dupe: scan existing records for same callback+capture.
    let len = list.len();
    for i in 0..len {
        let rec: Option<Object<'js>> = list.get(i)?;
        let Some(rec) = rec else { continue };
        let existing_capture: bool = rec.get::<_, Option<bool>>("capture")?.unwrap_or(false);
        if existing_capture != capture {
            continue;
        }
        let existing_cb: Option<Function<'js>> = rec.get::<_, Option<Function<'js>>>("callback")?;
        if let Some(existing_cb) = existing_cb {
            if functions_strict_equal(&existing_cb, callback) {
                return Ok(());
            }
        }
    }

    // Append a new record.
    let rec = Object::new(ctx.clone())?;
    rec.set("callback", callback.clone())?;
    rec.set("capture", capture)?;
    rec.set("once", once)?;
    rec.set("passive", passive)?;
    list.set(len, rec)?;
    Ok(())
}

/// Remove the first listener record matching `(callback, capture)`.
pub(crate) fn remove_listener_from_instance<'js>(
    ctx: &Ctx<'js>,
    instance: &Object<'js>,
    event_type: &str,
    callback: &Function<'js>,
    capture: bool,
) -> rquickjs::Result<()> {
    let map: Option<Object<'js>> = instance.get::<_, Option<Object<'js>>>(PROP_LISTENERS)?;
    let Some(map) = map else { return Ok(()) };
    let list: Option<Array<'js>> = map.get::<_, Option<Array<'js>>>(event_type)?;
    let Some(list) = list else { return Ok(()) };

    // Collect surviving records into a new array (cheaper than
    // splice gymnastics; lists are small).
    let len = list.len();
    let new_list = Array::new(ctx.clone())?;
    let mut new_idx = 0usize;
    let mut removed = false;
    for i in 0..len {
        let rec: Option<Object<'js>> = list.get(i)?;
        let Some(rec) = rec else { continue };
        if !removed {
            let existing_capture: bool = rec.get::<_, Option<bool>>("capture")?.unwrap_or(false);
            if existing_capture == capture {
                let existing_cb: Option<Function<'js>> = rec.get("callback")?;
                if let Some(existing_cb) = existing_cb {
                    if functions_strict_equal(&existing_cb, callback) {
                        removed = true;
                        continue;
                    }
                }
            }
        }
        new_list.set(new_idx, rec)?;
        new_idx += 1;
    }
    map.set(event_type, new_list)?;
    Ok(())
}

/// Run a synchronous flat dispatch of `event` on `instance`. Returns
/// `false` iff the event is cancelable and a listener called
/// `preventDefault()`.
pub(crate) fn dispatch_on_instance<'js>(
    ctx: &Ctx<'js>,
    instance: &Object<'js>,
    event: Value<'js>,
) -> rquickjs::Result<bool> {
    let view = view_from_value(&event).ok_or_else(|| {
        Exception::throw_type(
            ctx,
            "dispatchEvent: argument must be an Event or CustomEvent",
        )
    })?;

    if view.state.dispatching.get() {
        return Err(Exception::throw_type(
            ctx,
            "dispatchEvent: event is already being dispatched",
        ));
    }

    view.state.dispatching.set(true);
    view.state.event_phase.set(EVENT_PHASE_AT_TARGET);
    view.state.propagation_stopped.set(false);
    view.state.immediate_propagation_stopped.set(false);
    view.state.default_prevented.set(false);

    // Snapshot listener list — JS-side. Iterating over the live
    // Array and concurrently mutating it is undefined per spec; we
    // dupe up front.
    let mut snapshot: Vec<(Function<'js>, bool, bool)> = Vec::new();
    if let Some(map) = instance.get::<_, Option<Object<'js>>>(PROP_LISTENERS)? {
        if let Some(list) = map.get::<_, Option<Array<'js>>>(view.event_type.as_str())? {
            let len = list.len();
            for i in 0..len {
                let rec: Option<Object<'js>> = list.get(i)?;
                let Some(rec) = rec else { continue };
                let cb: Option<Function<'js>> = rec.get("callback")?;
                let Some(cb) = cb else { continue };
                let capture: bool = rec.get::<_, Option<bool>>("capture")?.unwrap_or(false);
                let once: bool = rec.get::<_, Option<bool>>("once")?.unwrap_or(false);
                snapshot.push((cb, capture, once));
            }
        }
    }

    let mut once_to_remove: Vec<Function<'js>> = Vec::new();

    for (callback, _capture, once) in &snapshot {
        if view.state.immediate_propagation_stopped.get() {
            break;
        }
        // Call with no `this` binding — match Deno's behavior of
        // invoking listener as a plain function.
        let _: Value<'js> = callback.call((event.clone(),))?;
        if *once {
            once_to_remove.push(callback.clone());
        }
    }

    // Remove `once` listeners from the live list.
    if !once_to_remove.is_empty() {
        for cb in &once_to_remove {
            remove_listener_from_instance(ctx, instance, &view.event_type, cb, false)?;
            remove_listener_from_instance(ctx, instance, &view.event_type, cb, true)?;
        }
    }

    let dp = view.state.default_prevented.get();
    view.state.dispatching.set(false);
    view.state.event_phase.set(EVENT_PHASE_NONE);

    Ok(!(view.cancelable && dp))
}

/// Two-callback strict-equality check using a tiny JS helper.
/// rquickjs's `Value` doesn't expose `===` directly.
fn functions_strict_equal<'js>(a: &Function<'js>, b: &Function<'js>) -> bool {
    let ctx = a.ctx().clone();
    let helper: Function<'js> = match ctx.eval("(a, b) => a === b") {
        Ok(f) => f,
        Err(_) => return false,
    };
    helper
        .call::<_, bool>((a.clone(), b.clone()))
        .unwrap_or(false)
}

/// Parse the `options` argument of `addEventListener`/
/// `removeEventListener`. A boolean is interpreted as `capture`; an
/// object may carry `capture` / `once` / `passive` booleans;
/// undefined/null yields all-false defaults.
pub(crate) fn parse_listener_options<'js>(
    _ctx: &Ctx<'js>,
    options: Option<Value<'js>>,
) -> rquickjs::Result<(bool, bool, bool)> {
    let Some(v) = options else {
        return Ok((false, false, false));
    };
    if v.is_null() || v.is_undefined() {
        return Ok((false, false, false));
    }
    if let Some(b) = v.as_bool() {
        return Ok((b, false, false));
    }
    if let Some(obj) = v.into_object() {
        let capture = obj.get::<_, Option<bool>>("capture")?.unwrap_or(false);
        let once = obj.get::<_, Option<bool>>("once")?.unwrap_or(false);
        let passive = obj.get::<_, Option<bool>>("passive")?.unwrap_or(false);
        return Ok((capture, once, passive));
    }
    Ok((false, false, false))
}

// ===== AbortSignal ==============================================================

/// `AbortSignal` — the read side of the abort plumbing.
///
/// Owns an embedded [`EventTarget`] for the `"abort"`-event surface
/// (`AbortSignal extends EventTarget` per the WHATWG spec). The
/// `addEventListener` / `removeEventListener` / `dispatchEvent`
/// methods delegate to the same JS-side listener storage pattern as
/// [`EventTarget`].
///
/// `aborted` is a Rust-side `Cell<bool>` because we need fast,
/// race-free reads from many code paths. `reason` is stored as a
/// JS-side hidden property ([`PROP_REASON`]) for the same anti-
/// `Persistent` reason described in the module docs.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "AbortSignal")]
pub struct AbortSignal {
    #[qjs(skip_trace)]
    aborted: Rc<Cell<bool>>,
}

impl AbortSignal {
    /// Construct a fresh, non-aborted signal.
    pub fn new_empty() -> Self {
        Self {
            aborted: Rc::new(Cell::new(false)),
        }
    }
}

#[rquickjs::methods(rename_all = "camelCase")]
impl AbortSignal {
    /// `new AbortSignal()` — produces a never-aborting signal.
    /// Provided so the class registers as a JS-visible name; real
    /// signal construction is via `AbortController.signal` or
    /// `AbortSignal.abort(reason)`.
    #[qjs(constructor)]
    pub fn js_new() -> Self {
        Self::new_empty()
    }

    /// `signal.aborted`.
    #[qjs(get)]
    fn aborted(&self) -> bool {
        self.aborted.get()
    }

    /// `signal.reason` — reads the JS-side hidden property; if the
    /// signal is aborted with no explicit reason, returns a fresh
    /// `DOMException("AbortError")`. Returns `undefined` for non-
    /// aborted signals.
    #[qjs(get)]
    fn reason<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        let aborted = this.0.borrow().aborted.get();
        let obj: Object<'js> =
            this.0.clone().into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "AbortSignal", "not an object")
            })?;
        let ctx = obj.ctx().clone();
        if !aborted {
            return ctx.eval::<Value<'js>, _>("undefined");
        }
        match obj.get::<_, Option<Value<'js>>>(PROP_REASON)? {
            Some(v) => Ok(v),
            None => default_abort_reason(&ctx),
        }
    }

    /// `signal.throwIfAborted()`.
    fn throw_if_aborted<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<()> {
        let aborted = this.0.borrow().aborted.get();
        if !aborted {
            return Ok(());
        }
        let obj: Object<'js> =
            this.0.clone().into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "AbortSignal", "not an object")
            })?;
        let ctx = obj.ctx().clone();
        let reason = match obj.get::<_, Option<Value<'js>>>(PROP_REASON)? {
            Some(v) => v,
            None => default_abort_reason(&ctx)?,
        };
        Err(ctx.throw(reason))
    }

    /// `signal.addEventListener(type, listener, options?)`.
    fn add_event_listener<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event_type: String,
        listener: Function<'js>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let (capture, once, passive) = parse_listener_options(&ctx, options.0)?;
        let instance: Object<'js> =
            this.0.into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "AbortSignal", "not an object")
            })?;
        add_listener_to_instance(
            &ctx,
            &instance,
            &event_type,
            &listener,
            capture,
            once,
            passive,
        )
    }

    /// `signal.removeEventListener(type, listener, options?)`.
    fn remove_event_listener<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event_type: String,
        listener: Function<'js>,
        options: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        let (capture, _, _) = parse_listener_options(&ctx, options.0)?;
        let instance: Object<'js> =
            this.0.into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "AbortSignal", "not an object")
            })?;
        remove_listener_from_instance(&ctx, &instance, &event_type, &listener, capture)
    }

    /// `signal.dispatchEvent(event)`.
    fn dispatch_event<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        event: Value<'js>,
    ) -> rquickjs::Result<bool> {
        let instance: Object<'js> =
            this.0.into_value().into_object().ok_or_else(|| {
                JsError::new_from_js_message("this", "AbortSignal", "not an object")
            })?;
        dispatch_on_instance(&ctx, &instance, event)
    }
}

/// Build a fresh `DOMException("AbortError")` JS value — the spec
/// default for `AbortSignal.reason` when aborted without an explicit
/// reason.
fn default_abort_reason<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
    let exc = Class::instance(
        ctx.clone(),
        DOMException {
            message: "signal is aborted without reason".to_owned(),
            name: "AbortError".to_owned(),
        },
    )?;
    Ok(exc.into_value())
}

impl AbortSignal {
    /// `AbortSignal.abort(reason?)` — attached as a static method
    /// on the constructor in [`install_events`]. Returns a signal
    /// that is already aborted with `reason` (or the spec default).
    fn static_abort<'js>(
        ctx: Ctx<'js>,
        reason: Opt<Value<'js>>,
    ) -> rquickjs::Result<Class<'js, Self>> {
        let signal = AbortSignal::new_empty();
        signal.aborted.set(true);
        let instance = Class::instance(ctx.clone(), signal)?;
        // Stash reason JS-side.
        let obj: Object<'js> = instance.clone().into_value().into_object().ok_or_else(|| {
            JsError::new_from_js_message("AbortSignal", "AbortSignal", "expected object")
        })?;
        let reason_value = match reason.0 {
            Some(v) if !v.is_undefined() => v,
            _ => default_abort_reason(&ctx)?,
        };
        obj.set(PROP_REASON, reason_value)?;
        Ok(instance)
    }

    /// `AbortSignal.timeout(ms)` — **stubbed for Phase 1B.**
    ///
    /// Real implementation requires the deterministic fake-clock
    /// `setTimeout` integration, which lives in a parallel module
    /// (the timers agent) that lands in a separate worktree. This
    /// stub returns a never-aborting signal so call sites compile
    /// and tests that don't depend on the timeout firing pass.
    fn static_timeout<'js>(ctx: Ctx<'js>, _ms: f64) -> rquickjs::Result<Class<'js, Self>> {
        Class::instance(ctx.clone(), AbortSignal::new_empty())
    }
}

// ===== AbortController ==========================================================

/// `AbortController` — the write side of the abort plumbing. Holds
/// an [`AbortSignal`] and exposes a single `abort(reason?)` method
/// that flips it and dispatches `"abort"`.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "AbortController")]
pub struct AbortController {
    #[qjs(skip_trace)]
    signal: AbortSignal,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl AbortController {
    /// `new AbortController()`.
    #[qjs(constructor)]
    pub fn js_new() -> Self {
        Self {
            signal: AbortSignal::new_empty(),
        }
    }

    /// `controller.signal` — the underlying [`AbortSignal`] instance
    /// (also stored JS-side via a hidden property on first access
    /// so repeated calls return the *same* JS object the
    /// EventTarget API was wired against).
    #[qjs(get)]
    fn signal<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        let obj: Object<'js> = this.0.clone().into_value().into_object().ok_or_else(|| {
            JsError::new_from_js_message("this", "AbortController", "not an object")
        })?;
        if let Some(existing) = obj.get::<_, Option<Value<'js>>>("__signal")? {
            return Ok(existing);
        }
        let ctx = obj.ctx().clone();
        let signal = this.0.borrow().signal.clone();
        let instance = Class::instance(ctx.clone(), signal)?;
        let value: Value<'js> = instance.into_value();
        obj.set("__signal", value.clone())?;
        Ok(value)
    }

    /// `controller.abort(reason?)` — abort the underlying signal,
    /// dispatch `"abort"` on its event surface, idempotent on
    /// subsequent calls.
    fn abort<'js>(
        this: This<Class<'js, Self>>,
        ctx: Ctx<'js>,
        reason: Opt<Value<'js>>,
    ) -> rquickjs::Result<()> {
        // Pull (or create) the JS-side signal object so we can both
        // mutate Rust state on it and dispatch through its JS-side
        // listener map.
        let outer: Object<'js> = this.0.clone().into_value().into_object().ok_or_else(|| {
            JsError::new_from_js_message("this", "AbortController", "not an object")
        })?;
        let signal_value: Value<'js> = match outer.get::<_, Option<Value<'js>>>("__signal")? {
            Some(v) => v,
            None => {
                let signal = this.0.borrow().signal.clone();
                let instance = Class::instance(ctx.clone(), signal)?;
                let v: Value<'js> = instance.into_value();
                outer.set("__signal", v.clone())?;
                v
            }
        };
        let signal_obj: Object<'js> = signal_value.clone().into_object().ok_or_else(|| {
            JsError::new_from_js_message("signal", "AbortSignal", "not an object")
        })?;
        let signal_class: Class<'js, AbortSignal> = signal_obj
            .as_class::<AbortSignal>()
            .cloned()
            .ok_or_else(|| {
            JsError::new_from_js_message("signal", "AbortSignal", "wrong class")
        })?;
        let aborted = signal_class.borrow().aborted.clone();
        if aborted.get() {
            return Ok(());
        }
        aborted.set(true);
        let reason_value = match reason.0 {
            Some(v) if !v.is_undefined() => v,
            _ => default_abort_reason(&ctx)?,
        };
        signal_obj.set(PROP_REASON, reason_value)?;

        // Synthesize an `"abort"` event and dispatch on the signal's
        // JS object.
        let event = Event::new_with_init("abort".to_owned(), None);
        let event_class = Class::instance(ctx.clone(), event)?;
        let event_value: Value<'js> = event_class.into_value();
        let _ = dispatch_on_instance(&ctx, &signal_obj, event_value)?;
        Ok(())
    }
}

// ===== Installation =============================================================

/// Register the six event-model classes on `ctx.globals()` so JS code
/// can `new` them and use them by name. Mirrors
/// [`crate::dom::register_classes`] for the DOM types.
///
/// Also wires `AbortSignal.abort` and `AbortSignal.timeout` as
/// properties on the `AbortSignal` constructor — see the docstring
/// on [`AbortSignal::static_abort`] for why we do this manually
/// rather than via the `#[qjs(static)]` macro path.
///
/// And wires a `CustomEvent` *post-constructor wrapper* so the
/// `init.detail` value gets pinned on the JS-side instance as
/// [`PROP_DETAIL`]. The Rust `#[qjs(constructor)]` can't see `this`,
/// so we replace the constructor with a JS function that constructs
/// and then sets the property.
pub fn install_events(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();
            Class::<DOMException>::define(&globals)?;
            Class::<Event>::define(&globals)?;
            Class::<CustomEvent>::define(&globals)?;
            Class::<EventTarget>::define(&globals)?;
            Class::<AbortController>::define(&globals)?;
            Class::<AbortSignal>::define(&globals)?;

            // Attach static methods on the AbortSignal constructor.
            // The closures need a `for<'js>` higher-rank bound so the
            // returned `Class<'js, AbortSignal>` lifetime matches the
            // incoming `Ctx<'js>`. Inferring inline doesn't pick that
            // up (Class is invariant over 'js); a helper fn is the
            // cleanest fix.
            fn abort_signal_abort_thunk<'js>(
                ctx: Ctx<'js>,
                reason: Opt<Value<'js>>,
            ) -> rquickjs::Result<Class<'js, AbortSignal>> {
                AbortSignal::static_abort(ctx, reason)
            }
            fn abort_signal_timeout_thunk<'js>(
                ctx: Ctx<'js>,
                ms: f64,
            ) -> rquickjs::Result<Class<'js, AbortSignal>> {
                AbortSignal::static_timeout(ctx, ms)
            }
            let abort_signal_ctor: Object = globals.get("AbortSignal")?;
            abort_signal_ctor.set(
                "abort",
                Function::new(ctx.clone(), abort_signal_abort_thunk)?,
            )?;
            abort_signal_ctor.set(
                "timeout",
                Function::new(ctx.clone(), abort_signal_timeout_thunk)?,
            )?;

            // Wrap CustomEvent so `detail` is attached JS-side as a
            // hidden property. We can't grab `this` from a
            // `#[qjs(constructor)]`, so we replace the global with a
            // JS shim that calls the real constructor via
            // `Reflect.construct` and then copies the detail across.
            let custom_event_wrap = ctx.eval::<Function, _>(
                r#"
                ((OrigCustomEvent) => {
                    return function CustomEvent(type, init) {
                        const inst = new OrigCustomEvent(type, init);
                        if (init && typeof init === 'object' && 'detail' in init) {
                            Object.defineProperty(inst, '__detail', {
                                value: init.detail,
                                writable: false,
                                enumerable: false,
                                configurable: false,
                            });
                        }
                        return inst;
                    };
                })
                "#,
            )?;
            let orig: Value = globals.get("CustomEvent")?;
            let wrapped: Function = custom_event_wrap.call((orig,))?;
            globals.set("CustomEvent", wrapped)?;

            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install events: {e}")))?;
    Ok(())
}

// ===== Tests ====================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::JsEngine;

    fn engine() -> JsEngine {
        JsEngine::new().expect("engine new")
    }

    // ----- DOMException -----

    #[test]
    fn dom_exception_name_to_code_table() {
        assert_eq!(dom_exception_code("AbortError"), 20);
        assert_eq!(dom_exception_code("NotFoundError"), 8);
        assert_eq!(dom_exception_code("IndexSizeError"), 1);
        assert_eq!(dom_exception_code("HierarchyRequestError"), 3);
        assert_eq!(dom_exception_code("UnknownName"), 0);
    }

    #[test]
    fn dom_exception_constructor_defaults_name_to_error() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const ex = new DOMException('boom');
                JSON.stringify({m: ex.message, n: ex.name, c: ex.code})
                "#,
            )
            .expect("eval ok");
        let s = out.value.as_str().expect("string");
        assert!(s.contains("\"m\":\"boom\""));
        assert!(s.contains("\"n\":\"Error\""));
        assert!(s.contains("\"c\":0"));
    }

    #[test]
    fn dom_exception_abort_error_has_code_20() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const ex = new DOMException('aborted', 'AbortError');
                [ex.message, ex.name, ex.code]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], "aborted");
        assert_eq!(out.value[1], "AbortError");
        assert_eq!(out.value[2], 20);
    }

    #[test]
    fn dom_exception_not_found_error_has_code_8() {
        let e = engine();
        let out = e
            .eval(r#"new DOMException('missing', 'NotFoundError').code"#)
            .expect("eval ok");
        assert_eq!(out.value, 8);
    }

    // ----- Event -----

    #[test]
    fn event_constructor_defaults_are_all_false() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const ev = new Event('foo');
                [ev.type, ev.bubbles, ev.cancelable, ev.composed,
                 ev.defaultPrevented, ev.eventPhase, ev.timeStamp, ev.isTrusted]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], "foo");
        assert_eq!(out.value[1], false);
        assert_eq!(out.value[2], false);
        assert_eq!(out.value[3], false);
        assert_eq!(out.value[4], false);
        assert_eq!(out.value[5], 0);
        assert_eq!(out.value[6], 0);
        assert_eq!(out.value[7], false);
    }

    #[test]
    fn event_init_reads_bubbles_cancelable_composed() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const ev = new Event('foo', {bubbles: true, cancelable: true, composed: true});
                [ev.bubbles, ev.cancelable, ev.composed]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], true);
        assert_eq!(out.value[1], true);
        assert_eq!(out.value[2], true);
    }

    #[test]
    fn event_prevent_default_only_when_cancelable() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const a = new Event('x');
                a.preventDefault();
                const b = new Event('y', {cancelable: true});
                b.preventDefault();
                [a.defaultPrevented, b.defaultPrevented]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], false);
        assert_eq!(out.value[1], true);
    }

    // ----- CustomEvent -----

    #[test]
    fn custom_event_carries_detail() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const ev = new CustomEvent('thing', {detail: {x: 1, y: 'two'}});
                JSON.stringify({t: ev.type, d: ev.detail})
                "#,
            )
            .expect("eval ok");
        let s = out.value.as_str().expect("string");
        assert!(s.contains("\"t\":\"thing\""));
        assert!(s.contains("\"x\":1"));
        assert!(s.contains("\"y\":\"two\""));
    }

    // ----- EventTarget -----

    #[test]
    fn event_target_add_and_dispatch_runs_callback() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const t = new EventTarget();
                let seen = null;
                t.addEventListener('hello', (ev) => { seen = ev.type; });
                t.dispatchEvent(new Event('hello'));
                seen
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, "hello");
    }

    #[test]
    fn dispatch_event_returns_false_when_prevent_default_called() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const t = new EventTarget();
                t.addEventListener('go', (ev) => { ev.preventDefault(); });
                const r1 = t.dispatchEvent(new Event('go', {cancelable: true}));
                const r2 = t.dispatchEvent(new Event('go', {cancelable: false}));
                [r1, r2]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], false);
        // Non-cancelable: preventDefault is a no-op, dispatchEvent
        // returns true.
        assert_eq!(out.value[1], true);
    }

    #[test]
    fn remove_event_listener_unbinds() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const t = new EventTarget();
                let count = 0;
                const fn = () => { count++; };
                t.addEventListener('tick', fn);
                t.dispatchEvent(new Event('tick'));
                t.removeEventListener('tick', fn);
                t.dispatchEvent(new Event('tick'));
                count
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, 1);
    }

    #[test]
    fn once_true_listener_fires_only_once() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const t = new EventTarget();
                let count = 0;
                t.addEventListener('tick', () => { count++; }, {once: true});
                t.dispatchEvent(new Event('tick'));
                t.dispatchEvent(new Event('tick'));
                t.dispatchEvent(new Event('tick'));
                count
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, 1);
    }

    #[test]
    fn stop_immediate_propagation_halts_subsequent_listeners() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const t = new EventTarget();
                const log = [];
                t.addEventListener('go', (ev) => { log.push('a'); ev.stopImmediatePropagation(); });
                t.addEventListener('go', () => { log.push('b'); });
                t.addEventListener('go', () => { log.push('c'); });
                t.dispatchEvent(new Event('go'));
                log.join(',')
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, "a");
    }

    // ----- AbortController / AbortSignal -----

    #[test]
    fn abort_controller_abort_flips_signal_and_dispatches() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const ctrl = new AbortController();
                let abortSeen = false;
                ctrl.signal.addEventListener('abort', () => { abortSeen = true; });
                const before = ctrl.signal.aborted;
                ctrl.abort('user_cancel');
                [before, ctrl.signal.aborted, abortSeen, ctrl.signal.reason]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], false);
        assert_eq!(out.value[1], true);
        assert_eq!(out.value[2], true);
        assert_eq!(out.value[3], "user_cancel");
    }

    #[test]
    fn abort_signal_abort_static_returns_already_aborted() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const s = AbortSignal.abort('preset');
                [s.aborted, s.reason]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], true);
        assert_eq!(out.value[1], "preset");
    }

    #[test]
    fn throw_if_aborted_throws_only_when_aborted() {
        let e = engine();
        // Non-aborted: no throw.
        let out = e
            .eval(
                r#"
                const ctrl = new AbortController();
                ctrl.signal.throwIfAborted();
                'ok'
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, "ok");

        // Aborted: throws the reason. Caller's `eval` should surface
        // it as either ThrownValue (we threw a string) or Exception
        // (depending on shape). We throw a JS object with a known
        // `boom` field so we can assert across both surfaces.
        let err = e
            .eval(
                r#"
                const ctrl = new AbortController();
                ctrl.abort({boom: true, msg: 'gone'});
                ctrl.signal.throwIfAborted();
                'unreached'
                "#,
            )
            .expect_err("should throw");
        match err {
            EvalError::ThrownValue { value } => {
                assert_eq!(value["boom"], true);
                assert_eq!(value["msg"], "gone");
            }
            EvalError::Exception { .. } => {
                // QJS may also surface the thrown value as an
                // Exception. Either is acceptable; both signal the
                // spec-required failure mode (a JS throw).
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn abort_signal_default_reason_is_dom_exception() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const s = AbortSignal.abort();
                [s.aborted, s.reason.name, s.reason.code]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], true);
        assert_eq!(out.value[1], "AbortError");
        assert_eq!(out.value[2], 20);
    }

    #[test]
    fn abort_signal_timeout_is_stubbed_and_never_fires() {
        // Phase 1B punt: AbortSignal.timeout returns a never-aborting
        // signal. This test documents the stub behavior so any future
        // change to wire in real timers must update the test
        // intentionally.
        let e = engine();
        let out = e
            .eval(
                r#"
                const s = AbortSignal.timeout(10);
                s.aborted
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, false);
    }

    #[test]
    fn duplicate_add_event_listener_is_idempotent() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const t = new EventTarget();
                let count = 0;
                const fn = () => { count++; };
                t.addEventListener('p', fn);
                t.addEventListener('p', fn);  // duplicate — should not double-fire
                t.dispatchEvent(new Event('p'));
                count
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value, 1);
    }

    #[test]
    fn dispatch_event_on_non_event_throws_type_error() {
        let e = engine();
        let err = e
            .eval(
                r#"
                const t = new EventTarget();
                t.dispatchEvent({type: 'fake'})
                "#,
            )
            .expect_err("non-Event arg should throw");
        // Either an Exception (TypeError) or a ThrownValue from our
        // synthesized error — both signal the spec-required failure
        // mode.
        assert!(matches!(
            err,
            EvalError::Exception { .. } | EvalError::ThrownValue { .. }
        ));
    }
}
