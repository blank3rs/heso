# heso

> Headless browser for the agent-relevant half of the web. Single Rust binary, no Chromium, no Node. Handles fetch, parse, JS, DOM, cookies, forms, clicks, sessions. Returns structured JSON with content-hashed receipts.

heso is the agent-shaped equivalent of `chromium --headless` + Playwright. The browser-agent stack today — Browser Use, Stagehand, Skyvern, Operator — wraps Playwright wrapping Chromium. Chromium is bloat for the agent use case: the rendering pipeline (Skia, Blink layout, compositor, GPU, video, WebGL, canvas) is roughly 70% of why headless Chromium ships at 180–240 MB. An agent reading a docs site, filling a login form, clicking through a checkout flow doesn't need any of that.

heso is what you get when you keep the boring half (fetch, parse, JS, DOM, cookies, forms, clicks, sessions) and drop the rendering half.

Each run produces a **plat** — a content-hashed, deterministic, agent-shaped JSON map of the page. Same plat → same hash, every time. Verifiable, shareable, replayable.

## The numbers

| Metric | Value |
|---|---|
| Binary size (stripped release) | **9.1 MB** |
| Cold start (banner only, median of 10) | **41 ms** *(min 39, p95 52)* |
| Cold start + JS engine init (`eval-js '1+1'`, median) | **40 ms** *(min 38, p95 40)* |
| Full fetch + parse + extract (`fetch https://example.com`, median) | **85 ms** *(min 82, p95 86)* |
| Full DOM eval over network (`eval-dom news.ycombinator.com`, median of 5) | **396 ms** *(min 379)* |
| Workspace lib tests | **273 passing** |

For comparison: headless Chromium ships at ~240 MB, cold-starts in 1–2 s, and idles at 100+ MB RAM. The agent-relevant workload heso targets (fetch, parse, JS, DOM) runs in well under those budgets.

## What it looks like

Real bytes off the wire, from a fresh `cargo build --release && ./heso`:

```console
$ heso eval-dom https://news.ycombinator.com 'document.title'
{
  "console": [],
  "ok": true,
  "url": "https://news.ycombinator.com/",
  "value": "Hacker News"
}
```

```console
$ heso eval-dom https://news.ycombinator.com \
    'Array.from(document.querySelectorAll(".titleline > a")).slice(0,5).map(a => a.textContent)'
{
  "console": [],
  "ok": true,
  "url": "https://news.ycombinator.com/",
  "value": [
    "The foundations of a provably secure operating system (PSOS) (1979) [pdf]",
    "GenCAD",
    "Crystals found inside wreckage from the first nuclear bomb test",
    "It is time to give up the dualism introduced by the debate on consciousness",
    "I turned a $80 RK3562 Android tablet into a Debian Linux workstation"
  ]
}
```

```console
$ heso open https://example.com
{
  "actions": [
    {
      "attrs": { "href": "https://iana.org/domains/example" },
      "name": "Learn more",
      "ref": "@e0",
      "role": "link",
      "section": "/example-domain",
      "tag": "a"
    }
  ],
  "description": "This domain is for use in documentation examples without needing permission.",
  "metadata": { "lang": "en", "meta": { "viewport": "width=device-width, initial-scale=1" } },
  "plat_hash": "abf42bb66917095eb4cafdd4deb00c0686835102e713a3342b32093578007289",
  "title": "Example Domain",
  "tree": {
    "description": "This domain is for use in documentation examples without needing permission.",
    "root": {
      "children": [
        {
          "heading": "Example Domain",
          "intro": "This domain is for use in documentation examples without needing permission. Avoid use in operations. Learn more",
          "level": 1,
          "path": "/example-domain",
          "slug": "example-domain",
          "summary": "Example Domain — This domain is for use in documentation examples without needing permission..."
        }
      ]
    }
  },
  "url": "https://example.com/"
}
```

## What's in (the agent-relevant half)

