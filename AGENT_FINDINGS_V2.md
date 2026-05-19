# heso real-agent-workload findings — round 2 (post-fix verification + harder workloads)

Generated 2026-05-19 by the second agent test run. heso commit: `99d61ce`.

Binary: `C:\Users\Akshay\Documents\projects\heso\target\release\heso.exe` (release build, 1m 05s, clean).

All output below is verbatim from the binary. Outputs are trimmed where the noise is the same line repeated.

---

## Tier 1 — Regression confirmation

### Task R1 — HN with `a.href`

**Verb sequence:**
```
heso eval-dom https://news.ycombinator.com "JSON.stringify(Array.from(document.querySelectorAll('.titleline > a')).slice(0,3).map(a => ({title: a.textContent, url: a.href})))"
```

**Result:** **FIXED.** Every URL is a non-empty absolute string starting with `http`.

**Output (verbatim, value field):**
```json
[
  {"title":"I've built a virtual museum with nearly every operating system you can think of","url":"https://virtualosmuseum.org/"},
  {"title":"I’ve joined Anthropic","url":"https://twitter.com/karpathy/status/2056753169888334312"},
  {"title":"Apple unveils new accessibility features","url":"https://www.apple.com/newsroom/2026/05/apple-unveils-new-accessibility-features-and-updates-with-apple-intelligence/"}
]
```

Note: only `external_handled` 1 (the external `hn.js`); no errors. `a.href` is a real resolved absolute URL.

---

### Task R2 — httpbin form submit

**Verb sequence:**
```
heso find   https://httpbin.org/forms/post
heso fill   https://httpbin.org/forms/post @e1 "agent v2"
heso submit https://httpbin.org/forms/post @e0
```

**Result:** **PARTIALLY FIXED.** The POST goes out (HTTP 200 confirmed against httpbin), but two things prevent this from being a useful end-to-end roundtrip:

1. **`fill` does not persist across verb calls.** Each verb is its own process; the second verb re-fetches the page and starts from a clean DOM. So step 2's value never reaches step 3's submission. The submit body is the form's *default* values (all empty for httpbin's pizza form).
2. **The submit response body is not returned.** The output gives `responseStatus`, `responseUrl`, `method`, `enctype`, etc. — but no `body` field, no echo. You can't `heso eval-dom` the `postUrl` either because httpbin's `/post` returns `405 Method Not Allowed` on the subsequent GET.

**Output (verbatim, all three steps trimmed to the submit):**
```json
{
  "ok": true,
  "op": "submit",
  "postUrl": "https://httpbin.org/post",
  "value": {
    "action": "/post",
    "defaultPrevented": false,
    "enctype": "application/x-www-form-urlencoded",
    "matched": true,
    "method": "POST",
    "responseStatus": 200,
    "responseUrl": "https://httpbin.org/post",
    "submitted": true
  }
}
```

**Workaround that actually works (from H2 below):** issue the POST inside a single `eval-dom --js-fetch` using the JS `fetch` global. The whole roundtrip (`fetch → json → assert echo`) executes in one session.

**Verdict:** the *POST machinery* is fixed (it really does serialize and POST per enctype). The agent UX — fill, submit, observe — is not, because of statelessness across verbs and the missing response body.

---

### Task R3 — Nested-promise resolve

**Verb sequence:**
```
heso eval-dom --js-fetch https://example.com "Promise.all([fetch('https://httpbin.org/get?ping=a'), fetch('https://httpbin.org/get?ping=b')]).then(rs => Promise.all(rs.map(r => r.json()))).then(js => js.map(j => j.args.ping))"
```

**Result:** **FIXED.** Returns `["a","b"]`. Also verified the un-mapped form returns full Response-shaped objects with real headers / status / url. Two-level promise nesting with Promise.all both inside and outside is correctly deep-resolved.

**Output (verbatim):**
```json
{"value": ["a", "b"]}
```

---

## Tier 2 — Harder workloads

### Task H1 — GET-method form submit

**Approach:** picked the DuckDuckGo lite form, but it is actually POST (action `/html/`, method `post`). Wikipedia's `#searchform` has no explicit `method` so defaults to GET, but the same fill-doesn't-persist problem from R2 kills the verb path. Fell back to the agent-realistic shape: directly constructing the GET URL.

