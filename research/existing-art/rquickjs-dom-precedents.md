# Existing art: Rust + QuickJS + DOM

**Topic:** Prior art survey for heso's JS-DOM bridge (Phase 1B/1C).
**Last updated:** 2026-05-18
**Status:** Snapshot — verify versions before depending on any crate.
**Question:** What's already been built that heso can depend on, copy, or learn from before we commit months to mutations + event dispatch + setTimeout/fetch shims?

**TL;DR:** Nobody has shipped a working **rquickjs + agent-shaped DOM** combo — the closest competitor (Lightpanda, ~30k stars) is Zig + V8 + Netsurf-libs, not our stack. We should depend on `dom_query` for the mutable DOM (active, May 2026, jQuery-like API on html5ever), copy LLRT's published `llrt_modules` source tree for fetch/timers/url/buffer/crypto shims (Apache-2.0, rquickjs-native, not yet on crates.io so vendor), and copy jsdom's implementation **order** (parse → DOM tree → events → timers → fetch; punt navigation + layout forever).

## Verdict by tier

| Find | Verdict |
|------|---------|
| Any direct **rquickjs + DOM** precedent in the wild? | **No.** rquickjs explicitly punts web APIs. Boa, LLRT, rquickjs-extra all stop at the Node-shaped runtime layer. |
| Lightpanda OSS / closed / what stack? | **OSS (AGPL-3.0), Zig + V8 + Netsurf DOM** — totally different stack. Not forkable; mine for ideas. |
| Best Rust mutable-DOM crate to depend on | **`dom_query`** (jQuery-like, html5ever-backed, 309k weekly downloads, 0.28.0 May 2026). Fallback: `markup5ever_rcdom` (low-level, "not production"). |
| Best jsdom architecture to copy | **Yes — order of implementation only.** Don't lift code (MIT JS in our Rust). Lift the "what we punt" list verbatim (navigation, layout). |
| Deno's `ext/web` JS files usable? | **Partial yes — MIT-licensed.** The pure JS (`02_event.js`, `01_dom_exception.js`, `03_abort_signal.js`) is portable in spirit but coupled to Deno bootstrap. Port logic, not bytes. |

---

## Top finds

### 1. Lightpanda — `lightpanda-io/browser` ([GitHub](https://github.com/lightpanda-io/browser))

The closest live competitor to heso's pitch ("browser engineered for AI and automation"). Zig, AGPL-3.0, ~30.4k stars, active. **Stack:** V8 (via their own `zig-js-runtime`), HTML parsing via Netsurf libs (their README has been inconsistent — earlier search hits cited html5ever, but the build pulls Netsurf), HTTP via libcurl, CDP server compat with Puppeteer/Playwright. They claim ~9× faster and ~16× lighter than Chrome. WebAPI coverage list: DOM tree, XHR, Fetch (polyfill), DOM dump, click, form input, cookies. They are explicit that "there are hundreds of Web APIs" and the implementation is "Beta, work in progress."

**What to do with it:** Watch and mine for design ideas. AGPL means we can't link any of their code. Zig + V8 vs Rust + QuickJS is enough divergence that file-level porting wouldn't work even if license allowed it. Their CDP-compat strategy is the inverse of heso's "one tool" thesis (ADR 0009) — don't copy it.

### 2. `dom_query` — `niklak/dom_query` ([GitHub](https://github.com/niklak/dom_query), [crates.io](https://crates.io/crates/dom_query))

A jQuery-like wrapper over `html5ever` + `selectors`. **Mutable**, with `set_attr`, `new_element`, `append_html`, `append_child`-equivalents, element rename, move-within-tree. Supports `:has`, `:has-text`, `:contains`, `:only-text`. Last release **0.28.0 (May 18, 2026)** — released the same day we're writing this. **88 stars** but **309k weekly downloads** (up 10× since March 2026 — strong adoption curve). 869 commits, 48 releases (27 breaking) — still pre-1.0 and willing to break.

**What to do with it:** Strongly consider depending on this for the underlying mutable tree before we wrap our own JS-DOM bridge over it. Fork of `nipper` with the rough edges sanded. The breaking-change cadence is a real risk for a load-bearing dep — pin minor versions. Confirm:  (a) handle stability across mutations (does an `Element` ref invalidate when the tree changes?), (b) whether the `selectors` version it pulls supports `:where()`/`:is()`.

