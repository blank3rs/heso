## 0014. Bundled QuickJS + agent-shaped DOM

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay
- **Relates to:** [ADR 0012 — fetch-only native engine](0012-fetch-only-native-engine.md) (this ADR *extends* the engine; v1 fetch-only path remains valid), [ADR 0013 — engine as semantic extractor](0013-engine-as-semantic-extractor.md) (the action graph this ADR will animate), [ADR 0010 — primitives as terminal commands](0010-primitives-as-terminal-commands.md), [ADR 0008 — deterministic execution](0008-deterministic-execution.md)

## Context

heso's positioning crystallized in conversation: heso is not a competitor to Firecrawl / Jina Reader / Playwright. It's the **browser engine those tools drive instead of headless Chromium**. Chromium is 180MB+ of human-browser concerns (layout pipeline, GPU compositing, audio, codec stack, animations, accessibility tree built for screen readers). For an LLM agent, ~99% of that is pure waste. heso aims to be **the agent-shaped browser engine**: a single tiny Rust binary that exposes the things agents actually use (parse, navigate, query, click, fill, evaluate handlers) and nothing they don't.

The blocker today is JavaScript. ADR 0012 explicitly punted on JS for v1 because the engine choice is the load-bearing decision: pick wrong and the whole repositioning fails. With the protocol surface ([`heso serve`](../crates/heso-cli/src/serve.rs)) and the action graph ([`crates/heso-engine-fetch/src/actions.rs`](../crates/heso-engine-fetch/src/actions.rs)) now real, the next move has to enable JavaScript — *but* in a way consistent with the "drop-in Chromium replacement, smaller and agent-shaped" pitch. That rules out a few of the candidates:

