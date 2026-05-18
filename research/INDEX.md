# Research Wiki — Index

> The searchable entry point to all research notes in this repo. Read this file first; jump to specifics by topic, by use-case, or by note name.

This index is the "wiki home page." It's designed so an AI agent (or human) can find what's relevant in **under 30 seconds of skimming**. Every research note in the repo is listed here with a one-line summary. If you're about to make a decision or write non-trivial code, scan the "Use-case lookup" table first.

If you're a new agent landing in this repo, also read [`.agent/HOWTO/use-research.md`](../.agent/HOWTO/use-research.md) — it tells you when and how to consult the wiki vs other sources (Context7, rust-analyzer, code).

---

## Use-case lookup

If you're about to do this → read this:

| You're about to ... | Read |
|---|---|
| Write or modify any Rust code | [`rust-llm-mistakes/README.md`](rust-llm-mistakes/README.md) — top 20 anti-patterns, all 10 topic areas |
| Look up an external crate's API | (Use Context7 first; this wiki is for project-wide concepts, not per-crate docs) |
| Understand what the agent actually calls (the *only* public surface) | [`docs/the-one-tool.md`](../docs/the-one-tool.md) + [ADR 0009](../decisions/0009-heso-run-single-tool.md) + [`skills/heso/SKILL.md`](../skills/heso/SKILL.md) |
| Design a primitive operation that the planner can emit | [ADR 0010](../decisions/0010-primitives-as-terminal-commands.md) for the vocabulary; [`browser-engines/agent-first-design.md`](browser-engines/agent-first-design.md) for the design principles (AX-tree first, semantic selectors, structured errors) |
| Understand the engine choice (why no Chrome / Servo) | [ADR 0012](../decisions/0012-fetch-only-native-engine.md) and the historical chain [ADR 0011](../decisions/0011-chromium-cdp-first-engine.md) → [ADR 0003](../decisions/0003-servo-as-first-engine.md) |
| Add a new primitive surface on top of the fetch engine | [`crates/heso-engine-fetch/src/lib.rs`](../crates/heso-engine-fetch/src/lib.rs) — see the "What it does not do" section for honest limits |
| Think about skipping or deferring an engine stage | [`browser-engines/rendering-pipeline.md`](browser-engines/rendering-pipeline.md) |
| Add any API surface (anywhere) — must respect determinism | [`browser-engines/determinism.md`](browser-engines/determinism.md) + ADR 0008 |
| Wire a clock, RNG, network call, or animation | [`browser-engines/determinism.md`](browser-engines/determinism.md) — see "The nondeterminism surface" |
| Consider swapping engines (Ladybird, custom, etc.) | [`browser-engines/ladybird-architecture.md`](browser-engines/ladybird-architecture.md) |
| Write async Rust (anything with `.await`) | [`rust-llm-mistakes/README.md#2-async--await`](rust-llm-mistakes/README.md) |
| Define a new error enum | [`rust-llm-mistakes/README.md#3-error-handling`](rust-llm-mistakes/README.md) |
| Argue for `Rc<RefCell<T>>` or `Arc<Mutex<T>>` | [`rust-llm-mistakes/README.md#1-ownership-borrowing-and-lifetimes`](rust-llm-mistakes/README.md) — usually wrong |
| Write `unsafe` code | [`rust-llm-mistakes/README.md#6-unsafe`](rust-llm-mistakes/README.md) + Rustonomicon |

---

## By topic

### Rust (the language and its idioms)

| Note | Summary |
|------|---------|
| [`rust-llm-mistakes/README.md`](rust-llm-mistakes/README.md) | The big one. 10 topic areas covering ownership/borrowing, async, error handling, iterators/strings, traits/dyn, unsafe, Cargo, API hallucinations, performance, testing. Top 20 quick-reference at the end. **Read before writing Rust.** |

### Browser engines (general background — engine-agnostic research)

| Note | Summary |
|------|---------|
| [`browser-engines/README.md`](browser-engines/README.md) | Map of `browser-engines/`. Start here for the engine-side wiki. |
| [`browser-engines/rendering-pipeline.md`](browser-engines/rendering-pipeline.md) | The 7 stages from HTML bytes to pixels (parse → DOM → CSSOM → style → layout → paint → composite). Which stages an agent-first browser can skip. **Load-bearing** for any future engine work. |
| [`browser-engines/agent-first-design.md`](browser-engines/agent-first-design.md) | What's different about a browser whose user is an LLM. AX tree first, semantic selectors, bulk ops, job-based async, structured errors. **The design-intent document.** |
| [`browser-engines/determinism.md`](browser-engines/determinism.md) | Sources of nondeterminism in a browser (wall clock, RNG, GPU, fonts, network, JIT, GC) and the per-source strategy heso uses. **Required reading before adding any API.** Anchors ADR 0008. |
| [`browser-engines/ladybird-architecture.md`](browser-engines/ladybird-architecture.md) | Ladybird as a future engine candidate. Multi-process design, Rust migration, why not today. |

