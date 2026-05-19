<div align="center">

# heso

**A browser for agents, not for humans.**

One ~7.8 MB Rust binary. No Chromium. No Node. No `npm install playwright`.
Fetches, parses, runs JS, holds a stateful page session across clicks, hands back content-hashed JSON you can sign, diff, and **replay byte-for-byte**.

</div>

---

## The problem

Every agent framework today — Browser Use, Stagehand, Skyvern, Operator — is a Python or Node loop wrapped around Playwright wrapped around **240 MB of headless Chromium**.

Your agent is reading a docs page. Filling a login form. Clicking through a checkout.

It does not need Skia. It does not need Blink's layout engine. It does not need the compositor, the GPU pipeline, WebGL, canvas, or the video stack. That is roughly **70% of why Chromium is huge** — and 100% of why it's slow to start, painful to deploy, and miserable to run at scale.

You're paying for a rendering engine to render pixels nobody will ever look at.

## The bet

Keep the boring half of the browser — fetch, parse, JS, DOM, cookies, forms, clicks, sessions, sandboxing.

Drop the rendering half.

Ship it as one binary.

## The "holy shit" demo

```console
$ heso eval-dom https://news.ycombinator.com \
    'Array.from(document.querySelectorAll(".titleline > a")).slice(0,5).map(a => a.textContent)'
{
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

Five real story titles, off the live wire, fetched + parsed + JS-evaluated, in **under 400 ms**, from a **~7.8 MB single binary**.

No Chromium. No Node. No browser download. Just `cargo build && ./heso`.

## Why this matters

|  | heso | Playwright + Chromium |
|---|---|---|
| Install size | **~7.8 MB** | ~240 MB + Node + browser bundle |
| Cold start | **40 ms** | 1–2 seconds |
| Idle RAM | tiny | 100+ MB per browser |
| Deploy unit | one static binary | runtime + browser + driver |
| Reproducibility | content-hashed, seeded RNG, virtual clock | non-deterministic |
| Audit trail | every fetch → signable receipt | nothing |
| Rendering pixels | ✗ — that's the point | ✓ |

If your agent needs to *look* at a canvas, a video, or a CSS animation: use Chromium. heso is honest about that.

If your agent needs to *do things* on the agent-relevant half of the web — read, click, fill, extract, audit — heso is built for exactly that and nothing else.

## 30-second quickstart

```sh
cargo build --release -p heso-cli
./target/release/heso open https://example.com
```

That gives you a **plat** — an agent-shaped JSON map of the page:

```json
{
  "url": "https://example.com/",
  "title": "Example Domain",
  "description": "...",
  "metadata": { "opengraph": {...}, "jsonld": [...] },
  "tree":     { "/": {...} },
  "actions":  [ { "ref": "@e0", "role": "link", "name": "More information..." } ],
  "plat_hash": "abf42bb66917095eb4cafdd4deb00c0686835102e713a3342b32093578007289"
}
```

Same page → same hash, byte for byte. On any machine. Forever.

That hash is the receipt. Sign it, store it, diff it next week, prove what the page said when your agent acted on it.

## One example per killer feature

**JS that mutates the DOM, on a real fetched page:**
```console
$ heso eval-dom https://example.com \
    'document.querySelector("h1").textContent = "Hijacked"; document.body.innerHTML.slice(0, 80)'
→ "<div><h1>Hijacked</h1><p>This domain is for use in documentation examples withou"
```

**Click a real link through the JS event model:**
```console
$ heso find https://news.ycombinator.com --role link --name "more"
→ { "ref": "@e220", "role": "link", "name": "More" }

