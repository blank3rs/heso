# Fix-02: JS engine stress (`bug-reports/03-js-engine-stress.md`)

Fixed three engine-level defects from the JS-engine stress report. All three were single-point fixes in `crates/heso-engine-js/`; no DOM-API surface was touched.

Branch: `worktree-agent-af6ee5bc2bb45a6df`

## Commits

| SHA | Bug | Title |
|---|---|---|
| `56cb753` | A (P0) | `engine-js: classic <script> runs in sloppy mode by default (bug-03 P0)` |
| `6996c03` | B (P2) | `engine-js: setTimeout/setInterval accept 1-arg form (bug-03 P2)` |
| `8227f43` | C (P2) | `engine-js: pump due-now timers in eval drain (bug-03 P2)` |

Diffstat (`git diff main --stat`):

```
 crates/heso-engine-js/src/engine.rs  | 167 +++++++++++++++++++++++++++++++++-
 crates/heso-engine-js/src/scripts.rs | 122 ++++++++++++++++++++++++-
 crates/heso-engine-js/src/timers.rs  | 105 ++++++++++++++++++++--
 3 files changed, 382 insertions(+), 12 deletions(-)
```

## What changed

### Bug A — sloppy-mode classic scripts (commit `56cb753`)

Files touched: `crates/heso-engine-js/src/scripts.rs`, `crates/heso-engine-js/src/engine.rs`.

Per WHATWG HTML §16.1.3, classic `<script>` bodies run in **sloppy mode** unless they begin with `"use strict"`. rquickjs's `Ctx::eval` defaults to strict (`EvalOptions::default()` sets `strict: true`), which turned the canonical browserify/UMD-style shape

```js
require = function () { ... };   // Apple ac-target.js line 1
RLCONF = { x: 1 };               // Wikipedia MediaWiki ResourceLoader
```

into `ReferenceError: <name> is not defined`.

Route classic `<script>` eval through `ctx.eval_with_options(source, EvalOptions { strict: false, .. })`. Three eval call sites were updated:

- `scripts::eval_one` — page `<script>` bodies
- `engine::eval_value_with_promise_await` — `heso eval-js` and the user-eval inside `heso eval-dom`
- `engine::install_document_with_pre_scripts` (inject-scripts) — `--inject-script` payloads

Modules stay strict per ECMA-262 §16.2.2 — `scripts::eval_one_module` is unchanged.

`EvalOptions` is `#[non_exhaustive]` in rquickjs 0.11, so the construction uses `EvalOptions::default()` + field mutation rather than struct literal syntax (forbidden across crate boundaries; I confirmed by reading `~/.cargo/registry/src/.../rquickjs-core-0.11.0/src/context/ctx.rs:28` and the corresponding `Default` impl).

Tests added (in `crates/heso-engine-js/src/scripts.rs` and `engine.rs`):

- `scripts::tests::classic_script_runs_in_sloppy_mode_bare_assign_succeeds`
- `scripts::tests::module_script_stays_strict_bare_assign_errors`
- `engine::tests::eval_value_with_promise_await_is_sloppy_by_default`

### Bug B — `setTimeout(handler)` 1-arg form (commit `6996c03`)

File touched: `crates/heso-engine-js/src/timers.rs`.

The rquickjs 0.11 `FromParam` impls are key here:

| Closure arg type | `ParamRequirement` | JS-side behavior |
|---|---|---|
| `Option<T>` (via `FromJs`) | `single()` — REQUIRED arg | `None` only on a literal `undefined` value |
| `Opt<T>` (in `rquickjs::prelude`) | `optional()` — truly optional | accepts a missing arg |

The old `setTimeout = Func::from(move |cb: Function, delay: Option<f64>|)` therefore rejected the 1-arg JS call shape with `"Error calling function with 1 argument(s) while 2 where expected"` at the binding boundary, before `clamp_delay` could see the missing arg. Switched both `setTimeout` and `setInterval` to `Opt<f64>`; `clamp_delay(None) == 0` already handled the missing-delay case.

Tests added:

- `timers::tests::set_timeout_one_arg_form_defaults_delay_to_zero`
- `timers::tests::set_interval_one_arg_form_does_not_throw`

### Bug C — `setTimeout(0)`-resolved Promises (commit `8227f43`)

File touched: `crates/heso-engine-js/src/engine.rs`.

The original `JsEngine::run_pending_jobs` pumped only the QuickJS microtask queue. A Promise constructed with `new Promise(r => setTimeout(r, 0))` deposits its resolver on the **timer wheel**, NOT the microtask queue — the bare microtask drain never settled it, and the deep-resolve thenable registration found the slot empty after the drain, so `engine.eval` returned `null` instead of the resolved value.

Added `JsEngine::fire_due_timers_and_drain_microtasks` which:
1. Calls `timers::advance_clock(0)` — fires every timer whose `fire_at_ms <= now` without advancing the virtual clock past `now`.
2. Runs one trailing microtask drain so the `.then` chain of the freshly-resolved Promise has a chance to land.

Wired it into both branches of `run_pending_jobs` (with and without fetch state). A `setTimeout(fn, 100)` is *not* triggered — `fire_at_ms = 100 > now = 0` — so the deterministic-virtual-clock contract from ADR 0008 is preserved.

