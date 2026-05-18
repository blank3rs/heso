# heso

> Headless browser for the agent-relevant half of the web. 30MB single binary, sub-100ms cold start, no Chromium. Handles fetch, parse, JS hydration, forms, clicks, sessions. Returns structured agent-shaped JSON with content-hashed signed receipts.

heso is the agent-shaped equivalent of `chromium --headless` + Playwright. Browser Use, Stagehand, Skyvern, and Operator are all wrappers around Playwright/Chromium — and Chromium is bloat for the agent use case. The rendering pipeline (Skia, Blink layout, compositor, GPU, video, WebGL, canvas) is roughly 70% of why headless Chromium ships at ~180–240 MB. An agent reading a docs site, filling a login form, clicking through a checkout flow doesn't need any of that. heso is what you get when you keep the boring half (fetch, parse, JS, DOM, cookies, forms, clicks, sessions) and drop the rendering half. See [ADR 0016](decisions/0016-positioning-headless-browser-for-agents.md).

One `heso` run produces a **plat**: a content-hashed, signable, agent-shaped JSON map of the page. Same plat → same hash, every time. See [ADR 0015](decisions/0015-heso-as-cartographer.md) for the plat as output artefact.

## The numbers

| | heso (today) | headless Chromium |
|---|---|---|
| Binary size (stripped release) | **8.1 MB** | ~240 MB |
| JS engine | QuickJS via `rquickjs` (~600 KB) | V8 (~30 MB) |
| Install | `cargo build && ./heso.exe` | `npm install playwright` + Chrome download |
| Cold start | sub-100ms target (TODO measure) | 1–2 s |
| Idle RAM | <20 MB target (TODO measure) | 100+ MB |
| Tests | 175 workspace lib tests green | n/a |

8.1 MB is the measurement today, post-QuickJS bundling. The 30 MB number in the one-liner is the budget ceiling for the full Phase 1C+ feature surface, not a current measurement.

## What's in (the agent-relevant half)

| Capability | Status |
|---|---|
| HTTP/HTTPS, redirects, cookies | done (`reqwest`) |
| HTML parse | done (`scraper`) |
| JS execution | Phase 1A landed (QuickJS via `rquickjs`) |
| Form fill + submit | days of work — action graph has the refs |
| Click links / buttons | weeks — follow href + POST |
| Wait for content | needs DOM wiring |
| LocalStorage / sessionStorage | days |
| Fetch API in JS | 1–2 weeks (proxy `reqwest` into QuickJS) |
| Multi-page sessions | designed in (`page_id` in `heso serve`) |
| File downloads / uploads | trivial / days |
| Headers, auth | trivial |
| IntersectionObserver, ResizeObserver | stub-able (fire-once) |

## What's out (and that's the bet)

