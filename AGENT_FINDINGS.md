# heso real-agent-workload findings

Generated 2026-05-19 by the agent test run. heso commit: `aeb0738`

Tested against `target/release/heso.exe` built from current working tree (uncommitted changes in `crates/heso-cli/src/main.rs`, `crates/heso-trace/{Cargo.toml,src/lib.rs}` per `git status`).

## Task 1 — Extract HN top stories

**Goal:** Get the top 5 story titles + URLs from `news.ycombinator.com` as `[{title, url}]`.

**Verb sequence tried:** `heso eval-dom https://news.ycombinator.com/ "<js>"`

**Result:** **partial → ok with workaround.**

**First attempt:** read `a.href` directly. Got titles back, but every `url` field was missing from the JSON — `a.href` silently does not reflect the resolved URL string for relative-href anchors.

**Workaround:** use `a.getAttribute('href')` instead. That returned the absolute URLs because HN uses absolute `href` attributes on story titles. Final output (5/5 correct):

```json
[
  {"title":"Apple unveils new accessibility features","url":"https://www.apple.com/newsroom/2026/05/apple-unveils-new-accessibility-features-and-updates-with-apple-intelligence/"},
  {"title":"I’ve joined Anthropic","url":"https://twitter.com/karpathy/status/2056753169888334312"},
  {"title":"I've built a virtual museum with nearly every operating system you can think of","url":"https://virtualosmuseum.org/"},
  {"title":"Gaussian Splat of a Strawberry","url":"https://superspl.at/scene/84df8849"},
  {"title":"Gentoo News: Copy Fail, Dirty Frag, and Fragnesia Kernel Vulnerabilities","url":"https://www.gentoo.org/news/2026/05/19/copy-fail-fragnesia-vulnerabilities.html"}
]
```

**Findings:**
- The static-HTML extraction path is fast and reliable — HN renders server-side, no JS needed.
- **Bug to file:** `HTMLAnchorElement.href` getter does NOT serialize back to a string when read in JS. Demonstrated by the fact that `a.hasAttribute('href') === true` and `a.tagName === "A"` both held, but `a.href` returned a falsy value. Workaround was trivial (`getAttribute('href')`) but this is going to confuse anyone porting a Playwright snippet.
- The skipped-external-script warning (`hn.js`) was helpful — it correctly identifies what `--js-fetch` would unlock.

---

## Task 2 — Search Wikipedia and follow first result

**Goal:** From `Special:Search?search=anthropic`, find the article and extract a summary.

**Verb sequence tried:** Single `heso eval-dom` on the search URL.

**Result:** **ok** — single-call shape worked perfectly.

Wikipedia redirects `Special:Search?search=anthropic` server-side (302) to `https://en.wikipedia.org/wiki/Anthropic`. The heso fetch engine followed the redirect transparently — `location.href` already reflected the final URL when JS ran. The two-call shape (find first result link, then fetch) was tested as a sanity check and the search-result CSS selector returned `null` because the page is the article, not a search results listing.

**Output (single call):**

```json
{
  "title": "Anthropic - Wikipedia",
  "url": "https://en.wikipedia.org/wiki/Anthropic",
  "firstP_via_first_p": "\n\n",
  "firstNonEmptyParagraph_count": 29,
  "summary": "Anthropic is an American artificial intelligence (AI) company headquartered in San Francisco. It has developed a range of large language models (LLMs) named Claude and focuses on AI safety.[7] Anthropic was founded in 2021 by former members of OpenAI, including siblings Daniela Amodei and Dario Amodei, who are president and CEO, respectively.[8] The company is privately held and as of February 2026[update] had an estimated value of $380 billion.[9]"
}
```

**Findings:**
- Redirects "just work" — agent doesn't need to manually re-issue against the canonical URL.
- A subtle gotcha: `document.querySelector('#mw-content-text p')` returns Wikipedia's layout-sentinel `<p>` (which is empty `"\n\n"`). Agents need `p.textContent.trim().length > 50` style filtering — same as on real browsers, but worth noting.
- Wikipedia's `RLCONF is not defined` console errors fired because the inline scripts reference globals that come from the external `load.php` script (which we skipped). The errors didn't block the extraction — heso's "JS errors don't poison the DOM" semantic worked.
- Found `executed_with_error: 3` in the scripts report — that's the right level of observability for an agent debugging a flaky extraction.

---

## Task 3 — Fill out and submit a real form

**Goal:** POST to `httpbin.org/forms/post` with `custname="test user"` and `delivery="10:00"`, read back the echoed `custname`.

