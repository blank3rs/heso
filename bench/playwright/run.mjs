// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Playwright sidecar for the heso compatibility benchmark.
//
// Reads `targets.json` (mirrors `crates/heso-compat-suite/src/main.rs`
// TARGETS), navigates each URL through headless Chromium, captures
// `document.title`, and emits a JSON scorecard on stdout in a shape
// that mirrors heso's `Report` so the two can be joined for a
// head-to-head comparison.
//
// This script is intentionally minimal — it is NOT a deep semantic
// equivalence harness. It measures the same "open URL → extract a
// trivial DOM value" cycle that heso's compat suite measures, so the
// two timings can be compared apples-to-apples.
//
// Usage:
//   node run.mjs                        # JSON to stdout
//   node run.mjs --filter wikipedia     # only matching name/category
//   node run.mjs --browser firefox      # chromium (default) | firefox | webkit
//   node run.mjs --timeout 30000        # per-target navigation timeout (ms)
//   node run.mjs --targets ./other.json # alternate targets file
//
// The script never throws on per-target failure — failures are part
// of the report (status: "fetch_error" or "js_error"), matching how
// the Rust suite classifies them.

import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { argv, stdout, stderr, exit } from 'node:process';

const __dirname = dirname(fileURLToPath(import.meta.url));

/**
 * Parse argv into a tiny options object. We hand-roll this rather than
 * pull in a CLI framework — only a handful of flags, and we want to
 * mirror the Rust suite's flag style (`--filter SUBSTR`, `--strict`).
 */
function parseArgs(args) {
  const opts = {
    filter: null,
    browser: 'chromium',
    timeoutMs: 30_000,
    targetsPath: resolve(__dirname, 'targets.json'),
    strict: false,
  };
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    switch (a) {
      case '--filter':
        opts.filter = args[++i] ?? null;
        break;
      case '--browser':
        opts.browser = args[++i] ?? 'chromium';
        break;
      case '--timeout':
        opts.timeoutMs = Number(args[++i] ?? '30000');
        break;
      case '--targets':
        opts.targetsPath = resolve(process.cwd(), args[++i] ?? '');
        break;
      case '--strict':
        opts.strict = true;
        break;
      case '--help':
      case '-h':
        stderr.write(
          'usage: node run.mjs [--filter SUBSTR] [--browser chromium|firefox|webkit] ' +
            '[--timeout MS] [--targets PATH] [--strict]\n',
        );
        exit(0);
        break;
      default:
        stderr.write(`unknown flag: ${a}\n`);
        exit(2);
    }
  }
  if (!['chromium', 'firefox', 'webkit'].includes(opts.browser)) {
    stderr.write(`invalid --browser: ${opts.browser}\n`);
    exit(2);
  }
  if (!Number.isFinite(opts.timeoutMs) || opts.timeoutMs <= 0) {
    stderr.write(`invalid --timeout: ${opts.timeoutMs}\n`);
    exit(2);
  }
  return opts;
}

/**
 * Load and validate the targets file. We deliberately do not crash on
 * an unknown extra field — the file is meant to be a superset of what
 * each consumer needs (heso's Rust suite, this script, future
 * comparators).
 */
async function loadTargets(path) {
  const raw = await readFile(path, 'utf8');
  const parsed = JSON.parse(raw);
  if (!parsed || !Array.isArray(parsed.targets)) {
    throw new Error(`targets file ${path} is missing a top-level "targets" array`);
  }
  for (const t of parsed.targets) {
    if (typeof t.name !== 'string' || typeof t.url !== 'string') {
      throw new Error(`invalid target entry: ${JSON.stringify(t)}`);
    }
  }
  return parsed.targets;
}

/**
 * Run one target through Playwright. Wall-clock timing is measured
 * from "we start setting up the browser context" through "we have a
 * title back" — mirroring how the Rust suite measures
 * fetch + parse + eval.
 *
 * We split the timing into `ms_fetch` (page.goto wall-clock) and
 * `ms_eval` (page.title wall-clock), so the JSON columns line up with
 * heso's `ms_fetch` / `ms_eval`. They aren't a perfect semantic
 * match — Playwright's `goto` includes some rendering — but they're
 * the closest decomposition we can get without instrumenting Chromium
 * itself.
 */
