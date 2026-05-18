## 0015. heso as cartographer (not browser)

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay
- **Relates to (reframes, does not supersede):** [ADR 0009 — `heso.run` single agent tool](0009-heso-run-single-tool.md), [ADR 0010 — primitives as terminal commands](0010-primitives-as-terminal-commands.md), [ADR 0012 — fetch-only native engine](0012-fetch-only-native-engine.md), [ADR 0013 — engine as semantic extractor](0013-engine-as-semantic-extractor.md), [ADR 0014 — bundled QuickJS + agent-shaped DOM](0014-bundled-quickjs-agent-dom.md)

## Context

Across this project's life heso has been described as: a headless browser for agents (early), a single-tool agent surface (ADR 0009), a terminal-shaped page interface (ADR 0010), a semantic extractor (ADR 0013), and most recently "**Chromium for agents, not humans**" — the agent-shaped browser engine frameworks would drive instead of Chromium. Each framing was true at the time. None is the right one going forward.

The "Chromium for agents" framing died on contact with the 2026 landscape during today's competitive reviews. Two independent reviewers landed on the same diagnosis:

- **Lightpanda already exists** with V8 + 17 CDP domains + Playwright drop-in + 11× faster benchmarks. The "smaller headless browser" lane has a winner that's not us.
- **Vercel Labs' [`agent-browser`](https://github.com/vercel-labs/agent-browser)** already shipped the `@e1`/`@e2` action-graph + `click @e2`/`fill @e3 "value"` vocabulary. That wedge is taken.
- The QuickJS+handwritten-DOM bet in [ADR 0014](0014-bundled-quickjs-agent-dom.md) is 6–12 months for vanilla onclick + jQuery, 2+ years for React/Next, **never** for dashboards. Lightpanda picked V8 because every shortcut on JS-engine completeness gets eaten by real sites. We were about to discover the same wall from the other side.

In the same session, the user's *original* phrasing — "*a dir for a js animation that leads to information*" — clicked into focus. heso isn't a smaller Chromium. heso isn't a browser at all. heso is a **cartographer**.

## Decision

**heso is a cartographer for web pages.** Its product is the **cartography artifact**: a static, text-based map of every reachable state of a given page, materialized as a filesystem tree where each directory is a reachable state and each edge is a user action (click, fill, submit, navigate). Agents *read the map* to find what they want; they do not drive a live browser through trial-and-error.

```
URL → heso → cartography
              {
                root: { state_id, tree, metadata, actions[], … },
                transitions: [
                  { action_ref: "@e5", kind: "navigate", target: <cartography> },
                  { action_ref: "@e12", kind: "reveal", target: <cartography> },
                  …
                ]
              }
```

The artifact is:
- **Static.** No live session, no per-step round trip. Once built, navigable like a filesystem.
- **Cacheable + shareable.** Same page → same artifact (modulo network state and the seed). Multiple agents on multiple machines can read the same map without re-fetching.
- **Signed.** A BLAKE3 hash over the canonical-JSON serialization gives every cartography a content id. Signed cartographies are verifiable evidence of what a page looked like at fetch time.
- **Deterministic.** Per [ADR 0008](0008-deterministic-execution.md), same seed + same recorded network = byte-identical cartography. Auditable.

The agent's loop changes from `act → observe → decide → act → observe → decide` (the universal pattern of Playwright / Stagehand / Browser Use / Lightpanda) to `read → answer`. Most queries are now zero tool calls — the answer is in the cartography the moment the page is opened. The remaining queries become "navigate to the path in the map that has the answer."

## What this reframes (and what it preserves)

Everything heso has built so far becomes a component of the cartography rather than a standalone feature:

| Already-shipped layer | Reframed role |
|---|---|
| [ADR 0012] static fetch engine | The "navigate-by-URL" edge case of cartography (zero-action paths). |
| [ADR 0013] structured metadata | The cartography's *root-state structured facts* — what's true about the page without clicking anything. |
| Heading-tree (`ls`/`cat`) | The cartography's *static slice* — the part visible in the initial DOM. |
| [ADR 0013] action graph (`@e0/@e1/…` refs) | The cartography's *edge set* — every interactive element is a candidate transition. |
| `heso serve` JSON-RPC protocol | The integration surface — frameworks ask for cartographies, not for browser sessions. |
| [ADR 0014] QuickJS engine | The exploration engine — runs handlers in a sandbox to *discover* edges, not to render a live browser. JS engine is **infrastructure for cartography**, not a competitor to Chromium. |
| [ADR 0008] deterministic receipts | The cryptographic anchor — a cartography's `state_id` is its content hash. |

