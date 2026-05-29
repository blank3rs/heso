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
    atom::PredefinedAtom,
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
const PROP_TARGET: &str = "__target";
/// Hidden JS-side property tracking the per-node `currentTarget` during
/// a tree-aware dispatch walk. Updated on every node visited by
/// [`dispatch_with_node_path`]; cleared back to the original target at
/// the end of dispatch. Reading `event.currentTarget` from JS prefers
/// this property and falls back to [`PROP_TARGET`].
const PROP_CURRENT_TARGET: &str = "__currentTarget";

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
    /// `Error.prototype.toString` shape. Delegates to the [`Display`]
    /// impl so Rust code and JS land print the same bytes.
    ///
    /// [`Display`]: std::fmt::Display
    #[qjs(rename = PredefinedAtom::ToString)]
    fn to_string_method(&self) -> String {
        format!("{self}")
    }
}

impl std::fmt::Display for DOMException {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "DOMException")
        } else {
            write!(f, "DOMException: {}", self.message)
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

    /// `e.target` — the element on which `dispatchEvent` was
    /// invoked. Stored as a JS-side hidden property by the dispatch
    /// path (see [`dispatch_with_map`]) rather than as a Rust-side
    /// `Persistent`, for the same anti-Persistent-footgun reasons
    /// described in the module docs.
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }

    /// `e.currentTarget` — the node whose listeners are currently
    /// being invoked. Updated per-node by the path-walking dispatcher
    /// ([`dispatch_with_node_path`]). Falls back to `target` for the
    /// flat-dispatch path (EventTarget / AbortSignal), which never
    /// updates `__currentTarget`.
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
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

    /// `e.target` — see [`Event::target`].
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }

    /// `e.currentTarget` — see [`Event::current_target`].
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
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

// ===== UI Events subclasses =====================================================
//
// WHATWG UI Events ([uievents]) defines a small hierarchy on top of
// the base `Event`:
//
//   Event
//   └─ UIEvent
//      ├─ FocusEvent
//      ├─ KeyboardEvent
//      ├─ InputEvent
//      └─ MouseEvent
//         ├─ PointerEvent
//         └─ WheelEvent
//
// Each subclass extends its parent with additional read-only init
// fields. Browsers expose all of them as `globalThis` constructors so
// pages can build synthetic events that the framework's `onChange` /
// `onKeyDown` / `onClick` handlers — which probe `event.key`,
// `event.button`, `event.shiftKey`, etc. — recognize as real.
//
// rquickjs's `#[rquickjs::class]` doesn't natively express IDL
// inheritance, so each is its own struct here. The prototype chain is
// rewired in `install_events` via `Object.setPrototypeOf`, the same
// dance `File extends Blob` uses in `web_apis.rs`. That makes both
// `instanceof KeyboardEvent` AND `instanceof Event` return `true` for
// a `new KeyboardEvent(...)` instance.
//
// OSS cross-referenced:
// - jsdom `lib/jsdom/living/events/{UIEvent,KeyboardEvent,InputEvent,
//   MouseEvent,PointerEvent,WheelEvent,FocusEvent}-impl.js` — MIT.
//   Source-of-truth for the per-class init-dictionary field set and
//   spec-default values.
// - happy-dom `src/event/events/{KeyboardEvent,InputEvent,MouseEvent,
//   PointerEvent,WheelEvent,FocusEvent}.ts` — MIT. Cleaner reference
//   for the read-only-IDL-attribute shape.
//
// [uievents]: https://w3c.github.io/uievents/

/// Common base fields for every UI Events subclass.
///
/// Each subclass below carries one of these plus its own extra
/// constructor-time-only fields. Kept in a small struct so the
/// per-class boilerplate stays small.
#[derive(Clone)]
struct EventBase {
    event_type: String,
    bubbles: bool,
    cancelable: bool,
    composed: bool,
    state: Rc<EventState>,
}

impl EventBase {
    fn from_init(event_type: String, init: EventInit) -> Self {
        Self {
            event_type,
            bubbles: init.bubbles,
            cancelable: init.cancelable,
            composed: init.composed,
            state: Rc::new(EventState::default()),
        }
    }
}

// ----- UIEvent ------------------------------------------------------

/// Optional fields added by [`UIEvent`] (and inherited by every
/// subclass below it). `view` is stored as a JS-side hidden property
/// because it must be a JS value (a `Window`-like reference); the rest
/// are primitives.
#[derive(Default, Clone)]
struct UIEventInit {
    detail: i64,
}

const PROP_UI_VIEW: &str = "__uiView";

fn parse_ui_event_init<'js>(
    ctx: &Ctx<'js>,
    init: Option<Value<'js>>,
) -> rquickjs::Result<(EventInit, UIEventInit, Option<Value<'js>>)> {
    let Some(value) = init else {
        return Ok((EventInit::default(), UIEventInit::default(), None));
    };
    if value.is_null() || value.is_undefined() {
        return Ok((EventInit::default(), UIEventInit::default(), None));
    }
    let base = parse_event_init(ctx, Some(value.clone()))?;
    let obj = value
        .clone()
        .into_object()
        .ok_or_else(|| JsError::new_from_js_message("init", "UIEventInit", "expected an object"))?;
    let detail = obj.get::<_, Option<i64>>("detail")?.unwrap_or(0);
    let view = obj.get::<_, Option<Value<'js>>>("view")?;
    Ok((base, UIEventInit { detail }, view))
}

/// `UIEvent` — base for `KeyboardEvent` / `InputEvent` / `MouseEvent`
/// / `FocusEvent` / etc. Adds `detail` and `view`.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "UIEvent")]
pub struct UIEvent {
    #[qjs(skip_trace)]
    base: EventBase,
    #[qjs(skip_trace)]
    detail: i64,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl UIEvent {
    /// `new UIEvent(type, init?)`. `init` extends `EventInit` with
    /// `detail: number` and `view: Window`.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let (base_init, ui_init, view) = parse_ui_event_init(&ctx, init.0)?;
        let _ = view; // Stashed by the JS-side constructor wrapper.
        Ok(Self {
            base: EventBase::from_init(event_type, base_init),
            detail: ui_init.detail,
        })
    }

    /// `e.type`.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.base.event_type.clone()
    }
    /// `e.bubbles`.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.base.bubbles
    }
    /// `e.cancelable`.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.base.cancelable
    }
    /// `e.composed`.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.base.composed
    }
    /// `e.defaultPrevented`.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.base.state.default_prevented.get()
    }
    /// `e.eventPhase`.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.base.state.event_phase.get()
    }
    /// `e.timeStamp` — 0 per the Phase-1B punt.
    #[qjs(get)]
    fn time_stamp(&self) -> f64 {
        0.0
    }
    /// `e.isTrusted` — synthesized events always report `false`.
    #[qjs(get)]
    fn is_trusted(&self) -> bool {
        false
    }
    /// `e.detail` — UIEvent-specific click-count-ish counter.
    #[qjs(get)]
    fn detail(&self) -> i64 {
        self.detail
    }
    /// `e.view` — the `Window`-like reference passed at construction.
    /// Stored JS-side because it's an arbitrary JS value, not a
    /// primitive.
    #[qjs(get)]
    fn view<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        view_property(this.0.clone().into_value())
    }
    /// `e.target` — see [`Event::target`].
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }
    /// `e.currentTarget` — see [`Event::current_target`].
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
    }
    /// `e.preventDefault()`.
    fn prevent_default(&self) {
        if self.base.cancelable {
            self.base.state.default_prevented.set(true);
        }
    }
    /// `e.stopPropagation()`.
    fn stop_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
    }
    /// `e.stopImmediatePropagation()`.
    fn stop_immediate_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
        self.base.state.immediate_propagation_stopped.set(true);
    }
}

// ----- KeyboardEvent ------------------------------------------------

/// Init dictionary for [`KeyboardEvent`]. Per
/// [UI Events §5.6.4](https://w3c.github.io/uievents/#interface-keyboardeventinit).
#[derive(Default, Clone)]
struct KeyboardEventInit {
    key: String,
    code: String,
    location: u32,
    repeat: bool,
    is_composing: bool,
    ctrl_key: bool,
    shift_key: bool,
    alt_key: bool,
    meta_key: bool,
    char_code: u32,
    key_code: u32,
    which: u32,
}

fn parse_keyboard_event_init<'js>(
    ctx: &Ctx<'js>,
    init: Option<Value<'js>>,
) -> rquickjs::Result<(EventInit, UIEventInit, Option<Value<'js>>, KeyboardEventInit)> {
    let (base, ui, view) = parse_ui_event_init(ctx, init.clone())?;
    let Some(value) = init else {
        return Ok((base, ui, view, KeyboardEventInit::default()));
    };
    if value.is_null() || value.is_undefined() {
        return Ok((base, ui, view, KeyboardEventInit::default()));
    }
    let obj = value.into_object().ok_or_else(|| {
        JsError::new_from_js_message("init", "KeyboardEventInit", "expected an object")
    })?;
    Ok((
        base,
        ui,
        view,
        KeyboardEventInit {
            key: obj.get::<_, Option<String>>("key")?.unwrap_or_default(),
            code: obj.get::<_, Option<String>>("code")?.unwrap_or_default(),
            location: obj.get::<_, Option<u32>>("location")?.unwrap_or(0),
            repeat: obj.get::<_, Option<bool>>("repeat")?.unwrap_or(false),
            is_composing: obj
                .get::<_, Option<bool>>("isComposing")?
                .unwrap_or(false),
            ctrl_key: obj.get::<_, Option<bool>>("ctrlKey")?.unwrap_or(false),
            shift_key: obj.get::<_, Option<bool>>("shiftKey")?.unwrap_or(false),
            alt_key: obj.get::<_, Option<bool>>("altKey")?.unwrap_or(false),
            meta_key: obj.get::<_, Option<bool>>("metaKey")?.unwrap_or(false),
            char_code: obj.get::<_, Option<u32>>("charCode")?.unwrap_or(0),
            key_code: obj.get::<_, Option<u32>>("keyCode")?.unwrap_or(0),
            which: obj.get::<_, Option<u32>>("which")?.unwrap_or(0),
        },
    ))
}

