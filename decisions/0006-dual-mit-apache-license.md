# 0006. Dual MIT + Apache-2.0 license

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay

## Context

We need to pick a license that:

- Maximizes adoption (we want heso to be used everywhere agents are used).
- Is compatible with the rest of the Rust ecosystem (the de-facto Rust standard is MIT OR Apache-2.0).
- Is compatible with Servo, which is **MPL-2.0** per-file.
- Provides patent protection for contributors.

## Decision

**Dual-license heso under MIT and Apache-2.0**, at the user's option. License files: `LICENSE-MIT` and `LICENSE-APACHE` at the repo root. Per-crate Cargo.toml metadata uses the SPDX expression `"MIT OR Apache-2.0"`.

For any files derived from or substantially adapted from Servo (in `heso-engine-servo` or elsewhere), retain the **MPL-2.0 header on those files individually**. MPL-2.0 is file-level copyleft, not project-level, so this dual-license + MPL-derived-files pattern is legally clean.

## Alternatives considered

- **MIT only.** Rejected: lacks Apache's explicit patent grant. Most modern Rust projects prefer Apache-2.0 for this reason.
- **Apache-2.0 only.** Rejected: incompatible with GPLv2-only projects (Apache adds restrictions GPLv2 doesn't allow). Dual-licensing with MIT keeps GPLv2 compat.
- **MPL-2.0 (matching Servo).** Rejected: file-level copyleft scares some enterprise adopters and makes it slightly harder to embed heso in proprietary products. We'd rather keep our code permissive and accept the MPL-2.0 burden only on files that genuinely need it.
- **AGPL-3.0.** Rejected: strong copyleft forces every hosted-service competitor to open-source their changes. Sounds great for "moat" but is hostile to enterprise adoption and the agentware-as-infrastructure pitch.

## Consequences

**Positive:**
- Matches the Rust ecosystem convention — every Rust library plays well with heso.
- Patent grant via Apache-2.0.
- GPLv2 compatibility via the MIT option.
- Per-file MPL-2.0 for Servo-derived code keeps us on the right side of Servo's license without infecting the whole project.

**Negative:**
- Hosted-service competitors can take heso and not contribute back. Acceptable: distribution and momentum matter more than legal moats at this stage.
- Contributors need to understand the per-file MPL-2.0 rule when touching Servo-derived files. We'll add a brief CONTRIBUTING.md note.

## References

- [Why dual-license MIT + Apache-2.0 in Rust](https://github.com/rust-lang/rust/blob/master/COPYRIGHT)
- [MPL-2.0 FAQ — file-level copyleft](https://www.mozilla.org/en-US/MPL/2.0/FAQ/)
- ADR 0003 (Servo as first engine) — sets up the MPL-2.0 compatibility need.