```
heso eval-dom "https://html.duckduckgo.com/html/?q=anthropic" "JSON.stringify(Array.from(document.querySelectorAll('a.result__a')).slice(0,3).map(a => ({title: a.textContent.trim(), url: a.href})))"
```

**Output (value field, trimmed):**
```json
[
  {"title":"Home \\ Anthropic","url":"https://duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.anthropic.com%2F&..."},
  {"title":"Anthropic - Wikipedia","url":"https://duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FAnthropic&..."},
  {"title":"The AI for Problem Solvers | Claude by Anthropic","url":"https://duckduckgo.com/l/?uddg=https%3A%2F%2Fclaude.com%2Fproduct%2Foverview&..."}
]
```

**Result:** worked once I bypassed the verbs. **Friction:** an honest agent that finds a `<form method=GET>` via `heso find` will hit the same fill-doesn't-persist wall as POST. The agent has to know that GET forms become URL queries and synthesize the URL themselves. No verb path goes from "form discovered" → "query submitted with my values."

---

### Task H2 — Two-hop navigation: submit then read response

**Realistic agent approach** (one `eval-dom --js-fetch` call):
```js
(async () => {
  const r = await fetch('https://httpbin.org/post', {
    method: 'POST',
    headers: {'Content-Type': 'application/x-www-form-urlencoded'},
    body: 'custname=agent+v2&size=large&topping=cheese'
  });
  const j = await r.json();
  return {form: j.form, url: j.url};
})()
```

**Output (value field, verbatim):**
```json
{
  "form": {"custname": "agent v2", "size": "large", "topping": "cheese"},
  "url": "https://httpbin.org/post"
}
```

**Result:** **WORKS — but only via raw fetch, not via the heso submit verb.** The roundtrip is clean: POST → parse JSON → values echoed back. The friction is the same as H1: the verb composition that the README implies (find → fill → submit → eval-dom on postUrl) is not actually viable today. Real agents will end up writing JS to do form POSTs themselves.

**Tried-but-failed alternative paths** (documented for completeness):
- `form.submit()` in JS — throws `TypeError: not a function`.
- `new FormData(form)` — `FormData is not defined`.
- `heso eval-dom https://httpbin.org/post` after a submit — `405 Method Not Allowed` because heso re-fetches via GET.

---

### Task H3 — Parallel fetch + extract (3 GitHub repos)

```
heso eval-dom --js-fetch https://example.com "Promise.all(['anthropics/anthropic-sdk-python', 'rust-lang/rust', 'microsoft/playwright'].map(r => fetch('https://api.github.com/repos/' + r).then(res => res.json()))).then(arr => arr.map(j => ({name: j.full_name, stars: j.stargazers_count})))"
```

**Output (value, verbatim):**
```json
[
  {"name":"anthropics/anthropic-sdk-python","stars":3490},
  {"name":"rust-lang/rust","stars":112914},
  {"name":"microsoft/playwright","stars":89013}
]
```

**Result:** **WORKS PERFECTLY.** Three parallel fetches, JSON parse, deep-resolve, return shape — all in one call, no errors, no skipped scripts. This is the *crown jewel* path for agent workflows post-fix. Time-to-result was sub-second.

---

### Task H4 — URL-decomposition mixin

```
heso eval-dom https://news.ycombinator.com "(() => { const links = Array.from(document.querySelectorAll('a')).slice(0,30); const hosts = links.map(a => a.hostname).filter(h => h); const unique = [...new Set(hosts)]; return {totalLinks: links.length, uniqueHostCount: unique.length, hosts: unique, sampleProtocols: links.slice(0,5).map(a => ({proto: a.protocol, host: a.hostname, path: a.pathname}))}; })()"
```

**Output (value, verbatim):**
```json
{
  "hosts": ["news.ycombinator.com","virtualosmuseum.org","twitter.com","www.apple.com"],
  "sampleProtocols": [
    {"host":"news.ycombinator.com","path":"/","proto":"https:"},
    {"host":"news.ycombinator.com","path":"/news","proto":"https:"},
    {"host":"news.ycombinator.com","path":"/newest","proto":"https:"},
    {"host":"news.ycombinator.com","path":"/front","proto":"https:"},
    {"host":"news.ycombinator.com","path":"/newcomments","proto":"https:"}
  ],
  "totalLinks": 30,
  "uniqueHostCount": 4
}
```

