# heso top-sites stress test — 2026-05-21

Engine: `heso 0.0.3` (release build from `target/release/heso.exe`).
Methodology: 47 real public URLs across news, docs, repos, reference, product landings, forums, Q&A, search, registries, status pages, SPAs. For each: `timeout 30 heso open <url>` then `timeout 30 heso read <url>`. Scored from the JSON: title, action count, `partial_reason`, `failed_scripts`, `text` length.

# TLDR

Out of 47 sites tested, **10 PASS clean, 31 DEGRADED, 6 effectively unusable**, **0 crashes / 0 hangs** in heso itself (the binary never panicked or timed out). The dominant failure mode is the embedded JS engine lacking common DOM globals — `getElementsByClassName`, `XMLHttpRequest`, `performance.mark`, every `HTML*Element` constructor — which breaks `document` hydration on the majority of major sites. One real network-level bug: heso silently returns empty title+body on a 403 response (crates.io). One real protocol bug: heso cannot fetch `data:` URLs (kills reddit).

# Pass/Fail Table

Legend: **PASS** = clean (no failed scripts, populated title/actions/text). **DEGRADED** = data returned, but scripts crashed or content is sparse. **EMPTY** = no usable title/text. **CRASH** = heso panic / non-zero exit (none observed). **BLOCKED** = remote returned bot challenge.

