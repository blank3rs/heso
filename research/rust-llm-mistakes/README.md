# Rust: common LLM mistakes

This document is a field guide for AI coding agents working in the **heso** codebase (a browser engine for AI agents, written in Rust). It catalogs the specific, recurring mistakes that LLM-based coding assistants make when generating Rust code, why they make them, and what to do instead. Read it before writing or modifying Rust here. The patterns below are drawn from clippy lint documentation, the Rust API Guidelines, the Rustonomicon, async working-group writeups, and post-mortems from teams running AI agents against Rust codebases. When in doubt, prefer the idiomatic pattern shown here over what feels "natural" from analogy to other languages — Rust's idioms are load-bearing for memory safety and performance, not stylistic preference.

## 1. Ownership, borrowing, and lifetimes

### The mistake
The "reflex `.clone()`": when the borrow checker rejects code, LLMs add `.clone()` (or worse, `Arc<Mutex<T>>` / `Rc<RefCell<T>>`) until it compiles, instead of restructuring the borrow. Closely related are hallucinated lifetime annotations (`<'a, 'b: 'a>`) added "just in case" and reflexive `'static` bounds on generics that don't actually need them. The `Rc<RefCell<T>>` reach is especially common because it visually resembles patterns from Java/C# (`Shared<Mutable<T>>`) and silences the compiler — at the cost of moving aliasing errors from compile time to runtime panics.

### Why LLMs make it
Training data contains many beginner Rust threads where someone *did* fix borrow-checker errors with `.clone()`, so the pattern is overrepresented. LLMs also reason by analogy from garbage-collected languages where "just hold a reference" is free. The borrow checker's error messages suggest `.clone()` in their help text, which LLMs read as endorsement. Finally, `'static` "feels safer" the way `final` does in Java — but in Rust it imposes a real constraint (the value owns nothing borrowed) that propagates virally through call sites.

### What to do instead
First, try restructuring: split a function so the borrow ends before the next use; introduce an intermediate `let` binding to extend a temporary's lifetime; pull a `&T` out of a larger struct (split borrows). Use `.clone()` only when the value is truly cheap (a `u64`, a small `String`) or the alternative would obscure intent. Reach for `Rc<RefCell<T>>` only when you genuinely need shared mutable ownership in a single thread — a graph or tree with back-references. Use `Arc<Mutex<T>>` only across threads. Never add explicit lifetimes unless the compiler asks for them; `'static` belongs on `&'static str` constants and trait-object bounds, not as a default. The corrode blog estimates 95%+ of Rust code needs zero explicit lifetimes ([corrode.dev](https://corrode.dev/blog/lifetimes/), [pretzelhammer's lifetime misconceptions](https://github.com/pretzelhammer/rust-blog/blob/master/posts/common-rust-lifetime-misconceptions.md)).

### Example

```rust
// WRONG — reflex clone, defeats the borrow checker
fn process(items: Vec<Item>) -> Vec<Item> {
    let names: Vec<String> = items.iter().map(|i| i.name.clone()).collect();
    log_names(names.clone());        // unnecessary clone
    items.into_iter().filter(|i| names.contains(&i.name.clone())).collect()
}

// RIGHT — borrow, don't clone; use &str where possible
fn process(items: Vec<Item>) -> Vec<Item> {
    let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
    log_names(&names);
    items.into_iter().filter(|i| names.contains(&i.name.as_str())).collect()
}
```

### Detection
`cargo clippy -- -W clippy::redundant_clone -W clippy::clone_on_copy -W clippy::unnecessary_to_owned -W clippy::needless_lifetimes`. The `redundant_clone` lint catches the most common cases. Grep for `Rc<RefCell` and `Arc<Mutex` and review each one — if it's not crossing a thread or representing a graph, it's probably wrong.

## 2. Async / await

### The mistake
Five recurring failures: (1) holding a `std::sync::MutexGuard` across an `.await` point, producing either a confusing `Send` error or a deadlock; (2) calling `tokio::runtime::Runtime::block_on` from inside an existing async context (immediate panic on a Tokio runtime); (3) reaching for `#[async_trait]` reflexively when native `async fn` in traits has worked since Rust 1.75; (4) forgetting `.await` so the returned `Future` is silently dropped and nothing runs; (5) mixing runtimes — sprinkling `smol::block_on` or `async_std::task::spawn` into a Tokio-based codebase, which appears to work in tests then deadlocks under load.

