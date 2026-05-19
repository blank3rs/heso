# heso

A headless browser for agents. One Rust binary. No Chromium, no Node.

heso fetches a URL, parses the HTML, runs the page's JavaScript against an agent-shaped DOM, holds a stateful session across clicks/fills/submits, and emits content-hashed JSON.

## Where heso fits

Two things distinguish heso from the existing options:

**Deployment shape.** A single static Rust binary, 7.65 MB, 17 MB peak RSS. No `node_modules`, no headless Chromium download, no Playwright driver. Drops cleanly into edge runtimes, lambda packages, CI images, CLI distributions, and locked-down environments where bundling a Node runtime or a browser binary is a problem.

**Reproducibility.** Every page session can be deterministic: seeded ChaCha20 PRNG (`Math.random` / `crypto.*` / `crypto.randomUUID`), virtual clock (`Date.now` / `setTimeout` / `performance.now`), content-hashed plats (BLAKE3), and a BLAKE3 Merkle chain over the action trace. The same trace replayed anywhere produces the same fingerprint, with no central server, no shared keys, and no shared clock. Useful for audit, compliance, archival, RAG provenance, and CI snapshot testing.

heso isn't trying to be a broader-compat jsdom or a smaller Chromium. It's a different deployment shape with a different reproducibility story.

## What heso isn't

- **Not a rendering engine.** No canvas, no WebGL, no computed CSS layout, no video, no compositor. If your agent reads pixels, use Chromium.
- **Not at parity with jsdom or Chromium on modern bundler SPAs yet.** Static + server-rendered + simple SPAs (Preact-shaped) work today. React 19 full interaction round-trips, Turbopack-chunked Next.js, and full SVG namespace handling are open work — see the Status table below.
- **Not a replacement for Playwright in long-running session loops.** Playwright with a persistent context amortizes Chromium's startup across many requests, and the per-request gap shrinks. heso's clearest wins are one-shot workloads, fleet-scale fan-out, and any case where memory or binary size matter.

## Demo

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

Live fetch, parse, JS eval — five real titles in under 400 ms from a 7.65 MB binary.

## Measured numbers

| Metric | heso | Notes |
|---|---|---|
| Install size | 7.65 MB | one static binary |
| Cold start (`--help`) | 15 ms | |
| One-shot per-URL wall-clock | 125 ms (mean of 8) | static + server-rendered + framework docs sites |
| Peak RSS after a 14-site run | 17 MB | |
| Workspace tests | 895 / 0 fail / 7 ignored | |

**vs. Playwright + Chromium, one-shot only**: 2.69× faster mean wall-clock on the same 8 URLs (heso 125 ms vs Playwright 336 ms), same machine, same network. This compares cold-start to cold-start — production Playwright setups that hold a persistent context across many requests amortize Chromium's startup and the per-request gap narrows. The honest framing: heso's speed advantage is in one-shot workloads (CI scrapers, edge functions, CLI tools, one-off agent calls) and at fleet scale where memory matters. Numbers and reproduction in [`bench/playwright/RESULTS.md`](bench/playwright/RESULTS.md).

**vs. jsdom + node-fetch**: not benchmarked head-to-head yet. jsdom is the obvious comparison for "minimal DOM environment" and has more years of compat work behind it. heso's differentiation is the deployment shape (one Rust binary vs `node_modules`) and the reproducibility/audit story (signed receipts, seeded RNG, virtual clock) — not raw compat breadth. If your workload is "fetch + parse + DOM read", jsdom probably handles it today; heso makes sense when you also need single-binary deploy or signed traces.

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

Same input → same hash, byte for byte, on any machine. The hash is a receipt: sign it, store it, diff it later to prove what the page said when your agent acted on it.

## Examples

**JS that mutates the DOM, on a real fetched page:**
```console
$ heso eval-dom https://example.com \
    'document.querySelector("h1").textContent = "Hijacked"; document.body.innerHTML.slice(0, 80)'
→ "<div><h1>Hijacked</h1><p>This domain is for use in documentation examples withou"
```

