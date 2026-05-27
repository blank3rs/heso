# @ixla/heso

A Rust runtime that lets an agent touch the web — fetch a page, run its JavaScript, query the resulting DOM, click, fill, submit, hold a session — and return JSON. Every run can be stamped into a signed file (a *plat*) that replays byte-identically off-network.

One binary. No Chromium, no Node.

<!-- heso:perf:start -->
```
binary       10.11 MB
cold start   ~10 ms   (help banner, no engine)
js init      ~21 ms   (eval-js null)
open URL     ~80 ms   (network included)
batch        ~1.1 s   for 8 URLs in parallel
```
<!-- heso:perf:end -->

**Site:** [heso.ca](https://www.heso.ca) · **Docs:** [heso.ca/docs](https://www.heso.ca/docs) · **GitHub:** [blank3rs/heso](https://github.com/blank3rs/heso) · **PyPI:** [heso](https://pypi.org/project/heso/)

## Install

```sh
npm install -g @ixla/heso         # CLI on PATH
npm install @ixla/heso            # library (also gives you the CLI shim)
npx @ixla/heso open https://example.com   # one-shot
```

> Ships prebuilt binaries for Windows x64, Linux x64 + arm64, macOS x64 + arm64. `npm` picks the right `@ixla/heso-<platform>-<arch>` via `optionalDependencies` — no native build step.

<!-- heso:version:start -->
> Shipping `v0.1.4` for Windows-x64, Linux x64 + arm64, macOS x64 + arm64. `cargo-dist` builds every target on tag; npm/PyPI publish through the same workflow.
<!-- heso:version:end -->

## Use as a CLI

The shortest path to structured data on a page is `eval-dom`: fetch + run page scripts + run your JS against the DOM + return JSON.

```sh
heso eval-dom https://news.ycombinator.com '
  Array.from(document.querySelectorAll(".athing")).slice(0, 5).map(row => ({
    title: row.querySelector(".titleline > a")?.innerText,
    href:  row.querySelector(".titleline > a")?.href,
  }))
'
```

For the broader view there's `read` (or `open` for just title + actions + tree):

```sh
heso open  https://example.com
heso read  https://nextjs.org/ --complete
heso batch read url1 url2 url3 --parallel 2

heso search "rust web scraping" --limit 5
heso click  https://news.ycombinator.com --text "more"
heso fill   https://example.com/search @e0 "rust"
heso submit https://example.com/search @form1 --field q=rust
heso wait   https://app.example.com/ --selector-exists ".dashboard" --timeout 5s
heso serve  # JSON-RPC over stdio for multi-step sessions
```

Plat tools (signed, replayable artifacts):

```sh
heso stamp  plan.json > out.plat       # plan → plat (executes + records cassette)
heso run    out.plat > replay.plat     # plat → plat (off-network, byte-identical hash)
heso replay out.plat                   # plat → step log (pure read, no engine)
heso replay --plan out.plat > plan.json  # plat → plan (edit, restamp)

heso info   out.plat                   # summary: hash, plan / cassette / steps counts
heso info   before.plat after.plat     # diff (plan, cassette URLs, fields)
heso seal   my.plat > sealed.plat      # wrap in Ed25519 envelope
heso unseal sealed.plat                # verify; exit 0 valid / 1 invalid / 2 wrong-alg or malformed
heso verify receipt.json --trusted-keys trusted.json
```

Full verb reference at **[heso.ca/docs](https://www.heso.ca/docs)**.

## Use as a library

```js
import {
  open, read, evalDom, session, wait,
  stamp, run, replay,
  verify, info, seal, unseal,
  registry,
  HesoError,
} from "@ixla/heso";

// One-shot calls
const page    = await open("https://example.com");
const results = await registry.search("rust web scraping", { limit: 5 });
const content = await read("https://example.com", { complete: true });
const value   = await evalDom("https://example.com", "document.title");

// Wait for a condition
const ready = await wait("https://app.example.com/", {
  selectorExists: ".dashboard",
  timeout: "5s",
});

// Stateful flow (cookies + DOM persist across calls)
await session(async (s) => {
  await s.open("https://example.com");
  await s.click({ text: "More information..." });
  const page = await s.read({ include: "text,actions" });
  const title = await s.eval("document.title");
});

// Polymorphic plat / receipt tools
const summary = await info("out.plat");                        // summary of any artifact
const verdict = await verify("out.plat");                      // { status: "valid", ... }
const sealed  = await seal("out.plat");                        // SealedPlat envelope
const status  = await unseal("sealed.plat");                   // { status: "valid", ... }
const body    = await unseal("sealed.plat", { extract: true }); // inner plat
```

All functions return `Promise<object>`. TypeScript declarations ship in `index.d.ts`.

## Error handling

`HesoError` extends `Error` and carries `code`, `stdout`, `stderr`, `command`:

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

For sites that crash some scripts, opt into the partial-success envelope instead — heso exits 0 with `partial: true`:

```js
const page = await read("https://shoelace.style", { bestEffort: true });
if (page.partial) {
  console.log("got partial:", page.partial_reason, page.failed_scripts);
}
```

## What works today

- **eval-dom is the structured-extraction primary** — fetch + run page scripts + run your JS against the DOM, one round trip.
- **Most second-tier sites Just Work.** Default UA is `heso/<version>` — honest, no impersonation, no residential-proxy farm — and that slips past a lot of WAF heuristics tuned for Playwright, Puppeteer, curl-impersonate, and headless-Chrome traffic. Sites that go through cleanly on a vanilla call include Zillow, Walmart, CoinGecko, LinkedIn anonymous pages, TripAdvisor, Yahoo Finance, and Reddit via `old.reddit.com`.
- **Find and read** — `search`, `open`, `read --complete`, `batch`, `wait`.
- **Interact** — `click` by ref/text/selector/aria, `fill`, `submit`; sessions also expose `navigate`.
- **Recover from broken sites** — `--best-effort` returns `partial: true` + structured failure envelope; `--inject-script` shims missing globals.
- **Detect state changes** — `read` returns `content_hash`; pass `--since <hash>` for a `delta` describing what changed.
- **Stateful sessions** — `session()` wraps a long-lived `heso serve` for login → navigate → scrape flows.

## What doesn't

- **No rendering** — no canvas, WebGL, CSS layout, or video. If the meaning is in pixels, use a real browser.
- **Full Cloudflare Challenge Mode and Imperva interstitials still block.** The narrow bot-challenge detection catches them — exit data is honest, exit content is empty. No CAPTCHA solver.
- **No Service Workers, WebRTC, WebUSB, WebBluetooth.**
- **QuickJS, not V8.** Most modern frameworks (Next.js, React, Vue, Svelte, SSR) run cleanly; some V8-specific JS doesn't.

## Honest about HTTP and bot walls

Every response carries `http_status`. Bodies containing Cloudflare's `__cf_chl_opt` JS shim or a `<title>` starting with one of nine well-known WAF phrases ("Just a moment…", "Attention Required", "Access Denied", "Verify you are human", and a few variants) surface as `partial_reason: "bot_challenge"` regardless of wrapper status. The detection is intentionally narrow — false positives are worse than misses — so many bot walls come back as `http_403` or `http_429` and you should treat those as bot-walled by default rather than retrying the same request.

## Plug into agent harnesses

| Harness style | How heso fits |
|---|---|
| **Node / TS frameworks** (Mastra, Vercel AI SDK, LangGraph.js, Stagehand, Browser Use TS) | `import { open, search } from "@ixla/heso"`. All async; types in `index.d.ts`. |
| **Skill-markdown harnesses** (Claude Code, Cursor, Aider, Cline, Continue, Windsurf) | Drop the manifest from the [main README](https://github.com/blank3rs/heso#use-as-an-agent-skill) into the harness's skills directory. |
| **CLI-spawning harnesses** | `heso <verb> ...` outputs JSON on stdout. Standard subprocess. |
| **Long-running JSON-RPC harnesses** | `heso serve` is JSON-RPC 2.0 over stdio. Cookies + DOM state persist. |

## Architecture

Both the CLI on PATH and the library functions spawn the same bundled Rust binary and parse the JSON it returns. No FFI, no N-API addon, no native module compile step. The npm `optionalDependencies` install only the right platform binary for your machine.

## Links

[GitHub](https://github.com/blank3rs/heso) · [Issues](https://github.com/blank3rs/heso/issues) · [PyPI](https://pypi.org/project/heso/) · [Docs](https://www.heso.ca/docs)

## License

MIT or Apache-2.0, your choice.