| # | URL | open | read | notes |
|---|---|---|---|---|
| 01 | news.ycombinator.com | DEGRADED | DEGRADED | `hn.js` crashes on first call to `byClass` → `getElementsByClassName` missing. Title/actions/text still populate from static HTML (228 actions). |
| 02 | theverge.com | DEGRADED | DEGRADED | "XMLHttpRequest is not defined" inside next.js bundle. 314 actions, 19k chars text — usable. |
| 03 | bbc.com | DEGRADED | DEGRADED | "Load full version of piano SDK" (Piano consent SDK throws). 262 actions, 13k chars — usable. |
| 04 | theguardian.com | DEGRADED | DEGRADED | "NodeList is not defined". 393 actions, 19k text — usable. |
| 05 | arstechnica.com | DEGRADED | DEGRADED | 4 failed scripts: `cannot read property 'createElement' of undefined`, `'search' of undefined`. 222 actions, 9k text. |
| 06 | developer.mozilla.org | DEGRADED | DEGRADED | `airgap.js` (Transcend consent) crashes on `not a function`. 635 actions, 19k text — content still usable. |
| 07 | docs.python.org | DEGRADED | DEGRADED | `sidebar.js` / `themetoggle.js` / `switchers.js` all crash on `not a function` (calling `getElementsByClassName`). 105 actions, 2.9k text. |
| 08 | kubernetes.io/docs | DEGRADED | DEGRADED | 6 failed scripts (`createElement of undefined`). 1776 actions, 25k text — actually very good. |
| 09 | docs.docker.com | DEGRADED | DEGRADED | XHR missing + `className of undefined`. 45 actions, only 1k text — search/AI content suspiciously short. |
| 10 | github.com/torvalds/linux | DEGRADED | DEGRADED | **93** failed scripts: every github-assets bundle starts with `performance.mark(...)`. 249 actions, 10k text still works from static HTML. |
| 11 | github.com/microsoft/typescript | DEGRADED | DEGRADED | Same `performance.mark` issue, 93 failed scripts. |
| 12 | en.wikipedia.org/wiki/JavaScript | DEGRADED | DEGRADED | `RLCONF is not defined`, `RLQ is not defined` (MediaWiki ResourceLoader globals lost between inline scripts). 1795 actions, 67k text. |
| 13 | en.wikipedia.org/wiki/Rust_(programming_language) | DEGRADED | DEGRADED | Same RLCONF/RLQ. 1937 actions, 79k text — content extraction excellent. |
| 14 | stripe.com | **PASS** | **PASS** | No failed scripts, 71 scripts executed clean. 208 actions, 12k text. |
| 15 | vercel.com | **PASS** | **PASS** | 346 scripts executed clean. 195 actions, 7k text. |
| 16 | anthropic.com | DEGRADED | DEGRADED | 3 failed scripts (`createElement of undefined`). 180 actions, 4k text. |
| 17 | openai.com | **EMPTY** | **EMPTY** | Inline script #1 crashes on `cannot read property 'document' of undefined` → 0 actions, 0 text, empty title. **Worst case.** |
| 18 | lobste.rs | **PASS** | **PASS** | 0 scripts executed, no failures. 274 actions, 3.9k text. |
| 19 | news.ycombinator.com/item?id=39538886 | DEGRADED | DEGRADED | Same `byClass` crash as HN root, but story+comments text extracted fine. |
| 20 | stackoverflow.com/questions/4869712 | **BLOCKED** | **BLOCKED** | "Just a moment..." Cloudflare challenge page. 0 actions, 16 chars text. heso reports `partial=ok` — should signal `bot_challenge`. |
| 21 | duckduckgo.com | **PASS** | DEGRADED | 41 scripts execute clean, but DDG is a JS-heavy next.js SPA and only 170 chars of text rendered. Title+16 actions OK. |
| 22 | crates.io | **EMPTY** | **EMPTY** | Server returns **HTTP 403** ("API data access policy"). heso silently swallows non-2xx, returns empty title/tree/text with `partial=ok` and exit 0. **P1 truthfulness bug.** |
| 23 | npmjs.com/package/react | **BLOCKED** | **BLOCKED** | Cloudflare interstitial. heso reports `partial=ok`. |
| 24 | pypi.org/project/requests | **BLOCKED** | **BLOCKED** | "Client Challenge" anti-bot. heso reports `partial=ok`. |
| 25 | docs.rs/serde | DEGRADED | DEGRADED | `HTMLLinkElement is not defined`; `settingsDataset is not initialized`; **`no setter for property` when assigning `element.style = "..."`** (string-to-style coercion not supported). 94 actions, 4.7k text. |
| 26 | githubstatus.com | DEGRADED | DEGRADED | 14 failed scripts (`createElement`, `currentPage`, `pollForChanges` of undefined). 125 actions, 15k text. |
| 27 | linear.app | DEGRADED | DEGRADED | `HTMLScriptElement is not defined` inside Next.js webpack chunk. 164 actions, 9k text. |
| 28 | notion.com | **PASS** | **PASS** | 29 next.js scripts execute clean. 142 actions, 4k text. |
| 29 | slack.com | DEGRADED | DEGRADED | 9 failures: XHR, `HTMLImageElement`, `createElement`, `track`/`model` of undefined. 310 actions, 11k text. |
| 30 | wiki.archlinux.org | DEGRADED | DEGRADED | `RLCONF is not defined` (MediaWiki). 127 actions, 2.7k text. |
| 31 | react.dev | **PASS** | **PASS** | Clean. 145 actions, 8k text. |
| 32 | rust-lang.org | **PASS** | **PASS** | Clean. 38 actions, 3.5k text. |
| 33 | go.dev | DEGRADED | DEGRADED | `not a function` in `prepMobileNavigationDrawer`; `createElement of undefined`. 159 actions, 7k text. |
| 34 | example.com | **PASS** | **PASS** | Control case — clean. 1 action, 142 chars (page itself is tiny). |
| 35 | reddit.com/r/programming | DEGRADED | DEGRADED | **Three `data:text/javascript,...` URLs all fail with `send: builder error` — reqwest doesn't handle data: scheme.** 83 actions, 5.9k text. |
| 36 | cloudflare.com | DEGRADED | DEGRADED | XHR, `HTMLVideoElement`, `not a function`. 103 actions, 7k text. |
| 37 | wikipedia.org | DEGRADED | DEGRADED | `portalSearchDomain is not defined`. 384 actions, 6k text. |
| 38 | medium.com | **BLOCKED** | **BLOCKED** | Cloudflare interstitial. `partial=ok` reported. |
| 39 | nextjs.org | **PASS** | **PASS** | Clean. 109 actions, 7k text. |
| 40 | supabase.com | **PASS** | **PASS** | Clean. 204 actions, 15k text. |
| 41 | cnn.com | DEGRADED | DEGRADED | `'pathname' of undefined`, "Automatic publicPath is not supported in this browser". 579 actions, 13k text. |
| 42 | nytimes.com | DEGRADED | DEGRADED | XHR missing, `'length' of undefined`, `'setInterval' of undefined`. 221 actions, 10k text. |
| 43 | x.com | **BLOCKED** | **BLOCKED** | "Something went wrong, but don't fret..." — anti-bot. Empty title, only 170 chars in tree intro. `partial=ok`. |
| 44 | docs.stripe.com | DEGRADED | DEGRADED | One `not a function`. 80 actions, 3k text. |
| 45 | en.wikipedia.org/wiki/Special:Random → article | DEGRADED | DEGRADED | RLCONF/RLQ. Resolved through redirect to a real article. 232 actions, 5.5k text. |
| 46 | blog.cloudflare.com | DEGRADED | DEGRADED | XHR missing. 320 actions, 9.5k text. |
| 47 | reuters.com | DEGRADED | DEGRADED | DataDome captcha (`ct.captcha-delivery.com`) script gets a `no setter for property` error. Title=`reuters.com`, 0 actions, 11 chars text. **BLOCKED in practice.** |