**Verb sequence tried:**
1. `heso find https://httpbin.org/forms/post` → mapped @e1 = custname input, @e11 = delivery (note: form sets `min="11:00"`, we used the task-specified "10:00"), @e0 = the form.
2. `heso fill https://httpbin.org/forms/post @e1 "test user"` → returned `{ok: true, value: true}` BUT this is stateless — the next CLI call re-fetches the page with no preserved value.
3. `heso submit https://httpbin.org/forms/post @e0` → returned `{ok: true, value: true}` but does NOT issue a real POST.
4. Tried `heso serve` JSON-RPC mode → its `ready` message advertises only `methods: ["open","ls","cat","find","close","ping"]`. No `fill`, no `submit`. So statefulness via JSON-RPC does not exist for write verbs.
5. Tried `eval-dom --js-fetch` with `await`/promise → fetch global exists, but the JS engine returns the script's last expression synchronously without draining the QuickJS job queue. A returned Promise serializes as `{}`. A `globalThis.__r = j` assignment from inside `.then()` was never observed because the .then callback hadn't run by the time the script returned.
6. Tried `heso action-hash + heso replay` → it does run the steps statefully in one `JsSession`, but the replay note in its own output reads:

> "Submit still has the no-real-POST limitation: JsSession::submit dispatches a click on the form's submit button rather than issuing an HTTP POST."

**Result:** **broken** — no path through the CLI can complete this task today.

**Exact CLI errors (none — every step returned `ok: true`).** The bug is "looks like it worked but didn't actually do anything network-visible."

**Findings (these are the showstoppers for write-side workloads):**
- **Showstopper #1:** `heso submit` is a no-op as far as the network is concerned. It dispatches a click on the submit button but doesn't serialize the form and POST it. Documented in the replay output's own `note` field. For an agent that needs to *do* things on the web, this is the single biggest gap.
- **Showstopper #2:** `eval-dom` does not pump the microtask / macrotask queue after the script returns. Even with `--js-fetch` enabled, you can't `await` anything because the runtime hands control back to Rust the instant the top-level expression evaluates. Returned Promises serialize to `{}`. Returning a `.then`-chain returns a Promise, not its resolved value.
- **Showstopper #3:** `heso serve` has no write verbs — its method list is read-only. So stateful CLI composition (the "do it across multiple CLI calls" workflow) is structurally impossible for writes.
- The `find` verb did its job perfectly — the action graph correctly identified every field, including `min="11:00"` on the time input (which is metadata an agent would want for input validation).

**Verbs to file bugs/feature-requests against:** `heso submit` (issue HTTP POST), `heso eval-dom` (drain pending jobs before returning), `heso serve` (add fill/submit RPC methods).

---

## Task 4 — Extract Stripe pricing tier

**Goal:** First standard pricing tier name + price from `stripe.com/pricing`.

**Verb sequence tried:** Plain `heso eval-dom https://stripe.com/pricing "<js>"` — no `--js-fetch`.

**Result:** **ok** — worked on the first try without `--js-fetch`.

**Output:**

```json
{"tier":"Standard","price":"2.9% + CA$0.30","finalUrl":"https://stripe.com/en-ca/pricing"}
```

**Findings:**
- Stripe's Next.js page is SSR-thorough enough that the first pricing tier ("Standard", `2.9% + CA$0.30`) is in the static HTML. Did not need `--js-fetch`.
- Geo-redirected to `/en-ca/pricing` because the test ran from Canada — heso followed that redirect transparently. Worth flagging that an agent doing competitive intel will get geo-localized prices unless we add header injection.
- One console error: `cannot read property 'mountTarget' of undefined` from an inline analytics script. Didn't affect the extraction — same "JS errors don't poison the DOM" pattern as Wikipedia. Good.
- The first time, the task description said "Starter" but the actual first tier on the live page is "Standard". I report what the page said, not what the task expected. (This is the kind of thing the prompt warned: "Don't make up plausible-looking values.")

---

## Task 5 — SPA navigation via history.pushState

**Goal:** Open react.dev, pushState to /learn, dispatch popstate, read new H1.

**Verb sequence tried:** Single `heso eval-dom` with a self-contained IIFE.

**Result:** **partial.** `history.pushState` and `popstate` both work; the H1 does not change.

**Output:**

```json
{"before":"https://react.dev/","afterPush":"https://react.dev/learn","popstateFired":true,"h1":"React"}
```

**Findings:**
- `history.pushState({}, '', 'https://react.dev/learn')` correctly mutated `location.href` to `https://react.dev/learn`. Good.
- `window.addEventListener('popstate', ...)` + `window.dispatchEvent(new Event('popstate'))` correctly fired the listener. `popstateFired: true`. Good.
- The H1 stays `"React"` (the static-HTML H1 of `/`) because heso does not refetch on SPA navigation — and the React router is not running because its full bundle (with router code) is in `<script src="…/_next/…">` files that heso skips without `--js-fetch`.
- This is honest and correct behavior — an SPA's "navigation" is in JS, not in HTTP. But the task implicitly expected "and now read the /learn page's content" which requires either (a) the React bundle running fully (needs `--js-fetch` + microtask draining + framework support that probably won't work) or (b) the agent re-issuing `heso eval-dom https://react.dev/learn` after the pushState. The pushState verb here is more of a "I want to fire the right event" primitive, not a "navigate me to the new content" primitive.
- The pure-DOM bits (`history.pushState`, `Event` constructor, `addEventListener`, `dispatchEvent`) all work cleanly. That's the recently-shipped global validated.

---

## Task 6 (free-form) — Latest `tokio` crate version from docs.rs

**Goal:** Get the latest published version of the `tokio` crate.

