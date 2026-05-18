# 0001. Cargo workspace layout

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay

## Context

heso is a multi-year project building a browser engine for AI agents. From day one we need a structure that:

- Supports many small, composable units rather than one monolithic binary.
- Allows the engine layer (Servo today, possibly other engines later) to be swapped without rewriting consumers.
- Lets external users depend on individual capabilities (e.g. `heso-extract`) without pulling in the entire stack.
- Stays organized as the project grows from ~3 crates to ~15+ over time.
- Is familiar to Rust contributors (no novel build system, no exotic monorepo tooling).

## Decision

Use a **single Cargo workspace** with one crate per responsibility, all under `crates/`:

```
heso/
├── Cargo.toml                  # workspace root
├── crates/
│   ├── heso-core/              # shared types, errors, traits
│   ├── heso-engine-api/        # the EngineApi trait — the swappable boundary
│   ├── heso-engine-servo/      # Servo implementation (added in M1)
│   ├── heso-primitives/        # the 15 terminal-shaped primitives (ADR 0010)
│   ├── heso-trace/             # trace AST types + Receipt + Cost + trace_hash
│   ├── heso-trace-exec/        # trace runner (Trace + EngineApi -> Receipt)
│   ├── heso-planner/           # plain-English → trace (M3)
│   ├── heso-identity/          # Ed25519 identity (M4)
│   ├── heso-audit/             # signed append-only log (M4)
│   ├── heso-mcp/               # MCP server exposing `heso.run` (M3)
│   └── heso-cli/               # the `heso` binary
└── xtask/                      # build automation (added later, when needed)
```

> **Note (2026-05-17):** the original list of crates above included
> `heso-fetch`, `heso-session`, `heso-extract`, and `heso-act`. Those crates
> were anticipated for a pre-[ADR 0009](0009-heso-run-single-tool.md) /
> pre-[ADR 0010](0010-primitives-as-terminal-commands.md) design where agents
> called many semantic operations directly. They are no longer planned:
> agents now call only `heso.run`, and the operations they cover are folded
> into `heso-primitives` (one primitive per shell command, with `/env/` paths
> replacing a separate session/cookie/storage crate). The list above reflects
> the current crate set.

Conventions:

- Kebab-case crate names with `heso-` prefix.
- One crate = one responsibility (Unix philosophy).
- Workspace-level versioning at `0.x.x` — all crates bump together until 1.0.
- Shared deps go in `[workspace.dependencies]`; crates reference via `{ workspace = true }`.
- `Cargo.lock` is checked in (workspace contains binary crates).

## Alternatives considered

- **Single monolithic crate.** Rejected: would couple unrelated concerns, hurt compile times, and prevent external consumers from depending on subsets.
- **Multi-repo (one repo per crate).** Rejected: drastically more overhead for solo / small team, breaks atomic refactors across crates, hostile to AI agents trying to understand the whole project.
- **Separate workspace per layer (engine workspace, agent-facing workspace, etc.).** Rejected: premature partitioning; we don't know the layer boundaries yet, and a single workspace lets them emerge.
- **Bazel or other build system.** Rejected: Cargo is the Rust standard, Bazel adds heavy tooling burden for unclear benefit at this scale.

## Consequences

**Positive:**
- Familiar to every Rust developer (and every AI agent trained on Rust code).
- Atomic refactors across crates are trivial.
- Splitting a crate later is `cargo new --lib crates/heso-new` plus moving code; no infra change.
- Each crate is independently publishable to crates.io.

**Negative:**
- Workspace-level Cargo.lock can grow large with many crates.
- Tempting to over-split early. We accept this risk and rely on the `.agent/HOWTO/add-a-crate.md` discipline.
- All crates must share an MSRV. Acceptable for our timeline.

## References

- [Cargo book: Workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html)
- ADR 0002 (engine trait boundary) — explains the `heso-engine-api` split.
- [`.agent/HOWTO/add-a-crate.md`](../.agent/HOWTO/add-a-crate.md)
