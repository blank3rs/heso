## 0016. Positioning — headless browser for the agent-relevant half of the web

- **Status:** Accepted
- **Date:** 2026-05-18
- **Deciders:** Akshay
- **Relates to:** [ADR 0009 — `heso.run` single agent tool](0009-heso-run-single-tool.md), [ADR 0012 — fetch-only native engine](0012-fetch-only-native-engine.md), [ADR 0014 — bundled QuickJS + agent-shaped DOM](0014-bundled-quickjs-agent-dom.md), [ADR 0015 — heso as cartographer](0015-heso-as-cartographer.md)

## Context

heso's public framing has been "the first browser built for agents instead of humans." It is technically true (we are headless-only and the user is software) but it is the wrong sentence to put on the front of the repo:

- "First browser built for agents" sounds like vaporware. It is a category claim with nothing concrete underneath it.
- It does not name the thing that makes heso different from Browser Use, Stagehand, Skyvern, Operator, ChatGPT Atlas, or Lightpanda.
- It hides the actual technical bet (drop the rendering pipeline, keep everything else, ship as a single Rust binary) under a slogan.

What heso actually is: the headless browser for the **agent-relevant half of the web**. The frame is the agent-shaped equivalent of `chromium --headless` + Playwright. Browser Use, Stagehand, Skyvern, and Operator are all wrappers around Playwright/Chromium — and Chromium is bloat for the agent use case. The rendering pipeline (Skia, Blink layout, compositor, GPU, video, WebGL, canvas) is roughly 70% of why headless Chromium ships at ~180–240 MB. An agent reading a docs site, filling a login form, clicking through a checkout flow does not need any of that. heso is what you get when you keep the boring half (fetch, parse, JS, DOM, cookies, forms, clicks, sessions) and drop the rendering half.

The technical artefacts that justify the claim already exist:

- 8.1 MB stripped release binary today, post-QuickJS-bundling (was 5.2 MB pre-Phase-1A). Chromium headless: ~240 MB.
- Single Rust binary. No Chromium dep, no Node dep, no Python dep, no `npm install playwright`.
- 175 workspace lib tests green.
- `heso-engine-fetch` (reqwest + scraper) carries the static path. `heso-engine-js` (rquickjs) carries the language for the dynamic path. Both ship in the same binary.

Cold start and idle RAM are not benchmarked yet — TODO. Target is sub-100ms cold start, <20 MB RAM at idle. Honest framing in the README until measured.

## Decision

The canonical one-line pitch is:

> **Headless browser for the agent-relevant half of the web. 30MB single binary, sub-100ms cold start, no Chromium. Handles fetch, parse, JS hydration, forms, clicks, sessions. Returns structured agent-shaped JSON with content-hashed signed receipts.**

(Today's measured size is 8.1 MB; the "30MB" figure is the budget ceiling for the full Phase 1C+ feature surface, not a current measurement. The README cites the real 8.1 MB number; the one-line public pitch cites the budget ceiling because it is the number that lands.)

The in/out capability table is canonical:

**What's in (the agent-relevant half):**

| Capability | Status |
|---|---|
| HTTP/HTTPS, redirects, cookies | done (`reqwest`) |
| HTML parse | done (`scraper`) |
| JS execution | Phase 1A landed (QuickJS via `rquickjs`) |
| Form fill + submit | days of work — action graph has the refs |
| Click links / buttons | weeks — follow href + POST |
| Wait for content | needs DOM wiring |
| LocalStorage / sessionStorage | days |
| Fetch API in JS | 1–2 weeks (proxy `reqwest` into QuickJS) |
| Multi-page sessions | designed in (`page_id` in `heso serve`) |
| File downloads / uploads | trivial / days |
| Headers, auth | trivial |
| IntersectionObserver, ResizeObserver | stub-able (fire-once) |

**What's out (and that's the bet):**

