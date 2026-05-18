# 0012. Fetch-only native engine (supersedes ADR 0011)

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay
- **Supersedes:** [ADR 0011 — Chromium via CDP as first engine](0011-chromium-cdp-first-engine.md), which itself superseded [ADR 0003 — Servo as first engine](0003-servo-as-first-engine.md)

## Context

ADR 0011 picked Chromium via CDP (`chromiumoxide`) as the M1 engine on the thesis that "engine choice doesn't determine the moat — the layer above does." That was right about where the moat lives. It was wrong about deployability:

- **`chromiumoxide` needs a system Chrome installed.** It auto-detects via registry / PATH / install paths, but if the user doesn't have Chrome, nothing runs. heso ceases to be a single deployable binary; it becomes "heso binary + Chrome install + Chrome version compatibility matrix."
- **The "deploy anywhere" pitch evaporates.** Anyone who can't (or doesn't want to) install Chrome — restricted environments, hardened containers, embedded use cases, customer machines — is locked out.
- **It positions heso as "Playwright with extra steps,"** which is the worst of both worlds: harder to deploy than Playwright (no Node ecosystem, no `playwright install`) while offering only architecture-layer benefits over it.

The opposite mistake is just as bad: ship `reqwest + scraper` and call it heso. That's the Rust equivalent of BeautifulSoup. Any agent author can wire that up in 50 lines; there's no differentiation. The moat — signed receipts, content-addressed pages, stable element refs, terminal-shell primitive vocabulary, planner-emitted traces, deterministic replay — has to actually *do something* beyond returning a parsed DOM.

The right path: **single Rust binary, no browser dep, with heso's differentiating layers (receipts, primitives, planner) doing real work on top.** Static pages cover the majority of read-only agent workflows (docs, news, blogs, listings, marketing, simple e-commerce). JS-rendered SPAs need a real browser engine — that's a future addition (Servo bundle, or a process-isolated WebView, or both), not a v1 requirement.

## Decision

**heso ships exactly one engine in v1: `heso-engine-fetch`.** Pure Rust: `reqwest` for HTTP, `html5ever` (via `scraper`) for HTML parsing, recursive DOM walk for visible-text extraction (script/style/noscript/template stripped, whitespace normalized). No browser, no Node, no system dep. Single static binary, ~10MB. Deploys anywhere `heso.exe` runs.

### What this enables today

- `heso fetch <url>` — direct surface, returns `{ url, text }` JSON. Used by the Flue test agent's `heso_fetch` tool. Real text from real sites, end-to-end pure-Rust, no Chrome.
- `heso run <url> <request>` — runs a trace through `heso-trace-exec` against the fetch engine. Today the trace is just `cd <url>`; the planner (T-022) will turn this into a real one-tool surface.
- All of `heso-trace` (signed receipts, content-hashed pages, deterministic `trace_hash`), `heso-primitives` (the 15-op terminal vocabulary), `heso-trace-exec` (trace runner) carry over unchanged — they're engine-agnostic by design (ADR 0002).

### Honest scope limits

- **No JavaScript.** SPA-heavy sites (Twitter, modern dashboards, JS-only landing pages) will return sparse or empty text. Acceptable for v1; most agent web-browsing tasks read static or server-rendered content.
- **No CSS layout.** We extract semantic structure from HTML/ARIA, not visual position.
- **No JS-validated form submission.** Plain `<form>` POSTs can be added via the same `reqwest::Client` (future primitive); JS-validated forms need a real browser.
- **No client-side rendering of dynamic content** (e.g. infinite scroll, lazy-loaded images, on-scroll fetch).

These limits are explicit. When an agent hits one, the receipt should show empty/sparse text and the planner can surface a clear "this site needs a JS-capable engine" error rather than silently returning partial data.

### Differentiation vs `reqwest + scraper`

This is the line that has to hold or the user's critique stands. heso provides, on top of what those crates do:

