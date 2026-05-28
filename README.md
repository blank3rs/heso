# heso — the auditable layer for the agent web.

**Site:** [heso.ca](https://www.heso.ca) · **Docs:** [heso.ca/docs](https://www.heso.ca/docs) · **[npm](https://www.npmjs.com/package/@ixla/heso)** · **[PyPI](https://pypi.org/project/heso/)** · **[Releases](https://github.com/blank3rs/heso/releases)**

A Rust runtime that lets an agent touch the web — fetch, JavaScript, DOM, forms, clicks, sessions — and emits a signed, replayable record of what happened.

Every run can be **stamped** into a *plat* — a signed replay file holding the plan that ran, the page observation, and the recorded network cassette, all hashed together. `heso run` re-executes the plat off-network and the resulting `plat_hash` is byte-identical to the original. Tamper one byte and the hash flags it. Hand the artifact to an auditor.

Capabilities return JSON. Failures come back as structured data (`partial: true`, `bot_challenge`, cassette miss), not opaque browser crashes. One Rust binary; no Chromium, no Node.

<!-- heso:perf:start -->
```
binary       10.27 MB
cold start   ~77 ms   (open https://example.com, network included)
engine only  ~28 ms   (no network)
batch        ~1.1 s   for 8 URLs in parallel
```
<!-- heso:perf:end -->

[![heso agent demo — 50 second screen recording](https://raw.githubusercontent.com/blank3rs/heso/main/demo/poster.jpg)](https://www.heso.ca/#demo)

A 50-second real recording — an LLM agent (Gemini) drives heso to find and compare two GitHub repositories by star count and README description, then stamps the run into a verifiable plat (tamper one byte → the hash flags it). No Chromium, no rendering pipeline, no driver. [▶ Watch the full demo on heso.ca](https://www.heso.ca/#demo)

## Contents

- [Install](#install)
- [What it can do](#what-it-can-do)
- [What it can't do](#what-it-cant-do)
- [Why not just use X?](#why-not-just-use-x)
- [Use as a library](#use-as-a-library)
- [Examples](#examples)
- [Signed receipts](#signed-receipts)
- [Error handling](#error-handling)
- [Plug into agent harnesses](#plug-into-agent-harnesses)
- [Verbs are open](#verbs-are-open)
- [Use as an agent skill](#use-as-an-agent-skill)
- [Global flags](#global-flags)
- [Stats](#stats)
- [Building from source](#building-from-source)
- [Status](#status)
- [License](#license)

## Install

```sh
# Python (uv, pipx, or pip — any of them)
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
> Shipping `v0.1.8` for Windows-x64, Linux x64 + arm64, macOS x64 + arm64. `cargo-dist` builds every target on tag; npm/PyPI publish through the same workflow.
<!-- heso:version:end -->

After install, `heso` is on `$PATH`:

```sh
heso open https://example.com
# → { url, title, description, tree, actions, plat_hash, ... }
```

You get JSON: title, description, a heading tree, and a list of clickable elements numbered `@e0`, `@e1`, and so on.

## What it can do

**Find and read things.**

- `heso search "<query>"` — searches the web (DuckDuckGo + Wikipedia, optional SearXNG). No API key.
- `heso open <url>` — fetches and returns a page summary: title, headings, actionable elements.
- `heso read <url>` — fetches, runs JS, returns the full picture: title, visible text, actions, forms, cookies, console output, framework detection. One call.
- `heso read <url> --complete` — same, but heso loops "fire pending observers + click load-more + wait for DOM to settle" until the page stops changing. For lazy-loaded sites.
- `heso batch [open|read] <urls...>` — runs many URLs in parallel. Shared cookie jar, JSON-Lines out.
- `heso wait <url> --selector-exists ".foo"` (also `--text-contains`, `--url-matches`, `--network-idle`, `--time`) — blocks until a condition is true. No polling loop.

**Interact with sites.**

- `heso click <url> @e7` — click by element ref.
- `heso click <url> --text "Sign in"` — or by visible text, CSS selector, or aria-label.
- `heso fill <url> @e3 "hello"` — type into an input.
- `heso submit <url> @e9` — submit a form.
- `heso serve` exposes a JSON-RPC `navigate` method for changing URL inside a stateful session.
- `heso eval-dom <url> "<js>"` — fetch, run scripts, then run your JS against the resulting DOM.

**Bundle, edit, replay, and re-execute action sequences.**

A *plan* is a JSON array of canonical actions (`open`, `click`, `fill`, `submit`). A *plat* is an observation, plus an embedded network *cassette* — every (method, URL, request-body) → (status, headers, response-body) tuple the engine touched during the run. Four verbs close the loop:

- `heso stamp <plan.json>` — executes the plan against the live web and mints a fresh plat that embeds the plan, the recorded cassette, and a per-step log. Each entry in `steps` carries a three-way `status` (`ok` / `partial` / `error`), the verb-specific `observed` payload (the JSON shape the live verb would emit), a `partial_reason` token on degraded steps (`http_404`, `bot_challenge`, `selector_not_matched`, …), and deterministic logical `started_at` / `finished_at` timestamps. Accepts a bare `Action[]` array, a plat with a `"plan"` field, or a `TraceFingerprint`. Exit 0 on a clean run; 1 if any step's `status` is `error` (still prints the partial plat with `error` + `steps`).
- `heso run <plat.plat>` — re-executes the plan against the embedded cassette. **No network.** Replay runs under the seed recorded in the plat (HESO/1.0 §4), so a deterministic re-run reproduces the same DOM instead of diverging on a fresh seed; `--seed` overrides it. For an unchanged cassette the output `plat_hash` equals the input's — byte-identical replay. Also walks the recorded `steps` array and asserts each re-executed step's `status` and `observed` match what was recorded; a divergence (cassette mutated to make a previously-partial step succeed, or vice versa) surfaces on stderr with the step index and the diverging field, and `run` exits 1. If the cassette has drifted (page changed since stamping), the failing step carries a structured `cassette miss: METHOD URL not recorded` error and `run` exits 1 — graceful, never silent. Before replaying, `run` verifies the input plat's own `plat_hash` and refuses a tampered or corrupted plat up front (exit 1, `error.code: "plat_integrity_mismatch"`); pass `--no-verify-input` to skip the check.
- `heso replay <plat.plat>` — pure observation. Reads the recorded step log from the plat and prints it. No engine, no JS, no cassette lookup, no network. Use `run` if you want to re-execute.
- `heso replay --plan <plat.plat>` — extracts just the `plan` field. Edit it standalone and pipe back into `stamp` to re-mint a fresh plat (with a fresh cassette since the requests changed).

```sh
cat > plan.json <<EOF
[
  {"verb": "open",   "url": "https://news.ycombinator.com/"},
  {"verb": "click",  "ref": "@e3"},
  {"verb": "fill",   "ref": "@e7", "value": "claude"},
  {"verb": "submit", "ref": "@form1"}
]
EOF
heso stamp plan.json > out.plat           # plan → plat (records cassette)
heso run out.plat > replay.plat           # plat → plat (off-network, byte-identical)
heso replay out.plat                      # plat → step log (pure read, no execution)
heso replay --plan out.plat > plan-again.json    # plat → plan (edit, restamp)
```

The plat's `plat_hash` (BLAKE3 over canonical JSON via RFC 8785) commits to the plan, the observed content, the recorded seed, AND the embedded cassette. Tamper with any of them and the hash no longer matches; `heso verify` will say so. Two different `<url>` inputs always produce different `plat_hash` values — the URL is part of the hashed canonical bytes, and a regression test in `crates/heso-engine-fetch/src/plat.rs::tests` pins that invariant against future drift.

**Inspect a plat.** Text dev tools, all baked into the main binary:

```sh
heso info   out.plat                       # human summary: hash, plan / cassette / steps counts, sealed status
heso info   before.plat after.plat         # what changed (plan, cassette URLs, fields, url / title / description)
heso seal   my.plat > sealed.plat          # wrap in Ed25519 envelope (default key: heso-local-data/identity.key)
heso unseal sealed.plat                    # verify; exit 0 valid / 1 invalid / 2 wrong-alg or malformed
heso unseal sealed.plat --extract          # verify, then print the inner plat body for piping
```

`seal` produces a `SealedPlat` JSON envelope (`{alg, content, signature}`) that any holder of the envelope + the `heso` binary can verify offline — no key material, no network, no clock. Mint a key once with `heso identity init`; from then on the same key signs every plat. `unseal` checks the algorithm tag, the embedded `plat_hash`, and the Ed25519 signature in order, and refuses to silently treat an unknown `alg` as Ed25519.

**Replay a published plat in one command.** Install `heso` (`uv tool install heso` / `pipx install heso` / `npm install -g @ixla/heso`), then:

```sh
curl -sL https://github.com/blank3rs/heso/releases/download/v0.0.10/replay-demo-1-goldfinger.plat.json \
  | heso run - \
  | jq -r .plat_hash
# → d93c08ba32b762dd6e47091a1d4bd4aa4d8308dbdbf44869f81146a3f5b8033a
```

That hash is BLAKE3 over the canonical bytes of the resulting plat. Anyone, any machine, any time — same hash. The cassette inside the plat carries every HTTP response the engine touched when it was stamped against the live Wikipedia article. No network is involved in `heso run` itself.

Three sample plats live as release assets on v0.0.10:
- [`replay-demo-1-goldfinger.plat.json`](https://github.com/blank3rs/heso/releases/download/v0.0.10/replay-demo-1-goldfinger.plat.json) — Wikipedia `Goldfinger_(film)` (1 MB plat, hash `d93c08ba…`)
- [`replay-demo-2-torvalds-bio.plat.json`](https://github.com/blank3rs/heso/releases/download/v0.0.10/replay-demo-2-torvalds-bio.plat.json) — Wikipedia `Linus_Torvalds` (1 MB plat, hash `27e66b0d…`)
- [`replay-demo-3-rust-lang-rust.plat.json`](https://github.com/blank3rs/heso/releases/download/v0.0.10/replay-demo-3-rust-lang-rust.plat.json) — `github.com/rust-lang/rust` (640 KB plat, hash `201e9410…`)

**Recover from broken sites.**

- `--best-effort` on `open` / `read` / `wait` — exit 0 even when scripts crash. Output includes `partial: true`, `partial_reason: "script_crash" | "wait_timeout" | "fetch_failed" | "parse_error" | "bot_challenge" | "non_html_content_type" | "http_<code>"`, and `failed_scripts: [...]`. The agent sees what broke and decides what to try next.
- `--inject-script "<inline-js>"` or `--inject-script @file.js` — run JS before the page's own scripts. Use it to shim a missing global (the canonical `window.lunr` cascade kind of thing).

**Detect cross-call state changes.**

- `heso read` always returns a `content_hash`. Pass `--since <prev_hash>` to get a `delta` describing what changed (`actions_added`, `actions_removed`, `forms_changed`, `text_changed`, `title_changed`).

**Honest about failure.**

- Every `open` / `read` / `fetch` response carries `http_status` (200, 403, 503, ...) — captured pre-body-consumption so 4xx/5xx pages never come back wearing a 200 mask. Cloudflare-style "Just a moment..." interstitials (and Reddit-style "Please wait for verification" walls) are detected and surfaced as `partial_reason: "bot_challenge"`. A `200 OK` carrying a non-HTML body (PDF, JSON, octet-stream) surfaces as `partial_reason: "non_html_content_type"` rather than pretending the empty extraction was a real page. No more silent "I got something" when the server returned an error page or a binary blob.
- `heso click @e7` on an `<a href="...">` actually follows the link — the response carries the destination page's `title`, `tree`, `actions`, and `http_status`, not the source page. `final_url` reports where the navigation actually landed after following the destination's redirect chain, and `redirects[]` lists each `{from, to, status}` hop along the way (empty when the click did not navigate or the destination served a direct 200).

**Web platform coverage.**

- `XMLHttpRequest` (sync + async, backed by the same `reqwest` client as `fetch`), `performance.mark` / `performance.measure`, `document.getElementsByClassName` / `getElementsByName` / `getElementsByTagName`, 60+ `HTMLElement` subclass constructors (`new HTMLDivElement()` works, `instanceof HTMLScriptElement` works), `element.style = "color: red"` string-coercion setter, `data:` URL fast path in `<script src>`.
- `MutationObserver` + `IntersectionObserver` fire on real DOM mutations and viewport intersections; `setTimeout` / `setInterval` accept the 1-arg form per WHATWG HTML; classic `<script>` runs sloppy-mode per spec (so sites like Apple and Wikipedia that use `var = ...` at the top level work); ES modules (`<script type="module">`) stay strict per ECMA-262.

**Stateful sessions.**

- `heso serve` — JSON-RPC over stdin/stdout. Cookies, DOM mutations, listeners, and history persist across calls. Useful for login → navigate → scrape flows.

## What it can't do

- **No rendering.** No canvas, WebGL, CSS layout, or video. If the meaning is in pixels, use a real browser.
- **CAPTCHAs and hard bot-detect.** Hits one, stops. The default user-agent is `heso/<version>` so anything fingerprinting will see us coming. We detect Cloudflare interstitials and surface them as `partial_reason: "bot_challenge"` rather than pretending the page loaded.
- **Service Workers, WebRTC, WebUSB, WebBluetooth.** Not implemented. The JS engine itself runs modern Next.js / React / Vue / Svelte / SSR sites cleanly; the gaps are in browser features above ECMAScript.
- **Sibling-script cascades we haven't shimmed.** When script A sets `window.X` and script B reads it, and X doesn't exist on first load, heso surfaces the crash and the agent can `--inject-script` a stub.

## Why not just use X?

Partial overlap everywhere; no exact shelf neighbor. The win is not "smaller browser" — it is **smaller failure surface** when the task is structured data, not pixels.

| Layer | Examples | What they ship | Gap vs heso |
|---|---|---|---|
| **Full Chromium stack** | [Playwright](https://playwright.dev/), [Puppeteer](https://pptr.dev/), [Browser Use](https://github.com/browser-use/browser-use), [Stagehand](https://www.browserbase.com/stagehand), [Skyvern](https://github.com/Skyvern-AI/skyvern) | V8 + full browser; often an AI planner on top | Heavy deps, opaque failures, no native JSON verb surface, no plat replay |
| **Smaller browser engine** | [Lightpanda](https://lightpanda.io/) | Zig engine, V8, CDP — drop-in for Playwright/Puppeteer | Still a *browser* mental model; agents drive it through CDP/wrappers, not verbs; no plat/cassette/receipt story |
| **Scraper APIs** | Firecrawl, Jina Reader, Crawl4AI | Fetch + extract markdown/JSON | Weak or no real click/fill/submit; often no honest partial-failure envelope |
| **DOM simulators (Node)** | [jsdom](https://github.com/jsdom/jsdom), [happy-dom](https://github.com/capricorn86/happy-dom) | Minimal DOM + JS in JS | Proven lane for the agent-relevant half; test harnesses, not shipped agent products |
| **Built-in fetch tools** | WebFetch (Claude), curl | Static HTML / no JS hydration | No DOM events, no forms, no session |
| **heso** | this repo | QuickJS + agent-shaped DOM + verbs + plats | QuickJS ≠ V8 (honest limit); CAPTCHAs/hard bot-detect still stop you |

**What heso adds on top of the capability list:** explicit in/out scope, verb-native JSON (no Playwright/CDP/Node required), structured partial failures, and byte-identical off-network replay via stamp/run.

## Use as a library

The Python (`heso`) and Node (`@ixla/heso`) packages each ship two faces of the same bundled binary: a CLI on `$PATH` and a programmatic API that spawns that binary under the hood and gives you back parsed JSON as native objects. No FFI, no Python extension module, no N-API addon — subprocess + JSON is the contract.

```python
# Python
import heso

page    = heso.open("https://example.com")              # -> dict
results = heso.search("rust web scraping", limit=5)     # -> dict
content = heso.read("https://example.com", complete=True)

# Stateful flow over one long-lived `heso serve` process:
with heso.session() as s:
    s.open("https://example.com")
    s.click(text="More information...")
    page = s.read()
```

```js
// Node
import { open, search, read, session } from "@ixla/heso";

const page    = await open("https://example.com");
const results = await search("rust web scraping", { limit: 5 });
const content = await read("https://example.com", { complete: true });

await session(async (s) => {
  await s.open("https://example.com");
  await s.click({ text: "More information..." });
  const page = await s.read();
});
```

Per-language idioms: Python is `snake_case` + sync, Node is `camelCase` + Promises. Full API at **[heso.ca/docs](https://www.heso.ca/docs)**.

## Examples

Search the web, then read the top hits in parallel:

```sh
heso search "rust web scraping" --limit 5
heso batch read url1 url2 url3 --parallel 2
```

Read everything from one page in one call:

```sh
heso read https://nextjs.org/
# → { title, text, actions, forms, cookies, console, framework,
#     content_hash, lazy_hints, partial: false, ... }
```

Find by visible text, click, follow:

```sh
heso click https://news.ycombinator.com --text "More"
```

Wait for an SPA condition:

```sh
heso wait https://app.example.com/ --selector-exists ".dashboard" --timeout 5s
```

Rescue a broken site with a polyfill:

```sh
heso open https://shoelace.style --best-effort \
  --inject-script "window.lunr = (() => ({ Index: { load: () => ({}) } }))()"
```

Multi-step session over stdio:

```sh
heso serve
# → JSON-RPC. Page state, cookies, DOM all persist across requests.
```

Reproducibility (same seed → same output across machines):

```sh
heso eval-js --seed 42 'Math.random()'   # 0.5140492957650241
heso eval-js --seed 42 'Math.random()'   # 0.5140492957650241
```

## Signed receipts

Every `heso open` / `heso read` call can emit a **signed receipt** alongside its JSON output — an Ed25519-signed envelope describing what was run, what came back, and the BLAKE3 trace hash. The recipient verifies the signature against an allowlist of trusted public keys (or rejects the receipt).

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

Verify the receipt — bind it to a trusted signer with `--trusted-keys`:

```sh
# trusted.json is a JSON array of base64 pubkeys you accept signatures from.
echo '["fdibx2rLqGfrIf+duGbRKlM1iPwVSynHUq+nEisjwIE="]' > trusted.json

heso verify --trusted-keys trusted.json receipt.json
# → OK fdibx2rLqGfrIf+duGbRKlM1iPwVSynHUq+nEisjwIE=
# exit 0
```

Or via the `HESO_TRUSTED_KEYS=<path>` env var if you'd rather not pass the flag every call.

Verify enforces three rejections:

```sh
# 1. Tampered receipt — any byte change invalidates the signature
sed -i 's/"seed": 0/"seed": 999/' receipt.json
heso verify --trusted-keys trusted.json receipt.json
# → INVALID: signature verification failed       (exit 1)

# 2. Wrong signer — receipt is well-formed but the pubkey isn't allowlisted
heso verify --trusted-keys other_keys.json receipt.json
# → INVALID: signing pubkey `...` is not in the trusted-keys allowlist   (exit 1)

# 3. `mode: live` — live runs use real time + real network and aren't
#    replay-safe, so the signature has no replay value
heso open https://example.com/ --receipt live.json --mode live
heso verify --trusted-keys trusted.json live.json
# → INVALID: receipt `mode: live` is not replay-safe ...   (exit 1)
```

Verify without an allowlist still works for backwards compatibility, but emits a stderr warning so the missing trust anchor isn't silent:

```sh
heso verify receipt.json
# stderr: warning: no pubkey allowlist configured (pass --trusted-keys PATH or set HESO_TRUSTED_KEYS ...)
# stdout: OK fdibx2...IE=
# exit 0
```

Exit codes: `0` valid + (allowlist empty OR pubkey allowlisted), `1` invalid (tampered, wrong signer, or `mode: live`), `2` missing/malformed receipt or `--trusted-keys` load failure.

## Error handling

Both libraries throw a structured error (`HesoError` in Python, `HesoError extends Error` in Node) when the binary exits non-zero. Fields on the error tell you what to retry:

```python
import heso
try:
    page = heso.read("https://shoelace.style")
except heso.HesoError as e:
    print(e.returncode, e.stderr[:200])  # exit code + first 200 chars of stderr
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
| **Node / TS frameworks** (Mastra, Vercel AI SDK, LangGraph.js, Stagehand, Browser Use TS) | `import { open, search } from "@ixla/heso"`. All async; TypeScript types ship in `index.d.ts`. |
| **Skill-markdown harnesses** (Claude Code, Cursor, Aider, Cline, Continue, Windsurf) | Drop the manifest in the "Use as an agent skill" block below into `~/.claude/skills/heso/SKILL.md` (or the harness's skills dir). The harness auto-discovers; `heso` on PATH does the rest. |
| **CLI-spawning harnesses** (Aider, shell-script agents, homegrown loops) | Same `heso <verb> ...` CLI used by both libraries. JSON on stdout. No special integration. |
| **Long-running JSON-RPC harnesses** | `heso serve` is a JSON-RPC 2.0 server over stdin/stdout. Cookies + DOM state persist across calls. |

The verbs are the contract — no heso-specific framework dependency, no adapter layer.

## Verbs are open

**HESO/1.0** is an open protocol; the `heso` binary is one implementation of it. The full spec lives at [`spec/HESO-1.0.md`](spec/HESO-1.0.md). It defines the core verb set — every conformant implementation MUST dispatch these. Beyond the core, anyone can define a verb under a domain they control, reverse-DNS style:

```json
{"verb": "com.example.scrape-pricing", "url": "https://example.com/products"}
{"verb": "org.archive.warc-import",    "path": "./snapshot.warc"}
```

No registration server, no central authority. **Dispatch is local-only** (spec §4.4) — receiving a plat with an unknown extension verb is a structured error, never a network fetch or a code download. The doc-under-your-domain is human documentation, not a code-delivery channel; discovering a verb (reading the doc) and dispatching it (running the code) are separate operations the spec keeps cleanly apart.

DNS ownership prevents anyone but you from claiming names *under your domain* — same anti-impersonation model as Java packages, Android application IDs, Maven groups, and OCI image labels. It does NOT solve typosquatting (`com.exarnple.foo` and `com.example.foo` are distinct names that look identical to a human reader). HESO/1.0 anchors trust on signing keys, not verb names: pin receivers to trusted signers via the existing `verify --trusted-keys` allowlist (spec §3.9, §4.6).

Today, the reference implementation (this binary, `v0.1.8`) ships only the core verbs — typing `heso com.example.foo ...` exits with `unknown subcommand`. Extension verbs are a namespace, not yet a registered-impl surface in this binary; to dispatch one today you implement HESO/1.0 yourself, in any language. The spec is what makes that implementation possible.

## Use as an agent skill

heso is built to be a tool an agent calls, not a library a human drives. The cleanest integration is the skill markdown pattern that Claude Code, Cursor, Aider, Cline, and similar harnesses use:

```markdown
---
name: heso
description: Use heso when an agent needs to touch the web — fetch pages, run JavaScript, click buttons, fill forms, get structured JSON back. Every run can be stamped into a signed, byte-identically replayable plat — proof of what the agent saw and did. One Rust binary; no Chromium, no Node. Prefer this over WebFetch when you need a DOM, stateful clicks, framework-rendered content, or a verifiable artifact.
---

## Verbs

- `heso search "<query>" [--limit N]` — web search via DDG + Wikipedia
- `heso open <url>` — page summary
- `heso read <url> [--complete]` — full content + actions + forms (use --complete for lazy-loaded sites)
- `heso wait <url> --selector-exists ".x"` — block until a condition is true
- `heso batch [open|read] <urls...> [--parallel N]` — parallel scrape
- `heso click <url> --text "..." | --selector "..." | @eN` — click
- `heso fill <url> @eN "value"` — type into input
- `heso submit <url> @eN` — submit form
- `heso eval-dom <url> "<js>"` — run JS against the page
- `heso serve` — multi-step JSON-RPC session
- `--best-effort` on open/read/wait — exit 0 on partial failures, surface what broke
- `--inject-script "<js>" | @file` — inject a polyfill before page scripts run
- `--timeout <DUR>` on every network verb — per-request wall-clock cap (default `30s`)
- `--js-timeout <DUR>` on `eval-js` / `eval-dom` — cap JS execution wall-clock (default: no cap)
- `--no-private-networks` — refuse URLs resolving to private/loopback/metadata IPs (SSRF protection; off by default)
```

The verbs are the contract. Same shape works in any harness that does tool or skill markdown.

### Global flags

Every network-touching verb accepts `--timeout <DUR>` — `open`, `read`, `click`, `fill`, `submit`, `eval-dom`, `batch`, `stamp`, `refresh`, `meta`, `find`, `tree`, `ls`, `cat`. Default: **30 seconds**.

```
heso open --timeout 3s https://example.com
heso read --timeout 500ms https://news.ycombinator.com
heso batch open --timeout 5s url1 url2 url3      # alias of --timeout-per-url
heso stamp --timeout 10s plan.json
```

Duration syntax matches `heso wait`: bare numbers are milliseconds, suffixes are `ms` / `s` / `m`. `--timeout 0` opts out of the cap entirely. On a timeout the verb emits a structured envelope on stdout and exits 1:

```json
{"ok": false, "error": {"code": "timeout", "timeout_ms": 30000, "elapsed_ms": 30000, "url": "https://..."}}
```

The budget is per network request — it applies to the full request (TLS handshake, redirect chain, response-body stream) and does not reset across redirects. `--timeout` bounds HTTP only. To bound JavaScript execution itself, `eval-js` and `eval-dom` accept a separate `--js-timeout <DUR>` that caps script wallclock and returns a structured `timeout` error on expiry (default: no cap). The `npm/@ixla/heso` and `python/heso` wrappers also install a `timeout + 5s` process-kill backstop so a hung binary still eventually unblocks the caller.

`--no-private-networks` opts into SSRF protection: heso resolves each target and refuses to connect if any resolved IP is loopback, RFC1918 private, link-local (including the `169.254.169.254` cloud-metadata address), unspecified, or CGNAT. The check runs on the resolved IP, so a hostname like `localhost` — or any domain whose DNS points inward — is caught, not just literal IPs. It is **off by default** so local testing against `localhost` keeps working; set it per invocation with the flag, or once for a hosted deployment with `HESO_BLOCK_PRIVATE_NETWORKS=1` in the environment (which protects every verb). On a refusal the verb emits a structured envelope on stdout and exits 1:

```json
{"ok": false, "error": {"code": "private_network_blocked", "url": "https://..."}}
```

## Stats

Measured on Windows 11, AMD x86_64, with the release binary:

| Thing | Number |
|---|---|
| Binary size | 10.27 MB |
| Cold start (`open https://example.com`, network included) | ~77 ms |
| Engine-only (no network, local fixture) | ~28 ms |
| Batch (8 URLs, `--parallel 8`) | ~1.1 s total |
| Search (DDG, 5 results) | ~1 s |

## Building from source

If you want to hack on heso itself (prebuilt binaries for Windows x64, Linux x64+arm64, macOS x64+arm64 ship from each release tag — see Install above):

```sh
git clone https://github.com/blank3rs/heso
cd heso
cargo build --release -p heso-cli
./target/release/heso search "rust web scraping" --limit 5
```

Requires Rust 1.90 (`rustup` from https://rustup.rs).

## Status

`v0.1.8` is shipping on every registry. The engine, the verbs, and plat replay are stable enough to use — the spot checks on GitHub, Cloudflare, and friends come back clean, and the 271-test suite is required green on every release. What may still shift before `v1.0` is the CLI surface: verb names, JSON field names, flag spellings. Pin the version if you embed it.

## License

MIT or Apache-2.0, your choice.

---

Full docs: **[heso.ca/docs](https://www.heso.ca/docs)** · Site: **[heso.ca](https://www.heso.ca)** · npm: **[@ixla/heso](https://www.npmjs.com/package/@ixla/heso)** · PyPI: **[heso](https://pypi.org/project/heso/)**
