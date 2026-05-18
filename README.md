# heso

> Headless browser for the agent-relevant half of the web. Single Rust binary, no Chromium, no Node. Returns structured JSON with content-hashed receipts.

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

Five real story titles, off the wire, from a 9.1 MB binary, in under 400 ms. No Chromium, no `npm install playwright`, no Node. Just `cargo build && ./heso`.

## The bet

The browser-agent stack today — Browser Use, Stagehand, Skyvern, Operator — wraps Playwright wrapping Chromium. Chromium is bloat for the agent use case: the rendering pipeline (Skia, Blink layout, compositor, GPU, video, WebGL, canvas) is roughly 70% of why headless Chromium ships at 180–240 MB. An agent reading a docs site, filling a login form, clicking through a checkout flow doesn't need any of that.

heso is what you get when you keep the boring half (fetch, parse, JS, DOM, cookies, forms, clicks, sessions) and drop the rendering half.

Each run produces a **plat** — a content-hashed, agent-shaped JSON map of the page. Same plat → same hash, every time. Verifiable, shareable, replayable.

## Numbers

| Metric | Value |
|---|---|
| Binary size (stripped release) | **9.1 MB** |
| Cold start (banner only, median of 10) | **41 ms** *(min 39, p95 52)* |
| Cold start + JS engine init (`eval-js '1+1'`, median) | **40 ms** *(min 38, p95 40)* |
| Full fetch + parse + extract (`fetch https://example.com`, median) | **85 ms** *(min 82, p95 86)* |
| Full DOM eval over network (`eval-dom news.ycombinator.com`, median of 5) | **396 ms** *(min 379)* |
| Workspace lib tests | **291 passing** |
| Idle RAM | TBD (not benchmarked) |

Headless Chromium for comparison: ~240 MB on disk, 1–2 s cold start, 100+ MB idle RAM.

## What it looks like

Real bytes, fresh `cargo build --release && ./heso`.

### JS reaches into the DOM

```console
$ heso eval-dom https://example.com \
    'document.querySelector("h1").textContent + " | " + document.querySelectorAll("a").length + " links"'
{
  "console": [],
  "ok": true,
  "url": "https://example.com/",
  "value": "Example Domain | 1 links"
}
```

### Mutations actually mutate

```console
$ heso eval-dom https://example.com \
    'document.querySelector("h1").textContent = "Hijacked"; document.body.innerHTML.slice(0, 80)'
{
  "console": [],
  "ok": true,
  "url": "https://example.com/",
  "value": "<div><h1>Hijacked</h1><p>This domain is for use in documentation examples withou"
}
```

### Events + AbortController, on a real page

```console
$ heso eval-dom https://example.com \
    'const t = new EventTarget(); let seen = null;
     t.addEventListener("hi", (e) => seen = e.detail);
     t.dispatchEvent(new CustomEvent("hi", { detail: { ok: true, count: 7 } }));
     const c = new AbortController(); c.abort("done");
     ({ listener_saw: seen, abort_state: c.signal.aborted, reason: c.signal.reason })'
{
  "console": [],
  "ok": true,
  "url": "https://example.com/",
  "value": {
    "abort_state": true,
    "listener_saw": { "count": 7, "ok": true },
    "reason": "done"
  }
}
```

### Click, fill, submit — wired to the JS event model

```console
$ heso click https://example.com/ @e0
{
  "console": [],
  "ok": true,
  "op": "click",
  "ref": "@e0",
  "selector": "a[href=\"https://iana.org/domains/example\"]",
  "url": "https://example.com/",
  "value": true
}
```

`fill` mutates `.value` and fires both `input` and `change`. `submit` walks the form and clicks its first submit control. The same `@e0/@e1/…` refs `heso find` and `heso open` hand back are how you address an element across a click/fill/submit cycle.

### Same seed, same bytes

```console
$ heso eval-js --seed 42 'Math.random()'
{ "console": [], "ok": true, "value": 0.5140492957650241 }

$ heso eval-js --seed 42 'Math.random()'
{ "console": [], "ok": true, "value": 0.5140492957650241 }

$ heso eval-js --seed 99 'Math.random()'
{ "console": [], "ok": true, "value": 0.5052084295432834 }
```