async function runTarget(target, browser, timeoutMs) {
  const t0 = Date.now();
  let context;
  let page;
  let msFetch = 0;
  let msEval = 0;
  try {
    context = await browser.newContext();
    page = await context.newPage();
    page.setDefaultNavigationTimeout(timeoutMs);
    page.setDefaultTimeout(timeoutMs);

    const fetchStart = Date.now();
    const response = await page.goto(target.url, { waitUntil: 'load' });
    msFetch = Date.now() - fetchStart;

    const evalStart = Date.now();
    const title = await page.title();
    msEval = Date.now() - evalStart;

    const msTotal = Date.now() - t0;
    return {
      name: target.name,
      category: target.category ?? '',
      url: target.url,
      status: 'ok',
      ms_total: msTotal,
      ms_fetch: msFetch,
      ms_eval: msEval,
      http_status: response?.status() ?? null,
      value: truncate(title),
      error: null,
    };
  } catch (err) {
    const msTotal = Date.now() - t0;
    const message = err instanceof Error ? err.message : String(err);
    // Classify: Playwright surfaces navigation failures as
    // TimeoutError / net::* errors. Anything else we assume happened
    // during page.title() (very unlikely in practice).
    const isFetchError = /timeout|net::|ERR_|navigation|Navigation/i.test(message);
    return {
      name: target.name,
      category: target.category ?? '',
      url: target.url,
      status: isFetchError ? 'fetch_error' : 'js_error',
      ms_total: msTotal,
      ms_fetch: msFetch,
      ms_eval: msEval,
      http_status: null,
      value: null,
      error: message,
    };
  } finally {
    if (page) {
      try { await page.close(); } catch { /* ignore */ }
    }
    if (context) {
      try { await context.close(); } catch { /* ignore */ }
    }
  }
}

/** Mirror the Rust suite's value truncation. */
function truncate(s) {
  const MAX = 240;
  if (typeof s !== 'string') return s;
  if (s.length <= MAX) return s;
  return s.slice(0, MAX) + '…';
}

async function main() {
  const opts = parseArgs(argv.slice(2));
  const allTargets = await loadTargets(opts.targetsPath);
  const targets = opts.filter
    ? allTargets.filter(
        (t) =>
          (t.name ?? '').includes(opts.filter) ||
          (t.category ?? '').includes(opts.filter),
      )
    : allTargets;

  // Import Playwright lazily so `node --check` and `--help` work even
  // when playwright isn't installed yet.
  const { chromium, firefox, webkit } = await import('playwright');
  const browserType = { chromium, firefox, webkit }[opts.browser];

  const browser = await browserType.launch({ headless: true });
  const results = [];
  try {
    for (const t of targets) {
      const r = await runTarget(t, browser, opts.timeoutMs);
      // Stream progress to stderr — stdout is reserved for the
      // final JSON report, just like the Rust suite.
      stderr.write(
        `${r.status.padEnd(6)} ${String(r.ms_total).padStart(5)}ms  ${r.name}\n`,
      );
      results.push(r);
    }
  } finally {
    await browser.close();
  }

  const passed = results.filter((r) => r.status === 'ok').length;
  const total = results.length;
  const report = {
    tool: 'playwright',
    browser: opts.browser,
    playwright_version: await playwrightVersion(),
    results,
    summary: {
      total,
      passed,
      failed: total - passed,
    },
  };

  stdout.write(JSON.stringify(report, null, 2) + '\n');

  if (opts.strict && report.summary.failed > 0) {
    exit(1);
  }
}

/**
 * Best-effort version lookup. We read `playwright/package.json`
 * directly so we don't have to spawn a subprocess or trust an env var.
 */
async function playwrightVersion() {
  try {
    // `import.meta.resolve` is sync (returns a string URL) on Node 18+
    // — no await needed. We wrap in try/catch in case Playwright isn't
    // installed or doesn't expose package.json.
    const pkgUrl = import.meta.resolve('playwright/package.json');
    const raw = await readFile(fileURLToPath(pkgUrl), 'utf8');
    return JSON.parse(raw).version ?? null;
  } catch {
    return null;
  }
}

main().catch((err) => {
  stderr.write(`fatal: ${err?.stack ?? err}\n`);
  exit(1);
});
