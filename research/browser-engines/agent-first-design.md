# Agent-First Browser Design

**Topic:** What's different when the browser's user is an LLM, not a human
**Last updated:** 2026-05-17
**Status:** initial research — *now framed as internals after ADR 0009*

> **Reframing note (2026-05-17, revised):** Per [ADR 0009](../../decisions/0009-heso-run-single-tool.md), the patterns described in this document are **internal primitives**, not the public agent-facing API. Per [ADR 0010](../../decisions/0010-primitives-as-terminal-commands.md), the *vocabulary* of those primitives is shell-shaped (`pwd`, `ls`, `cd`, `cat`, `find`, `grep`, `echo`, `rm`, `click`, `submit`, `wget`, `wait`, `screenshot`, `eval`, `diff`) and the page is modelled as the working directory.
>
> heso's public surface is exactly one tool: `heso.run(start_url, request)`. The semantic selectors, bulk operations, structured errors, etc. described below are how heso's internal *primitives layer* works — what the **planner** emits and the **trace runner** executes. Agents never call these directly.
>
> Where this note uses earlier names (`click(@e3)`, `fill(@e5, val)`, `extract({...})`, `click_sequence([...])`), translate as follows: `click` on a navigating element → `cd @ref`; `click` on a button/toggle → `click @ref`; `fill` → `echo "val" > @ref`; bulk extract → planner emits a sequence of `ls`/`find`/`cat`; bulk click → planner emits a sequence of `click`/`cd`. The design *principles* in this note (AX-tree-first, semantic selectors, bulk = planner job not primitive, structured errors with candidates, idempotency, determinism) all still apply.
>
> Read this doc to understand the engine internals. Read [`docs/the-one-tool.md`](../../docs/the-one-tool.md) and [`skills/heso/SKILL.md`](../../skills/heso/SKILL.md) to understand what agents actually see. Read [ADR 0010](../../decisions/0010-primitives-as-terminal-commands.md) for the canonical primitive list.

## Summary

A browser for agents differs from a browser for humans in three deep ways: the **default page representation** is structured text (accessibility tree + extracted markdown), not pixels; **actions are semantic**, not coordinate-based; and **state is required to be deterministic, diffable, and idempotent** in a way browsers normally aren't. Almost every UI subsystem in a human browser can be stripped. Almost every guarantee an agent needs (idempotency keys, page-state diffs, fingerprint isolation per session) is missing from human browsers and has to be added.

The design choices below shape heso's internal primitives layer — the small, fixed set of operations the planner composes into traces. They are *not* the agent's API; the agent's API is `heso.run`.

## Default page representation: AX tree + markdown, not screenshots

The current best practice in agent browsing is the **accessibility (AX) tree**. Browsers maintain it for screen readers; every interactive element has a node with role, name, state, and value (W3C Core-AAM). Playwright's MCP server ships the AX tree to LLMs instead of HTML or screenshots — and the token math is decisive: an AX snapshot averages 2–5 KB, a screenshot of the same page runs 100+ KB. That's 20–50× on input tokens per page.

heso's default representation should be:

1. **AX tree** — primary. Annotated with stable element refs (`@e1`, `@e2`, …) the agent uses for actions.
2. **Extracted markdown** — secondary. Built from semantic HTML (headings, lists, links, paragraphs). Cleaner than raw text content.
3. **Screenshots** — opt-in. Only when the agent explicitly asks for visual context.
4. **Raw HTML** — opt-in. Almost never needed; provided for escape-hatch debugging.

The renderer pipeline note (`rendering-pipeline.md`) lists which engine stages this lets us skip.

## Semantic selectors, not coordinates

Coordinates (`click(x: 482, y: 91)`) are the worst possible interface for an LLM:
- Tokens wasted on numbers it has to guess
- Brittle to layout shifts
- Impossible to verify after the fact
- Encourages the agent to think about pixels instead of intent

heso should expose `click(@e3)`, `fill(@e5, "akku41809@gmail.com")`, `select(@e8, "United States")` — semantic actions against opaque element refs that map to AX-tree nodes. The `EngineApi` should never accept raw coordinates from agents; if a tool absolutely needs pixel input (drag to draw, canvas painting), it's a separate, narrower API.

## Bulk operations

Humans do one click at a time. Agents do "fill these 14 fields then submit." Single-action APIs force 14 round-trips, each costing an LLM call to reason about the response. heso's API should support:

- `fill_form({@e1: "x", @e2: "y", ...})`
- `click_sequence([@e3, @e5, @e7])` with retry/short-circuit semantics
- `extract({title: "h1", price: ".price", reviews: "[data-review]"})` — batch extraction in one round trip

## Job-based async