$ heso click https://news.ycombinator.com @e220
→ { "ok": true }
```

`fill` fires both `input` and `change`. `submit` walks the form. The `@e0/@e1/…` refs are stable across the whole click/fill/submit cycle.

**Determinism, on tap:**
```console
$ heso eval-js --seed 42 'Math.random()'  →  0.5140492957650241
$ heso eval-js --seed 42 'Math.random()'  →  0.5140492957650241
$ heso eval-js --seed 99 'Math.random()'  →  0.5052084295432834
```

`Math.random`, `crypto.getRandomValues`, `crypto.randomUUID` — all routed through a seeded ChaCha20 PRNG. Same seed, byte-identical output, on every machine, forever.

**Sites as filesystems:**
```sh
heso tree https://stripe.com
heso ls   https://stripe.com /pricing
heso cat  https://stripe.com /pricing/business
```

The page is a tree of heading-defined sections. Navigate it like a directory.

**Stateful replay — every action keyed, every page recoverable:**
```console
$ heso action-hash https://example.com '[{"verb":"open","url":"https://example.com/"},{"verb":"click","ref":"@e0"}]' > trace.json
$ heso replay trace.json
{
  "algorithm": "heso-trace-fp/v1",
  "trace_id": "632b9a3c…0ef3b2",
  "fingerprint_valid": true,
  "ok": true,
  "steps": [ … ]
}
```

The `trace_id` is a **BLAKE3 Merkle chain** over the URL + canonical actions. Anyone running the same trace anywhere gets the same hash — no keys, no central server, no central clock. Tampering breaks it. Replay carries one `JsSession` across every step: DOM mutations persist, `addEventListener` handlers fire, `setTimeout` chains progress through a virtual clock, `e.preventDefault()` on `<a href>` clicks stops navigation just like a real SPA router.

**Drop-in for any agent framework:**
```sh
heso serve     # JSON-RPC 2.0 over stdin/stdout, stateful page sessions
```

Point Browser Use, Stagehand, or your own loop at the stdio transport. Swap Chromium out, leave the agent code alone.

## Who this is for

- **Agent framework builders** who are tired of shipping 240 MB of Chromium to do `document.querySelector`.
- **RAG pipelines** that need to ingest docs sites at scale without operating a headless Chromium farm.
- **Compliance / archival** workflows where "prove what the page said" matters more than "show me the pixels."
- **CI test suites** that need reproducible page snapshots without flaky timing.
- **Anyone wide-crawling for competitive intel** who wants ~100 ms per page on one machine instead of a fleet.

Not for: scraping data behind canvas, video, computed CSS layout, WebGL, or service workers. Use a real browser — that's what they're for.

## Status

| | |
|---|---|
| HTTP/HTTPS, cookies, redirects | ✅ |
| HTML parse (html5ever) | ✅ |
| Sandboxed JS (QuickJS) | ✅ |
| DOM read + mutate, `createElement` / `createTextNode` / `createElementNS` | ✅ |
| Events with W3C capture/bubble walk, timers, `AbortController` | ✅ |
| **Document in dispatch path** — React 19 synthetic events delegate cleanly | ✅ |
| Node traversal — `childNodes`, `firstChild`/`lastChild`, `nextSibling`/`previousSibling`, `firstElementChild`/`lastElementChild`, `*ElementSibling`, `childElementCount`, `hasChildNodes`, `contains`, `isConnected`, `cloneNode(deep)`, `remove()` | ✅ |
| `nodeType`, `nodeName`, `parentNode`, `ownerDocument`, `getElementsByTagName`, `insertBefore` | ✅ |
| `element.className` setter, `classList`, `setAttribute` (bool/number/null coerced per spec) | ✅ |
| **HTMLInputElement IDL split** — `.value` / `.checked` separate from content attrs; `defaultValue` / `defaultChecked`; `disabled`/`readOnly`/`required` reflected; `.type` / `.name` / `.placeholder` IDL | ✅ |
| `Element.style` as `CSSStyleDeclaration`-shaped Proxy with real CSS-property allow-list (~500 props + custom `--*`) | ✅ |
| **Text/comment node wrapper safety** — element-only ops return empty default or throw `TypeError` | ✅ |
| `click` / `fill` / `submit` through `dispatchEvent` (returns `defaultPrevented`) | ✅ |
| `<script>`-on-load (SPA inline-script hydration), relative `<script src>` resolved against page URL | ✅ |
| `fetch()` inside JS (shared `reqwest::Client`) | ✅ |
| **Stateful `JsSession`** — one engine, one document, listeners persist across calls | ✅ |
| **Stateful replay** (`heso replay trace.json`) — anchor preventDefault, navigation tracking, `--seed N` | ✅ |
| **Trace fingerprints** — keyless, algorithm-derived BLAKE3 Merkle chain | ✅ |
| Seeded RNG (`--seed N`) — `Math.random`, `crypto.*` | ✅ |
| `Date.now` / zero-arg `new Date()` routed through VirtualClock | ✅ |
| `window`, `window.location`, `window.history`, lazy DOM-ctor stubs via prototype Proxy | ✅ |
| WHATWG-shaped `URL` global (`new URL(href, base)`, `.canParse`) | ✅ |
| `navigator` (`.userAgent`/`.language`/`.webdriver=false`), `queueMicrotask`, `requestAnimationFrame`/`cancelAnimationFrame`, `performance.now()` | ✅ |
| `atob` / `btoa`, `matchMedia`, in-memory `localStorage` / `sessionStorage` | ✅ |
| `document.readyState='complete'`, `document.activeElement`, `document.cookie` (stub), `document.contains` | ✅ |
| Element layout zero-stubs — `getBoundingClientRect`, `getClientRects`, `client*`/`offset*`/`scroll*` dims, `focus`/`blur`/`scrollIntoView` | ✅ |
| Content-hashed plats (BLAKE3) | ✅ |
| Ed25519 signed receipts | ✅ |
| **TodoMVC Preact renders end-to-end** through `heso eval-dom --js-fetch` | ✅ |
| **565 workspace tests, 2 ignored** (TypeError-throw guards pending Ctx-bound merge with IDL paths) | ✅ |
| Recorded-network playback (cassettes) for byte-identical replay | 🚧 designed |
| `MutationObserver` / `IntersectionObserver` / `ResizeObserver` (noop stubs) | 🚧 next |
| SVG namespace tracking, full WHATWG URL mutation (`searchParams`), `history.pushState` → `popstate` | 🚧 next |
| React 19 full interaction round-trip — `KeyboardEvent` / `InputEvent` / `MouseEvent` ctors, focus tracker | 🚧 weeks |
| Real `document.cookie` jar (shared with `reqwest`) | 🚧 weeks |

Honest about scope. Honest about gaps. No vapor.

## The precedent

[jsdom](https://github.com/jsdom/jsdom) (~50k LOC of JS) and [happy-dom](https://github.com/capricorn86/happy-dom) (~30k LOC) both proved that **a minimal DOM + JS environment handles the agent half of the web**. Both are slow because they're JS-in-JS. Both are framed as test tools, not as agent infrastructure.

Doing it in Rust against QuickJS is the obvious next move — and nobody has shipped it yet. That gap is the bet.

## Try it

```sh
git clone https://github.com/Akshay-Dongare/heso
cd heso
cargo build --release -p heso-cli

./target/release/heso open      https://example.com
./target/release/heso meta      https://stripe.com
./target/release/heso find      https://news.ycombinator.com --role link
./target/release/heso eval-dom  https://example.com 'document.title'
./target/release/heso serve     # JSON-RPC over stdio
```

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE).

---

<div align="center">

**Built on the bet that the agent half of the web doesn't need a rendering engine.**

</div>
