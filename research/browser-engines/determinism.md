# Browser Determinism

**Topic:** Sources of nondeterminism in a browser and how heso eliminates each
**Last updated:** 2026-05-17
**Status:** initial research

## Summary

A standard browser is non-deterministic in dozens of small ways — wall clocks, RNGs, GPU, fonts, network timing, GC, JIT. For heso, determinism is a first-class property (ADR 0008), and that means each source of nondeterminism gets a specific strategy: eliminated, mocked, seeded, recorded, or explicitly opted into. This note enumerates the sources, the strategies, what's tractable in heso v1, and what's still open.

## Why determinism

Without it, three heso promises break:

1. **Signed audit logs (ADR 0005)** become attestation-of-a-historical-run, not attestation-of-a-reproducible-action.
2. **Agent test loops** flake. The agent can't distinguish bug from noise.
3. **Replay debugging** is impossible. "It worked when I tried it" is the default failure mode.

For an agent browser, these aren't nice-to-haves.

## The nondeterminism surface

Sources, grouped roughly by tractability:

### Trivially fixable (one config or pref flip)

- **`Math.random()`** — set V8/SpiderMonkey seed at startup ([V8 `--random_seed`](https://chromium.googlesource.com/v8/v8.git/+/refs/heads/main/src/flags/flag-definitions.h)). SpiderMonkey has an equivalent pref (`javascript.options.discard_system_source`-adjacent hooks; verify via Servo prefs).
- **GPU rendering nondeterminism** — use `SoftwareRenderingContext` always. Servo provides this out of the box (see [`servo-internals/embedding-api-v0-1-0.md`](../servo-internals/embedding-api-v0-1-0.md)). Same bytes on every platform, no driver variance.
- **Color management, sub-pixel anti-aliasing, font hinting platform variance** — pin to grayscale AA, fixed gamma, software-only renderer.
- **CSS animations / transitions** — disable globally via the `prefers-reduced-motion` media query and `animation: none !important` user stylesheet.

### Moderately fixable (heso-side wrapping required)

- **`Date.now()`, `Date()` constructor, `Date.UTC()`** — install a fake clock at session start. Inject via Servo prefs if exposed; otherwise patch the SpiderMonkey global. Clock advances on explicit agent API call or on each tick of `setTimeout`/`requestAnimationFrame`.
- **`performance.now()`** — same fake clock. Round to integer ms to defeat side-channel timing.
- **`requestAnimationFrame` / `setTimeout` / `setInterval`** — drive from fake clock. heso's event loop decides when to advance time and which scheduled callbacks fire.
- **Network requests** — intercept all via `WebViewDelegate::intercept_web_resource_load` (see [`servo-internals/embedding-api-v0-1-0.md`](../servo-internals/embedding-api-v0-1-0.md)). First run records (URL → response bytes + status + headers + timing). Replay serves from the record. Hash mismatch on a request not in the record → error, not a real fetch.
- **Font fallback** — bundle a single fixed font stack (e.g. Noto family for Latin + Noto CJK + Noto Symbols). Disable system font enumeration. Cross-platform identical layout.
- **`crypto.getRandomValues()`** — seed the PRNG from the session seed. **Caveat:** this breaks WebCrypto's security guarantee. An agent that opts into real cryptography (e.g. for legitimate web crypto operations) loses the determinism guarantee.

### Harder (engine-internal work)

- **JIT tier changes** — SpiderMonkey runs interpreter → baseline → Ion → Warp. Tier transitions are observable via timing and (rarely) via JS semantics in edge cases. For strict determinism, force interpreter-only mode (`javascript.options.baselinejit` etc. via Servo prefs). Performance cost is large (5–20×).
- **GC pauses** — observable via `performance.now()` deltas, eventually via memory pressure visible to the page. For strict determinism, force a deterministic GC schedule (e.g. major GC after N allocations rather than time-based). SpiderMonkey supports zeal modes for this.
- **JS event loop ordering** — microtask ordering, message channel timing. Single-threaded mode where Servo allows. Serialize message ordering through one queue.
- **`Error.prototype.stack`** — leaks compiled code addresses (ASLR). Normalize: strip column numbers and inline locations.
- **`Function.prototype.toString()`** — implementations differ in whitespace / comment preservation. Normalize via a passthrough.

### Hardest (would require forking Servo)

- **Stylo's parallel selector matching** — uses `rayon`. Result is deterministic (CSS spec mandates ordering) but timing of work-stealing introduces observable timing differences. Probably fine — outputs match, only timing varies. Audit before declaring done.
- **Network stack internal nondeterminism** — connection pooling, HTTP/2 stream multiplexing. heso's record/replay layer mostly side-steps this since we intercept above the network layer.
- **Servo crash / hang under stress** — observable to agent. No real fix; treat as a bug to report.

### Outside heso's control (must document)

- **Site-side cache-busting** (e.g. `?_=Date.now()` query strings). The site's JS uses our fake `Date.now()`, so the cache-buster is stable per seed — agents see the same URL on every replay. **Good.**
- **Site-side real entropy via WebRTC, WebUSB, sensors, etc.** All disabled in deterministic mode. Agents that opt into them lose the guarantee.
- **Server-side state changes between record and replay.** A login that creates a user can't be "replayed" against a server that doesn't know about that user. Mitigate via record-replay of *both* request and response; never round-trip in replay mode.

## The three modes

heso exposes three operating modes per session (per ADR 0008):

| Mode | Use case | Guarantees |
|---|---|---|
| `deterministic` | Default for agent sessions. Default for tests. Default for signed-action workflows. | Bit-for-bit reproducible given seed + recorded network trace. |
| `recording` | First-run sessions where the agent is exploring a new flow. | Wall clock and RNG are real, but every observable input is logged for replay. |
| `live` | Interactive debugging by a human. | No guarantees. Agent identity refuses to sign actions in this mode. |

## What heso v1 can deliver

Realistic scope for v1 (M1 + M2):

- ✅ Fake clock (Date.now, performance.now, setTimeout/setInterval, requestAnimationFrame)
- ✅ Seeded `Math.random()`
- ✅ Software rendering only (SoftwareRenderingContext)
- ✅ Pinned font stack
- ✅ Network record/replay via `intercept_web_resource_load`
- ⚠️ Seeded `crypto.getRandomValues()` (with explicit opt-out)
- ⚠️ Disable WebRTC, WebUSB, WebBluetooth, sensors
- ❌ JIT interpreter-only mode (defer — perf cost too high for v1; add as a `strict-deterministic` sub-mode in M3)
- ❌ GC zeal mode (defer — needs measurement before deciding it's necessary)
- ❌ Stylo timing audit (defer to post-M2)

## Implementation order

Roughly:

1. **M1 (during Servo embed work):** Wire `SoftwareRenderingContext`. Pin font stack. Disable animations.
2. **Early M2:** Build the fake clock + seeded RNG. Inject via Servo prefs where possible, patch where necessary. Add seed to session config struct.
3. **Mid M2:** Build network record/replay over `WebViewDelegate::intercept_web_resource_load`. JSON-Lines record format with content-addressed bodies.
4. **M2 exit criteria:** A session run twice in `deterministic` mode with the same seed and the same record produces byte-identical AX tree, extracted text, and screenshot for at least 5 reference sites (example.com, en.wikipedia.org, news.ycombinator.com, a JS-heavy SPA, a form-submission flow).
5. **M3 (during identity work):** Determinism becomes a precondition for signing. Audit log entries include the seed + record hash.

## Open questions

- **Q-004 (new):** Can we eliminate JS event loop microtask ordering nondeterminism without patching SpiderMonkey? Investigate.
- **Q-005 (new):** Is `Stylo`'s parallel selector matching output-deterministic in practice (only timing varies) or also output-nondeterministic in edge cases? Build a test.
- **Q-006 (new):** What's the right granularity for the fake clock — agent-controlled (explicit `advance_clock(ms)`), tick-based (auto-advance on next animation frame), or some hybrid? Implications for site compat.

## References

- ADR 0005 (identity + signed audit log) — why we need determinism.
- ADR 0008 (this decision) — the high-level position.
- [V8 deterministic mode flags](https://chromium.googlesource.com/v8/v8.git/+/refs/heads/main/src/flags/flag-definitions.h) — `--random_seed`, related.
- [Mozilla rr (record-replay)](https://rr-project.org/) — OS-level alternative; we chose not to.
- [Headless Chrome deterministic capabilities (slides)](https://docs.google.com/presentation/d/1gqK9F4lGAY3TZudAtdcxzMQNEE7PcuQrGu83No3l0lw/htmlpresent) — listed "deterministic time, date, random numbers, etc." as headless mode capabilities.
- [Prando — deterministic PRNG for JS/TS](https://github.com/zeh/prando) — implementation reference.
- [Tom Anthony: Googlebot's JS `random()` is deterministic](https://www.tomanthony.co.uk/blog/googlebot-javascript-random/) — prior art that the world's largest crawler runs in deterministic mode.
- [Andrew Healey: Creating randomness without `Math.random`](https://healeycodes.com/creating-randomness) — implementation reference.
- [`servo-internals/embedding-api-v0-1-0.md`](../servo-internals/embedding-api-v0-1-0.md) — where `intercept_web_resource_load` and `SoftwareRenderingContext` live.
- [`browser-engines/agent-first-design.md`](agent-first-design.md) — determinism in context of the larger agent-first design.