### 3. LLRT and `llrt_modules` — `awslabs/llrt` ([GitHub](https://github.com/awslabs/llrt))

AWS's QuickJS-based serverless runtime (Apache-2.0). The interesting bit is **`llrt_modules`**: a meta-crate of pure-Rust rquickjs modules for Node/WinterCG APIs — `llrt_fetch`, `llrt_timers`, `llrt_console`, `llrt_crypto`, `llrt_url`, `llrt_buffer`, `llrt_stream`. **Not yet published to crates.io** (per their README — clone as a path dep). Last release `v0.8.1-beta` Feb 20, 2026 — actively maintained. They use rquickjs natively, so the binding patterns will transplant directly into heso.

**What to do with it:** Vendor the specific modules we need (fetch + timers first) as a git-submodule or `path = ".../llrt/llrt_modules/llrt_fetch"` dep until they publish. License is compatible (Apache-2.0 vs our MIT-or-Apache dual). Don't take all of llrt — most of it is Lambda-shaped (S3/DynamoDB sdks etc.) and irrelevant. Confirm whether `llrt_fetch` honors our determinism contract (ADR 0008) before wiring it up — if it uses a wall clock for cache invalidation or Date headers, we have to gate it.

### 4. Deno's `ext/web` JS modules — `denoland/deno/ext/web` ([GitHub](https://github.com/denoland/deno/tree/main/ext/web))

The most-real "web platform implementation in Rust" in production. **MIT-licensed.** The directory ships ~16 JS files for: DOMException, Event/EventTarget, AbortSignal/AbortController, structuredClone, Streams, TextEncoder/TextDecoder, File/FileReader, URL, Performance, Location, MessagePort, base64, compression, etc. The Event file alone is ~1.6k lines and implements the full WHATWG event dispatch including bubbling/capturing/shadow-DOM retargeting.

**What to do with it:** **Port the Event/EventTarget logic** (the spec is finicky; reimplementing from the WHATWG spec is a known LLM trap). Same for AbortController/AbortSignal and DOMException. The JS is engine-agnostic in *content* but uses Deno-specific `primordials` and `__bootstrap` hooks — we'd lift the logic, not the bytes. Cite Deno in the resulting Rust source and the trace runner's JS preamble. Skip the Streams file unless we genuinely need it (it's huge and most of v1 won't).

### 5. linkedom — `WebReflection/linkedom` ([GitHub](https://github.com/WebReflection/linkedom))

**ISC license**, ~2k stars, JS. Different model from jsdom: every node is two linked siblings (start + end), so the whole tree is one doubly-linked list. Moving N nodes = updating 4 pointers, no recursion. Skips deprecated APIs, skips live collections (`getElementsByTagName` returns static), prioritizes server-side rendering speed over spec compliance. 1/3 the heap and 1/3 the time vs jsdom on the same workload.

**What to do with it:** **Architecture model worth studying** if `dom_query`'s memory model bottlenecks us — the triple-linked-list approach maps cleanly onto Rust's arena pattern (one `Vec<Node>` indexed by node id, two `Option<NodeId>` per node for prev/next). Don't port the code; port the data structure idea.

### 6. jsdom — `jsdom/jsdom` ([GitHub](https://github.com/jsdom/jsdom))

12+ years old, MIT, ~21k stars, v29.1.1 (Apr 30, 2026). **The canonical "DOM-in-JS" effort.** Their explicit "not implemented" list after a decade-plus is short: **navigation** (clicking links / setting `location.href`) and **layout** (`getBoundingClientRect` returns zeros). Everything else they got to eventually. Architecturally: WebIDL files define the shape, codegen produces type-conversion boilerplate, JS implementations live in separate files.

**What to do with it:** Lift the **implementation order** and the **punt list**. Their punt list is precisely what heso's architecture lets us punt forever (no navigation per ADR 0009 — agents call `heso.run` again; no layout per ADR 0012 — fetch engine doesn't render). Their codegen pipeline is overkill for our scope but the IDL-first discipline is worth thinking about.

