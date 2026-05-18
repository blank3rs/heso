# AGENTS.md

> Primary entry point for any AI coding agent (Claude Code, Codex, Cursor, Cline, Windsurf, Continue, Copilot, Gemini CLI) working in this repository.

If you are an agent and just landed here: read this file end-to-end, then `cat .agent/MAP.md`. Total cost: ~300 tokens. After that you'll know where everything lives.

---

## What heso is

Headless browser for the agent-relevant half of the web. 30 MB single binary, sub-100ms cold start, no Chromium. Handles fetch, parse, JS hydration, forms, clicks, sessions. Returns structured agent-shaped JSON with content-hashed signed receipts. See [`README.md`](README.md) for the full public pitch and [ADR 0016](decisions/0016-positioning-headless-browser-for-agents.md) for the positioning rationale. One run produces a **plat** (the cartography artefact, ADR 0015). The category we coined is **agentware**: software whose user is software.

## The first rule

**This repo is itself agentware.** Every directory exists so an AI agent can be productive in this codebase within minutes. If something here isn't agent-friendly, that's a bug — open an issue or fix it.

## Where things live

- [`crates/`](crates/) — all Rust crates (Cargo workspace members). Each does one thing (Unix philosophy).
- [`.agent/`](.agent/) — meta directory specifically for AI agents. Maps, conventions, workflows, glossary, how-tos.
- [`decisions/`](decisions/) — Architecture Decision Records (ADRs), numbered. Every significant architectural choice has one.
- [`research/`](research/) — captured research notes, prior art, papers, links. Read these before reinventing.
- [`proposals/`](proposals/) — RFCs for changes that need design discussion before code.
- [`docs/`](docs/) — long-form user-facing docs.
- [`state.json`](state.json) — current focus, milestones, tasks, open questions, blockers. **Read this to know what to work on.**

## The agent contract (read this first)

heso exposes exactly **one tool** to agents that consume it: `heso.run(start_url, request, options?) → { status, data, receipt, cost }`. Agents give heso a starting URL and a plain-English description of what they want. heso plans the steps, executes them deterministically, returns the answer plus a signed receipt.

If you are working in this codebase, internalize the layered architecture:

```
   agent layer     →   heso.run (the one MCP tool)
                       │
                       ▼
   planner          →   request + page  →  trace
                       │
                       ▼
   trace runner     →   trace + engine  →  result + signed receipt
                       │
                       ▼
   primitives layer →   the page is a directory; primitives are shell commands:
                       pwd, ls, cd, cat, find, grep, echo, rm,
                       click, submit, wget, wait, screenshot, eval, diff
                       (15 primitives — see ADR 0010)
                       │
                       ▼
   EngineApi        →   heso-engine-fetch — native Rust (reqwest + html5ever).
                       heso-engine-js wraps QuickJS (rquickjs) for JS-handler
                       execution (ADR 0014 Phase 1A landed; DOM types in 1B).
                       Swappable via ADR 0002.
```

When you design or modify anything:
- The **agent layer** (one tool) is the only public surface. Treat it as load-bearing.
- The **planner**, **trace runner**, **primitives layer**, and **engine** are internal. We can rewire them as much as we want without breaking agents.
- The primitives layer is **terminal-shaped**: the current page is the working directory, elements are files, cookies and storage live under `/env/`. New primitives have to fit the metaphor — if you find yourself reaching for a verb that isn't a shell command, stop and re-read ADR 0010.
- See ADR 0009 (one tool), ADR 0010 (primitive vocabulary), [`docs/the-one-tool.md`](docs/the-one-tool.md), and [`skills/heso/SKILL.md`](skills/heso/SKILL.md) for the full contract.

## Reflexes you should have

