# Browser Engine Research Index

**Topic:** Map of the research notes in `research/browser-engines/`
**Last updated:** 2026-05-17
**Status:** stable

## Summary

This directory holds **engine-agnostic** browser-engine background for heso. The notes here apply regardless of what engine heso ships on. Read `agent-first-design.md` before designing any new API surface; read `determinism.md` before wiring any clock/RNG/network call; read `rendering-pipeline.md` if you're thinking about skipping or deferring engine stages.

heso's current engine is `heso-engine-fetch` (pure-Rust HTTP + HTML, no browser dep) per [ADR 0012](../../decisions/0012-fetch-only-native-engine.md). The Servo-specific embedding research that used to live in `../servo-internals/` was deleted when ADR 0012 ruled out the Servo path; it's recoverable from git history if a future ADR brings Servo back as a bundled JS-capable engine.

## What lives here

- **`rendering-pipeline.md`** — Parse → DOM → CSSOM → style → layout → paint → composite. What each stage costs and which stages an agent-first browser can skip or defer. Useful when planning what a future JS-capable engine should do (or not).
- **`agent-first-design.md`** — What's actually different about a browser whose user is an LLM. AX tree first, semantic selectors, structured errors with candidates, idempotency. The design-intent document that the terminal-shaped primitive vocabulary ([ADR 0010](../../decisions/0010-primitives-as-terminal-commands.md)) translates into concrete primitives.
- **`determinism.md`** — Sources of nondeterminism in a browser (wall clock, RNG, GPU, fonts, network, JIT, GC) and per-source mitigation strategies. Anchors [ADR 0008](../../decisions/0008-deterministic-execution.md). The static-fetch engine satisfies most of this for free (no clock, no RNG); the rest applies when we add a JS-capable engine.
- **`ladybird-architecture.md`** — Ladybird as a potential v2 engine if/when we want a real browser engine but don't want Chromium. Multi-process design, ongoing Rust migration. Alpha shipping 2026, beta 2027.

## Pointers to load-bearing notes

| Question you have | Note |
|---|---|
| "Should we expose `click(x, y)` or `click(selector)`?" | `agent-first-design.md` |
| "Can we skip paint entirely for headless agent runs?" | `rendering-pipeline.md` |
| "How do I make this primitive reproducible?" | `determinism.md` + ADR 0008 |
| "When do we add a real browser engine?" | `ladybird-architecture.md` + ADR 0012's "Negative consequences" |

## Refresh discipline

These notes are evergreen — they're about how browser engines work in general, not specific versions. Update only if a load-bearing claim is contradicted by reality (e.g. if the AX-tree-first idea stops being best practice).
