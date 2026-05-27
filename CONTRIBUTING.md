# Contributing to heso

Thanks for taking the time to look at heso. Outside contributions are
welcome; what follows is what we look for in them.

## Where to start

heso is pre-1.0. The agent surface — verb names, JSON field names,
flag spellings — is still moving. Two kinds of contribution are easy
to land right now:

- **Bug reports against the current verb surface.** A failing site, an
  unexpected exit code, a JSON shape that doesn't match the docs. Open
  an issue with the exact command, the heso version, and the observed
  vs expected output.
- **Fixes that don't expand scope.** A clearer error message, a
  determinism bug in the JS engine, a missing field on a verb's output,
  a flaky test. Open a PR; we'll review.

For anything larger — a new verb, a new plat field, a new engine
behavior, a new dependency — open an issue first so we can agree on
the shape before you write the code. We'd rather discuss the design
than review a PR that fights the existing architecture.

## Building

heso is Rust. Install the toolchain from <https://rustup.rs>; the
[`rust-toolchain.toml`](rust-toolchain.toml) pin governs the version.

```sh
git clone https://github.com/blank3rs/heso
cd heso
cargo build --release -p heso-cli
./target/release/heso open https://example.com
```

The release binary is the only shipped artifact. The npm meta package
(`@ixla/heso`), the per-platform npm packages
(`@ixla/heso-<platform>-<arch>`), and the PyPI package (`heso`) all
spawn it as a subprocess and parse the JSON it returns. The library
wrappers add no native code of their own.

## Testing

```sh
cargo test --release -p heso-engine-js --lib
cargo test --release -p heso-engine-fetch --lib
```

Both crates carry the load-bearing invariants — JS-engine determinism,
plat canonicalization, cassette replay. CI runs the full workspace; a
PR that breaks either of these crates won't land.

Compatibility tests live in
[`crates/heso-compat-tests`](crates/heso-compat-tests). They pin
behavior against recorded fixtures (`crates/heso-compat-tests/cassettes/`)
so the test suite runs offline. If you touch the engine, run these too:

```sh
cargo test --release -p heso-compat-tests
```

## Layout

- [`crates/heso-cli`](crates/heso-cli) — the `heso` binary; verb
  dispatch lives in `src/main.rs`.
- [`crates/heso-engine-fetch`](crates/heso-engine-fetch) — the static
  engine (HTTP, parsing, action graph, plat construction).
- [`crates/heso-engine-js`](crates/heso-engine-js) — QuickJS-backed JS
  execution, virtual clock, seeded PRNG, DOM shims.
- [`crates/heso-compat-suite`](crates/heso-compat-suite) — the
  external-site compatibility runner.
- [`crates/heso-compat-tests`](crates/heso-compat-tests) — the offline
  conformance corpus.
- [`spec/HESO-1.0.md`](spec/HESO-1.0.md) — a pointer; the canonical
  spec lives at <https://heso.ca/spec>.

## Style

- Match the existing Rust style; `cargo fmt` and `cargo clippy
  --workspace --all-targets -- -D warnings` are CI gates.
- Comments describe what the code does and why, in the present tense,
  as if the current shape was always intended. Don't apologize for
  past behavior in comments — they should read clean to someone who
  arrives without history.
- Verb names, plat field names, error variants, exit codes — these
  are load-bearing. Don't rename one without an issue discussion first.

## Opening a pull request

- Title: one short line. Body: what changed and why, in plain prose. A
  link to an issue if there is one.
- Keep the diff focused. If you notice unrelated drift, open a separate
  issue or PR rather than bundling it in.
- Run the tests above locally before pushing. CI will catch the rest.

## Reporting a security issue

See [`SECURITY.md`](SECURITY.md).

## License

By contributing, you agree that your contribution is dual-licensed
under MIT and Apache-2.0, matching [`LICENSE-MIT`](LICENSE-MIT) and
[`LICENSE-APACHE`](LICENSE-APACHE).