Browser operations have unpredictable latency (page loads, redirects, JS spinning). Synchronous blocking forces the agent to wait. The Browserbase/Stagehand pattern: every operation returns a job ID, the agent polls or subscribes. heso should:

- Default to job-handles for all operations longer than ~100ms
- Provide a sync convenience wrapper for short ops
- Allow the agent to chain jobs without blocking (`then`-like semantics)

## Structured errors with retry candidates

When a click fails, the worst response is "Error: element not found." The best response is:
```json
{
  "error": "stale_element",
  "candidates": [
    {"ref": "@e7", "role": "button", "name": "Sign in", "confidence": 0.91},
    {"ref": "@e12", "role": "link", "name": "Log in", "confidence": 0.62}
  ],
  "page_changed_since_last_snapshot": true
}
```

This is the most heso-specific design point. Every error path should include enough context for the LLM to retry without another snapshot round-trip.

## Identity-signed actions

Agent actions on a user's behalf should be auditable. Each action should carry:
- Agent identity (model + session)
- Authorization scope (what the user consented to)
- Cryptographic signature, ideally cheap (Ed25519)

Sites can then opt into agent-aware policies. This is forward-looking — no standard exists yet — but heso's API should reserve the field. (See ADR 0005.)

## Cost reporting per call

Every operation should return its cost: bytes downloaded, ms of CPU, tokens-in-response. Agents that don't see their cost won't optimize against it.

## What humans need that agents don't

Strip aggressively from heso v1:
- **Rendering** (paint + composite) — only on demand
- **Animations / transitions** — disable globally
- **Audio / video** — disable unless required, never auto-play
- **Font rendering** — only needed for screenshots
- **Accessibility for *humans*** (screen reader integration, magnifier) — note: we *use* the AX tree, we don't *expose* it back to OS screen readers
- **Print** — gone
- **Sync / bookmarks / history UI** — agents have their own memory
- **Extensions UI / settings UI / preferences UI** — config is API
- **Address bar UI / autocomplete UI** — there is no UI

## Deterministic execution (first-class property)

Reproducibility is **non-negotiable** for heso — see [ADR 0008](../../decisions/0008-deterministic-execution.md) and [`determinism.md`](determinism.md) for full reasoning and the per-source strategies. The short version:

- **Same seed + same recorded inputs → byte-identical observable output** (AX tree, extracted text, screenshot, signed audit log).
- **All clocks are fake** by default — `Date.now()`, `performance.now()`, `setTimeout`, `setInterval`, `requestAnimationFrame` all drive from a session-seeded fake clock the agent controls.
- **All randomness is seeded** — `Math.random()`, `crypto.getRandomValues()` (with an opt-out for legitimate crypto use).
- **All network goes through record/replay** via `WebViewDelegate::intercept_web_resource_load`.
- **Rendering is always software** — `SoftwareRenderingContext`, pinned font stack, no system fonts, no GPU.
- **JS event loop ordering is serialized** where Servo allows.

Three operating modes per session: `deterministic` (default, full reproducibility), `recording` (real wall clock, all inputs logged for replay), `live` (no guarantees, identity refuses to sign).

Determinism is a *precondition* for the signed audit log to mean anything (ADR 0005). It's also a precondition for flake-free agent tests and for replay debugging.

When you design any new API surface — extractor, action, job primitive — your first question is "would this be reproducible across two runs with the same seed?" If no, redesign or make the nondeterminism explicit in the API name (`session.unsafe_use_real_entropy()`).

## What agents need that humans don't

Add to heso v1:
- **Deterministic state** — see the section above; this is foundational, not an add-on.
- **Idempotency keys** on every state-changing operation
- **Page-state diffs** — "what changed since snapshot @s7?" cheaper than re-snapshotting
- **Per-session fingerprint isolation** — each agent session is a fresh browser identity (cookies, storage, fingerprint, IP if proxied)
- **Time travel** — snapshot/restore page state
- **Network record/replay** — for reproducible runs and debugging (also feeds determinism)

## References

- [Playwright MCP: accessibility-tree page representation](https://playwright.dev/mcp/introduction)
- [W3C Core Accessibility API Mappings 1.2](https://w3c.github.io/core-aam/)
- [WAI-ARIA 1.3](https://w3c.github.io/aria/)
- [Stagehand docs](https://docs.stagehand.dev/)
- [Browserbase docs](https://docs.browserbase.com/)
- [Agent Browser vs Puppeteer & Playwright (Webfuse)](https://www.webfuse.com/blog/agent-browser-vs-puppeteer-and-playwright)
- [Accessibility-First Browser Automation (proofsource.ai)](https://proofsource.ai/2026/01/agent-browser-the-accessibility-first-approach-to-browser-automation/)
- [Beyond cookies: browser fingerprinting in 2025](https://shivankaul.com/blog/beyond-cookies-browser-fingerprinting-in-2025-1)
