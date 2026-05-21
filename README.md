# heso — The agent-native web engine. No Chromium. No Node. One Rust binary.

**Site:** [heso.ca](https://www.heso.ca) · **Docs:** [heso.ca/docs](https://www.heso.ca/docs) · **[npm](https://www.npmjs.com/package/@ixla/heso)** · **[PyPI](https://pypi.org/project/heso/)** · **[Releases](https://github.com/blank3rs/heso/releases)**

It fetches a URL, runs the JavaScript, lets you click, fill forms, search the web, and scrape many pages in parallel — and returns everything as JSON so an agent can use it.

```
binary       9.2 MB
cold start   ~80 ms   (open https://example.com, network included)
engine only  ~35 ms   (no network)
batch        ~1.1 s   for 8 URLs in parallel
```

![heso agent demo](demo/demo.gif)

That's a real recording — Claude Code (`claude -p` from the repo root, with the heso skill loaded) discovering the verbs, navigating the page tree, and pulling the live top story off Hacker News. No edits, no replays.

## Install

```sh
# Python (uv, pipx, or pip — any of them)
uv tool install heso          # or: pipx install heso  /  pip install heso

# Node
npm install -g @ixla/heso     # or one-shot: npx @ixla/heso open https://example.com

# Direct binary
# Windows:
powershell -c "irm https://github.com/blank3rs/heso/releases/latest/download/heso.zip -OutFile heso.zip; Expand-Archive heso.zip -DestinationPath ."
```

> Currently shipping `v0.0.1` Windows-x64 only. Linux + macOS binaries land with `v0.0.2` (CI builds wiring up now). On other platforms, [build from source](#building-from-source) for now.

After install, `heso` is on `$PATH`:

```sh
heso open https://example.com
# → { url, title, description, tree, actions, plat_hash, ... }
```

You get JSON: title, description, a heading tree, and a list of clickable elements numbered `@e0`, `@e1`, and so on.

## A note before you read further

Most of this codebase was written with help from Claude under one person's direction. The co-author tag is on basically every commit. It moved fast, which means the feature surface ran ahead of real usage. Treat this as working code that needs more eyes on real workloads, not a finished product.

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
- `heso navigate` — change URL within a session.
- `heso eval-dom <url> "<js>"` — fetch, run scripts, then run your JS against the resulting DOM.

**Bundle, edit, and replay action sequences.**

A *plan* is a JSON array of canonical actions (`open`, `click`, `fill`, `submit`). A *plat* is an observation. The three verbs below close the loop:

- `heso stamp <plan.json>` — executes the plan against the live web and mints a fresh plat that embeds the plan. Accepts a bare `Action[]` array, a plat with a `"plan"` field, or a `TraceFingerprint`. Exit 0 on a clean run; 1 if any step failed (still prints the partial plat with `error` + `steps`).
- `heso replay <plat.json>` — re-executes the embedded plan and prints a per-step session log. No plat output — use `stamp` for that. Stateful: one `JsSession` carries DOM mutations / RNG / cookies across steps.
- `heso unpack <plat.json>` — extracts just the `plan` field. Edit it standalone and pipe back into `stamp` to re-mint.

```sh
cat > plan.json <<EOF
[
  {"verb": "open",   "url": "https://news.ycombinator.com/"},
  {"verb": "click",  "ref": "@e3"},
  {"verb": "fill",   "ref": "@e7", "value": "claude"},
  {"verb": "submit", "ref": "@form1"}
]
EOF
heso stamp plan.json > plat.json          # plan → plat
heso replay plat.json                     # plat → step log (no artifact)
heso unpack plat.json > plan-again.json   # plat → plan (edit, restamp)
```

The plat's `plat_hash` (BLAKE3 over canonical JSON via RFC 8785) commits to both the plan AND the observed content. Edit either and the hash no longer matches. `heso plat-verify` will say so.

**Recover from broken sites.**

- `--best-effort` on `open` / `read` / `wait` — exit 0 even when scripts crash. Output includes `partial: true`, `partial_reason: "script_crash" | "wait_timeout" | "fetch_failed" | "parse_error"`, and `failed_scripts: [...]`. The agent sees what broke and decides what to try next.
- `--inject-script "<inline-js>"` or `--inject-script @file.js` — run JS before the page's own scripts. Use it to shim a missing global (the canonical `window.lunr` cascade kind of thing).

**Detect cross-call state changes.**

- `heso read` always returns a `content_hash`. Pass `--since <prev_hash>` to get a `delta` describing what changed (`actions_added`, `actions_removed`, `forms_changed`, `text_changed`, `title_changed`).

**Stateful sessions.**

- `heso serve` — JSON-RPC over stdin/stdout. Cookies, DOM mutations, listeners, and history persist across calls. Useful for login → navigate → scrape flows.

## What it can't do

- **No rendering.** No canvas, WebGL, CSS layout, or video. If the meaning is in pixels, use a real browser.
- **CAPTCHAs and hard bot-detect.** Hits one, stops. The default user-agent is `Mozilla/5.0 (compatible; heso/0.0.1)` so anything fingerprinting will see us coming.
- **Pages built on tech we don't simulate.** Service Workers, WebRTC, WebUSB, WebBluetooth — not supported.
- **Sites whose JS we can't run.** QuickJS isn't V8. Most works; some doesn't.
- **Sibling-script cascades we haven't shimmed.** When script A sets `window.X` and script B reads it, and X doesn't exist on first load, heso surfaces the crash and the agent can `--inject-script` a stub.

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

The verbs are the contract (see [ADR 0017](https://github.com/blank3rs/heso/blob/main/decisions/0017-verbs-as-agent-surface.md)) — no heso-specific framework dependency, no adapter layer.

## Use as an agent skill

heso is built to be a tool an agent calls, not a library a human drives. The cleanest integration is the skill markdown pattern that Claude Code, Cursor, Aider, Cline, and similar harnesses use:

```markdown
---
name: heso
description: Use the heso headless browser (one Rust binary, no Chromium, no Node) to search the web, fetch pages, run their JavaScript, extract content, navigate, fill forms, or click links. Prefer this over WebFetch when you need a DOM, stateful clicks, or framework-rendered content.
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
```

The verbs are the contract. Same shape works in any harness that does tool or skill markdown.

## Stats

Measured on Windows 11, AMD x86_64, with the release binary:

| Thing | Number |
|---|---|
| Binary size | 9.2 MB |
| Cold start (`open https://example.com`, network included) | ~80 ms |
| Engine-only (no network, local fixture) | ~35 ms |
| Batch (8 URLs, `--parallel 8`) | ~1.1 s total |
| Search (DDG, 5 results) | ~1 s |

No comparisons to other tools — different tools have different tradeoffs and "X is faster than Y" framing rarely survives contact with a real workload.

## Building from source

If you're on Linux/macOS today (v0.0.2 will ship prebuilt binaries) or want to hack on heso itself:

```sh
git clone https://github.com/blank3rs/heso
cd heso
cargo build --release -p heso-cli
./target/release/heso search "rust web scraping" --limit 5
```

Requires Rust 1.80+ (`rustup` from https://rustup.rs).

## Status

Pre-alpha. `v0.0.1` is on every registry. Worth trying if the use case fits; not worth depending on in production yet. Next ([`v0.0.2`](https://github.com/blank3rs/heso/milestone/2)) ships Linux + macOS binaries and the library APIs above.

## License

MIT or Apache-2.0, your choice.

---

Full docs: **[heso.ca/docs](https://www.heso.ca/docs)** · Site: **[heso.ca](https://www.heso.ca)** · npm: **[@ixla/heso](https://www.npmjs.com/package/@ixla/heso)** · PyPI: **[heso](https://pypi.org/project/heso/)**
