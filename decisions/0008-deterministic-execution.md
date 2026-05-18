# 0008. Deterministic execution as a first-class property

- **Status:** Accepted
- **Date:** 2026-05-17
- **Deciders:** Akshay

## Context

heso is a browser whose user is an AI agent. Three properties heso has already committed to depend on *reproducibility*:

- **Signed action logs (ADR 0005).** A signature on an action only has meaning if "replaying the same input" produces the same observable output. Otherwise the signature attests to *one specific historical run*, not to a reproducible action.
- **Agent test loops.** Flaky tests are bad for any project; for an agent that re-runs its own behavior against expectations, they're fatal. The agent can't tell whether a test failure means a real regression or a non-deterministic flake.
- **Debug / replay.** When an agent run goes wrong, the developer (human or agent) needs to reproduce the exact sequence. "It worked on my machine" is the worst possible state.

A regular browser is **deeply non-deterministic** by design. Sources include:

- Wall-clock time (`Date.now()`, `performance.now()`, `requestAnimationFrame` timing, `setTimeout` jitter)
- Randomness (`Math.random()`, `crypto.getRandomValues()`)
- Network timing and ordering (DNS, TCP, HTTP/2 multiplexing, server load)
- JIT compilation tier changes (warm-up effects)
- GC pauses (observable via timing-sensitive code)
- GPU driver behavior (sub-pixel rounding, anti-aliasing)
- Font fallback (cross-platform font availability)
- System entropy (ASLR-affected pointer values exposed via some APIs)
- Multi-threaded event ordering

For heso to honor its commitments, these have to be eliminated, mocked, recorded, or made explicit.

## Decision

**Deterministic execution is a first-class property of heso.** It is non-negotiable. Every API default and every implementation choice optimizes for determinism. Sources of nondeterminism are either:

- **Eliminated** at the engine layer (e.g. force `SoftwareRenderingContext`, no GPU)
- **Mocked** with seedable, agent-controlled values (e.g. wall clock, RNG, network)
- **Recorded** for later replay (e.g. server responses, server-side state)
- **Made explicit** via opt-in API (the rare case where the agent genuinely wants entropy — `session.unsafe_use_real_entropy()`)

Every agent session has a deterministic seed value. Given the same seed and the same recorded network trace, two runs of the same actions produce byte-identical observable output (AX tree, extracted text, screenshots, signed audit log).

### Specifically

For each known source of nondeterminism, the heso-side approach:

| Source | Strategy |
|---|---|
| `Date.now()`, `Date()` constructor, `Date.UTC()` | Seeded fake clock. Advances on `setTimeout` / `setInterval` / `requestAnimationFrame` tick or by explicit agent API. |
| `performance.now()` | Same fake clock, sub-millisecond precision off (return integer ms). |
| `Math.random()` | PRNG seeded from session seed. Inject via Servo prefs or DOM binding patch. |
| `crypto.getRandomValues()` | PRNG seeded from session seed. **Note:** breaks WebCrypto security properties — agent must opt out for real crypto use cases. |
| `requestAnimationFrame` | Driven by fake clock; agent controls tick rate. |
| `setTimeout` / `setInterval` | Use fake clock; fire deterministically. |
| Network requests | Record-replay layer via `WebViewDelegate::intercept_web_resource_load`. First run records; replay runs serve from the record. Hash mismatch on a request that wasn't recorded → error, not a real fetch. |
| GPU rendering | Force `SoftwareRenderingContext`. Same bytes on every platform. |
| Font fallback | Pin to a bundled font stack (e.g. Noto family). No system fonts. |
| JIT tier changes | Configure SpiderMonkey to single-tier interpreter for deterministic mode (perf cost; on by default in `mode: deterministic`). |
| GC pauses | Force deterministic GC schedule (gen-major after N allocations). |
| Multi-threaded event ordering | Single-threaded mode where Servo allows; serialize message ordering through one queue. |
| ASLR-affected APIs | Disable `Error.prototype.stack` line info, normalize `Function.prototype.toString` output. |
| Servo internal nondeterminism | Audit per release. File upstream issues. Track in `state.json`. |

### Three operating modes

heso sessions run in one of three modes:

1. **`deterministic`** (default for agent sessions) — all of the above. Slowest. Bit-for-bit reproducible.
2. **`recording`** — wall-clock and RNG are real, but every observable input is logged so the run can be replayed later in `deterministic` mode.
3. **`live`** — no determinism guarantees. For interactive debugging only. The agent identity should refuse to sign actions in this mode.

## Alternatives considered

- **Best-effort determinism.** Mock the easy things (Date, Math.random), shrug at the rest. Rejected: signed action logs become meaningless because the agent can't prove the action was reproducible; tests stay flaky.
- **Optional determinism (off by default, flag-on).** Rejected: agents will forget to turn it on; behavioral surprises in production; "did the test fail because of a real bug or because deterministic mode was off?"
- **OS-level record/replay (rr-style).** Heavy. Constrains heso to Linux x86_64. Doesn't compose well with cloud deployment. Worth re-evaluating in 2028+ when rr-like tools have better cross-platform support.
- **Defer determinism work to "later."** Rejected: every API designed without determinism in mind becomes a future breaking change. Building it in from the start is much cheaper than retrofitting.

## Consequences

**Positive:**
- Signed action logs are meaningful: anyone with the recorded network trace + seed + action log can verify the run independently.
- Agent tests are flake-free.
- Replay debugging works: re-run a failing agent session to inspect intermediate state.
- Caching of agent behavior across runs becomes possible (same seed + same actions → same result → cache hit).
- Forces clean API design: any source of nondeterminism in heso's API surface is a bug.

**Negative:**
- Significant engineering work. Each nondeterminism source on the table above needs an implementation.
- Some real-world web features become unavailable in `deterministic` mode (real WebRTC, hardware-backed crypto, location services). Agents opting into these lose the determinism guarantee.
- Performance cost. SoftwareRenderingContext is slower than GPU. Single-tier JIT is much slower than full SpiderMonkey. Determinism is paid for in CPU.
- May require patching Servo for nondeterminism sources Servo doesn't expose configurably. Increases maintenance burden against upstream.
- Some nondeterminism in third-party JS code (sites' own use of `Date.now()` for cache-busting query strings, etc.) is observable to agents and we can't eliminate it — only "freeze" the agent's view of time so the cache-buster value is stable per seed.

## References

- ADR 0005 (Ed25519 identity + signed audit log) — depends on determinism for signatures to mean reproducibility, not just historical fact.
- ADR 0003 (Servo as first engine) — Servo provides several deterministic-mode hooks we'll use (prefs for fake clock, `SoftwareRenderingContext`).
- [`research/browser-engines/determinism.md`](../research/browser-engines/determinism.md) — technical reality and per-source implementation strategies.
- [V8 `--random_seed` flag](https://chromium.googlesource.com/v8/v8.git/+/refs/heads/main/src/flags/flag-definitions.h) — prior art for seeded `Math.random()`.
- [Mozilla rr (record-replay) project](https://rr-project.org/) — what we explicitly chose not to do at OS level.
- [Chrome Headless deterministic mode notes](https://docs.google.com/presentation/d/1gqK9F4lGAY3TZudAtdcxzMQNEE7PcuQrGu83No3l0lw/htmlpresent) — Headless Chrome had "deterministic time, date, random numbers" as a stated capability.
- [Prando — deterministic PRNG for JS](https://github.com/zeh/prando) — implementation reference for seeded JS-side randomness.
- `state.json` open questions Q-001 (JS engine) and the new Q-004 (which nondeterminism sources block v1?).