/// `KeyboardEvent` — fires on `keydown` / `keyup` / `keypress`. Carries
/// `key` (printable char or named key), `code` (physical key), plus the
/// four modifier flags + the legacy `keyCode` / `which` / `charCode`
/// trio. Frameworks (React in particular) probe `event.key` and
/// `event.shiftKey` so a partial-shape synthetic was useless.
///
/// Per [UI Events §5.6](https://w3c.github.io/uievents/#interface-keyboardevent).
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "KeyboardEvent")]
pub struct KeyboardEvent {
    #[qjs(skip_trace)]
    base: EventBase,
    #[qjs(skip_trace)]
    detail: i64,
    #[qjs(skip_trace)]
    kb: KeyboardEventInit,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl KeyboardEvent {
    /// `new KeyboardEvent(type, init?)`. See [`KeyboardEventInit`]
    /// (private struct) for the accepted fields.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let (base, ui, _view, kb) = parse_keyboard_event_init(&ctx, init.0)?;
        Ok(Self {
            base: EventBase::from_init(event_type, base),
            detail: ui.detail,
            kb,
        })
    }

    // ---- Event base getters (mirror UIEvent) ----
    /// `e.type`.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.base.event_type.clone()
    }
    /// `e.bubbles`.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.base.bubbles
    }
    /// `e.cancelable`.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.base.cancelable
    }
    /// `e.composed`.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.base.composed
    }
    /// `e.defaultPrevented`.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.base.state.default_prevented.get()
    }
    /// `e.eventPhase`.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.base.state.event_phase.get()
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
    /// `e.detail` (from UIEvent).
    #[qjs(get)]
    fn detail(&self) -> i64 {
        self.detail
    }
    /// `e.view` (from UIEvent).
    #[qjs(get)]
    fn view<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        view_property(this.0.clone().into_value())
    }
    /// `e.target`.
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }
    /// `e.currentTarget`.
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
    }
    /// `e.preventDefault()`.
    fn prevent_default(&self) {
        if self.base.cancelable {
            self.base.state.default_prevented.set(true);
        }
    }
    /// `e.stopPropagation()`.
    fn stop_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
    }
    /// `e.stopImmediatePropagation()`.
    fn stop_immediate_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
        self.base.state.immediate_propagation_stopped.set(true);
    }

    // ---- KeyboardEvent-specific ----
    /// `e.key` — printable character or named key (e.g. `"a"`, `"Enter"`).
    #[qjs(get)]
    fn key(&self) -> String {
        self.kb.key.clone()
    }
    /// `e.code` — physical key identifier (e.g. `"KeyA"`, `"Enter"`).
    #[qjs(get)]
    fn code(&self) -> String {
        self.kb.code.clone()
    }
    /// `e.location` — `KeyboardEvent.DOM_KEY_LOCATION_*`.
    #[qjs(get)]
    fn location(&self) -> u32 {
        self.kb.location
    }
    /// `e.repeat` — true on auto-repeat.
    #[qjs(get)]
    fn repeat(&self) -> bool {
        self.kb.repeat
    }
    /// `e.isComposing` — IME composition session active.
    #[qjs(get)]
    fn is_composing(&self) -> bool {
        self.kb.is_composing
    }
    /// `e.ctrlKey`.
    #[qjs(get)]
    fn ctrl_key(&self) -> bool {
        self.kb.ctrl_key
    }
    /// `e.shiftKey`.
    #[qjs(get)]
    fn shift_key(&self) -> bool {
        self.kb.shift_key
    }
    /// `e.altKey`.
    #[qjs(get)]
    fn alt_key(&self) -> bool {
        self.kb.alt_key
    }
    /// `e.metaKey`.
    #[qjs(get)]
    fn meta_key(&self) -> bool {
        self.kb.meta_key
    }
    /// `e.charCode` — legacy, deprecated.
    #[qjs(get)]
    fn char_code(&self) -> u32 {
        self.kb.char_code
    }
    /// `e.keyCode` — legacy, deprecated.
    #[qjs(get)]
    fn key_code(&self) -> u32 {
        self.kb.key_code
    }
    /// `e.which` — legacy, deprecated.
    #[qjs(get)]
    fn which(&self) -> u32 {
        self.kb.which
    }
    /// `e.getModifierState(key)` — returns whether the named modifier
    /// is currently pressed. We honor the four canonical modifiers
    /// `Control` / `Shift` / `Alt` / `Meta`; everything else returns
    /// `false` (we don't track CapsLock / NumLock / etc.).
    fn get_modifier_state(&self, key: String) -> bool {
        match key.as_str() {
            "Control" => self.kb.ctrl_key,
            "Shift" => self.kb.shift_key,
            "Alt" => self.kb.alt_key,
            "Meta" => self.kb.meta_key,
            _ => false,
        }
    }
}

// ----- InputEvent ---------------------------------------------------

/// Init dictionary for [`InputEvent`]. Per
/// [UI Events §5.7.6](https://w3c.github.io/uievents/#interface-inputeventinit).
#[derive(Default, Clone)]
struct InputEventInit {
    data: Option<String>,
    input_type: String,
    is_composing: bool,
}

fn parse_input_event_init<'js>(
    ctx: &Ctx<'js>,
    init: Option<Value<'js>>,
) -> rquickjs::Result<(EventInit, UIEventInit, Option<Value<'js>>, InputEventInit)> {
    let (base, ui, view) = parse_ui_event_init(ctx, init.clone())?;
    let Some(value) = init else {
        return Ok((base, ui, view, InputEventInit::default()));
    };
    if value.is_null() || value.is_undefined() {
        return Ok((base, ui, view, InputEventInit::default()));
    }
    let obj = value.into_object().ok_or_else(|| {
        JsError::new_from_js_message("init", "InputEventInit", "expected an object")
    })?;
    Ok((
        base,
        ui,
        view,
        InputEventInit {
            data: obj.get::<_, Option<String>>("data")?,
            input_type: obj.get::<_, Option<String>>("inputType")?.unwrap_or_default(),
            is_composing: obj
                .get::<_, Option<bool>>("isComposing")?
                .unwrap_or(false),
        },
    ))
}

/// `InputEvent` — `input` / `beforeinput`. Carries `data` (the
/// inserted text, may be null) and `inputType` (e.g. `"insertText"`,
/// `"deleteContentBackward"`).
///
/// Per [UI Events §5.7](https://w3c.github.io/uievents/#interface-inputevent).
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "InputEvent")]
pub struct InputEvent {
    #[qjs(skip_trace)]
    base: EventBase,
    #[qjs(skip_trace)]
    detail: i64,
    #[qjs(skip_trace)]
    ie: InputEventInit,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl InputEvent {
    /// `new InputEvent(type, init?)`. See [`InputEventInit`] (private
    /// struct) for the accepted fields.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let (base, ui, _view, ie) = parse_input_event_init(&ctx, init.0)?;
        Ok(Self {
            base: EventBase::from_init(event_type, base),
            detail: ui.detail,
            ie,
        })
    }

    /// `e.type`.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.base.event_type.clone()
    }
    /// `e.bubbles`.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.base.bubbles
    }
    /// `e.cancelable`.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.base.cancelable
    }
    /// `e.composed`.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.base.composed
    }
    /// `e.defaultPrevented`.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.base.state.default_prevented.get()
    }
    /// `e.eventPhase`.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.base.state.event_phase.get()
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
    /// `e.detail`.
    #[qjs(get)]
    fn detail(&self) -> i64 {
        self.detail
    }
    /// `e.view`.
    #[qjs(get)]
    fn view<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        view_property(this.0.clone().into_value())
    }
    /// `e.target`.
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }
    /// `e.currentTarget`.
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
    }
    /// `e.preventDefault()`.
    fn prevent_default(&self) {
        if self.base.cancelable {
            self.base.state.default_prevented.set(true);
        }
    }
    /// `e.stopPropagation()`.
    fn stop_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
    }
    /// `e.stopImmediatePropagation()`.
    fn stop_immediate_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
        self.base.state.immediate_propagation_stopped.set(true);
    }

    // ---- InputEvent-specific ----
    /// `e.data` — the inserted text (or `null` when unavailable, e.g.
    /// `deleteContentBackward`).
    #[qjs(get)]
    fn data<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        let borrowed = this.0.borrow();
        let ctx = this.0.clone().into_value().ctx().clone();
        match &borrowed.ie.data {
            Some(s) => Ok(rquickjs::String::from_str(ctx, s)?.into_value()),
            None => ctx.eval::<Value<'js>, _>("null"),
        }
    }
    /// `e.inputType` — see WHATWG Input Events list of `inputType`
    /// values; commonly `"insertText"` / `"deleteContentBackward"`.
    #[qjs(get)]
    fn input_type(&self) -> String {
        self.ie.input_type.clone()
    }
    /// `e.isComposing`.
    #[qjs(get)]
    fn is_composing(&self) -> bool {
        self.ie.is_composing
    }
}

