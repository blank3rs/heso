# heso JS engine stress test — bug report

Run date: 2026-05-21
heso binary: `target/release/heso.exe` (head: `0830292 identity: Ed25519 signed receipts (item H)`)
rquickjs: 0.11.x via workspace, features `classes, properties, macro, loader, disable-assertions`
Test command shape: `timeout 60 ./target/release/heso.exe read URL --include console,framework,scripts`
Sites covered: 21 real-world URLs spanning React, Vue, Svelte, Astro, Next.js, vanilla, jQuery, browserify-UMD, ESM-only, classic vintage SSR.

## TLDR

The QuickJS host runtime is rock-solid: 21/21 sites returned exit 0, zero panics, zero hangs, zero stderr bytes from heso itself. The historical astro.build / vercel.com GC assert is dead — `disable-assertions` + the explicit `Drop` impl on `JsEngine` hold up under both serial (5x repeat) and parallel (`heso batch read --parallel 5`) load.

What broke the most pages was **not** the engine — it was missing/incomplete *web API surface*. One single root-cause defect (P0) — heso evaluates every `<script>` body in **strict mode by default**, while browsers default classic `<script>` to sloppy mode — produced visible script crashes on apple.com, wikipedia.org, and any page that ships browserify-style UMD with a bare `require=function(){...}` top-level assignment. The remaining bugs are concrete API gaps: `XMLHttpRequest`, `TextEncoder`/`TextDecoder`, `HTMLScriptElement`, `HTMLVideoElement`, `XPathEvaluator`, `ReadableStream`, `document.location` (only `window.location` is wired), `performance.mark`/`measure`, `requestIdleCallback`, `structuredClone`, `Request`/`Response`/`fetch` in eval-dom-without-`--js-fetch`, etc.

Two other engine-level findings: (1) **`setTimeout(fn)` with no delay argument throws** — fathom, GitHub Apple-CMS, and most analytics SDKs rely on this and crash; (2) **the `eval-dom` Promise-drain does NOT drive `setTimeout`/`requestAnimationFrame` callbacks** — so `new Promise(r => setTimeout(() => r('x'), 0))` resolves to `null`. Microtask-resolved promises do work.

Hydration coverage is genuinely happening — linear.app and tailwindcss.com both end with **more** text after the script pump than a raw curl, and Next.js / React.dev / Vue / Svelte all post-hydrate to within ~10% of raw HTML text size despite our cascade of script failures.

## Heavy-SPA results

| Site | Open? | Read? | Console errors | Framework detected | Heso post-hydration text vs raw |
|---|---|---|---|---|---|
| react.dev                | ok | ok |  2 | next.js (correct)            |  6,989B vs  8,136B raw   |
| nextjs.org               | ok | ok |  0 | next.js (correct)            |  6,894B vs  7,140B raw   |
| linear.app               | ok | ok |  1 | next.js (correct)            | 27,998B vs  9,484B raw (+) |
| vercel.com               | ok | ok |  1 | next.js (correct)            |  6,117B vs  7,126B raw   |
| astro.build              | ok | ok |  2 | astro (correct)              |  5,106B vs  6,607B raw   |
| svelte.dev               | ok | ok |  2 | svelte (correct)             |  2,181B vs  2,134B raw   |
| vuejs.org                | ok | ok |  1 | vue (correct)                |  1,720B vs  2,175B raw   |
| stripe.com               | ok | ok |  0 | next.js (correct)            | 11,172B vs 12,701B raw   |
| tailwindcss.com          | ok | ok |  0 | next.js (correct)            | 10,214B vs  7,985B raw (+) |
| figma.com                | ok | ok |  1 | next.js (correct)            |  5,794B vs  5,356B raw   |
| cloudflare.com           | ok | ok |  9 | astro (correct)              |  6,148B vs  7,100B raw   |
| play.tailwindcss.com     | ok | ok |  2 | next.js (correct)            |    n/a                   |
| github.com               | ok | ok | 71 | vanilla (correct)            |    n/a                   |
| news.ycombinator.com     | ok | ok |  1 | vanilla (correct)            |  4,425B vs  3,977B raw   |
| en.wikipedia.org         | ok | ok |  4 | vanilla (correct)            | 67,617B vs 68,012B raw   |
| developer.mozilla.org    | ok | ok |  1 | vanilla (acceptable; Yari)   |    n/a                   |
| solidjs.com              | ok | ok |  1 | vanilla (WRONG; is SolidJS)  |    n/a                   |
| htmx.org                 | ok | ok |  4 | vanilla (correct)            |    n/a                   |
| esbuild.github.io        | ok | ok |  0 | vanilla (correct)            |    n/a                   |
| apple.com                | ok | ok |  6 | apple-cms (correct)          |    n/a                   |
| discord.com              | ok | ok |  7 | vanilla (acceptable)         |    n/a                   |
| shopify.com              | ok | ok |  3 | vanilla (acceptable; Remix)  |    n/a                   |
| notion.so                | ok | ok |  0 | next.js (correct)            |  2,679B vs  4,229B raw   |
| x.com                    | ok | ok |  0 | vanilla (acceptable)         |    n/a                   |
| reddit.com               | ok | ok |  0 | vanilla (BOT-WALLED; n/a)    |    n/a                   |
| microsoft.com            | ok | ok |  4 | vanilla (BOT-WALLED; n/a)    |    n/a                   |

