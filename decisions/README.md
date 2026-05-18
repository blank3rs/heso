# Architecture Decision Records

Every significant architectural choice in heso gets a numbered, immutable record. ADRs let future maintainers (human or AI) understand *why* something is the way it is — not just *what* it is.

## Index

| # | Title | Status |
|---|-------|--------|
| 0001 | [Cargo workspace layout](0001-cargo-workspace-layout.md) | Accepted |
| 0002 | [Engine trait boundary](0002-engine-trait-boundary.md) | Accepted |
| 0003 | [Servo as first engine](0003-servo-as-first-engine.md) | Superseded by 0011 |
| 0004 | [MCP as primary API surface](0004-mcp-as-primary-api.md) | Accepted |
| 0005 | [Ed25519 identity + signed audit log](0005-ed25519-identity.md) | Accepted |
| 0006 | [Dual MIT + Apache-2.0 license](0006-dual-mit-apache-license.md) | Accepted |
| 0007 | [The .agent/ directory pattern](0007-agent-meta-directory.md) | Accepted |
| 0008 | [Deterministic execution as a first-class property](0008-deterministic-execution.md) | Accepted |
| 0009 | [`heso.run` — the single agent-facing tool](0009-heso-run-single-tool.md) | Accepted (primitive list superseded by 0010) |
| 0010 | [Primitives as terminal commands](0010-primitives-as-terminal-commands.md) | Accepted |
| 0011 | [Chromium via CDP as first engine](0011-chromium-cdp-first-engine.md) | Superseded by 0012 |
| 0012 | [Fetch-only native engine](0012-fetch-only-native-engine.md) | Accepted |
| 0013 | [Engine as semantic extractor](0013-engine-as-semantic-extractor.md) | Accepted |
| 0014 | [Bundled QuickJS + agent-shaped DOM](0014-bundled-quickjs-agent-dom.md) | Accepted |
| 0015 | [heso as cartographer (not browser)](0015-heso-as-cartographer.md) | Accepted (contextualized by 0016) |
| 0016 | [Positioning — headless browser for the agent-relevant half of the web](0016-positioning-headless-browser-for-agents.md) | Accepted |

## How to add an ADR

See [`.agent/HOWTO/add-an-adr.md`](../.agent/HOWTO/add-an-adr.md).

## Template

Copy [`0000-template.md`](0000-template.md) when starting a new ADR.

## Status values

- **Proposed** — under discussion, not yet effective
- **Accepted** — the current decision
- **Superseded by NNNN** — replaced by a later ADR (keep the old one — history matters)
- **Deprecated** — no longer relevant, not yet replaced
