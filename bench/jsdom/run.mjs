// SPDX-License-Identifier: MIT OR Apache-2.0
//
// jsdom sidecar for the heso benchmark.
//
// Same target list as playwright sidecar. For each URL: fetch the
// body with Node fetch, build a jsdom Document with the HTML +
// runScripts: "outside-only" so we measure parse + DOM construction
// (without executing page scripts — jsdom can do that but it adds
// network latency from external scripts that's already accounted for
// in Playwright's run). Emits the same JSON shape as the heso
// compat-suite for direct comparison.
//
// Usage: node run.mjs > results.json

import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { argv, stdout, stderr, exit } from 'node:process';
import { JSDOM } from 'jsdom';

const __dirname = dirname(fileURLToPath(import.meta.url));

function parseArgs(args) {
  const opts = { filter: null, timeout: 30000, targets: null };
  for (let i = 2; i < args.length; i++) {
    const a = args[i];
    if (a === '--filter') opts.filter = args[++i];
    else if (a === '--timeout') opts.timeout = Number(args[++i]);
    else if (a === '--targets') opts.targets = args[++i];
  }
  return opts;
}

async function loadTargets(opts) {
  const path = opts.targets
    ? resolve(opts.targets)
    : resolve(__dirname, '..', 'playwright', 'targets.json');
  const raw = await readFile(path, 'utf8');
  const parsed = JSON.parse(raw);
  let targets = parsed.targets || [];
  if (opts.filter) {
    const f = opts.filter.toLowerCase();
    targets = targets.filter(
      (t) =>
        (t.name && t.name.toLowerCase().includes(f)) ||
        (t.category && t.category.toLowerCase().includes(f)),
    );
  }
  return targets;
}

async function runOne(target, opts) {
  const start = Date.now();
  const memBefore = process.memoryUsage().rss;
  let html;
  let msFetch = 0;
  try {
    const fetchStart = Date.now();
    const resp = await fetch(target.url, {
      headers: {
        'User-Agent':
          'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36',
        'Accept': 'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8',
      },
      signal: AbortSignal.timeout(opts.timeout),
    });
    html = await resp.text();
    msFetch = Date.now() - fetchStart;
  } catch (e) {
    return {
      name: target.name,
      category: target.category,
      url: target.url,
      status: 'fetch_error',
      ms_total: Date.now() - start,
      ms_fetch: msFetch,
      ms_eval: 0,
      peak_rss_kb: Math.round(process.memoryUsage().rss / 1024),
      value: null,
      error: String(e && e.message ? e.message : e),
    };
  }

  let value = null;
  let evalErr = null;
  const evalStart = Date.now();
  try {
    const dom = new JSDOM(html, {
      url: target.url,
      runScripts: 'outside-only',
      pretendToBeVisual: true,
    });
    const doc = dom.window.document;
    if (target.name === 'news.ycombinator.com (count)') {
      value = String(doc.querySelectorAll('.titleline').length);
    } else {
      value = (doc.title || '').trim();
    }
    dom.window.close();
  } catch (e) {
    evalErr = String(e && e.message ? e.message : e);
  }
  const msEval = Date.now() - evalStart;
  const memAfter = process.memoryUsage().rss;

  return {
    name: target.name,
    category: target.category,
    url: target.url,
    status: evalErr ? 'js_error' : 'ok',
    ms_total: Date.now() - start,
    ms_fetch: msFetch,
    ms_eval: msEval,
    peak_rss_kb: Math.round(Math.max(memBefore, memAfter) / 1024),
    value,
    error: evalErr,
  };
}

async function main() {
  const opts = parseArgs(argv);
  const targets = await loadTargets(opts);
  const results = [];
  for (const t of targets) {
    const r = await runOne(t, opts);
    results.push(r);
    stderr.write(
      `[jsdom] ${r.name.padEnd(40)} ${String(r.status).padEnd(11)} ${String(r.ms_total).padStart(5)}ms\n`,
    );
  }
  stdout.write(JSON.stringify({ results }, null, 2));
  stdout.write('\n');
}

main().catch((e) => {
  stderr.write(`jsdom sidecar fatal: ${e && e.stack ? e.stack : e}\n`);
  exit(1);
});