**Why I picked this:** A real-world agent task I'd actually want — checking dependency staleness. Tests heso against a Rust-ecosystem documentation site with a stable predictable layout.

**Verb sequence tried:** `heso eval-dom https://docs.rs/tokio/latest/tokio/ "<js>"` — extract the "Permalink" anchor's href, which on docs.rs resolves `/latest/` to the actual versioned URL.

**Result:** **ok** — got the answer cleanly: tokio latest is **1.52.3**.

**Output:**

```json
{
  "title": "tokio - Rust",
  "finalUrl": "https://docs.rs/tokio/latest/tokio/",
  "sampleVersionLinks": [{"text": "Permalink", "href": "/tokio/1.52.3/tokio/"}]
}
```

**Findings:**
- docs.rs has the latest version embedded as a "Permalink" anchor — heso extracted it in one call.
- `heso meta` also returned useful structured metadata (`description: "A runtime for writing reliable network applications without compromising speed."`, `generator: "rustdoc"`) which is the kind of thing a "Rust crate librarian" agent would actually want to index.
- The skipped-external-script warning is a minor noise channel but didn't block the task.

---

## Summary

### Counts

- **Attempted:** 6
- **Completed cleanly:** 3 (Tasks 2, 4, 6)
- **Completed with workaround:** 1 (Task 1 — `getAttribute('href')` instead of `.href`)
- **Partial:** 1 (Task 5 — pushState/popstate work, but no content swap because heso doesn't run framework JS)
- **Broken / blocked:** 1 (Task 3 — no real POST possible)

### Top 3 bugs / gaps that blocked workflows

1. **`heso submit` and `JsSession::submit` do not issue HTTP POSTs.** They dispatch click events instead. This silently breaks every "agent fills a form and reads the response" workload. The replay note already calls this out internally — make it a tracked task and fix it. Without this, heso cannot complete real transactional workflows.
2. **`eval-dom` returns synchronously without draining the QuickJS job queue.** Any `async`/`await` or `fetch().then()` workflow returns `{}` because the promise hasn't resolved. With `--js-fetch` already shipping, this is the next logical step. Probably needs a `--await` flag or an automatic "drain until job queue is empty or timeout" pump.
3. **`HTMLAnchorElement.href` does not return the resolved URL string.** `getAttribute('href')` works as a workaround, but every Playwright/puppeteer migration is going to trip over this. Probably a one-line implementation gap in the engine's anchor element getter.

### Other smaller bugs to keep on the list

- `FormData` is undefined (URLSearchParams works) — limits multipart form construction even after we fix the submit POST. (Task 3, in passing.)
- `heso serve` advertises a method list that is read-only (`open/ls/cat/find/close/ping`) — no parity with the write verbs (`fill`, `submit`, `click`). Closing this gap is a precondition for any framework integration that wants long-lived sessions.

### What worked surprisingly well

- **Static-HTML extraction is rock solid.** Tasks 1, 2, 4, 6 all worked off SSR'd HTML without `--js-fetch`. Modern Next.js / SSR sites have moved most agent-relevant content server-side, so heso's "fetch-only-native-engine" bet (ADR 0012) is paying off. Faster and more deterministic than spinning Chrome.
- **Redirect handling is invisible-and-correct.** Wikipedia's 302 to the canonical article and Stripe's geo-redirect both landed in `location.href` with no extra agent work.
- **`history.pushState` + `popstate` work cleanly** for the parts they're supposed to cover. The recently-shipped globals are solid.
- **The `find` action graph is well-designed.** It exposes input constraints (`min="11:00"` on the time input, `type="email"` on email fields, name attributes) in a single JSON blob. That's the kind of structured affordance Playwright doesn't give you for free.
- **The "exit non-zero on JS error" + "console array in output" pattern** is a great agent-debugging UX. Wikipedia threw 3 errors and Stripe threw 1; the extractions still succeeded; the errors were visible in case I needed to debug.
- **`heso meta` and `heso find` are pure agent-shaped APIs.** Nothing in Playwright is shaped like these — they only exist because heso threw out the "render for humans" muscle.

### Subjective: would I, as an agent, want to use heso over Playwright?

**For read-shaped workloads** (RAG ingest, competitive intel, dependency librarianship, content summarization): **yes, today.** It's faster, smaller, more deterministic, the action graph is friendlier than Playwright selectors, and the SSR-shape of the modern web makes the "no real renderer" bet pay off. Tasks 1, 2, 4, 6 were all faster to write and faster to run than the equivalent Playwright would be.

**For write-shaped workloads** (form fills that need to actually submit, multi-step transactions, anything that needs to wait on async): **no, not yet.** Task 3 is the canonical example — heso has all the verbs in name but `submit` doesn't actually submit. Until the three top bugs above are fixed (real POST, job-queue drain, anchor.href resolution), an agent doing actual transactions will hit the wall almost immediately. The good news: all three are well-scoped fixes, none of them require architectural changes.

Net-net: heso is delivering on the "browser for the agent-relevant half of the web" pitch for the read half. The write half needs the next milestone before I'd ship a write-shaped workload on it.
