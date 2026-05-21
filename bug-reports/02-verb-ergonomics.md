# 02 — Verb Ergonomics: Pressure-testing heso vs Playwright on real sites

Tester: bug-finder subagent (run 2026-05-21). Binary: `target/release/heso.exe` (0.0.3, prebuilt). Driven exclusively through the CLI surface to simulate an LLM agent harness.

## TLDR

Attempted **12 scenarios**. Cleanly completed **5** end-to-end: Wikipedia search (S1), HN front-page → comments (S2), DDG search → docs.rs (S3), MDN article navigation (S6), lobste.rs front-page → comments (S10), heso batch (S11), and heso wait (S12). Two — **crates.io (S7)** and **npmjs.com (S8)** — were blocked by SPA / Cloudflare and silently returned `count: 0`. **Stack Overflow (S9)** was Cloudflare-walled at fetch. **GitHub README (S4)** and **Stripe docs sidebar (S5)** found and clicked the right link, but `click` does not return the resolved URL so the chain was forced into manual URL reconstruction.

The headline finding is that **the verbs are a leaky abstraction**: each verb is its own one-shot HTTP request, so there is **no session continuity between verbs**, and `click` on a navigational `<a href>` returns `value: true` with no `postUrl`. Only `submit` returns post-action navigation state. An LLM driving heso has to manually concat hrefs to URLs after every click to keep going. Playwright's whole value is that `page.click(...)` mutates the in-memory page and you just keep calling more methods; heso loses that the moment you hit the second verb.

The second large finding is that **error visibility is patchy**: HTTP 4xx/5xx come back as `ok: true` with empty bodies on `fetch`/`open`, CF challenges return `"Just a moment..."` as the text with no warning, and `find --role <unknown-role>` returns 0 matches with no hint that the role string is unrecognised.

On the positive side, `submit` is genuinely good (one-shot fill+post+follow), the `ambiguous: N elements matched…` error message lists candidates by ref/tag/text which is exactly what an LLM needs, and `batch open` over a shared `reqwest::Client` gave a measurable 2x speedup vs `--parallel 1` (109 ms vs 231 ms across five docs.rs URLs).

## Per-scenario walkthroughs

All commands run from the project root. Output trimmed to the relevant fields.

### S1 — Wikipedia search → article (clean)