### 7. happy-dom — `capricorn86/happy-dom` ([GitHub](https://github.com/capricorn86/happy-dom))

MIT, ~4.5k stars, v20.9.0 (Apr 13, 2026), 681 releases. **Faster jsdom alternative** for Vitest/Jest test envs. Explicitly trades spec compliance for the common path. Implements custom elements, declarative shadow DOM, mutation observer, tree walker, fetch.

**What to do with it:** Skim, don't depend. The "common-path-only" philosophy lines up with heso's "agents don't need full spec compliance" thesis, but happy-dom is browser-test-shaped and includes things we don't need (custom elements, shadow DOM). Read their "what we skipped" wiki page once when defining our DOM compat target — they've already done the prioritization work for a 90%-good-enough-for-real-pages cut.

### 8. Servo (`servo` 0.1.0 on crates.io, Apr 2026) — ([servo.org blog](https://servo.org/blog/2026/04/13/servo-0.1.0-release/))

Servo's `script` crate was the historic monolith we wanted to extract a DOM from. **News from April 2026:** Servo published as an embeddable crate (`servo = 0.1.0`) with an embedding API. Still tightly coupled to layout/style — pulling out a standalone DOM crate has been a multi-year work-in-progress (issue #1799, prototype `script_bindings` split, `servo-dom-struct` PR #43458). Not extractable cleanly today; possibly tractable as Servo finishes the split.

**What to do with it:** **Don't depend on Servo for DOM.** Stay with the html5ever family. Re-check in 12 months — if Servo lands the script-crate split and ships a `servo-dom` crate, that becomes an interesting fallback engine path (and ties in with the "WebView for JS" deferred item in PRIORITIES.md).

### 9. Boa-DOM — does not exist

Searched exhaustively. **Boa is a JS engine only.** `boa_runtime` adds console, fetch, setTimeout, queueMicrotask but no DOM. No "Boa-DOM" project has shipped. The Rust-JS-engine ecosystem has bifurcated into "engine only" (rquickjs, Boa) and "engine + runtime, no DOM" (LLRT). Nobody has built the DOM layer on top in Rust.

**What to do with it:** Heso is the first attempt to ship rquickjs + DOM. That's exciting (no existing precedent to depend on) and concerning (nobody has validated this combination at scale). Plan for the unknown-unknowns.

### 10. Obscura — `h4ckf0r0day/obscura` ([GitHub](https://github.com/h4ckf0r0day/obscura))

Apache-2.0, **178 stars** (launched April 13, 2026 — very fresh). Rust headless browser, embeds **V8 directly** (~5 min first build), speaks CDP, 30 MB memory / 70 MB binary. 6-crate Rust workspace. Built for AI agents + scraping.

**What to do with it:** Closest "Rust + V8 + headless + agent-shaped" project in the wild. Different JS engine (V8 not QuickJS) and different agent surface (CDP not "one tool"), so direct code reuse is unlikely. Watch their crate split as a sanity check on our own `crates/` layout. Stars trajectory is steep — they could be a real competitor in 6 months.

### 11. rquickjs-extra and rquickjs-extension — ([GitHub](https://github.com/rquickjs/rquickjs-extra), [docs.rs](https://docs.rs/rquickjs-extension))

`rquickjs-extra` (Apache-2.0, v0.2.1 Dec 2025, **14 stars**): partial console, OS, timers, URL, sqlite. Notably calls itself "an overflow for modules not yet integrated into AWS LLRT." `rquickjs-extension` (MIT, v0.0.3): not modules but an **extension framework** — `Extension` trait, `ExtensionBuilder`, `ModuleLoader`, intended to standardize how rquickjs modules expose themselves.

**What to do with it:** Pass on `rquickjs-extra` (too thin, too few stars). Consider `rquickjs-extension`'s trait shape as inspiration for how heso's primitives layer hands JS-backed primitives to the trace runner — not as a dependency, but as a "what's a clean Rust idiom for module registration" reference.

---

## Implementation-order lessons (from jsdom, happy-dom, linkedom, Deno, LLRT)

What the experienced implementations did first, in order, and what they punted:

