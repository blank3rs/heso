# @ixla/heso

**The agent-native web engine. No Chromium. No Node. One Rust binary.**

Fetches a URL, runs the JavaScript, lets you click, fill forms, search the web, and scrape many pages in parallel — and returns everything as JSON so an agent can use it.

```
binary       9.2 MB
cold start   ~80 ms   (open https://example.com, network included)
engine only  ~35 ms   (no network)
batch        ~1.1 s   for 8 URLs in parallel
```

**Site:** [heso.ca](https://www.heso.ca) · **Docs:** [heso.ca/docs](https://www.heso.ca/docs) · **GitHub:** [blank3rs/heso](https://github.com/blank3rs/heso) · **PyPI:** [heso](https://pypi.org/project/heso/)

## Install

```sh
npm install -g @ixla/heso         # CLI on PATH
npm install @ixla/heso            # library (also gives you the CLI shim)
npx @ixla/heso open https://example.com   # one-shot
```

> `v0.0.1` ships Windows-x64 only. Linux + macOS binaries land with `v0.0.2`.

## Use as a CLI

```sh
heso open https://example.com
# → { url, title, description, tree, actions, plat_hash, ... }

heso search "rust web scraping" --limit 5
heso read https://nextjs.org/ --complete
heso batch read url1 url2 url3 --parallel 2
heso click https://news.ycombinator.com --text "More"
heso wait https://app.example.com/ --selector-exists ".dashboard" --timeout 5s
heso open https://x.com --best-effort --inject-script "window.lunr = (() => ({}))()"
heso serve     # JSON-RPC over stdio for multi-step sessions

heso stamp  plan.json > out.plat    # plan → plat (executes + mints)
heso replay out.plat                # plat → per-step log (no artifact)
heso unpack out.plat > plan.json    # plat → plan (edit, restamp)
```

Full verb reference at **[heso.ca/docs](https://www.heso.ca/docs)**.

## Use as a library

```js
import {
  open, search, read, evalDom, session, wait,
  stamp, replay, unpack,
  HesoError,
} from "@ixla/heso";

// One-shot calls
const page    = await open("https://example.com");
const results = await search("rust web scraping", { limit: 5 });
const content = await read("https://example.com", { complete: true });
const value   = await evalDom("https://example.com", "document.title");

// Wait for a condition
const ready = await wait("https://app.example.com/", {
  selector_exists: ".dashboard",
  timeout: "5s",
});

// Stateful flow (cookies + DOM persist across calls)
await session(async (s) => {
  await s.open("https://example.com");
  await s.click({ text: "More information..." });
  const page = await s.read({ include: "text,actions" });
  const title = await s.eval({ js: "document.title" });
});
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

For sites that crash some scripts, opt into the partial-success envelope instead:

```js
const page = await read("https://shoelace.style", { best_effort: true });
if (page.partial) {
  console.log("got partial:", page.partial_reason, page.failed_scripts);
}
```

## What it can do

- **Find and read**: `search`, `open`, `read --complete`, `batch`, `wait`
- **Interact**: `click` by ref/text/selector/aria, `fill`, `submit`, `navigate`
- **Recover from broken sites**: `--best-effort` returns `partial: true` + structured failure envelope; `--inject-script` shims missing globals
- **Detect state changes**: `read` returns `content_hash`; pass `--since <hash>` for a `delta` describing what changed
- **Stateful sessions**: `session()` wraps a long-lived `heso serve` for login → navigate → scrape flows

## What it can't do

- **No rendering** — no canvas, WebGL, CSS layout, or video. If the meaning is in pixels, use a real browser.
- **CAPTCHAs and hard bot-detect** — hits one, stops.
- **Pages built on tech we don't simulate** — Service Workers, WebRTC, WebUSB, WebBluetooth.
- **Sites whose JS we can't run** — QuickJS isn't V8. Most works; some doesn't.

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
