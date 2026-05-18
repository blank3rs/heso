//! # heso-engine-js
//!
//! The JavaScript path of heso — the headless browser for the
//! agent-relevant half of the web. Sibling of
//! [`heso-engine-fetch`](../heso_engine_fetch/index.html) (the static path,
//! ADR 0012); together they cover the in-scope half from
//! [ADR 0016](../../decisions/0016-positioning-headless-browser-for-agents.md):
//! fetch, parse, JS, DOM (Phase 1B), forms, clicks, sessions.
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
//! **Phase 1B** (the agent-shaped DOM) is under way — `Document` and
//! `Element` types backed by `dom_query::Document` (a mutable
//! `html5ever`-backed tree), exposing both the read half
//! (querySelector / textContent / getAttribute / ...) and the
//! mutation surface real pages reach for during hydration
//! (setAttribute / innerHTML setter / appendChild / classList).
//! Phase 1C runs `<script>` tags on load so SPA hydration actually
//! happens. Phase 1D fills out window globals.
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
//! QuickJS itself is deterministic. The remaining sources of
//! nondeterminism — `setTimeout`, `Date.now`, `Math.random`,
//! `crypto.getRandomValues`, `performance.now`, `fetch` — are not
//! installed in Phase 1A; Phase 2 will replace them with fake-clock /
//! seeded-PRNG / recorded-network shims so the determinism
//! guarantees from ADR 0008 carry over.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod dom;
pub mod engine;
pub mod events;
pub mod timers;

pub use dom::{Document, DomTokenList, Element};
pub use engine::{ConsoleEntry, ConsoleLevel, EvalError, EvalOutcome, JsEngine};
pub use events::{AbortController, AbortSignal, CustomEvent, DOMException, Event, EventTarget};
pub use timers::VirtualClock;
