## 0013. Engine as semantic extractor

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay
- **Relates to:** [ADR 0012 — fetch-only native engine](0012-fetch-only-native-engine.md), [ADR 0010 — terminal-shaped primitives](0010-primitives-as-terminal-commands.md), [ADR 0008 — deterministic execution](0008-deterministic-execution.md)

## Context

ADR 0012 settled the engine implementation (native Rust, `reqwest + scraper`, no Chrome, no Node). But that ADR only committed the engine to producing two views:

1. **Visible body text** — `extract_visible_text`: a flat string with `<script>` / `<style>` / `<noscript>` / `<template>` stripped.
2. **Heading-derived tree** — `HtmlTree`: sections defined by `<h1>`–`<h6>`, with `intro` text under each, navigable via `ls` / `cat`.

Both views treat the page as **text inside a hierarchy**. They throw away everything else the document already declares in structured form:

- **Schema.org JSON-LD** blocks (`<script type="application/ld+json">`) — `Organization`, `Product`, `Article`, `FAQPage`, `BreadcrumbList`, `Recipe`, `LocalBusiness`. Every page that wants Google rich-snippet treatment has these. They are **already JSON**.
- **OpenGraph** (`<meta property="og:*">`) — title, description, image, type, site_name. Every page that wants a decent social-preview card has these.
- **Twitter cards** (`<meta name="twitter:*">`) — title, description, card type.
- **Standard SEO meta** — `description`, `keywords`, `author`, `robots`, `theme-color`.
- **Canonical URL**, **icons**, **`<html lang>`**.

When the user critiqued the M2 surface ("why arent we building the engine part that takes all the code for the website thats static and puts it in our engine and our engine should be able to turn that into smth that agent can do easier"), this was the gap. The engine was passing structured data through as if it were prose. The agent then spent context budget *reading prose* to recover facts the page had already structured.

Concrete cost: on a typical B2B SaaS marketing page (suprvisr.ai), answering "what does this company do / where are they based / what do they sell" took **5–7 LLM tool calls** (read preamble → ls / → cat /features → cat /pricing → ...), each one a round-trip burning latency and context. The page's own JSON-LD `Organization` block had `description`, `areaServed`, `knowsAbout`, and `contactPoint` declared inline — answer available with **zero** tool calls if the engine surfaced it.

## Decision

**The engine extracts and surfaces structured metadata as a first-class view of every page**, alongside the existing visible-text and heading-tree views. The agent layer pre-loads it into the LLM's first turn so the LLM rarely needs a tool call for facts the page already declared.

### Shape

New `PageMetadata` struct on `FetchPage`, populated during `EngineApi::open`:

```rust
pub struct PageMetadata {
    pub jsonld: Vec<serde_json::Value>,          // Schema.org docs in document order
    pub opengraph: BTreeMap<String, String>,     // og:* without prefix, sorted
    pub twitter: BTreeMap<String, String>,       // twitter:* without prefix, sorted
    pub meta: BTreeMap<String, String>,          // other <meta name="...">, sorted
    pub canonical: Option<String>,               // <link rel="canonical">
    pub icons: Vec<String>,                      // icon / apple-touch-icon / shortcut-icon
    pub lang: Option<String>,                    // <html lang="...">
}
```

All maps are `BTreeMap` for sorted serialization. All vectors preserve document order. The whole module has zero clocks, zero RNG — determinism (ADR 0008) is preserved for free.

Implemented in [`crates/heso-engine-fetch/src/metadata.rs`](../crates/heso-engine-fetch/src/metadata.rs).

### CLI surface

- `heso meta <url>` — fetch + extract + print `PageMetadata` JSON.
- `heso open <url>` — fetch once, return the agent-shaped bundle:
  ```json
  { "url": ..., "title": ..., "description": ..., "metadata": {...}, "tree": {...} }
  ```
  This is the single-subprocess call external agents prefer — one `spawn`, all the pre-computed context.

The pre-existing `heso fetch` / `heso tree` / `heso ls` / `heso cat` / `heso run` are unchanged.

### Determinism

The HTML document is parsed exactly **once** per `EngineApi::open` and shared across:
- `extract_visible_text_from_doc(&doc)` → body text
- `metadata::extract(&doc)` → structured metadata
- `tree::build_tree_from_doc(&doc, &url)` → heading tree

That's three views off one parse. Previously the body-text and tree extractors each parsed independently; the new `_from_doc` variants let them share.

### Agent integration

The Flue test agent (`heso-test-agent`) was switched from `heso tree` to `heso open` in one subprocess call. The `navigate-page` skill receives:

- `metadata` (the new structured data)
- `preamble` (root.intro, clipped to 600 chars — page hero text before first heading)
- `outline` (expanded: top-level sections with `intro_preview` ≤200 chars + immediate sub-section headings)

