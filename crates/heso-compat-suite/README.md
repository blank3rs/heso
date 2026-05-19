# heso-compat-suite

End-to-end compatibility + timing benchmark for `heso`. Runs the engine
against a curated set of real-world site / framework targets, asserts
one narrow probe per target, and emits a JSON report (and optionally a
markdown scorecard).

Not a CI net — this crate makes live network calls. The pinned-URL
regression net is `heso-compat-tests`, which uses recorded `wiremock`
cassettes and does zero network I/O. See the rustdoc at the top of
`src/main.rs` for the longer "why two crates" rationale.

## Usage

```sh
cargo run -p heso-compat-suite                         # JSON to stdout
cargo run -p heso-compat-suite -- --markdown COMPAT.md # markdown scorecard
cargo run -p heso-compat-suite -- --filter esm         # only the ESM subset
cargo run -p heso-compat-suite -- --strict             # exit 1 on any real failure
```

Filters match against `name` OR `category`, substring. `--strict` only
fires on *unexpected* failures (see "Expected-fail targets" below).

## How a target is wired

Every entry in `TARGETS` (in `src/main.rs`) is one row in the
scorecard:

- `name` — human-readable label
- `category` — bucket (`smoke`, `server-rendered`, `spa`,
  `framework-docs`, `feature`, `esm`, …)
- `url` — the live URL to fetch
- `js_fetch` — whether to install the in-JS `fetch()` global and run
  external `<script src=…>` references through it. Default `false`
  (most extraction probes work on static HTML)
- `probe` — one of `Contains { js, needle }`, `NonEmptyString { js }`,
  `NumberAtLeast { js, min }`. The JS is evaluated against the
  post-script-pump DOM; the assertion fires on the returned value
- `expected_fail` — see next section

## Expected-fail targets — `category: "esm"`

Some targets are added *ahead* of a feature slice landing — they're
known-failing today, and they'll start passing once a specific gating
slice ships. We keep them in the run as a **regression lock**: if you
ship the slice and don't notice, the target flips to `ok` and the
unflipped `expected_fail` flag becomes the surprise. If you ship the
slice and a target stays failing, the gating wasn't actually what we
thought.

Failures on `expected_fail` targets are reported with status
`expected_fail` (markdown icon ⏳) instead of `assertion_failed`
(❌). `--strict` ignores them.

### ESM target subset (gated on M-A loader)

These 4 targets exist to verify ES Module loading once the M-A slice
lands. They are expected to fail today (pre-M-A) and pass once M-A is
in place:

| Target | Failure mode today | Gating slice |
|---|---|---|
| `lit.dev/?mods=heso-esm-loaded` | 8 inline `<script type="module">` blocks share one global lexical scope under the classic-script shim, so `let e = …` in script #1 collides with `const e = …` in script #2 at parse time (`redeclaration of 'e'`). Script #2's `body.classList.add(?mods value)` never runs. | M-A (per-module lexical scope) |
| `www.solidjs.com` | Purely client-rendered: static `<head>` has no `<title>`, static `<body>` is just `<div id="app">`. The external Vite bundle uses top-level `export` and dynamic `import()` for routing; classic-mode QuickJS rejects `export` outright (`unsupported keyword`). Solid Router never mounts; `document.title` stays empty. | M-A (real module compile) + M-C (dynamic `import()`) |
| `threejs.org/examples/webgl_animation_keyframes.html` | Uses `<script type="importmap">` to map the bare specifier `"three"` to a bundle path, then an inline `<script type="module">` does `import * as THREE from 'three'`. The importmap is parsed as a data block and skipped; the module is evaluated as classic and throws `Unexpected token '*'`. `#container` stays empty (Stats.js DOM never appended). | M-A + M-B (import maps with bare specifiers) |
| `threejs.org/manual/` | The simplest ESM in the wild: one inline module body of `import * as THREE from '../build/three.module.js'; window.THREE = THREE;`. Today QuickJS rejects the `import * as` token; `window.THREE` stays undefined. No importmap involvement, no external module — purely a "did we treat this script as a module?" probe. | M-A (inline modules with relative-path imports) |

Once M-A lands, run `cargo run -p heso-compat-suite -- --filter esm`
and remove the `expected_fail: true` flag from each target that flips
to `ok`. Keep the flag set on any target that stays failing — that's a
diagnostic. The intent is that the `expected_fail` flag tracks the
*latest* gap, not the historical one.

### Adding a new ESM (or other expected-fail) target

1. Pick a target where the *static HTML is missing the content the
   probe asserts* — pre-rendered pages give false positives.
2. The probe must be observable from a single JS expression on the
   post-script-pump DOM. Console state and engine metadata
   (`scripts.executed_with_error`) are *not* visible to the probe.
3. Document **why each probe fails today** in the target's inline
   comment. The header of `src/main.rs` describes the script pump's
   exact simplifications — the comment should connect that to the
   specific token / API the target trips on.
4. Don't pick a startup landing page that might disappear in six
   months. Documentation sites, official examples, and stable
   open-source-project pages only.

## Adding a normal (not-expected-fail) target

1. Pick a narrow probe — one expression, one assertion.
2. Use a needle that's part of the page's permanent identity (a page
   title, an `id`, a brand name), not something that might change
   daily (a headline, a price).
3. Add it to `TARGETS` in `src/main.rs`; the runner picks it up
   automatically.

See the comment block above `const TARGETS: …` in `src/main.rs` for
the canonical version of these rules.
