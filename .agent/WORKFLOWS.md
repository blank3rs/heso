# Workflows

Common commands. Use the cargo aliases defined in `.cargo/config.toml`.

## Build & check

```sh
cargo ck                  # alias: check --workspace --all-targets
cargo build --workspace   # full build
cargo build --release     # release build
```

## Test

```sh
cargo ct                  # alias: test --workspace --all-targets
cargo test -p heso-core   # single crate
```

## Lint & format

```sh
cargo cf                  # alias: fmt --all (rustfmt)
cargo cl                  # alias: clippy --workspace --all-targets -- -D warnings
```

## Dependency hygiene

```sh
cargo deny check          # licenses, advisories, bans, sources
cargo update              # update Cargo.lock within semver
cargo tree -p <crate>     # see what pulls in what
```

## Adding a crate

See [`HOWTO/add-a-crate.md`](HOWTO/add-a-crate.md).

## Adding an ADR

See [`HOWTO/add-an-adr.md`](HOWTO/add-an-adr.md).

## Updating state.json

See [`HOWTO/update-state.md`](HOWTO/update-state.md). **Update state as you work, not in bulk at the end.**

## Local dev loop (typical)

```sh
cargo ck                  # quick syntax + type check
cargo ct                  # tests
cargo cl                  # lints (zero warnings policy in CI)
cargo cf                  # format
```

If all four are green, you're ready to commit.

## Release (future, M4+)

Not yet defined. Will use `cargo xtask release` once `xtask/` exists.
