# heso

**Site:** [heso.ca](https://www.heso.ca) · **Docs:** [heso.ca/docs](https://www.heso.ca/docs) · **[npm](https://www.npmjs.com/package/@ixla/heso)** · **[PyPI](https://pypi.org/project/heso/)** · **[Releases](https://github.com/blank3rs/heso/releases)**

A Rust runtime that lets an agent touch the web — fetch a page, run its JavaScript, query the resulting DOM, click, fill, submit, hold a session — and return JSON. Every run can be stamped into a signed file (a *plat*) that replays byte-identically off-network.

One binary. No Chromium, no Node.

<!-- heso:perf:start -->
```
binary       10.11 MB
cold start   ~77 ms   (open https://example.com, network included)
engine only  ~28 ms   (no network)
batch        ~1.1 s   for 8 URLs in parallel
```
<!-- heso:perf:end -->

[![heso agent demo — 50 second screen recording](https://raw.githubusercontent.com/blank3rs/heso/main/demo/poster.jpg)](https://www.heso.ca/#demo)

A 50-second recording: an LLM agent drives heso to compare two GitHub repositories by star count and README description, then stamps the run into a plat (tamper one byte and the hash flags it). [▶ Watch on heso.ca](https://www.heso.ca/#demo)

## Contents

- [Install](#install)
- [The 60-second tour](#the-60-second-tour)
- [What works today](#what-works-today)
- [What doesn't](#what-doesnt)
- [Why not just use X?](#why-not-just-use-x)
- [Use as a library](#use-as-a-library)
- [Plats: stamp, run, verify](#plats-stamp-run-verify)
- [Signed receipts](#signed-receipts)
- [Error handling](#error-handling)
- [Plug into agent harnesses](#plug-into-agent-harnesses)
- [Use as an agent skill](#use-as-an-agent-skill)
- [Stats](#stats)
- [Building from source](#building-from-source)
- [Status](#status)
- [License](#license)

## Install

```sh
# Python (uv, pipx, or pip)
uv tool install heso          # or: pipx install heso  /  pip install heso

# Node
npm install -g @ixla/heso     # or one-shot: npx @ixla/heso open https://example.com

# Direct binary installers
# macOS / Linux:
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/blank3rs/heso/releases/latest/download/heso-cli-installer.sh | sh

# Windows:
powershell -ExecutionPolicy Bypass -c "irm https://github.com/blank3rs/heso/releases/latest/download/heso-cli-installer.ps1 | iex"
```

<!-- heso:version:start -->
> Shipping `v0.1.4` for Windows-x64, Linux x64 + arm64, macOS x64 + arm64. `cargo-dist` builds every target on tag; npm/PyPI publish through the same workflow.
<!-- heso:version:end -->

## The 60-second tour

The shortest path to structured data on a page is `eval-dom`: fetch the URL, run its `<script>` tags against a DOM, then evaluate your own JS against the result and get JSON back.

```sh
heso eval-dom https://news.ycombinator.com '
  Array.from(document.querySelectorAll(".athing")).slice(0, 5).map(row => ({
    rank: row.querySelector(".rank")?.innerText,
    title: row.querySelector(".titleline > a")?.innerText,
    href: row.querySelector(".titleline > a")?.href,
  }))
'
# → JSON array of the top 5 stories.
```

For the broader "give me everything" view there's `read`:

```sh
heso read https://nextjs.org/
# → { title, text, actions, forms, cookies, console, framework,
#     content_hash, partial, partial_reason, http_status, ... }
```

`actions` is the list of clickable / fillable elements, numbered `@e0`, `@e1`, ... You point `click` / `fill` / `submit` at those refs:

```sh
heso click  https://news.ycombinator.com --text "more"
heso fill   https://example.com/search @e0 "rust"
heso submit https://example.com/search @form1 --field q=rust
```

Search the web (DuckDuckGo HTML + Wikipedia, no API key) and fan out:

```sh
heso search "rust web scraping" --limit 5
heso batch  read url1 url2 url3 --parallel 2
```

## What works today

**Most second-tier sites Just Work.** Default UA is `heso/<version>` — honest about what it is, no fingerprint impersonation, no residential-proxy farm. That happens to slip past a lot of WAF heuristics tuned for Playwright, Puppeteer, curl-impersonate, and headless-Chrome traffic. In practice, sites that go through cleanly on a vanilla call include Zillow (DataDome), Walmart (PerimeterX), CoinGecko (Cloudflare), LinkedIn anonymous pages, TripAdvisor, Yahoo Finance, and Reddit via `old.reddit.com`.

**eval-dom is the structured-extraction primary.** Fetch + run page scripts + run your JS against the DOM + return JSON. One round trip closes most "scrape this list" tasks without click choreography. `--seed N` makes `Math.random` / `crypto.getRandomValues` / `crypto.randomUUID` deterministic. `--js-fetch` lets the page's own `fetch()` and `<script src=...>` run through the same `reqwest::Client`, so cookies and recorded requests stay coherent.

**Fetch, navigate, observe.** `open` returns a summary (title, headings, action graph). `read` adds post-hydration text, grouped forms, cookies, console, framework detection, scripts. `read --complete` keeps firing observers and clicking load-more buttons until the DOM stops changing — for lazy-loaded sites. `batch [open|read] <urls...>` runs many URLs in one process with a shared cookie jar and connection pool.

**Wait for SPA conditions.** `heso wait <url>` with `--selector-exists`, `--text-contains`, `--url-matches`, `--network-idle`, or `--time` (advances the deterministic virtual clock). No polling loop in your code.

**Interact.** `click` / `fill` / `submit` accept either `@eN` refs or locator flags (`--text`, `--selector`, `--aria-label`). A click on an `<a href>` follows the link — the response carries the destination page's `title`, `tree`, `actions`, and `http_status`, not the source page.

**Stateful sessions.** `heso serve` is JSON-RPC 2.0 over stdio. Cookies, DOM mutations, listeners, and history persist across calls.

**Recover from broken sites.** `--best-effort` on `open`, `read`, and `wait` exits 0 with a `partial: true` envelope (`partial_reason`, `failed_scripts: [...]`, `console_errors_count`) when scripts crash. `--inject-script "<js>"` or `--inject-script @file.js` runs your code before the page's, useful for shimming a missing global.

**Cross-call deltas.** `read` returns a `content_hash`. Pass `--since <prev_hash>` and you get a `delta` describing what changed (`actions_added`, `actions_removed`, `forms_changed`, `text_changed`, `title_changed`).

**Honest about HTTP and bot walls.** Every response carries `http_status`, captured pre-body-consumption so 4xx/5xx pages never come back wearing a 200 mask. Bodies that contain Cloudflare's `__cf_chl_opt` JS shim or a `<title>` starting with one of nine well-known WAF phrases ("Just a moment…", "Attention Required", "Access Denied", "Verify you are human", "Checking your browser", "One moment, please", and a few variants) surface as `partial_reason: "bot_challenge"` regardless of wrapper status. The detection is intentionally narrow — false positives are worse than misses — so many bot walls come back as `http_403` or `http_429` and you should treat those as bot-walled by default rather than retrying the same request.

**Web platform coverage.** `XMLHttpRequest` (sync + async, same client as `fetch`), `performance.mark` / `performance.measure`, `document.getElementsByClassName` / `getElementsByName` / `getElementsByTagName`, 60+ `HTMLElement` subclass constructors (`new HTMLDivElement()` works, `instanceof HTMLScriptElement` works), `element.style = "color: red"` string-coercion setter, `data:` URL fast path in `<script src>`. `MutationObserver` and `IntersectionObserver` fire on real DOM mutations and viewport intersections. `setTimeout` and `setInterval` accept the 1-arg form per WHATWG HTML. Classic `<script>` runs sloppy-mode (so sites that use top-level `var` work); ES modules (`<script type="module">`) stay strict per ECMA-262.

## What doesn't

- **No rendering.** No canvas, WebGL, CSS layout, video. If the meaning is in pixels, use a real browser.
- **Full Cloudflare Challenge Mode and Imperva interstitials still block.** The narrow bot-challenge detection catches them — exit data is honest, exit content is empty. No CAPTCHA solver.
- **Service Workers, WebRTC, WebUSB, WebBluetooth.** Not implemented.
- **QuickJS, not V8.** Modern Next.js / React / Vue / Svelte / SSR sites generally run; some JS that depends on V8-specific behavior won't.
- **Sibling-script cascades we haven't shimmed.** When script A sets `window.X` and script B reads it on first load, the crash is surfaced. Use `--inject-script` for the polyfill.

## Why not just use X?

Partial overlap everywhere; no exact shelf neighbor. The win is not "smaller browser" — it is a smaller failure surface when the task is structured data, not pixels.

| Layer | Examples | What they ship | Gap vs heso |
|---|---|---|---|
| **Full Chromium stack** | [Playwright](https://playwright.dev/), [Puppeteer](https://pptr.dev/), [Browser Use](https://github.com/browser-use/browser-use), [Stagehand](https://www.browserbase.com/stagehand), [Skyvern](https://github.com/Skyvern-AI/skyvern) | V8 + full browser; often an AI planner on top | Heavy deps, opaque failures, no native JSON verb surface, no plat replay |
| **Smaller browser engine** | [Lightpanda](https://lightpanda.io/) | Zig engine, V8, CDP — drop-in for Playwright/Puppeteer | Still a browser mental model; agents drive it via CDP/wrappers, not verbs; no plat/cassette/receipt story |
| **Scraper APIs** | Firecrawl, Jina Reader, Crawl4AI | Fetch + extract markdown/JSON | Weak or no real click/fill/submit; often no honest partial-failure envelope |
| **DOM simulators (Node)** | [jsdom](https://github.com/jsdom/jsdom), [happy-dom](https://github.com/capricorn86/happy-dom) | Minimal DOM + JS in JS | Test harnesses, not agent products; no CLI, no plat, no session |
| **Built-in fetch tools** | WebFetch (Claude), curl | Static HTML, no JS hydration | No DOM events, no forms, no session |
| **heso** | this repo | QuickJS + agent-shaped DOM + verbs + plats | QuickJS ≠ V8; CAPTCHAs and Challenge Mode still stop you |

## Use as a library

The Python (`heso`) and Node (`@ixla/heso`) packages ship the same bundled binary with two faces: a CLI on `$PATH` and a programmatic API that spawns the binary under the hood and returns parsed JSON as native objects. No FFI, no Python extension module, no N-API addon — subprocess + JSON is the contract.

```python
import heso

page    = heso.open("https://example.com")
results = heso.search("rust web scraping", limit=5)
content = heso.read("https://example.com", complete=True)
data    = heso.eval_dom("https://news.ycombinator.com", "document.title")

with heso.session() as s:
    s.open("https://example.com")
    s.click(text="More information...")
    page = s.read()
```

```js
import { open, search, read, evalDom, session } from "@ixla/heso";

const page    = await open("https://example.com");
const results = await search("rust web scraping", { limit: 5 });
const content = await read("https://example.com", { complete: true });
const data    = await evalDom("https://news.ycombinator.com", "document.title");

await session(async (s) => {
  await s.open("https://example.com");
  await s.click({ text: "More information..." });
  const page = await s.read();
});
```

Per-language idioms: Python is `snake_case` + sync, Node is `camelCase` + Promises. Full API at **[heso.ca/docs](https://www.heso.ca/docs)**.

## Plats: stamp, run, verify

A *plan* is a JSON array of canonical actions (`open`, `click`, `fill`, `submit`). A *plat* is the observation produced by running that plan, plus an embedded network *cassette* — every (method, URL, request-body) → (status, headers, response-body) tuple the engine touched. Four verbs close the loop:

- `heso stamp <plan.json>` executes the plan against the live web and mints a plat that embeds the plan, the cassette, and a per-step log. Exit 0 on a clean run, 1 if any step failed (prints the partial plat with `error` + `steps`).
- `heso run <plat>` re-executes the embedded plan against the embedded cassette. **No network.** For an unchanged cassette the output `plat_hash` equals the input's — byte-identical replay ([ADR 0008](decisions/0008-deterministic-execution.md)). Cassette miss (page drifted since stamp) surfaces as `cassette miss: METHOD URL not recorded` and exits 1 — graceful, never silent.
- `heso replay <plat>` reads the recorded step log. Pure observation; no engine, no JS, no network. With `--plan`, it emits just the `plan` field so you can edit it and pipe back into `stamp`.
- `heso refresh <plat>` re-stamps against the live web and reports drift. Emits `{ok, drifted, input_plat_hash, live_plat_hash, diff?}`. Exit 0 unchanged / 1 drifted / 2 usage error.

```sh
cat > plan.json <<'EOF'
[
  {"verb": "open",   "url": "https://news.ycombinator.com/"},
  {"verb": "click",  "ref": "@e3"},
  {"verb": "fill",   "ref": "@e7", "value": "claude"},
  {"verb": "submit", "ref": "@form1"}
]
EOF
heso stamp plan.json > out.plat                     # plan → plat (records cassette)
heso run out.plat > replay.plat                     # plat → plat (off-network, byte-identical)
heso replay out.plat                                # plat → step log (pure read)
heso replay --plan out.plat > plan-again.json       # plat → plan (edit, restamp)
```

The plat's `plat_hash` (BLAKE3 over canonical JSON via RFC 8785) commits to the plan, the observation, AND the cassette. Tamper any of them and the hash no longer matches; `heso verify` will say so. Two different `<url>` inputs always produce different `plat_hash` values — the URL is part of the hashed canonical bytes, and a regression test in `crates/heso-engine-fetch/src/plat.rs::tests` pins that invariant against future drift.

**Inspect a plat:**

```sh
heso info   out.plat                       # summary: hash, plan / cassette / steps counts, sealed status
heso info   before.plat after.plat         # diff (plan, cassette URLs, fields, url / title / description)
heso seal   my.plat > sealed.plat          # wrap in Ed25519 envelope
heso unseal sealed.plat                    # verify; exit 0 valid / 1 invalid / 2 wrong-alg or malformed
heso unseal sealed.plat --extract          # verify, then print the inner plat body
```

`seal` produces a `SealedPlat` JSON envelope (`{alg, content, signature}`) that any holder of the envelope + the `heso` binary can verify offline — no key material, no network, no clock. Mint a key once with `heso identity init`; the same key signs every plat from then on. `unseal` checks the algorithm tag, the embedded `plat_hash`, and the Ed25519 signature in order, and refuses to silently treat an unknown `alg` as Ed25519.

**Replay a published plat in one command.** Install `heso`, then:

```sh
curl -sL https://github.com/blank3rs/heso/releases/download/v0.0.10/replay-demo-1-goldfinger.plat.json \
  | heso run - \
  | jq -r .plat_hash
# → d93c08ba32b762dd6e47091a1d4bd4aa4d8308dbdbf44869f81146a3f5b8033a   (under heso 0.0.10)
```

That hash is BLAKE3 over the canonical bytes of the resulting plat. Anyone, any machine, any time — same hash, given the same binary. The cassette inside the plat carries every HTTP response the engine touched at stamp time. No network is involved in `heso run` itself.

The three demo plats below were stamped by `heso 0.0.10` and reproduce only under that binary — the canonical-JSON shape has evolved since (HESO/1.0 §5 nails the v0.2+ shape down). To reproduce the published hashes, pin the matching version: `pipx install 'heso==0.0.10'` or `npm install -g @ixla/heso@0.0.10`.

- [`replay-demo-1-goldfinger.plat.json`](https://github.com/blank3rs/heso/releases/download/v0.0.10/replay-demo-1-goldfinger.plat.json) — Wikipedia `Goldfinger_(film)` (1 MB plat, hash `d93c08ba…`)
- [`replay-demo-2-torvalds-bio.plat.json`](https://github.com/blank3rs/heso/releases/download/v0.0.10/replay-demo-2-torvalds-bio.plat.json) — Wikipedia `Linus_Torvalds` (1 MB plat, hash `27e66b0d…`)
- [`replay-demo-3-rust-lang-rust.plat.json`](https://github.com/blank3rs/heso/releases/download/v0.0.10/replay-demo-3-rust-lang-rust.plat.json) — `github.com/rust-lang/rust` (640 KB plat, hash `201e9410…`)

## Signed receipts

Every `heso open` / `heso read` call can emit a **signed receipt** alongside its JSON output — an Ed25519-signed envelope describing what was run, what came back, and the BLAKE3 trace hash. The recipient verifies the signature against an allowlist of trusted public keys (or rejects the receipt). Per [ADR 0005](decisions/0005-ed25519-identity.md) + [ADR 0008](decisions/0008-deterministic-execution.md).

One-time setup — generate a local Ed25519 identity:

```sh
heso identity init
# → {"path": "heso-local-data/identity.key", "public_key": "fdibx2...IE=", "algorithm": "Ed25519"}
```

Sign a receipt on every call by passing `--receipt PATH`:

```sh
heso open https://example.com/ --receipt receipt.json
# stdout: the normal page JSON
# receipt.json (sibling file):
# {
#   "trace": [{"op": "cd", "target": {"kind": "url", "url": "https://example.com/"}}],
#   "results": [{"op": "cd", "url": "https://example.com/"}],
#   "trace_hash": "7e501fac...",
#   "seed": 0, "mode": "deterministic", "cost": {...},
#   "signature": {"algorithm": "Ed25519", "public_key": "fdibx2...IE=", "signature": "bNBb...Cg=="}
# }
```

Verify the receipt and bind it to a trusted signer with `--trusted-keys`:

```sh
echo '["fdibx2rLqGfrIf+duGbRKlM1iPwVSynHUq+nEisjwIE="]' > trusted.json

heso verify --trusted-keys trusted.json receipt.json
# → OK fdibx2rLqGfrIf+duGbRKlM1iPwVSynHUq+nEisjwIE=
# exit 0
```

`HESO_TRUSTED_KEYS=<path>` works as an env-var alternative.

Verify rejects three classes of receipt:

```sh
# 1. Tampered — any byte change invalidates the signature
sed -i 's/"seed": 0/"seed": 999/' receipt.json
heso verify --trusted-keys trusted.json receipt.json
# → INVALID: signature verification failed       (exit 1)

# 2. Wrong signer — well-formed but the pubkey isn't allowlisted
heso verify --trusted-keys other_keys.json receipt.json
# → INVALID: signing pubkey `...` is not in the trusted-keys allowlist   (exit 1)

# 3. mode: live — live runs use real time + real network and aren't
#    replay-safe, so the signature has no replay value (ADR 0008)
heso open https://example.com/ --receipt live.json --mode live
heso verify --trusted-keys trusted.json live.json
# → INVALID: receipt `mode: live` is not replay-safe — per ADR 0008 ...   (exit 1)
```

Verify without an allowlist still works for backwards compatibility but emits a stderr warning so the missing trust anchor isn't silent.

Exit codes: `0` valid + (allowlist empty OR pubkey allowlisted), `1` invalid (tampered, wrong signer, `mode: live`), `2` missing/malformed receipt or `--trusted-keys` load failure.

## Error handling

Both libraries throw a structured error (`HesoError` in Python, `HesoError extends Error` in Node) when the binary exits non-zero. Fields on the error tell you what to retry:

```python
import heso
try:
    page = heso.read("https://shoelace.style")
except heso.HesoError as e:
    print(e.returncode, e.stderr[:200])
```

```js
import { read, HesoError } from "@ixla/heso";
try {
  const page = await read("https://shoelace.style");
} catch (e) {
  if (e instanceof HesoError) {
    console.error(e.code, e.stderr.slice(0, 200));
  }
}
```

For sites that crash some scripts, use `best_effort` / `bestEffort` instead — heso exits 0 with a `partial: true` envelope so you handle the failure as data, not an exception:

```python
page = heso.read("https://shoelace.style", best_effort=True)
if page["partial"]:
    print("got partial:", page["partial_reason"], page["failed_scripts"])
```

## Plug into agent harnesses

heso is harness-agnostic. The same package serves five integration patterns:

| Harness style | How heso fits |
|---|---|
| **Python frameworks** (LangChain, Pydantic AI, LangGraph, smolagents, AgentScope) | `import heso`. Each function returns a `dict`. Wrap with `@tool` / `Tool(...)` / a function schema. |
| **Node / TS frameworks** (Mastra, Vercel AI SDK, LangGraph.js, Stagehand, Browser Use TS) | `import { open, search } from "@ixla/heso"`. All async; types in `index.d.ts`. |
| **Skill-markdown harnesses** (Claude Code, Cursor, Aider, Cline, Continue, Windsurf) | Drop the manifest in [Use as an agent skill](#use-as-an-agent-skill) into the harness's skills directory. `heso` on PATH does the rest. |
| **CLI-spawning harnesses** | `heso <verb> ...` outputs JSON on stdout. Standard subprocess; no special integration. |
| **Long-running JSON-RPC harnesses** | `heso serve` is JSON-RPC 2.0 over stdin/stdout. Cookies + DOM state persist across calls. |

The verbs are the contract (see [ADR 0017](https://github.com/blank3rs/heso/blob/main/decisions/0017-verbs-as-agent-surface.md)) — no heso-specific framework dependency, no adapter layer.

**HESO/1.0** is an open protocol; the `heso` binary is one implementation. Spec lives at [`spec/HESO-1.0.md`](spec/HESO-1.0.md). It defines the core verb set every conformant implementation must dispatch. Beyond the core, anyone can define a verb under a domain they control, reverse-DNS style:

```json
{"verb": "com.example.scrape-pricing", "url": "https://example.com/products"}
{"verb": "org.archive.warc-import",    "path": "./snapshot.warc"}
```

Dispatch is local-only (spec §4.4) — receiving a plat with an unknown extension verb is a structured error, never a network fetch or a code download. Today the reference binary ships only the core verbs; typing `heso com.example.foo ...` exits with `unknown subcommand`. Extension verbs are a namespace, not yet a registered-impl surface in this binary.

DNS ownership prevents anyone but you from claiming names under your domain — same anti-impersonation model as Java packages, Android application IDs, Maven groups, OCI image labels. It does not solve typosquatting. HESO/1.0 anchors trust on signing keys, not verb names: pin receivers to trusted signers via the existing `verify --trusted-keys` allowlist (spec §3.9, §4.6).

## Use as an agent skill

heso is built to be a tool an agent calls, not a library a human drives. The cleanest integration is the skill-markdown pattern that Claude Code, Cursor, Aider, Cline, and similar harnesses use:

```markdown
---
name: heso
description: Use heso when an agent needs to touch the web — fetch pages, run JavaScript, click buttons, fill forms, get structured JSON back. Every run can be stamped into a signed, byte-identically replayable plat — proof of what the agent saw and did. One Rust binary; no Chromium, no Node. Prefer this over WebFetch when you need a DOM, stateful clicks, framework-rendered content, or a verifiable artifact.
---

## Verbs

- `heso search "<query>" [--limit N]` — web search via DDG + Wikipedia
- `heso open <url>` — page summary
- `heso read <url> [--complete]` — full content + actions + forms
- `heso eval-dom <url> "<js>"` — run JS against the post-hydration DOM, get JSON
- `heso wait <url> --selector-exists ".x"` — block until a condition is true
- `heso batch [open|read] <urls...> [--parallel N]` — parallel scrape
- `heso click <url> --text "..." | --selector "..." | @eN` — click
- `heso fill <url> @eN "value"` — type into input
- `heso submit <url> @eN` — submit form
- `heso serve` — multi-step JSON-RPC session
- `--best-effort` on open/read/wait — exit 0 on partial failures, surface what broke
- `--inject-script "<js>" | @file` — inject a polyfill before page scripts run
```

Same shape works in any harness that does tool or skill markdown.

## Stats

Measured on Windows 11, AMD x86_64, with the release binary. Cold-start and JS-engine-init numbers come from `scripts/bench.ps1` (average of 10 runs after three warm-ups); the others are real wall-clock against the live network so they vary with the route to GitHub / DDG.

| Thing | Number |
|---|---|
| Binary size | 10.11 MB |
| Cold start (help banner, no engine) | ~10 ms |
| JS engine init (`eval-js null`) | ~21 ms |
| `open https://example.com` (network included) | ~80 ms |
| Batch (8 URLs, `--parallel 8`) | ~1.1 s total |
| Search (DDG, 5 results) | ~1 s |

## Building from source

Prebuilt binaries for Windows x64, Linux x64+arm64, macOS x64+arm64 ship from each release tag. To hack on heso itself:

```sh
git clone https://github.com/blank3rs/heso
cd heso
cargo build --release -p heso-cli
./target/release/heso search "rust web scraping" --limit 5
```

Requires Rust 1.90 (`rustup` from https://rustup.rs).

## Status

`v0.1.4` is shipping on every registry. The engine, the verbs, and plat replay are stable enough to use — spot checks across the second-tier sites listed above come back clean, and the `heso-engine-js` lib-test suite (265 tests) is required green on every release. What may still shift before `v1.0` is the CLI surface: verb names, JSON field names, flag spellings. Pin the version if you embed it.

## License

MIT or Apache-2.0, your choice.

---

Full docs: **[heso.ca/docs](https://www.heso.ca/docs)** · Site: **[heso.ca](https://www.heso.ca)** · npm: **[@ixla/heso](https://www.npmjs.com/package/@ixla/heso)** · PyPI: **[heso](https://pypi.org/project/heso/)**