**Top-level `await` + `heso.flush()` for framework re-renders:**
```js
// Pass this as the <js> arg to heso eval-dom. The IIFE returns a
// Promise; heso awaits it via its microtask pump and serializes the
// resolved value. `await heso.flush()` yields to whatever the
// framework (Preact / React) queued for re-render.
(async () => {
    const input = document.querySelector('.new-todo');
    input.value = 'buy milk';
    input.dispatchEvent(new Event('keydown'));
    await heso.flush();           // let the framework's render microtask run
    return document.querySelector('.todo-list').innerHTML;
})()
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

`Math.random`, `crypto.getRandomValues`, `crypto.randomUUID` all route through a seeded ChaCha20 PRNG. Same seed, byte-identical output across machines.

**Sites as filesystems:**
```sh
heso tree https://stripe.com
heso ls   https://stripe.com /pricing
heso cat  https://stripe.com /pricing/business
```

The page is a tree of heading-defined sections. Navigate it like a directory.

**Stateful replay with a content-hashed trace:**
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

`trace_id` is a BLAKE3 Merkle chain over the URL + canonical actions. The same trace replayed elsewhere produces the same hash — no shared keys, server, or clock. Replay carries one `JsSession` across every step: DOM mutations persist, `addEventListener` handlers fire, `setTimeout` chains progress through the virtual clock, `e.preventDefault()` on `<a href>` clicks stops navigation as on a real SPA.

**Stateful sessions over stdio:**
```sh
heso serve     # JSON-RPC 2.0, persistent DOM across calls
```

Point Browser Use, Stagehand, or your own agent loop at the stdio transport. The session keeps DOM mutations, listeners, and form state alive across `fill` → `click` → `submit` cycles.

## Use as a Claude Code (or other harness) skill

heso is designed to be a tool an LLM agent calls. The cleanest integration is the skill/tool markdown pattern that Claude Code, Cursor, Aider, Cline, and similar harnesses support — a markdown file describing when to invoke the tool and which verbs are available.

Drop the following into `.claude/skills/heso/SKILL.md` (or the equivalent path in your harness):

```markdown
---
name: heso
description: Use the heso headless browser (one 7.65 MB Rust binary, no Chromium, no Node) for any task that needs to fetch a real web page, parse it, run its JavaScript, extract content, navigate, fill forms, or click links. Default to this over WebFetch + WebSearch when the workflow needs a DOM, stateful clicks, or framework-rendered content. Output is structured JSON.
---

# heso — agent-native browser

## When to invoke me

- "Get the content / title / metadata of <URL>"
- "Extract <thing> from <page>" — table data, links, prices, dates, anything in the DOM
- "Fill out this form and submit" — `find` then `fill` then `submit`
- "Search this site for X and follow the first result"
- "Run this JavaScript against <page> and tell me what it returns"
- "Click the @e0 / @e1 / … action and tell me what changes"
- Any task where Playwright would be overkill or where determinism matters

## Verbs (each prints JSON to stdout — pipe through `jq` if needed)

- `heso open <url>` — full page summary: `title`, `metadata`, `actions: [{ref: "@eN", role, name}]`, content-hashed `plat_hash`
- `heso meta <url>` — just the metadata block (OpenGraph, JSON-LD)
- `heso find <url> [--role link|button|input|form] [--name "regex"]` — locate an actionable element, returns its `@eN` ref
- `heso click <url> @e<N>` — dispatch click on action @eN
- `heso fill <url> @e<N> "value"` — type into input @eN (fires `input` + `change`)
- `heso submit <url> @e<N>` — submit form @eN (walks the form, fires `submit`)
- `heso eval-dom [--js-fetch] [--seed N] <url> "<js>"` — fetch URL, parse, run inline `<script>`s, then eval your JS. Pass `-` to read JS from stdin. `--js-fetch` enables in-JS `fetch()`. Inline JS can `await heso.flush()` to yield to framework re-render microtasks.
- `heso tree <url>` / `heso ls <url> <path>` / `heso cat <url> <path>` — navigate the page as a directory of heading-defined sections
- `heso eval-js [--seed N] "<js>"` — sandboxed JS only, no DOM
- `heso serve` — JSON-RPC 2.0 over stdin/stdout for multi-step sessions where state must persist across calls

## Recipes

**Read a page:** `heso open https://example.com | jq '.title, .description'`

**Extract structured data:** `heso eval-dom https://news.ycombinator.com 'Array.from(document.querySelectorAll(".titleline > a")).slice(0,5).map(a => ({title: a.textContent, url: a.href}))'`

**Drive a framework:** `heso eval-dom --js-fetch https://app.example.com '(async () => { document.querySelector(".search").value = "claude"; document.querySelector(".search").dispatchEvent(new Event("input")); await heso.flush(); return document.querySelector(".results").textContent; })()'`

**Determinism on tap:** add `--seed 42` to any verb — `Math.random`, `crypto.getRandomValues`, `crypto.randomUUID`, `Date.now`, `setTimeout` all become reproducible.

## What I can't do