One existing timer unit test (`set_timeout_one_arg_form_defaults_delay_to_zero`, added in Bug B's commit) was rewritten to reflect the new drain semantics: a `setTimeout(0)` scheduled inside `engine.eval` fires before the call returns, just like a real browser drains its microtask queue after the synchronous prefix completes.

Tests added:

- `engine::tests::eval_drain_resolves_promise_via_set_timeout_zero`
- `engine::tests::eval_drain_resolves_nested_promise_via_set_timeout_zero`

## Test additions (summary)

- `scripts::tests::classic_script_runs_in_sloppy_mode_bare_assign_succeeds` — page `<script>RLCONF={x:1}; window.RLCONF=RLCONF</script>` succeeds, no console error, no failure entry.
- `scripts::tests::module_script_stays_strict_bare_assign_errors` — same body inside `<script type="module">` does NOT create the global.
- `engine::tests::eval_value_with_promise_await_is_sloppy_by_default` — `eval-js "MY_BARE_ASSIGN = 7; MY_BARE_ASSIGN"` returns `7`.
- `timers::tests::set_timeout_one_arg_form_defaults_delay_to_zero` — `setTimeout(fn)` with no delay schedules a 0-delay timer that fires inside the eval drain.
- `timers::tests::set_interval_one_arg_form_does_not_throw` — sibling for `setInterval(fn)`.
- `engine::tests::eval_drain_resolves_promise_via_set_timeout_zero` — `new Promise(r => setTimeout(r, 0))` settles to its resolved value.
- `engine::tests::eval_drain_resolves_nested_promise_via_set_timeout_zero` — `Promise.resolve(new Promise(r => setTimeout(r, 0)))` round-trips.

`cargo test --workspace --offline` is clean (no failures, no regressions). The full engine-js lib suite went from 252 → 259 tests, all passing.

## Before / after — direct repros

All four runs below use `./target/release/heso.exe` built from `8227f43`.

### Bug A repro: `eval-js "MY_BARE_ASSIGN = 1; MY_BARE_ASSIGN"`

**Before** (per `bug-reports/03-js-engine-stress.md` line 56):

> `./target/release/heso.exe eval-js "MY_BARE_ASSIGN = 1; MY_BARE_ASSIGN"` → `MY_BARE_ASSIGN is not defined`

**After:**

```json
{
  "console": [],
  "ok": true,
  "value": 1
}
```

### Bug B repro: `eval-js "setTimeout(() => {}); 'ok'"`

**Before** (per `bug-reports/03-js-engine-stress.md` line 57):

> `./target/release/heso.exe eval-js "setTimeout(() => {}); 'ok'"` → throws (`Error calling function with 1 argument(s) while 2 where expected`)

**After:**

```json
{
  "console": [],
  "ok": true,
  "value": "ok"
}
```

### Bug C repro: `eval-dom https://example.com "new Promise(r => setTimeout(() => r('m_ok'), 0))"`

**Before** (per `bug-reports/03-js-engine-stress.md` line 66):

> `./target/release/heso.exe eval-dom https://example.com "new Promise(r => setTimeout(() => r('m_ok'), 0))"` → `null`

**After:**

```json
{
  "console": [],
  "ok": true,
  ...
  "value": "m_ok"
}
```

## Before / after — real-site repros

### Apple (`heso read https://www.apple.com --include console,scripts`)

Before (per the bug-report TLDR + table row line 42): **6 console errors**, including `require is not defined` at `ac-target.js:461` (literal first line of the file is `require=function(){...}` — the strict-mode bare-assign failure).

After:

```
console entries: 3
  1. (warn) "AT: Adobe Target content delivery is disabled..."
  2. (error) head.built.js: cannot read property 'width' of undefined
  3. (error) globalheader.umd.js: Error converting from js 'int' into type 'string'
  4. (error) localeswitcher.built.js: cannot read property 'split' of undefined

scripts: { executed: 11, executed_with_error: 3, external_handled: 12, skipped_non_script_type: 4 }
```

The `ac-target.js` `require is not defined` failure (the P0 strict-mode regression) is **gone**. The remaining three errors are downstream API gaps (missing DOM properties like `width`, type-coercion mismatches, `split` of undefined) which the bug report categorized separately (P1/P2 — not in scope for this fix).

### Wikipedia (`heso read https://en.wikipedia.org --include console,scripts`)

Before (per the bug-report table row line 37): **4 console errors**, all `RLCONF is not defined` / cascade.

After:

```
console entries: 0
scripts: { executed: 4, executed_with_error: 0, external_handled: 1, skipped_non_script_type: 1 }
```

**Zero console errors.** All four bare-assign-related failures (which cascaded into disabling RLQ and the MediaWiki ResourceLoader pipeline) are eliminated. Wikipedia now executes its top-level scripts without any errors at all.

## Out of scope

Per the task brief, no DOM-API surface work (XMLHttpRequest, TextEncoder, HTMLScriptElement, etc.) was done — another agent owns that. None of the existing DOM tests broke; the engine-stress bug report's P1/P2 API-gap findings (cloudflare HTMLVideoElement, figma TextEncoder, htmx XPathEvaluator, shopify ReadableStream, etc.) remain open and are still tracked in the bug report.