### Engine that ships today

heso v1 ships [`heso-engine-fetch`](../crates/heso-engine-fetch/src/lib.rs) per [ADR 0012](../decisions/0012-fetch-only-native-engine.md) — pure Rust HTTP + HTML, no browser dep. Read the crate's own module doc-comment for what's in scope and what's deferred. The `browser-engines/` research notes above stay relevant for the next engine when we add one (likely a bundled WebView for JS support).

---

## By concept (cross-cutting)

If you want to understand one concept across multiple notes:

| Concept | Notes that touch it |
|---|---|
| **Accessibility tree as primary representation** | `browser-engines/agent-first-design.md`, `browser-engines/rendering-pipeline.md` |
| **`EngineApi` trait boundary** | ADR 0002 (why it exists), `crates/heso-engine-api/src/lib.rs` (the trait), `crates/heso-engine-fetch/src/lib.rs` (the one current impl) |
| **What to skip / defer for headless agent runs** | `browser-engines/rendering-pipeline.md`, `browser-engines/agent-first-design.md` |
| **Engine-swap escape hatch** | ADR 0002 + `browser-engines/ladybird-architecture.md` |
| **Determinism (first-class property — ADR 0008)** | `browser-engines/determinism.md`, `browser-engines/agent-first-design.md` (§ Deterministic execution) |
| **The one tool / agent contract (ADR 0009)** | `docs/the-one-tool.md`, `skills/heso/SKILL.md`, `decisions/0009-heso-run-single-tool.md`, `browser-engines/agent-first-design.md` (now framed as internals) |
| **Terminal-shaped primitives (ADR 0010)** | `decisions/0010-primitives-as-terminal-commands.md`, `crates/heso-primitives/src/lib.rs`, `browser-engines/agent-first-design.md` (the underlying design principles) |
| **Native fetch engine choice (ADR 0012)** | `decisions/0012-fetch-only-native-engine.md`, `crates/heso-engine-fetch/src/lib.rs` |
| **Async + Rust** | `rust-llm-mistakes/README.md` §2 |
| **Error handling** | `rust-llm-mistakes/README.md` §3, `browser-engines/agent-first-design.md` (structured errors with candidates) |

---

## Search by keyword

Plain-text terms most likely to be useful for in-document grep:

- `accessibility` `ARIA` `AX tree` `AT tree` → `agent-first-design.md`, `rendering-pipeline.md`
- `bench` `flamegraph` `samply` `DHAT` → `rust-llm-mistakes/README.md` §9
- `block_on` `MutexGuard` `tokio::sync` → `rust-llm-mistakes/README.md` §2
- `clippy` lint names → `rust-llm-mistakes/README.md` (every section's Detection block)
- `composite` `WebRender` `display list` → `rendering-pipeline.md`
- `determinism` `idempotency` `state diff` → `determinism.md`, `agent-first-design.md`, ADR 0008
- `EngineApi` `trait boundary` `swappable` → ADR 0002, `crates/heso-engine-api/src/lib.rs`
- `fetch engine` `html5ever` `scraper` `reqwest` → ADR 0012, `crates/heso-engine-fetch/src/lib.rs`
- `Ladybird` `LibWeb` `LibJS` → `ladybird-architecture.md`
- `lifetime` `borrow checker` `'static` → `rust-llm-mistakes/README.md` §1

---

## Adding to the wiki

See [`.agent/HOWTO/add-research-note.md`](../.agent/HOWTO/add-research-note.md) (or `use-research.md` for the broader workflow). When you add a note:

1. Pick or create the right subfolder (`browser-engines/`, `servo-internals/`, `rust-llm-mistakes/`, etc.).
2. Use the header template (Topic / Last updated / Status / Summary / sections / References).
3. Add a row to the "By topic" table above.
4. Add to the "Use-case lookup" table if your note answers a specific recurring question.
5. Add relevant keywords to the keyword search list.
6. Cross-link from any related ADRs (`decisions/`) and tasks (`state.json`).

Notes don't expire — they go stale. If a note's claims are no longer current, prepend a dated warning at the top instead of deleting.

---

## What is *not* in the wiki

- **Per-crate API docs.** Use Context7 (`mcp__context7__resolve-library-id`, `mcp__context7__get-library-docs`) for that.
- **Codebase navigation.** Use rust-analyzer LSP (installed) for definitions, references, types.
- **Recent commits / who-changed-what.** Use `git log` / `git blame`.
- **Project state (tasks, milestones, blockers).** That's in [`state.json`](../state.json), not here.
- **Architectural decisions.** Those live in [`decisions/`](../decisions/) as ADRs. The wiki *informs* ADRs but doesn't replace them.
