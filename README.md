# heso

A small headless browser for scripts and agents that read web pages without needing them rendered. Fetches a URL, parses the HTML, runs the JavaScript, lets you click and fill forms, and returns the result as JSON. One Rust binary, around 8 MB. No Chromium, no Node.

## A note before you read further

Most of this codebase was written with help from Claude under one person's direction. The co-author tag is on basically every commit. It moved fast, which means the feature surface ran ahead of real usage. Treat the README as design intent plus working code — not as battle-tested production claims — until more people have actually tried it on real workloads.

## What works today

- Fetching pages, following redirects, cookies.
- HTML parsing.
- JavaScript via QuickJS, with a DOM the engine implements directly.
- `open`, `read`, `wait`, `click`, `fill`, `submit`, `eval`, `navigate`, `serve` — CLI verbs plus JSON-RPC for multi-step sessions.
- `heso read <url>` returns the page in one call: title, visible text, actionable elements, forms, cookies, console output, framework detection. One call, not five.
- `heso wait <url> --selector-exists "..."` (also `--text-contains`, `--url-matches`, `--network-idle`, `--time`) blocks until a condition is met. No polling loop.
- Stateful sessions where DOM mutations, listeners, and form state persist across calls.
- Real cookie jar shared between the HTTP layer and JS `document.cookie`. Login flows work — `Set-Cookie` on response → next `fetch()` sends it → `document.cookie` reflects it.
- Optional reproducibility: seed the random number generator, freeze the clock, and the same page processed the same way produces the same hash.
- Spec-correct events: `KeyboardEvent` / `InputEvent` / `MouseEvent` / `FocusEvent` constructors with full options. `heso fill` dispatches the focus → keydown → beforeinput → input → keyup → change cascade. `heso click` does mousedown → mouseup → click.
- Common modern JS surface: `fetch`, `URLSearchParams`, `history.pushState`, `Blob`/`File`/`FormData`, multipart upload.
- ES modules: `<script type="module">`, dynamic `import()`, import maps. Shared cache between the static and dynamic paths.
- Web Components: `customElements.define`, `HTMLElement` as a base class, `connectedCallback`/`disconnectedCallback`/`attributeChangedCallback` lifecycle, `HTMLTemplateElement`, `attachShadow`, `ShadowRoot`, `<slot>` with `assignedElements`. Late-upgrade re-prototyping works.
- Modern site coverage: `nextjs.org`, `vercel.com`, `stripe.com`, `supabase.com`, `posthog.com`, `bun.sh`, `astro.build`, `ui.shadcn.com`, `lit.dev/playground`, `hono.dev`, `linear.app` all hydrate (title + h1 + body content). Turbopack + RSC + heap headroom + the QuickJS iterator-helper crash workaround add up here.

## What doesn't

- No rendering. No canvas, WebGL, CSS layout, or video. If your agent needs pixels, use a real browser.
- Sites that depend on a sibling script having already set a global the next script reads. `shoelace.style` is the canonical case — its bundle dies on `window.lunr` not being defined yet. Each cascade is its own small polyfill and they're not all shimmed.
- Lazy-loaded content: `IntersectionObserver` and `MutationObserver` are noop constructors. Anything that defers rendering on visibility (infinite scroll, deferred images, observer-driven hydration) doesn't fire.
- SVG namespace handling is incomplete; sites that touch SVG via JS may break.
- Compatibility breadth is well behind jsdom. jsdom has had years to handle weird real-world JavaScript. This is early.

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

Fetch, parse, run JS, get five titles in under 400 ms.

## Quickstart

```sh
cargo build --release -p heso-cli
./target/release/heso open https://example.com
```

You get a JSON summary of the page: title, description, a tree of headings, and a list of clickable elements numbered `@e0`, `@e1`, and so on.

## Examples

Read everything about a page in one call:

```sh
heso read --js-fetch https://nextjs.org/
# → { "title": "...", "text": "...", "actions": [...], "forms": [...],
#     "cookies": [...], "console": [...], "framework": "next.js", ... }
```

Read structured data:

```sh
heso eval-dom https://news.ycombinator.com \
  'Array.from(document.querySelectorAll(".titleline > a")).slice(0,5).map(a => a.textContent)'
```

Find and click:

```sh
heso find  https://news.ycombinator.com --role link --name "more"   # → @e220
heso click https://news.ycombinator.com @e220
```

Or skip the find step entirely and click by visible text, CSS selector, or aria-label:

```sh
heso click https://news.ycombinator.com --text "More"
heso fill  https://news.ycombinator.com --selector "input[name=q]" "rust"
heso click https://news.ycombinator.com --aria-label "Close dialog"
```

`--text` and `--aria-label` match case-insensitive substrings (Playwright `get_by_text` / `get_by_role` semantics). Multiple matches return an "ambiguous: N matches" error listing every candidate's ref so the agent can pick by `@e<N>` on the retry.