## Score totals

| Score | Count | % |
|---|---|---|
| PASS | 10 | 21% |
| DEGRADED (data returned, scripts crashed) | 31 | 66% |
| EMPTY (no usable content) | 2 | 4% |
| BLOCKED (remote anti-bot) | 6 | 13% (overlapping with EMPTY in 1 case) |
| CRASH (heso panicked) | 0 | 0% |
| HANG (timeout fired) | 0 | 0% |

Note: BLOCKED sites overlap with EMPTY — that's not double-counting. The honest split is: **10 fully clean, 31 partially working, 6 stubbed by anti-bot, 0 heso-side crashes**.

# Bug Cluster List

| Severity | Cluster | Sites affected | Symptom | Repro command |
|---|---|---|---|---|
| **P0** | **Missing `getElementsByClassName` on `document`** | HN root, HN item, MDN, python docs, k8s, anthropic, ghstatus, slack, go.dev, k8s, ars, cnn (indirect via `createElement of undefined` chains rooted in querying-by-class libs) | Every script that calls `document.getElementsByClassName(...)` throws `not a function`. Verified: `heso eval-dom https://example.com "typeof document.getElementsByClassName"` returns `"undefined"`. | `./target/release/heso.exe open https://news.ycombinator.com` — see `failed_scripts[0].message = "not a function\n    at byClass (eval_script:2:67)"`. |
| **P0** | **Missing `performance.mark` (and friends)** | github.com/torvalds/linux, github.com/microsoft/typescript (and presumably every github page) | Every github-assets bundle starts with `performance.mark("js-parse-end:...")`. 93 of 93 external scripts crash. Verified: `heso eval-js "typeof performance.mark"` returns `"undefined"` though `performance.now` is `"function"`. | `./target/release/heso.exe read https://github.com/torvalds/linux` — `scripts.executed_with_error = 93`. |
| **P0** | **Missing `XMLHttpRequest` global** | theverge, docs.docker, slack, cloudflare, blog.cloudflare, nytimes | Bundled libs (jQuery-style fetch shims, Cookielaw, etc.) fall back to XHR when fetch is absent; XHR is also undefined, so the whole bundle dies. Verified: `heso eval-dom URL "typeof XMLHttpRequest"` → `"undefined"`. | `./target/release/heso.exe open https://www.theverge.com` — `failed_scripts` contains `"XMLHttpRequest is not defined"`. |
| **P0** | **Missing `HTML*Element` constructors** | docs.rs/serde (HTMLLinkElement), linear.app (HTMLScriptElement), slack.com (HTMLImageElement), cloudflare.com (HTMLVideoElement), guardian.com (NodeList) | Code uses `instanceof HTMLLinkElement` / `instanceof HTMLImageElement` (cf. lazy-loading polyfills, framework hydration) and the constructor isn't bound on `window`. Verified: `typeof HTMLLinkElement === "undefined"` in eval-dom. | `./target/release/heso.exe read https://docs.rs/serde` — `"HTMLLinkElement is not defined\n    at <anonymous> (eval_script:1:208)"`. |
| **P0** | **`window.fetch` not provided to scripts by default** | Implicit across every site running modern JS — falls back to XHR (also missing) | `heso eval-dom URL "typeof fetch"` returns `"undefined"`. `--js-fetch` flag fixes it for `eval-dom`, but `heso open` / `heso read` do not seem to pass that through to inline-script execution. | `./target/release/heso.exe eval-dom https://example.com "typeof fetch"` → `"undefined"`; with `--js-fetch` → `"function"`. |
| **P1** | **HTTP non-2xx responses silently return empty body** | crates.io (HTTP 403), pypi (anti-bot 200 stub), npmjs / stackoverflow / medium / x.com (200 with challenge body) | A 403 returns `{"text":"", "url":...}` with exit 0 and `partial_reason="ok"`. There is no `http_status`, no `bot_challenge` partial reason. An agent driving heso has no way to know it got blocked vs. got a real empty page. | `./target/release/heso.exe fetch https://crates.io` returns `{"text":"","url":"https://crates.io/"}` exit=0 despite 403. |
| **P1** | **`data:` URL scheme not supported in script fetcher** | reddit.com/r/programming (3 separate data:text/javascript inline-loaders fail) | Reddit (and several modern sites) inline small bootstrap scripts via `data:text/javascript,...` `<script src=...>`. heso's reqwest-backed fetcher reports `send: builder error for url (data:text/javascript,...)`. Reason: reqwest only speaks HTTP(S). Needs an inline handler that parses the data URL and feeds the body straight to the script engine. | `./target/release/heso.exe open https://www.reddit.com/r/programming` — `failed_scripts[*].reason = "fetch_failed"` with `data:text/javascript,…` URLs. |
| **P1** | **`element.style = "css string"` assignment unsupported** | docs.rs/serde, reuters.com (captcha-delivery c.js) | Code does `backdrop.style = "display:none;position:fixed;…"`. heso throws `no setter for property`. The `style` property must support string-coercion (writes propagate to a CSSStyleDeclaration in real browsers). | `./target/release/heso.exe read https://docs.rs/serde` — `"no setter for property\n    at <anonymous> (eval_script:37:5)"` URL `menu.js`. |
| **P1** | **Wikipedia MediaWiki `RLCONF` / `RLQ` lost between inline scripts** | en.wikipedia.org/* (3 pages tested), wiki.archlinux.org, wikipedia.org root | First inline script sets `RLCONF = {...}` / `RLQ = window.RLQ || []`. A later inline script crashes with `RLCONF is not defined` / `RLQ is not defined`. Either inline `<script>`s aren't sharing the same global object, or `var` declarations don't escape onto `window`. (MediaWiki is a great canary because every Wikipedia article ships the exact same pattern.) | `./target/release/heso.exe open https://en.wikipedia.org/wiki/JavaScript` — `failed_scripts` includes `"RLCONF is not defined"`. |
| **P2** | **Anti-bot challenge pages not flagged** | stackoverflow.com (Just a moment...), npmjs.com (Just a moment...), pypi.org (Client Challenge), medium.com (Just a moment...), x.com (Something went wrong...) | All five return title strings that obviously indicate a bot wall, but `partial_reason="ok"`. heso should heuristically detect `Just a moment...` / Cloudflare HTML and emit `partial_reason="bot_challenge"` so the caller can route around. | `./target/release/heso.exe open https://www.npmjs.com/package/react` — `title="Just a moment..."`, `actions=[]`, but `partial_reason=ok`. |
| **P2** | **`Image`, `Audio` constructors undefined** | Latent on lazy-loading-heavy news sites (theverge has 93 lazy_images, guardian 90, slack 56) | `heso eval-dom URL "typeof Image"` returns `"undefined"`. Many pre-hydration scripts do `new Image(); img.src = ...` to warm the cache. | `./target/release/heso.exe eval-dom https://example.com "typeof Image"` → `"undefined"`. |
| **P2** | **jQuery / `$` not initialised, page-internal jQuery scripts dependent on it crash** | Likely linked to "$ is not defined" appearing 11 times across read outputs (ghstatus, arch wiki, etc.) | Scripts loaded by sites that ship jQuery (via `<script src>`) crash before jQuery's own bundle runs to completion — feeding the cascade. Root cause is probably one of the above clusters (the jQuery bundle is the one that crashed); listed separately because of frequency. | `./target/release/heso.exe read https://www.githubstatus.com` — `failed_scripts` includes `"$ is not defined"`. |
| **P3** | **`scripts.executed` reported as `0` even when `failed_scripts` is non-empty** | HN, HN item, lobsters, wiki-js, wiki-rust, github-linux, github-ts, archwiki | `scripts.executed` should reflect total scripts attempted. On HN the counter shows `executed=0, executed_with_error=1` — the failed script does not count as "executed". This is a metrics bookkeeping bug that makes it hard to tell whether scripts ran but failed (degraded) vs. scripts didn't run at all. | `./target/release/heso.exe read https://news.ycombinator.com` — `scripts = {"executed":0, "executed_with_error":1, ...}`. |

# Top 5 fixes (ranked by impact)

1. **Add `document.getElementsByClassName` (+ `getElementsByName`, `getElementsByTagName` already exists per the eval-dom check, but verify `getElementsByClassName` returns an `HTMLCollection`-shaped object).** This single API is called by virtually every hand-written legacy script (HN's `hn.js` is *just* aliases for it, Sphinx-generated python docs depend on it, k8s docs use it, and so does anything jQuery-pre-1.7). Fixing it converts ~10 sites from "first inline script crashes immediately" to "script runs to completion." **Highest agent-facing impact per LoC of patch.**

2. **Add `performance.mark`, `performance.measure`, `performance.clearMarks`, `performance.clearMeasures`.** GitHub assets all start with `performance.mark("js-parse-end:…")` and 93 of 93 external scripts blow up before they even reach their real logic. Every page on github.com is in this state. Making these no-op stubs (return `undefined`, accept any args) takes 5 lines and rescues all of github.com plus any production app using the navigation-timing API.

3. **Bind `XMLHttpRequest` and the `HTML*Element` constructor family (`HTMLLinkElement`, `HTMLScriptElement`, `HTMLImageElement`, `HTMLVideoElement`, `NodeList`, `Image`, `Audio`) onto `window`.** These are constructor *references*, used overwhelmingly for `instanceof` checks in framework hydration code (linear.app, slack, supabase-adjacent, docs.rs). They don't need to be functional constructors — they just need to be the prototype objects so `instanceof` succeeds. The XHR one is bigger: it should be a usable network shim (forward to the same reqwest pool that `--js-fetch` uses). This is the gating fix for the entire next.js/webpack hydration class of sites.

4. **Surface HTTP status codes and detect bot-challenge pages.** Concretely:
   - In `heso fetch` / `open` / `read` JSON output, add `http_status: 200`. When non-2xx, set `partial_reason="http_error"` and include the status text.
   - Add a regex/heuristic pass over the rendered title and first 500 bytes of body. If it matches Cloudflare's "Just a moment..." or Datadome's "Client Challenge" or x.com's "Something went wrong", set `partial_reason="bot_challenge"`.
   - This is a truthfulness fix: today an agent driving heso has no way to distinguish "site is genuinely empty" from "you got blocked." That's the worst class of silent failure in an agent context — the model will happily write a downstream plan as if the page had no content.

5. **Implement `data:` URL handling in the inline-script fetcher.** Reddit ships three `<script src="data:text/javascript,...">` tags for environment-injection (`window.STICKY_CANARY`, `window.PRE_PRODUCTION`, fetch-wrapping). All three fail with `send: builder error` because reqwest only knows HTTP. Fix: in the script-loader path, before handing to reqwest, check for `url.scheme() == "data"`, decode the body inline (RFC 2397 — `data:[<mediatype>][;base64],<data>`), and evaluate it directly. Tiny patch (~20 lines), but it's the gating fix for reddit and a growing fraction of modern SPAs that inline runtime config this way.

## Honorable mentions (not in top 5 but cheap wins)

- **Wikipedia/MediaWiki `RLCONF`/`RLQ` scope leak fix.** Wikipedia is the highest-traffic content surface on the public web. The bug is almost certainly: multiple `<script>` tags share an *inline-only* global object but `let`/`var` declarations at the top level aren't reaching `window`. Confirm by checking what `var foo = 1; ` in the first inline script makes `window.foo` resolve to in a follow-up script. If `undefined`, that's the bug.

- **`element.style = "..."` string-to-CSS coercion.** docs.rs and the DataDome captcha agent both hit this. Real browsers route assignment to `style` through a setter that parses the string into a CSSStyleDeclaration. heso throws "no setter for property" — implying the property is defined as get-only. Make it accept strings.

- **`heso fetch`/`read` should also report the response Content-Type and Content-Length.** Currently crates.io's 403 returns nothing useful; even a `Content-Type: text/plain; charset=utf-8` hint would let a caller detect that the body isn't HTML.

## Methodology footnotes

- All runs done with `timeout 30 ./target/release/heso.exe <verb> <url>` on Windows 11 PowerShell harness, via Bash.
- Raw outputs live in `bug-reports/raw/NN-<slug>-{open,read}.{out,err}` (47 sites × 2 verbs = 94 .out files).
- No heso runs panicked; no .err files contained content. All exits = 0.
- DOM-API gap checks done via `heso eval-dom https://example.com "typeof Foo"`. Confirmed missing: `getElementsByClassName`, `XMLHttpRequest`, `NodeList`, `HTMLLinkElement`, `HTMLScriptElement`, `HTMLImageElement`, `HTMLVideoElement`, `Image`, `Audio`, `performance.mark`, `fetch` (default; `--js-fetch` fixes), `CSSStyleSheet`. Confirmed present: `getElementById`, `getElementsByTagName`, `querySelector(All)`, `addEventListener`, `Element`, `HTMLElement`, `Document`, `Event`, `CustomEvent`, `history`, `location`, `setTimeout`, `requestAnimationFrame`, `localStorage`, `sessionStorage`, `customElements`, `matchMedia`, `IntersectionObserver`, `MutationObserver`, `navigator`, `FormData`, `URL`, `URLSearchParams`, `Blob`, `File`, `Promise`, `Map`, `Set`, `Symbol`, `Proxy`, `performance.now`.
