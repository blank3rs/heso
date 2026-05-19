# Playwright sidecar benchmark

A small Node.js script that runs the **same target URLs** as
[`crates/heso-compat-suite`](../../crates/heso-compat-suite) through
Playwright's headless Chromium, and emits a JSON scorecard in a shape
that lines up with heso's report. The point: head-to-head comparison.

> **You do not need Node.js to use heso.** This directory is for
> *benchmarking* heso against Playwright. Skip it unless you're
> producing a comparison table.

---

## Why this exists

heso aims to be the fast, low-overhead way for AI agents to load and
inspect web pages. "Fast and low-overhead" is only meaningful
relative to *something* — and the natural comparison is Playwright,
the canonical headless-Chromium-as-a-library stack.

A reviewer of the main README correctly pointed out that publishing a
compatibility scorecard for heso without anything to compare it
against is half a measurement. This sidecar gives the other half. The
two scripts navigate the same URLs, do the same trivial DOM probe
(`document.title`), and write JSON in the same column layout, so a
downstream merge script can produce a single table with both timings
side by side.

## Install + run

You need Node.js 18+ and a recent npm. From this directory:

```bash
npm install                          # installs Playwright + browser binaries
npm start                            # runs the benchmark, JSON to stdout
```

The first `npm install` will download a headless Chromium build
(~150–200 MB). That's a one-time cost.

To save the JSON to a file:

```bash
npm start -- > playwright.json
```

To filter targets (mirrors `cargo run -p heso-compat-suite -- --filter`):

```bash
npm start -- --filter wikipedia
```

To use a different browser engine (Chromium is the default, since
that's what most agent stacks ship today):

```bash
npm start -- --browser firefox
npm start -- --browser webkit
```

Other flags:

| Flag | Default | Meaning |
|---|---|---|
| `--filter SUBSTR` | none | Run only targets whose name or category contains the substring |
| `--browser NAME` | `chromium` | One of `chromium`, `firefox`, `webkit` |
| `--timeout MS` | `30000` | Per-target navigation timeout |
| `--targets PATH` | `./targets.json` | Use an alternate targets file |
| `--strict` | off | Exit non-zero if any target fails |

Progress lines go to **stderr** (just like the Rust suite). The JSON
report is the only thing written to **stdout**, which makes piping
clean:

```bash
npm start -- 2>/dev/null | jq '.summary'
```

## JSON shape

```jsonc
{
  "tool": "playwright",
  "browser": "chromium",
  "playwright_version": "1.60.0",
  "results": [
    {
      "name": "example.com",
      "category": "smoke",
      "url": "https://example.com",
      "status": "ok",           // one of: ok | fetch_error | js_error
      "ms_total": 412,          // wall-clock for the whole probe
      "ms_fetch": 380,          // page.goto wall-clock
      "ms_eval": 4,             // page.title wall-clock
      "http_status": 200,
      "value": "Example Domain",
      "error": null
    }
  ],
  "summary": { "total": 8, "passed": 8, "failed": 0 }
}
```

This is intentionally close to heso's `Report` shape (see
[`crates/heso-compat-suite/src/main.rs`](../../crates/heso-compat-suite/src/main.rs)).
The matching field names — `name`, `category`, `url`, `status`,
`ms_total`, `ms_fetch`, `ms_eval`, `value` — are what make a join
trivial.

### Joining with heso's scorecard

Given `heso.json` (from `cargo run -p heso-compat-suite > heso.json`)
and `playwright.json` (from this script), a one-liner produces a
side-by-side comparison via `jq`:

```bash
jq -s '
  .[0].results as $heso
  | .[1].results as $pw
  | [
      $heso | to_entries[] | .key as $i |
      {
        name: .value.name,
        heso_status: .value.status,
        heso_ms: .value.ms_total,
        pw_status: $pw[$i].status,
        pw_ms: $pw[$i].ms_total,
        ratio: (
          if $pw[$i].ms_total > 0
          then (.value.ms_total / $pw[$i].ms_total)
          else null end
        )
      }
    ]
' heso.json playwright.json
```

(The targets are ordered identically because both tools read the same
`targets.json` — or, in heso's case, the matching `TARGETS` const.
If you reorder one, reorder the other.)

## Methodology notes

This is a **wall-clock** benchmark, not a microbench. The numbers
include DNS, TLS, TCP handshakes, page download, parse, and
JavaScript evaluation. That's the right comparison for "how long until
an agent has a useful answer back," which is what heso is optimizing
for, but it's the wrong comparison for "how fast does the JS engine
go in isolation."

Run on a quiet network. Average over multiple runs if you care about
real numbers — neither tool is deterministic across network conditions.

The probe is intentionally trivial (`document.title`) because the
goal is to compare the *baseline* page-open cost. Once both tools can
reliably extract a title across the suite, future revisions of this
benchmark can layer on more sophisticated probes (querySelector
counts, framework hydration, form submission) and the joining script
above will still work.

## Source of truth for the target list

[`targets.json`](./targets.json) is the canonical list. Its entries
mirror the `TARGETS` const in
[`crates/heso-compat-suite/src/main.rs`](../../crates/heso-compat-suite/src/main.rs).
**When you add or change a target, update both.** A future change
could have the Rust suite read from `targets.json` directly to avoid
the drift risk — but for now the const + JSON file are intentionally
duplicated, since the Rust suite needs Probe info this file doesn't
carry, and we don't want to invent a richer schema until we need it.

## Why pin Playwright exactly

`package.json` pins Playwright to a single version, not a range.
Benchmark stability depends on the browser version being predictable
across machines and across time — a floating `^1.x.y` would silently
swap Chromium builds out from under us and make month-over-month
comparisons useless.

To upgrade: bump the version in `package.json`, re-run, and note the
new version in the comparison output (the JSON includes
`playwright_version` so this is automatic).

## License

MIT OR Apache-2.0, same as the rest of the heso repo. This sidecar
was written from scratch using the Playwright public docs (resolved
via Context7) — no vendored third-party code.