- **Bundled WebView** (wry/Tauri runtime) — uses the OS's native webview. Gets JS + DOM "free." But ties the binary to per-OS deployments (different DLLs/frameworks on Windows/Mac/Linux), breaks the "single binary, deploys anywhere" thesis. Rejected.
- **rusty_v8 / Deno's V8 bindings** — fastest JS, real DOM-host story possible via Deno's web runtime. But ~35MB+ binary, C++ build deps, complicates the "tiny single binary" pitch by an order of magnitude. Rejected for v1; revisitable if QuickJS proves perf-blocking.
- **Boa** (pure-Rust JS) — Rust-native, zero C deps, ergonomic. But incomplete (missing many web platform APIs), known correctness gaps on real-world JS, slow compared to QuickJS. Rejected.
- **Drive system Chromium via CDP** — we explicitly rejected this in ADR 0011/0012. Re-rejecting here. The Chromium dep is exactly the problem we're solving.
- **QuickJS** (Fabrice Bellard's, via [`rquickjs`](https://crates.io/crates/rquickjs)) — ~600KB engine. C dep but it's a small, audited, MIT C library that the [`rquickjs`](https://crates.io/crates/rquickjs) crate already vendors as a build dep with safe Rust bindings. No DOM out of the box — we build that ourselves, which is the cost. But every DOM API we implement is one we control: agent-shaped, deterministic, no rendering pipeline.

QuickJS is the choice. The cost is the DOM-build-out — months of work, scoped explicitly below.

## Decision

**Bundle [`rquickjs`](https://crates.io/crates/rquickjs) (QuickJS) as heso's JavaScript engine, and implement a deliberately minimal, agent-shaped DOM** in Rust on top of the existing [`scraper`](https://crates.io/crates/scraper) parse. The binary grows from ~5.2MB to ~6MB. JS handlers can execute. The full state-graph vision (every reachable page state pre-computed as a sub-directory) becomes possible.

### Architecture

A new crate `heso-engine-js` (sibling of `heso-engine-fetch`) implements the JS-capable path. It depends on `heso-engine-fetch` for the parse + tree + metadata + action graph, and adds:

1. **DOM types as `#[rquickjs::class]` Rust structs.** Backed by `scraper::Html` (already in memory after the parse). Types: `Document`, `Element`, `Node`, `NodeList`, `Text`, `Attr`, plus the HTMLElement subclasses we care about (`HTMLAnchorElement`, `HTMLButtonElement`, `HTMLInputElement`, `HTMLFormElement`, `HTMLTextAreaElement`, `HTMLSelectElement`).
2. **DOM API methods on those classes.** `getAttribute`, `setAttribute`, `removeAttribute`, `querySelector`, `querySelectorAll`, `getElementById`, `getElementsByTagName`, `addEventListener`, `removeEventListener`, `dispatchEvent`, `click()`, `focus()`, `blur()`. Most are read-only over `scraper::Html`; mutations land in a side-table of overlays so the original parse stays immutable.
3. **Window globals.** `window`, `document`, `location`, `navigator` (stub), `history` (stub).
4. **Event model.** `Event`, `CustomEvent`, `MouseEvent`, `KeyboardEvent`. Bubbling, capturing, `preventDefault`. The minimum a real handler needs.
5. **Standard library subset.** `fetch` (points at our existing `reqwest::Client`), `setTimeout` / `setInterval` / `clearTimeout` / `clearInterval` (backed by a **deterministic fake clock** per ADR 0008 — virtual time, not wall time), `JSON`, `console` (writes to a per-run trace, not stderr — so agent receipts are clean), `URL`, `URLSearchParams`.
6. **No-op or absent APIs.** Listed in *Non-goals* below.

### How a handler runs

```
heso open URL                  (existing, no JS — pure static)
  → fetch HTML
  → parse Html (scraper)
  → build tree + metadata + action graph
  → return FetchPage

heso open --js URL             (new, with JS)
  → fetch HTML
  → parse Html (scraper)
  → build tree + metadata + action graph
  → spin up rquickjs Runtime + Context
  → install Document / Element / window / fetch / setTimeout etc. globals
  → execute every <script> tag in document order (mutations recorded)
  → return FetchPage (now reflects post-load JS state)

heso click <page_id> @e7       (new, JS optional)
  → look up @e7 in cached action graph
  → if the page was opened with --js: dispatch a real MouseEvent through
    rquickjs to the element; let handlers run; capture DOM mutations;
    re-extract tree + actions for the new state
  → if not: for <a href>, fetch the href as a new page; for buttons
    without a JS path, return "this element needs --js"
```

### Determinism is preserved

QuickJS itself is fully deterministic. Per ADR 0008 we replace any source of nondeterminism:

| API | Replacement |
|-----|-------------|
| `setTimeout` / `setInterval` / `requestAnimationFrame` | Fake clock — virtual time stepped explicitly between operations |
| `Date.now()` / `new Date()` | Seeded clock (start time configurable; trace records the seed) |
| `Math.random()` | Seeded PRNG (Xoshiro256**, seed from `Session.seed`) |
| `crypto.getRandomValues` | Same seeded PRNG, type-correct fill |
| `crypto.randomUUID` | Deterministic UUIDs derived from PRNG |
| `performance.now()` | Fake clock (virtual-time delta from session start) |
| `fetch` | Our `reqwest::Client`; network recording/replay per ADR 0008 |

Same seed + same recorded network = byte-identical receipt, including any DOM mutations the page's JS performed. This is what no other JS-capable browser can offer.

### Scope: what we WILL implement

Phase 1 (the JS-capable v1):
- Core DOM (Document / Element / Node / Text / Attr / NodeList)
- Element queries: `getElementById`, `getElementsByTagName`, `getElementsByClassName`, `querySelector`, `querySelectorAll`
- Element manipulation: `getAttribute`, `setAttribute`, `removeAttribute`, `textContent`, `innerHTML` (read only initially), `appendChild`, `removeChild`, `replaceChild`, `insertBefore`
- Class manipulation: `classList.add/remove/toggle/contains`
- Style read: `element.style.foo` (we *read* declared inline styles; we don't compute anything, since there's no layout)
- Event model: `addEventListener`, `removeEventListener`, `dispatchEvent`, `preventDefault`, `stopPropagation`, capturing + bubbling phases. Event types: `Event`, `CustomEvent`, `MouseEvent` (click, mousedown, mouseup, mouseenter, mouseleave), `KeyboardEvent` (keydown, keyup, keypress), `InputEvent`, `SubmitEvent`, `FocusEvent`, `LoadEvent`, `DOMContentLoaded`, `popstate`.
- HTMLElement methods: `click()`, `focus()`, `blur()`. `HTMLAnchorElement.click()` triggers navigation if no handler `preventDefault`'d.
- HTMLFormElement: `submit()` (serializes form fields and POSTs via our `reqwest::Client`; respects `action`/`method`). `reset()`.
- HTMLInputElement: `value` get/set, `checked` get/set, `disabled` get/set. `select()`.
- Window globals: `window`, `document`, `location` (read + assign triggers navigation), `history.pushState`/`history.back`/`history.forward` (in-memory stack), `navigator.userAgent`, `navigator.language`.
- Standard library: `JSON`, `URL`, `URLSearchParams`, `Promise` (QuickJS native), `async`/`await` (QuickJS native), `Map`, `Set`, `Array.from`, all the JS-engine-side standard library QuickJS gives us for free.
- Async: `fetch` (full Request/Response/Headers types backed by our `reqwest::Client`). `Promise`-based, awaitable.
- Timers: `setTimeout`/`setInterval`/`clearTimeout`/`clearInterval`, deterministic fake-clock backed.

This is enough to run: vanilla onclick handlers, jQuery, most React/Vue runtime mounts that don't reach for canvas/WebGL/workers, typical AJAX, typical form-submit-with-validation flows.

### Scope: what we WILL NOT implement (non-goals)

The non-goals are the price of staying tiny and agent-shaped. heso is **not** trying to be Chromium — only what an LLM agent actually uses on real read+click+submit flows.

- **No layout pipeline.** `getBoundingClientRect()`, `offsetWidth`, `clientHeight`, `scrollTop`, `scrollHeight` either return zero, throw, or return synthetic values. No browser-shaped layout. Sites depending on layout-derived JS branches will misbehave; we accept it.
- **No paint / no canvas / no WebGL.** `<canvas>` is a black box. WebGL context construction returns `null`. Sites that gate functionality behind canvas (recaptcha, fingerprinting, games) will not work.
- **No CSS engine.** `getComputedStyle` returns the declared inline style only. No selector matching for computed values, no cascade. Sites that read computed colors / dimensions for branching break here.
- **No Web Workers, Service Workers, SharedWorker.** Single thread. No background JS.
- **No IndexedDB, no WebSQL.** localStorage / sessionStorage **are** implemented (in-memory, deterministic, exposed under `/env/storage/*` per ADR 0010).
- **No WebRTC, no WebSockets** (initially; the latter could land in phase 2 since `reqwest` supports tungstenite). No media APIs (`<video>`, `<audio>` are inert).
- **No mutation observers / IntersectionObserver / ResizeObserver.** Layout-dependent. Sites' lazy-load tricks won't fire — the agent gets the initial state.
- **No clipboard API, no geolocation, no notifications, no device sensors.** Stubs that throw or return null.
- **No CSS transitions / animations.** No matter how long you `setTimeout`, transition events don't fire.
- **No accessibility tree.** We have our **own** semantic representation (action graph from ADR 0013) which is better for agents than the AOM heuristic anyway.
- **No print API, no PDF rendering.** Obviously.
- **No browser extensions, no WebAssembly streaming compilation** (Wasm compile-time via `WebAssembly.instantiate` from bytes is doable later; not v1).

The honest framing for the README: *heso runs the JavaScript that handles clicks, fetches, and computes state. It does not run the JavaScript that paints pixels or animates them. For sites whose business logic depends on visual layout to expose data, an agent still needs a real browser; for everything else, heso is the cheaper and more deterministic path.*

### CLI surface additions

- `heso open --js <url>` — open a page WITH JS execution (default remains static).
- `heso click <page_id> <ref>` — dispatch a click on an element. Returns `{ tree, actions, mutations }` for the new state.
- `heso fill <page_id> <ref> <value>` — set an input/textarea value and dispatch an `input` event.
- `heso submit <page_id> <form_ref>` — submit a form (with or without JS depending on whether the page has a handler).
- `heso eval <page_id> <js>` — evaluate a JS expression in the page context (escape hatch; not the planned path, but useful for debugging + agent extension).

All of these are reachable through `heso serve` as JSON-RPC methods too.

### Crate layout

```
crates/
  heso-engine-fetch     ← unchanged (static path)
  heso-engine-js        ← NEW: rquickjs + DOM + event model + fake clock
  heso-cli              ← gains --js flag, new subcommands
```

`heso-engine-js` depends on `heso-engine-fetch` (for the action graph + parse) and on `rquickjs` (with the `loader`, `async`, `chrono` feature flags). The `EngineApi` trait (ADR 0002) gets one extension method `open_with_js(url, options)`; default impl returns `Error::NotSupported`, `heso-engine-js` overrides.

## Alternatives considered

Already covered above: WebView (per-OS deps), V8 (binary size), Boa (correctness/perf), system Chromium via CDP (re-rejected). One more worth noting:

- **Skip the JS engine and bet on "engine for static pages + planner intelligence."** Argument: most read-only agent tasks don't need JS; the static + action-graph + protocol surface heso already has is enough. *Why we rejected:* the modern web is increasingly SPA-first. We'd be locking ourselves out of every React/Next.js/Vue marketing site, every dashboard, every SaaS app. The pitch is "agents drive heso instead of Chromium" — without JS that pitch is "agents drive heso for the 30% of sites that still server-render."

## Consequences

**Positive:**
- Real JS execution unblocks the "drive instead of Chromium" pitch. Browser Use / Stagehand could plausibly target heso once Phase 1 ships.
- Every DOM API is one we control — agent-shaped, deterministic, no rendering pipeline. *That* is the moat: Chrome can't be made deterministic; we are deterministic by construction.
- Determinism per ADR 0008 stays free. QuickJS + fake clock + seeded RNG + recorded network = byte-identical replay including post-load JS state. Nobody else offers this for JS-driven pages.
- Binary stays tiny — ~6MB vs Chromium's 180MB+. The "single binary, deploys anywhere" pitch holds.
- The action graph (ADR 0013) becomes far more powerful: `click @e7` actually does something. The protocol surface (`heso serve`) gains the `click` / `fill` / `submit` methods.
- The state-graph vision (every reachable page state pre-computed as a sub-directory) becomes possible. For each interactive element with a handler, run the handler in a sandbox snapshot, capture mutations, materialize as `/path/-onclick/`. Months of work but the right north star.

**Negative:**
- The DOM build-out is **months of work**, possibly years to reach parity with what real sites depend on. We're explicitly trading time for control. Recovery path if this is too expensive: fall back to bundled WebView (wry) for a hosted v2 with the same shaped API. The DOM Rust types we build aren't wasted in that scenario — they become the agent-shaped layer on top.
- We will fail on some real sites. The non-goals list above is long. Every failed site is a customer experience loss, mitigated only by being honest about the scope (the README block in ADR 0013's spirit must call this out explicitly).
- New C dep (QuickJS) — small (~150KB compiled), audited, MIT, but a C dep nonetheless. `cargo build` still works on any platform with a C compiler. `rquickjs` vendors and builds QuickJS as part of the crate; we don't need a system QuickJS.
- New crate to maintain (`heso-engine-js`). Engine count goes from 1 to 2 — the swappability promised in ADR 0002 was supposed to be "we keep one engine and the trait lets us swap"; we now have two engines that share the same trait. Acceptable; the static-only path stays useful for read-only crawling and the JS path is opt-in via `--js`.
- Re-opens the ADR 0012 question of "what JS engine?" — but cleanly, not as a reversal. ADR 0012's "no JS in v1" was the right call given what we knew then; the work since (protocol, action graph, positioning) earned the right to extend with eyes open.

## Implementation phases

Phase 0 (this ADR + nothing else changing): committed to QuickJS via [`rquickjs`](https://crates.io/crates/rquickjs). No code yet.

Phase 1 (next session, multi-week scope):
- Bring up `heso-engine-js` crate with `rquickjs` integrated. "Hello world" — evaluate `1 + 1` from Rust.
- Implement core DOM types (Document, Element, Node, NodeList) backed by `scraper::Html`.
- Implement window globals + console + JSON + URL.
- `<script>` tag execution on page load. Verify on a vanilla onclick page.

Phase 2:
- Event model. `dispatchEvent`, `addEventListener`, `click()` actually firing handlers.
- HTMLInputElement value get/set + InputEvent.
- HTMLFormElement.submit() backed by our reqwest.
- `setTimeout`/`setInterval` with fake clock.
- `fetch` backed by reqwest, Promise-aware.

Phase 3:
- State-graph extraction: for each interactive ref in the action graph, run the handler in a sandbox snapshot, capture DOM mutations, materialize as a sub-directory. This is the "every open path becomes a folder" feature.
- `heso click` / `heso fill` / `heso submit` CLI + RPC methods. Real `click @e7` that actually does something.

Phase 4+:
- React/Vue compatibility passes — discover the common things they reach for that we missed, implement those.
- Determinism audit + signed receipts that cover post-load state.
- Network recording/replay (per ADR 0008's plan).

## References

- [`rquickjs` crate documentation](https://docs.rs/rquickjs/)
- [QuickJS — Fabrice Bellard](https://bellard.org/quickjs/)
- [WHATWG DOM Living Standard](https://dom.spec.whatwg.org/) — the spec we're implementing a deliberately small subset of
- ADR 0002 (engine trait boundary) — `EngineApi` extends with one method to accommodate the JS path
- ADR 0008 (determinism) — preserved by fake clock + seeded RNG + recorded network
- ADR 0010 (terminal primitives) — `click`, `submit`, `eval`, `wait` from the original 15 finally have engine support to back them
- ADR 0012 (fetch-only native engine) — this ADR *extends* rather than supersedes; the static path stays the default
- ADR 0013 (engine as semantic extractor) — the action graph becomes the address space the JS layer dispatches against
