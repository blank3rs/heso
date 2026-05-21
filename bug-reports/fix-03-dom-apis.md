# fix-03-dom-apis — top 6 DOM/Web-API gaps closed

Branch: `agent-a26494d27998751e6-dom-gaps`
Worktree: `C:\Users\Akshay\Documents\projects\heso\.claude\worktrees\agent-a26494d27998751e6`
Commits (in order):

| SHA | Title | Files | LoC |
|---|---|---|---|
| `b3e9aed` | engine-js: getElementsByClassName + getElementsByName | `crates/heso-engine-js/src/dom.rs`, `crates/heso-engine-js/tests/dom_collection_apis.rs` | +342 |
| `45391f5` | engine-js: performance.mark / measure / clearMarks / clearMeasures | `crates/heso-engine-js/src/engine.rs`, `crates/heso-engine-js/tests/performance_user_timing.rs` | +356 |
| `bd9770f` | engine-js: element.style = "cssText" string-coercion setter | `crates/heso-engine-js/src/dom.rs`, `crates/heso-engine-js/tests/style_setter.rs` | +168 |
| `a6fadf3` | engine-js: data: URL fast path in `<script src>` loader | `crates/heso-engine-js/src/fetch.rs`, `crates/heso-engine-js/src/scripts.rs`, `crates/heso-engine-js/tests/data_url_scripts.rs` | +146 / −5 |
| `cf2c052` | engine-js: HTML*Element subclass family + NodeList + HTMLCollection | `crates/heso-engine-js/src/custom_elements.rs`, `crates/heso-engine-js/tests/html_element_subclasses.rs` | +443 |
| `6c3d6da` | engine-js: XMLHttpRequest backed by the shared reqwest::Client | `crates/heso-engine-js/src/xhr.rs` (new), `crates/heso-engine-js/src/lib.rs`, `crates/heso-engine-js/src/engine.rs`, `crates/heso-engine-js/tests/xhr_integration.rs` | +1181 / −9 |
| `edbf57d` | engine-js: currentScript shim carries localName so HTMLScriptElement instanceof passes | `crates/heso-engine-js/src/scripts.rs` | +11 |

## Per-API

### 1. XMLHttpRequest — `crates/heso-engine-js/src/xhr.rs` (new, 690 LoC) + wiring in `engine.rs` / `lib.rs`

- `#[rquickjs::class] XmlHttpRequest` registers the constructor; a JS-level wrapper (`Reflect.construct` + `initState`) pre-populates the spec-default IDL fields so `new XMLHttpRequest()` has `readyState = 0`, `status = 0`, `on*` = `null`, etc.
- JS bootstrap patches the prototype with `open`, `setRequestHeader`, `send`, `abort`, `getResponseHeader`, `getAllResponseHeaders`, `overrideMimeType`, `addEventListener`/`removeEventListener`/`dispatchEvent`, plus the readyState constants on both the constructor and the prototype.
- `xhr.send()` queues a `PendingXhr` onto a per-engine `XhrQueue` parallel to `FetchQueue`. `JsEngine::run_pending_jobs` drains it alongside fetch — same `reqwest::Client`, so cookies / UA / cassette behavior matches.
- readyState transitions: UNSENT → OPENED at `open()`; HEADERS_RECEIVED, LOADING, DONE fire from the drain step, each followed by `readystatechange`. `onload` fires on success, `onerror` on network failure, `onloadend` always at the end.
- `responseType`: `''` / `'text'` route to `responseText` (UTF-8); `'json'` parses via JS-native `JSON.parse`; `'arraybuffer'` returns the underlying ArrayBuffer.
- `data:` URLs supported via the shared `parse_data_url` helper.
- `DeterministicNoCassette` mode rejects synchronously, same gate as fetch (ADR 0008).
- Tests: `tests/xhr_integration.rs` — 14 tests covering constructor, readyState constants, end-to-end GET + onload, onreadystatechange transitions, responseText decoding, responseType=json, POST with body + headers, 404 fires onload (not onerror), network error fires onerror, response header reads (case-insensitive), and the vercel-shaped `XMLHttpRequest.prototype.send = function(){...}` monkey-patch path. **All 14 pass.**

### 2. HTMLElement constructor family — `crates/heso-engine-js/src/custom_elements.rs` (+170 LoC)

Adds the HTML spec's per-element interface family to `globalThis`. Each constructor:
- Throws `TypeError: Illegal constructor` on direct `new` (`makeIllegalConstructor`).
- Shares `Element.prototype` so `instanceof` walks succeed.
- Overrides `Symbol.hasInstance` to discriminate by `localName.toLowerCase()` against the spec's per-element mapping.