// ----- MouseEvent ---------------------------------------------------

/// Init dictionary for [`MouseEvent`]. Per
/// [UI Events §5.4.6](https://w3c.github.io/uievents/#interface-mouseeventinit).
#[derive(Default, Clone)]
struct MouseEventInit {
    screen_x: f64,
    screen_y: f64,
    client_x: f64,
    client_y: f64,
    button: i16,
    buttons: u32,
    ctrl_key: bool,
    shift_key: bool,
    alt_key: bool,
    meta_key: bool,
    movement_x: f64,
    movement_y: f64,
}

#[allow(clippy::type_complexity)]
fn parse_mouse_event_init<'js>(
    ctx: &Ctx<'js>,
    init: Option<Value<'js>>,
) -> rquickjs::Result<(
    EventInit,
    UIEventInit,
    Option<Value<'js>>,
    Option<Value<'js>>,
    MouseEventInit,
)> {
    let (base, ui, view) = parse_ui_event_init(ctx, init.clone())?;
    let Some(value) = init else {
        return Ok((base, ui, view, None, MouseEventInit::default()));
    };
    if value.is_null() || value.is_undefined() {
        return Ok((base, ui, view, None, MouseEventInit::default()));
    }
    let obj = value.into_object().ok_or_else(|| {
        JsError::new_from_js_message("init", "MouseEventInit", "expected an object")
    })?;
    let related_target = obj.get::<_, Option<Value<'js>>>("relatedTarget")?;
    Ok((
        base,
        ui,
        view,
        related_target,
        MouseEventInit {
            screen_x: obj.get::<_, Option<f64>>("screenX")?.unwrap_or(0.0),
            screen_y: obj.get::<_, Option<f64>>("screenY")?.unwrap_or(0.0),
            client_x: obj.get::<_, Option<f64>>("clientX")?.unwrap_or(0.0),
            client_y: obj.get::<_, Option<f64>>("clientY")?.unwrap_or(0.0),
            button: obj.get::<_, Option<i16>>("button")?.unwrap_or(0),
            buttons: obj.get::<_, Option<u32>>("buttons")?.unwrap_or(0),
            ctrl_key: obj.get::<_, Option<bool>>("ctrlKey")?.unwrap_or(false),
            shift_key: obj.get::<_, Option<bool>>("shiftKey")?.unwrap_or(false),
            alt_key: obj.get::<_, Option<bool>>("altKey")?.unwrap_or(false),
            meta_key: obj.get::<_, Option<bool>>("metaKey")?.unwrap_or(false),
            movement_x: obj.get::<_, Option<f64>>("movementX")?.unwrap_or(0.0),
            movement_y: obj.get::<_, Option<f64>>("movementY")?.unwrap_or(0.0),
        },
    ))
}

const PROP_RELATED_TARGET: &str = "__relatedTarget";

/// `MouseEvent` — `click` / `mousedown` / `mouseup` / `mousemove`. Carries
/// `button` (0=left, 1=middle, 2=right), `buttons` bitmask, the four
/// modifier flags, and the spatial fields (`screenX/Y`, `clientX/Y`,
/// `movementX/Y`).
///
/// Per [UI Events §5.4](https://w3c.github.io/uievents/#interface-mouseevent).
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "MouseEvent")]
pub struct MouseEvent {
    #[qjs(skip_trace)]
    base: EventBase,
    #[qjs(skip_trace)]
    detail: i64,
    #[qjs(skip_trace)]
    me: MouseEventInit,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl MouseEvent {
    /// `new MouseEvent(type, init?)`. See [`MouseEventInit`] (private
    /// struct) for the accepted fields.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let (base, ui, _view, _related, me) = parse_mouse_event_init(&ctx, init.0)?;
        Ok(Self {
            base: EventBase::from_init(event_type, base),
            detail: ui.detail,
            me,
        })
    }

    /// `e.type`.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.base.event_type.clone()
    }
    /// `e.bubbles`.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.base.bubbles
    }
    /// `e.cancelable`.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.base.cancelable
    }
    /// `e.composed`.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.base.composed
    }
    /// `e.defaultPrevented`.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.base.state.default_prevented.get()
    }
    /// `e.eventPhase`.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.base.state.event_phase.get()
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
    /// `e.detail`.
    #[qjs(get)]
    fn detail(&self) -> i64 {
        self.detail
    }
    /// `e.view`.
    #[qjs(get)]
    fn view<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        view_property(this.0.clone().into_value())
    }
    /// `e.target`.
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }
    /// `e.currentTarget`.
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
    }
    /// `e.preventDefault()`.
    fn prevent_default(&self) {
        if self.base.cancelable {
            self.base.state.default_prevented.set(true);
        }
    }
    /// `e.stopPropagation()`.
    fn stop_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
    }
    /// `e.stopImmediatePropagation()`.
    fn stop_immediate_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
        self.base.state.immediate_propagation_stopped.set(true);
    }

    // ---- MouseEvent-specific ----
    /// `e.screenX`.
    #[qjs(get)]
    fn screen_x(&self) -> f64 {
        self.me.screen_x
    }
    /// `e.screenY`.
    #[qjs(get)]
    fn screen_y(&self) -> f64 {
        self.me.screen_y
    }
    /// `e.clientX`.
    #[qjs(get)]
    fn client_x(&self) -> f64 {
        self.me.client_x
    }
    /// `e.clientY`.
    #[qjs(get)]
    fn client_y(&self) -> f64 {
        self.me.client_y
    }
    /// `e.pageX` — for now reports the same value as `clientX` (no
    /// scroll offset tracking yet).
    #[qjs(get)]
    fn page_x(&self) -> f64 {
        self.me.client_x
    }
    /// `e.pageY` — see [`Self::page_x`].
    #[qjs(get)]
    fn page_y(&self) -> f64 {
        self.me.client_y
    }
    /// `e.offsetX` — for now reports the same value as `clientX`.
    #[qjs(get)]
    fn offset_x(&self) -> f64 {
        self.me.client_x
    }
    /// `e.offsetY` — see [`Self::offset_x`].
    #[qjs(get)]
    fn offset_y(&self) -> f64 {
        self.me.client_y
    }
    /// `e.x` — alias of `clientX`.
    #[qjs(get)]
    fn x(&self) -> f64 {
        self.me.client_x
    }
    /// `e.y` — alias of `clientY`.
    #[qjs(get)]
    fn y(&self) -> f64 {
        self.me.client_y
    }
    /// `e.button` — 0=left, 1=middle, 2=right.
    #[qjs(get)]
    fn button(&self) -> i16 {
        self.me.button
    }
    /// `e.buttons` — bitmask of currently held buttons.
    #[qjs(get)]
    fn buttons(&self) -> u32 {
        self.me.buttons
    }
    /// `e.ctrlKey`.
    #[qjs(get)]
    fn ctrl_key(&self) -> bool {
        self.me.ctrl_key
    }
    /// `e.shiftKey`.
    #[qjs(get)]
    fn shift_key(&self) -> bool {
        self.me.shift_key
    }
    /// `e.altKey`.
    #[qjs(get)]
    fn alt_key(&self) -> bool {
        self.me.alt_key
    }
    /// `e.metaKey`.
    #[qjs(get)]
    fn meta_key(&self) -> bool {
        self.me.meta_key
    }
    /// `e.movementX`.
    #[qjs(get)]
    fn movement_x(&self) -> f64 {
        self.me.movement_x
    }
    /// `e.movementY`.
    #[qjs(get)]
    fn movement_y(&self) -> f64 {
        self.me.movement_y
    }
    /// `e.relatedTarget` — stashed JS-side at construction.
    #[qjs(get)]
    fn related_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        related_target_property(this.0.clone().into_value())
    }
    /// `e.getModifierState(key)`. See [`KeyboardEvent::get_modifier_state`].
    fn get_modifier_state(&self, key: String) -> bool {
        match key.as_str() {
            "Control" => self.me.ctrl_key,
            "Shift" => self.me.shift_key,
            "Alt" => self.me.alt_key,
            "Meta" => self.me.meta_key,
            _ => false,
        }
    }
}

// ----- PointerEvent -------------------------------------------------

/// Init dictionary for [`PointerEvent`]. Per
/// [W3C Pointer Events §5.4](https://www.w3.org/TR/pointerevents/#dictdef-pointereventinit).
#[derive(Default, Clone)]
struct PointerEventInit {
    pointer_id: i32,
    width: f64,
    height: f64,
    pressure: f64,
    tangential_pressure: f64,
    tilt_x: i32,
    tilt_y: i32,
    twist: i32,
    pointer_type: String,
    is_primary: bool,
}