(+) means heso captured *more* post-hydration text than the raw HTML — real hydration is firing.

## Bug list

| Severity | Site(s) | JS-layer symptom | Stderr / error excerpt | Repro |
|---|---|---|---|---|
| **P0** | apple.com, wikipedia.org, browserify-bundled scripts everywhere | Classic `<script>` runs in strict mode; bare top-level assignments throw `<name> is not defined` | `RLCONF is not defined` (wikipedia); `require is not defined` at `ac-target.js:461` (apple — file literally starts `require=function(){...`) | `./target/release/heso.exe eval-js "MY_BARE_ASSIGN = 1; MY_BARE_ASSIGN"` → `MY_BARE_ASSIGN is not defined` |
| **P0** | astro.build, vuejs.org, apple.com, vercel.com | `setTimeout(fn)` with **only 1 arg** throws | `Error calling function with 1 argument(s) while 2 where expected` at fathom `script.js:4655` (the literal source is `setTimeout(function(){window.fathom.trackPageview()})`). Same shape blocks Apple's globalheader.umd.js. | `./target/release/heso.exe eval-js "setTimeout(() => {}); 'ok'"` → throws; `setTimeout(()=>{},0)` works |
| **P1** | linear.app, github.com, figma.com | `HTMLScriptElement` not defined | linear.app: webpack chunk `webpack-574f57cb768cfeac.js` crashes with `HTMLScriptElement is not defined at a (eval_script:44:12278)` | `./target/release/heso.exe eval-js "typeof HTMLScriptElement"` → `undefined` |
| **P1** | cloudflare.com, vercel.com | `XMLHttpRequest` not defined; vercel saw `Error patching XMLHttpRequest:` in console | otSDKStub.js: `XMLHttpRequest is not defined at <anonymous>(eval_script:1:11400)` | `typeof XMLHttpRequest` → `undefined` |
| **P1** | figma.com (next-chunk), every page with `npm:textencoder` polyfill probe | `TextEncoder` / `TextDecoder` not defined | `webpack-b6dc02567cc552d3.js` crashes with `TextEncoder is not defined at 49781 (eval_script:1:103122)` | `typeof TextEncoder` → `undefined` |
| **P1** | shopify.com | `ReadableStream` not defined (Remix runtime streams the SSR response) | `ReadableStream is not defined at <eval>(eval_script:1:386)` followed by two cascading `cannot read property 'enqueue'/'close' of undefined` | `typeof ReadableStream` → `undefined` |
| **P1** | htmx.org | `XPathEvaluator` not defined — htmx uses XPath selector path | `XPathEvaluator is not defined at <anonymous> (eval_script:2764:27)` from `htmx.js:5156` | `typeof XPathEvaluator` → `undefined`; `typeof document.evaluate` → `undefined`; `typeof document.createRange` → `undefined` |
| **P1** | cloudflare.com | `HTMLVideoElement` not defined (used by inline hero-video hydration code) | `HTMLVideoElement is not defined at <anonymous>(eval_script:100:30) at <eval>(eval_script:125:3)` twice (different astro islands) | `typeof HTMLVideoElement` → `undefined`; also `typeof Image, typeof Audio, typeof Option` undefined |
| **P1** | All sites doing performance instrumentation (github.com `js-parse-end:high-contrast-cookie...`) | `performance.mark` / `performance.measure` not defined; only `performance.now` exists | github.com: 71 console errors, all `not a function at <eval>(eval_script:1:12)` — every chunk starts with `performance.mark("js-parse-end:...")` | `./target/release/heso.exe eval-dom https://example.com "typeof performance.mark"` → `undefined` |
| **P1** | htmx.org/buttons.js, jQuery-bundle on discord.com | `document.location` not wired (only `window.location`) | buttons.github.io: `cannot read property 'protocol' of undefined at <anonymous>(eval_script:6:18897)` — the source reads `document.location.protocol`. jQuery hits the same pattern as `cannot read property 'createElement' of undefined`. | `./target/release/heso.exe eval-dom https://example.com "typeof document.location"` → `"undefined"`; `window.location.protocol` works |
| **P1** | All `eval-dom` outputs that depend on `setTimeout` to resolve a Promise | Promise drain does NOT advance setTimeout/rAF callbacks | `eval-dom example.com "new Promise(r => setTimeout(() => r('x'), 0))"` → `value: null` (drain didn't pump macrotasks). `queueMicrotask` and `Promise.resolve` chains DO resolve correctly. `setTimeout` is documented in `--help` as deterministic-clock-advance-only-via-wait, but the eval-dom drain not pumping it is undocumented and a real footgun. | `./target/release/heso.exe eval-dom https://example.com "new Promise(r => setTimeout(() => r('m_ok'), 0))"` → `null`; same shape with `queueMicrotask` → `'m_ok'` |
| **P2** | wikipedia.org/discord.com/all polyfilled sites | Wikipedia's `RLCONF=...` shape is technically the strict-mode bug above, but the cascade leaves `RLQ` and the entire MediaWiki ResourceLoader pipeline disabled. 4 console errors. | (same as the strict-mode P0; called out separately because the cascade is severe) |
| **P2** | discord.com (cdn.localizeapi.com) | `no setter for property` — a defineProperty on a non-configurable descriptor (likely a window.* assignment heso has wired read-only) | `no setter for property at <anonymous>(eval_script:1:19229)` — exact API not yet pinned down. Could be `window.localStorage` or one of the DOM bridge properties. | manual repro pending; see `/tmp/heso-bug/discord.json` `failed_scripts[0]` |
| **P2** | astro.build, vercel.com (`/.netlify/scripts/rum`), discord.com (webflow `discord-2022....js`), google.com (search homepage inline scripts), apple.com (multiple) | Generic `not a function` and `cannot read property X of undefined` — caused by missing DOM globals upstream (Image, HTMLElement subclasses, MediaQueryList, etc.). Symptoms are downstream of the P1 API gaps. | repros in `/tmp/heso-bug/{astro,discord,google,apple}.json` |
| **P2** | linear.app `next.js` runtime | Console reports a single "client-side exception" (next.js error overlay text) — visible as `level: error` with redacted body. Indicates Next.js hydration aborted but heso silently produced an action graph anyway. | `./target/release/heso.exe read https://react.dev --include console` (similar shape on react.dev) |
| **P3** | All scripts | Stack frames show source name `eval_script:N:M` for every script — confusing when 71 scripts crash, you can't tell which file is which in the trace. The script URL is captured in `failed_scripts[].url`, but the inner stack frames lose that context. | grep `/tmp/heso-bug/github.json` `.failed_scripts` — message says `eval_script:1:12` 71 times |
| **P3** | solidjs.com | Framework sniff reports `vanilla` instead of `solid` — SolidJS official homepage is SolidJS. (`fw=vanilla` in batch table) | `./target/release/heso.exe read https://www.solidjs.com --include framework` |

## DOM / Web API gaps

Captured via `eval-dom https://example.com "typeof X"` probes plus error messages from real sites. Anything missing here that a real site needed is included.

Globals **missing** (typeof undefined):

- `XMLHttpRequest` — cloudflare otSDKStub, lots of analytics SDKs
- `TextEncoder` / `TextDecoder` — webpack 5 runtime, Next.js chunks (figma)
- `ReadableStream` / `WritableStream` / `TransformStream` — Remix SSR streams (shopify)
- `Request` / `Response` — fetch API constructors (only the verb is wired, not the types)
- `HTMLScriptElement` — linear webpack runtime checks `instanceof HTMLScriptElement` to find its own bootstrapper
- `HTMLVideoElement` — cloudflare hero-video hydration (other Element subclasses are likely also missing; only base `Element` and `HTMLElement` exist)
- `HTMLAnchorElement`, `HTMLInputElement`, `HTMLFormElement`, `HTMLButtonElement`, `HTMLImageElement` — not tested but likely the same. Apps with `if (x instanceof HTMLXxxElement)` checks will all fail.
- `Image`, `Audio`, `Option` constructors — convenience constructors for `<img>` / `<audio>` / `<option>`
- `XPathEvaluator`, `document.evaluate`, `document.createRange`, `document.createTreeWalker`, `Range`, `TreeWalker` — htmx, libs that walk the DOM
- `DOMParser` — for HTML/SVG parsing in JS
- `WebSocket`, `EventSource` — real-time
- `requestIdleCallback` / `cancelIdleCallback` — many SDKs (Sentry, RUM) call this
- `navigator.serviceWorker`, `indexedDB`, `caches` — PWA basics
- `Intl` — internationalization (huge gap; almost every modern SPA uses `Intl.NumberFormat`/`DateTimeFormat`)
- `structuredClone` — modern object cloning
- `crypto.subtle` — Web Crypto subtle API (only `crypto.getRandomValues` is wired)
- `getComputedStyle`, `window.matchMedia`, `MediaQueryList` — responsive code paths
- `window.scrollTo`, `scrollBy`, `focus`, `alert`, `confirm`, `open`, `close`, `postMessage` — many libs probe these
- `NodeList`, `HTMLCollection` (as constructors; the instances returned by `querySelectorAll` are typed as plain arrays/objects)

Document / DOM **partials**:

- `document.location` — **MISSING**. Only `window.location` is wired. jQuery and many older libs use `document.location` interchangeably. (Real browsers: `document.location === window.location`.)
- `document.getElementsByClassName` — undefined (hn.js crashed at `byClass = el.getElementsByClassName(cl)`)
- `document.visibilityState`, `document.hidden`, `document.referrer`, `document.domain`, `document.URL`, `document.documentURI`, `document.baseURI`, `document.charset`, `document.compatMode`, `document.doctype` — all undefined
- `document.createComment` — undefined (Vue/React hydration emit comment markers)
- `performance.mark`, `performance.measure`, `performance.getEntriesByName`, `performance.getEntriesByType` — undefined (`performance.now` works; `PerformanceObserver` constructor exists)
- Stack traces in `failed_scripts[].message` are all `eval_script:N:M` — every script gets the same synthetic name. URL is captured in the outer record but inner frames lose it.

Surface that **works** (confirmed via probes):

- `document.querySelector` / `querySelectorAll`, `getElementsByTagName`, `getElementById`, `createElement`, `createElementNS`, `createDocumentFragment`, `createTextNode`
- `document.body.classList`, `document.head.appendChild`, `document.body.appendChild`
- `customElements`, `MutationObserver`, `IntersectionObserver`, `ResizeObserver`, `PerformanceObserver` (constructors all live; observe/disconnect typed)
- `requestAnimationFrame`, `queueMicrotask` (queueMicrotask drains; rAF does not advance from eval-dom — see P1 above)
- `atob` / `btoa`, `crypto.getRandomValues`, `URL`, `URLSearchParams`, `Blob`, `File`, `FormData`, `AbortController`, `AbortSignal`, `Headers`
- `localStorage`, `sessionStorage`, `history` (object), `window === globalThis`, `top` / `parent` / `self` all aliases of `window`
- Promise.finally, Promise.any, Promise.allSettled, async/await, microtask draining, deep Promise resolution

## Top 5 engine-level fixes (ranked)

### 1. Run classic `<script>` source in sloppy mode by default
**Rationale**: This single change would unblock apple.com, wikipedia.org, and every browserify/UMD bundle in the wild. Real browsers run classic `<script>` in sloppy mode; only `"use strict"`, `type=module`, and `class`/lexical-decl heads opt into strict. heso's `ctx.eval::<Value, _>(source)` in `crates/heso-engine-js/src/scripts.rs:1103` uses rquickjs's default which is strict. Switch to `ctx.eval_with_options` (or whatever rquickjs 0.11 exposes for the `STRICT` flag) and clear the strict bit for classic scripts. `eval-js` should also default to sloppy. ES-module scripts stay strict per spec.

### 2. Make `setTimeout(fn)` accept 1 arg, and pump macrotasks in the eval-dom Promise drain
**Rationale**: Two near-bugs that compound. `setTimeout(fn)` should default delay to 0 per HTML spec — fathom, Apple's globalheader.umd.js, and many polyfills depend on this. The fix is a one-liner in `crates/heso-engine-js/src/timers.rs`. The second half: the eval-dom Promise drain (referenced in the `--help` text as "deep-resolve Promises") only pumps microtasks; if the final value is a Promise resolved from a setTimeout callback, the drain returns null. This makes `await fetch(...)` work (because it's microtasks all the way down inside reqwest), but `new Promise(r => setTimeout(r, 0))` doesn't resolve. Even one pass of `advance_clock(0)` after each microtask burst would fix the common case. Document the exact contract once fixed (the help text says "Bare side-effect reads will NOT work" — the setTimeout case needs an equivalent footnote, or better, a fix).