1. **Signed receipts with BLAKE3 `trace_hash`** — every `heso run` produces a receipt that's content-addressed and replay-verifiable. `reqwest + scraper` doesn't.
2. **Content-addressed pages** (planned, M2) — every page fetched goes into `pages_seen` with a hash. Two runs against the same URL and same network record produce byte-identical receipts.
3. **Terminal-shell primitive vocabulary** (ADR 0010) — `pwd`/`ls`/`cd`/`cat`/`find`/`grep`/`echo`/`rm`/`click`/`submit`/`wget`/`wait`/`screenshot`/`eval`/`diff`. Agents (LLMs) get a 15-op surface that maps to shell intuition, not a per-call API where they have to remember which scraper method does what.
4. **Stable `@e0/@e1/...` element refs across snapshots** (planned, M2) — a `find` on one fetch returns refs that a `cat` on a later fetch can still use. `scraper` gives you opaque borrowed `ElementRef`s tied to a single `Html` parse.
5. **Plain-English request → planner → trace → receipt** (planned, M3) — `heso.run(url, "find the cheapest laptop under $1000")` does the planning. The agent doesn't write CSS selectors.
6. **Deterministic by construction for static fetches.** Same URL + same network record = same receipt, byte-for-byte. `reqwest` follows redirects with timing, but the static-fetch path has no clocks or RNG to seed — replayability is free.
7. **Future: AX-tree-shaped representation** derived from ARIA + HTML5 semantic tags. Agents get `(role, name, ref, children)` trees, not raw DOM nodes. (Planned, M2.)
8. **One-tool agent contract.** `heso.run(start_url, request)` is the only thing an agent calls. ADR 0009.

## Alternatives considered

- **Stay on Chromium via CDP (ADR 0011).** Rejected: deployability is broken. Requires system Chrome. Heso becomes "Playwright with extra steps."
- **Bundle a Chromium binary.** Rejected for v1: 150-300MB distribution, version maintenance burden, "single binary" claim becomes "single 300MB download." Worth revisiting in M4+ as an optional companion build for JS-heavy sites.
- **Servo embed (ADR 0003).** Rejected for v1: 6+ weeks of SpiderMonkey build pain, distribution headaches, Servo's site-compat gaps. Reconsider in M4+ if/when JS support is required.
- **`heso-engine-cdp` as opt-in second engine.** Rejected per user direction ("dont need optin"). Keeping two engines doubles the maintenance surface for value most users won't exercise. If JS support becomes a real M2+ need, add a single best-fit engine then — likely a bundled WebView (wry / Tauri runtime) for the same OS-native distribution feel rather than another Chrome dep.
- **Just ship `reqwest + scraper`.** Rejected: indistinguishable from existing tooling. heso has to deliver the layer above (receipts, primitives, planner, terminal model) or there's no reason to use it.

## Consequences

**Positive:**
- **Single Rust binary, ~10MB, deploys anywhere.** No Chrome install, no Node, no Python, no system deps beyond a TLS root store. Drop-in for CI, containers, embedded, restricted environments.
- **Build time collapses.** No SpiderMonkey, no Chromium download. `cargo build --release -p heso-cli` is ~30s on a cold cache.
- **All cross-cutting heso work** (receipts, primitives, trace runner, planner) keeps moving forward unaffected — those crates are engine-agnostic by design.
- **Determinism is largely free** on the static-fetch path. No clock, no RNG, no GPU.
- **Honest positioning.** heso is "the agent-first read-the-web library, single Rust binary, no browser." Not "Playwright but Rust."

**Negative:**
- **No JavaScript.** Sites that need JS won't work. The honest answer is "use a JS-capable engine for those" — but in v1 we don't have one. The Flue agent's `heso_fetch` tool description calls this out explicitly so the LLM knows.
- **Anti-bot resistance is worse than a real browser.** A `reqwest` User-Agent + missing Chrome fingerprint is more obviously a bot than headless Chrome. Mitigation: future header-tuning, optional residential-proxy support, eventually a bundled-WebView engine for sites that block scrapers.
- **The `agent-first-design.md` AX-tree story isn't free yet.** Real AX trees come from layout-pass + display:none filtering; we can approximate from raw HTML's ARIA + semantic tags but it's not equivalent. M2 task to implement.
- **JS-rendered marketing sites might disappoint.** Many vibe-coded landing pages are React/Vue SPAs. Test before promising users they "just work."

## References

- ADR 0002 (engine trait boundary) — unchanged; this ADR ships one engine but the trait still supports adding more.
- ADR 0003 (Servo as first engine) — superseded by 0011, now historical.
- ADR 0008 (deterministic execution) — unchanged; static-fetch path satisfies it for free.
- ADR 0009 (one tool — heso.run) — unchanged.
- ADR 0010 (terminal-shaped primitives) — unchanged; 15-op vocabulary still defines the planner's target.
- ADR 0011 (Chromium via CDP) — superseded by this ADR; engine deleted from the workspace.
- [`scraper` crate](https://crates.io/crates/scraper) — Servo's `html5ever` parser with a friendly query API.
- [`reqwest` crate](https://crates.io/crates/reqwest) — HTTP client with rustls TLS, HTTP/2, gzip/brotli, redirect following.
