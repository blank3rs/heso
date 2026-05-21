//! # heso-engine-js
//!
//! The JavaScript path of heso — the agent-native web engine. No Chromium.
//! No Node. One Rust binary. Sibling of
//! [`heso-engine-fetch`](../heso_engine_fetch/index.html) (the static path,
//! ADR 0012); together they cover the in-scope half from
//! [ADR 0016](../../decisions/0016-positioning-headless-browser-for-agents.md):
//! fetch, parse, JS, DOM, forms, clicks, sessions.
//!
//! Per [ADR 0014](../../decisions/0014-bundled-quickjs-agent-dom.md), the
//! JS engine is QuickJS via [`rquickjs`](https://crates.io/crates/rquickjs)
//! — ~600 KB, single-binary-friendly, MIT-licensed. The bet is that an
//! agent-shaped DOM (the parts real pages actually call) on top of QuickJS
//! is enough for the agent half of the web, without the V8 + Blink +
//! Skia + compositor weight of headless Chromium.
//!
//! ## Status
//!
//! **Phase 1A** (the language) landed: evaluate a string of JavaScript
//! inside a sandboxed QuickJS context and return the result as a
//! [`serde_json::Value`] alongside any captured `console.*` output.
//! **Phase 1B** (the agent-shaped DOM) is done — `Document` and
//! `Element` types backed by `dom_query::Document` (a mutable
//! `html5ever`-backed tree), exposing both the read half
//! (querySelector / textContent / getAttribute / ...) and the
//! mutation surface real pages reach for during hydration
//! (setAttribute / innerHTML setter / appendChild / classList);
//! the [`events`] module (`addEventListener` / `dispatchEvent` /
//! `CustomEvent` / `AbortController` / `DOMException`); the [`timers`]
//! module (`setTimeout` / `setInterval` over a [`VirtualClock`]); the
//! [`rng`] module (`Math.random` / `crypto.getRandomValues` /
//! `crypto.randomUUID` over a ChaCha20 [`SeededRng`]); and the
//! [`JsEngine::dispatch_click`] / [`JsEngine::set_input_value`] /
//! [`JsEngine::submit_form`] bridges that let `heso click` /
//! `heso fill` / `heso submit` fire real events through the DOM.
//! **Phase 1C** is next: run `<script>` tags on page load so SPA
//! hydration actually happens, route `Date.now` / `new Date()`
//! through [`VirtualClock`] to close the last nondeterminism source,
//! and ship `fetch()` inside JS (proxied through `reqwest`). Phase 1D
//! fills out the remaining window globals.
//!
//! ## Why QuickJS
//!
//! See ADR 0014 for the full alternatives-considered. Short version:
//! V8 is too big for the 30 MB binary budget in ADR 0016; WebView ties us
//! to per-OS deployments; Boa is incomplete; CDP-to-Chromium is exactly
//! the dep we're replacing. QuickJS is small, audited, and the
//! agent-shaped DOM on top of it is the obvious move nobody has shipped
//! as a product. The precedent is jsdom + happy-dom (50k + 30k LOC of
//! JavaScript proving a minimal DOM handles the agent half of the web) —
//! we're doing it in Rust against a real JS engine instead of JS-in-JS.
//!
//! ## Determinism
//!
//! QuickJS itself is deterministic. The other JS sources of
//! nondeterminism are mostly closed:
//!
//! - `setTimeout` / `setInterval` route through [`VirtualClock`] — fired
//!   in `(scheduled_time, sequence)` order via [`JsEngine::tick`], no
//!   wall clock involved.
//! - `Math.random`, `crypto.getRandomValues`, and `crypto.randomUUID`
//!   route through [`SeededRng`] (ChaCha20). Construct the engine with
//!   [`JsEngine::new_with_seed`] (the CLI exposes this as `--seed N` on
//!   `heso eval-js` and `heso eval-dom`); same seed, byte-identical
//!   output across runs and machines.
//!
//! - `Date.now()` and zero-arg `new Date()` route through the same
//!   [`VirtualClock`] — `Date.now()` reads `clock.now_ms()` as an
//!   `f64`, and `new Date()` (the zero-arg construction form, where
//!   the spec reads the host clock) is monkey-patched to
//!   `new Date(Date.now())`. Explicit-input forms
//!   (`new Date(ms)`, `new Date(str)`, `new Date(y, m, d, ...)`,
//!   `Date.parse`, `Date.UTC`) are pure functions of their inputs
//!   and stay on the QuickJS built-in. A fresh engine starts at
//!   virtual epoch `0` (= midnight 1970-01-01 UTC); the host advances
//!   it via [`JsEngine::advance_clock`], the same control surface as
//!   timers.
//!
//! `fetch()` / `XMLHttpRequest` are not fully deterministic yet — `fetch`
//! is installed but currently lives in `DeterministicNoCassette` mode
//! under `--seed N` until record/replay (ADR 0008 item M) lands and the
//! recorded-network shim makes JS-issued HTTP calls reproducible too.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cookies;
pub(crate) mod custom_elements;
pub mod dom;
pub mod engine;
pub mod events;
pub mod fetch;
pub(crate) mod form_submit;
pub mod history;
pub mod import_map;
pub mod intersection_observer;
pub mod modules;
pub(crate) mod mutation_observer;
pub mod rng;
pub mod scripts;
pub mod session;
pub mod timers;
pub mod url_search_params;
pub mod wait_for;
pub mod web_apis;
pub mod xhr;

pub use dom::{Document, DomTokenList, Element, ShadowRoot};
pub use engine::{ConsoleEntry, ConsoleLevel, EvalError, EvalOutcome, JsEngine};
pub use events::{
    AbortController, AbortSignal, CustomEvent, DOMException, Event, EventTarget, FocusEvent,
    InputEvent, KeyboardEvent, MouseEvent, PointerEvent, UIEvent, WheelEvent,
};
pub use fetch::FetchMode;
pub use import_map::{parse_import_map, ImportMap, ImportMapError};
pub use modules::{HttpFetcher, HttpLoader, HttpResolver, ModuleCache, SharedImportMap};
pub use rng::SeededRng;
pub use scripts::{ScriptFailure, ScriptFetchPolicy, ScriptOutcome};
pub use session::JsSession;
pub use timers::VirtualClock;
pub use url_search_params::{UrlClass, UrlSearchParamsClass};
pub use wait_for::{wait_for_on_engine, WaitCondition, WaitOutcome};
pub use web_apis::{Blob, File, FormData, Headers};
