# 0011. Chromium via CDP as first engine (superseded by ADR 0012)

- **Status:** Superseded by [ADR 0012 — Fetch-only native engine](0012-fetch-only-native-engine.md)
- **Date:** 2026-05-17
- **Deciders:** Akshay
- **Supersedes:** [ADR 0003 — Servo as first engine](0003-servo-as-first-engine.md)

> **Supersession note (2026-05-17):** the strategic insight in this ADR (that
> ~80% of heso's differentiation lives above the engine) still holds — it
> motivated 0012. But the conclusion that Chromium-via-CDP made sense was
> wrong on deployability: chromiumoxide requires a system Chrome install, so
> heso couldn't ship as a single deployable binary. ADR 0012 swaps to a
> pure-Rust `reqwest + html5ever` engine (`heso-engine-fetch`); the
> `heso-engine-cdp` crate from this ADR has been deleted. Servo remains a
> possible future addition if JS support becomes critical (M4+).

## Context

ADR 0003 chose Servo as the first engine on the thesis that "novel engine = real moat" — that vertical integration over the engine was what made heso defensible against Browserbase / Stagehand / Browser Use / Hyperbrowser. We accepted a 6-12+ month build slope to get the moat.

Two things changed since 0003:

1. **The primitive layer landed (T-020, T-021) and we now know what the engine actually has to do.** It's 15 well-defined operations (ADR 0010): navigate, list AX nodes, read element text, find by role/name, regex page text, fill, click, submit, screenshot, evaluate JS, wait, diff, fetch bytes, write/read/clear env (cookies + storage). Every one of those has a first-class Chrome DevTools Protocol method. We do **not** need engine ownership for the primitive surface — CDP exposes everything.

2. **A focused research pass on `chromiumoxide` (the Rust CDP client, v0.9.1) confirmed the determinism story is achievable on Chromium:** `Emulation.setVirtualTimePolicy` is the fake clock; `Fetch.enable` + `requestPaused` is record/replay; `Emulation.setDeviceMetricsOverride` pins viewport for byte-identical screenshots; `Accessibility.queryAXTree { role, accessible_name }` is our `find` primitive natively; `Page.addScriptToEvaluateOnNewDocument` patches `Math.random` at page-load time (detectable in principle by hostile JS, but fine for the vast majority of sites and agent workflows).

The strategic question becomes: **where does heso's moat actually live?** Re-examining the design:

- **One-tool agent surface** (`heso.run`) — engine-agnostic.
- **Plain-English requests + planner** — engine-agnostic.
- **Terminal-shell primitive vocabulary** (ADR 0010) — engine-agnostic.
- **Signed receipts + content-addressed pages** (ADRs 0005, 0008) — engine-agnostic.
- **Determinism contract** — 80% engine-agnostic (record/replay, virtual time, viewport pinning are all CDP-exposed); the 20% that needs engine ownership (e.g. true SpiderMonkey-level `Math.random` seeding that's undetectable by hostile JS, deep fingerprint isolation, single-binary distribution without a system Chrome) is real but is *not* on the critical path for M1-M3.

In other words, **~80% of what differentiates heso from "Playwright with an MCP wrapper" lives in layers above the engine.** Owning the engine is a 20% gain at 6× the schedule cost.

Comparable shape elsewhere:

| Project | Engine they own | Where the moat is |
|---|---|---|
| OpenAI | None (Nvidia + Azure) | The model, the API, the product |
| Stripe | None (Visa, Mastercard, bank rails) | The API, the developer UX, the reach |
| Vercel | None (AWS, Cloudflare under the hood) | The DX, the abstractions, the team |
| Cursor | None (forks VS Code) | The agent integration, the UX |

heso's analogue: **use Chromium underneath, build the agent-first layer above.** The moat is "easiest way for an agent to browse the web with a signed audit trail," not "we wrote our own renderer."

## Decision

**Take Path A from ADR 0003 (Chromium via CDP) — but reframe it.** Chromium is the *engine* in `heso-engine-cdp`; everything that makes heso distinct still lives in `heso-primitives`, `heso-trace`, `heso-trace-exec`, `heso-planner`, `heso-mcp`. ADR 0003 rejected Path A as "doesn't differentiate" because it conflated *engine choice* with *product differentiation*. With M2's primitives layer + M3's planner + M4's signed receipts, the differentiation is in our hands regardless of which engine we sit on.

### What we ship

- **`heso-engine-cdp`** (new crate, M1) — implements `EngineApi` via `chromiumoxide 0.9` (Rust CDP client). Uses system Chrome or Chromium on Windows/macOS/Linux. No engine binary bundled in v1.
- **Determinism preconditions** (M2, retasked) — wire `Emulation.setVirtualTimePolicy` (fake clock), `Fetch.requestPaused` (record/replay), `Emulation.setDeviceMetricsOverride` (pinned viewport), `Page.addScriptToEvaluateOnNewDocument` (seeded `Math.random`).
- **Engine trait stays clean** (ADR 0002 unchanged). `heso-engine-cdp` is one implementation. If we later want `heso-engine-servo` we add it alongside without breaking anything else.

### Servo is deferred, not dead

The case for a lower-level engine remains real:
- Undetectable `Math.random` patching (Chromium polyfill is detectable in adversarial settings).
- Engine-level fingerprint isolation (Chromium always looks like Chromium to the server).
- Single-binary distribution without depending on system Chrome.
- Novel agent-first APIs that no browser engine exposes (custom AX tree shapes, headless-only modes that skip layout entirely).

These are **M4+ concerns** if they ever become blockers. Until then, Servo is a `heso-engine-servo` crate we can add alongside `heso-engine-cdp` without architectural change.

## Alternatives considered

- **Stay on Servo (ADR 0003).** Rejected: the 80/20 analysis. The differentiation story is in the layer above the engine, and we'd be paying 6 weeks of SpiderMonkey build pain *before knowing what we actually need from a lower-level engine*. The path was right on the original thesis; the thesis was wrong about where the moat lives.
- **Wrap Playwright directly** (the Node SDK, via a sidecar process). Rejected: extra moving parts (Node runtime alongside Rust), worse error paths, and `chromiumoxide` gives us the same CDP surface in pure Rust.
- **Fork Chromium.** Rejected for the same reasons as in ADR 0003: ~35M LOC, weekly security patches, full-time team needed.
- **Ladybird.** Rejected for v1 (same reasoning as ADR 0003). Re-evaluate in 12+ months alongside Servo.
- **Hybrid (Chromium AND Servo together from M1).** Rejected as premature. Better to ship one solid engine, learn from real planner traces, then add the second when we have data on what's missing.

## Consequences

**Positive:**
- **Days, not weeks, to a working `heso.run` on real sites.** ~2-3 days for `heso-engine-cdp` MVP vs ~3-6 weeks for the Servo embed.
- **Site compatibility is solved.** Every modern site that works in Chrome works in heso (vs Servo's known SPA-compat gaps).
- **Determinism gets ~80% delivered immediately** via CDP knobs we don't have to invent.
- **chromiumoxide is mature** (v0.9.1, well-maintained, tokio-native, async-first — fits our stack).
- **ADR 0002 (engine trait boundary) does its job.** We can add Servo later without churn.
- **The planner gets real data faster** — M3's planner v0 can train on actual page captures, not on Servo's subset.

**Negative:**
- **No engine ownership.** Chromium fingerprint is recognizable. Sites with aggressive anti-bot will block us until we sit behind a residential proxy / Browserbase-style layer.
- **`Math.random` patching via `addScriptToEvaluateOnNewDocument` is detectable** by hostile JS that checks `Math.random.toString()`. Acceptable for the vast majority of agent workflows; not acceptable for adversarial testing.
- **Distribution depends on system Chrome.** Users must have Chrome / Edge / Chromium installed. chromiumoxide auto-detects on Windows via registry; macOS/Linux similar. Bundling Chromium binaries is a possible future enhancement.
- **The "first browser written in Rust" story from ADR 0003 is gone** — we're now "the first browser-grade agent surface, on top of Chromium." Honest reframing.
- **The `EngineApi` trait probably becomes async.** chromiumoxide is async-first; pretending to be sync at the trait layer is a footgun. Trait gets `async fn open` and `async fn text`; `heso-cli` becomes `#[tokio::main]`. Breaking change to current users of the trait, but the only current user is `heso-cli` itself plus the stub `DummyEngine` in tests.
- **Open questions Q-001 (JS engine) and Q-003 (binary distribution) are mooted** at this layer — they re-emerge only if/when we add `heso-engine-servo`.

## References

- ADR 0002 (engine trait boundary) — unchanged; this ADR exercises the trait's intended swap-ability.
- ADR 0003 (Servo as first engine) — superseded by this ADR.
- ADR 0008 (deterministic execution) — unchanged; this ADR shows we can deliver ~80% of it on Chromium today.
- ADR 0009 (one tool — heso.run) — unchanged.
- ADR 0010 (terminal-shaped primitives) — unchanged; the 15 primitives all map to CDP methods.
- [`chromiumoxide` 0.9.1 docs](https://docs.rs/chromiumoxide/0.9.1).
- Chrome DevTools Protocol: `Accessibility`, `Emulation`, `Fetch`, `Network`, `Page`, `Runtime` domains.
- Research note to be added: `research/chromium-cdp/embedding-api.md` (planned).
