# Bug-report synthesis — 5-agent sweep, 2026-05-21

Five agents in parallel, each on a distinct angle. ~1,900 real-site operations + verb chains + receipt round-trips + long-running stress. See per-agent reports `01-` through `05-` for full repros.

## TL;DR

**The engine is structurally solid. The surface leaks at every boundary.**

- Zero panics across 47+ sites, zero memory leaks across 1,825 ops, GC workaround holds. The foundation we built isn't the problem.
- Every layer above the engine has at least one truthfulness or wiring bug: HTTP says "ok" when it isn't, `click` says "done" without navigating, the receipts pitch isn't wired to the CLI at all.
- ~12-15 surface-level bugs together kill the product experience. Not 50 deep bugs — fixable in roughly one focused week.

## P0 — Trust-blockers (fix before any marketing)

| # | Bug | Source | Why it's P0 |
|---|---|---|---|
| 1 | `heso click` on `<a href>` returns `value: true` and does NOT navigate | agent 2 | THE verb pitch. If click doesn't navigate, "verbs beat selectors" is a lie. |
| 2 | 4xx/5xx + Cloudflare interstitials return `ok: true` with empty body, `partial_reason="ok"`, exit 0 | agents 1 + 2 (two independent hits) | An LLM agent thinks it got data when it got blocked. Worst possible failure mode. |
| 3 | Classic `<script>` runs in strict mode by default | agent 3 | Silently breaks Apple, Wikipedia. Per HTML spec classic scripts are sloppy. |
| 4 | Signed receipts library-only — no CLI verb calls `run_signed` | agent 5 | The pitch ("signed audit trails") is vaporware from a user's POV. Users can't produce a receipt. |

## P1 — Capability gaps (block real sites)

Cross-validated by agents 1 + 3 (convergent finding = priority signal):

- `XMLHttpRequest` missing
- `HTML*Element` constructor family missing (`new HTMLDivElement()` etc.)
- `performance.mark` / `performance.measure` missing
- `document.getElementsByClassName` missing
- `data:` URL fetch unsupported
- `element.style = "..."` string-coercion setter missing

Plus from agent 5 (security/correctness of the receipts pitch even after #4 is fixed):
- `receipt-verify` accepts ANY pubkey (no allowlist)
- `receipt-verify` accepts `mode: live` despite ADR forbidding it

## P2 — Polish

- `setTimeout(fn)` rejects 1-arg form [agent 3]
- eval-dom microtask drain skips setTimeout-resolved promises [agent 3]
- Cookies: host-only cookies emit `domain: ""` [agent 4 — `crates/heso-cli/src/main.rs:2447`]
- Cookies: `batch read --include cookies` snapshots non-deterministic across siblings [agent 4]
- `plat_hash` leaks server UUIDs on dynamic pages (github.com etc.) [agent 5]
- ADR 0008 cassette/replay mode designed but unbuilt [agent 5]
- ~20 more low-impact DOM APIs missing (Intl, ReadableStream, XPathEvaluator, HTMLVideoElement, etc.) — defer until a real site forces them

## What's GOOD (don't touch)

- QuickJS host runtime: rock solid. Zero panics, zero hangs across 26+ JS-heavy sites including SPAs.
- Memory: no leaks across 1,825 ops. Throughput improved under sustained load (likely tokio/reqwest pool warming up).
- File descriptors: no leaks.
- The `disable-assertions` workaround holds under 5x serial + 5-way parallel.
- Receipt JSON: byte-identical on static pages (the non-determinism is server-side UUID leakage, not heso's fault).

## The picture

| Layer | State |
|---|---|
| Engine (QuickJS host, GC, memory, fd) | ✅ solid |
| DOM/Web API coverage | ⚠️ ~6 critical gaps, ~20 long-tail gaps |
| HTTP truthfulness | ❌ lies about success |
| Verb semantics (click especially) | ❌ pitch-breaking |
| Receipts (CLI surface) | ❌ unwired |
| Receipts (crypto correctness) | ⚠️ verify accepts any key |
| Cookies | ⚠️ minor schema/race bugs |
| Determinism | ⚠️ leaks on dynamic pages |
| Reliability under load | ✅ solid |

## Proposed first sprint

The fix order is concentrated, not scattered.

**Day 1-2 — Trust pass**
- Fix `ok: true` on 4xx/5xx (single change in fetch error path; should propagate to `open`/`read`/`find`/`batch`)
- Fix `click` on `<a href>` to actually follow the navigation

**Day 3 — Strict-mode default**
- Wrap classic `<script>` content in non-strict prologue per HTML spec

**Day 4-5 — API gap closure** (the convergent P1 list)
- `XMLHttpRequest` polyfill
- `HTMLElement` / `HTMLDivElement` / `HTMLScriptElement` etc. as `#[rquickjs::class]`
- `performance.mark` / `performance.measure`
- `getElementsByClassName`
- `data:` URL fetch
- `element.style = "..."` setter

**Day 6 — Receipts to CLI**
- Pipe `run_signed` into a verb (`heso open --sign` or `heso sign-trace`)
- Pubkey allowlist on `receipt-verify`
- Reject `mode: live` per ADR
- README section explaining the receipts flow

**Day 7 — Polish + demo**
- Cookie bugs
- 5-line homepage example using the now-clean verb chain
- Re-run the 47-site sweep, expect ~35/47 clean

After this sprint heso goes from "10/47 clean, lies about success, click doesn't navigate, audit story is vaporware" to "~35/47 clean, honest about failure, click works, signed receipts producible from one command."

That's the punch list to take "exists" to "good."
