# Priorities

> Updated frequently. Reflects what matters THIS month. Defer everything not listed here.

**Last updated:** 2026-05-18

## North star

heso is the **headless browser for the agent-relevant half of the web** — 30 MB single binary, no Chromium dep, fetch + parse + JS + forms + clicks + sessions in scope; canvas/WebGL/video/CSS-layout explicitly out. See [ADR 0016](../decisions/0016-positioning-headless-browser-for-agents.md). The public surface is **one tool**: `heso.run(start_url, request)`. One run produces a plat (the cartography artefact, ADR 0015). Everything else is internals. See ADR 0009.

Internally, the primitives the planner emits are **terminal-shaped**: the current page is the working directory, elements are files, cookies and storage live under `/env/`. The 15 primitives are shell commands (`pwd`, `ls`, `cd`, `cat`, `find`, `grep`, `echo`, `rm`, `click`, `submit`, `wget`, `wait`, `screenshot`, `eval`, `diff`). See ADR 0010.

The engine is **native single-binary Rust**. `heso-engine-fetch` (`reqwest` + `html5ever` via `scraper`) is the static path (ADR 0012). `heso-engine-js` wraps QuickJS via `rquickjs` for JS execution (ADR 0014); Phase 1A is the bare evaluator with no DOM, Phase 1B adds agent-shaped DOM types. No Chrome, no Node, no Python. Deploys anywhere.

heso is also **completely deterministic by default** (ADR 0008). Same seed + same recorded inputs → byte-identical observable output. Static fetches are deterministic by construction (no clock, no RNG).

## Right now (May 2026)

M0/M1/M2 are mostly shipped, cartography V0 landed (ADR 0015), and ADR 0014 Phase 1A landed 2026-05-18 — `heso-engine-js` exists as a sandboxed QuickJS evaluator (no DOM yet), `heso eval-js` works end-to-end, 145 workspace lib tests green, 8.1 MB release binary. The next big push is bringing JS-capable cartography online so SPAs map like static pages do today.

Order of work for the next few weeks:

1. **DONE — M1: native fetch engine** (`heso-engine-fetch`, ADR 0012). `heso fetch <url>` returns real `{ url, text }` from real sites. Single Rust binary.
2. **DONE — M2: primitives + trace runner** (T-020/T-021). 15-op AST, `execute()` dispatcher, `Receipt` with stable BLAKE3 `trace_hash`.
3. **DONE — semantic extractors + cartography V0** (ADR 0013, ADR 0015). `heso open`, `heso meta`, `heso tree/ls/cat/find`, `heso serve`, content-addressed `plat_hash`. `--explore-links` builds same-origin sub-page trees.
4. **DONE — ADR 0014 Phase 1A: QuickJS evaluator** (`heso-engine-js`). Sandboxed `JsEngine::eval()`, console capture, isolation per engine, 16 unit tests. CLI: `heso eval-js`. No DOM yet — this is the language, not the browser.
5. **NEXT — ADR 0014 Phase 1B: agent-shaped DOM** — Document / Element / Node / NodeList / Text as `#[rquickjs::class]` Rust types backed by `scraper::Html`. window/document globals. This is the bulk of the months-of-work scope.
6. **ADR 0014 Phase 1C** — run `<script>` tags during `heso open --js`, so SPA hydration actually happens.
7. **More primitives on the fetch engine** — `cat @ref` (lookup by ref), `find -role X`, `ls`, `grep` over page text (some already live via cartography V0; verify and round out per ADR 0010).
8. **M3: Build planner v0** — pattern-matched conversion of plain-English requests to traces (~5 request shapes: list extraction, single-item search, form submission, multi-page join, page watch — T-022).
9. **M3: Wire `heso.run`** — single MCP tool stitching planner + trace runner (T-023).
10. **M3: Skill MD + demos** — agent-readable skill, end-to-end demos against fixtures + live pages (T-024, T-025).
11. **M0 mechanical leftovers** — push to GitHub + stand up CI (T-010, T-011). Build in the open.

## Explicitly DEFER

- **A JS engine that isn't bundled QuickJS** — ADR 0014 picked QuickJS via `rquickjs` after considering wry/Tauri WebView and Servo. Don't re-litigate without a superseding ADR. Chromium-via-CDP stays rejected (system-Chrome dep).
- The Claude Code plugin (M5) — premature until `heso.run` works on at least 5 request shapes.
- Identity / signed audit log (M4) — important, but trace runner has to exist first.
- Multi-tab, sessions, cookies — basic single-tab + single-request path first.
- Anti-bot work — meaningful only after `heso.run` works on cooperative sites.
- Planner v1 (small in-engine LLM) — v0 pattern-matched planner first, learn from real requests, then upgrade.
- DSL surfaces (hesoql, weave, etc.) — abandoned in favor of plain-English requests + one tool (see ADR 0009).
- Multiple MCP tools — exactly one tool, per ADR 0009.

## What "done" looks like for the next milestone (M3)

Single command (manually written, not generated):

```
heso run \
  --start https://news.ycombinator.com \
  --request "top 10 stories with title, url, score, comments" \
  --seed 42
```

returns structured JSON, a signed receipt, and a cost report — and produces byte-identical output on a second run with the same seed and recorded network.

## North-star check

When you design any new API surface, ask four questions in order:

1. **Is it under the one tool?** If a new public method is being added beside `heso.run`, stop and re-read ADR 0009.
2. **Is it deterministic?** If the operation has a hidden clock, RNG, or network read, the design is wrong or the nondeterminism must be explicit in the name (`unsafe_use_real_entropy`).
3. **Does it fit the shell metaphor?** New primitives have a terminal-command name (`pwd`/`ls`/`cd`/`cat`/...) and a shell-shaped contract. If you can't pick a terminal verb that fits, stop and re-read ADR 0010 — you may be smuggling in a layer that should live in the planner or trace runner, not the primitives.
4. **Does the planner need to learn it?** New primitive operations require the planner to know when to emit them. If the planner can't, the primitive is dead weight.