- Canvas pixels, WebGL, Three.js demos, Figma. Agents don't need this.
- Video / audio playback.
- WebRTC.
- Service Workers (most agent sites don't depend on SW).
- Real CSS layout, animations, transitions.

Anyone who says "but my site uses canvas/WebGL/video for data" is not in heso's audience for v1. Use a real browser. That's fine.

## Precedent

The minimal-DOM-plus-JS-engine pattern is established. [jsdom](https://github.com/jsdom/jsdom) is ~50k LOC of JavaScript implementing the slice of the DOM that real pages call. [happy-dom](https://github.com/capricorn86/happy-dom) is ~30k LOC of the same idea. Both prove a minimal DOM + JS environment handles the agent half of the web. Both are slow because they are JS running JS, used mostly for testing, never shipped as a product. Doing it in Rust against QuickJS is the obvious next move and nobody has shipped it as a product aimed at agents. There is a real gap on the shelf.

The existing 2025–26 wave (Browser Use, Stagehand, Skyvern, Operator, ChatGPT Atlas, Lightpanda, Firecrawl `/agent`) is competing around Chromium — wrappers, smart loops, persistent memory, faster Playwright. None of them attacks the Chromium-dep problem directly. heso does.

## Alternatives considered

- **Keep the cartography-only framing.** Rejected: ADR 0015 is correct that a plat is heso's output artefact, but "we build maps of web pages" describes what one `heso run` *produces*, not what heso *is* relative to existing browser engines. A reader landing on the repo asks "what category is this in?" before "what does it output?" ADR 0015 stays — the plat is the deliverable of a single run — but the headline noun for the *product* is "headless browser," not "cartographer." Both can be true at different levels: heso is a headless browser whose output is a plat.

- **Pivot to scraper-only positioning** ("a faster Firecrawl"). Rejected: the market is smaller, the moat is thinner (everyone with `reqwest` + an LLM thinks they have a scraper), and it forecloses the JS hydration / forms / clicks work that ADR 0014 commits to. The agent use case requires more than fetch+parse.

- **Stay with "first browser built for agents instead of humans."** Rejected: the current framing. Sounds like a slogan, doesn't name the technical bet, gives the reader nothing to verify.

- **Position as "Chromium for agents."** Rejected by ADR 0015 already (Lightpanda owns that lane, V8 wins on JS-engine completeness). Recorded here for completeness.

## Consequences

**Positive:**

- The category claim is concrete and verifiable. Binary size, cold start, RAM at idle are numbers a reader can check. "First browser built for agents" cannot be verified.
- The competitive frame names real products (Browser Use, Stagehand, Skyvern, Operator, Lightpanda) and explains in one sentence what's different (no Chromium dep).
- The in/out table makes the scope honest. Anyone whose site depends on canvas/WebGL/video knows in 10 seconds heso is not for them — no time wasted in either direction.
- The 8.1 MB number on a single binary against Chromium's 240 MB is a marketing fact that survives being checked. The "30MB ceiling" is a budget commitment, not a measurement claim.
- jsdom + happy-dom as precedent give the bet credibility. The minimal-DOM lane has been proven viable in JavaScript; doing it in Rust is the obvious next step.

**Negative:**

- The agent-shaped DOM month (Phase 1B of ADR 0014) is the project's longest unbroken stretch of work between user-visible wins. Realistic risk: it feels terrible and stalls solo work. The README's roadmap calls it out as the load-bearing month.
- "Headless browser" frames against an audience that has expectations from Chromium/Playwright. The first time someone tries to render a Figma page and fails, they will say "this is not a browser." That is the correct response and the answer is "yes, that's the bet." Communicating it without sounding defensive is a writing problem.
- ADR 0015 (cartographer) and this ADR (headless browser) need to coexist cleanly. The hierarchy is: heso is a headless browser; one run produces a plat (cartography artefact). The README leads with the headless-browser framing; the plat is what `heso open` returns. Anyone reading both ADRs should land on the same mental model.

## References

- [ADR 0009](0009-heso-run-single-tool.md) — `heso.run` remains the one public surface; the result is a plat.
- [ADR 0012](0012-fetch-only-native-engine.md) — the static engine path.
- [ADR 0014](0014-bundled-quickjs-agent-dom.md) — the JS engine commitment.
- [ADR 0015](0015-heso-as-cartographer.md) — the plat is the output of one run; this ADR contextualizes 0015 by saying *what heso is* alongside *what it produces*.
- [jsdom](https://github.com/jsdom/jsdom) — minimal DOM + JS in ~50k LOC of JavaScript. Precedent that the agent-relevant half is implementable.
- [happy-dom](https://github.com/capricorn86/happy-dom) — same precedent, ~30k LOC.
- [Lightpanda](https://lightpanda.io/) — the "smaller Chromium" lane we're explicitly not competing in.
- [Browser Use](https://github.com/browser-use/browser-use), [Stagehand](https://www.browserbase.com/blog/stagehand-v3), [Skyvern](https://github.com/Skyvern-AI/skyvern), [OpenAI Operator](https://openai.com/index/introducing-operator/) — the Playwright/Chromium wrappers heso is the alternative to.
