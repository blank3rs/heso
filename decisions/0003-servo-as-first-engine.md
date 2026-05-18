# 0003. Servo as first engine

- **Status:** Superseded by [ADR 0011](0011-chromium-cdp-first-engine.md)
- **Date:** 2026-05-17
- **Deciders:** Akshay

> **Supersession note (2026-05-17):** the strategic re-think behind ADR 0011 —
> ~80% of heso's differentiation lives in layers above the engine
> (one-tool surface, terminal primitives, signed receipts, planner) — meant
> paying 6+ weeks of SpiderMonkey build pain for Servo was the wrong trade.
> We now ship `heso-engine-cdp` (Chromium via the `chromiumoxide` Rust CDP
> client) as the M1 engine. Servo is deferred to M4+ as a second engine
> (`heso-engine-servo`) if and only if we hit a wall Chromium can't close
> (true `Math.random` patching, engine-level fingerprint isolation, single
> binary distribution). ADR 0002 (engine trait boundary) makes the swap free.

## Context

heso needs a browser engine. We considered three categories of approach (documented during planning):

- **Path A:** Wrap Chromium via Playwright / CDP. Ships in ~4 weeks. Lowest differentiation — we are a thin wrapper that copycats can replicate.
- **Path B:** Fork Chromium and strip the human-facing surface. Ships in ~3 months. Real binary differentiation but Chromium is ~35M LOC and fork maintenance is significant ongoing work.
- **Path C:** Build on Servo (Rust browser engine). Ships in 6–12+ months. Genuinely novel — Servo is a clean Rust codebase, recently embeddable via the `servo` crate v0.1.0 (April 2026).

Earlier in May 2026, Servo released the `servo` v0.1.0 crate with a clean embedding API (`ServoBuilder`, `WebView`, pixel readback). This is the first time embedding Servo has been a `cargo add` story instead of a patch-and-build story.

## Decision

Take **Path C: build on Servo**. Use the `servo` crate v0.1.0+ as the first engine, behind the `EngineApi` trait (ADR 0002). Implementation lives in `heso-engine-servo`.

## Alternatives considered

- **Path A (Chromium wrapper).** Rejected: doesn't differentiate. Browserbase, Stagehand, Browser Use, Hyperbrowser already occupy this space. We would compete on price and polish against well-funded incumbents.
- **Path B (Chromium fork).** Rejected: viable, but maintaining a Chromium fork at ~35M LOC with weekly security patches is a full-time job for a team, not a side project. Better as a v2 if Path C hits a dead end.
- **Ladybird.** Rejected for v1: less mature than Servo for embedding, C++ rather than Rust (less aligned with our stack), smaller community. Worth re-evaluating in 12+ months.
- **Custom engine from scratch.** Rejected: insane. Servo took a decade to reach embedding maturity.

## Consequences

**Positive:**
- Rust all the way down — one language, one toolchain.
- Servo's codebase is much cleaner than Chromium's; modifications and forks are tractable.
- We can contribute upstream to Servo and benefit from the ecosystem.
- Real story: "the first browser built in Rust, for agents." Distinct and defensible.

**Negative:**
- Servo is research-grade. Many sites don't render correctly. We accept this and plan to file upstream bugs as we hit them.
- Servo uses **SpiderMonkey** for JavaScript — a C++ engine that is heavy to build and adds significant build-tooling requirements on each platform (Q-001 in state.json). We may revisit this for a pure-Rust alternative (Boa) later.
- Servo's embedding API is brand new (April 2026). Expect breaking changes between minor versions for the next 12+ months.
- Multi-platform binary distribution is harder than for a pure-Rust app. See research/servo-internals/ for ongoing notes.
- Time to first usable heso is measured in months, not weeks.

## References

- [Servo crate on crates.io](https://crates.io/crates/servo)
- [Simon Willison: Exploring the new `servo` crate (April 2026)](https://simonwillison.net/2026/Apr/13/servo-crate-exploration/)
- [`paulrouget/servo-embedding-example`](https://github.com/paulrouget/servo-embedding-example) — kept in sync with Servo releases.
- [Verso](https://github.com/versotile-org/verso) — another browser embedding Servo; prior art for the binary-distribution problem.
- ADR 0002 (engine trait boundary).
- `state.json` Q-001 (JS engine question), Q-003 (binary distribution question).
