# 0002. Engine trait boundary

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay

## Context

heso starts with Servo as its browser engine (see ADR 0003), but Servo is a research-grade project with rough embedding ergonomics. Over the project's lifetime we may want to:

- Swap engines (Ladybird, custom engine, hybrid).
- Use multiple engines side by side (text-only fast path vs. full-render path).
- Test crates downstream of the engine without spinning up a real engine (a mock impl).

If Servo types leak into our higher-level crates (`heso-extract`, `heso-act`, `heso-cli`, `heso-mcp`), every engine swap becomes a rewrite of the world.

## Decision

Define an **`EngineApi` trait** in a dedicated crate `heso-engine-api`. All other crates depend on the trait, not on any concrete engine. Specific engines live in `heso-engine-<name>` crates that implement the trait.

**Rules:**
- No Servo (or any other engine-vendor) types in `heso-engine-api`'s public surface.
- Trait methods return heso-native types defined in `heso-core` (e.g. `Page`, `Url`, `AccessibilityTree`).
- Concrete engine crates depend on `heso-engine-api` and `heso-core`, never the reverse.
- Downstream crates (`heso-extract`, `heso-act`, etc.) take `impl EngineApi` or `Arc<dyn EngineApi>` as input.
- `heso-cli` and `heso-mcp` choose a concrete engine at compile time (or runtime via feature flags later).

## Alternatives considered

- **No trait — depend on Servo directly everywhere.** Rejected: locks every crate to Servo forever, makes mocking impossible, makes the future engine swap a multi-month rewrite.
- **Inheritance-style multiple traits (`Renderer`, `Navigator`, `Inspector`, ...).** Rejected: premature factoring. Start with one trait, split it later if a real engine can implement only a subset.
- **Engine as a service / IPC boundary instead of a trait.** Rejected: too much overhead for the in-process case. Reserve IPC for the eventual sandboxed-engine architecture (separate ADR if/when we get there).

## Consequences

**Positive:**
- Engine swappability is built into the architecture from day one.
- Downstream crates can be unit-tested against a mock engine.
- The trait acts as a forcing function for clean naming — anything we put in it must make sense across engines.

**Negative:**
- Designing a good trait without knowing the second engine is hard. We accept that v1 of `EngineApi` will need revisions as more engines are integrated.
- Trait dispatch (`dyn EngineApi`) adds a tiny runtime cost. Negligible vs. the cost of rendering a page.
- Engines may have features the trait can't express. We deal with this via optional traits (`EngineApiAdvanced`, etc.) added later, not via Servo-specific escape hatches.

## References

- ADR 0001 (workspace layout).
- ADR 0003 (Servo as first engine).
- Servo crate API: https://docs.rs/servo