1. **DOM tree + read-only traversal first.** jsdom, happy-dom, linkedom, Deno's web layer all built the read path before mutations. Phase 1B Day 1 in heso already did this — we are on the well-trodden path.
2. **Querying (CSS selectors) before mutations.** All four shipped `querySelector` before `appendChild` mattered. `dom_query` already gives us this on the Rust side.
3. **`textContent` and `innerHTML` are the most-used mutations.** Implement them next, before per-attribute setters. The Lightpanda and Browser-Use telemetry both call this out — agents overwhelmingly want to read text and dump HTML; mutation is rare unless filling a form.
4. **Events before timers.** jsdom and Deno both shipped Event/EventTarget/CustomEvent before `setTimeout`. Reason: nothing useful runs in a script-driven page until events dispatch, and timer callbacks dispatch *as* events.
5. **`setTimeout` is hard because of determinism.** Every implementation we surveyed cheats: jsdom uses real `setTimeout`, happy-dom has a `FakeTimers` mode, Deno uses real ones in production and fakes in tests. **heso must take the happy-dom approach: virtual clock, advance only when a primitive explicitly waits.** ADR 0008 (determinism) makes the real-timer path off-limits without `unsafe_` in the name.
6. **`fetch` is the next big one and the next big trap.** LLRT's `llrt_fetch` is closest to what we need. Make sure it routes through `heso-engine-fetch` rather than calling `reqwest` directly — otherwise we get two HTTP stacks with different cookie jars, redirect policies, and audit-receipt coverage.
7. **`Promise` integration was the highest-value cheap win in every Rust JS-engine project.** rquickjs already does this (Promise ↔ Rust `Future`). Use it. Don't write a microtask queue from scratch.
8. **Punt forever, like jsdom did:** navigation (we don't need it — agents call `heso.run` again), layout (ADR 0012 — no rendering), CSS computed styles (same), service workers, web workers, WebRTC, WebGL, canvas-2D rendering, audio. heso's architecture **encodes** these punts; jsdom *discovered* them after 12 years.
9. **Mutation observers can wait.** None of jsdom/happy-dom shipped them in the first year. heso likely doesn't need them at all — primitives observe the page directly.
10. **Custom elements / shadow DOM can wait or never come.** happy-dom shipped them because frontend test suites need them. heso's target is *agents reading real pages*, not *agents running test suites*. We can skip both.

## Recommendations

- **Depend on:**
  - **`dom_query`** ([crates.io](https://crates.io/crates/dom_query)) — load-bearing mutable DOM under our JS-DOM bridge. Pin minor (`= 0.28.x`) due to active breaking changes. Confirm handle stability under mutation before committing.
  - **`rquickjs`** (already in tree) with `features = ["futures"]` for Promise↔Future. Confirmed actively maintained, 922 stars, broad platform support including WASM.

- **Copy ideas (and JS logic, not bytes) from:**
  - **Deno's `ext/web`** ([GitHub](https://github.com/denoland/deno/tree/main/ext/web)) — Port Event/EventTarget, AbortController, DOMException by reading their JS and re-implementing in Rust + minimal QuickJS preamble. MIT, cite them. The spec is the source of truth; their code is the de-bugging shortcut.
  - **LLRT's `llrt_modules`** ([GitHub](https://github.com/awslabs/llrt/tree/main/llrt_modules)) — Vendor `llrt_fetch`, `llrt_timers` as path/git deps until published. Wire `llrt_fetch` through `heso-engine-fetch`, not direct reqwest. Apache-2.0.
  - **jsdom's punt list** — Lift "navigation + layout = forever-defer" into our own README of the JS-DOM bridge as load-bearing scope discipline.
  - **happy-dom's `FakeTimers` design** — Inspire our deterministic virtual-clock setTimeout implementation (ADR 0008 mandates it; happy-dom shows it works).
  - **linkedom's triple-linked-list** — If `dom_query`'s mutation cost becomes a hot path, consider a Rust port of linkedom's data structure as our own crate.

- **Build from scratch:**
  - **The JS-DOM bridge itself** (heso-specific binding from rquickjs `Object` to the underlying `dom_query` `Selection`/`Node`). No prior art exists for rquickjs + DOM; we are first. Plan for design iteration.
  - **The deterministic event loop** that funnels primitives, timers, fetches, and microtasks into a single replayable order. ADR 0008 makes this a hard requirement and no prior art respects determinism the way we need.

- **Watch but don't copy:**
  - **Lightpanda** — different stack (Zig+V8+Netsurf), AGPL incompatible. Mine their public WebAPI list as a coverage target.
  - **Obscura** — fresh (April 2026), V8-based, CDP-shaped. Will inform what an "AI-shaped browser ergonomics" Rust API looks like; not source-compatible with us.
  - **Browser Use** ([GitHub](https://github.com/browser-use/browser-use)) — 50k+ stars but Python + Playwright + Chromium. They are heso's *agent-side* analogue, not our DOM-side analogue.
  - **Servo `0.1.0`** crate — too coupled to layout today. Re-check in 12 months once `script` is split.

## Out of scope for this note

- **WebATLAS (arXiv 2510.22732)** is the per-agent-memory paper referenced in the prompt. Skimmed: it's about *agent cognition* (Planner/Actor/Critic + Cognitive Map memory) not browser implementation. Their stack is OS-level memory atop existing browser agents, not a browser. Positioning gap confirmed — they emit per-agent memory; heso emits shared signed traces. Not a source for browser-engine ideas. ([arXiv](https://arxiv.org/abs/2510.22732))
- **WPT (web-platform-tests) and DOM-compat corpora** — exists but is enormous and browser-test-shaped. No agent-flavored curated subset surfaced in the search. Future work: assemble heso's own 100-URL "agent-cooperative-page" corpus as we hit real pages. Not in scope to enumerate now.
- **Per-crate API details** for each candidate. This note is the survey. When a specific dep is chosen, read its docs via `mcp__context7__get-library-docs` at the moment of integration.
- **`scraper` pseudo-class coverage** for `:has()`, `:where()`, `:is()` — could not confirm definitively in this pass. The underlying Servo `selectors` crate (v0.38 in scraper, lives in mozilla-central) supports `:where()` syntax internally; whether it's exposed varies. Verify before promising `:has()` to agents.
- **Stagehand and Skyvern** ([Stagehand](https://github.com/browserbase/stagehand), [Skyvern](https://www.skyvern.com/)) — both are agent layers over Chromium-via-CDP, not browser engines. Same positioning as Browser Use. Out of scope for the engine question.

## References (URLs cited in this note)

- Lightpanda — https://github.com/lightpanda-io/browser, https://lightpanda.io/
- `dom_query` — https://github.com/niklak/dom_query, https://crates.io/crates/dom_query
- `kuchikiki` — https://github.com/brave/kuchikiki (last release v0.8.2, May 2023 — stale)
- `nipper` — https://github.com/importcjj/nipper (likely stale)
- `markup5ever_rcdom` — https://docs.rs/markup5ever_rcdom/ (testing-only, per upstream)
- `html5gum` — https://docs.rs/html5gum (tokenizer; no DOM)
- LLRT — https://github.com/awslabs/llrt, https://github.com/awslabs/llrt/tree/main/llrt_modules
- Deno ext/web — https://github.com/denoland/deno/tree/main/ext/web (MIT)
- rquickjs — https://github.com/DelSkayn/rquickjs
- rquickjs-extra — https://github.com/rquickjs/rquickjs-extra
- rquickjs-extension — https://docs.rs/rquickjs-extension
- jsdom — https://github.com/jsdom/jsdom (v29.1.1, Apr 2026)
- happy-dom — https://github.com/capricorn86/happy-dom (v20.9.0, Apr 2026)
- linkedom — https://github.com/WebReflection/linkedom (ISC, ~2k stars)
- Boa — https://github.com/boa-dev/boa (engine only — no DOM)
- Servo crate — https://servo.org/blog/2026/04/13/servo-0.1.0-release/
- Obscura — https://github.com/h4ckf0r0day/obscura (Apr 2026, Apache-2.0)
- Browser Use — https://github.com/browser-use/browser-use
- WebATLAS — https://arxiv.org/abs/2510.22732