Interfaces shipped: `HTMLDivElement`, `HTMLSpanElement`, `HTMLAnchorElement`, `HTMLAreaElement`, `HTMLButtonElement`, `HTMLInputElement`, `HTMLTextAreaElement`, `HTMLSelectElement`, `HTMLOptionElement`, `HTMLOptGroupElement`, `HTMLLabelElement`, `HTMLFormElement`, `HTMLFieldSetElement`, `HTMLLegendElement`, `HTMLOutputElement`, `HTMLProgressElement`, `HTMLMeterElement`, `HTMLDataListElement`, `HTMLImageElement`, `HTMLPictureElement`, `HTMLSourceElement`, `HTMLVideoElement`, `HTMLAudioElement`, `HTMLMediaElement`, `HTMLCanvasElement`, `HTMLIFrameElement`, `HTMLEmbedElement`, `HTMLObjectElement`, `HTMLParamElement`, `HTMLScriptElement`, `HTMLStyleElement`, `HTMLLinkElement`, `HTMLMetaElement`, `HTMLTitleElement`, `HTMLBaseElement`, `HTMLHeadElement`, `HTMLBodyElement`, `HTMLHtmlElement`, `HTMLUListElement`, `HTMLOListElement`, `HTMLLIElement`, `HTMLDListElement`, `HTMLParagraphElement`, `HTMLPreElement`, `HTMLQuoteElement` (blockquote/q), `HTMLHRElement`, `HTMLBRElement`, `HTMLHeadingElement` (h1-h6), `HTMLTableElement`, `HTMLTableRowElement`, `HTMLTableCellElement` (td/th), `HTMLTableSectionElement` (thead/tbody/tfoot), `HTMLTableColElement` (col/colgroup), `HTMLTableCaptionElement`, `HTMLDialogElement`, `HTMLDetailsElement`, `HTMLMenuElement`, `HTMLMapElement`, `HTMLModElement` (ins/del), `HTMLTimeElement`, `HTMLDataElement`, `HTMLTrackElement`. Plus `NodeList` and `HTMLCollection` with array-like `hasInstance`.

Tests: `tests/html_element_subclasses.rs` — 21 tests. **All 21 pass.**

### 3. performance.mark / measure / clearMarks / clearMeasures — `crates/heso-engine-js/src/engine.rs` (+170 LoC in BROWSER_APIS_BOOTSTRAP)

WHATWG user-timing level 2 surface added to the existing `performance` POJO:
- `mark(name, options?)` — stores a zero-duration `PerformanceEntry` with `entryType: 'mark'`. Honors options form `{startTime, detail}`.
- `measure(name, startMark?, endMark?)` plus the single-options-object form `{start, end, duration, detail}`. Computes duration from `startTime` to `endTime`; clamps to >= 0.
- `clearMarks(name?)` / `clearMeasures(name?)` — `undefined name` clears all of that type.
- `getEntries()` / `getEntriesByName(name, type?)` / `getEntriesByType(type)`.

Entries stored in a closure-private array; timestamps via `performance.now()` so determinism stays coherent with the VirtualClock contract.

Tests: `tests/performance_user_timing.rs` — 12 tests, including the github-canonical repro `performance.mark("js-parse-end:high-contrast-cookie-abc123")`. **All 12 pass.**

### 4. document.getElementsByClassName — `crates/heso-engine-js/src/dom.rs` (+100 LoC)

- `Document::get_elements_by_class_name(className)` — splits on ASCII whitespace, builds a compound `.a.b.c` CSS selector via dom_query's selector engine, manual-walk fallback for class names CSS can't lex.
- `Document::get_elements_by_name(name)` — same shape, attribute selector.
- `Element::get_elements_by_class_name(className)` — subtree-scoped form.
- `Element::get_elements_by_tag_name(name)` — was missing; added for the same parity.

Tests: `tests/dom_collection_apis.rs` — 9 tests, including the canonical HN `function byClass(el, cl) { return el.getElementsByClassName(cl); }` repro. **All 9 pass.**

### 5. data: URL fetch — `crates/heso-engine-js/src/fetch.rs` (promote `parse_data_url` to `pub(crate)`), `crates/heso-engine-js/src/scripts.rs` (+15 LoC)

The in-JS `fetch()` already handled `data:` URLs. The missing piece was the `<script src=...>` loader: reqwest only speaks HTTP(S), so `<script src="data:text/javascript,...">` returned `send: builder error` and reddit's three runtime-config bootstrap scripts all died.