**Result:** **WORKS.** `a.hostname`, `a.protocol`, `a.pathname` all return correct decomposed values. (The first 30 anchors on HN are mostly self-referential — that's just HN's structure, not a heso bug.)

---

### Task H5 — Multi-step Wikipedia exploration

**Step 1** (`heso open https://en.wikipedia.org/wiki/Anthropic`) — returned a 600+ action graph. Found a Claude link.

**Step 2** (`heso find ... --role link --name Claude`) — 32 matches. First high-quality hit: `@e153 → /wiki/Claude_(language_model)`.

**Step 3** (`heso eval-dom https://en.wikipedia.org/wiki/Claude_(language_model)` with a paragraph-extraction script).

First selector `#mw-content-text .mw-parser-output > p` returned empty (the `>` direct-child selector and Wikipedia's hatnote markup interacted poorly — I'm not sure if heso's selector engine handles `>` here; both Real Browser and a fallback `Array.from(document.querySelectorAll('p')).find(...)` are needed).

Fallback worked:
```
(() => { const ps = Array.from(document.querySelectorAll('p')); const firstReal = ps.find(p => p.textContent.trim().length > 100); return firstReal ? firstReal.textContent.trim().slice(0, 500) : 'no paragraph'; })()
```

**Output (value, verbatim):**
> "Claude is a series of large language models developed by American software company Anthropic. Claude was released as a AI chatbot in March 2023. It is also used in AI-assisted software development."

**Result:** **WORKS.** The 3-step open → find → eval-dom flow is the realistic agent shape and it composes cleanly because nothing needs to *persist DOM mutations* between steps. The mild friction is selector-engine flakiness (`> p` direct child) — I'd want to know whether that's a real selector bug or a Wikipedia-DOM peculiarity. Worth a follow-up regression.

There were also 3 hydration errors logged from Wikipedia inline scripts (`RLQ`, `RLCONF` not defined). They are noise — the extraction succeeded anyway.

---

### Task H6 (free-form) — Latest Simon Willison blog post

**Approach 1 (live page):**
```
heso eval-dom https://simonwillison.net/ "(() => { const all = Array.from(document.querySelectorAll('h3 a')); return all.slice(0, 5).map(a => ({text: a.textContent.trim(), href: a.href})); })()"
```

**Output (value, verbatim):**
```json
[{"text":"The last six months in LLMs in five minutes","href":"https://simonwillison.net/2026/May/19/5-minute-llms/"}]
```

**Approach 2 (GitHub API, "does rust-lang/rust have CONTRIBUTING.md?"):**
```
heso eval-dom --js-fetch https://example.com "(async () => { const r = await fetch('https://api.github.com/repos/rust-lang/rust/contents/CONTRIBUTING.md'); const j = await r.json(); return {name: j.name, size: j.size, has_contributing: !!j.name}; })()"
```

**Output (value, verbatim):**
```json
{"has_contributing": true, "name": "CONTRIBUTING.md", "size": 2712}
```

**Why I picked these:** "Does this repo have docs X?" and "what is this person's latest post?" are two of the single most common agent-driven workflows. Both work in one heso call with zero fuss. This is a genuinely impressive demo path now that R3 is fixed.

---

## Tier 3 — Failure mode hunting

### Task F1 — File upload form

**Approach:** no real-world file-upload form on httpbin's normal endpoints, so I tried two synthetic paths:

1. **Construct a form via `document.body.innerHTML` then `heso fill @ref` it** — can't; `heso fill` doesn't persist across verb invocations, and there's no verb to set a file input's value programmatically in any session-friendly way.

2. **Use `FormData` + `Blob` in `eval-dom --js-fetch`:**
   ```
   heso eval-dom --js-fetch https://example.com "(async () => { const fd = new FormData(); fd.append('upload', new Blob(['hi from heso'], {type: 'text/plain'}), 'hi.txt'); ... })()"
   ```

**Output:**
```json
{"error": "ReferenceError: FormData is not defined"}
```

**Result:** **CONFIRMED BROKEN.** Three separate paths to file upload are all dead:
- `heso submit` over a form with `<input type=file>` — per source docs (`form_submit.rs` line 32) sends filename only, no body. Real servers reject this.
- `FormData` global — undefined.
- `Blob`, `File`, `Headers` globals — all undefined.

This is the single biggest missing API surface for "agent does anything with file content." Anything that wants to upload a CSV / image / audio sample is unreachable today.

---

### Task F2 — Simulated login flow

**Verb sequence:** open → find custname (@e1) → fill → find custtel (@e2) → fill → find submit button (@e13) → submit form (@e0).

Every individual verb returned `ok: true`. But the actual semantics:
- After `heso fill ... @e1 alice`: the response says `value: true`, but the next process invocation re-fetches the page; `alice` is gone.
- After `heso fill ... @e2 secret-pw`: same.
- The final `heso submit @e0`: POSTed an empty body, returned `responseStatus: 200`.

**Result:** **the verb path is theatrical.** Every verb claims success, but real form data never makes it to the server. An LLM agent reading the output of each call would honestly believe the login worked.

This is the highest-priority footgun I can see. Either:
- the fill verb should error/warn that it has no session-persisting effect, or
- the submit verb should accept a `--field NAME=VALUE` map (or take a JSON body) so an agent in one shot can submit a form with explicit values, or
- there should be a stateful session (the `heso serve` path) that supports fill+submit (currently only `open/ls/cat/find/close/ping`).

For now, the "real" login-flow shape for an agent is the H2/H6 pattern: one `eval-dom --js-fetch` call that constructs the body and POSTs via raw fetch. The verbs are misleading here.

---

### Task F3 — MutationObserver / framework sites

**nextjs.org:**
- Scripts: 24 executed, **49 with error**, 24 external_handled, 0 skipped non-script.
- Globals: `MutationObserver: function`, `IntersectionObserver: function`, `ResizeObserver: function`, `window: object`, `document: object`, **`self: undefined`**.
- Title extracted: `"Next.js by Vercel - The React Framework"`.
- Most common error: `self is not defined`. Other observers exist but framework hydration code is unreachable.

**react.dev:**
- 15 scripts in document, 3 executed, **11 errored**, 11 external_handled, 1 skipped non-script.
- Title extracted: `"React"`.
- Note: `document.scripts` (the HTMLCollection accessor) is `undefined` — verified separately. Same for `document.forms`, `document.images`, `document.links`. Only `document.querySelectorAll('script')` works.

**vuejs.org:**
- 8 scripts in document, 6 executed, **2 errored**, 3 external_handled.
- Title extracted: `"Vue.js - The Progressive JavaScript Framework | Vue.jsPlay icon"` (the trailing `"Play icon"` smells like a `<title>` with embedded markup or alt text bleeding in — minor extraction quirk).
- Vue.js is the friendliest of the three by a wide margin (75% script success vs. ~50% for Next, ~21% for React).

**Result:** **MutationObserver itself works** (it's a function), but the *real* observer-fueled init paths on React/Next.js fail upstream because of missing `self` (the WindowOrWorkerGlobalScope alias). For SPAs, heso reaches DOM content but not hydrated state. For SSR-only paths (Vue.js docs site appears mostly SSR), it's fine.

---

## Bonus findings (not asked for, worth noting)

While reading session source, I bumped into and confirmed:

### Missing IDL properties on `<form>`
`form.method`, `form.action`, `form.name`, `form.enctype` — all return `undefined` (only `getAttribute('...')` works). This is HTMLFormElement's basic IDL surface; sibling to the `.href` mixin you just fixed. An agent that wants to inspect a form before submitting it gets a much worse story than for `<a>` post-fix.

Output (verbatim):
```json
{"hasActionAttr": "/w/index.php", "formMethodType": "undefined", "formActionType": "undefined"}
```

### Missing collection accessors
`document.scripts`, `document.forms`, `document.images`, `document.links` — all `undefined`. The `forms` one is particularly relevant for agent flows.

### Missing web platform globals (relevant subset)
- `FormData` — undefined.
- `Blob` — undefined.
- `File` — undefined.
- `Headers` — undefined (Response works but `new Headers(...)` doesn't).
- `URL`, `URLSearchParams` — both work.

### `heso serve` is too thin for stateful workflows
Currently exposes: `open`, `ls`, `cat`, `find`, `close`, `ping`. No `fill`, `submit`, `click`, or `eval` in serve mode — which means even when an agent wants a session, they can't get one for the operations that need it. Adding fill/submit/click/eval-dom to serve mode would solve R2/H1/H2/F2 in one go (with persistence inside the session).

### JSON.stringify swallow with undefined values
When a key's value is `undefined`, the serialized output drops the key entirely (standard JSON behavior — this is correct, but means agents reading heso's output can't distinguish "key missing" from "key was undefined"). Use `typeof` or `JSON.stringify` inside the script to make missing-vs-undefined visible. Documented because it bit me twice during this run.

---

## Summary

- **Regressions confirmed fixed: 2/3 cleanly, 1/3 partially.** R1 (`a.href`) and R3 (nested-promise) are unambiguously fixed. R2 (form submit) is fixed at the protocol layer (real POST goes out, real status returned) but unusable end-to-end because fill doesn't persist across verb calls and the response body isn't returned.
- **Harder workloads attempted: 6. Completed cleanly: 4** (H3 parallel fetch, H4 URL-decomposition mixin, H5 multi-step Wikipedia, H6 free-form). **2 with significant friction** (H1, H2 — both worked only after bypassing the verbs and using raw fetch in JS).
- **Failure modes hunted: 3. Real bugs found: 3.**
  - F1: file upload completely unreachable (no `FormData`/`Blob`/`File`, and verb path sends filename-only).
  - F2: simulated login is theatrical — every verb succeeds, no real data submitted.
  - F3: SPA hydration is mostly broken (`self is not defined` cascade across React/Next), though basic DOM/title extraction works.

### Top NEW bugs / gaps that blocked workloads (next-batch priorities, ranked)

1. **`fill` value does not survive across verb invocations** — the single biggest gap, makes the find/fill/submit story untrue. Either persist via a session ID, expand `heso serve` to include fill/submit, or refactor submit to take `--field name=value` overrides.
2. **`heso submit` does not return the response body** — agents can't observe what they just submitted. Add `body`/`text`/`json` to the submit output (or expose the response page as a subsequent eval-dom-friendly URL).
3. **`HTMLFormElement` IDL properties missing** (`form.method`, `form.action`, etc.) — sibling fix to the `.href` work in this batch.
4. **`FormData` / `Blob` / `File` / `Headers` globals missing** — blocks all file upload and most modern fetch patterns.
5. **`self` global missing in eval-dom** — silently breaks the inline-hydration scripts on every major framework site (React, Next.js, lots more downstream).
6. **`document.scripts` / `.forms` / `.images` / `.links` HTMLCollection accessors missing** — small, but common idiom in scraping code.
7. **`form.submit()` JS method not implemented** — agents that try the standard JS submission path get `TypeError: not a function`.

### Things that worked SURPRISINGLY well now

- **H3 parallel fetch + JSON parse + array map** — one `eval-dom --js-fetch` call to three GitHub API endpoints returned a clean 3-row array in sub-second time. This was *impossible* before R3 was fixed. This is the most agent-shaped workflow in the whole test and it's genuinely strong now.
- **`heso meta`** — first-class Open Graph / Twitter / JSON-LD extraction. Tried on `anthropic.com/news` and got a clean structured object. Underrated for "what is this site?" queries.
- **`heso find --role X --name regex`** filter combo — produces a tight action graph fast. Used it on Wikipedia/Anthropic to find Claude links in one call; 32 matches, all sensibly typed.
- **The deep-resolve in eval-dom** — once it's there, every async pattern I tried (parallel, nested, `.then` chained, `await`-ed) "just worked." This is the highest-leverage fix in the batch.

### Subjective: how close is heso to "I would use this for real agent workloads"?

**For read-only scraping / extraction workloads:** very close. R1 + R3 + URL-decomposition + meta extraction + multi-step open/find/eval-dom together cover the bulk of "given a URL, get me X" agent tasks. I would use heso for this today.

**For interactive / transactional workloads (login, search, file upload, multi-page form flows):** still not there. The verb-level statelessness + missing FormData/Blob + missing `form.method`/`.action` + bare `heso serve` together mean the *only* path to interactive workloads is "write raw JS that does fetch() with hand-built bodies in `eval-dom`." That's actually a usable path — H2 and H6 prove it — but the *advertised* verb path (find → fill → submit) is misleading. An agent that trusts the verbs will silently fail.

**Concrete next 1-week priority I'd push for:** make fill+submit work in one verb shot. The simplest path: extend `heso submit` to accept `--field name=value` repeated args (or a `--data <json>` flag) so a single call can supply form values and get the response back. That alone unblocks F2, H1, H2, and the R2 end-to-end roundtrip.