#[allow(clippy::type_complexity)]
fn parse_pointer_event_init<'js>(
    ctx: &Ctx<'js>,
    init: Option<Value<'js>>,
) -> rquickjs::Result<(
    EventInit,
    UIEventInit,
    Option<Value<'js>>,
    Option<Value<'js>>,
    MouseEventInit,
    PointerEventInit,
)> {
    let (base, ui, view, related, me) = parse_mouse_event_init(ctx, init.clone())?;
    let Some(value) = init else {
        return Ok((base, ui, view, related, me, PointerEventInit::default()));
    };
    if value.is_null() || value.is_undefined() {
        return Ok((base, ui, view, related, me, PointerEventInit::default()));
    }
    let obj = value.into_object().ok_or_else(|| {
        JsError::new_from_js_message("init", "PointerEventInit", "expected an object")
    })?;
    Ok((
        base,
        ui,
        view,
        related,
        me,
        PointerEventInit {
            pointer_id: obj.get::<_, Option<i32>>("pointerId")?.unwrap_or(0),
            width: obj.get::<_, Option<f64>>("width")?.unwrap_or(1.0),
            height: obj.get::<_, Option<f64>>("height")?.unwrap_or(1.0),
            pressure: obj.get::<_, Option<f64>>("pressure")?.unwrap_or(0.0),
            tangential_pressure: obj
                .get::<_, Option<f64>>("tangentialPressure")?
                .unwrap_or(0.0),
            tilt_x: obj.get::<_, Option<i32>>("tiltX")?.unwrap_or(0),
            tilt_y: obj.get::<_, Option<i32>>("tiltY")?.unwrap_or(0),
            twist: obj.get::<_, Option<i32>>("twist")?.unwrap_or(0),
            pointer_type: obj
                .get::<_, Option<String>>("pointerType")?
                .unwrap_or_default(),
            is_primary: obj.get::<_, Option<bool>>("isPrimary")?.unwrap_or(false),
        },
    ))
}

/// `PointerEvent` — generic pointing-device event (mouse / pen /
/// touch). Inherits everything from [`MouseEvent`] and adds the
/// pointer-shape fields.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "PointerEvent")]
pub struct PointerEvent {
    #[qjs(skip_trace)]
    base: EventBase,
    #[qjs(skip_trace)]
    detail: i64,
    #[qjs(skip_trace)]
    me: MouseEventInit,
    #[qjs(skip_trace)]
    pe: PointerEventInit,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl PointerEvent {
    /// `new PointerEvent(type, init?)`. See [`PointerEventInit`]
    /// (private struct) for the accepted fields.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let (base, ui, _view, _related, me, pe) = parse_pointer_event_init(&ctx, init.0)?;
        Ok(Self {
            base: EventBase::from_init(event_type, base),
            detail: ui.detail,
            me,
            pe,
        })
    }

    /// `e.type`.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.base.event_type.clone()
    }
    /// `e.bubbles`.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.base.bubbles
    }
    /// `e.cancelable`.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.base.cancelable
    }
    /// `e.composed`.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.base.composed
    }
    /// `e.defaultPrevented`.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.base.state.default_prevented.get()
    }
    /// `e.eventPhase`.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.base.state.event_phase.get()
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
    /// `e.detail`.
    #[qjs(get)]
    fn detail(&self) -> i64 {
        self.detail
    }
    /// `e.view`.
    #[qjs(get)]
    fn view<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        view_property(this.0.clone().into_value())
    }
    /// `e.target`.
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }
    /// `e.currentTarget`.
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
    }
    /// `e.preventDefault()`.
    fn prevent_default(&self) {
        if self.base.cancelable {
            self.base.state.default_prevented.set(true);
        }
    }
    /// `e.stopPropagation()`.
    fn stop_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
    }
    /// `e.stopImmediatePropagation()`.
    fn stop_immediate_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
        self.base.state.immediate_propagation_stopped.set(true);
    }

    // ---- MouseEvent inherited ----
    /// `e.screenX`.
    #[qjs(get)]
    fn screen_x(&self) -> f64 {
        self.me.screen_x
    }
    /// `e.screenY`.
    #[qjs(get)]
    fn screen_y(&self) -> f64 {
        self.me.screen_y
    }
    /// `e.clientX`.
    #[qjs(get)]
    fn client_x(&self) -> f64 {
        self.me.client_x
    }
    /// `e.clientY`.
    #[qjs(get)]
    fn client_y(&self) -> f64 {
        self.me.client_y
    }
    /// `e.button`.
    #[qjs(get)]
    fn button(&self) -> i16 {
        self.me.button
    }
    /// `e.buttons`.
    #[qjs(get)]
    fn buttons(&self) -> u32 {
        self.me.buttons
    }
    /// `e.ctrlKey`.
    #[qjs(get)]
    fn ctrl_key(&self) -> bool {
        self.me.ctrl_key
    }
    /// `e.shiftKey`.
    #[qjs(get)]
    fn shift_key(&self) -> bool {
        self.me.shift_key
    }
    /// `e.altKey`.
    #[qjs(get)]
    fn alt_key(&self) -> bool {
        self.me.alt_key
    }
    /// `e.metaKey`.
    #[qjs(get)]
    fn meta_key(&self) -> bool {
        self.me.meta_key
    }
    /// `e.movementX`.
    #[qjs(get)]
    fn movement_x(&self) -> f64 {
        self.me.movement_x
    }
    /// `e.movementY`.
    #[qjs(get)]
    fn movement_y(&self) -> f64 {
        self.me.movement_y
    }
    /// `e.relatedTarget`.
    #[qjs(get)]
    fn related_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        related_target_property(this.0.clone().into_value())
    }
    /// `e.getModifierState(key)`.
    fn get_modifier_state(&self, key: String) -> bool {
        match key.as_str() {
            "Control" => self.me.ctrl_key,
            "Shift" => self.me.shift_key,
            "Alt" => self.me.alt_key,
            "Meta" => self.me.meta_key,
            _ => false,
        }
    }

    // ---- PointerEvent-specific ----
    /// `e.pointerId`.
    #[qjs(get)]
    fn pointer_id(&self) -> i32 {
        self.pe.pointer_id
    }
    /// `e.width`.
    #[qjs(get)]
    fn width(&self) -> f64 {
        self.pe.width
    }
    /// `e.height`.
    #[qjs(get)]
    fn height(&self) -> f64 {
        self.pe.height
    }
    /// `e.pressure`.
    #[qjs(get)]
    fn pressure(&self) -> f64 {
        self.pe.pressure
    }
    /// `e.tangentialPressure`.
    #[qjs(get)]
    fn tangential_pressure(&self) -> f64 {
        self.pe.tangential_pressure
    }
    /// `e.tiltX`.
    #[qjs(get)]
    fn tilt_x(&self) -> i32 {
        self.pe.tilt_x
    }
    /// `e.tiltY`.
    #[qjs(get)]
    fn tilt_y(&self) -> i32 {
        self.pe.tilt_y
    }
    /// `e.twist`.
    #[qjs(get)]
    fn twist(&self) -> i32 {
        self.pe.twist
    }
    /// `e.pointerType` — `"mouse"` / `"pen"` / `"touch"`.
    #[qjs(get)]
    fn pointer_type(&self) -> String {
        self.pe.pointer_type.clone()
    }
    /// `e.isPrimary`.
    #[qjs(get)]
    fn is_primary(&self) -> bool {
        self.pe.is_primary
    }
}

// ----- WheelEvent ---------------------------------------------------

/// Init dictionary for [`WheelEvent`]. Per
/// [UI Events §5.5.5](https://w3c.github.io/uievents/#interface-wheeleventinit).
#[derive(Default, Clone)]
struct WheelEventInit {
    delta_x: f64,
    delta_y: f64,
    delta_z: f64,
    delta_mode: u32,
}

#[allow(clippy::type_complexity)]
fn parse_wheel_event_init<'js>(
    ctx: &Ctx<'js>,
    init: Option<Value<'js>>,
) -> rquickjs::Result<(
    EventInit,
    UIEventInit,
    Option<Value<'js>>,
    Option<Value<'js>>,
    MouseEventInit,
    WheelEventInit,
)> {
    let (base, ui, view, related, me) = parse_mouse_event_init(ctx, init.clone())?;
    let Some(value) = init else {
        return Ok((base, ui, view, related, me, WheelEventInit::default()));
    };
    if value.is_null() || value.is_undefined() {
        return Ok((base, ui, view, related, me, WheelEventInit::default()));
    }
    let obj = value.into_object().ok_or_else(|| {
        JsError::new_from_js_message("init", "WheelEventInit", "expected an object")
    })?;
    Ok((
        base,
        ui,
        view,
        related,
        me,
        WheelEventInit {
            delta_x: obj.get::<_, Option<f64>>("deltaX")?.unwrap_or(0.0),
            delta_y: obj.get::<_, Option<f64>>("deltaY")?.unwrap_or(0.0),
            delta_z: obj.get::<_, Option<f64>>("deltaZ")?.unwrap_or(0.0),
            delta_mode: obj.get::<_, Option<u32>>("deltaMode")?.unwrap_or(0),
        },
    ))
}

/// `WheelEvent` — scroll-wheel event. Inherits from [`MouseEvent`] and
/// adds the four `delta*` fields.
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "WheelEvent")]
pub struct WheelEvent {
    #[qjs(skip_trace)]
    base: EventBase,
    #[qjs(skip_trace)]
    detail: i64,
    #[qjs(skip_trace)]
    me: MouseEventInit,
    #[qjs(skip_trace)]
    we: WheelEventInit,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl WheelEvent {
    /// `new WheelEvent(type, init?)`. See [`WheelEventInit`] (private
    /// struct) for the accepted fields.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let (base, ui, _view, _related, me, we) = parse_wheel_event_init(&ctx, init.0)?;
        Ok(Self {
            base: EventBase::from_init(event_type, base),
            detail: ui.detail,
            me,
            we,
        })
    }