1. `heso find https://en.wikipedia.org --role searchbox` → **count: 0**. (Wikipedia's `<input type=search>` is mapped to `role: "textbox"`, not `searchbox`, so the conventional ARIA term misses.)
2. `heso find https://en.wikipedia.org --name search` → 8 matches; the relevant form is `@e18` (id `searchform`), the input is `@e19` (id `searchInput`), submit button is `@e20`.
3. `heso submit https://en.wikipedia.org @e18 --field search=Rust` → `ok: true`, `postUrl: https://en.wikipedia.org/wiki/Rust`, full HTML in `responseBody`. Clean.
4. `heso ls https://en.wikipedia.org/wiki/Rust` (under Git Bash) → `ls failed: invalid path C:/Program Files/Git/rust: must start with /`. **Bash + MSYS path-mangling clobbers any `/path` arg.** Workaround: `//rust` (Bash escape) or run in PowerShell. (Confirmed PowerShell works fine.)
5. `heso cat https://en.wikipedia.org/wiki/Rust //rust` → first 700 chars of "Rust → chemical reactions" article.

Verb count to answer "search Wikipedia for X and read the result": 3 verbs (find, submit, ls/cat). Reasonable. Caveat — `submit @e18` worked but a search-by-default-role would not.

### S2 — Hacker News front page → top story comments (clean, with caveat)

1. `heso find https://news.ycombinator.com --role link` → 226 links. Top story is at `@e11` (`href="https://openai.com/index/model-disproves-discrete-geometry-conjecture/"`), its comments link at `@e14` (`href="item?id=48212493"`).
2. `heso click https://news.ycombinator.com @e14` → `ok: true, value: true`. **No `postUrl`, no `navigated_to`.** The agent has to look at the `href` attribute from step 1, manually resolve `item?id=48212493` against the base URL, and synthesise the next request.
3. `heso cat https://news.ycombinator.com/item?id=48212493 //` → ~1 kB of comment text. Works.

Verb count: 3 (find, click, cat) — but the click is informational only and not actually load-bearing in the chain. A naive LLM would have to know to skip the click result and go from the `href` directly. P1 ergonomic gap.

### S3 — DDG search → top result → read (clean)

1. `heso search "rust async" --limit 5` → ranked results from `ddg` + `wiki` engines, top hit `https://doc.rust-lang.org/book/ch17-00-async-await.html`. Fast (no JS engine spin-up).
2. `heso cat https://doc.rust-lang.org/book/ch17-00-async-await.html //` → **empty content**. The page's body content starts at an h1, so the "/" section has no intro text. (Use `ls` then drill in.)
3. `heso ls …` → 3 entries; `/fundamentals-of-asynchronous-programming` holds the real text.

Verb count: 3. Works, but the "empty cat at /" footgun is a real surprise — you don't know to use ls until cat returns "".

### S4 — GitHub README (partial)

1. `heso find https://github.com/torvalds/linux --name README` → 4 matches; `@e207` is the file link (`/torvalds/linux/blob/master/README`).
2. `heso click https://github.com/torvalds/linux @e207` → `ok: true, value: true`, no resolved URL. Same gap as S2.
3. `heso ls https://github.com/torvalds/linux/blob/master/README` → returned nav chrome and "Navigation Menu" at the top — actual README body is reachable but buried under chrome.
4. `heso read … --include text` (the text key the user wants) → the README body is in `tree.root.children[…].children[…].intro`, ~6 levels deep, mixed with footer/copyright. **Hard to extract for an agent.**

### S5 — Stripe docs sidebar (partial)

1. `heso find https://stripe.com/docs --name payments` → 10 matches across sections. `@e11` is the Payments tab link.
2. `heso click https://stripe.com/docs @e11` → succeeded with the usual chorus of "skipped external script" warnings (15+ entries), no `postUrl`. Same navigation-loss issue.
3. Side observation: the redirect `stripe.com/docs` → `docs.stripe.com` works; the printed URL in the response is the original (`https://stripe.com/docs`) but the relative hrefs are relative to the post-redirect host. **Agents that naive-concat `https://stripe.com/docs` + `/payments` will hit a 404.** They have to know to use the response's actual URL.

### S6 — MDN article (clean)

1. `heso find https://developer.mozilla.org/en-US/docs/Web/JavaScript --name Promise` → 3 matches; section was `/javascript/help-improve-mdn` for *all three* — semantically wrong (these links are in the article body, not in a "Help improve MDN" sidebar). Indicates the section-detector picks the nearest preceding heading text and gets confused by long page structure.
2. `heso ls https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise` → 1 top-level section `/promise`.
3. `heso cat … //promise` → clean intro text including "The Promise object represents the eventual completion (or failure) of an asynchronous operation."

### S7 — crates.io (blocked)

1. `heso fetch https://crates.io` → returned full HTML.
2. `heso find https://crates.io` → **count: 0**. (Ember.js SPA — body is empty `<div id="ember-app">` until the JS hydrates.)
3. `heso eval-dom --js-fetch https://crates.io "document.title"` → also empty string. The bundled JS engine doesn't run Ember.
4. **Doc claims `heso read` supports `--js-fetch`** (per `heso --help` text under `batch`), but `heso read --js-fetch …` errors: `unknown flag --js-fetch`. CLI vs docs drift.
5. No warning is emitted when find returns 0 matches on an SPA. Agent has no signal that the issue is "site needs JS" rather than "no interactive elements on the page".

### S8 — npmjs.com (blocked)

1. `heso fetch https://www.npmjs.com` → response body is literally `"Just a moment..."` (Cloudflare interstitial).
2. `heso find https://www.npmjs.com` → `count: 0`.
3. heso never says "this looks like a Cloudflare/anti-bot challenge". An LLM seeing `count: 0` has no way to disambiguate "page has no interactive elements" from "we got blocked by CF".

### S9 — Stack Overflow deep link (blocked)

1. `heso fetch https://stackoverflow.com/questions/76252011` → `"Just a moment..."` (CF). Same as S8.

### S10 — lobste.rs (clean)

1. `heso find https://lobste.rs --role link` → 274 links.
2. `heso click https://lobste.rs --text "Active"` → matched `@e1` (case-insensitive). Good.
3. `heso click https://lobste.rs --text "Login"` → `ambiguous: 2 elements matched locator { text: "Login" }` followed by a candidate list with `@e5` / `@e6` and both texts. **Excellent error.**
4. `heso click https://lobste.rs --text "Comments"` → **`ambiguous: 25 elements matched`** — substring match means the nav link "Comments" collides with all 14-comments-style story links. Locator is too fuzzy for common cases.
5. `heso click https://lobste.rs --aria-label "Active"` → `no element matched locator { aria-label: "Active" }` — `--aria-label` only checks the literal attribute, doesn't fall back to accessible name from text content. Inconsistent with `--text`.
6. `heso find https://lobste.rs --name comments` → 25 matches; pick `@e15` (link href `/s/k21pdb/...`).
7. `heso cat https://lobste.rs/s/k21pdb/aggressive_ai_scrapers_are_making_it //` → readable comment text.

Verb count: 3-4. Works once you know to use `find --name` not `click --text` for non-unique link text.

### S11 — heso batch (clean)

1. `heso batch open https://example.com https://docs.rs https://crates.io/users/sign-in https://lobste.rs https://en.wikipedia.org` → 5 JSONL records, all `ok: true`. Completion order was: example.com, docs.rs, lobste.rs, crates.io, en.wikipedia.org (Wikipedia last because the page is large).
2. `heso batch open https://docs.rs/{tokio,reqwest,serde,clap,anyhow} --parallel 5` → completes in 109 ms. Same call with `--parallel 1` → 231 ms. **Shared connection pool + parallel scheduling delivers.**

### S12 — heso wait (mostly clean)

1. `heso wait https://example.com --selector-exists h1` → ok in 0 ms.
2. `heso wait https://example.com --selector-exists '#nope' --timeout 3s` → exit 1, `error: timeout`, `elapsed_ms: 3006`. Honest.
3. `heso wait https://example.com --text-contains "example"` → ok in 0 ms.
4. `heso wait https://example.com --network-idle` → ok in 503 ms (default 500 ms idle window — works).
5. `heso wait https://lobste.rs --text-contains "Privacy" --timeout 8s` → timeout in 8009 ms (lobste.rs front page doesn't actually contain "Privacy", confirmed).
6. `heso wait https://example.com --url-matches "no[t.match"` → `--url-matches: invalid regex: regex parse error … unclosed character class`. Clean error.
7. `heso wait https://example.com --time 2s` → **returns instantly, `elapsed_ms: 0`.** The flag advances the deterministic virtual clock, not wall-clock. The name `wait --time` reads like "sleep 2s" and is going to surprise every first-time user.

## Bug list

| Severity | Verb | Symptom | Site | Repro command |
|---|---|---|---|---|
| P0 | `click` | Returns `value: true` and `selector` but no resolved URL after navigating an `<a href>`. Forces agents to manually concat hrefs from a prior `find`. Breaks the whole "click → next page" chain. | HN, GitHub, Stripe, lobste.rs (every site) | `heso click https://news.ycombinator.com @e14` |
| P0 | `fetch` / `open` | HTTP 4xx and 5xx return `ok: true` with empty body, no status field. Agents cannot tell success from failure. | httpbin | `heso fetch https://httpbin.org/status/500` |
| P0 | `fetch` | Cloudflare interstitial returned as `text: "Just a moment..."` with no warning. `find` then returns `count: 0` with no hint why. | npm, SO | `heso fetch https://www.npmjs.com` |
| P1 | `find` / `read` / SPA | Pages that hydrate via JS (Ember/React SPAs) return `count: 0` actions; `--js-fetch` doesn't rescue crates.io. No warning surfaces. | crates.io, npm | `heso find https://crates.io` |
| P1 | `read` | `--help` and README both imply `--js-fetch` works on `read`; CLI rejects with `unknown flag --js-fetch`. Doc/CLI drift. | crates.io | `heso read --js-fetch https://crates.io` |
| P1 | `ls` / `cat` / `find --section` | Under Git Bash on Windows, any arg starting with `/` is MSYS-translated to `C:/Program Files/Git/...`. heso surfaces the mangled string verbatim in errors and in the response `filters.section`, but doesn't catch the obvious pattern (`looks like a Windows path, not a node path`). | any | `heso ls https://en.wikipedia.org/wiki/Rust /rust` |
| P1 | `submit` | Accepts a ref pointing at an `<input>` (not a form), silently posts to `action=""` with no field data, returns `ok: true`. Should error or auto-resolve to the closest enclosing `<form>`. | Wikipedia | `heso submit https://en.wikipedia.org @e19` |
| P1 | `wait --time` | Returns instantly (`elapsed_ms: 0`) instead of sleeping. Flag advances a virtual clock, not wall-clock; verb name + arg shape (`--time 2s`) lead users to expect a real wait. | any | `heso wait https://example.com --time 2s` |
| P1 | `click --text` | Substring match across all elements. Nav link "Comments" collides with 24 "N comments" story links. Needs at least an opt-in `--exact` or implicit exact-text-first/substring-fallback strategy. | lobste.rs | `heso click https://lobste.rs --text "Comments"` |
| P1 | `cat` | When the page body's first child is an `<h1>` (no preamble text), `cat URL /` returns `content: ""` with no hint to drill in via `ls`. | docs.rs, MDN | `heso cat https://doc.rust-lang.org/book/ch17-00-async-await.html //` |
| P2 | `find` | Unknown roles (`--role nonsense` or `--role searchbox` when the site doesn't use that role) return `count: 0` with no validation. Hard to debug "did I spell the role wrong, or does the page just lack it?" | Wikipedia | `heso find https://en.wikipedia.org --role searchbox` |
| P2 | `click --aria-label` | Only matches the literal `aria-label` attribute. Doesn't fall back to accessible-name computation (which would include text content). Inconsistent with `--text` which is case-insensitive substring. | lobste.rs | `heso click https://lobste.rs --aria-label "Active"` |
| P2 | `find` | Section assignment is sometimes wrong — MDN Promise links in body content were tagged `section: /javascript/help-improve-mdn`. Suggests the section-detector grabs the nearest preceding heading globally rather than respecting the DOM ancestor tree. | MDN | `heso find https://developer.mozilla.org/en-US/docs/Web/JavaScript --name Promise` |
| P2 | `fill` | Verb writes to the in-memory DOM and exits. Because every verb is a fresh process, the typed value never reaches a follow-up `submit`. Useful only paired with `eval-dom` in one shot — `submit --field` is the actual right tool, but the verb's existence as a sibling of submit is misleading. | Wikipedia | `heso fill … @e19 "Rust"` then `heso submit … @e18` |
| P2 | `fill` / dispatch | On Wikipedia, every dispatch verb emits `RLCONF is not defined / RLQ is not defined` script errors into `console`. Looks alarming; user can't easily tell it's benign noise from MediaWiki's own bootstrap script trying to call into a global the agent harness chose not to populate. | Wikipedia | `heso fill https://en.wikipedia.org @e19 "Tokio"` |
| P3 | help text | `heso --help` lists `--js-fetch` under `batch read`, which suggests it's a read flag; actual surface differs. | — | `heso --help` vs `heso read --js-fetch …` |

## Ergonomic observations

**The click-doesn't-tell-you-where-you-landed problem is the single largest gap.** Every multi-step navigation scenario hit it. The mental model heso wants — "verbs are the differentiator vs Playwright" — assumes the verbs compose. They mostly don't, because each one is a fresh fetch and only `submit` propagates the resolved URL. Concretely, today's chain to read a HN story's comments is:

```
heso find https://news.ycombinator.com --name comments     # discover @e14 with href="item?id=…"
# Now MANUALLY look at @e14.attrs.href, MANUALLY concat with the base URL
heso cat https://news.ycombinator.com/item?id=48212493 //  # finally read
```

The natural chain — `click @e14` → "now read the page that click landed on" — does not work because the click result doesn't expose the landing URL. An LLM agent has to know to ignore the click result and go straight to the href attribute. Worse, on sites that use relative URLs (Stripe), it has to know to use the response's `url` field (which may differ from the requested URL after a redirect) as the base. That's a lot of "first you have to know this" for a tool whose pitch is "one binary, ergonomic verbs."

**HTTP status invisibility is the second-largest issue.** An agent asked to "fetch this URL and tell me what it says" cannot distinguish a 500 from a 200 with empty body. `heso fetch https://httpbin.org/status/500` returns `{"text": "", "url": "..."}`. No `status_code`, no `ok: false`. Same for `heso open`. Cloudflare challenges leak into the success path (`text: "Just a moment..."`). Reliable agents need a status field.

**Locator ergonomics are uneven.** `--text` is case-insensitive substring, `--aria-label` is exact-string-against-literal-attribute. Submit / click / fill all share locators, so when one works and another doesn't, it's surprising. Practically: prefer `find` → harvest `@eN` refs → `click @eN`. Don't reach for `--text` on common words.

**Path mangling under Git Bash is brutal.** Almost every `cat URL /` or `ls URL /path` under Bash on Windows hits MSYS path translation. heso prints `C:/Program Files/Git/...` back at the user. We could detect the pattern (any path arg that starts with a Windows drive letter and includes `Git`) and emit a one-line `did you mean /...? (Git Bash translated your /path; use // or run under PowerShell)`.

**Error messages are mostly excellent.** The `ambiguous: N elements matched locator … candidates (use one of these refs):` block is gold — it tells the agent exactly what to do next. `no element at ref @e9999` is clean. `invalid regex … unclosed character class` shows the underlying error. Heso has good error UX where it has error UX; the failures above are "no error emitted at all" failures, not "error message is bad" failures.

**Section paths in `find` output are sometimes semantically wrong.** S6 (MDN) put body-content Promise links under `/javascript/help-improve-mdn`. Agents that filter by `--section` to scope to a known region of the page would be misled.

**`fill` exists but composes with nothing.** Because of the no-session model, `heso fill` writes to a DOM that's thrown away the moment the process exits. Submit has its own `--field` / `--data` flags that bypass it entirely. We should either rename / hide `fill`, or ship a session model (`heso open --save sess.json` + `heso fill --session sess.json` + `heso submit --session sess.json`) that makes the verb honest.

**MediaWiki and GitHub both spew unrelated console errors during dispatch.** The agent sees 4-6 entries of "RLCONF is not defined" / "skipped external script" per call. Filterable noise, but it makes it hard to spot real errors.

## Top 5 ergonomic fixes (highest impact first)

1. **`click` should return `navigated_to: <resolved-href>` (or `null` if it was a JS handler with no nav).** This unlocks the whole multi-step chain that today requires manual href-resolution. Implementation is mostly already there for `<a href>` — compute it from the matched element's `href` attribute resolved against the page's base URL, before dispatch. For form-submit-buttons inside a form, ideally also fetch the resulting page and surface `responseBody` like `submit` does. P0 because it directly contradicts the verb-chain pitch.

2. **Always surface HTTP status in `fetch` / `open` / `read` (and bubble it up through `batch`).** Add `{status_code, status_ok}` to every fetched-page response. Treat 4xx/5xx as `ok: false` by default (with a `--allow-error-status` opt-in). Also: detect the Cloudflare "Just a moment…" interstitial body and emit a `partial_reason: "challenge_page"` instead of `"ok"`. Agents currently have zero signal for these failure modes.

3. **Make locator semantics symmetric and predictable, with `--exact` opt-in.** Today `--text` is case-insensitive substring and `--aria-label` is exact-attribute. Either (a) make both substring-with-`--exact`, or (b) make both exact-with-`--contains`. Either way, fix the worst-case: `click --text Comments` on lobste.rs matching 25 elements is a paper cut every multi-step session will hit. Also: `--aria-label` should compute the accessible name (text content fallback) rather than just reading the literal attribute. Brings parity with Playwright `getByText` / `getByLabel`.

4. **Diagnose the SPA / anti-bot / empty-actions case and tell the user what happened.** When `find` returns `count: 0`, run one extra heuristic: does the served HTML have `<div id="app">` or `<div id="root">` and an empty body? Did the response body match the Cloudflare interstitial signature? Did all the `<script>` tags get skipped because of no `--js-fetch`? Surface one of `empty_html`, `looks_spa`, `looks_challenge_page`, `no_js_executed` in the output. Without this, every Ember/React/CF site is a silent failure and the user blames heso.

5. **Detect MSYS path-mangling on Windows and emit a one-line hint.** When a `path` arg matches `^[A-Z]:[/\\].*` instead of `^/`, print `note: Git Bash translated your /path arg to '<mangled>'. Use '//path' or run heso under PowerShell.` Five lines of code, kills a whole class of "wtf heso why" moments for every Windows user. Bonus: also flag `cat URL /` returning `""` with a `hint: try \`heso ls URL\` to discover sections — the page's intro is empty because its first child is a heading`.

Honorable mention: **a session model** that lets `fill` / `click` actually compose with each other (state.json mentions T-XX style follow-up work). Today, the verb surface fakes statefulness in `submit --field` and that's enough for forms, but not for "click cookie-banner OK → search → read result"-style chains. This is bigger than fix #5 and probably needs an ADR, so it's not in the top 5, but it's the long-tail follow-up.

---

End of report. No source modified. Background processes: none.