`--seed N` reroutes `Math.random`, `crypto.getRandomValues`, and `crypto.randomUUID` through a seeded ChaCha20 PRNG. Same seed, byte-identical output. Works on `eval-dom` too. `Date.now` / `new Date()` are the one remaining nondeterminism source; clock-seeding is next.

### Metadata extract — JSON-LD, OpenGraph, the lot

```console
$ heso meta https://stripe.com
{
  "canonical": "https://stripe.com/en-ca",
  "jsonld": [
    {
      "@type": "Organization",
      "@id": "https://stripe.com/#organization",
      "description": "Stripe powers online and in-person payment processing and financial solutions for businesses of all sizes.",
      "founders": [{"@type": "Person", "name": "Patrick Collison"}, ...]
    }
  ],
  "opengraph": { "site_name": "Stripe", ... },
  ...
}
```

Often answers "what does this company do" with zero LLM tool calls.

### Action graph — every clickable thing, stable refs

```console
$ heso find https://news.ycombinator.com --role link --name "more"
{
  "count": 1,
  "matches": [
    {
      "attrs": { "href": "?p=2", "rel": "next" },
      "name": "More",
      "ref": "@e220",
      "role": "link",
      "section": "/",
      "tag": "a"
    }
  ]
}
```

### Content-hashed plats — verifiable, replayable

```console
$ heso open https://example.com > plat.json
$ heso plat-hash plat.json
abf42bb66917095eb4cafdd4deb00c0686835102e713a3342b32093578007289
$ heso plat-verify plat.json
OK abf42bb66917095eb4cafdd4deb00c0686835102e713a3342b32093578007289
```

BLAKE3 over the canonical-JSON serialization. Same inputs, same hash, every machine.

## Why you'd use it

- **RAG ingestion of docs sites.** Point heso at `docs.foo.com/`, get a tree of sections, intros, and inline-script hydration data. One parse per page, no headless Chromium farm to operate.
- **Drop-in headless-Chromium replacement for agent frameworks.** `heso serve` speaks JSON-RPC 2.0 over stdin/stdout — Browser Use, Stagehand, and friends can swap the transport without rewriting the loop.
- **Deterministic page snapshots for tests and audits.** Capture a plat, store the hash, re-fetch later, `plat-verify` catches any drift in the page content.
- **Compliance / archival scraping where receipts matter.** Every fetch produces a content-hashed artefact you can sign and pin. Ed25519 signing arrives next; the hash is already there.
- **Lightweight competitive-intel jobs.** Metadata + tree + action graph in ~100 ms each — wide crawls fit on one machine, no infrastructure.

If your data is locked behind canvas, video, computed CSS layout, WebGL, or service workers, heso isn't for you. Use a real browser. That's fine.

## Comparison

| | heso | Playwright + Chromium | Browser Use / Stagehand / Skyvern |
|---|---|---|---|
| Single binary, no install | ✓ 9.1 MB | ✗ ~240 MB + Node + browser | ✗ Playwright + Python/Node + LLM |
| JavaScript execution | ✓ QuickJS | ✓ V8 | ✓ V8 |
| Full CSS layout / canvas / WebGL | ✗ (the bet) | ✓ | ✓ |
| Content-hashed page artefacts | ✓ | ✗ | ✗ |
| Deterministic by construction | mostly (clock + RNG seeded; `Date.now` next) | ✗ | ✗ |
| Signed audit receipts | planned | ✗ | ✗ |
| Render pixels / screenshots of layout | ✗ | ✓ | ✓ |
| Cold start | 40 ms | 1–2 s | 1–2 s |

heso loses every cell where the rendering pipeline matters. That's the trade.

## What's in (the agent-relevant half)