1. **Before writing any Rust code** → `cat research/rust-llm-mistakes/README.md` (at minimum the top-20 quick-reference). Catalogues the specific mistakes LLMs make in Rust. Violations get caught in review.
2. **Before writing code that uses any external library** → call `mcp__context7__resolve-library-id` then `mcp__context7__get-library-docs`. Don't write API calls from memory.
3. **Before making an architectural change** → `cat decisions/README.md` for the supersession table, then `cat decisions/NNNN-*.md` for the relevant ADR. If your change conflicts, propose a new ADR that supersedes it.
4. **Before reinventing something** → `cat research/INDEX.md`. It has a use-case lookup table that maps "what you're about to do" to "what to read first."
5. **Before designing or modifying anything below the one tool** → `cat decisions/0009-heso-run-single-tool.md` (layering), `cat decisions/0010-primitives-as-terminal-commands.md` (vocabulary), `cat decisions/0012-fetch-only-native-engine.md` and `cat decisions/0014-bundled-quickjs-agent-dom.md` (engines). Design principles live in `research/browser-engines/agent-first-design.md`.
6. **Before implementing a primitive** → cross-check at least three sources: the ADR that names it, the crate doc-comment, and [`crates/heso-primitives/src/lib.rs`](crates/heso-primitives/src/lib.rs) itself. If those three disagree (it has happened — see ADR 0010's context section), **stop and surface the disagreement to the human before writing code**. Doc repetition is not consensus.
7. **Before touching engine code** → `cat decisions/0012-fetch-only-native-engine.md` and `cat decisions/0014-bundled-quickjs-agent-dom.md`, then read the relevant `crates/heso-engine-*/src/lib.rs`. ADR 0011 is historical, superseded.
8. **Before adding a crate** → `cat .agent/HOWTO/add-a-crate.md`.
9. **Before writing a long-running pattern or learning future-you will need** → capture it as a research note (`cat .agent/HOWTO/use-research.md`) or as an ADR (`cat .agent/HOWTO/add-an-adr.md`).
10. **When you finish a task** → update [`state.json`](state.json) per `cat .agent/HOWTO/update-state.md`.

## Reading the repo cold (fresh-agent checklist)

If you've just landed in this repo and are about to do real work, walk this checklist *in order* before touching code. Skipping steps is what leads to bugs like "shipping `click_link`/`click_button` because five docs say so when ADR 0010 — and the LLM-friendly metaphor — actually wants `cd` + `click`."

1. **Read this file** end-to-end. (You're doing it.)
2. **`cat .agent/MAP.md`** for the 150-token orientation.
3. **`jq '.current_focus' state.json`** then **`jq '.tasks[] | select(.id == "<next_action_id>")' state.json`** to see what specifically to do next.
4. **For every ADR your task touches:** `cat decisions/README.md` for the supersession column, then `cat decisions/NNNN-*.md` for the ADR itself AND check the *Status* line. An ADR may say "Accepted" but be partially superseded by a later one. ADR 0009 is the canonical example — it locks the agent surface but its primitive list is superseded by ADR 0010.
5. **For every named concept in your task** (primitive name, crate name, file path) — grep the repo. If you find more than one spelling, you've found documentation drift. **Don't pick one and ship; surface the drift to the human first.** This repo has had cases where five files agreed on one spelling and a research note disagreed — the research note encoded the real design intent.
6. **Trust the implementation over paraphrased docs.** If [`crates/heso-primitives/src/lib.rs`](crates/heso-primitives/src/lib.rs) and a `.md` file disagree, the code is the most recent ground truth (it had to compile); the doc is most likely stale. Update the doc, don't change the code to match.
7. **Trust the user over your own synthesis.** When in doubt, ask one focused question with the candidate options. Cheaper than reverting.

## Knowledge sources at a glance

| Question shape | Source |
|---|---|
| "What does this crate's API look like?" | **Context7** (`mcp__context7__*`) |
| "Where is `foo` defined?" | **rust-analyzer LSP** |
| "What's the current focus / next task?" | `jq '.current_focus' state.json` |
| "What was decided about X?" | `cat decisions/README.md` then the ADR |
| "What does this project know about X?" | `cat research/INDEX.md` then drill in |
| "How do I do operation X in this repo?" | `cat .agent/HOWTO/<x>.md` |
| "What's the convention for Y?" | `cat .agent/CONVENTIONS.md` |
| "What does term Z mean here?" | `cat .agent/GLOSSARY.md` (no paraphrasing — terms are load-bearing) |

## What we will NOT do

- Build a human-facing GUI. heso is headless-only. If you find yourself reaching for `gtk`, `iced`, `egui`, or any windowing crate — stop and re-read the project thesis.
- Wrap Chromium. We are building native single-binary Rust (see ADR 0012, ADR 0014).
- Add features humans want (printing, bookmarks, settings UI, history UI, sync). The user is software.
- Add any API that violates determinism without `unsafe_` in the name. heso is **completely deterministic by default** (ADR 0008). Real wall clocks, real RNG, real network, GPU rendering — all of these are off in `deterministic` mode. If you genuinely need entropy, the API name must say so. See [`research/browser-engines/determinism.md`](research/browser-engines/determinism.md).

## When in doubt

`cat .agent/MAP.md`, then `cat .agent/PRIORITIES.md`, then `jq '.current_focus' state.json`. That triple tells you where you are, what matters right now, and what specifically to do next.