### Why LLMs make it
Async Rust changed substantially over five years; training data conflates pre-1.75 patterns (`#[async_trait]` everywhere, `Pin<Box<dyn Future>>` returns) with current ones. The `std::sync::Mutex` vs `tokio::sync::Mutex` distinction is subtle and rarely flagged in tutorials. Forgetting `.await` is plausible-looking because in Python/JS, async functions often auto-schedule. Runtime mixing happens because LLMs treat all async crates as interchangeable.

### What to do instead
Use `std::sync::Mutex` for short critical sections that never cross `.await` (it's faster). Use `tokio::sync::Mutex` only when you must hold a guard across `.await`. Drop guards explicitly with a scoped block before any await. For traits with async methods: prefer native `async fn` in traits (stable since 1.75) and add `#[trait_variant::make(Send)]` if callers need `Send` futures ([Rust Blog announcement](https://blog.rust-lang.org/2023/12/21/async-fn-rpit-in-traits/)). Keep `#[async_trait]` only when you need `dyn Trait` dispatch. Never call `block_on` inside `async fn` — use `.await` or `tokio::task::spawn_blocking` for CPU work. Pick one runtime per binary and stick to it.

### Example

```rust
// WRONG — std MutexGuard held across .await: Send error or deadlock
async fn handle(state: Arc<std::sync::Mutex<State>>) {
    let mut guard = state.lock().unwrap();
    guard.requests += 1;
    fetch_remote().await;            // guard alive across await
    guard.requests -= 1;
}

// RIGHT — scope the lock so the guard drops before .await
async fn handle(state: Arc<std::sync::Mutex<State>>) {
    {
        let mut guard = state.lock().unwrap();
        guard.requests += 1;
    } // <- guard dropped here
    fetch_remote().await;
    state.lock().unwrap().requests -= 1;
}
```

### Detection
`cargo clippy -- -W clippy::await_holding_lock` catches the `MutexGuard` case directly. `unused_must_use` (warn-by-default in newer rustc) catches forgotten `.await` when the future has `#[must_use]`. The `tokio::runtime::Runtime::block_on` panic is loud at runtime. See [Qovery's "Common Mistakes with Rust Async"](https://www.qovery.com/blog/common-mistakes-with-rust-async) and the [Tokio shared-state tutorial](https://tokio.rs/tokio/tutorial/shared-state).

## 3. Error handling

### The mistake
Six bad habits: (1) `anyhow::Error` in a public library API instead of a `thiserror` enum, denying callers the ability to match on failure modes; (2) `.unwrap()` and `.expect()` scattered through production code paths; (3) `fn main() -> ()` with `?` somewhere inside, forcing nested `match` or panics; (4) `.ok()` to silently drop errors when the intent was actually to log-and-continue; (5) panicking with `panic!()` for recoverable conditions that callers might reasonably handle; (6) hand-written `impl From<X> for MyError` with brittle `match` arms instead of `#[from]`.

### Why LLMs make it
`anyhow` is faster to write and shows up in tutorials and small examples, so LLMs default to it everywhere. `.unwrap()` is endemic in `main()`-only example code and bleeds into library code via copy-paste. The `fn main() -> Result<(), Box<dyn Error>>` pattern is well-documented but takes a few extra characters, so LLMs skip it. `.ok()` looks idiomatic but silently throws away the error context that future debuggers will need.

### What to do instead
Library code: define a `thiserror` enum with one variant per distinct failure mode the caller might want to handle differently. Use `#[from]` for boilerplate-free conversions and `#[source]` to preserve the chain. Application code (binaries, main): use `anyhow::Result` with `.context("doing X")` to add operator-visible context as errors bubble up ([Luca Palmieri's deep dive](https://www.lpalmieri.com/posts/error-handling-rust/), [dtolnay/anyhow](https://github.com/dtolnay/anyhow)). Make `main` return `anyhow::Result<()>`. Reserve `.unwrap()` for cases that are *provably* infallible — and even then prefer `.expect("invariant: …")` so the panic message documents the invariant. Never use `.ok()` to silence an error unless you genuinely don't care; otherwise `if let Err(e) = ... { tracing::warn!(?e, "..."); }`.

### Example

```rust
// WRONG — anyhow in a library, no structured error info for callers
pub fn parse_config(path: &Path) -> anyhow::Result<Config> {
    let s = std::fs::read_to_string(path)?;
    Ok(toml::from_str(&s)?)
}

// RIGHT — thiserror enum, callers can match on variants
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}")]
    Io { path: PathBuf, #[source] source: std::io::Error },
    #[error("malformed config: {0}")]
    Parse(#[from] toml::de::Error),
}

pub fn parse_config(path: &Path) -> Result<Config, ConfigError> {
    let s = std::fs::read_to_string(path)
        .map_err(|source| ConfigError::Io { path: path.into(), source })?;
    Ok(toml::from_str(&s)?)
}
```

### Detection
`cargo clippy -- -W clippy::unwrap_used -W clippy::expect_used -W clippy::panic` (these are restriction-level — enable per-crate). For libraries, grep `pub fn.*anyhow::` — any hit is suspect. CI rule: deny `unwrap_used` in `src/` but allow in `tests/` and `examples/`.

## 4. Iterators, slices, and strings

### The mistake
Six string/iterator anti-patterns: (1) C-style index loops `for i in 0..v.len() { v[i] }` instead of `for x in &v`; (2) `.collect::<Vec<_>>()` followed immediately by another `.iter()` (materializing for no reason); (3) `.clone()` calls inside `.map()` closures when `.copied()` or borrowing would work; (4) `String` parameters when `&str` would suffice (`fn f(s: String)` blocks callers from passing `&"literal"`); (5) confusing `to_string()` / `to_owned()` / `String::from()` / `.into()` — these all do the same thing for `&str→String` but the semantics differ; (6) treating `char` as a "user character" when it's really a Unicode scalar — emoji, ZWJ sequences, and combining accents break this.

### Why LLMs make it
Index loops are the universal first-language idiom. `.collect()` "looks done" so LLMs add it eagerly. Clone-in-closure is the path of least resistance when a borrow check fails inside `.map()`. `String` arguments look more "real" than `&str` (analog to `std::string` vs `const char*`). The `char` / grapheme confusion is universal across languages — Python's `len("👨‍👩‍👧")` returns 5, JavaScript returns 8, Rust's `.chars().count()` returns 5; only grapheme libraries give the user-expected `1`.

### What to do instead
Iterate with `for x in &v`, `for x in v.iter()`, or `for (i, x) in v.iter().enumerate()`. Skip `.collect()` unless you need to store, return, or iterate multiple times. Prefer `.copied()` over `.cloned()` for `Copy` types; for non-`Copy` types, ask whether you need to own the result at all. Accept `&str`, `&[T]`, `&Path`, `&impl AsRef<Path>` in function arguments — not `String`, `Vec<T>`, `PathBuf`. For `&str → String` conversion, prefer `.to_owned()` semantically (you're taking ownership, not "stringifying"); `String::from(s)` is equally idiomatic. For Unicode-aware text segmentation, use the `unicode-segmentation` crate's `.graphemes(true)` — never assume `char` means "what a user sees" ([Rust Book ch 8.2](https://doc.rust-lang.org/book/ch08-02-strings.html)).

### Example

```rust
// WRONG — index loop, premature collect, clone in closure
fn shouts(words: Vec<String>) -> Vec<String> {
    let upper: Vec<String> = words.iter().map(|w| w.clone().to_uppercase()).collect();
    let mut out = Vec::new();
    for i in 0..upper.len() {
        out.push(format!("{}!", upper[i]));
    }
    out
}

// RIGHT — borrow input, single iterator chain, no needless clones
fn shouts(words: &[String]) -> Vec<String> {
    words.iter().map(|w| format!("{}!", w.to_uppercase())).collect()
}
```

### Detection
`cargo clippy -- -W clippy::needless_range_loop -W clippy::needless_collect -W clippy::ptr_arg -W clippy::cloned_instead_of_copied -W clippy::unnecessary_to_owned`. The `ptr_arg` lint specifically flags `&String`, `&Vec<T>`, `&PathBuf` parameters that should be `&str`, `&[T]`, `&Path`.

## 5. Traits, generics, and dyn dispatch

### The mistake
Five trait pitfalls: (1) `Box<dyn Trait>` reached for reflexively when a generic `<T: Trait>` would be both faster and more flexible — or vice versa, monomorphizing generics into bloated binaries when one `dyn Trait` would suffice; (2) `Box<dyn Trait>` without `+ Send + Sync` bounds, which then fails to spawn on Tokio; (3) attempting to `impl SomeTrait for Vec<T>` from a downstream crate (orphan rule violation); (4) `impl Trait` in argument position assumed equivalent to a named generic — it's not, the caller can't `turbofish` it; (5) calling generic methods on a `dyn Trait` (object-safety violation) and being confused by the cryptic error.

### Why LLMs make it
Many "interface" examples in tutorials use `Box<dyn Trait>` for pedagogical simplicity, so it's overrepresented. The `Send + Sync` requirement for trait objects in async contexts is poorly documented and only surfaces as a compile error when you try to `spawn`. Orphan rule errors look mysterious if you don't know coherence rules. The `impl Trait` argument-vs-return distinction is subtle: in argument position it desugars to a generic, in return position it's an opaque existential — these have different semantics ([quinedot's dyn Trait overview](https://quinedot.github.io/rust-learning/dyn-trait-overview.html)).

### What to do instead
Default to generics with trait bounds — monomorphization gives static dispatch and inlining. Use `Box<dyn Trait>` (or `Arc<dyn Trait>`) only when you need a heterogeneous collection or need to break monomorphization bloat. Always add `+ Send + Sync + 'static` to trait objects stored in async tasks (or `+ Send` minimum). For traits used as objects, keep them object-safe: no generic methods, no `Self` by value, no `where Self: Sized`. For extension traits on foreign types, define a wrapper newtype (`pub struct MyVec<T>(Vec<T>)`) rather than fighting the orphan rule. Prefer `where` clauses over inline bounds for readability when you have more than one or two bounds.

### Example

```rust
// WRONG — Box<dyn Trait> with no Send/Sync, won't work with tokio::spawn
struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

// Also wrong — using Box<dyn> when a generic gives you free monomorphization
fn run(stage: Box<dyn Stage>) { stage.execute(); }

// RIGHT — generic for hot/single-impl call sites, dyn + Send for collections
fn run<S: Stage>(stage: S) { stage.execute(); }

struct Pipeline {
    stages: Vec<Box<dyn Stage + Send + Sync>>,
}
```

### Detection
For missing `Send`/`Sync`: the compiler errors with "future cannot be sent between threads" the first time you call `tokio::spawn`. `cargo clippy -- -W clippy::missing_trait_methods` and `-W clippy::borrowed_box`. Grep `Box<dyn ` and audit each for whether it needs to be heterogeneous and whether it has appropriate auto-trait bounds.

## 6. Unsafe

### The mistake
Five unsafe sins: (1) reaching for `unsafe` to "fix" a borrow checker error (almost always wrong — the borrow checker is right); (2) `unsafe fn` or `unsafe { ... }` blocks without a `// SAFETY:` comment explaining what invariant the caller / writer must uphold; (3) `mem::transmute` between types whose layouts aren't `repr(C)` or `repr(transparent)`, producing silent UB; (4) using `std::mem::uninitialized()` (deprecated since 1.38 — use `MaybeUninit`); (5) raw pointer arithmetic that's offset-by-one or wraps past allocation boundaries, producing UB even if the value is never read.

### Why LLMs make it
The training data contains pre-1.38 code using `mem::uninitialized` because it was idiomatic for years. `unsafe` is treated as an escape hatch in casual writing ("just use unsafe if you need to"), masking how strict the invariants actually are. `transmute` looks like a C-style cast and LLMs reason about it that way, missing that Rust optimizers exploit type invariants aggressively. The `// SAFETY:` comment convention is enforced socially, not by the compiler, so it's often skipped.

### What to do instead
First, never use `unsafe` to silence the borrow checker — restructure or use `Cell`/`RefCell`/`Mutex` instead. When `unsafe` is genuinely needed (FFI, low-level allocator, lock-free data structure), every `unsafe` block gets a `// SAFETY:` comment that states the invariant that makes the operation sound; this is enforced by `clippy::undocumented_unsafe_blocks`. Replace `mem::uninitialized` with `MaybeUninit<T>::uninit()` ([Rustonomicon: Uninitialized Memory](https://doc.rust-lang.org/nomicon/uninitialized.html)). For byte reinterpretation, prefer the `bytemuck` crate (checked) over `transmute` (unchecked). For raw pointer work, use `NonNull<T>` instead of `*mut T` where possible, and check `wrapping_offset` semantics carefully — see [Rustonomicon: Transmutes](https://doc.rust-lang.org/nomicon/transmutes.html). Transmuting `&T → &mut T` is **always** UB, no exceptions.

### Example

```rust
// WRONG — undocumented unsafe, deprecated API, UB-prone transmute
fn read_header(buf: &[u8]) -> Header {
    unsafe {
        let mut h: Header = std::mem::uninitialized();        // deprecated
        std::ptr::copy_nonoverlapping(buf.as_ptr(), &mut h as *mut _ as *mut u8, 16);
        h
    }
}

// RIGHT — MaybeUninit, SAFETY comment, or better: bytemuck::from_bytes
fn read_header(buf: &[u8]) -> Header {
    use std::mem::MaybeUninit;
    assert!(buf.len() >= std::mem::size_of::<Header>());
    let mut h = MaybeUninit::<Header>::uninit();
    // SAFETY: buf has at least size_of::<Header>() bytes (asserted above);
    // Header is plain-old-data with no padding (verified by test/repr(C)).
    unsafe {
        std::ptr::copy_nonoverlapping(
            buf.as_ptr(), h.as_mut_ptr() as *mut u8, std::mem::size_of::<Header>(),
        );
        h.assume_init()
    }
}
```

### Detection
`cargo clippy -- -W clippy::undocumented_unsafe_blocks -W clippy::multiple_unsafe_ops_per_block -W clippy::transmute_ptr_to_ref`. Run [Miri](https://github.com/rust-lang/miri) on test suites to catch UB at runtime. `#![deny(unsafe_code)]` at the crate root for crates that shouldn't have any.

## 7. Cargo, features, and dependencies

### The mistake
Five Cargo failures: (1) missing feature flags — using `serde::Deserialize` derive without `serde = { version = "1", features = ["derive"] }`, or `tokio::main` without `tokio = { version = "1", features = ["macros", "rt-multi-thread"] }`; (2) version mismatch hallucinations — LLMs name APIs that exist in reqwest 0.11 when the project is on 0.12 (or vice versa); (3) `default-features = false` set without re-enabling the features the code actually uses; (4) workspace dependency inheritance ignored, so a sub-crate pins `tokio = "1.30"` while the workspace uses `"1.40"` — Cargo dedupes them as long as semver allows, but config differences cause confusion; (5) accidentally enabling `tokio = { version = "1", features = ["full"] }` everywhere, ballooning compile times.

### Why LLMs make it
Crate APIs evolve across versions and training data is undated — LLMs blend `reqwest::blocking` (still in 0.12) with `reqwest::Client` (the async path), or confuse `chrono` and `time` crate APIs. Feature flags are silent: the compiler error is "no method named X" without explaining "this method exists behind feature Y". Workspace inheritance (`{ workspace = true }`) is newer (2022) and underrepresented in training data.

### What to do instead
When you add or change a `use` for a crate, immediately verify the feature flag in `Cargo.toml` — does the method/macro need a feature? Common cases: `serde = { features = ["derive"] }`, `tokio = { features = ["macros", "rt-multi-thread"] }` (or `"rt"` for current-thread), `chrono = { features = ["serde"] }`, `reqwest = { features = ["json"] }`, `uuid = { features = ["v4", "serde"] }`. Pin versions in the *workspace* `[workspace.dependencies]` and inherit with `{ workspace = true }` in sub-crates ([Cargo Book: Features](https://doc.rust-lang.org/cargo/reference/features.html)). When porting code from a tutorial, check the crate's CHANGELOG.md for breaking changes between the tutorial's version and yours. Avoid `features = ["full"]` — list only what you use.

### Example

```toml
# WRONG — vague, missing features, sub-crate ignores workspace
# crates/handler/Cargo.toml
[dependencies]
tokio = "1"
serde = "1"
reqwest = "0.12"

# RIGHT — explicit features, workspace inheritance
# Cargo.toml (workspace)
[workspace.dependencies]
tokio = { version = "1.40", features = ["macros", "rt-multi-thread", "sync"] }
serde = { version = "1", features = ["derive"] }
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }

# crates/handler/Cargo.toml
[dependencies]
tokio = { workspace = true }
serde = { workspace = true }
reqwest = { workspace = true }
```

### Detection
`cargo tree -e features` shows the resolved feature graph. `cargo machete` (or `cargo udeps`) finds unused dependencies. `cargo +nightly check -Z unstable-options --message-format=json` will produce structured errors that often pinpoint missing features. Compile errors of the form "cannot find macro `X` in this scope" with an `unresolved import` almost always mean a missing feature flag.

## 8. API hallucination patterns

### The mistake
Six hallucination patterns: (1) invented function names that "should exist" — `Vec::sort_descending()`, `String::reverse()`, `HashMap::sorted()`, none of which exist; (2) confusing methods between similar crates — calling `reqwest`'s `.text().await` on a `hyper::Response`, or `chrono::Utc::now()` syntax on `time::OffsetDateTime`; (3) signatures from the wrong version — `tokio::spawn(async move { ... })` works, but `tokio::task::spawn_local` requires a `LocalSet`; (4) hallucinated trait methods — assuming any iterator has `.sum_by()` or any error has `.with_context()` without importing `anyhow::Context`; (5) missing imports — using `Path::new(...)` without `use std::path::Path`; (6) hallucinated derive macros — `#[derive(Builder)]` without depending on `derive_builder`.

### Why LLMs make it
This is fundamentally a training-data problem: the model has seen many years of Rust code with overlapping APIs and conflates them. Confidence is uncalibrated — the LLM doesn't know it doesn't know. Similar-named methods across crates (`json()`, `text()`, `bytes()`) are especially error-prone because the *names* are right but the *receiver type* is wrong. Trait methods that require `use` imports look like inherent methods in finished code, so LLMs reproduce the call without the import.

### What to do instead
**Always run `mcp__context7__resolve-library-id` and `mcp__context7__get-library-docs` before writing code that calls an external crate.** Context7 returns current docs straight from the source. After writing code, run `cargo check` immediately — don't write twenty lines before checking. If the compiler says "no method named X for Y", don't guess at corrections; consult `cargo doc --open` (or [docs.rs](https://docs.rs)) for the exact crate version in `Cargo.lock`. Prefer fully-qualified paths in unfamiliar code (`std::collections::HashMap::new()`) so missing imports surface immediately. When using `anyhow`, always `use anyhow::Context` to bring `.context()` and `.with_context()` into scope. When a method "should exist" — slow down and search docs.rs instead.

### Example

```rust
// WRONG — hallucinated APIs, missing imports
let mut v = vec![3, 1, 2];
v.sort_descending();                         // doesn't exist
let now = chrono::Utc::now().to_rfc3339();   // ok if chrono in Cargo.toml
let resp = client.get(url).send().await?.json().await?;  // missing trait
let s: String = path.to_string();            // PathBuf has display(), not to_string

// RIGHT — real APIs, explicit imports
use std::path::Path;
let mut v = vec![3, 1, 2];
v.sort_by(|a, b| b.cmp(a));                  // or sort_unstable_by
let now = chrono::Utc::now().to_rfc3339();
let resp: MyType = client.get(url).send().await?.json::<MyType>().await?;
let s: String = path.display().to_string();
```

### Detection
`cargo check` is the primary defense — run it after every significant edit. `cargo doc --no-deps --open` to verify method signatures. The rust-analyzer LSP (already installed at user scope) flags unresolved methods immediately. When in doubt, write a one-line test that exercises the suspect API before building anything on top of it.

## 9. Performance traps

### The mistake
Six allocator-thrashers: (1) `push_str(&format!("..."))` in a loop — allocates a temporary `String` each iteration; (2) `format!("{}{}", a, b)` for simple concatenation when `[a, b].concat()` or `a.to_owned() + b` would do; (3) `Vec::new()` followed by many `.push()` calls without `Vec::with_capacity(n)`; (4) `Box<dyn Trait>` dispatch in tight inner loops (vtable indirection defeats inlining); (5) `.to_string()` / `.clone()` / `.to_vec()` calls in hot iterator chains that materialize whole copies; (6) missing `#[inline]` on tiny generic functions used across crate boundaries (cross-crate inlining is opt-in via inline annotation).

### Why LLMs make it
`format!` is the universal "build a string" tool LLMs reach for; nothing about it warns "this allocates." Pre-sizing `Vec` requires the LLM to predict capacity, which it can't always do, so it skips. Inlining heuristics are invisible. Hot-loop performance is hard to reason about statically; LLMs optimize for readability.

### What to do instead
Use `write!(&mut s, "...")?` (from `std::fmt::Write`) instead of `s.push_str(&format!(...))` — the [`clippy::format_push_string`](https://rust-lang.github.io/rust-clippy/master/index.html#format_push_string) lint catches this. Use `String::with_capacity(n)` / `Vec::with_capacity(n)` when you know or can estimate `n` ([Rust Performance Book: Heap Allocations](https://nnethercote.github.io/perf-book/heap-allocations.html)). In hot loops, prefer generics over `dyn` so the compiler can inline. Reuse buffers across iterations with `.clear()` instead of allocating new ones. For tiny generic helpers (a few lines, called everywhere), add `#[inline]` — see clippy's `missing_inline_in_public_items` if you want it enforced. Profile before optimizing: `cargo flamegraph` or `samply` will identify the actual hot spot.

### Example

```rust
// WRONG — quadratic-ish: format! allocates a String each iteration
fn render(rows: &[Row]) -> String {
    let mut s = String::new();
    for r in rows {
        s.push_str(&format!("{}: {}\n", r.name, r.value));
    }
    s
}

// RIGHT — pre-sized buffer, write! reuses the existing String
fn render(rows: &[Row]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(rows.len() * 32);
    for r in rows {
        let _ = writeln!(&mut s, "{}: {}", r.name, r.value);
    }
    s
}
```

### Detection
`cargo clippy -- -W clippy::format_push_string -W clippy::format_in_format_args -W clippy::useless_format`. For allocation profiling, use [DHAT](https://valgrind.org/docs/manual/dh-manual.html) or [dhat-rs](https://crates.io/crates/dhat). For CPU profiling, `cargo flamegraph` or [`samply`](https://github.com/mstange/samply).

## 10. Testing and tooling

### The mistake
Six testing missteps: (1) test helpers (`mod fixtures`, helper structs) defined outside `#[cfg(test)]` so they ship in release binaries; (2) integration tests put in `src/tests.rs` instead of `tests/foo.rs`, never exercising the public API; (3) `#[should_panic]` everywhere instead of asserting on `Result::Err` (a `should_panic` test passes for *any* panic, even an unrelated one); (4) `assert!(a == b)` instead of `assert_eq!(a, b)` — the latter prints both values on failure; (5) deeply-nested builder fixtures that grow harder to maintain than the code under test; (6) snapshot tests (`insta`) committed without reviewing the snapshot, locking in bugs as "expected" output.

### Why LLMs make it
`#[should_panic]` is featured prominently in The Rust Book chapter on testing, so LLMs over-apply it. The `tests/` vs `src/` directory distinction is mechanical and unintuitive. `assert!` works syntactically the same as in C/Python, so the macro-aware `assert_eq!` feels like extra ceremony. Snapshot test workflow (`cargo insta review`) requires human judgment LLMs skip.

### What to do instead
Put unit tests in a `#[cfg(test)] mod tests { ... }` block at the bottom of each source file; they can access private items but won't be compiled into release. Put integration tests in `tests/*.rs` files at the crate root — they only see the public API ([Rust Book ch 11.3](https://doc.rust-lang.org/book/ch11-03-test-organization.html)). For failing-expectations: prefer `assert!(matches!(result, Err(MyError::X { .. })))` over `#[should_panic]`; use `should_panic` only for actual panic conditions (overflow, bounds) and always include `#[should_panic(expected = "specific message")]`. Use `assert_eq!`, `assert_ne!`, `assert_matches!` (1.82+) for better failure messages. Prefer builders for test setup over deeply nested struct literals. Always `cargo insta review` snapshot changes before committing.

### Example

```rust
// WRONG — helper not gated, #[should_panic] loses specificity, assert! hides values
pub fn make_test_user() -> User { User { id: 0, name: "test".into() } }  // ships in release!

#[test]
#[should_panic]                                       // passes on any panic
fn rejects_empty_name() {
    let u = User::new("");
    assert!(u.is_err() == true);                      // useless failure message
}

// RIGHT — gated helper, matches! on specific error, assert_eq!
#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_user() -> User { User { id: 0, name: "test".into() } }

    #[test]
    fn rejects_empty_name() {
        let result = User::new("");
        assert!(matches!(result, Err(UserError::EmptyName)));
    }

    #[test]
    fn fields_match() {
        let u = make_test_user();
        assert_eq!(u.name, "test");
    }
}
```

### Detection
`cargo clippy -- -W clippy::tests_outside_test_module -W clippy::missing_assert_message -W clippy::should_panic_without_expect`. Grep `#[should_panic]` (without `expected = `) and review each.

## Quick reference: top 20 anti-patterns

1. Reflex `.clone()` to silence the borrow checker — restructure or borrow first.
2. `Rc<RefCell<T>>` / `Arc<Mutex<T>>` instead of fixing the actual borrow problem.
3. Explicit lifetime annotations the compiler didn't ask for.
4. `'static` bounds on generics that don't need them.
5. Holding `std::sync::MutexGuard` across `.await` (use `tokio::sync::Mutex` or scope the lock).
6. `block_on(...)` inside an async context (instant panic).
7. `#[async_trait]` reflexively when native `async fn` in traits works (Rust 1.75+).
8. Forgetting `.await` so the `Future` is silently dropped.
9. `anyhow::Error` in a library's public API — use `thiserror` enums.
10. `.unwrap()` / `.expect()` in production paths instead of `?` propagation.
11. `.ok()` to silently discard errors that should be logged.
12. `String` / `Vec<T>` / `PathBuf` in function arguments instead of `&str` / `&[T]` / `&Path`.
13. `for i in 0..v.len()` index loops instead of `for x in &v`.
14. Premature `.collect::<Vec<_>>()` followed by another `.iter()`.
15. `Box<dyn Trait>` in hot loops; missing `+ Send + Sync` on async trait objects.
16. `mem::transmute` between non-`repr(C)` types; deprecated `mem::uninitialized` (use `MaybeUninit`).
17. `unsafe` blocks without a `// SAFETY:` comment explaining the invariant.
18. Hallucinated APIs (`Vec::sort_descending`, `String::reverse`) — run `cargo check` and Context7 lookups early and often.
19. Missing feature flags (`serde "derive"`, `tokio "macros"`, `reqwest "json"`).
20. `push_str(&format!(...))` in loops, `Vec::new()` then many `.push()` without `with_capacity`.

## Sources

- [Rust API Guidelines — Checklist](https://rust-lang.github.io/api-guidelines/checklist.html)
- [Clippy Lint Documentation](https://rust-lang.github.io/rust-clippy/master/index.html)
- [The Rustonomicon — Transmutes](https://doc.rust-lang.org/nomicon/transmutes.html), [Uninitialized Memory](https://doc.rust-lang.org/nomicon/uninitialized.html)
- [The Rust Performance Book — Heap Allocations](https://nnethercote.github.io/perf-book/heap-allocations.html)
- [Qovery — Common Mistakes with Rust Async](https://www.qovery.com/blog/common-mistakes-with-rust-async)
- [Tokio — Shared State Tutorial](https://tokio.rs/tokio/tutorial/shared-state)
- [Luca Palmieri — Error Handling in Rust: A Deep Dive](https://www.lpalmieri.com/posts/error-handling-rust/)
- [dtolnay/anyhow](https://github.com/dtolnay/anyhow)
- [pretzelhammer — Common Rust Lifetime Misconceptions](https://github.com/pretzelhammer/rust-blog/blob/master/posts/common-rust-lifetime-misconceptions.md)
- [corrode — Don't Worry About Lifetimes](https://corrode.dev/blog/lifetimes/)
- [quinedot — dyn Trait Overview](https://quinedot.github.io/rust-learning/dyn-trait-overview.html)
- [Rust Blog — async fn and RPITIT in traits (Rust 1.75)](https://blog.rust-lang.org/2023/12/21/async-fn-rpit-in-traits/)
- [Cargo Book — Features](https://doc.rust-lang.org/cargo/reference/features.html)
- [The Rust Book — Test Organization (ch 11.3)](https://doc.rust-lang.org/book/ch11-03-test-organization.html)
- [The Rust Book — Storing UTF-8 Encoded Text (ch 8.2)](https://doc.rust-lang.org/book/ch08-02-strings.html)