No canvas, WebGL, computed CSS layout, or video. If you need rendered pixels or a full modern bundler-SPA execution path, use Chromium.
```

That's it. The harness routes "go look at this URL" / "fill this form" / "scrape this page" calls to `heso` instead of spinning up Chromium-via-Playwright, and the rest of the agent code doesn't change.

Works the same way in any harness that supports tool/skill markdown — Cursor, Aider, Cline, custom MCP wrappers, langchain tool definitions, the OpenAI Assistants API's function-calling, etc. The verbs are the contract.

## Who this is for

- Edge / serverless / CI environments where bundling Node + Chromium hurts more than narrower modern-SPA compat does.
- RAG pipelines, archival workflows, and compliance use cases that need a signed, reproducible record of "what the page said when the agent acted on it."
- Wide-crawl or fan-out workloads where 17 MB per worker beats 100+ MB per browser context.
- One-shot agent calls (single-URL fetch + DOM read + maybe a click) where cold-start dominates total wall-clock.

Probably not the right fit for: deep React 19 / Turbopack-Next.js interaction loops today (those are work-in-progress — see Status), long-lived persistent-context Playwright sessions (Playwright amortizes startup), or anything needing rendered pixels.

## Status

Pre-alpha. The feature surface is broad for the age of the project, but the modern-SPA compat gap is real — see the 🚧 rows. Shippable today for static + server-rendered + simple-SPA workloads; not yet a default for production scraping against React 19 / Turbopack-chunked Next.js.

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
| **Top-level `await` + `heso.flush()`** — eval awaits returned Promises via microtask pump; user can yield to render scheduler | ✅ |
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
| **`MutationObserver` / `IntersectionObserver` / `ResizeObserver` / `PerformanceObserver`** — noop ctors with spec method surface; unblocks SPAs that init observers in hydration | ✅ |
| **WHATWG `URLSearchParams`** — `get`/`getAll`/`set`/`append`/`delete`/`has`/`sort`/`size`/iteration, with parent-URL reflection back into `url.toString()` | ✅ |
| **`history.pushState` / `replaceState` / `back` / `forward` / `go`** with synchronous `popstate` dispatch, cached `location` reference identity preserved | ✅ |
| **WHATWG `Blob` / `File` / `Headers` / `FormData`** — multipart serializer wired into `fetch()` (file uploads round-trip) | ✅ |
| **`HTMLFormElement` IDL** — `.method` / `.action` / `.enctype` / `.elements` / `.submit()` / `.reset()`, plus `document.scripts` / `forms` / `images` / `links` / `anchors` collections | ✅ |
| **`HTMLAnchorElement.href` + url-decomposition mixin** — `.protocol` / `.host` / `.hostname` / `.pathname` / `.search` / `.hash` reflect | ✅ |
| **`<script type="module">`** — real ES module loader (rquickjs `Module::declare` + custom HTTP-backed `Loader`), per-URL cache, cyclic-import safe | ✅ |
| **Import map parser** — WHATWG §8.1.3.5 `<script type="importmap">`, bare/scoped/longest-prefix resolution | ✅ |
| **Dynamic `import()`** — `globalThis.import` shim with pluggable resolver seam | ✅ |
| **`heso serve` multi-step sessions** — JSON-RPC `fill` / `click` / `submit` / `eval` / `navigate` methods persist DOM mutations + listeners across calls | ✅ |
| **895 workspace tests, 7 ignored** | ✅ |
| Recorded-network playback (cassettes) for byte-identical replay | 🚧 designed |
| Import-map wired into static `<script type="module">` resolver (parser ships, wire-up pending) | 🚧 next |
| Turbopack chunk env detection for Next.js-bundled SPAs | 🚧 next |
| SVG namespace tracking + `tagName` casing | 🚧 next |
| React 19 full interaction round-trip — `KeyboardEvent` / `InputEvent` / `MouseEvent` ctors, focus tracker | 🚧 weeks |
| Real `document.cookie` jar (shared with `reqwest`) | 🚧 weeks |

## The precedent

[jsdom](https://github.com/jsdom/jsdom) (~50k LOC of JS) and [happy-dom](https://github.com/capricorn86/happy-dom) (~30k LOC) showed that a minimal DOM + JS environment handles the agent-relevant half of the web. Both have more years of compat work than heso and broader Node-ecosystem support; for many workloads they're the right tool today. heso bets on a different deployment shape (one Rust binary, no Node runtime) and on reproducibility as a first-class engine concern. Whether those tradeoffs are worth the current compat delta depends entirely on your target sites and deployment constraints — be honest about that when picking.

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