| Capability | Status |
|---|---|
| HTTP/HTTPS, redirects, cookies | done (`reqwest`) |
| HTML parse | done (html5ever via `dom_query` / `scraper`) |
| Sandboxed JS evaluation | done (QuickJS via `rquickjs`) |
| Read-only DOM (`querySelector`, `textContent`, `getAttribute`…) | done |
| DOM mutations (`setAttribute`, `textContent =`, `innerHTML =`, `appendChild`, `classList`…) | done |
| Events (`addEventListener`, `dispatchEvent`, `CustomEvent`, `AbortController`, `DOMException`) | done |
| Timers (`setTimeout`/`setInterval`, deterministic virtual clock) | done |
| Click / submit / fill primitives wired to event dispatch | done (`heso click/fill/submit <url> @ref`) |
| Seeded RNG shims (`Math.random`, `crypto.getRandomValues`, `crypto.randomUUID`) | done (`--seed N`) |
| Page-load `<script>` execution (SPA hydration) | next |
| `Date.now` / `new Date()` seeding | after `<script>`-on-load |
| `fetch()` inside JS | 1–2 weeks (proxy `reqwest` into QuickJS) |
| Listener persistence across CLI calls | needs `<script>`-on-load + session model |
| `localStorage` / `sessionStorage` | days |
| Multi-page sessions | designed in (`page_id` in `heso serve`) |
| Ed25519 signed receipts | planned |

## What's out (and that's the bet)