    /// `e.type`.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.base.event_type.clone()
    }
    /// `e.bubbles`.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.base.bubbles
    }
    /// `e.cancelable`.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.base.cancelable
    }
    /// `e.composed`.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.base.composed
    }
    /// `e.defaultPrevented`.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.base.state.default_prevented.get()
    }
    /// `e.eventPhase`.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.base.state.event_phase.get()
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
    /// `e.detail`.
    #[qjs(get)]
    fn detail(&self) -> i64 {
        self.detail
    }
    /// `e.view`.
    #[qjs(get)]
    fn view<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        view_property(this.0.clone().into_value())
    }
    /// `e.target`.
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }
    /// `e.currentTarget`.
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
    }
    /// `e.preventDefault()`.
    fn prevent_default(&self) {
        if self.base.cancelable {
            self.base.state.default_prevented.set(true);
        }
    }
    /// `e.stopPropagation()`.
    fn stop_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
    }
    /// `e.stopImmediatePropagation()`.
    fn stop_immediate_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
        self.base.state.immediate_propagation_stopped.set(true);
    }

    // ---- MouseEvent inherited ----
    /// `e.screenX`.
    #[qjs(get)]
    fn screen_x(&self) -> f64 {
        self.me.screen_x
    }
    /// `e.screenY`.
    #[qjs(get)]
    fn screen_y(&self) -> f64 {
        self.me.screen_y
    }
    /// `e.clientX`.
    #[qjs(get)]
    fn client_x(&self) -> f64 {
        self.me.client_x
    }
    /// `e.clientY`.
    #[qjs(get)]
    fn client_y(&self) -> f64 {
        self.me.client_y
    }
    /// `e.button`.
    #[qjs(get)]
    fn button(&self) -> i16 {
        self.me.button
    }
    /// `e.buttons`.
    #[qjs(get)]
    fn buttons(&self) -> u32 {
        self.me.buttons
    }
    /// `e.ctrlKey`.
    #[qjs(get)]
    fn ctrl_key(&self) -> bool {
        self.me.ctrl_key
    }
    /// `e.shiftKey`.
    #[qjs(get)]
    fn shift_key(&self) -> bool {
        self.me.shift_key
    }
    /// `e.altKey`.
    #[qjs(get)]
    fn alt_key(&self) -> bool {
        self.me.alt_key
    }
    /// `e.metaKey`.
    #[qjs(get)]
    fn meta_key(&self) -> bool {
        self.me.meta_key
    }
    /// `e.movementX`.
    #[qjs(get)]
    fn movement_x(&self) -> f64 {
        self.me.movement_x
    }
    /// `e.movementY`.
    #[qjs(get)]
    fn movement_y(&self) -> f64 {
        self.me.movement_y
    }
    /// `e.relatedTarget`.
    #[qjs(get)]
    fn related_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        related_target_property(this.0.clone().into_value())
    }

    // ---- WheelEvent-specific ----
    /// `e.deltaX`.
    #[qjs(get)]
    fn delta_x(&self) -> f64 {
        self.we.delta_x
    }
    /// `e.deltaY`.
    #[qjs(get)]
    fn delta_y(&self) -> f64 {
        self.we.delta_y
    }
    /// `e.deltaZ`.
    #[qjs(get)]
    fn delta_z(&self) -> f64 {
        self.we.delta_z
    }
    /// `e.deltaMode` — 0 = pixel, 1 = line, 2 = page.
    #[qjs(get)]
    fn delta_mode(&self) -> u32 {
        self.we.delta_mode
    }
}

// ----- FocusEvent ---------------------------------------------------

fn parse_focus_event_init<'js>(
    ctx: &Ctx<'js>,
    init: Option<Value<'js>>,
) -> rquickjs::Result<(EventInit, UIEventInit, Option<Value<'js>>, Option<Value<'js>>)> {
    let (base, ui, view) = parse_ui_event_init(ctx, init.clone())?;
    let Some(value) = init else {
        return Ok((base, ui, view, None));
    };
    if value.is_null() || value.is_undefined() {
        return Ok((base, ui, view, None));
    }
    let obj = value.into_object().ok_or_else(|| {
        JsError::new_from_js_message("init", "FocusEventInit", "expected an object")
    })?;
    let related_target = obj.get::<_, Option<Value<'js>>>("relatedTarget")?;
    Ok((base, ui, view, related_target))
}

/// `FocusEvent` — `focus` / `blur` / `focusin` / `focusout`. Carries
/// `relatedTarget` (the element gaining/losing focus on the other
/// side of the transition).
///
/// Per [UI Events §5.3](https://w3c.github.io/uievents/#interface-focusevent).
#[derive(Clone, Trace, JsLifetime)]
#[rquickjs::class(rename = "FocusEvent")]
pub struct FocusEvent {
    #[qjs(skip_trace)]
    base: EventBase,
    #[qjs(skip_trace)]
    detail: i64,
}

#[rquickjs::methods(rename_all = "camelCase")]
impl FocusEvent {
    /// `new FocusEvent(type, init?)`. `init` extends `UIEventInit`
    /// with `relatedTarget`.
    #[qjs(constructor)]
    pub fn new<'js>(
        ctx: Ctx<'js>,
        event_type: String,
        init: Opt<Value<'js>>,
    ) -> rquickjs::Result<Self> {
        let (base, ui, _view, _related) = parse_focus_event_init(&ctx, init.0)?;
        Ok(Self {
            base: EventBase::from_init(event_type, base),
            detail: ui.detail,
        })
    }

    /// `e.type`.
    #[qjs(get, rename = "type")]
    fn event_type(&self) -> String {
        self.base.event_type.clone()
    }
    /// `e.bubbles`.
    #[qjs(get)]
    fn bubbles(&self) -> bool {
        self.base.bubbles
    }
    /// `e.cancelable`.
    #[qjs(get)]
    fn cancelable(&self) -> bool {
        self.base.cancelable
    }
    /// `e.composed`.
    #[qjs(get)]
    fn composed(&self) -> bool {
        self.base.composed
    }
    /// `e.defaultPrevented`.
    #[qjs(get)]
    fn default_prevented(&self) -> bool {
        self.base.state.default_prevented.get()
    }
    /// `e.eventPhase`.
    #[qjs(get)]
    fn event_phase(&self) -> u32 {
        self.base.state.event_phase.get()
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
    /// `e.detail`.
    #[qjs(get)]
    fn detail(&self) -> i64 {
        self.detail
    }
    /// `e.view`.
    #[qjs(get)]
    fn view<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        view_property(this.0.clone().into_value())
    }
    /// `e.target`.
    #[qjs(get)]
    fn target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        target_property(this.0.clone().into_value())
    }
    /// `e.currentTarget`.
    #[qjs(get)]
    fn current_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        current_target_property(this.0.clone().into_value())
    }
    /// `e.preventDefault()`.
    fn prevent_default(&self) {
        if self.base.cancelable {
            self.base.state.default_prevented.set(true);
        }
    }
    /// `e.stopPropagation()`.
    fn stop_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
    }
    /// `e.stopImmediatePropagation()`.
    fn stop_immediate_propagation(&self) {
        self.base.state.propagation_stopped.set(true);
        self.base.state.immediate_propagation_stopped.set(true);
    }
    /// `e.relatedTarget` — element gaining/losing focus.
    #[qjs(get)]
    fn related_target<'js>(this: This<Class<'js, Self>>) -> rquickjs::Result<Value<'js>> {
        related_target_property(this.0.clone().into_value())
    }
}

/// Shared getter body for `event.view` on every UI-event subclass —
/// the JS object was stashed JS-side at construction by the
/// post-constructor wrapper installed in [`install_event_constructors`].
fn view_property<'js>(event_value: Value<'js>) -> rquickjs::Result<Value<'js>> {
    let obj = event_value.as_object().cloned().ok_or_else(|| {
        JsError::new_from_js_message("this", "UIEvent", "not an object")
    })?;
    match obj.get::<_, Option<Value<'js>>>(PROP_UI_VIEW)? {
        Some(v) => Ok(v),
        None => obj.ctx().clone().eval::<Value<'js>, _>("null"),
    }
}

/// Shared getter body for `event.relatedTarget` on `MouseEvent` /
/// `PointerEvent` / `WheelEvent` / `FocusEvent`.
fn related_target_property<'js>(event_value: Value<'js>) -> rquickjs::Result<Value<'js>> {
    let obj = event_value.as_object().cloned().ok_or_else(|| {
        JsError::new_from_js_message("this", "Event", "not an object")
    })?;
    match obj.get::<_, Option<Value<'js>>>(PROP_RELATED_TARGET)? {
        Some(v) => Ok(v),
        None => obj.ctx().clone().eval::<Value<'js>, _>("null"),
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

