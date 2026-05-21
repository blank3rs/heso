# fix-01-trust-pass — P0 bugs A + B (click navigation + HTTP truthfulness)

Date: 2026-05-21
Branch: `worktree-agent-ace50e2e5292086bd`
Commits (bottom to top):

- `72153a3` — `fix: HTTP 4xx/5xx and Cloudflare interstitials no longer silent (bug 05-B)`
- `5c085c8` — `fix: heso click on <a href> follows the link (bug 05-A)`

## What changed

### Bug B — HTTP / bot-challenge truthfulness (commit 72153a3)

| File | Diff | What |
|------|----:|------|
| `crates/heso-engine-fetch/src/lib.rs` | +234 / -2 | Capture `response.status().as_u16()` on `FetchPage::http_status`. New helpers `is_bot_challenge` (scans `__cf_chl_opt`, `cf_chl_jschl_tk__`, `<title>Just a moment...`) and `partial_reason_for_status` (maps `(status, body)` → `Option<partial_reason>`). Inline unit tests for both helpers + their interaction. |
| `crates/heso-cli/src/main.rs` | +80 / -8 | `cmd_fetch` / `cmd_open` / `cmd_read` (incl. `--best-effort` short-circuit) surface `http_status` on the body and override `partial`/`partial_reason` from the engine's single source of truth. HTTP truthfulness wins over downstream `script_crash`. |
| `crates/heso-cli/src/serve.rs` | +50 / -10 | Mirror the CLI surface for parity (JSON-RPC clients see the same signal). |
| `crates/heso-cli/src/batch.rs` | +28 / -2 | `run_open_for_url` / `run_read_for_url` reject 4xx/5xx and bot-challenge bodies so the per-row `ok` flips to `false`. Existing exit-code semantics preserved. |
| `crates/heso-cli/tests/bug_fixes_http_truthfulness.rs` | +220 (new) | 6 integration tests against wiremock fixtures (403 / 500 / CF interstitial / clean 200 / batch). |

Test count: 6 integration + 7 unit (engine-fetch) = 13 new for Bug B.

### Bug A — click on `<a href>` follows the link (commit 5c085c8)

| File | Diff | What |
|------|----:|------|
| `crates/heso-cli/src/main.rs` | +152 / -1 | In `run_dispatch`'s click-success branch, when `action.tag == "a"` with a usable href, resolve against page URL via `Url::join`, fetch destination, and augment response with `navigated: true`, `navigated_to: <url>`, plus destination's `title` / `description` / `tree` / `actions` / `metadata` / `http_status`. New helpers `follow_anchor_href` (skips empty / fragment / `javascript:` / `mailto:` / `tel:` / `data:`) and `augment_click_with_destination`. |
| `crates/heso-cli/tests/bug_fixes_click_navigation.rs` | +239 (new) | 4 integration tests against wiremock fixtures (absolute href / relative href / non-anchor / fragment-only). |

Test count: 4 new for Bug A.

Workspace-wide: `cargo test --workspace` clean — 1400+ tests pass, zero regressions.

## Before / after — the exact repros from the bug reports

### Bug B repro (from bug-reports/01-top-sites.md item 22)

`heso open https://crates.io` — crates.io now returns 404 (originally 403):

```
$ ./target/release/heso.exe open https://crates.io
# BEFORE (per bug report):
#   {"title":"","tree":...,"partial_reason":"ok",...}   ← silently lies
#   exit 0

# AFTER:
{
  "actions": [],
  "http_status": 404,
  "partial": true,
  "partial_reason": "http_404",
  "title": "",
  ...
}
```

`heso open https://httpbin.org/status/500` (deterministic 500):

```
$ ./target/release/heso.exe open https://httpbin.org/status/500
# AFTER:
  "http_status": 500,
  "partial": true,
  "partial_reason": "http_5xx",
```

`heso open https://httpbin.org/status/403` (deterministic 403):

```
$ ./target/release/heso.exe open https://httpbin.org/status/403
# AFTER:
  "http_status": 403,
  "partial": true,
  "partial_reason": "http_403",
```

`heso fetch https://httpbin.org/status/500`:

```
$ ./target/release/heso.exe fetch https://httpbin.org/status/500
# BEFORE (per bug report):
#   {"text":"","url":"..."}   ← no status, exit 0
#   exit 0

# AFTER:
{
  "http_status": 500,
  "partial": true,
  "partial_reason": "http_5xx",
  "text": "",
  "url": "https://httpbin.org/status/500"
}
```

`heso open https://www.npmjs.com` (bot-walled per bug report):

```
# BEFORE (per bug report): "Just a moment..." with partial_reason: "ok"
# AFTER:
  "http_status": 403,
  "partial": true,
  "partial_reason": "http_403",
```

`heso batch open https://httpbin.org/status/403`:

```
$ ./target/release/heso.exe batch open https://httpbin.org/status/403
{"url":"https://httpbin.org/status/403","ok":false,"error":"http_403: status=403"}
$ echo $?
1
```

All-failed batch → exit 1. Mixed (one ok + one 403) → exit 0 with both rows labeled correctly. Batch semantics preserved.

### Bug A repro (from bug-reports/02-verb-ergonomics.md headline P0)

`heso click https://en.wikipedia.org/wiki/JavaScript @e261`:

```
$ ./target/release/heso.exe click https://en.wikipedia.org/wiki/JavaScript @e261
# BEFORE (per bug report):
#   {"value": true, "selector": "...", "url": "https://en.wikipedia.org/wiki/JavaScript"}
#   ← response.url still points at JavaScript page, no destination info

# AFTER:
  "http_status": 200,
  "navigated": true,
  "navigated_to": "https://en.wikipedia.org/wiki/Brendan_Eich",
  "title": "Brendan Eich - Wikipedia",
  ...
```

The selector originally in the bug report (`a[title="Brendan Eich"]`) matched 10 elements on the live page and produced the expected ambiguous-locator error — pinning to a specific `@e261` ref demonstrates the fix.

## Test additions

```
crates/heso-engine-fetch/src/lib.rs::tests::
  partial_reason_for_status_clean_2xx_is_none
  partial_reason_for_status_4xx_carries_exact_code
  partial_reason_for_status_5xx_buckets_to_http_5xx
  bot_challenge_detected_from_cf_chl_opt_marker
  bot_challenge_detected_from_cf_jschl_tk_marker
  bot_challenge_detected_from_title_just_a_moment
  bot_challenge_title_match_is_case_insensitive
  bot_challenge_false_for_normal_pages
  partial_reason_200_with_cf_interstitial_is_bot_challenge

crates/heso-cli/tests/bug_fixes_http_truthfulness.rs::
  open_403_surfaces_http_status_and_partial_reason
  open_500_buckets_to_http_5xx
  fetch_403_surfaces_http_status_and_partial_reason
  open_cloudflare_interstitial_marks_bot_challenge
  open_clean_200_still_reports_ok
  batch_open_403_row_marked_ok_false

crates/heso-cli/tests/bug_fixes_click_navigation.rs::
  click_anchor_follows_href_and_reports_destination
  click_anchor_resolves_relative_href
  click_non_anchor_does_not_navigate
  click_fragment_only_anchor_does_not_navigate
```

All 19 new tests pass on `cargo test --workspace`.