- Canvas pixels, WebGL, Three.js demos, Figma. Agents don't need this.
- Video / audio playback.
- WebRTC.
- Service Workers (most agent sites don't depend on SW).
- Real CSS layout, animations, transitions.

If your data is locked behind canvas or video, heso isn't for you. Use a real browser. That's fine.

## Precedent

[jsdom](https://github.com/jsdom/jsdom) (~50k LOC of JavaScript) and [happy-dom](https://github.com/capricorn86/happy-dom) (~30k LOC) both prove a minimal DOM + JS environment handles the agent half of the web. Both are slow because they are JS-in-JS, used mostly for testing, never shipped as a product. Doing it in Rust against QuickJS is the obvious next move and nobody has shipped it as a product aimed at agents. There is a real gap on the shelf.

## What works today

- **`heso open <url>`** — single subprocess returns the agent-shaped payload: `{ url, title, description, metadata, tree, actions, plat_hash }`. The bundle four extractors agreed on (lib/tree/metadata/actions, one parse). The `plat_hash` is a BLAKE3 content fingerprint anyone can recompute to verify the plat is unmodified.
- **`heso open --explore-links 1 --link-cap 20 <url>`** — V0 cartography. Pre-fetches up to 20 same-origin links and embeds each one's tree + metadata + action graph under `linked_pages`. ~0.5–1 s on docs sites. Filters out cross-origin, fragments, `mailto:`, `javascript:`, duplicates, cycles. Per-link errors captured individually.
- **`heso meta <url>`** — Schema.org JSON-LD, OpenGraph, Twitter cards, standard `<meta>`, canonical, icons, `<html lang>`. Sorted, deterministic. Often answers "what does this company do" with zero LLM tool calls. See [ADR 0013](decisions/0013-engine-as-semantic-extractor.md).
- **`heso tree <url>` / `heso ls <url> [path]` / `heso cat <url> <path|@ref>`** — page as a filesystem of heading-defined sections. `cat` is polymorphic — tree path or `@e7` action ref.
- **`heso find <url> [--role link|button|…] [--name SUBSTR] [--section /pricing]`** — list interactive elements with stable `@e0/@e1/…` refs derived from the action graph (ARIA-role-aware).
- **`heso eval-js <js>`** — sandboxed QuickJS evaluator. ADR 0014 Phase 1A. Runs the language but not the browser — no DOM, no `window`, no `document` yet. Phase 1B (agent-shaped DOM types backed by `scraper::Html`) is the next major lift.
- **`heso serve`** — long-running JSON-RPC 2.0 over stdin/stdout. Stateful page cache keyed by `page_id`. Methods: `open` (with `explore_links_depth`), `ls`, `cat`, `find`, `close`, `ping`. The integration surface frameworks plug into.
- **`heso fetch <url>`** — low-level `{ url, text }`. No Chrome, no Node.
- **`heso plat-hash <file>` / `heso plat-verify <file>`** — BLAKE3 over the canonical-JSON serialization of a plat. Exit codes for use in scripts and CI.
- Trace runner + BLAKE3 `trace_hash` receipts ([ADR 0008](decisions/0008-deterministic-execution.md)).

## What is not real yet

- **JS-driven content.** Phase 1A (the language) ships; Phases 1B (agent-shaped DOM) and 1C (run `<script>` tags on load) are the months of work remaining. SPA-mounted content is invisible to `heso open` until 1C.
- **Cross-fetch ref stability.** `@e0/@e1/…` are stable within one fetch only. Content-addressed cross-fetch stability is on the V2 roadmap.
- **Signed plats.** Today the plat has a BLAKE3 content hash; Ed25519 signing arrives with V2.
- **`heso run <url> <request>`** — still a `[STUB]`. Navigates only; request text isn't interpreted. Waits on the planner (T-022).
- **Most of the 15-primitive vocabulary in [ADR 0010](decisions/0010-primitives-as-terminal-commands.md).** `pwd`/`ls`/`cd`/`cat`/`find` (action graph) work; `click`/`submit`/`fill`/`eval`/the rest wait on Phase 1B.

## Try it

```sh
cargo build --release -p heso-cli

# basic — text + metadata + tree + actions, one parse
./target/release/heso open https://example.com

# cartography V0 — page + same-origin link sub-trees, ~0.5–1 s
./target/release/heso open --explore-links 1 --link-cap 20 https://docs.rs/

# evaluate JS in a sandbox — Phase 1A; no DOM yet
./target/release/heso eval-js '1 + 1'

# stable JSON-RPC server for frameworks (Browser Use, Stagehand, …)
./target/release/heso serve
```

## 6-month roadmap

Wire existing primitives against the action graph (~3–4 weeks). Cookies + storage, Fetch API in QuickJS (~3–4 weeks). Then the load-bearing stretch: the agent-shaped DOM (~2 months) — implement what real pages actually call, stub the rest. Hydration + determinism + receipt signing + a 100-URL compat harness + planner v0 + docs/packaging fill the remaining ~2 months. Total: ~4–5 months focused work with ~1 month slack. Tight but realistic. Full accounting in [ADR 0016](decisions/0016-positioning-headless-browser-for-agents.md).

## What makes it different

- **No Chromium dep.** Single Rust binary. No `npm install playwright`, no Chrome download, no Node, no Python. `cargo build && ./heso.exe`.
- **The plat is an artefact, not a session.** Every other agent-browser tool produces a live session — act, observe, decide, act, observe, decide. heso produces a serializable, content-hashed, deterministic map. The same plat of `stripe.com/pricing` serves every agent. See [ADR 0015](decisions/0015-heso-as-cartographer.md).
- **Engine as semantic extractor.** The engine doesn't just give you HTML — it pre-extracts metadata, the heading tree, an action graph with ARIA-role-aware refs, and (with `--explore-links`) the cartography of linked sub-pages. Four views, one parse. See [ADR 0013](decisions/0013-engine-as-semantic-extractor.md).
- **Deterministic by construction.** Sorted maps, document-ordered vectors, no clocks, no RNG in the engine path. The plat is a function of the inputs; same inputs, same plat — modulo network state, which the [ADR 0008](decisions/0008-deterministic-execution.md) recording story handles later.
- **Honest scope.** The non-goals are explicit in [ADR 0014](decisions/0014-bundled-quickjs-agent-dom.md) and [ADR 0016](decisions/0016-positioning-headless-browser-for-agents.md). No layout, no paint, no canvas/WebGL, no workers, no IndexedDB, no CSS engine. heso runs the JS that handles clicks, fills forms, and computes state; it does not run the JS that paints pixels.

## Working in this repo?

Read [`AGENTS.md`](AGENTS.md) first. It points to everything else — the agent meta directory, ADRs ([0016](decisions/0016-positioning-headless-browser-for-agents.md) is the current positioning; [0015](decisions/0015-heso-as-cartographer.md) frames the plat), research notes, conventions, and the current state in [`state.json`](state.json).

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE) at your option.
