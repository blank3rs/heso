# How to add a crate

We err toward more, smaller crates. Splitting is cheap; merging is hard.

## When to add one

- You're about to write a logically distinct unit of functionality (different responsibility, different reason-to-change).
- An existing crate is growing a second responsibility ("X and Y").
- You want to expose a piece of heso for use by external consumers without dragging the whole stack.

## Steps

1. Pick a name. Kebab-case, `heso-` prefix. Single word if possible (`heso-extract`, not `heso-page-extractor-tools`).
2. Create it: `cargo new --lib crates/heso-NAME` (or `--bin` for a binary).
3. Add to root `Cargo.toml` `workspace.members`.
4. Set the Cargo.toml package fields from workspace:
   ```toml
   [package]
   name = "heso-NAME"
   version.workspace = true
   edition.workspace = true
   rust-version.workspace = true
   license.workspace = true
   repository.workspace = true
   authors.workspace = true
   description = "One-line crate description."
   ```
5. Add `#![forbid(unsafe_code)]` and `#![warn(missing_docs)]` at the top of `src/lib.rs`.
6. Write a module-level doc comment at the top of `src/lib.rs` explaining what this crate is for and what its public surface is.
7. If this crate crosses an architectural boundary (defines a trait others must implement, becomes a new public-facing surface, etc.), **write an ADR**.
8. Run `cargo ck` to confirm the workspace still builds.
9. Update `state.json` if this work is associated with a task.

## Cargo.toml template

```toml
[package]
name = "heso-NAME"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
repository.workspace = true
authors.workspace = true
description = "One-line crate description."

[dependencies]
heso-core = { path = "../heso-core" }
thiserror.workspace = true
tokio = { workspace = true, optional = true }

[features]
default = []
```

## Don't

- Don't add a crate without giving it a single, clear responsibility.
- Don't have a crate that exists only to re-export from other crates ("facade" crates) without a strong reason in an ADR.
- Don't add `anyhow` to a library crate. Use `thiserror` and a per-crate error enum.