Fix: promote `fetch.rs::parse_data_url` from private to `pub(crate)`, and in `scripts::fetch_script_source` short-circuit on `src.starts_with("data:")` to decode inline (base64 / percent-encoded text) without hitting the network.

Tests: `tests/data_url_scripts.rs` — 5 tests. **All 5 pass.**

### 6. element.style = "color: red" string-coercion setter — `crates/heso-engine-js/src/dom.rs` (+30 LoC)

Per CSSOM §6, assigning a string to `.style` is equivalent to setting `.style.cssText` — parse as a CSS declaration block and replace the inline `style="..."` attribute. Without this setter, QuickJS rejected the assignment with `no setter for property`.

Setter accepts `String` / `null` / `undefined`; both null-ish forms clear the attribute. The proxy-based read path already round-trips through the same attribute layer.

Tests: `tests/style_setter.rs` — 7 tests, including the canonical docs.rs/serde menu.js repro and a generic CSS-cssText round-trip. **All 7 pass.**

## Site-flip results (against the report's named sites)

Baseline scoring from `bug-reports/01-top-sites.md` (agent 1, 2026-05-21):

| URL | Before | After | Failed scripts before → after | Notes |
|---|---|---|---|---|
| **news.ycombinator.com** | DEGRADED | **PASS** | 1 → 0 | `getElementsByClassName` fix |
| **docs.rs/serde** | DEGRADED | **PASS** | ≥2 → 0 | HTMLLinkElement + `.style = "..."` fixes both apply |
| **theverge.com** | DEGRADED | **PASS** | XHR-missing → 0 | XHR fix |
| **reddit.com/r/programming** | DEGRADED | **PASS** | 3 (data: URL) → 0 | `<script src="data:">` fix |
| **github.com/torvalds/linux** | DEGRADED (93 fails) | DEGRADED (8 fails) | `performance.mark` cluster closed | other gaps remain (cookie / readyState) |
| **vercel.com** | PASS (baseline) | **PASS** | 0 → 0 | (was already passing; sanity check) |
| **react.dev** | PASS (baseline) | **PASS** | 0 → 0 | (was already passing; sanity check) |
| **linear.app** | DEGRADED | DEGRADED | `HTMLScriptElement is not defined` → `TextEncoder is not defined` | HTMLScriptElement fix landed; downstream TextEncoder gap unrelated |
| **cloudflare.com** | DEGRADED (9 fails) | DEGRADED (1 fail) | XHR + HTMLVideoElement + className cluster closed | last failure is `applyState` callsite — unrelated |
| **docs.docker.com** | DEGRADED | DEGRADED (2 fails) | XHR + className closed | improvement, not flip |
| **slack.com** | DEGRADED (9 fails) | DEGRADED (7 fails) | XHR + HTMLImageElement cluster reduced | other gaps remain |

**Site flips (DEGRADED → PASS): 4 confirmed** — HN, docs.rs/serde, theverge.com, reddit.com/r/programming. Many more are now strictly improved (github.com dropped from 93 → 8 failed scripts on a representative page).

The named sites in the prompt (linear.app, vercel.com, react.dev) — vercel.com and react.dev were already PASS in agent 1's report; both still PASS. linear.app is still DEGRADED because the specific `HTMLScriptElement is not defined` failure is now resolved (one fewer of the listed cluster), but Next.js's webpack runtime hits the downstream `TextEncoder is not defined` gap (called out in bug-report 03 as a separate P1 — out of scope here).

## Workspace test totals

Baseline (worktree's main): **1188 passed, 0 failed.**
After all 7 commits: **1256 passed, 0 failed, 7 ignored.**
Net new tests: **+68** (9 + 12 + 7 + 5 + 21 + 14 = 68 from the new files).

## Branch name + commit list

Branch: `agent-a26494d27998751e6-dom-gaps`
Worktree branch base: `653d1b0`

Commits:
- `edbf57d` engine-js: currentScript shim carries localName so HTMLScriptElement instanceof passes
- `6c3d6da` engine-js: XMLHttpRequest backed by the shared reqwest::Client
- `cf2c052` engine-js: HTML*Element subclass family + NodeList + HTMLCollection
- `a6fadf3` engine-js: data: URL fast path in `<script src>` loader
- `bd9770f` engine-js: element.style = "cssText" string-coercion setter
- `45391f5` engine-js: performance.mark / measure / clearMarks / clearMeasures
- `b3e9aed` engine-js: getElementsByClassName + getElementsByName