The skill instructions teach the LLM to check `metadata.jsonld` and `preamble` **before** reaching for `heso_ls` / `heso_cat`. Most "what is this page about / who runs it / what do they sell / where are they based" questions now answer with **zero** tool calls.

## Alternatives considered

- **Leave it as a planner concern.** Have the planner emit a `find -type metadata` primitive that walks the DOM at trace time. Rejected: the metadata is universally useful, costs nothing to extract eagerly, and pulling it into every plan would just couple the planner to vocabulary the engine should own.
- **Make metadata its own subcommand only (`heso meta`), don't bundle into `heso open`.** Rejected: forces every agent to do two subprocess fetches per page (one for tree, one for metadata), wasting an HTTP round-trip. The page is loaded once; we should serve everything off that one parse.
- **Include metadata as a field inside `HtmlTree`.** Rejected: `HtmlTree` is specifically about heading-defined sections. Metadata is page-level, not section-level. Keeping them sibling fields on `FetchPage` (and on the `heso open` JSON) keeps the conceptual model clean.
- **Extract more right now (forms, tables, repeating-card patterns).** Deferred: this ADR commits to the *direction* — "engine transforms HTML into structured agent views" — and ships the highest-leverage extractor (metadata) first. Forms / tables / action-graph come in follow-up tasks; each needs its own design pass (form specs need to compose with the future `submit` primitive; action graph needs stable `@e0/@e1/...` refs that also feed `find` / `cat @ref`).

## Consequences

**Positive:**
- **Context budget collapse for common questions.** Where a "what does this company do" query used to need 5–7 LLM round-trips, it now needs 0 (metadata answers it directly).
- **JSON-LD makes the engine smart for free.** Every site with SEO investment (basically every modern marketing / docs / e-commerce site) declares structured data the engine now surfaces.
- **Better honest answers.** When a page genuinely doesn't have the answer, the LLM sees that quickly (metadata absent + preamble doesn't cover it + outline doesn't include the right section) and can say so honestly without burning the 10-call cap searching.
- **Determinism preserved.** Sorted maps, document-ordered vectors, no clocks, no RNG. `heso open` against the same URL + recorded bytes returns byte-identical JSON.
- **Sets the pattern for the next extractors.** This ADR is the first installment of "engine as semantic extractor." Forms, tables, action graphs, structured-list detection all fit the same template (parse once, extract a typed view, expose via `heso <noun>` + bundled into `heso open` + pre-loaded into skill args).

**Negative:**
- **Adds engine surface area.** Three more types (`PageMetadata`, the two new CLI commands) to maintain. Mitigated by being self-contained: `metadata.rs` is ~250 LOC including tests, doesn't touch the trace runner or primitives, and the new CLI commands are thin wrappers around `FetchEngine::open`.
- **`heso open` is a fourth way to fetch a page.** Plus `fetch`, `tree`, `ls`, `cat`. Documented in the binary banner and each command's doc-comment. Long-term, the planner (T-022) will subsume direct CLI use, but for now the multiple verbs reflect the multiple views agents want.
- **Sites that declare nothing get nothing.** Metadata won't help on a hand-written HTML page from 2003 — the engine surfaces what's there, doesn't invent. Mitigation: the tree / preamble / ls / cat path still works as before.

## Future work (out of scope for this ADR)

This ADR locks in *one* extractor (metadata). Successive ADRs / tasks should add:

1. **Form specs** (`<form>` → `{ action, method, fields: [{name, type, label, required, options}] }`). Compose with the planned `submit` primitive.
2. **Tables as data** (`<table>` → `{ headers, rows }` JSON).
3. **Repeating-pattern detection** (cards / product grids / article lists → arrays of structured items). The hard part is heuristic detection without false positives.
4. **Action graph** — every link / button gets a stable `@e0/@e1/...` ref, indexed by role / label. Feeds the future `find -role` / `cat @ref` primitives (ADR 0010).
5. **Single-parse refactor extended** — body text already shares the parse; visible-text walker should fold AX-tree extraction into the same DOM traversal once it lands.

Each item above is a separate task. This ADR's contribution is establishing the principle and shipping the first instance.

## References

- ADR 0008 (determinism) — preserved by sorted maps + document-ordered vectors + no clocks.
- ADR 0010 (terminal-shaped primitives) — future `find` / `cat @ref` will leverage the same engine-level extraction discipline.
- ADR 0012 (fetch-only native engine) — implementation lives in `heso-engine-fetch`.
- [Schema.org](https://schema.org/) — the vocabulary JSON-LD blocks declare.
- [Open Graph protocol](https://ogp.me/) — `og:*` meta specification.