| Capability | Status |
|---|---|
| HTTP/HTTPS, redirects, cookies | done (`reqwest`) |
| HTML parse | done (html5ever via `dom_query` / `scraper`) |
| Sandboxed JS evaluation | done (QuickJS via `rquickjs`) |
| Read-only DOM (`querySelector`, `textContent`, `getAttribute`…) | done |
| DOM mutations (`setAttribute`, `innerHTML =`, `appendChild`, `classList`…) | done |
| Events (`addEventListener`, `dispatchEvent`, `CustomEvent`, `AbortController`, `DOMException`) | done |
| Timers (`setTimeout`/`setInterval`, deterministic virtual clock) | done |
| Page-load JS hydration (`<script>` on load) | next |
| Form fill + submit | days once `click()` wires to dispatchEvent |
| Click links / buttons | days once `click()` wires to dispatchEvent |
| `fetch()` inside JS | 1–2 weeks (proxy `reqwest` into QuickJS) |
| `localStorage` / `sessionStorage` | days |
| Multi-page sessions | designed in (`page_id` in `heso serve`) |
| File downloads / uploads | trivial / days |
| `IntersectionObserver`, `ResizeObserver` | stub-able (fire-once) |

## What's out (and that's the bet)

- Canvas pixels, WebGL, Three.js demos, Figma. Agents don't need this.
- Video / audio playback.
- WebRTC.
- Service Workers (most agent sites don't depend on SW).
- Real CSS layout, animations, transitions.

If your data is locked behind canvas, video, or computed CSS layout, heso isn't for you. Use a real browser. That's fine.

## Precedent

[jsdom](https://github.com/jsdom/jsdom) (~50k LOC of JavaScript) and [happy-dom](https://github.com/capricorn86/happy-dom) (~30k LOC) both prove a minimal DOM + JS environment handles the agent half of the web. Both are slow because they are JS-in-JS, used mostly for testing, never shipped as a product. Doing it in Rust against QuickJS is the obvious next move and nobody has shipped it as a product aimed at agents. There is a real gap on the shelf.

## What works today

- **`heso open <url>`** — single subprocess returns the agent-shaped payload: `{ url, title, description, metadata, tree, actions, inline_data, data_attrs, plat_hash }`. One parse, four+ extractors. `plat_hash` is a BLAKE3 content fingerprint anyone can recompute.
- **`heso open --explore-links 1 --link-cap 20 <url>`** — V0 cartography. Pre-fetches up to 20 same-origin links and embeds each one's tree + metadata + action graph under `linked_pages`. ~0.5–1 s on docs sites. Filters out cross-origin, fragments, `mailto:`, `javascript:`, duplicates, cycles.
- **`heso meta <url>`** — Schema.org JSON-LD, OpenGraph, Twitter cards, standard `<meta>`, canonical, icons, `<html lang>`. Sorted, deterministic. Often answers "what does this company do" with zero LLM tool calls.
- **`heso tree <url>` / `heso ls <url> [path]` / `heso cat <url> <path|@ref>`** — page as a filesystem of heading-defined sections. `cat` is polymorphic — tree path or `@e7` action ref.
- **`heso find <url> [--role link|button|…] [--name SUBSTR] [--section /pricing]`** — list interactive elements with stable `@e0/@e1/…` refs (ARIA-role-aware).
- **`heso eval-js <js>`** — sandboxed QuickJS evaluator. Runs the language with `console.*` capture + typed exceptions. No DOM (use `eval-dom` for that).
- **`heso eval-dom <url> <js>`** — fetch + parse + run JS against the loaded `document`. `document.querySelector`, `element.textContent`, `element.setAttribute`, the rest. Live-tested on real pages (Hacker News, example.com).
- **`heso serve`** — long-running JSON-RPC 2.0 over stdin/stdout. Stateful page cache keyed by `page_id`. The integration surface frameworks plug into.
- **`heso fetch <url>`** — low-level `{ url, text }`.
- **`heso plat-hash <file>` / `heso plat-verify <file>`** — BLAKE3 over the canonical-JSON serialization of a plat. Exit codes for scripts and CI.
- Trace runner + BLAKE3 `trace_hash` receipts.

## What is not real yet

- **`<script>` on page load.** The DOM exists; JS can run; but heso doesn't yet execute the page's own scripts during `open`. So SPA-mounted content (the stuff that's empty until React/Vue hydrates) is still invisible. That's the next major lift.
- **Events & timers.** `addEventListener` / `dispatchEvent` / `AbortController` and `setTimeout` / `setInterval` are in flight — they're what unlocks `element.click()` actually firing handlers.
- **Form submission with JS validation.** Plain `<form>` POSTs are doable through `reqwest` today; JS-validated forms wait on events.
- **Cross-fetch ref stability.** `@e0/@e1/…` are stable within one fetch only. Content-addressed cross-fetch stability is on the roadmap.
- **Signed plats.** Today the plat has a BLAKE3 content hash; Ed25519 signing arrives next.
- **`heso run <url> <request>`** — still a stub. Navigates only; the natural-language request isn't interpreted yet. Waits on the planner.

## Try it

```sh
cargo build --release -p heso-cli

# basic — text + metadata + tree + actions, one parse
./target/release/heso open https://example.com

# cartography V0 — page + same-origin link sub-trees, ~0.5–1 s
./target/release/heso open --explore-links 1 --link-cap 20 https://docs.rs/

# evaluate JS in a sandbox — no DOM
./target/release/heso eval-js '1 + 1'

# fetch a page, run JS against its DOM — Phase 1B is live
./target/release/heso eval-dom https://news.ycombinator.com \
  "Array.from(document.querySelectorAll('.titleline > a')).slice(0,5).map(a => a.textContent)"

# stable JSON-RPC server for frameworks (Browser Use, Stagehand, …)
./target/release/heso serve
```

## Roadmap

**Now → 1 month:** finish Phase 1B (events, timers with deterministic virtual clock) + Phase 1C (run `<script>` tags on page load so SPAs actually hydrate). Wire existing primitives (`click`, `submit`, `fill`) against the action graph. This is where heso starts working on real React/Vue pages.

**1 → 3 months:** cookies + storage, `fetch()` inside JS proxied through the native client (so the engine's cookie jar and audit receipts stay coherent), Ed25519 signed receipts, a planner v0 (thin LLM wrapper — not where to spend the budget). A 100-URL compatibility harness to keep regressions out.

**3 → 6 months:** the long tail. React/Vue compatibility passes against the harness, MCP server polish, packaging, docs site. By month 6, heso is a credible single-binary alternative to headless-Chromium-plus-Playwright for the agent half of the web.

Tight but realistic for solo development. The DOM month (which already mostly landed via `dom_query` + the bridge work) was the highest-risk piece; what's left is largely a sequence of known-shape problems.

## What makes it different

- **No Chromium dep.** Single Rust binary. No `npm install playwright`, no Chrome download, no Node, no Python. `cargo build && ./heso`.
- **The plat is an artefact, not a session.** Every other agent-browser tool produces a live session — act, observe, decide, act, observe, decide. heso produces a serializable, content-hashed, deterministic map. The same plat of `stripe.com/pricing` serves every agent.
- **Engine as semantic extractor.** The engine doesn't hand back raw HTML — it pre-extracts metadata, the heading tree, an action graph with ARIA-role-aware refs, inline-script hydration data (Next.js `__next_f`, Apple `__ACGH_DATA__`, Netflix `netflix.reactContext`, `window.X` assignments), `data-*` JSON payloads, and (with `--explore-links`) the cartography of linked sub-pages. Many views, one parse.
- **Deterministic by construction.** Sorted maps, document-ordered vectors, no clocks or RNG in the engine path. The plat is a function of the inputs; same inputs, same plat — modulo network state, which a recording layer handles later.
- **Honest scope.** No layout, no paint, no canvas/WebGL, no workers, no IndexedDB, no CSS engine. heso runs the JS that handles clicks, fills forms, computes state. It does not run the JS that paints pixels.

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE) at your option.