/// Read an [`EventView`] from a JS value that should be a Class<Event>,
/// Class<CustomEvent>, or any of the UI-event subclasses. Returns
/// `None` if none of those.
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
    if let Some(c) = obj.as_class::<UIEvent>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.base.state.clone(),
            event_type: ev.base.event_type.clone(),
            cancelable: ev.base.cancelable,
        });
    }
    if let Some(c) = obj.as_class::<KeyboardEvent>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.base.state.clone(),
            event_type: ev.base.event_type.clone(),
            cancelable: ev.base.cancelable,
        });
    }
    if let Some(c) = obj.as_class::<InputEvent>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.base.state.clone(),
            event_type: ev.base.event_type.clone(),
            cancelable: ev.base.cancelable,
        });
    }
    if let Some(c) = obj.as_class::<MouseEvent>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.base.state.clone(),
            event_type: ev.base.event_type.clone(),
            cancelable: ev.base.cancelable,
        });
    }
    if let Some(c) = obj.as_class::<PointerEvent>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.base.state.clone(),
            event_type: ev.base.event_type.clone(),
            cancelable: ev.base.cancelable,
        });
    }
    if let Some(c) = obj.as_class::<WheelEvent>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.base.state.clone(),
            event_type: ev.base.event_type.clone(),
            cancelable: ev.base.cancelable,
        });
    }
    if let Some(c) = obj.as_class::<FocusEvent>() {
        let ev = c.borrow();
        return Some(EventView {
            state: ev.base.state.clone(),
            event_type: ev.base.event_type.clone(),
            cancelable: ev.base.cancelable,
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
    add_listener_to_map(ctx, &map, event_type, callback, capture, once, passive)
}

/// [`add_listener_to_instance`] but against a caller-supplied map
/// object — used by the DOM `Element` surface which keys listener
/// maps by `NodeId` on the long-lived `document` global rather than
/// by JS-wrapper identity (see `dom.rs::element_listener_map`).
pub(crate) fn add_listener_to_map<'js>(
    ctx: &Ctx<'js>,
    map: &Object<'js>,
    event_type: &str,
    callback: &Function<'js>,
    capture: bool,
    once: bool,
    passive: bool,
) -> rquickjs::Result<()> {
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
    remove_listener_from_map(ctx, &map, event_type, callback, capture)
}

/// [`remove_listener_from_instance`] but against a caller-supplied
/// map object. See [`add_listener_to_map`].
pub(crate) fn remove_listener_from_map<'js>(
    ctx: &Ctx<'js>,
    map: &Object<'js>,
    event_type: &str,
    callback: &Function<'js>,
    capture: bool,
) -> rquickjs::Result<()> {
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
    let map: Option<Object<'js>> = instance.get::<_, Option<Object<'js>>>(PROP_LISTENERS)?;
    let target = instance.clone().into_value();
    dispatch_with_map(ctx, map.as_ref(), Some(target), event)
}

/// [`dispatch_on_instance`] but against a caller-supplied listener
/// map (which may be `None` if no listeners have been registered
/// yet). `target` is what `event.target` / `event.currentTarget`
/// will report during dispatch; pass `None` only when there is no
/// meaningful target (which is rare — virtually every real
/// dispatchEvent caller wants the JS-side target set so listener
/// code can read `e.target.value`, `e.target.id`, etc.). Removes
/// spent `once` listeners from the same map.
pub(crate) fn dispatch_with_map<'js>(
    ctx: &Ctx<'js>,
    map: Option<&Object<'js>>,
    target: Option<Value<'js>>,
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

    // Pin the JS-side target on the event so listeners can read
    // `e.target.value` / `e.target.id` / etc. Stored as a hidden
    // JS property to avoid the Rust-side Persistent footgun.
    if let Some(target) = target.as_ref() {
        if let Some(ev_obj) = event.as_object() {
            ev_obj.set(PROP_TARGET, target.clone())?;
            // Flat dispatch has no capture/bubble phase, so currentTarget is
            // always the target. Set it explicitly rather than relying on the
            // PROP_TARGET fallback — a re-dispatch of an event previously sent
            // through the tree path leaves PROP_CURRENT_TARGET = null, which
            // would otherwise make the fallback report a stale null here.
            ev_obj.set(PROP_CURRENT_TARGET, target.clone())?;
        }
    }

    // Snapshot listener list — JS-side. Iterating over the live
    // Array and concurrently mutating it is undefined per spec; we
    // dupe up front.
    let mut snapshot: Vec<(Function<'js>, bool, bool)> = Vec::new();
    if let Some(map) = map {
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

    let mut once_to_remove: Vec<(Function<'js>, bool)> = Vec::new();

    for (callback, capture, once) in &snapshot {
        if view.state.immediate_propagation_stopped.get() {
            break;
        }
        // Bind `this` to currentTarget per WHATWG DOM
        // <https://dom.spec.whatwg.org/#concept-event-listener-invoke>:
        // "Set listener's invocation target to event's currentTarget."
        // Frameworks (Preact in particular) read `this.l` from inside
        // their registered event proxy to look up the actual handler;
        // calling with no `this` makes that lookup return undefined
        // and the handler silently no-ops.
        let call_result: Result<Value<'js>, _> = match target.as_ref() {
            Some(t) => callback.call((
                rquickjs::function::This(t.clone()),
                event.clone(),
            )),
            None => callback.call((event.clone(),)),
        };
        if let Err(err) = call_result {
            report_listener_exception(ctx, err);
        }
        if *once {
            once_to_remove.push((callback.clone(), *capture));
        }
    }

    // Remove `once` listeners from the live list. Listener identity is
    // (callback, capture) per WHATWG, so remove only the exact capture
    // that fired — removing both phases would also unregister a separate,
    // still-live listener that happens to share the same function value.
    if !once_to_remove.is_empty() {
        if let Some(map) = map {
            for (cb, capture) in &once_to_remove {
                remove_listener_from_map(ctx, map, &view.event_type, cb, *capture)?;
            }
        }
    }

    let dp = view.state.default_prevented.get();
    view.state.dispatching.set(false);
    view.state.event_phase.set(EVENT_PHASE_NONE);

    Ok(!(view.cancelable && dp))
}

/// Tree-aware dispatch: walk a path of `(listener_map, currentTarget)`
/// pairs through the standard W3C capture → at-target → bubble phases.
///
/// `path[0]` is the root, `path[path.len()-1]` is the target. Each
/// entry is `(map, current_target_js)` where `map` is the node's
/// listener map (or `None` if it has none) and `current_target_js` is
/// the JS [`Element`](crate::dom::Element) wrapper that
/// `event.currentTarget` should report while listeners on that node
/// fire.
///
/// Semantics:
/// - **Capture phase** (`eventPhase = 1`): walk `path[0 .. len-1]` and
///   fire listeners registered with `capture: true`.
/// - **At target** (`eventPhase = 2`): fire **all** listeners on
///   `path[len-1]` in registration order, regardless of capture flag.
/// - **Bubble phase** (`eventPhase = 3`): only if `event.bubbles`.
///   Walk `path[len-2 ..= 0]` and fire listeners with `capture: false`.
/// - `stopPropagation()` halts movement to the next node (but lets the
///   current node's remaining listeners finish).
/// - `stopImmediatePropagation()` halts the current node's remaining
///   listeners AND further nodes.
/// - `event.target` is the at-target node throughout the walk.
/// - `once: true` listeners are auto-removed from their map after
///   firing.
///
/// Returns `false` iff the event is cancelable and a listener called
/// `preventDefault()`.
pub(crate) fn dispatch_with_node_path<'js>(
    ctx: &Ctx<'js>,
    path: &[(Option<Object<'js>>, Value<'js>)],
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
    if path.is_empty() {
        return Ok(true);
    }

    view.state.dispatching.set(true);
    view.state.propagation_stopped.set(false);
    view.state.immediate_propagation_stopped.set(false);
    view.state.default_prevented.set(false);

    // Pin `event.target` to the at-target node (last entry).
    let target_value = path[path.len() - 1].1.clone();
    let ev_obj = event.as_object().cloned();
    if let Some(ref ev_obj) = ev_obj {
        ev_obj.set(PROP_TARGET, target_value.clone())?;
    }

    // bubbles flag — read from the JS-side Event object, not the view,
    // because view doesn't carry it. Cheap: one JS getter call.
    let bubbles: bool = ev_obj
        .as_ref()
        .and_then(|o| o.get::<_, Option<bool>>("bubbles").ok().flatten())
        .unwrap_or(false);

    let last = path.len() - 1;

    // --- Capture phase: path[0 .. last], capture-only ---
    view.state.event_phase.set(EVENT_PHASE_CAPTURING);
    for (map, current_target) in path.iter().take(last) {
        if view.state.propagation_stopped.get() {
            break;
        }
        if let Some(ref ev_obj) = ev_obj {
            ev_obj.set(PROP_CURRENT_TARGET, current_target.clone())?;
        }
        fire_listeners_on_node(ctx, map.as_ref(), &view, &event, Some(true), Some(current_target))?;
    }

    // --- At target: path[last], all listeners ---
    if !view.state.propagation_stopped.get() {
        view.state.event_phase.set(EVENT_PHASE_AT_TARGET);
        let (map, current_target) = &path[last];
        if let Some(ref ev_obj) = ev_obj {
            ev_obj.set(PROP_CURRENT_TARGET, current_target.clone())?;
        }
        fire_listeners_on_node(ctx, map.as_ref(), &view, &event, None, Some(current_target))?;
    }

    // --- Bubble phase: path[last-1 ..= 0], non-capture only ---
    if bubbles && last > 0 {
        view.state.event_phase.set(EVENT_PHASE_BUBBLING);
        for i in (0..last).rev() {
            if view.state.propagation_stopped.get() {
                break;
            }
            let (map, current_target) = &path[i];
            if let Some(ref ev_obj) = ev_obj {
                ev_obj.set(PROP_CURRENT_TARGET, current_target.clone())?;
            }
            fire_listeners_on_node(
                ctx,
                map.as_ref(),
                &view,
                &event,
                Some(false),
                Some(current_target),
            )?;
        }
    }

    let dp = view.state.default_prevented.get();
    view.state.dispatching.set(false);
    view.state.event_phase.set(EVENT_PHASE_NONE);
    // Restore currentTarget to the at-target node so any post-dispatch
    // reads see the original target rather than whichever node was
    // visited last (matches the spec: currentTarget is null after
    // dispatch, but our flat-fallback semantics return target).
    // Per spec: after dispatch, `event.currentTarget` must be `null`.
    // We set it explicitly to JS null rather than removing, so the
    // getter returns the right thing without consulting dispatch
    // state.
    if let Some(ref ev_obj) = ev_obj {
        let null_val: Value<'js> = ctx.eval("null")?;
        ev_obj.set(PROP_CURRENT_TARGET, null_val)?;
    }

    Ok(!(view.cancelable && dp))
}