- Canvas pixels, WebGL, Three.js demos, Figma. Agents don't need this.
- Video / audio playback.
- WebRTC.
- Service Workers (most agent sites don't depend on SW).
- Real CSS layout, animations, transitions.

## Precedent

[jsdom](https://github.com/jsdom/jsdom) (~50k LOC of JavaScript) and [happy-dom](https://github.com/capricorn86/happy-dom) (~30k LOC) both prove a minimal DOM + JS environment handles the agent half of the web. Both are slow because they are JS-in-JS, used mostly for testing, never shipped as a product aimed at agents. Doing it in Rust against QuickJS is the obvious next move and nobody has shipped it. There is a real gap on the shelf.

## Try it

```sh
cargo build --release -p heso-cli

# agent-shaped JSON: title, description, metadata, tree, actions, plat_hash
./target/release/heso open https://example.com

# cartography V0 — page + same-origin link sub-trees, ~0.5–1 s
./target/release/heso open --explore-links 1 --link-cap 20 https://docs.rs/

# JS in a sandbox — no DOM
./target/release/heso eval-js '1 + 1'

# JS against a fetched DOM — real pages, real querySelector
./target/release/heso eval-dom https://news.ycombinator.com \
  "Array.from(document.querySelectorAll('.titleline > a')).slice(0,5).map(a => a.textContent)"

# JSON-RPC server over stdin/stdout — for framework integration
./target/release/heso serve
```

## Subcommands

- **`heso open <url>`** — agent payload: `{ url, title, description, metadata, tree, actions, inline_data, data_attrs, plat_hash }`. One parse, many extractors.
- **`heso open --explore-links N <url>`** — pre-fetches same-origin links and embeds each sub-page's tree under `linked_pages`. Filters cross-origin, fragments, `mailto:`, cycles.
- **`heso meta <url>`** — JSON-LD, OpenGraph, Twitter cards, `<meta>`, canonical, icons, `<html lang>`. Sorted, deterministic.
- **`heso tree <url>` / `heso ls <url> [path]` / `heso cat <url> <path|@ref>`** — the page as a filesystem of heading-defined sections. `cat` is polymorphic: tree path or `@e7` action ref.
- **`heso find <url> [--role link|button|…] [--name SUBSTR] [--section /pricing]`** — interactive elements with stable `@e0/@e1/…` refs (ARIA-role aware).
- **`heso click <url> @ref`** — fetch `<url>`, resolve `@ref` against the action graph, dispatch a real `click` event through `addEventListener`. Handlers fire.
- **`heso fill <url> @ref <value>`** — set the input's `.value` and fire both `input` and `change`, matching browser typing behavior.
- **`heso submit <url> @form-ref` ** — find the form, click its first `button[type=submit]` / `input[type=submit]` descendant. (A real `reqwest::post` of the serialized form lands when sessions do.)
- **`heso eval-js [--seed N] <js>`** — sandboxed QuickJS, `console.*` capture, typed exceptions. No DOM. `--seed N` reroutes `Math.random` / `crypto.getRandomValues` / `crypto.randomUUID` through ChaCha20 (default 0).
- **`heso eval-dom [--seed N] <url> <js>`** — fetch + parse + run JS against `document`. Live on real pages. Same `--seed` flag.
- **`heso serve`** — long-running JSON-RPC 2.0 over stdin/stdout. Stateful page cache keyed by `page_id`.
- **`heso fetch <url>`** — low-level `{ url, text }`.
- **`heso plat-hash <file>` / `heso plat-verify <file>`** — BLAKE3 over canonical JSON. Exit codes for scripts and CI.

## What's not real yet

- **`<script>` on page load.** The DOM, JS engine, events, timers, click/fill/submit dispatch, and seeded RNG are all in. heso does not yet execute the page's own `<script>` tags during `open`. SPA-mounted content (the stuff that's empty until React/Vue hydrates) is still invisible. Next major lift; in flight.
- **`fetch()` inside JS.** No `XMLHttpRequest` either. Page scripts that talk to an API will throw `fetch is not defined`. Two items out — proxies through `reqwest` so the engine's cookie jar stays coherent.
- **`Date.now` / `new Date()` seeding.** `Math.random`, `crypto.getRandomValues`, `crypto.randomUUID` are routed through `--seed N` today; the clock is the one remaining nondeterminism source. Lands alongside `<script>`-on-load.
- **Listener persistence across CLI calls.** Each `heso click` / `heso fill` / `heso eval-dom` re-parses HTML and re-installs `document` from scratch — a listener attached in one invocation isn't visible to the next. Closes when `heso serve` exposes a session model.
- **`heso submit` doesn't yet hit the network.** It clicks the form's first submit control through the event model. A real `reqwest::post` of the serialized form lands with sessions.
- **Cross-fetch ref stability.** `@e0/@e1/…` are stable within one fetch only.
- **Signed plats.** BLAKE3 content hash today; Ed25519 signing next.
- **`heso run <url> <request>`** — stub. Navigates only; the natural-language request isn't interpreted yet. Waits on the planner.

## Roadmap

**Now → 1 month:** Phase 1B is done — events, timers, `click/fill/submit` wired through `dispatchEvent`, seeded RNG shims. Phase 1C is next: run `<script>` on page load so SPAs actually hydrate, seed `Date.now` / `new Date()` to close the last nondeterminism source, ship `fetch()` inside JS. This is where heso starts working on real React/Vue pages.

**1 → 3 months:** cookies + storage, `fetch()` inside JS proxied through the native client (so the engine's cookie jar and audit receipts stay coherent), Ed25519 signed receipts, a planner v0. A 100-URL compatibility harness to keep regressions out.

**3 → 6 months:** the long tail. React/Vue compatibility passes against the harness, MCP server polish, packaging, docs site. By month 6, heso is a credible single-binary alternative to headless-Chromium-plus-Playwright for the agent half of the web.

## What makes it different

- **No Chromium dep.** Single Rust binary. `cargo build && ./heso`.
- **The plat is an artefact, not a session.** Every other agent-browser tool produces a live session — act, observe, decide. heso produces a serializable, content-hashed map. The same plat of `stripe.com/pricing` serves every agent.
- **Engine as semantic extractor.** The engine doesn't hand back raw HTML — it pre-extracts metadata, the heading tree, an action graph with ARIA-role-aware refs, inline-script hydration data (Next.js `__next_f`, Apple `__ACGH_DATA__`, Netflix `netflix.reactContext`, `window.X` assignments), `data-*` JSON payloads, and (with `--explore-links`) the cartography of linked sub-pages. Many views, one parse.
- **Deterministic by construction (where it counts).** Sorted maps, document-ordered vectors, BLAKE3 over canonical JSON, virtual clock for `setTimeout`/`setInterval`, seeded `Math.random` / `crypto.getRandomValues` / `crypto.randomUUID` under `--seed N`. `Date.now` / `new Date()` is the one remaining nondeterminism source; lands with Phase 1C.
- **Honest scope.** No layout, no paint, no canvas/WebGL, no workers, no IndexedDB, no CSS engine. heso runs the JS that handles clicks, fills forms, computes state. It does not run the JS that paints pixels.

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache 2.0](LICENSE-APACHE) at your option.
