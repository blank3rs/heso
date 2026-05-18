# How to use the research wiki

This repo treats `research/` as a **wiki** — a body of durable, searchable knowledge that AI agents (and humans) can query before reinventing or hallucinating. This file is the user manual for that wiki.

## The wiki entry point

**Always start at [`research/INDEX.md`](../../research/INDEX.md).** It has:

- A **use-case lookup** table ("if you're about to do X, read Y").
- A **by-topic** map of every note.
- A **by-concept** cross-cutting map for things that span multiple notes.
- A **keyword search** list (plain text grep targets).

Don't try to discover notes by `ls research/`. The INDEX is curated; the directory is mechanical.

## When to consult the wiki vs. other sources

heso has several knowledge sources. They are not interchangeable:

| Question shape | Source | Why |
|---|---|---|
| "What does this crate's `do_x()` method do?" | **Context7** (`mcp__context7__resolve-library-id` + `mcp__context7__get-library-docs`) | Current per-crate docs straight from upstream. Always fresher than the wiki. |
| "Where is `foo` defined? Who calls it?" | **rust-analyzer LSP** | Semantic codebase navigation, free. |
| "Who changed this file last? When did X land?" | **`git log` / `git blame`** | Authoritative history. |
| "What's the current focus / what task am I picking up?" | **`state.json`** | Live project state. |
| "What was decided about X and why?" | **`decisions/`** (ADRs) | Immutable architectural decisions. |
| "Why does Y in this codebase work the way it does? What did we learn? What's the prior art? What do LLMs usually get wrong about Z?" | **`research/`** (the wiki — start at `INDEX.md`) | Durable project-specific knowledge that doesn't fit elsewhere. |

If the question is "what does this crate do," it's Context7. If it's "what does this *project* know about that crate / pattern / concept," it's the wiki.

## How to "search" the wiki

The wiki is markdown files. There is no fancy search engine. The retrieval techniques that work:

1. **Read `INDEX.md` first.** It exists so you don't have to skim every file.
2. **Use the keyword grep list** in `INDEX.md` to find notes mentioning specific terms.
3. **Use Grep tool** on `research/` directly if you have a precise string (`Grep pattern "MOZJS_ARCHIVE" path "research"`).
4. **Read note headers only.** Every note starts with a Summary section — that's a 2–3 sentence executive summary. Read 10 of those in 2 minutes; read full notes only when you've narrowed down.
5. **Follow cross-links inside notes.** Notes link to each other and to ADRs.

## The "always check the wiki" rule

Before doing any of the following, check `INDEX.md` (or the relevant note directly):

- **Writing Rust code** → `rust-llm-mistakes/README.md`. At minimum scan the "Top 20" list.
- **Touching anything related to the engine** → `browser-engines/agent-first-design.md` + `servo-internals/embedding-api-v0-1-0.md`.
- **Proposing a Servo source modification** → `browser-engines/conversion-strategies.md` (Layer 3 is rarely the right call) and `servo-internals/verso-prior-art.md` (cautionary tale).
- **Adding any agent-facing API surface** → `browser-engines/agent-first-design.md`.

If you violate one of the rules in a wiki note (e.g. you `.clone()` to silence the borrow checker, or you accept coordinates in an `EngineApi` method), expect it to come up in review.

## Citing the wiki

When you make a non-obvious decision based on a wiki note, **cite the note** in:

- **Commit messages** — `fix borrow without clone (per research/rust-llm-mistakes#ownership)`
- **Code comments** — only when the *why* is non-obvious (per the project's comment conventions). Cite the note in one line: `// see research/browser-engines/agent-first-design.md for AX-tree-first rationale`
- **ADRs** — every ADR's Context or References section should list any research notes that informed it
- **`state.json` task notes** — link via `links.research`

This keeps the wiki *load-bearing* rather than decorative.

## Updating the wiki

You can edit any note when you have new information. The rules:

1. **Don't delete.** If a claim is no longer true, prepend a dated warning at the top of the note. The history matters.
2. **Update the `Last updated` field** at the top of the note.
3. **Update `INDEX.md`** if you added/removed/significantly changed a note's purpose.
4. **Append to the keyword search list** in `INDEX.md` if your update introduces new searchable terms.
5. **Commit research updates separately** from code commits — easier to find later.

## Adding a new note

1. Pick the right subfolder. If none fits, create one (`research/<subfolder>/`) and add a one-line entry in `INDEX.md`'s "By topic" table.
2. Use this header template:
   ```markdown
   # <Title>

   **Topic:** <one-line summary>
   **Last updated:** YYYY-MM-DD
   **Status:** initial research | updated | stale-needs-review

   ## Summary

   2–3 sentence executive summary so an agent can decide whether to read the rest.

   ## <main content with H2/H3 sections>

   ## References

   - [Title](url)
   ```
3. Be specific. Generic facts ("browsers parse HTML") add no value. Project-specific judgment ("for heso we should X because Y") is what makes the wiki worth reading.
4. Cite sources inline. Every non-obvious claim links to its origin.
5. Add a row to `INDEX.md` "By topic" and a use-case to "Use-case lookup" if applicable.
6. Cross-link from any related ADR or task.

## What the wiki is *not*

- **Not a tutorial.** Don't write "How to use Servo, step by step." Servo's docs do that.
- **Not a code dump.** Notes describe ideas, decisions, and project-specific reasoning — not code that should live in `crates/`.
- **Not a TODO list.** That's `state.json`.
- **Not an ADR.** Decisions go in `decisions/`. The wiki gives ADRs their factual basis.

## Quick reference

- **Where to start:** `research/INDEX.md`
- **Before writing Rust:** `research/rust-llm-mistakes/README.md`
- **Before touching engine code:** `research/servo-internals/embedding-api-v0-1-0.md`
- **Before designing an agent-facing API:** `research/browser-engines/agent-first-design.md`
- **To add a note:** copy the header template, add to INDEX, cross-link