/// Fire listeners on a single node's listener map during a phase walk.
///
/// `phase_capture_filter`:
/// - `Some(true)`: only listeners registered with `capture: true`
///   (capture phase).
/// - `Some(false)`: only listeners registered with `capture: false`
///   (bubble phase).
/// - `None`: fire every listener (at-target phase).
///
/// Honors `stopImmediatePropagation`. Removes spent `once` listeners.
fn fire_listeners_on_node<'js>(
    ctx: &Ctx<'js>,
    map: Option<&Object<'js>>,
    view: &EventView,
    event: &Value<'js>,
    phase_capture_filter: Option<bool>,
    current_target: Option<&Value<'js>>,
) -> rquickjs::Result<()> {
    let Some(map) = map else { return Ok(()) };
    let list: Option<Array<'js>> = map.get::<_, Option<Array<'js>>>(view.event_type.as_str())?;
    let Some(list) = list else { return Ok(()) };

    // Snapshot up-front: mutating the live array mid-iteration is UB
    // per spec. Records are `(callback, capture, once)`.
    let mut snapshot: Vec<(Function<'js>, bool, bool)> = Vec::new();
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

    let mut once_to_remove: Vec<(Function<'js>, bool)> = Vec::new();
    for (callback, capture, once) in &snapshot {
        if view.state.immediate_propagation_stopped.get() {
            break;
        }
        if let Some(want) = phase_capture_filter {
            if *capture != want {
                continue;
            }
        }
        // Per WHATWG "report the exception": a throwing listener does
        // NOT halt subsequent listeners on this node, nor propagation.
        // We forward the exception's stringification to `console.error`
        // (best-effort; swallowed if console isn't installed) and
        // continue.
        // Bind `this` to currentTarget per WHATWG DOM
        // <https://dom.spec.whatwg.org/#concept-event-listener-invoke>.
        // Frameworks (Preact's E/H proxies in particular) read
        // `this.l` to look up the actual handler; without the bind,
        // `this` is undefined and the handler silently no-ops.
        let call_result: Result<Value<'js>, _> = match current_target {
            Some(t) => callback.call((
                rquickjs::function::This(t.clone()),
                event.clone(),
            )),
            None => callback.call((event.clone(),)),
        };
        if let Err(err) = call_result {
            report_listener_exception(ctx, err);
        }
        if *once {
            once_to_remove.push((callback.clone(), *capture));
        }
    }

    for (cb, cap) in &once_to_remove {
        remove_listener_from_map(ctx, map, &view.event_type, cb, *cap)?;
    }
    Ok(())
}

/// Shared getter body for `event.target` / `event.currentTarget` on
/// both [`Event`] and [`CustomEvent`]: reads the hidden JS-side
/// [`PROP_TARGET`] property set by [`dispatch_with_map`]. Returns JS
/// `null` if no dispatch has populated it (i.e. an event not yet
/// dispatched, or dispatched without a target).
fn target_property<'js>(event_value: Value<'js>) -> rquickjs::Result<Value<'js>> {
    let obj = event_value.as_object().cloned().ok_or_else(|| {
        JsError::new_from_js_message("this", "Event", "not an object")
    })?;
    match obj.get::<_, Option<Value<'js>>>(PROP_TARGET)? {
        Some(v) => Ok(v),
        None => obj.ctx().clone().eval::<Value<'js>, _>("null"),
    }
}

/// Shared getter for `event.currentTarget`. Prefers
/// [`PROP_CURRENT_TARGET`] (set per-node by the path-walking
/// dispatcher); falls back to [`PROP_TARGET`] so the flat-dispatch path
/// keeps its old semantics (currentTarget == target for non-DOM
/// EventTargets like `AbortSignal`).
fn current_target_property<'js>(event_value: Value<'js>) -> rquickjs::Result<Value<'js>> {
    let obj = event_value.as_object().cloned().ok_or_else(|| {
        JsError::new_from_js_message("this", "Event", "not an object")
    })?;
    if let Some(v) = obj.get::<_, Option<Value<'js>>>(PROP_CURRENT_TARGET)? {
        return Ok(v);
    }
    target_property(event_value)
}