What this reframes:
- The pitch in `README.md` and `AGENTS.md`: heso is "a cartographer for web pages," not "a browser for agents." The headline noun is **map**, not **browser**.
- The competitive frame: we are *not* in the "smaller Chromium" lane (that's Lightpanda's). We are not in the "JS-shaped agent loop" lane (that's Browser Use / Stagehand). We are in a lane no one is currently in — **pre-computed, signed, text-shaped maps of dynamic pages**.
- The success metric: not "drove a flow to completion." It is "produced a cartography that contained the answer the agent needed, without the agent ever clicking anything."

What this preserves:
- ADR 0009: `heso.run(url, request)` remains the one public surface; the result is now explicitly a cartography rather than a generic receipt.
- ADR 0010: the 15 terminal primitives are now the *vocabulary the agent uses to read the map*, not the vocabulary a planner emits against a live engine.
- ADR 0014: the QuickJS bet stays — it's how we *discover* state transitions for the cartography. But the success criterion changes from "render the page correctly" to "discover every state transition the page exposes." That's a much narrower (and more achievable) target.

## V0 → V1 → V2 — the cartography evolution

**V0 — static-link cartography** (this session).
No JavaScript engine required. For each `<a href>` in the action graph (same-origin, deduplicated, bounded depth + count), fetch the target and embed its tree + metadata + actions as a sub-cartography. The first installment that **works today** without ADR 0014's months-long DOM build-out. Already useful for docs sites, marketing trees, news indexes, anywhere navigation is hyperlink-driven. Implementation underway in this same session by a delegated agent.

**V1 — JS-driven cartography** (per [ADR 0014](0014-bundled-quickjs-agent-dom.md), multi-week).
For each interactive ref with a JS handler, the QuickJS engine runs the handler in a sandbox snapshot, captures the DOM mutation delta, and materializes a `/{path}/-onclick/` (or `-input/`, `-submit/`) sub-cartography. The state graph for a page becomes complete. Animation transitions disappear into the text — we don't represent the 300ms slide, we represent the post-animation state. This is what makes "a dir for a js animation that leads to information" real.

**V2 — signed, shareable cartography artifacts** ([ADR 0005](0005-ed25519-identity.md) + this ADR).
A cartography is content-addressed (BLAKE3 over canonical JSON) and signed (Ed25519). Two agents asking for "the cartography of stripe.com/pricing" can deduplicate; a CDN can cache them; cross-org audits can compare them by hash. The cartography becomes a *web-native primitive*, not just a heso-internal data structure.

## Why this is defensible

1. **Nobody else builds maps.** Lightpanda gives you a faster browser. Stagehand gives you a smarter wrapper. Firecrawl `/agent` gives you an autonomous loop. Vercel `agent-browser` gives you a typed click-vocabulary on top of Playwright. Every one of them produces a live session as their unit of value. heso produces a **persistent artifact**. That's a different category.

2. **Determinism becomes load-bearing, not decorative.** A live browser doesn't need determinism — every session is bespoke. A *map* must be deterministic by definition; otherwise it's not a map. ADR 0008's deterministic discipline (no clocks, no RNG, sorted maps, document-ordered vectors, recorded network) was always pointing here — we just kept describing it as "replayable receipts."

3. **The hallucination defense.** The apple.com test today produced a hallucinated answer. The defensible story is not "we'll fix the LLM" — it's "**heso returns provenance-grounded refusals — if the map didn't see it, the receipt proves it. Hallucination is a model problem; our cartography is what catches the model lying.**" That story holds only for an artifact-shaped product, not for a session-shaped one.

4. **Scale economics flip.** Live-browser tools charge per-session because every session costs. Cartographies are computed once and consumed many times. The same cartography of *stripe.com/pricing* serves every agent. That's a 100×–10,000× efficiency at scale.

5. **Honest scope for the JS engine.** ADR 0014 is right that we're committing to multi-week DOM work, but the bar is much lower under this framing. We need to discover state transitions, not render pages. That means we can ship a usable cartography engine without ever implementing `getBoundingClientRect`, computed styles, layout, paint, canvas, or workers. We just need handlers to fire and DOM mutations to be observable.

## Alternatives considered

- **Stay with "Chromium for agents."** Rejected: Lightpanda already occupies it, V8 is the right JS engine for that lane (not QuickJS), and the differentiator (size, single binary) is a marginal lane against well-funded incumbents.
- **Pivot to "agent-audit-infrastructure" (signed receipts only).** Rejected as the headline — too narrow, too unsexy. *Inside* the cartography framing it's a load-bearing feature; outside, it's a feature in search of a product.
- **Abandon JS engine ambitions, ship static-link cartography only.** Rejected: V0 alone leaves modern SPAs unmapped and most real-world agent workloads inaccessible. But shipping V0 *first* (this session) buys time to prove the artifact thesis before committing to V1's months.
- **Hire a real browser as the exploration backend** (drive Chromium via CDP for V1 instead of QuickJS). Considered fairly. Pro: works on every site that exists today. Con: re-introduces the system Chrome dep we explicitly rejected in ADR 0012 — and the artifact itself doesn't need to run in the consumer's binary, only the *cartographer* (the build-the-map step) needs the engine. **Open question** for V1: should V1's exploration backend be QuickJS (consistent with ADR 0014) or hosted Chromium (cartographies built on a server, consumed everywhere). Both can produce identical cartography artifacts. Decided in a follow-up ADR after V0 is real and we've learned what handlers actually do on real pages.

## Consequences

**Positive:**
- A defensible position in 2026 — we are the *only* tool producing persistent, signed page state graphs as the unit of value. Everyone else is in the act-observe-decide loop business.
- ADR 0014's months of DOM work has a sharper target — discover state transitions, not render correctly. Much smaller subset of the DOM standard needed.
- The receipts story (ADR 0008) and the determinism story (ADR 0008) become headline features rather than internal hygiene.
- Caching and sharing economics scale — cartographies are computed once, consumed many times.
- The agent loop simplifies — `read → answer` for most queries beats `act → observe → decide → act → observe → decide → answer`.
- Today's hallucination failure mode (apple.com test) has a principled answer: the map's content hash is the evidence; the LLM lying is no longer heso's problem because heso's contract is to produce the map honestly.

**Negative:**
- Re-pitching to existing audiences (anyone who saw "headless browser for agents" or "Chromium for agents") is communication debt. README rewrite required.
- V0 alone (static-link) is a narrower demo than the full vision. Honest framing about which pages it works on (docs, marketing trees, news indexes — yes; SPAs, dashboards, anything JS-mounted — no, wait for V1) is required.
- Cartography for a deeply interactive page is exponential in the worst case. Pruning heuristics (bounded depth, bounded count, skip-list, group-similar-elements, cycle prevention) are essential and inherently lossy. We're approximating the state graph, not enumerating it.
- We're now competing with prior art we may not yet have surveyed adequately — model-based-testing crawlers (Crawljax et al) produced state graphs in the 2010s. Concurrent research (delegated to a parallel agent in this session) is mapping the landscape so we know what to differentiate from.

## What ships first

This ADR commits the direction. The same session ships V0 (static-link cartography). The README is rewritten to lead with the cartography framing. The Flue agent in `heso-test-agent/` keeps using `heso open`, but its skill MD acknowledges link-explored sub-trees if they're present.

V1 (JS-driven cartography) remains scoped per [ADR 0014](0014-bundled-quickjs-agent-dom.md) — QuickJS + agent-shaped DOM — but with the sharper goal of "discover state transitions for cartography," not "be a smaller Chromium."

V2 (signed, shareable cartography artifacts) waits until V1 produces something interesting enough to be worth signing.

## Prior art surveyed (concurrent research in this session)

A delegated researcher with web-search access mapped what already exists. **Nobody has shipped this exact thing as a 2026 product**, but heso is *not* walking into greenfield — there is a 15-year-old academic crater right next to a 2026 land rush in adjacent shapes.

The clearest prior art is **[Crawljax](https://github.com/crawljax/crawljax)** (Mesbah et al., TU Delft, originally 2008, still maintained, cited in research as recent as the *MBTModelGenerator* paper, 2026). It does exactly what cartography proposes — DOM-after-action as a state node, click/fill as a transition edge, Levenshtein-thresholded state equivalence. It was built for QA test generation, not for LLMs to read. Its output is XML/Java that no one would call shareable. **ATUSA** (older invariant-checker on top of Crawljax) and **[WebMate](https://testfabrik.com/en/webmate/platform/overview/)** (Saarland → testfabrik.com; finite-state machines over Web 2.0 GUIs) are the same era. **None of them sign artifacts, none are deterministic by content hash, none are LLM-targeted.** That gap is the moat: take the Crawljax/Mesbah lineage as the credibility anchor, fix what they couldn't (signed, deterministic, agent-readable text), aim it at a 2026 audience nobody else is serving.

The 2025–26 wave isn't doing this. **Stagehand v3** ([browserbase.com](https://www.browserbase.com/blog/stagehand-v3)) added "persistent memory" that caches selectors/actions per workflow — *single-user, not a shareable map.* **[Firecrawl `/agent`](https://www.firecrawl.dev/blog/introducing-agent)** is runtime act-observe-decide, no persistent artifact, charged 100–2500 credits per query. **Browser Use**, **OpenAI Operator**, **ChatGPT Atlas**, **Comet** — all live act-observe-decide loops, stateless per task. The closest academic relative is **[WebATLAS](https://arxiv.org/abs/2510.22732)** (arXiv 2510.22732, Oct 2025): "persistent cognitive map via curiosity-driven exploration" — but it is a **private per-agent memory**, not a signed shareable artifact. That distinction is exactly the wedge: theirs is *what one agent keeps*; ours is *what every agent shares*.

Adjacent: **WebMCP** (Chrome 146, Feb 2026) is site-authored tool manifests. heso is effectively the **unauthorized** version sites get whether they want it or not — interesting framing but not the headline pitch.

Sources: [Crawljax repo](https://github.com/crawljax/crawljax), [Crawling Ajax via DOM state changes (Mesbah et al.)](https://dl.acm.org/doi/10.1145/2109205.2109208), [WebMate platform](https://testfabrik.com/en/webmate/platform/overview/), [WebATLAS](https://arxiv.org/abs/2510.22732), [Stagehand v3 launch](https://www.browserbase.com/blog/stagehand-v3), [Firecrawl /agent](https://www.firecrawl.dev/blog/introducing-agent), [LASER](https://arxiv.org/abs/2309.08172), [WebMCP overview](https://www.datacamp.com/tutorial/webmcp-tutorial), [DOM downsampling D2Snap](https://arxiv.org/html/2508.04412v1).

## Naming

"Cartography" works as the **concept noun**, but the **artifact noun** wants something tighter — short, memorable, unclaimed. The researcher surveyed candidates:

- ❌ **Atlas** — burned (ChatGPT Atlas browser, WebATLAS paper)
- ❌ **Sitemap** — already means a list of URLs
- ❌ **Manifest** — WebMCP-adjacent
- ✅ **Plat** — surveyor's deed-grade map, unclaimed in this space, short. Recommendation from the researcher.
- Honorable mentions: **Schematic**, **Blueprint**, **Charts**.

**Working terminology:** the *concept* is cartography; the *artifact* is a **plat**. *"heso builds plats of web pages."* "Plat of stripe.com/pricing" reads natural after a beat. Subject to revision before public launch, but reserved here so docs, types, and tests can use one word consistently.

## Killer demo (the one-tweet proof)

Per the researcher: a single airline's check-in flow rendered as **~40 nodes the agent reads in ~2 KB of tokens**, versus OpenAI Operator burning 80 screenshots and 20 minutes on the same task. One image, one tweet. This demo is V1-territory (requires JS) and is the headline target for the QuickJS milestone in [ADR 0014](0014-bundled-quickjs-agent-dom.md).

## References

- [Crawljax](https://github.com/crawljax/crawljax) — the canonical model-based DOM-state crawler. Our credibility anchor; the lineage we extend with signed, deterministic, agent-readable artifacts.
- [WebATLAS](https://arxiv.org/abs/2510.22732) — the closest 2025 academic relative; private per-agent memory rather than a shared artifact.
- [Lightpanda](https://lightpanda.io/) — the V8-based "headless browser for AI agents" we are *not* competing with under this framing.
- [Vercel Labs `agent-browser`](https://github.com/vercel-labs/agent-browser) — the typed click-vocabulary product whose `@e1`/`@e2` refs accidentally shadowed our action graph syntax. Not the same product under cartography framing.
- [ADR 0008](0008-deterministic-execution.md) — the determinism discipline that makes the cartography artifact viable.
- [ADR 0009](0009-heso-run-single-tool.md) — the one-tool agent surface; under this ADR, `heso.run(url, request)` returns a plat.
- [ADR 0014](0014-bundled-quickjs-agent-dom.md) — the JS engine commitment, retained but with sharpened success criteria.