### 3. Wire `document.location` as an alias of `window.location`
**Rationale**: One property, huge blast radius. jQuery and any pre-2015 lib treats them as interchangeable. buttons.github.io crashes today reading `document.location.protocol`. This is a 5-line change in `crates/heso-engine-js/src/dom.rs` — add a getter on the Document class that returns the session URL wrapped in the same Location prototype.

### 4. Add the missing HTMLElement subclass constructors and `instanceof` checks (`HTMLScriptElement`, `HTMLVideoElement`, `HTMLAnchorElement`, `HTMLInputElement`, etc.)
**Rationale**: Webpack 5 runtime, Next.js, React hydration, and Vue all do `instanceof HTMLScriptElement` checks to identify their bootstrap script. These don't have to be functional subclasses — they just need to be defined as constructors (callable, non-instantiable per spec) and have `Element.prototype` chain such that `currentScript instanceof HTMLScriptElement === true`. Pair the constructors with a small table mapping `<tag>` -> prototype so `document.createElement('script')` returns something whose `__proto__.constructor === HTMLScriptElement`. Same shape unblocks `Image`/`Audio`/`Option` and the analytics-SDK `HTMLVideoElement` checks on cloudflare.com.

### 5. Add `TextEncoder` / `TextDecoder` and `XMLHttpRequest` (shim only)
**Rationale**: Webpack 5 runtime instantiates a TextEncoder on bootstrap to hash chunks (figma broke here). It's an `encoding-rs`-backed shim, no DOM coupling. XMLHttpRequest is harder (full XHR is a lot of surface) but a minimal shim that delegates to `reqwest::Client` would unblock all the analytics SDKs that test `if (typeof XMLHttpRequest !== 'undefined')` and fall back to `fetch` when missing — except they don't fall back, they hard-crash trying to monkey-patch the XHR prototype (vercel's "Error patching XMLHttpRequest" console message is exactly this). A minimal "XMLHttpRequest exists as an empty class" shim might suffice for the polyfill-detection path; full method coverage can come later.

## Engine-stability findings (positive)

- **GC assert workaround holds**: 5x serial runs of astro.build, 5-way parallel batch of (astro, vercel, linear, nextjs, react.dev) — zero panics, zero stderr from heso, all rc=0.
- **Hydration genuinely runs**: linear.app gains 18kB of post-hydration text vs raw HTML; tailwindcss.com gains 2kB; react.dev / next.js / vue / svelte / stripe all hydrate to within ~10% of raw HTML text size despite having script failures.
- **Promise / async / await / Promise.allSettled / Promise.any** all work correctly through the drain.
- **fetch() redirects** through `--js-fetch` work (`/redirect/2` -> final URL captured correctly).
- **Parallel batch (5 sites)** completes in ~12s wall clock, no stderr noise.
- **Framework sniffer** correctly identifies Next.js, Astro, Vue, Svelte, and the Apple CMS on 22/26 real sites. SolidJS is misclassified as vanilla.

## Files referenced

- `C:\Users\Akshay\Documents\projects\heso\crates\heso-engine-js\src\scripts.rs:1101-1123` — `eval_one`, where the strict-vs-sloppy fix needs to land.
- `C:\Users\Akshay\Documents\projects\heso\crates\heso-engine-js\src\timers.rs` — setTimeout arity check.
- `C:\Users\Akshay\Documents\projects\heso\crates\heso-engine-js\src\dom.rs` — `document.location` alias, missing HTMLElement subclasses.
- `C:\Users\Akshay\Documents\projects\heso\crates\heso-engine-js\src\engine.rs` — Promise drain (already pumps microtasks; needs at least one macrotask pass).
- `C:\Users\Akshay\Documents\projects\heso\crates\heso-engine-js\Cargo.toml:28-44` — context on the GC assert and `disable-assertions`.

Captured artifacts (all in `C:\Users\Akshay\AppData\Local\Temp\heso-bug\`):
`react.json, nextjs.json, linear.json, vercel.json, astro.json, vuejs.json, svelte.json, stripe.json, tailwind.json, figma.json, cloudflare.json, github.json, ycomb.json, wikipedia.json, mdn.json, solidjs.json, htmx.org.json, esbuild.json, apple.json, discord.json, shopify.json, notion.json, twitter.json, reddit.json, msft.json, tailwindplay.json, redirect.json`.