/// Forward a listener exception to `console.error` per the WHATWG
/// "report the exception" step. Best-effort: swallows any failure of
/// the report path itself (e.g. if `console` is missing).
///
/// Reads `e.stack || String(e)` so both regular Error throws and
/// plain-value throws produce a useful line.
fn report_listener_exception<'js>(ctx: &Ctx<'js>, err: JsError) {
    // Pull the actual thrown value off the context (rquickjs captures
    // it as part of the Error::Exception variant on the catch side).
    // The simplest portable shape is to JSON-stringify via a tiny JS
    // helper; if that fails, fall back to the rust-side debug.
    let msg = format!("{err}");
    // If the error has a pending exception, prefer that string-form.
    let report: Function<'js> = match ctx.eval(
        r#"(m) => { try { if (globalThis.console && globalThis.console.error) {
            globalThis.console.error('Uncaught (in event listener): ' + m);
        } } catch (_) {} }"#,
    ) {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = report.call::<_, Value<'js>>((msg,));
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

/// Register the WHATWG UI Events subclasses (`UIEvent`,
/// `KeyboardEvent`, `InputEvent`, `MouseEvent`, `PointerEvent`,
/// `WheelEvent`, `FocusEvent`) on `globalThis` and rewire their
/// prototype chains to mirror the IDL inheritance hierarchy:
///
/// ```text
/// Event
///  ├─ UIEvent
///  │   ├─ FocusEvent
///  │   ├─ KeyboardEvent
///  │   ├─ InputEvent
///  │   └─ MouseEvent
///  │       ├─ PointerEvent
///  │       └─ WheelEvent
/// ```
///
/// After this runs, `new KeyboardEvent('keydown', {key: 'Enter'})
/// instanceof Event` returns `true`, which is what every framework's
/// `addEventListener('keydown', e => …)` handler tests for.
///
/// Also installs JS-side post-constructor wrappers that pin
/// non-primitive init fields (`view`, `relatedTarget`) as JS-hidden
/// properties on the freshly-constructed instance — the Rust
/// `#[qjs(constructor)]` can't see `this`, same constraint that
/// `CustomEvent`'s `detail` wrapper handles in [`install_events`].
///
/// Called by [`crate::engine::JsEngine::new`] after
/// [`install_events`].
pub fn install_event_constructors(context: &Context) -> Result<(), EvalError> {
    context
        .with(|ctx| -> rquickjs::Result<()> {
            let globals = ctx.globals();

            // ---- Register classes ----
            Class::<UIEvent>::define(&globals)?;
            Class::<KeyboardEvent>::define(&globals)?;
            Class::<InputEvent>::define(&globals)?;
            Class::<MouseEvent>::define(&globals)?;
            Class::<PointerEvent>::define(&globals)?;
            Class::<WheelEvent>::define(&globals)?;
            Class::<FocusEvent>::define(&globals)?;

            // ---- Rewire prototype chains ----
            //
            // The JS-side bootstrap reaches for each constructor by
            // name on globalThis (rquickjs sets them up there during
            // `Class::define`) and calls Object.setPrototypeOf to
            // splice the prototypes into the IDL chain. Same dance
            // `File extends Blob` uses in `web_apis.rs`.
            //
            // Also installs JS-side post-constructor wrappers that
            // pin `init.view` / `init.relatedTarget` as JS-hidden
            // properties on the freshly-constructed instance —
            // mirrors the CustomEvent.detail wrapper above.
            ctx.eval::<(), _>(EVENT_CONSTRUCTORS_BOOTSTRAP)?;

            // Install the typing-dispatch helper at engine setup so
            // both the host (via `JsEngine::set_input_value`) AND
            // user-level JS (any script that wants to script real
            // typing for testing) can call it.
            ctx.eval::<(), _>(TYPING_DISPATCH_HELPER_JS)?;

            Ok(())
        })
        .map_err(|e| EvalError::Engine(format!("install event constructors: {e}")))?;
    Ok(())
}

/// JS source defining `__hesoDispatchTyping(el, value)` — the
/// spec-correct typing pump used by [`crate::JsEngine::set_input_value`]
/// and [`crate::JsSession::fill`]. Dispatches the sequence:
///
/// 1. `focus` (FocusEvent, no bubble per spec)
/// 2. For each character of `value`:
///    - `keydown` (KeyboardEvent, key/code derived)
///    - `beforeinput` (InputEvent, `inputType: 'insertText'`,
///      `data: char`) — listeners may `preventDefault()` to abort.
///    - element `value` mutated
///    - `input` (InputEvent, same shape as beforeinput, non-cancelable
///      per UI Events §5.7.4)
///    - `keyup` (KeyboardEvent)
/// 3. `change` (Event, bubbles)
///
/// Mirrors the user-agent input pipeline real browsers run on a
/// keystroke (WHATWG HTML §4.10.5.5 "User interactions" +
/// UI Events §3.4 "Keyboard events").
///
/// Defined as a string constant rather than a JS file so it lives
/// inside the binary; embedded in [`crate::JsEngine::set_input_value`]
/// via a `format!`.
pub const TYPING_DISPATCH_HELPER_JS: &str = r#"
if (typeof globalThis.__hesoDispatchTyping !== 'function') {
    // ---- Key-shape mapper: char → {key, code, keyCode} -----------
    // Covers the common printable ASCII range + Enter / Tab / Backspace.
    // Outside that, `code` falls back to '' and `keyCode` to 0 — the
    // same degradation real browsers exhibit for Unicode chars that
    // don't map to a physical US-QWERTY scancode.
    function __hesoKeyInfo(ch) {
        const c = ch.charCodeAt(0);
        // Special characters by code:
        if (c === 13) return { key: 'Enter',     code: 'Enter',     keyCode: 13 };
        if (c === 9)  return { key: 'Tab',       code: 'Tab',       keyCode: 9 };
        if (c === 8)  return { key: 'Backspace', code: 'Backspace', keyCode: 8 };
        if (c === 27) return { key: 'Escape',    code: 'Escape',    keyCode: 27 };
        if (c === 32) return { key: ' ',         code: 'Space',     keyCode: 32 };
        // Letter A-Z / a-z → KeyA / KeyB ...
        if ((c >= 65 && c <= 90) || (c >= 97 && c <= 122)) {
            const upper = ch.toUpperCase();
            return { key: ch, code: 'Key' + upper, keyCode: upper.charCodeAt(0) };
        }
        // Digits 0-9 → Digit0 / ...
        if (c >= 48 && c <= 57) {
            return { key: ch, code: 'Digit' + ch, keyCode: c };
        }
        // Everything else (punctuation, Unicode): `key` is the char,
        // `code` is empty, `keyCode` is the code point.
        return { key: ch, code: '', keyCode: c };
    }

    globalThis.__hesoDispatchTyping = function (el, value) {
        // 1) focus
        el.dispatchEvent(new FocusEvent('focus', {
            bubbles: false, cancelable: false, composed: true,
        }));
        // (Also dispatch focusin which DOES bubble — frameworks
        // sometimes listen for it at the document level.)
        el.dispatchEvent(new FocusEvent('focusin', {
            bubbles: true, cancelable: false, composed: true,
        }));

        // 2) Per-character keydown / beforeinput / input / keyup.
        //    React-controlled inputs gate on the `input` event with
        //    real InputEvent.data — without it the controlled-input
        //    setState never fires.
        let current = '';
        for (const ch of value) {
            const info = __hesoKeyInfo(ch);
            const kd = new KeyboardEvent('keydown', {
                bubbles: true, cancelable: true, composed: true,
                key: info.key, code: info.code,
                keyCode: info.keyCode, which: info.keyCode, charCode: 0,
            });
            const kdAllowed = el.dispatchEvent(kd);
            if (!kdAllowed) {
                // Listener called preventDefault: skip the character
                // (browser behavior) but continue to keyup.
                el.dispatchEvent(new KeyboardEvent('keyup', {
                    bubbles: true, cancelable: true, composed: true,
                    key: info.key, code: info.code,
                    keyCode: info.keyCode, which: info.keyCode, charCode: 0,
                }));
                continue;
            }

            const bi = new InputEvent('beforeinput', {
                bubbles: true, cancelable: true, composed: true,
                data: ch, inputType: 'insertText',
            });
            const biAllowed = el.dispatchEvent(bi);
            if (biAllowed) {
                current += ch;
                el.value = current;
                el.dispatchEvent(new InputEvent('input', {
                    bubbles: true, cancelable: false, composed: true,
                    data: ch, inputType: 'insertText',
                }));
            }

            el.dispatchEvent(new KeyboardEvent('keyup', {
                bubbles: true, cancelable: true, composed: true,
                key: info.key, code: info.code,
                keyCode: info.keyCode, which: info.keyCode, charCode: 0,
            }));
        }

        // 3) Final value commit: ensure el.value reflects `value` even
        //    if a listener mutated it mid-typing, then fire `change`
        //    (the spec puts `change` on blur/commit, but every real
        //    page's onChange handler expects it after each fill — we
        //    fire it here for parity with the original behavior).
        if (el.value !== value) {
            try { el.value = value; } catch (_) {}
        }
        el.dispatchEvent(new Event('change', {
            bubbles: true, cancelable: false, composed: false,
        }));
    };
}
"#;

/// JS bootstrap source for [`install_event_constructors`]: rewires
/// the WHATWG UI-events prototype chain on the freshly-registered
/// constructors, and installs post-constructor wrappers that copy
/// `init.view` / `init.relatedTarget` onto the JS instance.
const EVENT_CONSTRUCTORS_BOOTSTRAP: &str = r#"
(function () {
    // ---- 1) Prototype chain ---------------------------------------
    // Order matters: walk top-down so each setPrototypeOf splices in
    // against a prototype that already inherits from Event.
    Object.setPrototypeOf(UIEvent.prototype, Event.prototype);
    Object.setPrototypeOf(FocusEvent.prototype, UIEvent.prototype);
    Object.setPrototypeOf(KeyboardEvent.prototype, UIEvent.prototype);
    Object.setPrototypeOf(InputEvent.prototype, UIEvent.prototype);
    Object.setPrototypeOf(MouseEvent.prototype, UIEvent.prototype);
    Object.setPrototypeOf(PointerEvent.prototype, MouseEvent.prototype);
    Object.setPrototypeOf(WheelEvent.prototype, MouseEvent.prototype);

    // ---- 2) DOM_KEY_LOCATION_* constants on KeyboardEvent ---------
    // Per UI Events §5.6.3.
    Object.defineProperty(KeyboardEvent, 'DOM_KEY_LOCATION_STANDARD',
        { value: 0, writable: false, enumerable: false, configurable: false });
    Object.defineProperty(KeyboardEvent, 'DOM_KEY_LOCATION_LEFT',
        { value: 1, writable: false, enumerable: false, configurable: false });
    Object.defineProperty(KeyboardEvent, 'DOM_KEY_LOCATION_RIGHT',
        { value: 2, writable: false, enumerable: false, configurable: false });
    Object.defineProperty(KeyboardEvent, 'DOM_KEY_LOCATION_NUMPAD',
        { value: 3, writable: false, enumerable: false, configurable: false });
    // And on the prototype, since the spec defines them in both
    // places.
    for (const k of ['DOM_KEY_LOCATION_STANDARD', 'DOM_KEY_LOCATION_LEFT',
                     'DOM_KEY_LOCATION_RIGHT', 'DOM_KEY_LOCATION_NUMPAD']) {
        Object.defineProperty(KeyboardEvent.prototype, k,
            { value: KeyboardEvent[k], writable: false, enumerable: false, configurable: false });
    }

    // ---- 3) Post-constructor wrappers -----------------------------
    //
    // Pin non-primitive init fields on the freshly-built instance so
    // the Rust-side getter can read them back. Mirrors the
    // CustomEvent.detail wrapper. Each shim:
    //   1. constructs via Reflect.construct on the underlying class
    //      (so `instanceof OurClass` still works correctly),
    //   2. copies the named init fields onto the instance via
    //      Object.defineProperty (non-enumerable, non-writable).
    function wrap(name, fields) {
        const Orig = globalThis[name];
        if (typeof Orig !== 'function') return;
        function wrapped(type, init) {
            // Mirror the spec: missing `type` throws TypeError.
            // QuickJS's class constructor will surface that via its
            // own argument-count check, so we just forward.
            const inst = Reflect.construct(Orig, [type, init], wrapped);
            if (init && typeof init === 'object') {
                for (const [name, prop] of fields) {
                    if (name in init && init[name] != null) {
                        Object.defineProperty(inst, prop, {
                            value: init[name],
                            writable: false,
                            enumerable: false,
                            configurable: false,
                        });
                    }
                }
            }
            return inst;
        }
        wrapped.prototype = Orig.prototype;
        Object.defineProperty(wrapped, 'name', { value: name, configurable: true });
        globalThis[name] = wrapped;
    }
    wrap('UIEvent',       [['view', '__uiView']]);
    wrap('KeyboardEvent', [['view', '__uiView']]);
    wrap('InputEvent',    [['view', '__uiView']]);
    wrap('MouseEvent',    [['view', '__uiView'], ['relatedTarget', '__relatedTarget']]);
    wrap('PointerEvent',  [['view', '__uiView'], ['relatedTarget', '__relatedTarget']]);
    wrap('WheelEvent',    [['view', '__uiView'], ['relatedTarget', '__relatedTarget']]);
    wrap('FocusEvent',    [['view', '__uiView'], ['relatedTarget', '__relatedTarget']]);
})();
"#;

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

    /// Bug 1 regression: a throwing listener used to leave
    /// `EventState.dispatching = true`, poisoning the event-object
    /// for any future dispatch ("already being dispatched"). The fix
    /// catches listener exceptions per WHATWG "report the exception"
    /// and continues — so a subsequent dispatch on the same target
    /// works.
    #[test]
    fn throwing_listener_does_not_poison_subsequent_dispatch() {
        let e = engine();
        let out = e
            .eval(
                r#"
                const t = new EventTarget();
                let after = 0;
                t.addEventListener('go', () => { throw new Error('x'); });
                t.addEventListener('go', () => { after++; });
                // First dispatch: the throwing listener is reported,
                // the second listener still runs.
                t.dispatchEvent(new Event('go'));
                // Second dispatch on the SAME target / same event-type
                // must not throw "already being dispatched".
                const ev2 = new Event('go');
                t.dispatchEvent(ev2);
                [after, ev2.eventPhase]
                "#,
            )
            .expect("eval ok");
        assert_eq!(out.value[0], 2);
        // currentTarget should be null after dispatch; eventPhase 0.
        assert_eq!(out.value[1], 0);
    }
}