Sites as filesystems:

```sh
heso tree https://stripe.com
heso ls   https://stripe.com /pricing
heso cat  https://stripe.com /pricing/business
```

Reproducibility:

```sh
heso eval-js --seed 42 'Math.random()'   # 0.5140492957650241
heso eval-js --seed 42 'Math.random()'   # 0.5140492957650241, every time
```

The same seed makes `Math.random`, `crypto.getRandomValues`, `crypto.randomUUID`, `Date.now`, and `setTimeout` produce the same output across machines.

Wait for a condition (no polling loop):

```sh
heso wait --js-fetch https://app.example.com/ --selector-exists ".dashboard" --timeout 5s
```

Multi-step session over stdio:

```sh
heso serve   # JSON-RPC; DOM persists across calls
```

Login flow end-to-end (real cookie jar carries the session):

```sh
# In a heso serve JSON-RPC session:
# 1. navigate to /login
# 2. fill user + pass
# 3. submit
# 4. wait for url-matches "/dashboard"
# 5. read everything on the dashboard
```

## Speed

For one-shot calls — fetch a single URL, get something out — heso is roughly 2.7× faster than Playwright plus Chromium on the same eight URLs, with around 17 MB of memory instead of 100+ MB. Numbers in [`bench/playwright/RESULTS.md`](bench/playwright/RESULTS.md).

Caveats worth knowing:

- The Playwright comparison is cold-start vs cold-start. In production, Playwright keeps the browser warm with a persistent context across requests, which eliminates most of heso's startup advantage.
- jsdom isn't benchmarked head-to-head yet. It's likely fast enough for many cases and definitely better at compatibility. heso's advantage there is single-binary deploy and reproducibility, not raw throughput.

## Use as an agent skill

heso is built to be a tool an agent calls, not a library a human drives. The cleanest integration is the skill / tool markdown pattern that Claude Code, Cursor, Aider, Cline, and similar harnesses use. A starter skill:

```markdown
---
name: heso
description: Use the heso headless browser (one Rust binary, no Chromium, no Node) to fetch a real web page, parse it, run its JavaScript, extract content, navigate, fill forms, or click links. Prefer this over WebFetch when you need a DOM, stateful clicks, or framework-rendered content.
---

## Verbs

- `heso open <url>` — page summary: title, metadata, actions, content hash
- `heso meta <url>` — metadata only (OpenGraph, JSON-LD)
- `heso find <url> [--role link|button|input|form] [--name "regex"]` — find an element
- `heso click <url> @e<N>` — click element @eN
- `heso click <url> --text "Submit"` — click by visible text (case-insensitive substring)
- `heso click <url> --selector "button.primary"` — click by CSS selector
- `heso click <url> --aria-label "Close"` — click by aria-label substring
- `heso fill <url> @e<N> "value"` — type into input @eN
- `heso fill <url> --selector "input[name=q]" "value"` — fill by locator (same flags as click)
- `heso submit <url> @e<N>` — submit form @eN
- `heso submit <url> --selector "form#login"` — submit by locator
- `heso eval-dom <url> "<js>"` — fetch URL, run scripts, then evaluate your JS against the resulting DOM
- `heso tree <url>` / `heso ls <url> <path>` / `heso cat <url> <path>` — navigate page sections
- `heso serve` — multi-step session over JSON-RPC stdio
```

The verbs are the contract. Same shape works in any harness with tool or skill markdown.

## How it compares

Not a replacement for either of these. Different tradeoffs.

Versus Playwright with Chromium: heso is smaller, uses less memory, starts faster, and runs on machines without a browser binary. Playwright renders pixels and works on every site.

Versus jsdom with Node: heso is a static binary, no `node_modules`, no Node runtime. jsdom has years of compatibility work that heso doesn't.

If your workload doesn't need single-binary deploy or content-hashed output, jsdom probably handles it better today.

## Status

Pre-alpha. Roughly two weeks of work. Built fast with LLM help, used by one person so far. Worth trying if the use case fits; not worth depending on for production yet.

Concrete next work, in rough order:

- A QuickJS GC teardown assertion that fires on a small number of pages (e.g. astro.build). The eval output is correct, but the engine aborts during drop. Real CI hazard, needs fixing.
- Turbopack-chunk detection for Next.js builds.
- SVG namespace and tag-name casing.
- Full `KeyboardEvent` / `InputEvent` / `MouseEvent` constructors so React 19 interactions round-trip cleanly.
- Real cookie jar shared between HTTP and `document.cookie`.
- `:host` and `::slotted()` CSS selectors for shadow-DOM-scoped queries.

## Try it

```sh
git clone https://github.com/Akshay-Dongare/heso
cd heso
cargo build --release -p heso-cli
./target/release/heso open https://example.com
```

## License

MIT or Apache-2.0, your choice.
