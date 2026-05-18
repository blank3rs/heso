# Conventions

Coding and contribution conventions for this repo. Bias toward strict, explicit, and boring.

## Language and edition

- **Rust 2021 edition**, MSRV pinned in `rust-toolchain.toml` (currently 1.90).
- Bump MSRV deliberately, in its own commit, with an ADR if it forces dropping a platform.

## Crate naming

- All workspace crates use the `heso-` prefix.
- Kebab-case for crate names (`heso-engine-api`), snake_case for modules and Rust identifiers.
- One crate = one well-defined responsibility (Unix philosophy). If a crate's docstring needs the word "and", consider splitting.

## Error handling

- **Libraries**: define a per-crate error enum with `thiserror`. Public functions return `Result<T, Error>` where `Error` is the crate's enum.
- **Binaries** (`heso-cli`, `xtask`): `anyhow` is fine for top-level orchestration.
- **Never** swallow errors. If something is recoverable, return it. If it's a logic bug, panic.
- Every error variant carries enough context for an agent to recover without re-running.

## Async

- **`tokio`** is the runtime. No `async-std`, no `smol` (revisit only via ADR).
- Library crates take a runtime handle when needed; don't call `tokio::main` outside binaries.
- Use `tokio::spawn` for fire-and-forget; use `JoinSet` for structured concurrency.

## Logging / tracing

- **`tracing`** crate. Spans for high-level operations, events for noteworthy points.
- Never `println!` in library code. The CLI may use `println!` for user-facing output.

## Documentation

- Every public item documented. `#![warn(missing_docs)]` on every library crate.
- Doctests for non-trivial public APIs. Examples should compile.
- Module-level docs explain the *why* and the *invariants*, not the *what*.

## Comments inside functions

- Default to none. Code names should carry the meaning.
- Comment only when the *why* is non-obvious: a hidden invariant, a workaround, a subtle perf trick.
- Never comment what the code is for (`// called by X`) — that rots.

## Dependencies

- Add via `cargo add`, not by hand-editing `Cargo.toml`.
- Pin minor versions in `workspace.dependencies`; crates reference via `{ workspace = true }`.
- Run `cargo deny check` before adding. License must be in `deny.toml` allow-list.
- Justify any dep with > 10 transitive deps. Justify any dep that brings in `cc-rs` or large native libs.

## Unsafe

- `#![forbid(unsafe_code)]` in every crate by default.
- Exception: engine-integration crates (e.g. `heso-engine-servo`) may need unsafe for FFI. Localize and audit it heavily. Add `# Safety` comments to every unsafe block.

## Commits

- Imperative mood: "add engine trait" not "added" or "adding".
- Reference task IDs from `state.json`: `add engine trait (T-013)`.
- Co-author tag on AI-assisted commits per global instructions.
- Separate commits for: code, ADR, research note, state update. Easier to review and revert.

## When unsure

Read the nearest ADR in `decisions/`. If none applies, ask in the PR or write a new ADR.
